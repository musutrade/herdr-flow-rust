//! Behavioural tests for the native TUI observation bridge.
//!
//! These mirror the original Python `test_codex_tui.py` suites: request /
//! response pairing, ephemeral-thread exclusion, settings runtime propagation,
//! WebSocket handshake observation, the review journal, marker/lock/socket
//! ownership and one real byte-proxy run.

#[path = "common/native_catalog.rs"]
mod catalog;
pub mod common;

use std::cell::RefCell;
use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::{symlink, PermissionsExt};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::thread;
use std::time::{Duration, Instant};

use herdr_flow::codex::rpc;
use herdr_flow::codex::tui::{
    cli, ObservationSink, ProxyBridge, ReviewEvents, ReviewTracker, SessionObserver, WireObserver,
    REVIEW_LIMIT,
};
use serde_json::{json, Map, Value};
use tempfile::TempDir;

const OLD: &str = "11111111-1111-1111-1111-111111111111";
const NEW: &str = "22222222-2222-2222-2222-222222222222";

#[derive(Clone, Default)]
struct Recorded {
    updates: Rc<RefCell<Vec<Map<String, Value>>>>,
    reviews: Rc<RefCell<Vec<Option<Value>>>>,
}

#[derive(Clone)]
struct RecordingSink(Recorded);

impl ObservationSink for RecordingSink {
    fn update(&mut self, fields: Map<String, Value>) -> herdr_flow::Result<()> {
        self.0.updates.borrow_mut().push(fields);
        Ok(())
    }

    fn review(
        &mut self,
        _thread_id: Option<&str>,
        event: Option<&Value>,
    ) -> herdr_flow::Result<()> {
        self.0.reviews.borrow_mut().push(event.cloned());
        Ok(())
    }
}

fn recording_observer() -> (SessionObserver, Recorded) {
    let recorded = Recorded::default();
    let observer =
        SessionObserver::new(Box::new(RecordingSink(recorded.clone()))).expect("observer");
    (observer, recorded)
}

fn start(observer: &mut SessionObserver, id: Value, method: &str, params: Value) {
    observer
        .request(&json!({"id": id, "method": method, "params": params}))
        .expect("request");
}

fn ready(observer: &mut SessionObserver, id: Value, session: &str) {
    observer
        .response(&json!({
            "id": id,
            "result": {
                "thread": {"id": session, "turns": ["private"]},
                "model": "m",
                "cwd": "/work",
                "reasoningEffort": "high"
            }
        }))
        .expect("response");
}

#[test]
fn only_matched_success_reports_ready() {
    let (mut observer, _recorded) = recording_observer();
    observer
        .response(&json!({"method": "thread/started", "params": {"thread": {"id": OLD}}}))
        .expect("notification");
    ready(&mut observer, json!(1), OLD);
    assert_eq!(observer.status(), "starting");

    start(&mut observer, json!(1), "thread/start", json!({}));
    ready(&mut observer, json!(2), OLD);
    assert_eq!(observer.status(), "starting");

    ready(&mut observer, json!(1), OLD);
    assert_eq!(observer.status(), "ready");
    assert_eq!(observer.session(), Some(OLD));
    let runtime = observer.runtime().expect("runtime");
    assert_eq!(runtime["thread"], json!({"id": OLD}));
    assert!(runtime["thread"].get("turns").is_none());
    assert_eq!(runtime["model"], json!("m"));
}

#[test]
fn starting_a_new_thread_clears_identity_and_ignores_obsolete_response() {
    let (mut observer, _recorded) = recording_observer();
    start(&mut observer, json!(1), "thread/start", json!({}));
    ready(&mut observer, json!(1), OLD);
    start(&mut observer, json!(2), "thread/start", json!({}));
    assert_eq!(observer.status(), "starting");
    assert_eq!(observer.session(), None);
    start(&mut observer, json!(3), "thread/start", json!({}));
    ready(&mut observer, json!(2), OLD);
    assert_eq!(observer.status(), "starting");
    ready(&mut observer, json!(3), NEW);
    assert_eq!(observer.session(), Some(NEW));
}

#[test]
fn error_mismatched_resume_and_invalid_identity_report_error() {
    let (mut observer, _recorded) = recording_observer();
    start(&mut observer, json!(1), "thread/start", json!({}));
    observer
        .response(&json!({"id": 1, "error": {"code": -32600, "message": "no rollout"}}))
        .expect("error response");
    assert_eq!(observer.status(), "error");
    assert_eq!(observer.session(), None);

    start(
        &mut observer,
        json!("r"),
        "thread/resume",
        json!({"threadId": OLD}),
    );
    ready(&mut observer, json!("r"), NEW);
    assert_eq!(observer.status(), "error");

    start(&mut observer, json!(3), "thread/start", json!({}));
    ready(&mut observer, json!(3), "not-a-uuid");
    assert_eq!(observer.status(), "error");

    start(&mut observer, json!(4), "thread/start", json!({}));
    observer
        .response(&json!({"id": 4, "result": {}}))
        .expect("empty result");
    assert_eq!(observer.status(), "error");
}

#[test]
fn json_rpc_id_types_cannot_cross_match() {
    let (mut observer, _recorded) = recording_observer();
    start(&mut observer, json!("1"), "thread/start", json!({}));
    ready(&mut observer, json!(1), OLD);
    assert_eq!(observer.status(), "starting");
    ready(&mut observer, json!("1"), OLD);
    assert_eq!(observer.status(), "ready");
}

