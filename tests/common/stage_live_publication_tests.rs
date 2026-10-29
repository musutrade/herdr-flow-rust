use super::*;
use std::sync::mpsc;

struct Publication {
    _temp: tempfile::TempDir,
    coordinator: PathBuf,
    worker: PathBuf,
    owner: J,
}
impl Publication {
    fn new() -> Self {
        let temp = tempfile::Builder::new()
            .permissions(fs::Permissions::from_mode(0o700))
            .tempdir()
            .unwrap();
        let coordinator = temp.path().join("coordinator");
        let worker = temp.path().join("dev-a1");
        let owner = json!({"queue_id":"audit-queue","task_id":"task","epoch":1,"instance_id":"audit-instance","queue_root":temp.path().join("queue")});
        for root in [&coordinator, &worker] {
            fs::create_dir(root).unwrap();
            fs::set_permissions(root, fs::Permissions::from_mode(0o700)).unwrap();
            util::protected::namespace(root).unwrap();
            util::atomic_write_json(&root.join("lifecycle-owner.json"), &owner).unwrap();
        }
        util::atomic_write_json(
            &worker.join("runtime.json"),
            &json!({"lifecycle_owner":owner}),
        )
        .unwrap();
        util::atomic_write_json(
            &worker.join("state.json"),
            &json!({"role":"development","session":"original-session"}),
        )
        .unwrap();
        Self {
            _temp: temp,
            coordinator,
            worker,
            owner,
        }
    }
    fn gate(&self) -> util::FileLock {
        util::FileLock::acquire(&self.coordinator.join("control-service.lock"), false)
            .unwrap()
            .unwrap()
    }
    fn read(&self) -> Option<J> {
        publication_view(&self.coordinator, &self.owner, &self.worker)
            .map(|s| s["presentation"].clone())
    }
}
fn panic_text(result: std::thread::Result<Option<J>>) -> String {
    let error = result.expect_err("persistent invalid presentation must fail closed");
    if let Some(text) = error.downcast_ref::<String>() {
        text.clone()
    } else {
        error.downcast_ref::<&str>().unwrap().to_string()
    }
}

#[test]
fn protected_presentation_publication_gap_defers_until_producer_releases_service_gate() {
    let fixture = Publication::new();
    let root = fixture.worker.clone();
    let coordinator = fixture.coordinator.clone();
    let expected = json!({"phase":"ready","session":"original-session"});
    let written = expected.clone();
    let (reserved, observe) = mpsc::sync_channel(0);
    let (publish, proceed) = mpsc::sync_channel(0);
    let writer = std::thread::spawn(move || {
        let _service = util::FileLock::acquire(&coordinator.join("control-service.lock"), false)
            .unwrap()
            .unwrap();
        let _presentation = util::FileLock::acquire(&root.join("presentation.lock"), false)
            .unwrap()
            .unwrap();
        let document = root.join("presentation.json");
        fs::create_dir(&document).unwrap();
        fs::set_permissions(&document, fs::Permissions::from_mode(0o700)).unwrap();
        reserved.send(()).unwrap();
        proceed.recv_timeout(Duration::from_secs(10)).unwrap();
        util::atomic_write_json(&document, &written).unwrap();
    });
    observe.recv_timeout(Duration::from_secs(10)).unwrap();
    assert!(!fixture
        .worker
        .join("presentation.json/.flow-document-v2")
        .exists());
    let racing = std::panic::catch_unwind(|| fixture.read());
    publish.send(()).unwrap();
    writer.join().unwrap();
    assert_eq!(racing.unwrap(), None);
    assert_eq!(fixture.read(), Some(expected));
    assert!(matches!(
        util::probe_existing_lock(&fixture.coordinator.join("control-service.lock")).unwrap(),
        util::LockProbe::Acquired(_)
    ));
}

