use super::*;
use crate::progress::artifact::{ArtifactAdapter, LocalArtifacts};
use std::sync::mpsc;
use std::time::Duration;

fn lineage(queue_root: &Path, spec: &J) -> J {
    // The public transaction is the same registered-coordinator/permit/real
    // adapter/save path used by CLI and the native queue control service.
    let _queue = queue::locked(queue_root, false).unwrap();
    queue::task_action(queue_root, "task-lineage", spec, Some(SESSION), &|_| {
        panic!("artifact registration never calls a model")
    })
    .unwrap()
}

fn register_reference(queue_root: &Path, task: &str, source: &Path, commit: &str) -> J {
    lineage(
        queue_root,
        &json!({"kind":"plan","task":task,"goal":"read an existing Git artifact",
        "basis":{"original":["fixture"],"changes":[],"specifications":[],
            "endpoint":"local fixture","confirmation":"fixture"},
        "authorization":[{"id":"inspect","action":"review","source":"fixture"}]}),
    );
    let state = queue::load_queue(queue_root).unwrap();
    let artifact = json!({"adapter":"git","id":commit,"location":source});
    let updated = lineage(
        queue_root,
        &json!({"kind":"artifact","task":task,
        "state_version":state["tasks"][task]["state_version"],"artifact":artifact}),
    );
    assert_eq!(
        updated["tasks"][task]["artifacts"][commit]["object"],
        artifact
    );
    artifact
}

fn consumer_queue(fixture: &Fixture, cross_queue: bool) -> PathBuf {
    if !cross_queue {
        return fixture.owner.queue_root.clone();
    }
    let root = fixture.temp.path().join("consumer-queue");
    let mut state = queue::init_isolated(&root, None, 30).unwrap();
    let original = fixture.state();
    state["guard_dir"] = original["guard_dir"].clone();
    state["receiver"] = original["receiver"].clone();
    state["tasks"] = json!({});
    let _queue = queue::locked(&root, false).unwrap();
    queue::save(&root, &mut state).unwrap();
    root
}

fn real_clone_commit(fixture: &Fixture) -> String {
    let root = fixture.clone_path();
    git(&root, &["init", "-q"]);
    git(&root, &["add", "evidence"]);
    git(
        &root,
        &[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.test",
            "commit",
            "-qm",
            "retained review evidence",
        ],
    );
    git(&root, &["rev-parse", "HEAD"])
}

fn reference_survives_retirement(cross_queue: bool) {
    let calls = Arc::new(Mutex::new(vec![json!({})]));
    let service = server(5, false, calls.clone());
    let mut fixture = Fixture::new(&service.path);
    calls.lock().unwrap()[0]["fixture_cwd"] = json!(fixture.clone_path());
    let commit = real_clone_commit(&fixture);
    let target = consumer_queue(&fixture, cross_queue);
    let before = register_reference(&target, "consumer-before", &fixture.clone_path(), &commit);
    for _ in 0..3 {
        fixture.pending_tick();
    }
    let original_review = fixture.active().review_workspaces[ASSIGNMENT].clone();
    let (ready, locked) = mpsc::channel();
    let (proceed, resume) = mpsc::channel();
    let owner = fixture.owner.clone();
    let worker = std::thread::spawn(move || {
        let permit = Arc::new(lifecycle::acquire(&owner, true).unwrap());
        let mut env = task_env::isolation::restore_active(&owner, permit.clone()).unwrap();
        let mut plan = load(&env).unwrap();
        assert_eq!(plan.entries[0].phase, "sessions");
        assert_eq!(plan.entries[0].sessions[0]["phase"], "deleted");
        ready.send(()).unwrap();
        resume.recv_timeout(Duration::from_secs(5)).unwrap();
        let result = execution::drive(&mut env, &mut plan, &permit);
        publish_summary(&plan, &permit, result.as_ref().err()).unwrap();
        result
    });
    locked.recv_timeout(Duration::from_secs(5)).unwrap();
    // The source A EX and environment handle stay held by the paused worker.
    // Registration is a real target-queue transaction, never a raw JSON edit.
    let during = register_reference(&target, "consumer-during", &fixture.clone_path(), &commit);
    assert_eq!(
        lifecycle::acquire(&fixture.owner, false).unwrap_err().kind,
        crate::ErrorKind::StateBusy
    );
    proceed.send(()).unwrap();
    worker.join().unwrap().unwrap();
    service.finish();
    assert_eq!(fixture.plan().phase, "complete");
    assert!(
        fixture.clone_path().is_dir(),
        "stage completion removed a registered cross-task Git source"
    );
    assert_eq!(
        fixture.active().review_workspaces[ASSIGNMENT],
        original_review
    );
    assert_eq!(
        fs::read_to_string(fixture.clone_path().join("evidence")).unwrap(),
        "old candidate"
    );
    assert!(fixture.broker.try_wait().unwrap().is_some());
    assert!(fixture.unrelated.try_wait().unwrap().is_none());
    // Completing stage cleanup also leaves subsequent legal registrations
    // available; no local tombstone silently bans another task's Git source.
    let after = register_reference(&target, "consumer-after", &fixture.clone_path(), &commit);
    for (task, artifact) in [
        ("consumer-before", before),
        ("consumer-during", during),
        ("consumer-after", after),
    ] {
        let state = queue::load_queue(&target).unwrap();
        assert_eq!(
            state["tasks"][task]["artifacts"][&commit]["object"],
            artifact
        );
        LocalArtifacts.verify(&artifact).unwrap();
    }
}

#[test]
fn same_queue_artifact_registrations_survive_stage_completion() {
    reference_survives_retirement(false);
}

#[test]
fn cross_queue_artifact_registrations_survive_stage_completion() {
    reference_survives_retirement(true);
}