#[test]
fn helper_and_ephemeral_threads_preserve_the_main_binding() {
    let (mut observer, recorded) = recording_observer();
    start(&mut observer, json!(1), "thread/start", json!({}));
    ready(&mut observer, json!(1), OLD);
    let before = recorded.updates.borrow().len();
    for (index, params) in [
        json!({"ephemeral": true, "threadSource": "thread_title"}),
        json!({"ephemeral": false, "threadSource": "thread_title"}),
        json!({"ephemeral": false, "threadSource": "subagent"}),
        json!({"ephemeral": true}),
        json!({"ephemeral": true, "threadSource": "user"}),
    ]
    .into_iter()
    .enumerate()
    {
        for method in ["thread/start", "thread/fork"] {
            let identity = format!("helper-{index}-{method}");
            start(&mut observer, json!(identity), method, params.clone());
            assert_eq!(recorded.updates.borrow().len(), before);
            ready(&mut observer, json!(identity), NEW);
            assert_eq!(observer.session(), Some(OLD));
        }
    }

    // A real interleaved main request is the only one that can bind.
    start(
        &mut observer,
        json!("main"),
        "thread/start",
        json!({"ephemeral": false, "threadSource": "user"}),
    );
    start(
        &mut observer,
        json!("title"),
        "thread/start",
        json!({"ephemeral": true, "threadSource": "thread_title"}),
    );
    ready(&mut observer, json!("main"), OLD);
    ready(&mut observer, json!("title"), NEW);
    assert_eq!(observer.status(), "ready");
    assert_eq!(observer.session(), Some(OLD));
}

#[test]
fn settings_notification_updates_only_the_ready_main_runtime() {
    let (mut observer, _recorded) = recording_observer();
    start(&mut observer, json!(1), "thread/start", json!({}));
    ready(&mut observer, json!(1), OLD);
    let notification = json!({
        "method": "thread/settings/updated",
        "params": {
            "threadId": OLD,
            "threadSettings": {
                "model": "next",
                "effort": "xhigh",
                "cwd": "/new-work",
                "sandboxPolicy": {"type": "dangerFullAccess"}
            }
        }
    });
    observer.response(&notification).expect("settings");
    let runtime = observer.runtime().expect("runtime");
    assert_eq!(runtime["reasoningEffort"], json!("xhigh"));
    assert_eq!(runtime["sandbox"], json!({"type": "dangerFullAccess"}));
    assert_eq!(runtime["thread"]["cwd"], json!("/new-work"));
    let before = runtime.clone();

    let mut wrong = notification.clone();
    wrong["params"]["threadId"] = json!(NEW);
    observer.response(&wrong).expect("wrong thread");
    assert_eq!(observer.runtime(), Some(&before));

    start(&mut observer, json!(2), "thread/start", json!({}));
    wrong["params"]["threadId"] = json!(OLD);
    observer.response(&wrong).expect("starting thread");
    assert!(observer.runtime().is_none());
}

#[test]
fn wire_observer_handles_partial_handshakes_and_frames() {
    let (observer, _recorded) = recording_observer();
    let observer = Rc::new(RefCell::new(observer));
    let mut client = WireObserver::new(true, observer.clone());
    let mut server = WireObserver::new(false, observer.clone());

    let request = json!({"id": 1, "method": "thread/start", "params": {}});
    let request_payload = serde_json::to_vec(&request).expect("request json");
    let frame = rpc::encode_frame(&request_payload, 1, true, true).expect("client frame");
    let mut wire = b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n".to_vec();
    wire.extend_from_slice(&frame);
    for byte in wire {
        client.feed(&[byte]).expect("client byte");
    }

    let response = json!({"id": 1, "result": {"thread": {"id": NEW}}});
    let response_payload = serde_json::to_vec(&response).expect("response json");
    let frame = rpc::encode_frame(&response_payload, 1, false, true).expect("server frame");
    let mut wire = b"HTTP/1.1 101 Switching Protocols\r\n\r\n".to_vec();
    wire.extend_from_slice(&frame);
    for byte in wire {
        server.feed(&[byte]).expect("server byte");
    }
    assert_eq!(observer.borrow().session(), Some(NEW));
}

#[test]
fn native_server_depth_is_separate_from_client_and_byte_limits() {
    for (client, depth, accepted) in [
        (false, 64, true),
        (false, 65, false),
        (true, 10, true),
        (true, 11, false),
    ] {
        let (observer, _) = recording_observer();
        let mut wire = WireObserver::new(client, Rc::new(RefCell::new(observer)));
        let header = if client {
            b"GET / HTTP/1.1\r\n\r\n".as_slice()
        } else {
            b"HTTP/1.1 101 OK\r\n\r\n".as_slice()
        };
        wire.feed(header).unwrap();
        let payload = serde_json::to_vec(&catalog::nested_message(depth)).unwrap();
        let frame = rpc::encode_frame(&payload, 1, client, true).unwrap();
        assert_eq!(
            wire.feed(&frame).is_ok(),
            accepted,
            "client={client}, depth={depth}"
        );
    }
    let (observer, _) = recording_observer();
    let mut wire = WireObserver::with_limit(false, Rc::new(RefCell::new(observer)), 32);
    wire.feed(b"HTTP/1.1 101 OK\r\n\r\n").unwrap();
    let frame = rpc::encode_frame(
        &serde_json::to_vec(&catalog::catalog()).unwrap(),
        1,
        false,
        true,
    )
    .unwrap();
    assert!(wire.feed(&frame).is_err());
}

#[test]
fn only_known_native_start_tool_schemas_get_extra_depth() {
    let mut cases = Vec::new();
    for namespace in [false, true] {
        cases.push((catalog::tool_start(12, namespace), true));
        cases.push((catalog::tool_start(64, namespace), true));
        cases.push((catalog::tool_start(65, namespace), false));
    }
    let mut unknown = catalog::tool_start(12, true);
    unknown["method"] = json!("future/start");
    cases.push((unknown, false));
    let mut config = catalog::tool_start(12, true);
    config["params"]["config"] = catalog::nested_message(10);
    cases.push((config, false));
    let mut unknown_tool = catalog::tool_start(12, true);
    unknown_tool["params"]["dynamicTools"][0]["tools"][0]["type"] = json!("future");
    cases.push((unknown_tool, false));
    for (request, accepted) in cases {
        let payload = serde_json::to_vec(&request).unwrap();
        assert!(rpc::decode_message(&payload).is_err());
        let (observer, _) = recording_observer();
        let mut wire = WireObserver::new(true, Rc::new(RefCell::new(observer)));
        wire.feed(b"GET / HTTP/1.1\r\n\r\n").unwrap();
        let frame = rpc::encode_frame(&payload, 1, true, true).unwrap();
        assert_eq!(wire.feed(&frame).is_ok(), accepted);
    }
}