#[test]
fn observing_unpublished_worker_never_creates_service_lock_or_document() {
    let fixture = Publication::new();
    assert_eq!(fixture.read(), None);
    assert!(!fixture.coordinator.join("control-service.lock").exists());
    assert!(!fixture.worker.join("presentation.lock").exists());
    assert!(!fixture.worker.join("presentation.json").exists());
}

#[test]
fn quiescent_unprotected_marker_only_and_malformed_presentations_are_errors() {
    for kind in ["unprotected", "marker-only", "malformed"] {
        let fixture = Publication::new();
        let document = fixture.worker.join("presentation.json");
        let lock = fixture.gate();
        match kind {
            "unprotected" => {
                fs::create_dir(&document).unwrap();
                fs::set_permissions(&document, fs::Permissions::from_mode(0o700)).unwrap();
            }
            "marker-only" => util::protected::create(&document).unwrap(),
            "malformed" => {
                util::atomic_write_json(&document, &json!({"phase":"ready"})).unwrap();
                fs::write(document.join("authority.json"), b"{broken").unwrap();
            }
            _ => unreachable!(),
        }
        assert_eq!(fixture.read(), None);
        drop(lock);
        let error = panic_text(std::panic::catch_unwind(|| fixture.read()));
        assert!(
            error.contains(&document.display().to_string()) && error.contains("op=read"),
            "{kind}: {error}"
        );
        assert!(matches!(
            util::probe_existing_lock(&fixture.coordinator.join("control-service.lock")).unwrap(),
            util::LockProbe::Acquired(_)
        ));
    }
}

#[test]
fn published_worker_without_service_gate_is_not_treated_as_unpublished() {
    let fixture = Publication::new();
    let document = fixture.worker.join("presentation.json");
    util::atomic_write_json(&document, &json!({"phase":"ready"})).unwrap();
    let error = panic_text(std::panic::catch_unwind(|| fixture.read()));
    assert!(
        error.contains("control-service.lock") && error.contains(&document.display().to_string()),
        "{error}"
    );
    assert!(!fixture.coordinator.join("control-service.lock").exists());
}

#[test]
fn service_gate_does_not_hide_corruption_in_other_runtime_documents() {
    let fixture = Publication::new();
    drop(fixture.gate());
    util::atomic_write_json(
        &fixture.worker.join("presentation.json"),
        &json!({"phase":"ready"}),
    )
    .unwrap();
    fs::write(
        fixture.worker.join("runtime.json/authority.json"),
        b"{broken",
    )
    .unwrap();
    let error = panic_text(std::panic::catch_unwind(|| fixture.read()));
    assert!(
        error.contains("op=read") && error.contains("runtime.json"),
        "{error}"
    );
}

#[test]
fn snapshots_release_gate_and_never_take_presentation_lock_or_accept_foreign_scope() {
    let fixture = Publication::new();
    drop(fixture.gate());
    util::atomic_write_json(
        &fixture.worker.join("presentation.json"),
        &json!({"phase":"ready"}),
    )
    .unwrap();
    let _presentation = util::FileLock::acquire(&fixture.worker.join("presentation.lock"), false)
        .unwrap()
        .unwrap();
    assert!(
        fixture.read().is_some(),
        "audit must not contend on presentation.lock"
    );
    assert!(matches!(
        util::probe_existing_lock(&fixture.coordinator.join("control-service.lock")).unwrap(),
        util::LockProbe::Acquired(_)
    ));
    let mut wrong_owner = fixture.owner.clone();
    wrong_owner["epoch"] = json!(2);
    assert!(std::panic::catch_unwind(|| publication_view(
        &fixture.coordinator,
        &wrong_owner,
        &fixture.worker
    ))
    .is_err());
    assert!(publication_view(
        &fixture.coordinator,
        &fixture.owner,
        &fixture.worker.with_file_name("recovery-a1")
    )
    .is_none());
}

#[test]
fn observer_first_gate_defers_real_ordinary_run_before_executing_then_publishes_once() {
    coordinator_continuation::observer_first_run(observation_gate);
}
