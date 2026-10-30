use super::*;
use crate::light::recovery::*;

struct Sender {
    calls: usize,
    outcome: Outcome,
}
impl ConstrainedSender for Sender {
    fn send(&mut self, _: &Authorization, _: &Turn, message: &str) -> Outcome {
        assert_eq!(message, CONTINUE_MESSAGE);
        self.calls += 1;
        self.outcome.clone()
    }
}
fn auth(t: &TempDir) -> Authorization {
    Authorization {
        thread_id: "01a18ad5-e362-7381-8aea-e9fd13e93020".into(),
        project: t.path().join("project"),
        history: t.path().join("history.sqlite3"),
        not_before: 100,
    }
}
fn failed(id: &str, ordinal: i64, completed: i64) -> Turn {
    Turn {
        thread_id: "01a18ad5-e362-7381-8aea-e9fd13e93020".into(),
        turn_id: id.into(),
        ordinal,
        status: "failed".into(),
        error_code: Some("serverOverloaded".into()),
        completed_at: Some(completed),
        items_terminal: true,
    }
}
#[test]
fn three_sends_survive_restart_and_new_turns_and_success_ends_chain() {
    let t = TempDir::new().unwrap();
    let a = auth(&t);
    let options = Options::new(t.path().join("ledger"));
    let mut sender = Sender {
        calls: 0,
        outcome: Outcome::Unknown,
    };
    for (i, delay) in DELAYS_SECONDS.iter().enumerate() {
        let mut c = Controller::open(&options, a.clone()).unwrap();
        let turn = failed(&format!("t{i}"), i as i64 + 1, 100 + i as i64 * 200);
        let now = turn.completed_at.unwrap() + delay;
        assert!(matches!(
            c.step(&turn, now - 1, &mut sender).unwrap(),
            Decision::Waiting { .. }
        ));
        sender.outcome = Outcome::Accepted {
            turn_id: format!("t{}", i + 1),
        };
        assert_eq!(
            c.step(&turn, now, &mut sender).unwrap(),
            Decision::Attempt {
                number: i + 1,
                outcome: sender.outcome.clone()
            }
        );
        assert_eq!(
            c.step(&turn, now, &mut sender).unwrap(),
            Decision::Ineligible
        );
    }
    let mut c = Controller::open(&options, a).unwrap();
    assert_eq!(
        c.step(&failed("t3", 4, 700), 850, &mut sender).unwrap(),
        Decision::Exhausted
    );
    assert_eq!(sender.calls, 3);
    let mut success = failed("t3", 4, 700);
    success.status = "completed".into();
    assert_eq!(
        c.step(&success, 850, &mut sender).unwrap(),
        Decision::Completed
    );
    assert_eq!(
        c.step(&failed("t3", 4, 700), 850, &mut sender).unwrap(),
        Decision::Ineligible
    );
    assert!(matches!(
        c.step(&failed("new", 5, 900), 930, &mut sender).unwrap(),
        Decision::Attempt { number: 1, .. }
    ));
}
#[test]
fn unknown_blocks_after_restart_even_if_new_failure_or_success_arrives() {
    let t = TempDir::new().unwrap();
    let a = auth(&t);
    let options = Options::new(t.path().join("ledger"));
    let mut s = Sender {
        calls: 0,
        outcome: Outcome::Unknown,
    };
    let mut c = Controller::open(&options, a.clone()).unwrap();
    assert!(matches!(
        c.step(&failed("t1", 1, 100), 130, &mut s).unwrap(),
        Decision::Attempt { number: 1, .. }
    ));
    drop(c);
    let mut c = Controller::open(&options, a).unwrap();
    for status in ["failed", "completed", "inProgress"] {
        let mut turn = failed("t2", 2, 200);
        turn.status = status.into();
        assert_eq!(c.step(&turn, 300, &mut s).unwrap(), Decision::Unknown);
    }
    assert_eq!(s.calls, 1);
}
#[test]
fn quota_auth_policy_retry_approval_old_missing_and_unfinished_never_send() {
    let t = TempDir::new().unwrap();
    let mut c = Controller::open(&Options::new(t.path().join("ledger")), auth(&t)).unwrap();
    let mut s = Sender {
        calls: 0,
        outcome: Outcome::Unknown,
    };
    for error in [
        "usageLimitExceeded",
        "unauthorized",
        "policy",
        "429",
        "WARN retrying",
        "contextWindowExceeded",
        "other",
    ] {
        let mut turn = failed("t", 1, 100);
        turn.error_code = Some(error.into());
        assert_eq!(c.step(&turn, 130, &mut s).unwrap(), Decision::Ineligible);
    }
    for status in ["inProgress", "interrupted", "waitingApproval", "unknown"] {
        let mut turn = failed("t", 1, 100);
        turn.status = status.into();
        assert_eq!(c.step(&turn, 130, &mut s).unwrap(), Decision::Ineligible);
    }
    let mut turn = failed("t", 1, 100);
    turn.items_terminal = false;
    assert_eq!(c.step(&turn, 130, &mut s).unwrap(), Decision::Ineligible);
    turn.items_terminal = true;
    turn.completed_at = None;
    assert_eq!(c.step(&turn, 130, &mut s).unwrap(), Decision::Ineligible);
    for completed in [99, 150, 0] {
        turn.completed_at = Some(completed);
        assert_eq!(c.step(&turn, 130, &mut s).unwrap(), Decision::Ineligible);
    }
    assert_eq!(
        c.step(&failed("t", 1, 100), 701, &mut s).unwrap(),
        Decision::Ineligible
    );
    turn = failed("t", 1, 100);
    turn.thread_id = "another".into();
    assert_eq!(c.step(&turn, 130, &mut s).unwrap(), Decision::Ineligible);
    assert_eq!(s.calls, 0);
}
#[test]
fn lock_and_authorization_drift_do_not_reset_budget() {
    let t = TempDir::new().unwrap();
    let a = auth(&t);
    let options = Options::new(t.path().join("ledger"));
    let c = Controller::open(&options, a.clone()).unwrap();
    assert!(Controller::open(&options, a.clone()).is_err());
    drop(c);
    let mut other = a.clone();
    other.not_before += 1;
    assert!(Controller::open(&options, other).is_err());
    assert!(Controller::open(&options, a).is_ok());
}
struct Crash;
impl ConstrainedSender for Crash {
    fn send(&mut self, _: &Authorization, _: &Turn, _: &str) -> Outcome {
        panic!("simulated crash after durable reservation")
    }
}
#[test]
fn crash_after_reservation_is_unknown_without_resending() {
    let t = TempDir::new().unwrap();
    let a = auth(&t);
    let options = Options::new(t.path().join("ledger"));
    let mut c = Controller::open(&options, a.clone()).unwrap();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        c.step(&failed("t", 1, 100), 130, &mut Crash)
    }));
    assert!(result.is_err());
    drop(c);
    let mut c = Controller::open(&options, a).unwrap();
    let mut s = Sender {
        calls: 0,
        outcome: Outcome::Rejected,
    };
    assert_eq!(
        c.step(&failed("t", 1, 100), 130, &mut s).unwrap(),
        Decision::Unknown
    );
    assert_eq!(s.calls, 0);
}
#[test]
fn failed_new_unrelated_turn_cannot_reset_chain() {
    let t = TempDir::new().unwrap();
    let mut c = Controller::open(&Options::new(t.path().join("ledger")), auth(&t)).unwrap();
    let mut s = Sender {
        calls: 0,
        outcome: Outcome::Accepted {
            turn_id: "child".into(),
        },
    };
    c.step(&failed("t", 1, 100), 130, &mut s).unwrap();
    assert_eq!(
        c.step(&failed("unrelated", 2, 200), 330, &mut s).unwrap(),
        Decision::Ineligible
    );
    assert_eq!(s.calls, 1);
}
#[test]
fn history_is_thread_scoped_readonly_and_missing_does_not_create() {
    let t = TempDir::new().unwrap();
    let a = auth(&t);
    assert!(read_latest(&a).is_err());
    assert!(!a.history.exists());
    let db = rusqlite::Connection::open(&a.history).unwrap();
    db.execute_batch("CREATE TABLE thread_turns(thread_id,turn_id,rollout_ordinal,status,error_json,completed_at); CREATE TABLE thread_items(thread_id,turn_id,rollout_ordinal,item_json);").unwrap();
    db.execute("INSERT INTO thread_turns VALUES(?1,'old',1,'failed','{\"codexErrorInfo\":\"serverOverloaded\"}',100)",[&a.thread_id]).unwrap();
    db.execute(
        "INSERT INTO thread_turns VALUES(?1,'latest',2,'completed',NULL,200)",
        [&a.thread_id],
    )
    .unwrap();
    db.execute_batch(
        "INSERT INTO thread_turns VALUES('other','private',999,'failed','invalid',300);",
    )
    .unwrap();
    drop(db);
    let before = fs::read(&a.history).unwrap();
    let latest = read_latest(&a).unwrap().unwrap();
    assert_eq!(latest.turn_id, "latest");
    assert_eq!(latest.status, "completed");
    assert_eq!(before, fs::read(&a.history).unwrap());
}

