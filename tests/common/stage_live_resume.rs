//! One opt-in recovery of a proven pre-send attempt, never an unknown replay.
use super::*;
use std::os::unix::fs::{FileTypeExt, MetadataExt};

pub(super) fn socket(path: &Path) -> J {
    let canonical = fs::canonicalize(path).unwrap();
    let meta = fs::symlink_metadata(&canonical).unwrap();
    assert!(canonical.is_absolute() && meta.file_type().is_socket());
    assert_eq!(meta.uid(), util::process::current_uid());
    assert_eq!(
        meta.mode() & 0o022,
        0,
        "upstream socket is writable by others"
    );
    json!({"path":canonical,"dev":meta.dev(),"ino":meta.ino(),"uid":meta.uid()})
}

pub(super) fn attach(root: &Path) -> Fixture {
    let lock = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    assert_eq!(fs::canonicalize(root).unwrap(), root);
    let meta = fs::symlink_metadata(root).unwrap();
    assert!(meta.is_dir() && meta.mode() & 0o077 == 0);
    assert_eq!(meta.uid(), util::process::current_uid());
    let commit = git(&root.join("repo"), &["rev-parse", "HEAD"]);
    let old_root = std::env::var_os("HERDR_TASK_ENV_ROOT");
    std::env::set_var(
        "HERDR_TASK_ENV_ROOT",
        root.join("queue/queue.json/task-environments"),
    );
    Fixture {
        temp: FixtureTemp::Retained(root.into()),
        queue: root.join("queue"),
        commit,
        old_root,
        _lock: lock,
    }
}

fn no_send_tree(root: &Path, owner: &J) {
    let entries: Vec<_> = fs::read_dir(root)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(
        entries,
        vec![std::ffi::OsString::from("coordinator")],
        "unexpected runtime already exists"
    );
    let coordinator = root.join("coordinator");
    assert_eq!(
        util::read_state_json(&coordinator, "lifecycle-owner.json").unwrap(),
        *owner
    );
    for entry in fs::read_dir(&coordinator).unwrap() {
        let entry = entry.unwrap();
        let name = entry.file_name();
        let name = name.to_str().unwrap();
        let meta = fs::symlink_metadata(entry.path()).unwrap();
        match name {
            "home" | "config" | "data" | "cache" | "state" | "tmp" | "c" | "runtime" => {
                assert!(meta.is_dir(), "prepared runtime directory changed");
                assert!(
                    fs::read_dir(entry.path()).unwrap().next().is_none(),
                    "runtime already contains execution evidence"
                );
            }
            "lifecycle-owner.json" => assert!(meta.is_dir()),
            ".flow-runtime-v2" | ".flow-namespace-v2" | "runtime.lock" => assert!(meta.is_file()),
            _ => panic!("pre-send recovery rejects existing runtime evidence: {name}"),
        }
    }
}

fn no_marked_process(instance: &str, original_supervisor_start: u64) {
    use std::io::Read;
    let started = Instant::now();
    let needle = format!("{}={instance}", task_env::MARKER_ENV);
    for (index, entry) in fs::read_dir("/proc").unwrap().enumerate() {
        assert!(
            index < 32768 && started.elapsed() < Duration::from_secs(5),
            "process audit bound exceeded"
        );
        let entry = entry.unwrap();
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<i32>().ok())
        else {
            continue;
        };
        let meta = match fs::metadata(entry.path()) {
            Ok(meta) => meta,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => panic!("cannot inspect process {pid}: {error}"),
        };
        if meta.uid() != util::process::current_uid() {
            continue;
        }
        let stat = match fs::read_to_string(entry.path().join("stat")) {
            Ok(stat) => stat,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => panic!("cannot inspect process start identity {pid}: {error}"),
        };
        let fields: Vec<_> = stat
            .rsplit_once(')')
            .unwrap()
            .1
            .split_whitespace()
            .collect();
        let start: u64 = fields[19].parse().unwrap();
        // This original supervisor could only create newer processes. Existing
        // shared services and older user processes are outside task ownership.
        if start < original_supervisor_start || matches!(fields[0], "Z" | "X" | "x") {
            continue;
        }
        let environ = match fs::File::open(entry.path().join("environ")) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                // SAFETY: positive PID and signal zero only observes existence.
                assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
                assert_eq!(
                    std::io::Error::last_os_error().raw_os_error(),
                    Some(libc::ESRCH)
                );
                continue;
            }
            Err(error) => panic!("cannot establish process absence for {pid}: {error}"),
        };
        let mut bytes = Vec::new();
        environ
            .take(1024 * 1024 + 1)
            .read_to_end(&mut bytes)
            .unwrap();
        assert!(
            bytes.len() <= 1024 * 1024,
            "process environment exceeds bound"
        );
        assert!(
            !bytes
                .split(|byte| *byte == 0)
                .any(|value| value == needle.as_bytes()),
            "owned process already exists; do not replay"
        );
    }
}

fn baseline(root: &Path) -> J {
    let path = root.join("stage-baseline.json");
    let meta = fs::symlink_metadata(&path).unwrap();
    assert!(meta.is_file() && meta.len() <= 64 * 1024);
    serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
}