#[test]
fn native_wire_rejects_oversize_server_and_client_frames_from_headers() {
    for (client, limit) in [(false, 128 << 20), (true, rpc::MAX_MESSAGE)] {
        let (observer, _) = recording_observer();
        let mut wire = WireObserver::new(client, Rc::new(RefCell::new(observer)));
        let header = if client {
            b"GET / HTTP/1.1\r\n\r\n".as_slice()
        } else {
            b"HTTP/1.1 101 OK\r\n\r\n".as_slice()
        };
        wire.feed(header).unwrap();
        let mut frame = vec![0x81, if client { 0xff } else { 0x7f }];
        frame.extend_from_slice(&((limit + 1) as u64).to_be_bytes());
        assert!(wire
            .feed(&frame)
            .unwrap_err()
            .message
            .contains("exceeds limit"));
    }
}

#[test]
fn wire_observer_rejects_close_and_binary_frames() {
    let (observer, _recorded) = recording_observer();
    let observer = Rc::new(RefCell::new(observer));
    let mut server = WireObserver::new(false, observer);
    server
        .feed(b"HTTP/1.1 101 Switching Protocols\r\n\r\n")
        .expect("handshake");
    let close = rpc::encode_frame(b"", 8, false, true).expect("close frame");
    let error = server.feed(&close).expect_err("close must be rejected");
    assert!(error.to_string().contains("sent close"));

    let (observer, _recorded) = recording_observer();
    let observer = Rc::new(RefCell::new(observer));
    let mut server = WireObserver::new(false, observer);
    server
        .feed(b"HTTP/1.1 101 Switching Protocols\r\n\r\n")
        .expect("handshake");
    let binary = rpc::encode_frame(b"{}", 2, false, true).expect("binary frame");
    let error = server.feed(&binary).expect_err("binary must be rejected");
    assert!(error.to_string().contains("binary"));
}

// ---------------------------------------------------------------------------
// Review journal
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct StoreSink {
    store: Rc<RefCell<ReviewEvents>>,
}

impl ObservationSink for StoreSink {
    fn update(&mut self, _fields: Map<String, Value>) -> herdr_flow::Result<()> {
        Ok(())
    }

    fn review(&mut self, thread_id: Option<&str>, event: Option<&Value>) -> herdr_flow::Result<()> {
        self.store.borrow_mut().update(thread_id, event)
    }
}

struct ReviewFixture {
    _temporary: TempDir,
    root: PathBuf,
    observer: SessionObserver,
}

impl ReviewFixture {
    fn new() -> Self {
        let temporary = common::tempdir().expect("tempdir");
        let root = temporary.path().to_path_buf();
        let store = Rc::new(RefCell::new(
            ReviewEvents::new(&root, "launch-one").expect("review events"),
        ));
        let observer = SessionObserver::new(Box::new(StoreSink {
            store: store.clone(),
        }))
        .expect("observer");
        Self {
            _temporary: temporary,
            root,
            observer,
        }
    }

    fn snapshot(&self) -> Value {
        let text = fs::read_to_string(self.root.join("review-events.json")).expect("snapshot");
        serde_json::from_str(&text).expect("snapshot json")
    }

    fn bind(&mut self, thread: &str, identity: Value) {
        start(
            &mut self.observer,
            identity.clone(),
            "thread/start",
            json!({}),
        );
        self.observer
            .response(&json!({"id": identity, "result": {"thread": {"id": thread}}}))
            .expect("bind");
    }

    fn notify(&mut self, status: &str, thread: &str, review_id: &str, turn: &str, extra: Value) {
        let mut params = json!({
            "threadId": thread,
            "turnId": turn,
            "reviewId": review_id,
            "startedAtMs": 1234,
            "review": {
                "status": status,
                "rationale": "Reason",
                "riskLevel": "low",
                "userAuthorization": "high"
            },
            "action": {"command": ["PRIVATE-COMMAND"], "toolArgs": {"secret": "DO-NOT-STORE"}}
        });
        if status != "inProgress" {
            params["completedAtMs"] = json!(2345);
            params["decisionSource"] = json!("agent");
        }
        if let Some(extra) = extra.as_object() {
            for (key, value) in extra {
                params[key] = value.clone();
            }
        }
        let method = if status == "inProgress" {
            "item/autoApprovalReview/started"
        } else {
            "item/autoApprovalReview/completed"
        };
        self.observer
            .response(&json!({"method": method, "params": params}))
            .expect("review notification");
    }
}

#[test]
fn review_events_require_the_ready_main_thread() {
    let mut fixture = ReviewFixture::new();
    fixture.notify("inProgress", OLD, "review-1", "turn-1", json!({}));
    assert!(fixture.snapshot()["events"].as_array().unwrap().is_empty());
    fixture.bind(OLD, json!(1));
    fixture.notify("inProgress", NEW, "wrong-thread", "turn-1", json!({}));
    fixture
        .observer
        .response(&json!({
            "id": 99,
            "method": "item/autoApprovalReview/started",
            "params": {
                "threadId": OLD,
                "turnId": "turn-1",
                "reviewId": "request",
                "review": {"status": "inProgress"}
            }
        }))
        .expect("server request");
    fixture.notify("inProgress", OLD, "review-1", "turn-1", json!({}));
    let snapshot = fixture.snapshot();
    assert_eq!(snapshot["thread_id"], json!(OLD));
    let events = snapshot["events"].as_array().expect("events");
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["status"], json!("inProgress"));
    assert!(events[0].get("action").is_none());
    let serialized = serde_json::to_string(events).expect("serialize");
    assert!(!serialized.contains("PRIVATE-COMMAND"));
    assert!(!serialized.contains("DO-NOT-STORE"));
}

