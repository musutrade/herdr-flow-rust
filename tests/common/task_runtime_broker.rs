use super::*;
#[path = "mod.rs"]
mod common;
#[path = "rpc_server.rs"]
mod rpc_fixture;
#[test]
fn persistent_connection_records_exact_turns_and_never_answers_approval() {
    let server = rpc_fixture::Server::start(|mut peer| {
        peer.initialize();
        let request = peer.request();
        assert_eq!(request["method"], "thread/read");
        peer.json(&json!({"id":request["id"],"result":{"thread":{"id":"s"}}}));
        peer.json(&json!({"method":"turn/completed","params":{"threadId":"s","turn":{"id":"t","status":"completed"}}}));
        peer.json(&json!({"id":91,"method":"item/commandExecution/requestApproval","params":{"threadId":"s","turnId":"t"}}));
        std::thread::sleep(Duration::from_millis(100));
    });
    let dir = common::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    common::create_dir(root.join("unused")).unwrap();
    common::create_dir_all(root.join("unused/nested")).unwrap();
    std::fs::hard_link(&server.path, root.join("server.sock")).unwrap();
    std::env::set_var(crate::task_env::MARKER_ENV, "broker-fixture");
    util::atomic_write_json(
        &root.join("runtime.json"),
        &json!({"instance":"broker-fixture"}),
    )
    .unwrap();
    let task_root = root.clone();
    let task = std::thread::spawn(move || serve(&task_root));
    for _ in 0..100 {
        if root.join("broker.sock").exists() {
            break;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    let result = call(&root, "thread/read", &json!({"threadId":"s"})).unwrap();
    assert_eq!(result["thread"]["id"], "s");
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
