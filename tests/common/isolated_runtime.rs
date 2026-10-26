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
    let listener = TcpListener::bind(("127.0.0.1", config["port"].as_u64().unwrap() as u16))
        .unwrap();
    fs::write(root.join("ready"), b"ready").unwrap();
    http::serve(listener, Path::new(config["cwd"].as_str().unwrap()));
}

fn linked(f: &Fixture) -> task_env::TaskEnvironment {
    let mut q = f.state();
    q["tasks"].as_object_mut().unwrap().remove("task");
    progress::lineage::action(&mut q, &json!({"kind":"plan","task":"task","goal":"fixture",
        "basis":{"original":["fixture"],"changes":[],"specifications":[],"endpoint":"local","confirmation":"fixture"},
        "authorization":[{"id":"develop","action":"development","source":"fixture"}]}), util::now()).unwrap();
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
    let root = task_runtime::prepare(env, "dev-a1").unwrap();
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let cwd = env.workspace_root.join("checkout");
    util::atomic_write_json(&root.join("fixture.json"), &json!({"cwd":cwd,"port":port})).unwrap();
    let binary = root.join("fixture-server");
    fs::write(&binary, format!("#!/bin/sh\nexport HERDR_ISOLATED_HTTP_FIXTURE='{}'\nexec '{}' --exact isolated_runtime::http_child --ignored --nocapture\n",
        root.display(), std::env::current_exe().unwrap().display())).unwrap();
    fs::set_permissions(&binary, fs::Permissions::from_mode(0o700)).unwrap();
    let mut child = FixtureChild(task_runtime::launch(env, &root,
        &json!({"adapter":"opencode","binary":binary,"cwd":cwd,"port":port,
            "model":{"providerID":"openai","id":"fixture"}})).unwrap());
    let until = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !root.join("ready").exists() {
        assert!(child.0.try_wait().unwrap().is_none());
        assert!(std::time::Instant::now() < until, "fixture startup timed out");
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
    assert_eq!(f.state()["waits"]["dev"]["producer"], before["waits"]["dev"]["producer"]);
    assert!(task_env::validate_path(&env, &f.queue).is_err());
}

#[test]
fn control_queue_rejects_foreign_paths_before_creating_authority() {
    let f = Fixture::new();
    let mut env = linked(&f);
    let (root, _child) = runtime(&mut env);
    for path in [env.temp_root.join("foreign"), f.queue.join("child"), f.temp.path().join("foreign")] {
        let mut spec = create_spec(&f);
        spec["queue_dir"] = json!(path);
        let before = f.state();
        assert!(task_runtime::native::create(&env, &root, &spec).is_err());
        assert!(!path.exists(), "foreign queue access created a directory");
        assert!(!root.join("creation.json").exists());
        assert_eq!(f.state(), before);
    }
}