#[test]
fn terminal_statuses_carry_metadata_and_do_not_store_actions() {
    let mut fixture = ReviewFixture::new();
    fixture.bind(OLD, json!(1));
    for status in ["approved", "denied", "timedOut", "aborted"] {
        let rationale = "x".repeat(1100);
        fixture.notify(
            status,
            OLD,
            status,
            "turn-1",
            json!({"review": {
                "status": status,
                "rationale": rationale,
                "riskLevel": "high",
                "userAuthorization": "low",
                "toolArgs": {"secret": "DO-NOT-STORE"}
            }}),
        );
    }
    let snapshot = fixture.snapshot();
    let events = snapshot["events"].as_array().expect("events");
    let statuses: Vec<&str> = events
        .iter()
        .map(|event| event["status"].as_str().unwrap_or_default())
        .collect();
    assert_eq!(statuses, ["approved", "denied", "timedOut", "aborted"]);
    for event in events {
        assert_eq!(event["rationale"].as_str().unwrap().chars().count(), 1000);
        assert_eq!(event["decision_source"], json!("agent"));
        assert_eq!(event["started_at_ms"], json!(1234));
        assert_eq!(event["completed_at_ms"], json!(2345));
        assert_eq!(event["risk_level"], json!("high"));
        assert_eq!(event["user_authorization"], json!("low"));
    }
    let text = fs::read_to_string(fixture.root.join("review-events.json")).expect("snapshot");
    assert!(!text.contains("PRIVATE-COMMAND"));
    assert!(!text.contains("DO-NOT-STORE"));
}

#[test]
fn duplicate_ids_are_stable_and_late_starts_do_not_reopen_reviews() {
    let mut fixture = ReviewFixture::new();
    fixture.bind(OLD, json!(1));
    fixture.notify("inProgress", OLD, "review-1", "turn-1", json!({}));
    fixture.notify("inProgress", OLD, "review-1", "turn-1", json!({}));
    let first = fixture.snapshot()["events"][0].clone();
    let first_id = first["event_id"].clone();
    fixture.notify("approved", OLD, "review-1", "turn-1", json!({}));
    fixture.notify("approved", OLD, "review-1", "turn-1", json!({}));
    fixture.notify("inProgress", OLD, "review-1", "turn-1", json!({}));
    fixture
        .observer
        .response(&json!({
            "method": "turn/completed",
            "params": {"threadId": OLD, "turn": {"id": "turn-1", "status": "failed"}}
        }))
        .expect("turn");
    let events = fixture.snapshot();
    let items = events["events"].as_array().expect("events");
    let statuses: Vec<&str> = items
        .iter()
        .map(|item| item["status"].as_str().unwrap_or_default())
        .collect();
    assert_eq!(statuses, ["inProgress", "approved"]);
    assert_eq!(items[0], first);
    assert_eq!(items[0]["event_id"], first_id);
    assert!(fixture.observer.reviews().is_empty());
}

#[test]
fn warnings_are_deduplicated_associated_and_truncated() {
    let mut fixture = ReviewFixture::new();
    fixture.bind(OLD, json!(1));
    let message = "w".repeat(1100);
    for thread in [NEW, OLD, OLD] {
        fixture
            .observer
            .response(&json!({
                "method": "guardianWarning",
                "params": {"threadId": thread, "message": message}
            }))
            .expect("warning");
    }
    let events = fixture.snapshot();
    let items = events["events"].as_array().expect("events");
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["kind"], json!("guardianWarning"));
    assert_eq!(items[0]["status"], json!("guardianWarning"));
    assert_eq!(
        items[0]["rationale"].as_str().unwrap().chars().count(),
        1000
    );

    fixture.notify("inProgress", OLD, "review-1", "turn-1", json!({}));
    fixture
        .observer
        .response(&json!({
            "method": "guardianWarning",
            "params": {"threadId": OLD, "message": "associated"}
        }))
        .expect("associated warning");
    let snapshot = fixture.snapshot();
    let items = snapshot["events"].as_array().expect("events");
    let warning = &items[2];
    assert_eq!(warning["related_review_id"], json!("review-1"));
    assert_eq!(warning["turn_id"], json!("turn-1"));
    assert!(warning["review_id"]
        .as_str()
        .unwrap_or_default()
        .ends_with(":review-1"));
}

#[test]
fn lookup_methods_do_not_claim_a_frontend_switch() {
    let (mut observer, _recorded) = recording_observer();
    start(&mut observer, json!(1), "thread/start", json!({}));
    ready(&mut observer, json!(1), OLD);
    for method in [
        "thread/read",
        "thread/list",
        "thread/loaded/list",
        "thread/unsubscribe",
    ] {
        start(
            &mut observer,
            json!(method),
            method,
            json!({"threadId": NEW}),
        );
        ready(&mut observer, json!(method), NEW);
    }
    assert_eq!(observer.status(), "ready");
    assert_eq!(observer.session(), Some(OLD));
}

#[test]
fn ambiguous_pending_reviews_do_not_guess_warning_identity() {
    let mut fixture = ReviewFixture::new();
    fixture.bind(OLD, json!(1));
    fixture.notify("inProgress", OLD, "one", "turn-1", json!({}));
    fixture.notify("inProgress", OLD, "two", "turn-1", json!({}));
    fixture
        .observer
        .response(&json!({
            "method": "guardianWarning",
            "params": {"threadId": OLD, "message": "Ambiguous concurrent review warning"}
        }))
        .expect("warning");
    let snapshot = fixture.snapshot();
    let items = snapshot["events"].as_array().expect("events");
    let warning = items.last().expect("warning");
    assert_eq!(warning["status"], json!("guardianWarning"));
    assert!(warning.get("related_review_id").is_none());
}

