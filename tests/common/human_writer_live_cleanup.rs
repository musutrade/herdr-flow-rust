//! Final assertions happen before Fixture::drop. No cleanup is manufactured here.
use super::*;
use std::collections::{BTreeMap, BTreeSet};

pub(super) fn verify(
    a: &mut evidence::Audit,
    f: &Fixture,
    config: &evidence::Configuration,
    target: &Path,
) {
    assert!(
        !a.invalidated.is_null(),
        "no observed post-return invalidation checkpoint"
    );
    assert!(a.invalidated["candidate"].is_null() && a.invalidated["approved"].is_null());
    assert!(
        a.invalidated["acceptance_round"].as_u64().unwrap()
            > a.original["task"]["acceptance_round"].as_u64().unwrap()
    );
    let handoff = &a.invalidated["human_writer"];
    assert_eq!(
        handoff["verification"]["model_result"]["disposition"],
        "recovered"
    );
    assert_eq!(
        handoff["verification"]["result"]["disposition"],
        "recovered"
    );
    assert_eq!(
        handoff["verification_proof"]["fingerprint"],
        a.returned["return"]["fingerprint"]
    );
    assert_eq!(
        handoff["verification_proof"]["operation_replay_allowed"],
        false
    );
    assert_ne!(a.acquire["before"], a.returned["return"]["fingerprint"]);
    assert_eq!(a.recovery["fault"]["kind"], "human_writer_return");
    assert_eq!(a.recovery["fault"]["id"], a.returned["return"]["id"]);
    assert_eq!(a.recovery["handoff"]["phase"], "submitted");
    assert_eq!(
        a.recovery["handoff"]["session"],
        a.views["coordinator"]["session"]
    );
    assert_eq!(
        a.terminal["verdicts"]["old-review"],
        a.original["task"]["verdicts"]["old-review"]
    );
    let coordinator_turns = a.views["coordinator"]["thread"]["thread"]["turns"]
        .as_array()
        .unwrap();
    assert!(coordinator_turns
        .iter()
        .any(|turn| turn["id"] == a.recovery["handoff"]["turn"] && turn["status"] == "completed"));
    for name in ["dev", "old-review", "fresh-review"] {
        let events: Vec<_> = a.protocol["events"]
            .as_object()
            .unwrap()
            .values()
            .filter(|event| event["task"] == name && event["applied"] == true)
            .collect();
        assert_eq!(
            events.len(),
            1,
            "expected one applied original result for {name}"
        );
        assert_eq!(events[0]["acknowledged"], true);
        assert!(
            a.protocol["deliveries"]
                .as_object()
                .unwrap()
                .values()
                .any(|delivery| {
                    delivery["phase"] == "acknowledged"
                        && delivery["events"]
                            .as_array()
                            .is_some_and(|ids| ids.contains(&events[0]["id"]))
                }),
            "model ACK receipt missing for {name}"
        );
    }
    assert_eq!(
        a.terminal["final_outcome"], "success",
        "terminal success was not observed before reclamation"
    );
    assert_eq!(a.terminal["candidate"], a.human_sha);
    assert_eq!(a.delivery["phase"], "confirmed");
    assert_eq!(a.terminal["delivery"], a.delivery["id"]);
    assert_eq!(a.delivery["artifact"]["id"], a.human_sha);
    assert_eq!(
        a.terminal["approved"]["round"],
        a.terminal["acceptance_round"]
    );
    assert_ne!(a.terminal["approved"], a.original["task"]["approved"]);
    for name in ["old-review", "fresh-review"] {
        let verdict = &a.terminal["verdicts"][name];
        assert_eq!(verdict["envelope"]["report"]["conclusion"], "passed");
        let criteria: BTreeSet<_> = verdict["envelope"]["report"]["checks"]
            .as_array()
            .unwrap()
            .iter()
            .map(|check| check["criterion"].as_str().unwrap())
            .collect();
        assert_eq!(
            criteria,
            ["answer", "explanation", "operator-note"]
                .into_iter()
                .collect()
        );
    }
    assert_ne!(
        a.terminal["verdicts"]["old-review"]["envelope"]["reviewer"],
        a.terminal["verdicts"]["fresh-review"]["envelope"]["reviewer"]
    );
    assert_eq!(
        json!(git(target, &["rev-parse", "refs/heads/accepted"])),
        a.human_sha
    );
    let sha = a.human_sha.as_str().unwrap();
    audit::bytes(target, sha, "answer.txt", b"42\n");
    audit::bytes(target, sha, "explanation.txt", b"6 * 7 = 42\n");
    audit::bytes(target, sha, "operator-note.txt", NOTE.as_bytes());
    assert_ne!(a.original["checkout_head"], a.human_sha);
    assert_eq!(
        git(target, &["rev-parse", &format!("{sha}^")]),
        a.original["checkout_head"].as_str().unwrap()
    );
    assert_eq!(
        git(
            target,
            &["diff-tree", "--no-commit-id", "--name-only", "-r", sha]
        ),
        "operator-note.txt"
    );
    let roles: BTreeSet<_> = a
        .views
        .values()
        .map(|v| v["role"].as_str().unwrap())
        .collect();
    assert_eq!(
        a.views
            .values()
            .map(|view| view["presentation"]["tab"].as_str().unwrap())
            .collect::<BTreeSet<_>>()
            .len(),
        5,
        "role tabs were reused"
    );
    assert_eq!(
        roles,
        ["coordinator", "developer", "reviewer", "recovery"]
            .into_iter()
            .collect()
    );
    assert_eq!(
        a.views.len(),
        5,
        "expected exactly five separately visible role sessions"
    );
    assert_eq!(
        a.views
            .values()
            .map(|v| v["session"].as_str().unwrap())
            .collect::<BTreeSet<_>>()
            .len(),
        5
    );
    for name in ["coordinator", "dev-a1", "old-review-a1", "fresh-review-a1"] {
        assert!(a.views.contains_key(name));
    }
    assert!(!a.processes.is_empty());
    for view in a.views.values() {
        let marker = &view["native_marker"];
        for (pid, ticks) in [
            ("pid", "pid_start_ticks"),
            ("wrapper_pid", "wrapper_start_ticks"),
        ] {
            let start = marker[ticks]
                .as_u64()
                .or_else(|| marker[ticks].as_str().and_then(|s| s.parse().ok()))
                .unwrap();
            assert!(
                !audit::exact_process_alive(&json!({"pid":marker[pid],"start_time":start})),
                "owned frontend survived"
            );
        }
    }
    for process in a.processes.values() {
        registered_process_absent(process);
    }
    // Private exact cwd inventories must be completely empty, including unknown
    // Guardian children. The same complete attached/pagination helper is used by
    // stage acceptance; memory-only absence is separately recorded there.
    let parents: Vec<_> = a.sessions.keys().cloned().collect();
    let cwds: BTreeSet<_> = a.sessions.values().cloned().collect();
    let mut absence = Vec::new();
    for cwd in cwds {
        let expected: BTreeMap<_, _> = a
            .sessions
            .iter()
            .filter(|(_, c)| **c == cwd)
            .map(|(id, c)| (id.clone(), c.clone()))
            .collect();
        let proof = audit::native_absence(&config.socket, &cwd, &expected, &parents);
        for page in proof["complete_persistent_inventories"].as_array().unwrap() {
            assert!(
                page["rows"].as_array().unwrap().is_empty(),
                "private cwd retains native sessions"
            );
        }
        absence.push(proof);
    }
    let panes = herdr(&[
        "pane",
        "list",
        "--workspace",
        config.parent_before["agent"]["workspace_id"]
            .as_str()
            .unwrap(),
    ]);
    let tabs = herdr(&[
        "tab",
        "list",
        "--workspace",
        config.parent_before["agent"]["workspace_id"]
            .as_str()
            .unwrap(),
    ]);
    presentations_absent(&panes, &tabs, &a.views);
    let sessions: BTreeSet<_> = a.sessions.keys().map(String::as_str).collect();
    let disk = disk_inventory(config, &sessions);
    assert!(!a.workspace.exists() && !a.scratch.exists());
    assert!(!task_env::is_registered("task").unwrap());
    assert!(
        task_env::load_registry().unwrap().is_empty(),
        "private registry retains root claims"
    );
    resume::no_marked_process(a.owner["instance_id"].as_str().unwrap(), a.process_start);
    let q = f.state();
    assert!(
        q["tasks"].as_object().unwrap().is_empty(),
        "parent/child queue inventory remains"
    );
    assert!(task_env::terminal::pending_terminal_owners(&f.queue)
        .unwrap()
        .is_empty());
    assert!(!git(
        &f.temp.path().join("repo"),
        &["worktree", "list", "--porcelain"]
    )
    .contains(a.workspace.to_str().unwrap()));
    a.cleanup = json!({"native_absence":absence,"disk_and_sql_associations":disk,"roots_removed":true,
        "registered_processes_removed":true,"queue_empty":true,"registration_removed":true,
        "terminal_obligations_empty":true,"worktree_removed":true,"herdr_live_and_saved_associations_absent":true});
}

