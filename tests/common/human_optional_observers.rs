use super::*;
use std::os::unix::fs::{symlink, PermissionsExt};

fn worker(f: &Fixture, env: &TaskEnvironment) -> PathBuf {
    super::super::super::super::prepare(env, "observer-boundary")
        .unwrap_or_else(|e| panic!("{}: {e}", f.root().display()))
}
fn check(env: &TaskEnvironment, root: &Path) -> Result<Vec<task_env::ProcessRecord>> {
    let mut allowed = Vec::new();
    observe::runtime_watches(env, root, &mut allowed)?;
    Ok(allowed)
}
fn observer(env: &TaskEnvironment, root: &Path) -> PathBuf {
    let path = root.join("recovery-observer");
    task_env::isolation::runtime_namespace(env, &path).unwrap();
    util::atomic_write_json(
        &path.join("observer.json"),
        &json!({"owner":task_env::isolation::owner(env),"coordinator":root,"observer":path}),
    )
    .unwrap();
    path
}

#[test]
fn optional_observer_absence_is_distinct_from_invalid_namespace_entries() {
    let (f, service) = Fixture::with_server(0, "idle");
    let env = f.active();
    let root = worker(&f, &env);
    assert!(check(&env, &root).unwrap().is_empty());
    let path = root.join("recovery-observer");
    fs::write(&path, b"file").unwrap();
    assert!(check(&env, &root).is_err());
    fs::remove_file(&path).unwrap();
    symlink(f.root().join("recovery-observer"), &path).unwrap();
    assert!(check(&env, &root).is_err());
    fs::remove_file(&path).unwrap();
    util::ensure_private_dir(&path, true).unwrap();
    assert!(
        check(&env, &root).is_err(),
        "an empty directory is not an observer namespace"
    );
    fs::remove_dir(&path).unwrap();
    let path = observer(&env, &root);
    assert!(check(&env, &root).unwrap().is_empty());
    for mode in [0o000, 0o777] {
        fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
        let rejected = check(&env, &root).is_err();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(rejected, "mode={mode:o}");
    }
    util::atomic_write_json(
        &path.join("observer.json"),
        &json!({"owner":null,"coordinator":root,"observer":path}),
    )
    .unwrap();
    assert!(check(&env, &root).is_err());
    fs::write(path.join("observer.json/authority.json"), b"invalid JSON").unwrap();
    assert!(check(&env, &root).is_err());
    service.finish();
}

#[test]
fn declared_observer_keeps_missing_directories_and_registered_identity_fail_closed() {
    let (f, service) = Fixture::with_server(0, "idle");
    let env = f.active();
    let root = worker(&f, &env);
    let guard = f.temp.path().join("worker-guard");
    util::ensure_private_dir(&guard, true).unwrap();
    util::atomic_write_json(&root.join("receiver.json"), &json!({"guard_dir":guard})).unwrap();
    assert!(
        check(&env, &root).is_err(),
        "declared observer directory must exist"
    );
    let path = observer(&env, &root);
    let pid = f.broker.id() as i32;
    let ticks = task_env::process_start_time(pid).unwrap();
    let registration = json!({"recovery":{"phase":"watching","state_dir":path,"pid":pid}});
    util::atomic_write_json(&guard.join("activation.json"), &registration).unwrap();
    assert!(
        check(&env, &root).is_err(),
        "registered observer must retain watch.json"
    );
    let watch = json!({"pid":pid,"start_ticks":ticks});
    util::atomic_write_json(&path.join("watch.json"), &watch).unwrap();
    assert_eq!(
        check(&env, &root).unwrap(),
        vec![task_env::ProcessRecord {
            pid,
            start_time: Some(ticks)
        }]
    );
    for invalid in [
        json!({"pid":pid,"start_ticks":ticks+1}),
        json!({"pid":pid+1,"start_ticks":ticks}),
        json!({"pid":pid}),
    ] {
        util::atomic_write_json(&path.join("watch.json"), &invalid).unwrap();
        assert!(check(&env, &root).is_err());
    }
    util::atomic_write_json(&path.join("watch.json"), &watch).unwrap();
    let saved = f.temp.path().join("saved-observer");
    fs::rename(&path, &saved).unwrap();
    assert!(
        check(&env, &root).is_err(),
        "deleting a declared namespace cannot erase its watch obligation"
    );
    fs::rename(&saved, &path).unwrap();
    let saved_guard = f.temp.path().join("saved-guard");
    fs::rename(&guard, &saved_guard).unwrap();
    assert!(
        check(&env, &root).is_err(),
        "registered receiver guard is mandatory"
    );
    fs::rename(&saved_guard, &guard).unwrap();
    assert!(
        check(&env, &root.join("missing-runtime")).is_err(),
        "missing runtime root must not be treated as quiet"
    );
    service.finish();
}
