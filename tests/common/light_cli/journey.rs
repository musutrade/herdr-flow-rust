use crate::support::*;
use serde_json::json;
use std::{fs, os::unix::fs::symlink};

#[test]
fn two_project_multi_batch_journey_with_history_bindings_and_backup() {
    let f = Fixture::new();
    let p1 = f.init_project();
    let root = f.temp.path().join("project2");
    fs::create_dir(&root).unwrap();
    let p2 = f.ok(
        &[
            "project",
            "register",
            "--root",
            path(&root),
            "--label",
            "second",
        ],
        None,
    )["id"]
        .as_i64()
        .unwrap();
    let alias = f.temp.path().join("alias");
    symlink(&f.cwd, &alias).unwrap();
    let original = f.ok(
        &[
            "project",
            "register",
            "--root",
            path(&alias),
            "--label",
            "ignored new label",
        ],
        None,
    );
    assert_eq!(original["id"], p1);
    assert_eq!(original["label"], "first");
    assert_eq!(
        f.ok(&["project", "list", "--limit", "1"], None)["next_after_id"],
        p1
    );
    assert_eq!(
        f.ok(&["project", "list", "--after-id", &p1.to_string()], None)["items"][0]["id"],
        p2
    );
    let a = f.ok(
        &["task", "add", "--input", "-"],
        Some(&task("a", p1, Some("same-batch"))),
    );
    let b = f.ok(
        &["task", "add", "--input", "-"],
        Some(&task("b", p1, Some("same-batch"))),
    );
    f.ok(
        &["task", "add", "--input", "-"],
        Some(&task("another-batch", p1, Some("batch-2"))),
    );
    let bid = b["task_id"].as_i64().unwrap();
    f.ok(
        &["binding", "attach", "--input", "-"],
        Some(&attach("same-pane", bid, 1, "developer")),
    );
    assert_eq!(
        f.ok(
            &[
                "binding",
                "show",
                "--project",
                &p1.to_string(),
                "--task",
                &bid.to_string()
            ],
            None
        )["items"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    let other = f.ok(
        &["task", "add", "--input", "-"],
        Some(&task("other", p2, None)),
    );
    assert_ne!(a["task_id"], b["task_id"]);
    let id = a["task_id"].as_i64().unwrap();
    let idstr = id.to_string();
    let req = report("report", id, 1);
    let input = f.request_file("report.json", &req);
    let receipt = f.ok(&["task", "report", "--input", path(&input)], None);
    assert_eq!(
        f.ok(&["task", "report", "--input", "-"], Some(&req)),
        receipt
    );
    f.ok(
        &["binding", "attach", "--input", "-"],
        Some(&attach("dev", id, 2, "developer")),
    );
    let mut reviewer = attach("reviewer", id, 3, "auditor");
    reviewer["change"]["binding"]["session"] =
        json!({"agent":"hermes","kind":"id","source":"herdr:hermes","value":"opaque"});
    f.ok(&["binding", "attach", "--input", "-"], Some(&reviewer));
    let roles = f.ok(
        &[
            "binding",
            "show",
            "--project",
            &p1.to_string(),
            "--task",
            &idstr,
        ],
        None,
    );
    assert_eq!(roles["items"].as_array().unwrap().len(), 2);
    assert!(roles["items"][0]["details"]["session"].is_null());
    assert_eq!(roles["items"][1]["details"]["session"]["agent"], "hermes");
    let list = f.ok(
        &[
            "task",
            "list",
            "--project",
            &p1.to_string(),
            "--batch",
            "same-batch",
            "--limit",
            "1",
        ],
        None,
    );
    assert_eq!(list["items"][0]["id"], id);
    assert_eq!(list["next_after_id"], id);
    assert_eq!(
        f.ok(
            &[
                "task",
                "list",
                "--project",
                &p1.to_string(),
                "--after-id",
                &idstr
            ],
            None
        )["items"][0]["id"],
        b["task_id"]
    );
    assert_eq!(
        f.ok(&["task", "list", "--project", &p2.to_string()], None)["items"][0]["id"],
        other["task_id"]
    );
    let show = f.ok(
        &[
            "task",
            "show",
            "--project",
            &p1.to_string(),
            "--task",
            &idstr,
            "--limit",
            "2",
        ],
        None,
    );
    assert_eq!(show["task"]["revision"], 4);
    assert_eq!(show["events"]["items"].as_array().unwrap().len(), 2);
    assert_eq!(
        f.ok(
            &[
                "task",
                "show",
                "--project",
                &p1.to_string(),
                "--task",
                &idstr,
                "--after-id",
                &show["events"]["next_after_id"].to_string()
            ],
            None
        )["events"]["items"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    error(
        f.run(
            &[
                "task",
                "show",
                "--project",
                &p2.to_string(),
                "--task",
                &idstr,
            ],
            None,
        ),
        3,
        "not_found",
    );
    error(
        f.run(
            &[
                "binding",
                "show",
                "--project",
                &p2.to_string(),
                "--task",
                &idstr,
            ],
            None,
        ),
        3,
        "not_found",
    );
    let backup = f.temp.path().join("backup");
    f.ok(&["backup", "--output-root", path(&backup)], None);
    let mut c = f.command(env!("CARGO_BIN_EXE_herdr-flow"));
    c.args([
        "light",
        "--data-root",
        path(&backup),
        "task",
        "show",
        "--project",
        &p1.to_string(),
        "--task",
        &idstr,
        "--limit",
        "2",
    ]);
    assert_eq!(success(output(c, None)), show);
    f.no_calls();
}

#[test]
fn missing_ids_conflicts_and_unavailable_store_have_stable_errors() {
    let f = Fixture::new();
    let p = f.init_project();
    error(
        f.run(
            &["task", "add", "--input", "-"],
            Some(&task("unknown", 999, None)),
        ),
        3,
        "not_found",
    );
    assert_eq!(
        f.ok(&["task", "list", "--project", "999"], None)["items"],
        json!([])
    );
    assert_eq!(
        f.ok(
            &["binding", "show", "--project", "999", "--task", "999"],
            None
        )["items"],
        json!([])
    );
    error(
        f.run(&["task", "show", "--project", "999", "--task", "999"], None),
        3,
        "not_found",
    );
    let t = f.ok(
        &["task", "add", "--input", "-"],
        Some(&task("create", p, None)),
    )["task_id"]
        .as_i64()
        .unwrap();
    let r = report("stable", t, 1);
    f.ok(&["task", "report", "--input", "-"], Some(&r));
    let mut changed = r.clone();
    changed["summary"] = json!("different");
    error(
        f.run(&["task", "report", "--input", "-"], Some(&changed)),
        4,
        "request_conflict",
    );
    let e = error(
        f.run(
            &["task", "report", "--input", "-"],
            Some(&report("stale", t, 1)),
        ),
        4,
        "revision_conflict",
    );
    assert_eq!(e["expected"], 1);
    assert_eq!(e["actual"], 2);
    let moved = f.temp.path().join("saved");
    fs::rename(&f.data, &moved).unwrap();
    error(
        f.run(&["task", "report", "--input", "-"], Some(&r)),
        6,
        "storage_unavailable",
    );
    assert!(!f.data.exists());
    fs::rename(moved, &f.data).unwrap();
    f.ok(&["task", "report", "--input", "-"], Some(&r));
    let shown = f.ok(
        &[
            "task",
            "show",
            "--project",
            &p.to_string(),
            "--task",
            &t.to_string(),
        ],
        None,
    );
    assert_eq!(shown["events"]["items"].as_array().unwrap().len(), 2);
    f.no_calls();
}

#[test]
fn malformed_or_oversized_input_and_options_do_not_create_a_store() {
    let f = Fixture::new();
    for args in [
        vec!["observe"],
        vec!["task", "list"],
        vec!["init", "--limit", "1"],
        vec!["init", "--data-root", "a"],
        vec!["init", "--busy-timeout-ms", "5001"],
        vec!["project", "list", "--limit", "0"],
        vec!["task", "show", "--project", "-1", "--task", "1"],
    ] {
        error(f.run(&args, None), 2, "usage");
    }
    let mut r = task("x", 1, None);
    r["extra"] = json!(true);
    error(
        f.run(&["task", "add", "--input", "-"], Some(&r)),
        2,
        "usage",
    );
    let mut r = task("x", 1, None);
    r["change"]["task"]["typo"] = json!(1);
    error(
        f.run(&["task", "add", "--input", "-"], Some(&r)),
        2,
        "usage",
    );
    error(
        f.run(
            &["task", "report", "--input", "-"],
            Some(&task("x", 1, None)),
        ),
        2,
        "usage",
    );
    error(
        f.run(
            &["task", "add", "--input", "-"],
            Some(&json!("x".repeat(65537))),
        ),
        2,
        "usage",
    );
    assert!(!f.data.exists());
}

#[test]
fn busy_schema_and_path_protection_are_exposed_without_retry() {
    let f = Fixture::new();
    let p = f.init_project();
    let db = rusqlite::Connection::open(f.data.join("records.sqlite3")).unwrap();
    db.execute_batch("BEGIN IMMEDIATE").unwrap();
    error(
        f.run(
            &["task", "add", "--input", "-", "--busy-timeout-ms", "0"],
            Some(&task("busy", p, None)),
        ),
        5,
        "busy",
    );
    db.execute_batch("ROLLBACK").unwrap();
    f.ok(
        &["task", "add", "--input", "-"],
        Some(&task("busy", p, None)),
    );
    db.pragma_update(None, "user_version", 2).unwrap();
    error(f.run(&["project", "list"], None), 6, "unsupported_schema");
    db.pragma_update(None, "user_version", 1).unwrap();
    error(
        f.run(&["init", "--forbid", path(f.temp.path())], None),
        2,
        "invalid_input",
    );
    let alias = f.temp.path().join("alias");
    symlink(&f.data, &alias).unwrap();
    let mut c = f.command(env!("CARGO_BIN_EXE_herdr-flow"));
    c.args([
        "light",
        "--data-root",
        path(&alias),
        "--forbid",
        path(&f.data),
        "init",
    ]);
    error(output(c, None), 2, "invalid_input");
    f.no_calls();
}

#[test]
fn default_state_path_and_separate_help_are_available_offline() {
    let f = Fixture::new();
    let mut c = f.command(env!("CARGO_BIN_EXE_herdr-flow"));
    c.args(["light", "init"]);
    let root = f.temp.path().join("state/herdr-light/records.sqlite3");
    assert_eq!(success(output(c, None))["database_path"], path(&root));
    let out = f.run(&["--help"], None);
    assert!(out.status.success());
    let light_help = String::from_utf8(out.stdout).unwrap();
    assert!(light_help.contains("Records and optional observations"));
    assert!(light_help.contains("Record commands require no Herdr connection"));
    assert!(light_help.contains("versions are diagnostic only"));
    let mut c = f.command(env!("CARGO_BIN_EXE_herdr-flow"));
    c.arg("--help");
    let help = c.output().unwrap();
    let help = String::from_utf8(help.stdout).unwrap();
    assert!(help.contains("Record commands:"));
    assert!(help.contains("Legacy control commands:"));
    let mut c = f.command(env!("CARGO_BIN_EXE_herdr-flow"));
    c.env("XDG_STATE_HOME", "relative").args(["light", "init"]);
    error(output(c, None), 2, "usage");
    f.no_calls();
}

#[test]
fn input_files_private_paths_and_output_failure_have_explicit_outcomes() {
    use std::{io::Write, os::unix::fs::PermissionsExt, process::Stdio};
    let f = Fixture::new();
    let p = f.init_project();
    for bytes in [b"not json".as_slice(),br#"{"request_id":"a","request_id":"b","reporter":"dev","summary":"x","refs":[],"change":{"kind":"report","task_id":1,"expected_revision":1,"reported_state":"x"}}"#] {
        let input=f.temp.path().join("invalid.json");fs::write(&input,bytes).unwrap();
        error(f.run(&["task","report","--input",path(&input)],None),2,"usage");
    }
    error(
        f.run(&["task", "add", "--input", path(&f.cwd)], None),
        2,
        "usage",
    );
    error(
        f.run(
            &[
                "task",
                "add",
                "--input",
                path(&f.temp.path().join("absent")),
            ],
            None,
        ),
        6,
        "storage_unavailable",
    );
    let mut invalid = task("invalid", p, None);
    invalid["request_id"] = json!("");
    error(
        f.run(&["task", "add", "--input", "-"], Some(&invalid)),
        2,
        "invalid_input",
    );
    let backup = f.temp.path().join("backup");
    f.ok(&["backup", "--output-root", path(&backup)], None);
    error(
        f.run(&["backup", "--output-root", path(&backup)], None),
        6,
        "storage_unavailable",
    );
    fs::set_permissions(&f.data, fs::Permissions::from_mode(0o755)).unwrap();
    error(f.run(&["project", "list"], None), 2, "invalid_input");
    fs::set_permissions(&f.data, fs::Permissions::from_mode(0o700)).unwrap();
    // A failed stdout write is not a failed transaction. Retry the exact request.
    let req = task("lost-output", p, None);
    let mut c = f.command(env!("CARGO_BIN_EXE_herdr-flow"));
    c.args([
        "light",
        "--data-root",
        path(&f.data),
        "task",
        "add",
        "--input",
        "-",
    ])
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped());
    let mut child = c.spawn().unwrap();
    drop(child.stdout.take());
    child
        .stdin
        .take()
        .unwrap()
        .write_all(req.to_string().as_bytes())
        .unwrap();
    error(child.wait_with_output().unwrap(), 6, "output_unavailable");
    let receipt = f.ok(&["task", "add", "--input", "-"], Some(&req));
    assert_eq!(receipt["revision"], 1);
    assert_eq!(
        f.ok(&["task", "list", "--project", &p.to_string()], None)["items"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    f.no_calls();
}

#[test]
fn home_fallback_and_empty_environment_errors_are_deterministic() {
    let f = Fixture::new();
    let mut c = f.command(env!("CARGO_BIN_EXE_herdr-flow"));
    c.env_remove("XDG_STATE_HOME").args(["light", "init"]);
    assert_eq!(
        success(output(c, None))["database_path"],
        path(&f.home.join(".local/state/herdr-light/records.sqlite3"))
    );
    let mut c = f.command(env!("CARGO_BIN_EXE_herdr-flow"));
    c.env_remove("XDG_STATE_HOME")
        .env_remove("HOME")
        .args(["light", "init"]);
    error(output(c, None), 2, "usage");
    error(
        f.run(&["project", "list", "--limit", "not-a-number"], None),
        2,
        "usage",
    );
    error(f.run(&["project", "list", "--limit"], None), 2, "usage");
    error(f.run(&["project", "other"], None), 2, "usage");
}
