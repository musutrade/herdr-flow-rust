//! Regressions for the three P1 audit findings; all repositories are local.
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use herdr_flow::ci::watch::GitHubApi;
use herdr_flow::model::J;
use herdr_flow::util::process::{run_command, CommandOptions};
use herdr_flow::{batch, pr, util, ErrorKind};
use serde_json::json;

fn shell(script: &str, stdin: Option<Vec<u8>>) -> herdr_flow::Result<util::CommandOutput> {
    run_command(
        &["/bin/sh".into(), "-c".into(), script.into()],
        &CommandOptions {
            timeout: Duration::from_millis(150),
            stdin,
            ..Default::default()
        },
    )
}

#[test]
fn command_deadline_covers_descendant_pipes_and_blocked_stdin() {
    for script in [
        "sleep 1 & exit 0",
        "sleep 1 & wait",
        "setsid /bin/sh -c 'sleep 1' & exit 0",
        "yes output",
    ] {
        let start = Instant::now();
        let result = shell(script, Some(vec![b'x'; 1024 * 1024]));
        assert!(start.elapsed() < Duration::from_millis(850), "{script}");
        assert_eq!(result.unwrap_err().kind, ErrorKind::Timeout);
    }
}

#[test]
fn timeout_kills_owned_process_group_only() {
    let tmp = tempfile::tempdir().unwrap();
    let marker = tmp.path().join("survived");
    let mut unrelated = Command::new("sleep").arg("2").spawn().unwrap();
    let script = format!(
        "(sleep 0.35; printf survived > {}) & wait",
        util::shell_quote(marker.to_str().unwrap())
    );
    let result = shell(&script, None);
    std::thread::sleep(Duration::from_millis(350));
    let unrelated_alive = unrelated.try_wait().unwrap().is_none();
    let _ = unrelated.kill();
    unrelated.wait().unwrap();
    assert_eq!(result.unwrap_err().kind, ErrorKind::Timeout);
    assert!(!marker.exists(), "descendant survived the command deadline");
    assert!(unrelated_alive, "cleanup killed an unrelated process");
}

