//! Read-only queue/Git observations, never a cleanup authorization or lease.
mod git;
mod records;

use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::json;

use crate::{model::J, util, FlowError, Result};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Spec {
    queue_id: String,
    task: String,
    state_version: u64,
    checkout: Option<PathBuf>,
}

#[derive(Default)]
struct Findings {
    blockers: Vec<J>,
    historical: Vec<J>,
}

impl Findings {
    fn block(&mut self, code: &str, detail: impl Into<String>) {
        self.blockers
            .push(json!({"code":code,"detail":detail.into(),"unknown":false}));
    }

    fn unknown(&mut self, code: &str, detail: impl Into<String>) {
        self.blockers
            .push(json!({"code":code,"detail":detail.into(),"unknown":true}));
    }

    fn history(&mut self, kind: &str, identity: J) {
        self.historical
            .push(json!({"kind":kind,"identity":identity}));
    }

    fn status(&self) -> &str {
        if self.blockers.iter().any(|b| b["unknown"] == true) {
            "unknown"
        } else if self.blockers.is_empty() {
            "clear"
        } else {
            "blocked"
        }
    }
}

fn require(ok: bool, message: &str) -> Result<()> {
    if ok {
        Ok(())
    } else {
        Err(FlowError::queue(message))
    }
}

fn object<'a>(v: &'a J, name: &str) -> Result<&'a serde_json::Map<String, J>> {
    v.as_object()
        .ok_or_else(|| FlowError::queue(format!("invalid {name} object")))
}

fn array<'a>(v: &'a J, name: &str) -> Result<&'a Vec<J>> {
    v.as_array()
        .ok_or_else(|| FlowError::queue(format!("invalid {name} array")))
}

fn text<'a>(v: &'a J, name: &str) -> Result<&'a str> {
    v.as_str()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| FlowError::queue(format!("missing {name}")))
}

fn path(v: &J) -> Result<PathBuf> {
    let p = Path::new(text(v, "recorded path")?);
    require(p.is_absolute(), "recorded path must be absolute")?;
    Ok(p.to_path_buf())
}

fn read(root: &Path) -> Result<J> {
    util::read_state_json_bounded(root, "queue.json", 32 * 1024 * 1024)
}

fn validate(s: &J, spec: &Spec) -> Result<()> {
    require(
        !spec.queue_id.is_empty() && !spec.task.is_empty(),
        "queue/task identity required",
    )?;
    require(
        s["version"] == 1 && s["id"] == spec.queue_id,
        "queue identity/version mismatch",
    )?;
    validate_task(s, spec)?;
    if let Some(p) = &spec.checkout {
        require(p.is_absolute(), "checkout must be absolute")?;
    }
    Ok(())
}

fn validate_task(s: &J, spec: &Spec) -> Result<()> {
    let tasks = object(&s["tasks"], "tasks")?;
    let t = tasks
        .get(&spec.task)
        .ok_or_else(|| FlowError::queue("task missing"))?;
    require(t["task"] == spec.task, "task map identity mismatch")?;
    require(
        spec.state_version > 0 && t["state_version"] == spec.state_version,
        "task version changed",
    )?;
    Ok(())
}

fn relevant(s: &J) -> J {
    // All ownership/reference records are included. Only top-level watcher
    // timing is excluded; a foreign task changing is conservatively unknown.
    let keys = [
        "id",
        "version",
        "tasks",
        "waits",
        "events",
        "deliveries",
        "active",
        "guard_dir",
        "receiver",
        "stopped",
        "notify",
    ];
    let mut result = json!({});
    for key in keys {
        result[key] = s[key].clone();
    }
    result
}

/// Securely read existing state; no directory/lock/readiness creation. The
/// report is informational even when its queue/Git subset is clear.
pub fn inspect(root: &Path, spec: &J) -> Result<J> {
    inspect_with(root, spec, git::observe)
}

