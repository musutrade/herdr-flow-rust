//! Synthetic native protocol over real isolated queue/instance ownership.
use super::*;
use std::net::TcpListener;

#[path = "runtime_http.rs"]
mod http;

#[test]
#[ignore = "subprocess-only HTTP fixture; no model"]
fn http_child() {
    let root = PathBuf::from(std::env::var_os("HERDR_ISOLATED_HTTP_FIXTURE").unwrap());
    let config = util::read_json(&root.join("fixture.json")).unwrap();
    let listener =
        TcpListener::bind(("127.0.0.1", config["port"].as_u64().unwrap() as u16)).unwrap();
    fs::write(root.join("ready"), b"ready").unwrap();
    http::serve(listener, Path::new(config["cwd"].as_str().unwrap()));
}

pub(super) fn linked(f: &Fixture) -> task_env::TaskEnvironment {
    let mut q = f.state();
    q["tasks"].as_object_mut().unwrap().remove("task");
    progress::lineage::action(&mut q, &json!({"kind":"plan","task":"task","goal":"fixture",
        "basis":{"original":["fixture"],"changes":[],"specifications":[],"endpoint":"local","confirmation":"fixture"},
        "authorization":[{"id":"develop","action":"development","source":"fixture"},
            {"id":"verify","action":"review","source":"fixture"}]}), util::now()).unwrap();
    for mut spec in [
        json!({"kind":"artifact","artifact":{"id":f.commit,"adapter":"git","location":f.temp.path().join("repo")}}),
        json!({"kind":"candidate","candidate":f.commit}),
    ] {
        spec["task"] = json!("task");
        spec["reason"] = json!("explicit fixture candidate");
        spec["state_version"] = q["tasks"]["task"]["state_version"].clone();
        progress::lineage::action(&mut q, &spec, util::now()).unwrap();
    }
    queue::save(&f.queue, &mut q).unwrap();
    let env = f.ready();
    let mut q = f.state();
    let spec = json!({"kind":"activity","task":"task","state_version":q["tasks"]["task"]["state_version"],
        "activity":"dev","role":"development","input":f.commit,"session_epoch":1,
        "result_wait":{"id":"dev","kind":"signal","task":"dev","epoch":1,"assignment":"dev-a1",
            "owner":"developer","reason":"fixture","condition":"report","timeout_seconds":600}});
    progress::lineage::action(&mut q, &spec, util::now()).unwrap();
    queue::save(&f.queue, &mut q).unwrap();
    env
}

fn runtime(env: &mut task_env::TaskEnvironment) -> (PathBuf, FixtureChild) {
    let cwd = env.workspace_root.join("checkout");
    runtime_at(env, "dev-a1", &cwd, None)
}

