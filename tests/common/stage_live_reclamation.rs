//! Read-only checks at the successor barrier; no delivery or lifecycle mutations.
use super::*;
#[path = "stage_live_deletion.rs"]
mod deletion;

pub(super) fn contains_identity(inventory: &J, identity: &J) -> bool {
    match inventory {
        J::String(_) => inventory == identity,
        J::Array(items) => items.iter().any(|item| contains_identity(item, identity)),
        J::Object(items) => items.values().any(|item| contains_identity(item, identity)),
        _ => false,
    }
}

pub(super) fn process_alive(process: &J) -> bool {
    let pid = process["pid"].as_i64().unwrap();
    assert!(
        pid > 1 && pid <= i32::MAX as i64,
        "invalid process identity"
    );
    let expected = process["start_time"].as_u64().unwrap();
    match fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(stat) => {
            let fields: Vec<_> = stat
                .rsplit_once(')')
                .unwrap()
                .1
                .split_whitespace()
                .collect();
            let actual: u64 = fields[19].parse().unwrap();
            actual == expected && !matches!(fields[0], "Z" | "X" | "x")
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            // Missing procfs alone is not proof. Signal zero only observes existence.
            // SAFETY: positive PID, signal zero never delivers a signal.
            assert_eq!(unsafe { libc::kill(pid as i32, 0) }, -1);
            assert_eq!(
                std::io::Error::last_os_error().raw_os_error(),
                Some(libc::ESRCH)
            );
            false
        }
        Err(error) => panic!("cannot observe exact process identity {pid}: {error}"),
    }
}

pub(super) fn rpc(socket: &str, session: &J) -> herdr_flow::Result<J> {
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut client = herdr_flow::codex::rpc::Client::new(socket);
    client.timeout = 5.0;
    client.connect()?;
    client.timeout = deadline
        .saturating_duration_since(Instant::now())
        .as_secs_f64();
    assert!(
        client.timeout > 0.0,
        "audit RPC initialization exhausted budget"
    );
    let reply = client.call(
        "thread/read",
        &json!({"threadId":session,"includeTurns":false}),
    );
    assert!(Instant::now() < deadline, "audit RPC exhausted budget");
    reply
}