#[test]
fn process_worker() {
    let Ok(root) = std::env::var("HERDR_LIGHT_RECOVERY_TEST_ROOT") else {
        return;
    };
    let root = std::path::PathBuf::from(root);
    let a: Authorization =
        serde_json::from_slice(&fs::read(root.join("authorization.json")).unwrap()).unwrap();
    let Ok(mut c) = Controller::open(&Options::new(root.join("ledger")), a) else {
        std::process::exit(72);
    };
    struct ExitSender {
        root: PathBuf,
        crash: bool,
    }
    impl ConstrainedSender for ExitSender {
        fn send(&mut self, _: &Authorization, _: &Turn, _: &str) -> Outcome {
            use std::io::Write;
            let mut file = fs::OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(self.root.join("actual-send"))
                .unwrap();
            file.write_all(b"one constrained send\n").unwrap();
            file.sync_all().unwrap();
            if self.crash {
                std::process::exit(73);
            }
            Outcome::Accepted {
                turn_id: "t3".into(),
            }
        }
    }
    let crash = std::env::var("HERDR_LIGHT_RECOVERY_TEST_CRASH").is_ok();
    let turn = if crash {
        failed("t0", 1, 100)
    } else {
        failed("t2", 3, 500)
    };
    let _ = c
        .step(
            &turn,
            if crash { 130 } else { 620 },
            &mut ExitSender { root, crash },
        )
        .unwrap();
}
fn worker(t: &TempDir, crash: bool) -> std::process::Child {
    let mut cmd = std::process::Command::new(std::env::current_exe().unwrap());
    cmd.args([
        "--exact",
        "light::tests::recovery::process_worker",
        "--nocapture",
    ])
    .env("HERDR_LIGHT_RECOVERY_TEST_ROOT", t.path());
    if crash {
        cmd.env("HERDR_LIGHT_RECOVERY_TEST_CRASH", "1");
    }
    cmd.spawn().unwrap()
}
#[test]
fn process_exit_after_reservation_and_before_receipt_never_repeats_send() {
    let t = TempDir::new().unwrap();
    let a = auth(&t);
    fs::write(
        t.path().join("authorization.json"),
        serde_json::to_vec(&a).unwrap(),
    )
    .unwrap();
    assert_eq!(worker(&t, true).wait().unwrap().code(), Some(73));
    let mut c = Controller::open(&Options::new(t.path().join("ledger")), a).unwrap();
    let mut s = Sender {
        calls: 0,
        outcome: Outcome::Rejected,
    };
    assert_eq!(
        c.step(&failed("t0", 1, 100), 160, &mut s).unwrap(),
        Decision::Unknown
    );
    assert_eq!(s.calls, 0);
    assert_eq!(
        fs::read(t.path().join("actual-send")).unwrap(),
        b"one constrained send\n"
    );
}
#[test]
fn concurrent_processes_compete_for_the_last_send_slot() {
    let t = TempDir::new().unwrap();
    let a = auth(&t);
    let options = Options::new(t.path().join("ledger"));
    fs::write(
        t.path().join("authorization.json"),
        serde_json::to_vec(&a).unwrap(),
    )
    .unwrap();
    let mut c = Controller::open(&options, a.clone()).unwrap();
    let mut s = Sender {
        calls: 0,
        outcome: Outcome::Accepted {
            turn_id: "t1".into(),
        },
    };
    c.step(&failed("t0", 1, 100), 130, &mut s).unwrap();
    s.outcome = Outcome::Accepted {
        turn_id: "t2".into(),
    };
    c.step(&failed("t1", 2, 300), 360, &mut s).unwrap();
    drop(c);
    let mut workers: Vec<_> = (0..4).map(|_| worker(&t, false)).collect();
    for w in &mut workers {
        assert!([Some(0), Some(72)].contains(&w.wait().unwrap().code()));
    }
    assert_eq!(
        fs::read(t.path().join("actual-send")).unwrap(),
        b"one constrained send\n"
    );
    let mut c = Controller::open(&options, a).unwrap();
    assert_eq!(
        c.step(&failed("t3", 4, 700), 820, &mut s).unwrap(),
        Decision::Exhausted
    );
    assert_eq!(s.calls, 2);
}
#[test]
fn atomic_sender_rejects_target_or_turn_change_without_injecting() {
    struct RacingSender {
        injected: bool,
    }
    impl ConstrainedSender for RacingSender {
        fn send(&mut self, _: &Authorization, failed: &Turn, _: &str) -> Outcome {
            let actual_latest = "new-user-turn";
            if failed.turn_id != actual_latest {
                return Outcome::Rejected;
            }
            self.injected = true;
            Outcome::Accepted {
                turn_id: "should-not-happen".into(),
            }
        }
    }
    let t = TempDir::new().unwrap();
    let mut c = Controller::open(&Options::new(t.path().join("ledger")), auth(&t)).unwrap();
    let mut sender = RacingSender { injected: false };
    assert_eq!(
        c.step(&failed("old-failed", 1, 100), 130, &mut sender)
            .unwrap(),
        Decision::Attempt {
            number: 1,
            outcome: Outcome::Rejected
        }
    );
    assert!(!sender.injected);
    assert_eq!(
        c.step(&failed("old-failed", 1, 100), 200, &mut sender)
            .unwrap(),
        Decision::Ineligible
    );
}