pub(super) fn runtime_at(
    env: &mut task_env::TaskEnvironment,
    assignment: &str,
    cwd: &Path,
    review: Option<&str>,
) -> (PathBuf, FixtureChild) {
    let root = task_runtime::prepare(env, assignment).unwrap();
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    util::atomic_write_json(&root.join("fixture.json"), &json!({"cwd":cwd,"port":port})).unwrap();
    let binary = root.join("fixture-server");
    fs::write(&binary, format!("#!/bin/sh\nexport HERDR_ISOLATED_HTTP_FIXTURE='{}'\nexec '{}' --exact isolated_runtime::http_child --ignored --nocapture\n",
        root.display(), std::env::current_exe().unwrap().display())).unwrap();
    fs::set_permissions(&binary, fs::Permissions::from_mode(0o700)).unwrap();
    let mut spec = json!({"adapter":"opencode","binary":binary,"cwd":cwd,"port":port,
        "model":{"providerID":"openai","id":"fixture"}});
    if let Some(key) = review {
        spec["review_snapshot"] = json!(key);
    }
    let mut child = FixtureChild(task_runtime::launch(env, &root, &spec).unwrap());
    let until = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !root.join("ready").exists() {
        assert!(child.0.try_wait().unwrap().is_none());
        assert!(
            std::time::Instant::now() < until,
            "fixture startup timed out"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    (root, child)
}

fn create_spec(f: &Fixture) -> J {
    json!({"queue_dir":f.queue,"wait":"dev","instruction":"fixture only","evidence":[]})
}

#[test]
fn bound_control_queue_outside_roots_supports_create_and_recover() {
    let f = Fixture::new();
    let mut env = linked(&f);
    assert!(task_env::validate_path(&env, &f.queue).is_err());
    let (root, _child) = runtime(&mut env);
    let state = task_runtime::native::create(&env, &root, &create_spec(&f)).unwrap();
    let before = f.state();
    let recovered = task_runtime::native::recover(&env, &root).unwrap();
    assert_eq!(recovered["session"], state["session"]);
    assert_eq!(
        f.state()["waits"]["dev"]["producer"],
        before["waits"]["dev"]["producer"]
    );
    assert!(task_env::validate_path(&env, &f.queue).is_err());
}

#[test]
fn control_queue_rejects_foreign_paths_before_creating_authority() {
    let f = Fixture::new();
    let mut env = linked(&f);
    let (root, _child) = runtime(&mut env);
    for path in [
        env.temp_root.join("foreign"),
        f.queue.join("child"),
        f.temp.path().join("foreign"),
    ] {
        let mut spec = create_spec(&f);
        spec["queue_dir"] = json!(path);
        let before = f.state();
        assert!(task_runtime::native::create(&env, &root, &spec).is_err());
        assert!(!path.exists(), "foreign queue access created a directory");
        assert!(!root.join("creation.json").exists());
        assert_eq!(f.state(), before);
    }
}

#[test]
fn control_queue_recovery_rejects_foreign_journal_and_alias() {
    let f = Fixture::new();
    let mut env = linked(&f);
    let (root, _child) = runtime(&mut env);
    let alias = f.temp.path().join("queue-alias");
    std::os::unix::fs::symlink(&f.queue, &alias).unwrap();
    let mut spec = create_spec(&f);
    spec["queue_dir"] = json!(alias);
    assert!(task_runtime::native::create(&env, &root, &spec).is_err());
    assert!(!root.join("creation.json").exists());
    task_runtime::native::create(&env, &root, &create_spec(&f)).unwrap();
    let before = f.state();
    let original = util::read_json(&root.join("creation.json")).unwrap();
    let state_before = util::read_json(&root.join("state.json")).unwrap();
    let foreign = env.temp_root.join("foreign");
    for path in [&foreign, &alias] {
        let mut journal = original.clone();
        journal["queue_dir"] = json!(path);
        util::atomic_write_json(&root.join("creation.json"), &journal).unwrap();
        assert!(task_runtime::native::recover(&env, &root).is_err());
        assert_eq!(
            util::read_json(&root.join("creation.json")).unwrap(),
            journal
        );
        assert_eq!(
            util::read_json(&root.join("state.json")).unwrap(),
            state_before
        );
        assert_eq!(f.state(), before);
        assert!(!foreign.exists());
    }
    util::atomic_write_json(&root.join("creation.json"), &original).unwrap();
    task_runtime::native::recover(&env, &root).unwrap();
}

#[test]
fn control_queue_never_grants_old_or_closing_owners_access() {
    let f = Fixture::new();
    let mut env = linked(&f);
    let (root, _child) = runtime(&mut env);
    let old = env.creation.as_ref().unwrap().binding.clone();
    env.creation.as_mut().unwrap().binding.epoch += 1;
    assert!(task_runtime::native::create(&env, &root, &create_spec(&f)).is_err());
    env.creation.as_mut().unwrap().binding = old;
    let saved_instance = env.instance_id.clone();
    env.instance_id = "foreign-instance".into();
    assert!(task_runtime::native::create(&env, &root, &create_spec(&f)).is_err());
    env.instance_id = saved_instance;
    task_runtime::native::create(&env, &root, &create_spec(&f)).unwrap();
    drop(env);
    f.finalize("failure").unwrap();
    let env = task_env::restore_environment("task").unwrap();
    assert!(task_runtime::native::create(&env, &root, &create_spec(&f)).is_err());
    assert!(task_runtime::native::recover(&env, &root).is_err());
}

#[test]
fn replaced_bound_queue_is_never_adopted_or_recreated() {
    let f = Fixture::new();
    let mut env = linked(&f);
    let (root, _child) = runtime(&mut env);
    task_runtime::native::create(&env, &root, &create_spec(&f)).unwrap();
    let original = f.state();
    let moved = f.temp.path().join("original-queue");
    fs::rename(&f.queue, &moved).unwrap();
    fs::create_dir(&f.queue).unwrap();
    fs::write(f.queue.join("sentinel"), "foreign").unwrap();
    assert!(task_runtime::native::create(&env, &root, &create_spec(&f)).is_err());
    assert!(task_runtime::native::recover(&env, &root).is_err());
    assert_eq!(
        fs::read_to_string(f.queue.join("sentinel")).unwrap(),
        "foreign"
    );
    assert!(!f.queue.join("queue.lock").exists());
    assert!(!f.queue.join("queue.json").exists());
    fs::remove_file(f.queue.join("sentinel")).unwrap();
    fs::remove_dir(&f.queue).unwrap();
    fs::rename(&moved, &f.queue).unwrap();
    assert_eq!(f.state(), original);
    task_runtime::native::recover(&env, &root).unwrap();
}