#[test]
fn schema_drift_warns_without_copying_native_payloads() {
    let mut fixture = ReviewFixture::new();
    fixture.bind(OLD, json!(1));
    fixture.notify("newStatus", NEW, "wrong-thread", "turn-1", json!({}));
    fixture.notify(
        "newStatus",
        OLD,
        "bad-status",
        "turn-1",
        json!({"review": {"status": "newStatus", "rationale": "PRIVATE-COMMAND"}}),
    );
    fixture.notify(
        "inProgress",
        OLD,
        "bad-shape",
        "turn-1",
        json!({"review": null}),
    );
    let snapshot = fixture.snapshot();
    let items = snapshot["events"].as_array().expect("events");
    assert_eq!(items.len(), 2);
    assert!(items
        .iter()
        .all(|event| event["status"] == json!("guardianWarning")));
    let serialized = serde_json::to_string(items).expect("serialize");
    assert!(!serialized.contains("PRIVATE-COMMAND"));
    assert!(items
        .iter()
        .all(|event| event.get("related_review_id").is_none()));
}

#[test]
fn same_launch_resume_preserves_unconsumed_events() {
    let mut fixture = ReviewFixture::new();
    fixture.bind(OLD, json!(1));
    fixture.notify("denied", OLD, "review-1", "turn-1", json!({}));
    let before = fixture.snapshot()["events"].clone();
    start(
        &mut fixture.observer,
        json!(2),
        "thread/resume",
        json!({"threadId": OLD}),
    );
    assert!(fixture.snapshot()["thread_id"].is_null());
    assert!(fixture.snapshot()["events"].as_array().unwrap().is_empty());
    fixture
        .observer
        .response(&json!({"id": 2, "result": {"thread": {"id": OLD}}}))
        .expect("resume");
    assert_eq!(fixture.snapshot()["events"], before);
    assert_eq!(fixture.snapshot()["thread_id"], json!(OLD));
}

#[test]
fn failed_turn_aborts_only_its_pending_reviews() {
    for status in ["failed", "interrupted"] {
        let mut fixture = ReviewFixture::new();
        fixture.bind(OLD, json!(1));
        fixture.notify("inProgress", OLD, status, status, json!({}));
        fixture.notify("inProgress", OLD, "other", "other-turn", json!({}));
        fixture
            .observer
            .response(&json!({
                "method": "turn/completed",
                "params": {"threadId": NEW, "turn": {"id": status, "status": status}}
            }))
            .expect("wrong thread turn");
        let latest = fixture.snapshot();
        assert_eq!(
            latest["events"].as_array().unwrap().last().unwrap()["status"],
            json!("inProgress")
        );
        fixture
            .observer
            .response(&json!({
                "method": "turn/completed",
                "params": {
                    "threadId": OLD,
                    "turn": {
                        "id": status,
                        "status": status,
                        "error": {"message": "PRIVATE-COMMAND"}
                    }
                }
            }))
            .expect("turn abort");
        let snapshot = fixture.snapshot();
        let items = snapshot["events"].as_array().expect("events");
        let event = items.last().expect("aborted event");
        assert_eq!(event["review_id"], json!(status));
        assert_eq!(event["status"], json!("aborted"));
        assert_eq!(event["source"], json!("turn/completed"));
        assert!(event["rationale"].as_str().unwrap().contains(status));
        assert!(!serde_json::to_string(event)
            .unwrap()
            .contains("PRIVATE-COMMAND"));
        assert!(fixture
            .observer
            .reviews()
            .iter()
            .any(|review| review == "other"));
    }
}

#[test]
fn launch_and_thread_switches_archive_history() {
    let mut fixture = ReviewFixture::new();
    fixture.bind(OLD, json!(1));
    fixture.notify("denied", OLD, "review-1", "turn-1", json!({}));
    start(
        &mut fixture.observer,
        json!(2),
        "thread/fork",
        json!({"threadId": OLD}),
    );
    assert!(fixture.snapshot()["thread_id"].is_null());
    fixture
        .observer
        .response(&json!({"id": 2, "result": {"thread": {"id": NEW}}}))
        .expect("fork");
    fixture.notify("approved", NEW, "review-2", "turn-2", json!({}));
    assert_eq!(fixture.snapshot()["thread_id"], json!(NEW));

    // A new launch archives the active snapshot before writing its own.
    let replacement = ReviewEvents::new(&fixture.root, "launch-two").expect("launch two");
    drop(replacement);
    let snapshot = fixture.snapshot();
    assert_eq!(snapshot["nonce"], json!("launch-two"));
    assert!(snapshot["thread_id"].is_null());
    assert!(snapshot["events"].as_array().unwrap().is_empty());

    let history: Vec<Value> = fs::read_dir(fixture.root.join("review-history"))
        .expect("history")
        .flatten()
        .map(|entry| {
            let text = fs::read_to_string(entry.path()).expect("history file");
            serde_json::from_str(&text).expect("history json")
        })
        .collect();
    let threads: Vec<&str> = history
        .iter()
        .map(|item| item["thread_id"].as_str().unwrap_or_default())
        .collect();
    assert!(threads.contains(&OLD));
    assert!(threads.contains(&NEW));
    assert!(history
        .iter()
        .all(|item| item["nonce"] == json!("launch-one")));
    let statuses: Vec<&str> = history
        .iter()
        .map(|item| item["events"][0]["status"].as_str().unwrap_or_default())
        .collect();
    assert!(statuses.contains(&"approved"));
    assert!(statuses.contains(&"denied"));

    let review_mode = fs::symlink_metadata(fixture.root.join("review-events.json"))
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    let history_mode = fs::symlink_metadata(fixture.root.join("review-history"))
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(review_mode, 0o600);
    assert_eq!(history_mode, 0o700);
}

