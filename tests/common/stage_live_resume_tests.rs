use super::*;
use std::os::unix::net::UnixListener;

struct Prepared {
    f: Fixture,
    env: Option<task_env::TaskEnvironment>,
    expected: J,
    current: J,
    _socket: UnixListener,
}

impl Prepared {
    fn new() -> Self {
        let f = Fixture::new();
        let env = setup(&f, &f.temp.path().join("output.git"));
        task_runtime::prepare(&env, "coordinator").unwrap();
        let path = f.temp.path().join("service.sock");
        let listener = UnixListener::bind(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        let owner =
            serde_json::to_value(lifecycle::task_owner(&f.state(), "task").unwrap().unwrap())
                .unwrap();
        let parent = json!({"agent":{"pane_id":"w1:p1","terminal_id":"term1","workspace_id":"w1","tab_id":"w1:t1","cwd":"/fixture","agent_session":{"value":SESSION}}});
        let current = json!({"model":{"id":"fixture-coordinator"},"role_models":{"developer":{"id":"fixture-developer"},"reviewer":{"id":"fixture-reviewer"}},"parent":parent,"binary":"/fixture/codex","config_sha256":"fixed-config","socket_identity":socket(&path)});
        let saved = json!({"owner":owner,"agreement":f.state()["tasks"]["task"]["requirements"],"model":current["model"],"role_models":current["role_models"],"parent":parent,"binary":current["binary"],"config_sha256":current["config_sha256"],"socket":path});
        util::atomic_write_json(&f.temp.path().join("stage-baseline.json"), &saved).unwrap();
        Self {
            expected: json!({"root":f.temp.path(),"owner":owner,"socket_identity":current["socket_identity"]}),
            f,
            env: Some(env),
            current,
            _socket: listener,
        }
    }

    fn validate(&self) -> J {
        super::validate(&self.f, &self.expected, &self.current)
    }

    fn rejects(&self) {
        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.validate())).is_err()
        );
    }
}

impl Drop for Prepared {
    fn drop(&mut self) {
        drop(self.env.take());
        upstream_runtime::finalize_with_retry(&self.f);
        assert_eq!(
            queue::finalization::drive(&self.f.queue).unwrap().pending,
            0
        );
    }
}

#[test]
fn untouched_protected_pre_send_fixture_passes_without_rewriting_baseline() {
    let fixture = Prepared::new();
    let path = fixture.f.temp.path().join("stage-baseline.json");
    let before = fs::read(&path).unwrap();
    let result = fixture.validate();
    assert_eq!(result["owner"], fixture.expected["owner"]);
    assert_eq!(fs::read(path).unwrap(), before);
    assert!(!fixture.f.temp.path().join("stage-resume.json").exists());
}

#[test]
fn any_creation_receiver_turn_or_presentation_evidence_blocks_resume() {
    let fixture = Prepared::new();
    let root = fixture.f.temp.path().join("scratch/rt/coordinator");
    for name in [
        "runtime.json",
        "upstream-session.json",
        "receiver.json",
        "creation.json",
        "turn.json",
        "presentation.json",
        "start.json",
    ] {
        let path = root.join(name);
        util::atomic_write_json(&path, &json!({"phase":"unknown"})).unwrap();
        fixture.rejects();
        fs::remove_dir_all(path).unwrap();
    }
    util::atomic_write_json(&fixture.f.temp.path().join("stage-resume.json"), &json!({})).unwrap();
    fixture.rejects();
}

#[test]
fn original_owner_model_config_and_parent_must_match() {
    let mut fixture = Prepared::new();
    let original = fixture.current.clone();
    for (field, changed) in [
        ("model", json!({"id":"other"})),
        ("role_models", json!({})),
        ("config_sha256", json!("other")),
        ("binary", json!("/other/codex")),
        ("parent", json!({})),
    ] {
        fixture.current[field] = changed;
        fixture.rejects();
        fixture.current = original.clone();
    }
    fixture.expected["owner"]["instance_id"] = json!("foreign");
    fixture.rejects();
}

#[test]
fn changed_agreement_output_or_registered_process_blocks_resume() {
    let fixture = Prepared::new();
    let original = fixture.f.state();
    let mut changed = original.clone();
    changed["tasks"]["task"]["requirements"]["1"]["goal"] = json!("changed agreement");
    queue::save(&fixture.f.queue, &mut changed).unwrap();
    fixture.rejects();
    let mut restored = fixture.f.state();
    restored["tasks"]["task"]["requirements"] = original["tasks"]["task"]["requirements"].clone();
    queue::save(&fixture.f.queue, &mut restored).unwrap();
    fs::write(
        fixture.f.temp.path().join("workspace/checkout/answer.txt"),
        "42\n",
    )
    .unwrap();
    fixture.rejects();
    fs::remove_file(fixture.f.temp.path().join("workspace/checkout/answer.txt")).unwrap();
    let target = fixture.f.temp.path().join("output.git");
    git(&target, &["update-ref", "-d", "refs/heads/accepted"]);
    fixture.rejects();
    git(
        &target,
        &["update-ref", "refs/heads/accepted", &fixture.f.commit],
    );
    let env_path = fixture.env.as_ref().unwrap().state_file();
    let mut env = util::read_state_json(env_path.parent().unwrap(), "task.json").unwrap();
    env["child_processes"] = json!([{"pid":123,"start_time":99}]);
    util::atomic_write_json(env_path, &env).unwrap();
    fixture.rejects();
    env["child_processes"] = json!([]);
    util::atomic_write_json(env_path, &env).unwrap();
}

#[test]
fn unregistered_owned_process_blocks_pre_send_resume() {
    let fixture = Prepared::new();
    let mut child = FixtureChild(
        Command::new("sleep")
            .arg("30")
            .env(
                task_env::MARKER_ENV,
                fixture.expected["owner"]["instance_id"].as_str().unwrap(),
            )
            .spawn()
            .unwrap(),
    );
    fixture.rejects();
    child.0.kill().unwrap();
    child.0.wait().unwrap();
}

#[test]
fn socket_preflight_resolves_alias_and_rejects_unsafe_target_before_setup() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("service.sock");
    let _listener = UnixListener::bind(&path).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    let alias = temp.path().join("published.sock");
    std::os::unix::fs::symlink(&path, &alias).unwrap();
    assert_eq!(socket(&alias), socket(&path));
    fs::set_permissions(&path, fs::Permissions::from_mode(0o666)).unwrap();
    assert!(std::panic::catch_unwind(|| socket(&alias)).is_err());
}

#[test]
fn borrowed_fixture_root_survives_drop_without_allocating_a_new_fixture() {
    let mut original = Fixture::new();
    original.temp.disable_cleanup(true);
    let path = original.temp.path().to_owned();
    let queue_id = original.state()["id"].clone();
    drop(original);
    let mut borrowed = attach(&path);
    borrowed.temp.disable_cleanup(false);
    assert_eq!(borrowed.state()["id"], queue_id);
    drop(borrowed);
    assert!(path.exists());
    fs::remove_dir_all(path).unwrap();
}

#[test]
fn fresh_fixture_temporary_root_keeps_automatic_cleanup() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().to_owned();
    let fresh = FixtureTemp::Fresh(temp);
    drop(fresh);
    assert!(!path.exists());
}