fn inspect_with(
    root: &Path,
    input: &J,
    observe: impl FnOnce(&Path, &mut Findings) -> Result<J>,
) -> Result<J> {
    let started = util::now();
    let spec: Spec = serde_json::from_value(input.clone())
        .map_err(|e| FlowError::usage(format!("invalid cleanup-preflight spec: {e}")))?;
    let before = read(root)?;
    validate(&before, &spec)?;
    let mut findings = Findings::default();
    let candidates = records::candidates(&before, &spec.task, &mut findings);
    let checkout = records::select(&candidates, spec.checkout.as_deref(), &mut findings);
    let mut git = J::Null;
    if let Some(p) = &checkout {
        records::inspect(&before, &spec.task, root, p, &mut findings);
        match observe(p, &mut findings) {
            Ok(value) => {
                git = value;
                records::check_head(&candidates, p, &git, &mut findings);
            }
            Err(e) => findings.unknown("git_observation_failed", e.to_string()),
        }
    }
    readback(root, &before, &mut findings);
    Ok(
        json!({"snapshot":true,"deletion_authorized":false,"owner_recheck_required":true,
        "queue_git_status":findings.status(),"queue_id":before["id"],"queue_version":before["version"],
        "task":spec.task,"epoch":before["tasks"][&spec.task]["epoch"],"state_version":spec.state_version,
        "queue_reference_digest":util::sha256_json_sorted(&relevant(&before)),
        "candidate_checkout":checkout,"candidate_provenance":candidates,"git":git,
        "blockers":findings.blockers,"historical_references":findings.historical,
        "started_at":started,"finished_at":util::now(),
        "not_checked":["process_cwd_and_open_descriptors","herdr_workspace_ownership",
            "unregistered_external_writers","unrecorded_shared_asset_boundaries"],
        "next_step":"Owner must recheck live references, process and Herdr ownership before any separately authorized native removal; this snapshot cannot authorize deletion."}),
    )
}

fn readback(root: &Path, before: &J, f: &mut Findings) {
    match read(root) {
        Ok(after) if relevant(before) == relevant(&after) => (),
        Ok(_) => f.unknown(
            "queue_changed",
            "queue ownership/reference records changed during observation",
        ),
        Err(e) => f.unknown("queue_readback_failed", e.to_string()),
    }
}

pub fn exit_code(report: &J) -> i32 {
    if report["queue_git_status"] == "clear" {
        0
    } else {
        1
    }
}

pub fn summary(report: &J) -> String {
    format!("queue/Git only: {}\ncheckout: {}\nblockers: {}\nnot checked: {}\ndelete authorized: false; owner recheck required\n{}",
        report["queue_git_status"], report["candidate_checkout"], report["blockers"],
        report["not_checked"], report["next_step"])
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, os::unix::fs::PermissionsExt};

    #[test]
    fn readback_changes_or_failure_are_unknown_without_retry() {
        let temp = tempfile::Builder::new()
            .permissions(fs::Permissions::from_mode(0o700))
            .tempdir()
            .unwrap();
        let s = json!({"version":1,"id":"q","tasks":{"t":{"task":"t","state_version":1,"epoch":1,
            "candidate":"a".repeat(40),"operations":{},"writer":{"workspace":temp.path()}}}});
        let file = temp.path().join("queue.json");
        let spec = json!({"queue_id":"q","task":"t","state_version":1});
        for remove in [false, true] {
            util::atomic_write_json(&file, &s).unwrap();
            let r = inspect_with(temp.path(), &spec, |_, _| {
                if remove {
                    fs::remove_file(&file)?;
                } else {
                    let mut changed = s.clone();
                    changed["tasks"]["foreign"] = json!({"writer":true});
                    util::atomic_write_json(&file, &changed)?;
                }
                Ok(json!({"head":"a".repeat(40)}))
            })
            .unwrap();
            let code = if remove {
                "queue_readback_failed"
            } else {
                "queue_changed"
            };
            assert_eq!(r["queue_git_status"], "unknown");
            assert!(r["blockers"]
                .as_array()
                .unwrap()
                .iter()
                .any(|b| b["code"] == code));
        }
    }

    #[test]
    fn heartbeat_changes_do_not_hide_ownership_changes() {
        let s = json!({"id":"q","tasks":{},"heartbeat":1,"updated_at":1});
        let mut after = s.clone();
        after["heartbeat"] = json!(2);
        after["updated_at"] = json!(2);
        assert_eq!(relevant(&s), relevant(&after));
        after["tasks"]["new"] = json!({});
        assert_ne!(relevant(&s), relevant(&after));
    }
}