#[test]
fn retention_limit_has_visible_noop_overflow() {
    let mut fixture = ReviewFixture::new();
    fixture.bind(OLD, json!(1));
    for index in 0..105 {
        fixture.notify("denied", OLD, &index.to_string(), "turn-1", json!({}));
    }
    let snapshot = fixture.snapshot();
    let events = snapshot["events"].as_array().expect("events");
    assert_eq!(events.len(), 100);
    assert_eq!(snapshot["overflow_count"], json!(5));
    assert_eq!(events[0]["review_id"], json!("5"));
    fixture.notify("denied", OLD, "104", "turn-1", json!({}));
    assert_eq!(fixture.snapshot(), snapshot);
}

#[test]
fn review_tracker_bounds_pending_and_completed() {
    assert_eq!(
        herdr_flow::codex::tui::review_transition("inProgress"),
        Some("start")
    );
    for status in ["approved", "denied", "timedOut", "aborted"] {
        assert_eq!(
            herdr_flow::codex::tui::review_transition(status),
            Some("finish")
        );
    }
    assert_eq!(herdr_flow::codex::tui::review_transition("newStatus"), None);

    let mut tracker = ReviewTracker::new();
    for index in 0..REVIEW_LIMIT {
        let event = json!({
            "status": "inProgress",
            "review_id": index.to_string(),
            "turn_id": "turn-1",
            "thread_id": OLD,
            "kind": "autoApprovalReview"
        });
        assert!(tracker.apply(&event).is_none());
    }
    let warning = tracker.apply(&json!({
        "status": "inProgress",
        "review_id": "new",
        "turn_id": "turn-1",
        "thread_id": OLD,
        "kind": "autoApprovalReview"
    }));
    assert_eq!(
        warning.as_deref(),
        Some("More than 100 native auto-reviews remain pending.")
    );
    assert_eq!(tracker.pending.len(), REVIEW_LIMIT);
    assert!(!tracker.pending_contains("0"));
    assert!(tracker.pending_contains("new"));

    let mut tracker = ReviewTracker::new();
    for index in 0..(REVIEW_LIMIT + 5) {
        let review_id = index.to_string();
        tracker.apply(&json!({
            "status": "inProgress", "review_id": review_id,
            "turn_id": "turn-1", "thread_id": OLD, "kind": "autoApprovalReview"
        }));
        tracker.apply(&json!({
            "status": "approved", "review_id": review_id,
            "turn_id": "turn-1", "thread_id": OLD, "kind": "autoApprovalReview"
        }));
    }
    assert_eq!(tracker.completed.len(), REVIEW_LIMIT);
    assert!(!tracker.completed_contains("0"));
    let pending_before = tracker.pending.clone();
    tracker.apply(&json!({
        "status": "inProgress", "review_id": (REVIEW_LIMIT + 4).to_string(),
        "turn_id": "turn-1", "thread_id": OLD, "kind": "autoApprovalReview"
    }));
    assert_eq!(tracker.pending, pending_before);

    let mut tracker = ReviewTracker::new();
    tracker.apply(&json!({
        "status": "inProgress", "review_id": "matching",
        "turn_id": "turn-1", "thread_id": OLD, "kind": "autoApprovalReview"
    }));
    tracker.apply(&json!({
        "status": "inProgress", "review_id": "other",
        "turn_id": "turn-2", "thread_id": OLD, "kind": "autoApprovalReview"
    }));
    let recorded = tracker.abort_turn(&json!({"id": "turn-1", "status": "failed"}));
    assert_eq!(recorded.len(), 1);
    assert_eq!(recorded[0]["status"], json!("aborted"));
    assert_eq!(recorded[0]["source"], json!("turn/completed"));
    assert!(recorded[0]["rationale"]
        .as_str()
        .unwrap()
        .contains("failed"));
    assert!(!tracker.pending_contains("matching"));
    assert!(tracker.pending_contains("other"));
    assert!(tracker.unique_pending().is_some());
}

// ---------------------------------------------------------------------------
// Bridge lifecycle
// ---------------------------------------------------------------------------

fn file_mode(path: &Path) -> u32 {
    fs::symlink_metadata(path)
        .expect("metadata")
        .permissions()
        .mode()
        & 0o777
}

#[test]
fn bridge_prepares_private_marker_lock_socket_and_cleanup() {
    let temporary = common::tempdir().expect("tempdir");
    let root = temporary.path().join("state");
    let mut first = ProxyBridge::new(&root, "/server", "/binary", "/cwd", "fresh", None);
    let mut second = ProxyBridge::new(&root, "/server", "/binary", "/cwd", "other", None);
    first.prepare().expect("first prepare");
    let marker = first.marker_value().expect("marker");
    assert_eq!(marker["nonce"], json!("fresh"));
    assert_eq!(marker["status"], json!("starting"));
    assert!(marker["session"].is_null());
    assert_eq!(marker["review_events_version"], json!(1));
    assert_eq!(marker["server_socket"], json!("/server"));
    assert_eq!(
        first.review_events_value().unwrap()["nonce"],
        json!("fresh")
    );
    assert_eq!(file_mode(&root), 0o700);
    assert_eq!(file_mode(&root.join("tui.json")), 0o600);
    assert_eq!(file_mode(&root.join("tui.sock")), 0o600);

    let error = second.prepare().expect_err("lock must be exclusive");
    assert!(error.to_string().contains("another TUI bridge"));
    assert_eq!(first.marker_value().unwrap()["nonce"], json!("fresh"));

    second.cleanup().expect("second cleanup");
    first.cleanup().expect("first cleanup");
    assert!(!root.join("tui.sock").exists());
}