/// Parent cleanup summary and persisted plan must agree at an admitted successor.
/// This deliberately does not depend on seeing the transient pending phase.
pub(super) fn completed_plan(parent: &J, env: &J, owner: &J) -> J {
    assert!(
        parent["final_outcome"].is_null(),
        "early audit ran after final outcome"
    );
    let plan = &env["stage_resources"];
    let cleanup = &parent["stage_delivery"]["resource_cleanup"];
    assert_eq!(plan["owner"], *owner);
    assert_eq!(plan["delivery"], parent["stage_delivery"]["delivery"]["id"]);
    assert_eq!(plan["scope_epoch"], 1);
    assert_eq!(plan["phase"], "complete");
    assert_eq!(cleanup["phase"], "complete");
    assert_eq!(cleanup["delivery"], plan["delivery"]);
    assert_eq!(cleanup["selected"], 2);
    assert_eq!(cleanup["completed"], 2);
    assert!(cleanup["error"].is_null());
    assert_eq!(plan["retained"], json!([]));
    let entries = plan["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 2);
    let assignments: std::collections::BTreeSet<_> = entries
        .iter()
        .map(|entry| entry["binding"]["assignment"].as_str().unwrap())
        .collect();
    assert_eq!(
        assignments,
        ["dev-a1", "stage-review-a1"].into_iter().collect()
    );
    for entry in entries {
        assert_eq!(entry["phase"], "complete");
        assert_eq!(entry["binding"]["lineage"]["scope_epoch"], 1);
        assert_eq!(entry["runtime"]["lifecycle_owner"], *owner);
        assert_eq!(entry["broker"]["pid"], entry["runtime"]["pid"]);
        assert_eq!(
            entry["broker"]["start_time"],
            entry["runtime"]["start_time"]
        );
        assert_eq!(entry["presentation"]["phase"], "closed");
        assert_eq!(entry["frontends"].as_array().unwrap().len(), 2);
        let sessions = entry["sessions"].as_array().unwrap();
        assert!(!sessions.is_empty() && sessions.len() <= 128);
        for session in sessions {
            assert_eq!(session["phase"], "deleted");
        }
    }
    plan.clone()
}

pub(super) fn observe(
    audit: &StageAudit,
    f: &Fixture,
    q: &J,
    env: &J,
    socket: &str,
    store: &Path,
) -> J {
    let plan = completed_plan(&q["tasks"]["task"], env, &audit.owner);
    let panes = herdr(&[
        "pane",
        "list",
        "--workspace",
        audit.parent["agent"]["workspace_id"].as_str().unwrap(),
    ]);
    let tabs = herdr(&[
        "tab",
        "list",
        "--workspace",
        audit.parent["agent"]["workspace_id"].as_str().unwrap(),
    ]);
    let mut expected_sessions = BTreeMap::new();
    let workspace = PathBuf::from(env["workspace_root"].as_str().unwrap());
    let scope = [
        workspace.join("checkout"),
        workspace.join("reviews/stage-review-a1"),
    ];
    let mut originals = std::collections::BTreeSet::new();
    for entry in plan["entries"].as_array().unwrap() {
        let name = entry["binding"]["assignment"].as_str().unwrap();
        let original = &audit.views[name];
        assert_eq!(entry["runtime"]["root"], entry["root"]);
        assert_eq!(entry["sessions"][0]["session"], original["session"]);
        assert_eq!(entry["runtime"]["cwd"], original["cwd"]);
        for process in
            std::iter::once(&entry["broker"]).chain(entry["frontends"].as_array().unwrap())
        {
            assert!(
                !process_alive(process),
                "retired exact process still alive: {process}"
            );
        }
        for (inventory, fields) in [(&panes, &["pane", "terminal"][..]), (&tabs, &["tab"][..])] {
            for field in fields {
                assert!(
                    !contains_identity(inventory, &entry["presentation"][*field]),
                    "retired {field} remains in live inventory"
                );
            }
        }
        for session in entry["sessions"].as_array().unwrap() {
            let id = session["session"].as_str().unwrap();
            assert_eq!(session["instance"], audit.owner["instance_id"]);
            assert_eq!(session["runtime"]["lifecycle_owner"], audit.owner);
            assert_eq!(session["runtime"]["cwd"], entry["runtime"]["cwd"]);
            assert_eq!(session["runtime"]["upstream_socket"]["path"], socket);
            let cwd = PathBuf::from(session["runtime"]["cwd"].as_str().unwrap());
            assert!(
                expected_sessions.insert(id.to_owned(), cwd).is_none(),
                "duplicate original session binding"
            );
            originals.insert(id);
        }
        let root = Path::new(entry["root"].as_str().unwrap());
        assert_eq!(
            read(root, "stage-retirement.json").unwrap()["phase"],
            "retired"
        );
        assert_eq!(read(root, "runtime.json").unwrap(), entry["runtime"]);
        assert_eq!(
            read(root, "upstream-session.json").unwrap(),
            entry["sessions"][0]
        );
        assert!(read(root, "collection-receipt.json").is_some());
    }
    let parents: std::collections::BTreeSet<String> = audit
        .views
        .values()
        .map(|view| view["session"].as_str().unwrap().to_owned())
        .chain(expected_sessions.keys().cloned())
        .collect();
    let deletion_proof = deletion::observe(
        socket,
        &scope,
        &expected_sessions,
        &parents.into_iter().collect::<Vec<_>>(),
    );
    saved_presentations_absent(store, &originals);
    let mut alive = BTreeMap::new();
    for name in ["coordinator", "final-dev-a1"] {
        let view = &audit.views[name];
        let root = audit.root.join(name);
        let runtime = read(&root, "runtime.json").unwrap();
        assert_eq!(runtime["lifecycle_owner"], audit.owner);
        assert!(
            process_alive(&json!({"pid":runtime["pid"],"start_time":runtime["start_time"]})),
            "{name} broker is not alive"
        );
        let agent = herdr(&[
            "agent",
            "get",
            view["presentation"]["pane"].as_str().unwrap(),
        ]);
        assert!(agent
            .to_string()
            .contains(view["session"].as_str().unwrap()));
        let reply = rpc(socket, &view["session"]).unwrap();
        assert_eq!(reply["thread"]["id"], view["session"]);
        assert_eq!(reply["thread"]["cwd"], runtime["cwd"]);
        alive.insert(name, json!({"runtime":runtime,"presentation":view["presentation"],"thread":reply["thread"]["id"]}));
    }
    retained_review(audit, env);
    let after = f.state();
    assert_eq!(
        serde_json::to_value(lifecycle::task_owner(&after, "task").unwrap().unwrap()).unwrap(),
        audit.owner
    );
    assert_eq!(after["tasks"]["task"]["requirements"], audit.requirement);
    assert!(
        after["tasks"]["task"]["final_outcome"].is_null(),
        "early audit overlapped terminal finalization"
    );
    assert!(task_env::is_registered("task").unwrap());
    json!({"barrier":"fresh final-dev presentation; before final parent outcome","plan":plan,
        "original_sessions_absent":deletion_proof,"same_owner_alive":alive,
        "parent_before":q["tasks"]["task"],"parent_after":after["tasks"]["task"],
        "review_workspace":env["review_workspaces"]["stage-review-a1"],"registration_retained":true})
}

pub(super) fn retained_review(audit: &StageAudit, env: &J) {
    let record = &env["review_workspaces"]["stage-review-a1"];
    assert_eq!(record["owner"], audit.owner);
    assert_eq!(record["phase"], "ready");
    assert_eq!(record["binding"]["assignment"], "stage-review-a1");
    let clone = Path::new(record["path"].as_str().unwrap());
    assert!(
        clone.join(".git/objects").is_dir(),
        "stage review object store was removed early"
    );
    let sha = audit.stage["delivery"]["artifact"]["id"].as_str().unwrap();
    assert_eq!(git(clone, &["rev-parse", "HEAD"]), sha);
    bytes(clone, sha, "answer.txt", b"42\n");
    assert_eq!(git(clone, &["cat-file", "-t", sha]), "commit");
    if !audit.early.is_null() {
        assert_eq!(
            *record, audit.early["review_workspace"],
            "retained review registration changed"
        );
    }
}

#[cfg(test)]
#[path = "stage_live_reclamation_tests.rs"]
mod tests;

pub(super) fn native_snapshot(socket: &str, cwd: &Path, parents: &[String]) -> J {
    deletion::snapshot(socket, cwd, parents)
}
pub(super) fn native_absence(
    socket: &str,
    scope: &[PathBuf],
    expected: &BTreeMap<String, PathBuf>,
    parents: &[String],
) -> J {
    deletion::observe(socket, scope, expected, parents)
}
