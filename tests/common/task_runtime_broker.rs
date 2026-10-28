use super::*;
#[path = "mod.rs"]
mod common;
#[path = "rpc_server.rs"]
pub(crate) mod rpc_fixture;
#[path = "broker_subscription.rs"]
mod subscription;
#[test]
fn persistent_connection_records_exact_turns_and_never_answers_approval() {
    let (release, received) = std::sync::mpsc::channel();
    let dir = common::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    common::create_dir(root.join("unused")).unwrap();
    common::create_dir_all(root.join("unused/nested")).unwrap();
    std::env::set_var(crate::task_env::MARKER_ENV, "broker-fixture");
    util::atomic_write_json(
        &root.join("runtime.json"),
        &json!({"instance":"broker-fixture"}),
    )
    .unwrap();
    let task_root = root.clone();
    let task = std::thread::spawn(move || serve(&task_root));
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        if util::read_json(&root.join("broker-status.json"))
            .is_ok_and(|status| status["phase"] == "running")
        {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "broker did not become ready"
        );
        std::thread::sleep(Duration::from_millis(2));
    }
    // Start the RPC fixture's connection deadline only after broker setup.
    let server = rpc_fixture::Server::start(move |mut peer| {
        peer.initialize();
        let request = peer.request();
        assert_eq!(request["method"], "thread/read");
        peer.json(&json!({"id":request["id"],"result":{"thread":{"id":"s"},"large":"x".repeat(2 * 1024 * 1024)}}));
        received.recv_timeout(Duration::from_secs(10)).unwrap();
        peer.json(&json!({"method":"turn/completed","params":{"threadId":"s","turn":{"id":"t","status":"completed"}}}));
        peer.json(&json!({"id":91,"method":"item/commandExecution/requestApproval","params":{"threadId":"s","turnId":"t"}}));
    });
    std::fs::hard_link(&server.path, root.join("server.sock")).unwrap();
    let observed = call_observed(&root, "thread/read", &json!({"threadId":"s"})).unwrap();
    assert_eq!(observed.server_version, "1");
    let result = observed.result;
    assert!(result.get("server_info").is_none());
    assert_eq!(result["thread"]["id"], "s");
    assert_eq!(result["large"].as_str().unwrap().len(), 2 * 1024 * 1024);
    release.send(()).unwrap();
    assert!(task.join().unwrap().is_err());
    server.finish();
    let events = util::read_json(&root.join("broker-events.json")).unwrap();
    assert_eq!(events["pending"]["91"]["id"], 91);
    assert_eq!(
        turn(&root, &json!({"session":"s"}), &json!("t"))
            .unwrap()
            .unwrap()["status"],
        "completed"
    );
    assert!(turn(&root, &json!({"session":"other"}), &json!("t")).is_err());
    assert!(turn(&root, &json!({"session":"s"}), &json!("absent"))
        .unwrap()
        .is_none());
    let mut connection = None;
    assert!(execute(
        &root,
        &mut connection,
        &json!({"method":"approve","params":{}})
    )
    .is_err());
}

#[test]
fn dynamic_tool_response_uses_original_connection_and_is_not_sent_twice() {
    let (release, released) = std::sync::mpsc::channel();
    let root = common::tempdir().unwrap();
    util::atomic_write_json(
        &root.path().join("runtime.json"),
        &json!({"instance":"tool-fixture"}),
    )
    .unwrap();
    let request = json!({"id":91,"method":"item/tool/call","params":{"threadId":"s","turnId":"t","tool":"herdr_queue"}});
    let native_request = request.clone();
    let result = json!({"success":true,"contentItems":[{"type":"inputText","text":"checked"}]});
    let native_result = result.clone();
    let server = rpc_fixture::Server::start(move |mut peer| {
        peer.initialize();
        let read = peer.request();
        peer.json(&native_request);
        peer.json(&json!({"id":read["id"],"result":{"thread":{"id":"s"}}}));
        assert_eq!(peer.request(), json!({"id":91,"result":native_result}));
        released.recv_timeout(Duration::from_secs(5)).unwrap();
    });
    std::fs::hard_link(&server.path, root.path().join("server.sock")).unwrap();
    let mut connection = None;
    execute(
        root.path(),
        &mut connection,
        &json!({"method":"thread/read","params":{"threadId":"s"}}),
    )
    .unwrap();
    let journal = util::read_state_json(root.path(), "broker-tools.json").unwrap();
    assert_eq!(journal["91"]["request"], request);
    util::atomic_write_json(
        &root.path().join("control-tools.json"),
        &json!({"91":{"phase":"complete","request":request,"result":result}}),
    )
    .unwrap();
    let response = json!({"method":"tool/respond","params":{"id":91,"result":result}});
    assert!(connection
        .as_mut()
        .unwrap()
        .respond_tool(&J::Null, &result)
        .is_err());
    for _ in 0..2 {
        assert_eq!(
            execute(root.path(), &mut connection, &response).unwrap()["responded"],
            true
        );
    }
    assert_eq!(
        util::read_state_json(root.path(), "broker-tools.json").unwrap()["91"]["phase"],
        "responded"
    );
    release.send(()).unwrap();
    server.finish();
}