#[test]
fn bridge_uses_proc_fd_alias_for_long_socket_paths() {
    let temporary = common::tempdir().expect("tempdir");
    let root = temporary.path().join("long-".repeat(24));
    let mut bridge = ProxyBridge::new(&root, "/server", "/binary", "/cwd", "fresh", None);
    bridge.prepare().expect("prepare");
    let endpoint = bridge.proxy_endpoint().expect("endpoint").to_string();
    assert!(endpoint.starts_with(&format!("unix:///proc/{}/fd/", std::process::id())));
    let alias = Path::new(endpoint.trim_start_matches("unix://"));
    let alias_inode = fs::metadata(alias).expect("alias").ino();
    let real_inode = fs::metadata(root.join("tui.sock")).expect("real").ino();
    use std::os::unix::fs::MetadataExt;
    assert_eq!(alias_inode, real_inode);
    bridge.cleanup().expect("cleanup");
    assert!(!root.join("tui.sock").exists());
}

#[test]
fn bridge_refuses_symlink_state_directory_and_non_socket_proxy() {
    let temporary = common::tempdir().expect("tempdir");
    let root = temporary.path().to_path_buf();
    let real = root.join("real");
    common::create_dir(&real).expect("real dir");
    let link = root.join("link");
    symlink(&real, &link).expect("symlink");
    let mut bridge = ProxyBridge::new(&link, "/server", "/binary", "/", "fresh", None);
    assert!(bridge.prepare().is_err());
    bridge.cleanup().expect("cleanup");

    fs::write(real.join("tui.sock"), "keep").expect("regular file");
    let mut bridge = ProxyBridge::new(&real, "/server", "/binary", "/", "fresh", None);
    assert!(bridge.prepare().is_err());
    bridge.cleanup().expect("cleanup");
    assert_eq!(fs::read_to_string(real.join("tui.sock")).unwrap(), "keep");
}

#[test]
fn cli_usage_errors_exit_two_runtime_errors_exit_one() {
    assert_eq!(cli(&[]), 2);
    assert_eq!(cli(&["--help".to_string()]), 0);

    let temporary = common::tempdir().expect("tempdir");
    let state = temporary.path().join("state");
    let args = vec![
        "--state-dir".to_string(),
        state.display().to_string(),
        "--socket".to_string(),
        "/server.sock".to_string(),
        "--binary".to_string(),
        "/not-a-real-binary".to_string(),
        "--cwd".to_string(),
        "/cwd".to_string(),
        "--nonce".to_string(),
        String::new(),
    ];
    // Invalid nonce is a runtime bridge error, printed to stderr, exit 1.
    assert_eq!(cli(&args), 1);
    assert!(!state.join("tui.sock").exists());

    let missing_binary = vec![
        "--state-dir".to_string(),
        state.display().to_string(),
        "--socket".to_string(),
        "/server.sock".to_string(),
        "--binary".to_string(),
        "/not-a-real-binary".to_string(),
        "--cwd".to_string(),
        "/cwd".to_string(),
        "--nonce".to_string(),
        "fresh".to_string(),
    ];
    // Preparation succeeds, spawn fails, and cleanup still removes the socket.
    assert_eq!(cli(&missing_binary), 1);
    assert!(!state.join("tui.sock").exists());
}

#[test]
fn bridge_validation_accepts_only_canonical_uuid_threads() {
    assert_eq!(
        herdr_flow::codex::tui::thread_uuid(OLD).unwrap(),
        OLD.to_string()
    );
    assert!(herdr_flow::codex::tui::thread_uuid("not-a-uuid").is_err());
    let no_dash = NEW.replace('-', "");
    assert!(herdr_flow::codex::tui::thread_uuid(&no_dash).is_err());
    assert_eq!(
        herdr_flow::codex::tui::thread_uuid(&OLD.to_uppercase()).unwrap(),
        OLD
    );

    let temporary = common::tempdir().expect("tempdir");
    let mut bridge = ProxyBridge::new(
        temporary.path().join("state"),
        "/server",
        "/binary",
        "/cwd",
        "",
        None,
    );
    let error = bridge.prepare().expect_err("empty nonce");
    assert!(error.to_string().contains("nonce"));
}

// ---------------------------------------------------------------------------
// End-to-end byte proxy
// ---------------------------------------------------------------------------

fn read_until(stream: &mut std::os::unix::net::UnixStream, needle: &[u8], limit: usize) -> Vec<u8> {
    let mut data = Vec::new();
    let mut buffer = [0u8; 4096];
    while !data.windows(needle.len()).any(|window| window == needle) && data.len() < limit {
        match stream.read(&mut buffer) {
            Ok(0) => break,
            Ok(count) => data.extend_from_slice(&buffer[..count]),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                break
            }
            Err(_) => break,
        }
    }
    data
}