#[test]
fn command_captures_both_streams_while_delivering_large_stdin() {
    let data = vec![b'x'; 256 * 1024];
    let output = run_command(
        &[
            "/bin/sh".into(),
            "-c".into(),
            "tee /dev/stderr; exit 7".into(),
        ],
        &CommandOptions {
            timeout: Duration::from_secs(5),
            stdin: Some(data.clone()),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(output.status, 7);
    assert_eq!(output.stdout, data);
    assert_eq!(output.stderr, data);
    assert!(shell("exit 0", Some(vec![0; 1024 * 1024]))
        .unwrap()
        .success());
    assert_eq!(shell("kill -TERM $$", None).unwrap().status, -15);
}

fn git(root: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .current_dir(root)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

fn commit(root: &Path, content: &str) -> String {
    fs::write(root.join("tracked"), content).unwrap();
    git(root, &["add", "tracked"]);
    git(root, &["commit", "-qm", content]);
    git(root, &["rev-parse", "HEAD"])
}

fn repository(root: &Path, content: &str) -> String {
    fs::create_dir_all(root).unwrap();
    git(root, &["init", "-q"]);
    git(root, &["config", "user.name", "P1 regression"]);
    git(root, &["config", "user.email", "p1@example.invalid"]);
    git(
        root,
        &["remote", "add", "origin", "https://github.com/o/r.git"],
    );
    commit(root, content)
}

fn source(root: &Path, base: &str, head: &str) -> J {
    json!({"checkout": root, "repo": "o/r", "host": "github.com",
        "original_base": base, "head": head, "own_commits": [head]})
}

// Use a separate test process so inherited environment attacks cannot race
// parallel tests or require process-global environment mutation.
#[test]
fn git_environment_child() {
    let Some(fixture) = std::env::var_os("HERDR_P1_GIT_FIXTURE") else {
        return;
    };
    let fixture: J = serde_json::from_str(fixture.to_str().unwrap()).unwrap();
    let root = Path::new(fixture["root"].as_str().unwrap());
    let other = Path::new(fixture["other"].as_str().unwrap());
    assert!(pr::provenance(&source(
        root,
        fixture["other_base"].as_str().unwrap(),
        fixture["other_head"].as_str().unwrap()
    ))
    .is_err());
    assert!(pr::provenance(&fixture["valid"]).is_ok());
    assert_eq!(util::checkout_root(root), root);
    assert!(!util::inside_checkout(&other.join("tracked"), root));
    assert_eq!(
        util::fingerprint(root).unwrap(),
        fixture["fingerprint"].as_str().unwrap()
    );
    assert_eq!(
        pr::git(root, &["rev-parse", "HEAD"]).unwrap(),
        fixture["valid"]["head"]
    );
    assert_eq!(
        batch::git::output(root, &["rev-parse".into(), "HEAD".into()]).unwrap(),
        fixture["valid"]["head"]
    );
}

#[test]
fn git_environment_cannot_redirect_provenance_or_fingerprint_including_worktrees() {
    let tmp = tempfile::tempdir().unwrap();
    let a = tmp.path().join("a");
    let b = tmp.path().join("b");
    let base = repository(&a, "A base");
    let head = commit(&a, "A head");
    let monitor_marker = tmp.path().join("fsmonitor-ran");
    let monitor = tmp.path().join("fsmonitor.sh");
    fs::write(
        &monitor,
        format!(
            "#!/bin/sh\ntouch {}\n",
            util::shell_quote(monitor_marker.to_str().unwrap())
        ),
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&monitor, fs::Permissions::from_mode(0o700)).unwrap();
    let other_base = repository(&b, "B base");
    let other_head = commit(&b, "B head");
    let worktree = tmp.path().join("worktree");
    git(
        &a,
        &[
            "worktree",
            "add",
            "--detach",
            worktree.to_str().unwrap(),
            &head,
        ],
    );
    git(&a, &["config", "core.fsmonitor", monitor.to_str().unwrap()]);
    for root in [&a, &worktree] {
        let fixture = json!({"root": root, "other": b, "other_base": other_base,
            "other_head": other_head, "valid": source(root, &base, &head),
            "fingerprint": util::fingerprint(root).unwrap()});
        check_inherited_git_environment(&fixture, &b);
    }
    assert!(
        !monitor_marker.exists(),
        "Git executed checkout fsmonitor code"
    );
}

fn check_inherited_git_environment(fixture: &J, other: &Path) {
    for config_attack in [false, true] {
        let mut child = Command::new(std::env::current_exe().unwrap());
        child
            .args(["--exact", "git_environment_child", "--nocapture"])
            .env("HERDR_P1_GIT_FIXTURE", fixture.to_string())
            .env("GIT_DIR", other.join(".git"));
        if config_attack {
            use std::os::unix::ffi::OsStringExt;
            child
                .env("GIT_WORK_TREE", other)
                .env("GIT_COMMON_DIR", other.join(".git"))
                .env("GIT_INDEX_FILE", other.join(".git/index"))
                .env("GIT_OBJECT_DIRECTORY", other.join(".git/objects"))
                .env("GIT_CONFIG_COUNT", "1")
                .env("GIT_CONFIG_KEY_0", "remote.origin.url")
                .env("GIT_CONFIG_VALUE_0", "https://github.com/evil/other.git")
                .env(
                    std::ffi::OsString::from_vec(b"GIT_\xff".to_vec()),
                    "invalid",
                )
                .env("HERDR_P1_NON_UTF8", std::ffi::OsString::from_vec(vec![255]));
        }
        let output = child.output().unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

struct BranchApi(String);
impl GitHubApi for BranchApi {
    fn get(&self, suffix: &str) -> herdr_flow::Result<J> {
        assert_eq!(suffix, "branches/main");
        Ok(json!({"commit": {"sha": self.0}}))
    }
}

fn conflict_state(tmp: &Path) -> (J, String) {
    let source = tmp.join("source");
    let base = repository(&source, "base\n");
    let left = commit(&source, "left\n");
    git(&source, &["checkout", "--detach", &base]);
    let right = commit(&source, "right\n");
    let workspace = tmp.join("batch");
    seed_bare(&workspace, &source, &base, &[&left, &right]);
    let active = json!({"id": "batch-1", "base": base, "phase": "integration_prepared",
        "revision": 0, "workspace": workspace, "corrections": [], "versions": [],
        "members": [
            {"task": "left", "epoch": 1, "checkout": source, "original_base": base, "head": left, "own_commits": [left], "dependencies": []},
            {"task": "right", "epoch": 1, "checkout": source, "original_base": base, "head": right, "own_commits": [right], "dependencies": []}],
        "integration": {"offset": 0, "mapping": [], "current": null, "conflict": null}});
    let lane = json!({"task": "lane", "epoch": 1, "assignment": "a1", "candidate": base,
        "control": "running", "phase": "blocked", "state_version": 1, "writer": null,
        "pending": [], "history": [], "acceptance": {}, "owner": "coordinator",
        "lane": {"repo": "o/r", "host": "github.com", "base_ref": "main",
            "workspace_root": workspace, "repairs": {}, "max_repairs": 2, "active": active}});
    let member = |name: &str, head: &str| {
        json!({"task": name, "candidate": head, "epoch": 1,
        "control": "running", "phase": "blocked", "pending": [], "writer": null, "findings": {}})
    };
    (
        json!({"stopped": false, "tasks": {"lane": lane,
        "left": member("left", &left), "right": member("right", &right)}}),
        base,
    )
}

fn seed_bare(workspace: &Path, source: &Path, base: &str, heads: &[&str]) -> PathBuf {
    fs::create_dir_all(workspace).unwrap();
    util::atomic_write_json(
        &workspace.join("owner.json"),
        &json!({"batch": "batch-1", "repo": "o/r", "base": base}),
    )
    .unwrap();
    let bare = workspace.join("objects.git");
    git(workspace, &["init", "--bare", "-q", bare.to_str().unwrap()]);
    git(
        &bare,
        &["remote", "add", "origin", "https://github.com/o/r.git"],
    );
    for head in heads {
        git(
            &bare,
            &["fetch", "--no-tags", source.to_str().unwrap(), head],
        );
    }
    bare
}

#[test]
fn conflict_token_survives_checkpoints_reload_and_authorizes_one_repair() {
    let tmp = tempfile::tempdir().unwrap();
    let (mut state, base) = conflict_state(tmp.path());
    let path = tmp.path().join("queue.json");
    let mut saw_conflict = false;
    let mut persist = |state: &mut J| {
        let active = &state["tasks"]["lane"]["lane"]["active"];
        if active["phase"] == "integration_conflict" {
            saw_conflict = true;
            assert_eq!(active["failure_token"], batch::lane::repair_token(active));
        }
        util::atomic_write_json(&path, state)
    };
    let returned = batch::lane::integrate(
        &mut state,
        &json!({"task": "lane", "state_version": 1, "batch": "batch-1"}),
        100.0,
        &mut persist,
        Some(&BranchApi(base)),
    )
    .unwrap();
    assert!(saw_conflict);
    let saved: J = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
    assert_eq!(returned, saved["tasks"]["lane"]);
    assert_eq!(returned, state["tasks"]["lane"]);
    let mut reloaded = saved["tasks"]["lane"].clone();
    let active = &reloaded["lane"]["active"];
    assert_eq!(active["phase"], "integration_conflict");
    let token = active["failure_token"].as_str().unwrap().to_owned();
    let binding = json!({"batch": active["id"], "token": token,
        "candidate": reloaded["candidate"], "base": active["base"]});
    let action = json!({"operation": "repair-op", "writes": true,
        "workspace": tmp.path().join("isolated-repair"), "batch_repair": binding,
        "repair_diagnosis": {"kind": "code", "evidence": "inspected conflicting line"}});
    let event = json!({"outcome": "failed"});
    let mut stale = action.clone();
    stale["batch_repair"]["token"] = json!("stale");
    assert!(batch::lane::repair_intent(&mut reloaded, &stale, &event, 101.0).is_err());
    batch::lane::repair_intent(&mut reloaded, &action, &event, 101.0).unwrap();
    assert_eq!(reloaded["lane"]["repairs"][&token]["binding"], binding);
    assert!(batch::lane::repair_intent(&mut reloaded, &action, &event, 102.0).is_err());
}

#[test]
fn conflict_token_is_retained_when_checkpoint_reports_failure() {
    let tmp = tempfile::tempdir().unwrap();
    let (mut state, base) = conflict_state(tmp.path());
    let path = tmp.path().join("queue.json");
    let result = batch::lane::integrate(
        &mut state,
        &json!({"task": "lane", "state_version": 1, "batch": "batch-1"}),
        100.0,
        &mut |state| {
            util::atomic_write_json(&path, state)?;
            if state["tasks"]["lane"]["lane"]["active"]["phase"] == "integration_conflict" {
                return Err(herdr_flow::FlowError::io(
                    "checkpoint acknowledgement failed",
                ));
            }
            Ok(())
        },
        Some(&BranchApi(base)),
    );
    assert_eq!(result.unwrap_err().kind, ErrorKind::Io);
    let saved: J = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
    assert_eq!(state["tasks"]["lane"], saved["tasks"]["lane"]);
    let active = &saved["tasks"]["lane"]["lane"]["active"];
    assert_eq!(active["phase"], "integration_conflict");
    assert_eq!(active["failure_token"], batch::lane::repair_token(active));
}
