use super::*;
use std::sync::{mpsc, Arc, Mutex};

const ID: &str = "reconnected-session";

fn result() -> J {
    json!({"thread":{"id":ID,"cwd":"/task","modelProvider":"openai","turns":[]},
        "model":"fixture","modelProvider":"openai","reasoningEffort":"high","cwd":"/task",
        "approvalPolicy":"on-request","approvalsReviewer":"user","runtimeWorkspaceRoots":[],
        "collaborationMode":{"mode":"default","settings":{"model":"fixture","reasoning_effort":"high"}},
        "multiAgentMode":"explicitRequestOnly","disabledPluginIds":[],
        "sandbox":{"type":"workspaceWrite","networkAccess":false,"writableRoots":[]}})
}

fn turn_policy() -> J {
    json!({"type":"workspaceWrite","networkAccess":false,"writableRoots":["/task"],
        "excludeTmpdirEnvVar":true,"excludeSlashTmp":true})
}

fn resumed_result() -> J {
    let mut resumed = result();
    resumed["sandbox"] = turn_policy();
    resumed["sandbox"]["writableRoots"] = json!([]);
    resumed
}

fn runtime(root: &Path, socket: &Path) {
    util::atomic_write_json(
        &root.join("runtime.json"),
        &json!({"configuration":"upstream",
        "instance":"subscription-fixture","cwd":"/task","approval_policy":"on-request",
        "model":{"providerID":"openai","id":"fixture","reasoning_effort":"high"},
        "upstream_socket":crate::task_runtime::upstream::socket_identity(socket).unwrap()}),
    )
    .unwrap();
}

fn request(method: &str, params: J) -> J {
    json!({"method":method,"params":params})
}

fn record(peer: &mut rpc_fixture::Peer, methods: &Mutex<Vec<J>>) -> J {
    let request = peer.request();
    methods.lock().unwrap().push(request.clone());
    request
}

#[test]
fn reconnect_resumes_same_session_before_new_turn_and_sends_one_original_tool_reply() {
    for explicit_resume in [false, true] {
        reconnect(explicit_resume);
    }
}

fn reconnect(explicit_resume: bool) {
    let root = common::tempdir().unwrap();
    let methods = Arc::new(Mutex::new(Vec::new()));
    let received = methods.clone();
    let (old_done, old_release) = mpsc::channel();
    let (new_done, new_release) = mpsc::channel();
    let tool = json!({"id":77,"method":"item/tool/call","params":{"threadId":ID,
        "turnId":"new-notification","callId":"original-ack","tool":"herdr_queue",
        "arguments":{"action":"ack","delivery":"original-delivery","spec":""}}});
    let emitted = tool.clone();
    let response = json!({"success":true,"contentItems":[{"type":"inputText","text":"original ACK recorded"}]});
    let expected = response.clone();
    let mut connection_index = 0;
    let server = rpc_fixture::Server::start_connections(2, move |mut peer| {
        peer.initialize();
        let first = record(&mut peer, &received);
        if connection_index == 0 {
            assert_eq!(first["method"], "thread/start");
            peer.json(&json!({"id":first["id"],"result":result()}));
            let turn = record(&mut peer, &received);
            assert_eq!(turn["method"], "turn/start");
            peer.json(&json!({"id":turn["id"],"result":{"turn":{"id":"old-notification"}}}));
            old_release.recv_timeout(Duration::from_secs(10)).unwrap();
        } else {
            // A metadata read or new turn on this new connection does not
            // subscribe. Only this exact same-thread resume enables tool events.
            assert_eq!(first["method"], "thread/resume");
            assert_eq!(first["params"], json!({"threadId":ID}));
            peer.json(&json!({"id":first["id"],"result":resumed_result()}));
            let turn = record(&mut peer, &received);
            assert_eq!(turn["method"], "turn/start");
            assert_eq!(turn["params"]["input"], "new-notification");
            peer.json(&json!({"id":turn["id"],"result":{"turn":{"id":"new-notification"}}}));
            peer.json(&emitted);
            assert_eq!(
                record(&mut peer, &received),
                json!({"id":77,"result":expected})
            );
            new_release.recv_timeout(Duration::from_secs(10)).unwrap();
        }
        connection_index += 1;
    });
    runtime(root.path(), &server.path);
    let mut connection = None;
    execute(
        root.path(),
        &mut connection,
        &request("thread/start", json!({"cwd":"/task"})),
    )
    .unwrap();
    execute(
        root.path(),
        &mut connection,
        &request(
            "turn/start",
            json!({"threadId":ID,"input":"old-notification","sandboxPolicy":turn_policy(),
                "model":"fixture","effort":"high"}),
        ),
    )
    .unwrap();
    let configuration = util::read_state_json(root.path(), "upstream-session.json").unwrap();
    assert_eq!(
        configuration["configuration_transition"]["phase"],
        "applied"
    );
    assert_eq!(
        configuration["configuration_transition"]["turn"],
        "old-notification"
    );
    assert_eq!(
        configuration["session_configuration"]["sandbox"],
        turn_policy()
    );
    drop(connection.take());
    old_done.send(()).unwrap();
    if explicit_resume {
        execute(
            root.path(),
            &mut connection,
            &request("thread/resume", json!({"threadId":ID})),
        )
        .unwrap();
    }
    execute(
        root.path(),
        &mut connection,
        &request(
            "turn/start",
            json!({"threadId":ID,"input":"new-notification"}),
        ),
    )
    .unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while util::read_state_json(root.path(), "broker-tools.json").is_err() {
        assert!(
            std::time::Instant::now() < deadline,
            "subscribed connection did not receive its tool"
        );
        observe(root.path(), connection.as_mut().unwrap()).unwrap();
    }
    let journal = util::read_state_json(root.path(), "broker-tools.json").unwrap();
    assert_eq!(journal["77"]["request"], tool);
    util::atomic_write_json(
        &root.path().join("control-tools.json"),
        &json!({"77":{
        "phase":"complete","request":tool,"result":response}}),
    )
    .unwrap();
    for _ in 0..2 {
        execute(
            root.path(),
            &mut connection,
            &request("tool/respond", json!({"id":77,"result":response})),
        )
        .unwrap();
    }
    new_done.send(()).unwrap();
    server.finish();
    let methods = methods.lock().unwrap();
    let names: Vec<_> = methods.iter().map(|r| r["method"].clone()).collect();
    assert_eq!(
        names,
        json!([
            "thread/start",
            "turn/start",
            "thread/resume",
            "turn/start",
            null
        ])
        .as_array()
        .unwrap()
        .clone()
    );
    assert_eq!(
        util::read_state_json(root.path(), "broker-tools.json").unwrap()["77"]["phase"],
        "responded"
    );
}