#[test]
fn deleting_database_while_retaining_lock_cannot_reset_budget() {
    let t = TempDir::new().unwrap();
    let options = Options::new(t.path().join("ledger"));
    drop(Controller::open(&options, auth(&t)).unwrap());
    fs::remove_file(options.data_root.join("recovery.sqlite3")).unwrap();
    assert!(Controller::open(&options, auth(&t)).is_err());
}

#[test]
fn missing_state_row_cannot_reinitialize_budget() {
    let t = TempDir::new().unwrap();
    let options = Options::new(t.path().join("ledger"));
    drop(Controller::open(&options, auth(&t)).unwrap());
    let db = rusqlite::Connection::open(options.data_root.join("recovery.sqlite3")).unwrap();
    db.execute("DELETE FROM recovery", []).unwrap();
    drop(db);
    assert!(Controller::open(&options, auth(&t)).is_err());
}

#[test]
fn history_structured_network_errors_and_pending_items_fail_closed() {
    let t = TempDir::new().unwrap();
    let a = auth(&t);
    let db = rusqlite::Connection::open(&a.history).unwrap();
    db.execute_batch("CREATE TABLE thread_turns(thread_id,turn_id,rollout_ordinal,status,error_json,completed_at); CREATE TABLE thread_items(thread_id,turn_id,rollout_ordinal,item_json);").unwrap();
    db.execute(
        "INSERT INTO thread_turns VALUES(?1,'last',1,'failed',NULL,100)",
        [&a.thread_id],
    )
    .unwrap();
    for (code, expected) in [
        (serde_json::Value::Null, true),
        (serde_json::json!(503), true),
        (serde_json::json!(401), false),
        (serde_json::json!(429), false),
    ] {
        let e = serde_json::json!({"codexErrorInfo":{"responseTooManyFailedAttempts":{"httpStatusCode":code}}});
        db.execute("UPDATE thread_turns SET error_json=?1", [e.to_string()])
            .unwrap();
        assert_eq!(
            read_latest(&a).unwrap().unwrap().error_code.is_some(),
            expected
        );
    }
    for item in [
        serde_json::json!({"type":"commandExecution","status":"inProgress"}),
        serde_json::json!({"type":"unknownFutureTool","status":"completed"}),
        serde_json::json!({"type":"mcpToolCall","status":"waitingApproval"}),
    ] {
        db.execute("DELETE FROM thread_items", []).unwrap();
        db.execute(
            "INSERT INTO thread_items VALUES(?1,'last',1,?2)",
            rusqlite::params![a.thread_id, item.to_string()],
        )
        .unwrap();
        assert!(!read_latest(&a).unwrap().unwrap().items_terminal);
    }
}