#[test]
fn real_byte_proxy_reports_ready_exit_and_reviews() {
    let temporary = common::tempdir().expect("tempdir");
    let root = temporary.path().to_path_buf();
    let state = root.join("state");
    let server_path = root.join("server.sock");
    let listener = UnixListener::bind(&server_path).expect("server listener");
    listener.set_nonblocking(true).expect("nonblocking");

    let mut request = catalog::tool_start(12, true);
    request["params"]["cwd"] = json!("/exact");
    let request_payload = serde_json::to_vec(&request).expect("request json");
    let request_frame = rpc::encode_frame(&request_payload, 1, true, true).expect("request frame");
    let response = json!({
        "id": 7,
        "result": {"thread": {"id": NEW}, "model": "native"}
    });
    let response_payload = serde_json::to_vec(&response).expect("response json");
    let catalog = serde_json::to_vec(&catalog::catalog()).unwrap();
    assert!(rpc::decode_message(&catalog).is_err());
    // Native plugin/list directory traffic is about 24 MB on the observed
    // service. Exercise the actual transparent byte proxy above 16 MiB too.
    let plugin = serde_json::to_vec(
        &json!({"id":90,"result":{"marketplaces":[{"description":"x".repeat(rpc::MAX_MESSAGE)}]}}),
    )
    .unwrap();
    assert!(rpc::decode_message(&plugin).is_err());
    let mut response_bytes =
        rpc::encode_frame_with_limit(&plugin, 1, false, true, 128 << 20).unwrap();
    response_bytes.extend(rpc::encode_frame(&catalog, 1, false, true).unwrap());
    response_bytes
        .extend(rpc::encode_frame(&response_payload, 1, false, true).expect("response frame"));
    let review = json!({
        "method": "item/autoApprovalReview/completed",
        "params": {
            "threadId": NEW,
            "turnId": "turn",
            "reviewId": "native-review",
            "review": {"status": "approved", "rationale": "Allowed"},
            "decisionSource": "agent",
            "action": {"command": ["DO-NOT-STORE"]}
        }
    });
    let review_payload = serde_json::to_vec(&review).expect("review json");
    response_bytes.extend_from_slice(&rpc::encode_frame(&review_payload, 1, false, true).unwrap());
    let expected_response = response_bytes.clone();
    let expected_request = request_frame.clone();
    let server_expected_response = expected_response.clone();
    let server_request_length = expected_request.len();

    let server_thread = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut accepted = None;
        while Instant::now() < deadline {
            match listener.accept() {
                Ok((stream, _)) => {
                    accepted = Some(stream);
                    break;
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(10));
                }
                Err(_) => break,
            }
        }
        let mut stream = accepted.expect("upstream accept");
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("timeout");
        let _handshake = read_until(&mut stream, b"\r\n\r\n", 64 * 1024);
        stream
            .write_all(b"HTTP/1.1 101 Switching Protocols\r\n\r\n")
            .expect("101");
        let mut captured = Vec::new();
        let mut buffer = [0u8; 4096];
        while captured.len() < server_request_length {
            let count = stream.read(&mut buffer).expect("request bytes");
            if count == 0 {
                break;
            }
            captured.extend_from_slice(&buffer[..count]);
        }
        stream
            .write_all(&server_expected_response)
            .expect("response");
        // Keep the upstream open until the native side has consumed the
        // response and closed, exactly like the real app-server connection.
        let mut sink = [0u8; 1024];
        loop {
            match stream.read(&mut sink) {
                Ok(0) => break,
                Ok(_) => {}
                Err(_) => break,
            }
        }
        captured
    });

    let native_root = root.clone();
    let native_expected = expected_response.clone();
    let native_request = expected_request.clone();
    let native_thread = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut endpoint = None;
        while Instant::now() < deadline {
            if let Ok(text) = fs::read_to_string(native_root.join("state/tui.json")) {
                if let Ok(marker) = serde_json::from_str::<Value>(&text) {
                    if let Some(value) = marker.get("proxy_endpoint").and_then(Value::as_str) {
                        endpoint = Some(value.trim_start_matches("unix://").to_string());
                        break;
                    }
                }
            }
            thread::sleep(Duration::from_millis(10));
        }
        let endpoint = endpoint.expect("proxy endpoint");
        let mut stream = std::os::unix::net::UnixStream::connect(endpoint).expect("native connect");
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("timeout");
        stream
            .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .expect("handshake");
        let _response_head = read_until(&mut stream, b"\r\n\r\n", 64 * 1024);
        stream.write_all(&native_request).expect("request frame");
        let mut received = vec![0u8; native_expected.len()];
        let mut read = 0;
        while read < native_expected.len() {
            let count = stream.read(&mut received[read..]).expect("response bytes");
            if count == 0 {
                break;
            }
            read += count;
        }
        received.truncate(read);

        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if let Ok(text) = fs::read_to_string(native_root.join("state/tui.json")) {
                if let Ok(marker) = serde_json::from_str::<Value>(&text) {
                    if marker["status"] == json!("ready") {
                        break;
                    }
                }
            }
            thread::sleep(Duration::from_millis(10));
        }
        drop(stream);
        // Wait for the proxy to drain both EOFs before ending the fake native
        // process. This tests connection cleanup independently of child exit.
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let closed = fs::read_to_string(native_root.join("state/tui.json"))
                .ok()
                .and_then(|text| serde_json::from_str::<Value>(&text).ok())
                .is_some_and(|marker| marker["error"] == "native WebSocket connection closed");
            if closed {
                break;
            }
            assert!(Instant::now() < deadline, "proxy did not drain both EOFs");
            thread::sleep(Duration::from_millis(10));
        }
        fs::write(native_root.join("native-finished"), b"consumed").expect("native completion");
        received
    });

    let binary = root.join("native");
    // Exit after the wire exchange, not after an arbitrary sleep that may
    // expire before instrumented proxy threads get scheduled. Retain a bound
    // and a nonzero exit if the exchange never finishes.
    fs::write(
        &binary,
        format!(
            "#!/bin/sh\nn=0\nwhile [ ! -f '{}/native-finished' ]; do\n  n=$((n+1))\n  [ \"$n\" -lt 500 ] || exit 2\n  sleep 0.01\ndone\n",
            root.display()
        ),
    )
    .expect("fake native");
    fs::set_permissions(&binary, fs::Permissions::from_mode(0o700)).expect("mode");
    let mut bridge = ProxyBridge::new(
        &state,
        &server_path,
        &binary.to_string_lossy(),
        &root.to_string_lossy(),
        "live",
        None,
    );
    assert_eq!(bridge.run(), 0);
    let captured = server_thread.join().expect("server thread");
    let received = native_thread.join().expect("native thread");
    assert_eq!(captured, expected_request);
    assert_eq!(received, expected_response);

    let marker = bridge.marker_value().expect("marker");
    assert_eq!(marker["status"], json!("exited"));
    assert_eq!(marker["nonce"], json!("live"));
    assert!(marker["pid_start_ticks"].as_i64().unwrap_or(0) > 0);
    let reviews = bridge.review_events_value().expect("reviews");
    assert_eq!(reviews["nonce"], json!("live"));
    assert_eq!(reviews["events"][0]["status"], json!("approved"));
    assert!(!serde_json::to_string(&reviews)
        .unwrap()
        .contains("DO-NOT-STORE"));
    assert!(!state.join("tui.sock").exists());
}
