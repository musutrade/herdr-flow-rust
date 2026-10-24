//! Typed checkout provenance. No evidence-file or free-text path discovery.
mod references;

use super::{array, object, path, require, text, Findings};
use crate::{model, model::J, Result};
use serde_json::json;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

pub(super) fn candidates(s: &J, name: &str, f: &mut Findings) -> Vec<J> {
    let mut found = Vec::new();
    if let Err(e) = collect(s, name, &mut found) {
        f.unknown("checkout_provenance", e.to_string());
        found.clear();
    }
    for record in &mut found {
        record["task"] = s["tasks"][name]["task"].clone();
        record["epoch"] = s["tasks"][name]["epoch"].clone();
    }
    found
}

fn push(found: &mut Vec<J>, value: &J, provenance: &str, head: &J) -> Result<()> {
    let checkout = path(value)?;
    found.push(json!({"checkout":checkout,"provenance":provenance,"head":head}));
    Ok(())
}

fn collect(s: &J, name: &str, found: &mut Vec<J>) -> Result<()> {
    let t = &s["tasks"][name];
    require(
        t["candidate"].as_str().is_some_and(model::is_full_sha),
        "task candidate missing",
    )?;
    require(
        t["epoch"].as_u64().is_some_and(|n| n > 0),
        "task epoch missing",
    )?;
    task_locations(t, found)?;
    pr_location(t, found)?;
    for (owner, task) in object(&s["tasks"], "tasks")? {
        lane_candidates(task, owner, t, found)?;
    }
    Ok(())
}

fn task_locations(t: &J, found: &mut Vec<J>) -> Result<()> {
    if !t["writer"].is_null() {
        push(
            found,
            &t["writer"]["workspace"],
            "task.writer",
            &t["candidate"],
        )?;
    }
    for (id, op) in object(&t["operations"], "operations")? {
        if !op["intent"]["workspace"].is_null() {
            push(
                found,
                &op["intent"]["workspace"],
                &format!("operation:{id}"),
                &t["candidate"],
            )?;
        }
    }
    Ok(())
}

fn pr_location(t: &J, found: &mut Vec<J>) -> Result<()> {
    if !t["pr"].is_null() {
        require(
            t["pr"]["head"] == t["candidate"],
            "PR candidate identity mismatch",
        )?;
        push(found, &t["pr"]["checkout"], "task.pr", &t["pr"]["head"])?;
    }
    Ok(())
}

fn lane_candidates(task: &J, owner: &str, target: &J, found: &mut Vec<J>) -> Result<()> {
    let lane = &task["lane"];
    if lane.is_null() {
        return Ok(());
    }
    for m in array(&lane["pending"], "lane.pending")? {
        member_candidate(m, owner, target, found)?;
    }
    if !lane["active"].is_null() {
        batch_candidates(&lane["active"], owner, target, found)?;
    }
    for batch in array(&lane["history"], "lane.history")? {
        batch_candidates(batch, owner, target, found)?;
    }
    Ok(())
}

fn batch_candidates(batch: &J, owner: &str, t: &J, found: &mut Vec<J>) -> Result<()> {
    for m in array(&batch["members"], "batch.members")? {
        member_candidate(m, owner, t, found)?;
    }
    Ok(())
}

fn member_candidate(m: &J, owner: &str, t: &J, found: &mut Vec<J>) -> Result<()> {
    if m["task"] != t["task"] {
        return Ok(());
    }
    require(
        m["epoch"] == t["epoch"] && m["head"] == t["candidate"],
        "lane member epoch/candidate mismatch",
    )?;
    require(
        t["batch_delivery"]["owner"] == owner,
        "lane member owner mismatch",
    )?;
    require(
        m["receipt_id"] == t["batch_delivery"]["receipt_id"],
        "lane member receipt mismatch",
    )?;
    push(found, &m["checkout"], &format!("lane:{owner}"), &m["head"])
}

pub(super) fn select(found: &[J], requested: Option<&Path>, f: &mut Findings) -> Option<PathBuf> {
    match selection(found, requested) {
        Ok(p) => Some(p),
        Err(e) => {
            f.unknown("checkout_selection", e.to_string());
            None
        }
    }
}

fn selection(found: &[J], requested: Option<&Path>) -> Result<PathBuf> {
    let paths: BTreeSet<_> = found
        .iter()
        .map(|v| path(&v["checkout"]))
        .collect::<Result<_>>()?;
    let p = match requested {
        Some(p) => {
            require(paths.contains(p), "checkout is not an exact recorded path")?;
            p.to_path_buf()
        }
        None => {
            require(paths.len() == 1, "checkout is missing or ambiguous")?;
            paths.into_iter().next().unwrap()
        }
    };
    require(
        p.canonicalize()? == p && p.is_dir(),
        "checkout must be canonical existing directory without aliases",
    )?;
    Ok(p)
}

pub(super) fn check_head(found: &[J], p: &Path, git: &J, f: &mut Findings) {
    for record in found
        .iter()
        .filter(|r| r["checkout"].as_str() == p.to_str())
    {
        if record["head"].is_null() {
            continue;
        }
        let head = record["head"].as_str().unwrap_or("");
        if !model::is_full_sha(head) || record["head"] != git["head"] {
            f.unknown("recorded_head_mismatch", record["provenance"].to_string());
        }
    }
}

pub(super) fn inspect(s: &J, name: &str, root: &Path, p: &Path, f: &mut Findings) {
    if let Err(e) = references::inspect(s, name, p, f) {
        f.unknown("queue_reference_invalid", e.to_string());
    }
    overlap(p, root, "selected queue", f);
    for value in [
        &s["guard_dir"],
        &s["receiver"]["state_dir"],
        &s["receiver"]["cwd"],
    ] {
        if !value.is_null() {
            match path(value) {
                Ok(other) => overlap(p, &other, "receiver/guard boundary", f),
                Err(e) => f.unknown("shared_boundary", e.to_string()),
            }
        }
    }
}

pub(super) fn overlaps(a: &Path, b: &Path) -> bool {
    let a = crate::util::resolve_path(a);
    let b = crate::util::resolve_path(b);
    a.starts_with(&b) || b.starts_with(&a)
}

pub(super) fn overlap(a: &Path, b: &Path, label: &str, f: &mut Findings) {
    let b = crate::util::resolve_path(b);
    if overlaps(a, &b) {
        f.block(
            "shared_boundary_overlap",
            format!("{label}: {}", b.display()),
        );
    }
}
