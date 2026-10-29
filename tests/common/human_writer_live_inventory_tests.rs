//! Audit false-pass regressions; no real service or model is contacted.
use super::*;

fn run(spec: &J) -> std::process::Output {
    use std::io::Write;
    let mut child = Command::new("python3")
        .arg("-c")
        .arg(include_str!("../fixtures/human_live_inventory.py"))
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(spec.to_string().as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

#[test]
fn final_inventory_rejects_sql_json_rollouts_and_saved_herdr_backups() {
    let temp = tempfile::tempdir().unwrap();
    let database = temp.path().join("associations.sqlite");
    let store = temp.path().join("session.json");
    let native = temp.path().join("native");
    fs::create_dir(&native).unwrap();
    fs::write(
        &store,
        json!({"version":3,"workspaces":[],"unrelated_log_text":SESSION}).to_string(),
    )
    .unwrap();
    let out = Command::new("python3").arg("-c").arg(
        "import sqlite3,sys; db=sqlite3.connect(sys.argv[1]); db.execute('CREATE TABLE associations (metadata TEXT)'); db.execute('INSERT INTO associations VALUES (?)', (sys.argv[2],)); db.commit(); db.close()"
    ).arg(&database).arg(json!({"native_session":SESSION}).to_string()).output().unwrap();
    assert!(out.status.success());
    let spec = json!({"sessions":[SESSION],"databases":[{"path":database,"json_relations":[{"table":"associations","column":"metadata","path":"$.native_session","meaning":"native-session-association","source":"explicit fixture association schema"}]}],"native_data":native,"herdr_store":store});
    let before = fs::read(&database).unwrap();
    let blocked = run(&spec);
    assert!(!blocked.status.success());
    assert!(String::from_utf8_lossy(&blocked.stderr).contains("owned session association remains"));
    assert_eq!(
        fs::read(&database).unwrap(),
        before,
        "audit changed shared database"
    );
    let clear = Command::new("python3").arg("-c").arg(
        "import sqlite3,sys; db=sqlite3.connect(sys.argv[1]); db.execute('DELETE FROM associations'); db.execute('CREATE TABLE logs (thread_id TEXT, message TEXT)'); db.execute('INSERT INTO logs VALUES (?, ?)', ('external-supervisor', sys.argv[2])); db.commit(); db.close()"
    ).arg(&database).arg(format!("External observer mentioned {SESSION}; preserve its log")).output().unwrap();
    assert!(clear.status.success());
    let edge = Command::new("python3").arg("-c").arg(
        "import sqlite3,sys; db=sqlite3.connect(sys.argv[1]); db.execute('CREATE TABLE thread_spawn_edges (parent_thread_id TEXT, child_thread_id TEXT)'); db.execute('INSERT INTO thread_spawn_edges VALUES (?, ?)', ('external-parent', sys.argv[2])); db.commit(); db.close()"
    ).arg(&database).arg(SESSION).output().unwrap();
    assert!(edge.status.success());
    assert!(
        !run(&spec).status.success(),
        "typed child_thread_id association was ignored"
    );
    let clear_edge = Command::new("python3").arg("-c").arg(
        "import sqlite3,sys; db=sqlite3.connect(sys.argv[1]); db.execute('DELETE FROM thread_spawn_edges'); db.execute('CREATE TABLE rollout_migration_state (last_checked_thread_id TEXT)'); db.execute('INSERT INTO rollout_migration_state VALUES (?)', (sys.argv[2],)); db.execute('CREATE TABLE jobs (ownership_token TEXT)'); db.execute('INSERT INTO jobs VALUES (?)', (sys.argv[2],)); db.commit(); db.close()"
    ).arg(&database).arg(SESSION).output().unwrap();
    assert!(clear_edge.status.success());
    let archived = native.join("archived_sessions");
    fs::create_dir(&archived).unwrap();
    let rollout = archived.join(format!("rollout-fixture-{SESSION}.jsonl"));
    fs::write(&rollout, b"{}").unwrap();
    assert!(!run(&spec).status.success(), "archived rollout was ignored");
    fs::remove_file(rollout).unwrap();
    let backups = temp.path().join("session-backups");
    fs::create_dir(&backups).unwrap();
    let backup = backups.join("backup.json");
    fs::write(&backup, json!({"version":3,"workspaces":[{"tabs":[{"panes":{"1":{"agent_session":{"agent":"codex","value":SESSION}}}}]}]}).to_string()).unwrap();
    assert!(
        !run(&spec).status.success(),
        "Herdr backup association was ignored"
    );
    fs::remove_file(backup).unwrap();
    let before = fs::read(&database).unwrap();
    let passed = run(&spec);
    assert!(
        passed.status.success(),
        "{}",
        String::from_utf8_lossy(&passed.stderr)
    );
    assert_eq!(fs::read(&database).unwrap(), before);
    assert_eq!(
        serde_json::from_slice::<J>(&passed.stdout).unwrap()["owned_associations_absent"],
        true
    );
}

#[test]
fn live_herdr_inventory_requires_exact_ids_and_preserves_unrelated_mentions() {
    let views = [(
        "coordinator".into(),
        json!({"presentation":{"pane":"wJ:p3","tab":"wJ:t2","terminal":"terminal-7"}}),
    )]
    .into_iter()
    .collect();
    let panes = json!({"panes":[{"id":"wJ:p3A","label":"diagnostic mentions wJ:p3, wJ:t2 and terminal-7"}]});
    let tabs = json!({"tabs":[{"id":"wJ:t2A","terminal":"terminal-7-extra"}]});
    presentations_absent(&panes, &tabs, &views);
    for identity in ["wJ:p3", "wJ:t2", "terminal-7"] {
        assert!(
            std::panic::catch_unwind(|| presentations_absent(
                &json!({"nested":[{"id":identity}]}),
                &tabs,
                &views
            ))
            .is_err(),
            "an exact owned resource identity was ignored"
        );
    }
}

#[test]
fn dead_root_does_not_hide_a_saved_live_descendant_with_an_unrelated_marker() {
    use std::os::unix::process::CommandExt;
    let mut root = FixtureChild(
        Command::new("sleep")
            .arg("60")
            .process_group(0)
            .spawn()
            .unwrap(),
    );
    let mut descendant = FixtureChild(
        Command::new("sleep")
            .arg("60")
            .env(task_env::MARKER_ENV, "unrelated-audit-fixture")
            .process_group(0)
            .spawn()
            .unwrap(),
    );
    let root_pid = root.0.id() as i32;
    let child_pid = descendant.0.id() as i32;
    let mut record = json!({"pid":root_pid,"pgid":root_pid,"start_time":task_env::process_start_time(root_pid).unwrap(),
        "descendants":[{"pid":child_pid,"start_time":task_env::process_start_time(child_pid).unwrap()}],"direct_children":[]});
    root.0.kill().unwrap();
    root.0.wait().unwrap();
    // This saved ownership snapshot, rather than a current marker or live root,
    // must keep the descendant in the final absence proof.
    assert!(std::panic::catch_unwind(|| registered_process_absent(&record)).is_err());
    record["direct_children"] = record["descendants"].clone();
    record["descendants"] = json!([]);
    registered_process_absent(&record); // unrelated diagnostic children are not owned.
    descendant.0.kill().unwrap();
    descendant.0.wait().unwrap();
    record["descendants"] = record["direct_children"].clone();
    registered_process_absent(&record);
    record["descendants"][0]["start_time"] = J::Null;
    assert!(
        std::panic::catch_unwind(|| registered_process_absent(&record)).is_err(),
        "missing identity must fail closed"
    );
}