#[test]
fn legacy_metadata_reads_do_not_grant_unverified_subscription_for_business() {
    let root = common::tempdir().unwrap();
    let (done, released) = mpsc::channel();
    let server = rpc_fixture::Server::start(move |mut peer| {
        peer.initialize();
        let read = peer.request();
        assert_eq!(read["method"], "thread/read");
        peer.json(&json!({"id":read["id"],"result":result()}));
        released.recv_timeout(Duration::from_secs(5)).unwrap();
    });
    runtime(root.path(), &server.path);
    let runtime = util::read_state_json(root.path(), "runtime.json").unwrap();
    util::atomic_write_json(
        &root.path().join("upstream-session.json"),
        &json!({
        "phase":"created","instance":"subscription-fixture","runtime":runtime,"session":ID}),
    )
    .unwrap();
    let mut connection = None;
    execute(
        root.path(),
        &mut connection,
        &request("thread/read", json!({"threadId":ID})),
    )
    .unwrap();
    for (field, value) in [
        ("model", json!("foreign-model")),
        ("effort", json!("low")),
        ("cwd", json!("/foreign")),
        ("collaborationMode", json!({"mode":"plan"})),
    ] {
        let mut params = json!({"threadId":ID});
        params[field] = value;
        assert!(execute(root.path(), &mut connection, &request("turn/start", params)).is_err());
    }
    let error = execute(
        root.path(),
        &mut connection,
        &request("turn/start", json!({"threadId":ID})),
    )
    .unwrap_err();
    assert!(error.message.contains("configuration is unavailable"));
    assert!(connection.as_ref().unwrap().subscribed_session.is_none());
    done.send(()).unwrap();
    server.finish();
}

#[test]
fn reconnect_configuration_mismatch_blocks_the_business_request() {
    for (field, value) in [
        ("/thread/id", json!("foreign-session")),
        ("/thread/cwd", json!("/other")),
        ("/model", json!("other")),
        ("/modelProvider", json!("other")),
        ("/reasoningEffort", json!("low")),
        ("/approvalPolicy", json!("never")),
        ("/sandbox/networkAccess", json!(true)),
        ("/sandbox/writableRoots", json!(["/other"])),
        ("/sandbox/excludeTmpdirEnvVar", json!(false)),
        ("/sandbox/excludeSlashTmp", json!(false)),
        ("/multiAgentMode", json!("other")),
        ("/disabledPluginIds", json!(["changed"])),
        ("/collaborationMode/mode", json!("plan")),
    ] {
        let root = common::tempdir().unwrap();
        let mut changed = resumed_result();
        *changed.pointer_mut(field).unwrap() = value;
        let (done, released) = mpsc::channel();
        let server = rpc_fixture::Server::start(move |mut peer| {
            peer.initialize();
            let resume = peer.request();
            assert_eq!(resume["method"], "thread/resume");
            peer.json(&json!({"id":resume["id"],"result":changed}));
            // No turn/start reaches upstream after any identity/policy mismatch.
            released.recv_timeout(Duration::from_secs(5)).unwrap();
        });
        runtime(root.path(), &server.path);
        let runtime = util::read_state_json(root.path(), "runtime.json").unwrap();
        let baseline =
            crate::task_runtime::upstream::subscription::configuration(&resumed_result());
        util::atomic_write_json(&root.path().join("upstream-session.json"),&json!({"phase":"created",
            "instance":"subscription-fixture","runtime":runtime,"session":ID,"session_configuration":baseline})).unwrap();
        let mut connection = None;
        assert!(execute(
            root.path(),
            &mut connection,
            &request("turn/start", json!({"threadId":ID}))
        )
        .is_err());
        assert!(connection.as_ref().unwrap().subscribed_session.is_none());
        done.send(()).unwrap();
        server.finish();
    }
}

#[test]
fn nullable_optional_configuration_fields_normalize_to_omitted() {
    let baseline = result();
    let mut with_nulls = baseline.clone();
    with_nulls["activePermissionProfile"] = J::Null;
    assert_eq!(
        crate::task_runtime::upstream::subscription::configuration(&baseline),
        crate::task_runtime::upstream::subscription::configuration(&with_nulls)
    );
}