/// `expected` is the caller-pinned original Owner and socket identity, not task data.
pub(super) fn validate(f: &Fixture, expected: &J, current: &J) -> J {
    assert_eq!(expected["root"], json!(f.temp.path()));
    let saved = baseline(f.temp.path());
    let q = f.state();
    let task = &q["tasks"]["task"];
    let owner = serde_json::to_value(lifecycle::task_owner(&q, "task").unwrap().unwrap()).unwrap();
    assert_eq!(owner, expected["owner"]);
    assert_eq!(saved["owner"], owner);
    assert_eq!(owner["queue_root"], json!(f.queue));
    assert_eq!(q["tasks"].as_object().unwrap().len(), 1);
    assert_eq!(task["requirements"], saved["agreement"]);
    assert_eq!(saved["agreement"]["1"]["goal"], GOAL);
    assert_eq!(task["candidate"], f.commit);
    assert!(task["scope_epoch"].is_null());
    assert_eq!(task["state_version"], 6);
    assert_eq!(task["phase"], "coordinating");
    assert_eq!(task["pending"], json!([]));
    assert_eq!(task["operations"], json!({}));
    assert_eq!(task["control"], "running");
    assert!(task["writer"].is_null());
    for field in [
        "stage",
        "stage_delivery",
        "approved",
        "final_outcome",
        "cleanup_obligation",
    ] {
        assert!(task[field].is_null(), "parent already advanced: {field}");
    }
    assert!(task["activities"].as_object().unwrap().is_empty());
    for field in ["waits", "events", "deliveries"] {
        assert!(
            q[field].as_object().unwrap().is_empty(),
            "queue already advanced: {field}"
        );
    }
    for field in ["model", "role_models", "binary", "config_sha256"] {
        assert_eq!(
            saved[field], current[field],
            "resume {field} differs from original baseline"
        );
    }
    for field in [
        "pane_id",
        "terminal_id",
        "workspace_id",
        "tab_id",
        "agent_session",
        "cwd",
    ] {
        assert_eq!(
            saved["parent"]["agent"][field], current["parent"]["agent"][field],
            "original parent identity changed: {field}"
        );
    }
    assert_eq!(
        socket(Path::new(saved["socket"].as_str().unwrap())),
        current["socket_identity"]
    );
    assert_eq!(current["socket_identity"], expected["socket_identity"]);
    for name in [
        "stage-start.json",
        "stage-audit.json",
        "stage-result.json",
        "stage-resume.json",
        "stage-watch.log",
    ] {
        assert!(
            !f.temp.path().join(name).exists(),
            "prior send/resume evidence exists: {name}"
        );
    }
    let env_path = Path::new(task["environment"]["state_file"].as_str().unwrap());
    assert_eq!(
        env_path,
        f.queue
            .join("queue.json/task-environments/registry.json/envs/task.json")
    );
    let env = util::read_state_json(env_path.parent().unwrap(), "task.json").unwrap();
    assert_eq!(env["instance_id"], owner["instance_id"]);
    assert_eq!(env["state"], "active");
    assert_eq!(env["creation"]["phase"], "ready");
    assert_eq!(env["creation"]["commit"], f.commit);
    assert_eq!(
        env["workspace_root"],
        json!(f.temp.path().join("workspace"))
    );
    assert_eq!(env["temp_root"], json!(f.temp.path().join("scratch")));
    assert_eq!(env["child_processes"], json!([]));
    assert!(
        env["review_workspaces"].is_null()
            || env["review_workspaces"]
                .as_object()
                .is_some_and(|v| v.is_empty())
    );
    assert!(
        !task_env::cleanup_state_path("task").unwrap().exists(),
        "cleanup obligation already exists"
    );
    assert_eq!(
        env["creation"]["process_boundary"]["boot_id"],
        fs::read_to_string("/proc/sys/kernel/random/boot_id")
            .unwrap()
            .trim()
    );
    assert!(env["stage_resources"].is_null() && env["delivery"].is_null());
    no_send_tree(&f.temp.path().join("scratch/rt"), &owner);
    let original_supervisor_start = env["creation"]["process_boundary"]["start_ticks"]
        .as_u64()
        .filter(|v| *v > 0)
        .unwrap();
    no_marked_process(
        owner["instance_id"].as_str().unwrap(),
        original_supervisor_start,
    );
    let target = f.temp.path().join("output.git");
    assert_eq!(fs::canonicalize(&target).unwrap(), target);
    assert_eq!(saved["agreement"]["1"]["basis"]["endpoint"], json!(target));
    assert_eq!(
        env["delivery_targets"],
        json!([{"path":target,"target_type":"git","description":"private stage acceptance output"}])
    );
    assert_eq!(
        git(&target, &["rev-parse", "refs/heads/accepted"]),
        f.commit
    );
    let checkout = f.temp.path().join("workspace/checkout");
    assert_eq!(git(&checkout, &["rev-parse", "HEAD"]), f.commit);
    assert!(git(
        &checkout,
        &[
            "--no-optional-locks",
            "status",
            "--porcelain",
            "--untracked-files=all"
        ]
    )
    .is_empty());
    assert!(git(&checkout, &["ls-tree", "--name-only", "HEAD"]).is_empty());
    json!({"owner":owner,"baseline_sha256":util::pyjson::sha256_hex(&fs::read(f.temp.path().join("stage-baseline.json")).unwrap()),
        "socket_identity":current["socket_identity"],"process_start_cutoff":original_supervisor_start,"validated":"original pre-send fixture only; no runtime, process or send evidence","original_baseline":saved})
}

#[cfg(test)]
#[path = "stage_live_resume_tests.rs"]
mod tests;