#[test]
fn malformed_authorization_history_and_ledger_are_errors_not_resets() {
    let t = TempDir::new().unwrap();
    let a = auth(&t);
    let options = Options::new(t.path().join("ledger"));
    let mut invalid = a.clone();
    invalid.thread_id = "name-not-uuid".into();
    assert!(Controller::open(&options, invalid).is_err());
    let mut invalid = a.clone();
    invalid.project = "relative".into();
    assert!(Controller::open(&options, invalid).is_err());
    let mut invalid = a.clone();
    invalid.history = "relative".into();
    assert!(Controller::open(&options, invalid).is_err());
    let mut invalid = a.clone();
    invalid.not_before = -1;
    assert!(Controller::open(&options, invalid).is_err());
    assert!(!options.data_root.exists());
    drop(Controller::open(&options, a.clone()).unwrap());
    let db = rusqlite::Connection::open(options.data_root.join("recovery.sqlite3")).unwrap();
    db.pragma_update(None, "user_version", 99).unwrap();
    drop(db);
    assert!(Controller::open(&options, a).is_err());
}

#[test]
fn structured_errors_never_classify_unknown_or_quota_as_network() {
    let t = TempDir::new().unwrap();
    let a = auth(&t);
    let db = rusqlite::Connection::open(&a.history).unwrap();
    db.execute_batch("CREATE TABLE thread_turns(thread_id,turn_id,rollout_ordinal,status,error_json,completed_at); CREATE TABLE thread_items(thread_id,turn_id,rollout_ordinal,item_json);").unwrap();
    assert!(read_latest(&a).unwrap().is_none());
    db.execute(
        "INSERT INTO thread_turns VALUES(?1,'last',1,'failed',NULL,100)",
        [&a.thread_id],
    )
    .unwrap();
    let cases = [
        (serde_json::json!("serverOverloaded"), true),
        (serde_json::json!("usageLimitExceeded"), false),
        (serde_json::json!({"unknown":{"httpStatusCode":503}}), false),
        (serde_json::json!({"responseStreamDisconnected":{}}), false),
        (
            serde_json::json!({"responseStreamDisconnected":{"httpStatusCode":null},"extra":true}),
            false,
        ),
    ];
    for (code, expected) in cases {
        let json = serde_json::json!({"codexErrorInfo":code});
        db.execute("UPDATE thread_turns SET error_json=?1", [json.to_string()])
            .unwrap();
        assert_eq!(
            read_latest(&a).unwrap().unwrap().error_code.is_some(),
            expected
        );
    }
    db.execute("UPDATE thread_turns SET error_json='not json'", [])
        .unwrap();
    assert!(read_latest(&a).is_err());
}

#[test]
fn reply_with_failed_turn_id_or_empty_id_is_quarantined() {
    for id in ["failed", ""] {
        let t = TempDir::new().unwrap();
        let mut c = Controller::open(&Options::new(t.path().join("ledger")), auth(&t)).unwrap();
        let mut sender = Sender {
            calls: 0,
            outcome: Outcome::Accepted { turn_id: id.into() },
        };
        assert_eq!(
            c.step(&failed("failed", 1, 100), 130, &mut sender).unwrap(),
            Decision::Attempt {
                number: 1,
                outcome: Outcome::Unknown
            }
        );
        assert_eq!(
            c.step(&failed("other", 2, 200), 330, &mut sender).unwrap(),
            Decision::Unknown
        );
    }
}