fn registered_process_absent(process: &J) {
    let registered: task_env::RegisteredProcess = serde_json::from_value(process.clone()).unwrap();
    // Saved descendants retain ownership even after their parent exits or their
    // marker changes. direct_children is diagnostic and can belong to others.
    let root = task_env::ProcessRecord {
        pid: registered.pid,
        start_time: registered.start_time,
    };
    for identity in std::iter::once(&root).chain(&registered.descendants) {
        assert!(
            !audit::exact_process_alive(&serde_json::to_value(identity).unwrap()),
            "owned registered process identity survived: {identity:?}"
        );
    }
}

fn presentations_absent(panes: &J, tabs: &J, views: &BTreeMap<String, J>) {
    for view in views.values() {
        for key in ["pane", "terminal", "tab"] {
            for inventory in [panes, tabs] {
                assert!(
                    !audit::contains_identity(inventory, &view["presentation"][key]),
                    "owned Herdr {key} remains in live inventory"
                );
            }
        }
    }
}

fn disk_inventory(config: &evidence::Configuration, sessions: &BTreeSet<&str>) -> J {
    let spec = json!({"databases":config.databases,"native_data":config.native_data,"sessions":sessions,
        "herdr_store":config.store});
    let mut child = Command::new("python3")
        .arg("-c")
        .arg(include_str!("../fixtures/human_live_inventory.py"))
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    use std::io::Write;
    child
        .stdin
        .take()
        .unwrap()
        .write_all(spec.to_string().as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "read-only disk/SQL inventory: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).unwrap()
}

#[cfg(test)]
#[path = "human_writer_live_inventory_tests.rs"]
mod tests;
