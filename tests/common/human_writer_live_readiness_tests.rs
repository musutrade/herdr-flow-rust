//! First-dispatch closure instrumentation; no model RPC, ACK or business result.
use super::*;
use std::cell::Cell;

fn fixture(process: J) -> (PreDispatch, J) {
    let owner = json!({"queue_id":"private","task_id":"task","epoch":1,"instance_id":"instance","queue_root":"/private/queue"});
    let receiver = json!({"bound_session":SESSION});
    let configuration = json!({"approvalPolicy":"on-request","approvalsReviewer":"auto_review", "sandbox":{"type":"workspaceWrite","writableRoots":[],"networkAccess":true,"excludeTmpdirEnvVar":false,"excludeSlashTmp":false}});
    let expected = json!({"owner":owner,"receiver":receiver,"queue_process":process,"receiver_process":process,"session_configuration":configuration});
    let health = json!({"healthy":true,"status":"running","pid":process["pid"],"start_ticks":process["start_time"].as_u64().unwrap().to_string()});
    let observed = json!({"owner":owner,"notify":true,"stopped":false,"queue_child_running":true,"queue_process":process,
        "queue_health":health,"receiver_health":health,"guard_dir":"/private/guard","guard":"/private/guard",
        "receiver":receiver,"guard_scope":receiver,"registration":{"phase":"bound","guard_dir":"/private/guard","state":{"session":SESSION}},
        "guard_state":{"receiver_activation":"active","phase":"monitoring","instance":"instance","session":SESSION},
        "upstream_phase":"created","upstream_session":SESSION,"original_start_absent":true,"session_configuration":configuration});
    (
        PreDispatch {
            expected,
            coordinator: "/private/coordinator".into(),
            queue: "/private/queue".into(),
            guard: "/private/guard".into(),
        },
        observed,
    )
}

#[test]
fn first_dispatch_requires_both_live_watchers_exact_identity_binding_and_configuration() {
    let mut child = FixtureChild(Command::new("sleep").arg("60").spawn().unwrap());
    let (gate, ready) = fixture(identity(child.0.id()));
    let sends = Cell::new(0);
    for (path, value) in [
        (&["queue_health", "healthy"][..], json!(false)),
        (&["receiver_health", "healthy"][..], json!(false)),
        (&["queue_health", "pid"][..], json!(child.0.id() + 1)),
        (
            &["receiver_health", "start_ticks"][..],
            json!("wrong-start"),
        ),
        (&["stopped"][..], json!(true)),
        (&["notify"][..], json!(false)),
        (
            &["guard_scope", "bound_session"][..],
            json!("another-session"),
        ),
        (
            &["session_configuration", "approvalsReviewer"][..],
            json!("user"),
        ),
        (
            &["session_configuration", "sandbox", "networkAccess"][..],
            json!(false),
        ),
    ] {
        let mut rejected = ready.clone();
        let mut field = &mut rejected;
        for key in path {
            field = &mut field[*key];
        }
        *field = value;
        assert!(gate
            .dispatch(rejected, |_| sends.set(sends.get() + 1))
            .is_err());
        assert_eq!(
            sends.get(),
            0,
            "unready/mismatched watcher or permission allowed a first send"
        );
    }
    gate.dispatch(ready.clone(), |_| sends.set(sends.get() + 1))
        .unwrap();
    assert_eq!(
        sends.get(),
        1,
        "verified readiness did not admit first dispatch"
    );
    child.0.kill().unwrap();
    child.0.wait().unwrap();
    // Even a stale positive health snapshot cannot authorize a dead exact PID.
    let exited_sends = Cell::new(0);
    assert!(gate
        .dispatch(ready, |_| exited_sends.set(exited_sends.get() + 1))
        .is_err());
    assert_eq!(
        exited_sends.get(),
        0,
        "exited watcher admitted a first send"
    );
}

#[test]
fn ordinary_queue_watcher_initializes_after_environment_lock_release_without_a_turn() {
    let f = Fixture::new();
    let env = setup_human(&f, &f.temp.path().join("output.git"));
    let coordinator = task_runtime::prepare(&env, "coordinator").unwrap();
    drop(env);
    let mut watcher = watch(&f);
    let expected = identity(watcher.0.id());
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let health = herdr_flow::watch_service::health(&queue::artifacts(&f.queue).unwrap());
        assert!(
            watcher.0.try_wait().unwrap().is_none(),
            "ordinary queue watcher exited before readiness"
        );
        if health["healthy"] == true {
            assert_eq!(health["pid"], expected["pid"]);
            assert_eq!(
                ticks(&health["start_ticks"]),
                expected["start_time"].as_u64()
            );
            break;
        }
        assert!(
            Instant::now() < deadline,
            "ordinary queue watcher readiness observation bound"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(absent(&coordinator.join("receiver-start.json")));
    assert!(absent(&coordinator.join("receiver-turn.json")));
    watcher.0.kill().unwrap();
    watcher.0.wait().unwrap();
    upstream_runtime::finalize_with_retry(&f);
    assert_eq!(queue::finalization::drive(&f.queue).unwrap().pending, 0);
}
