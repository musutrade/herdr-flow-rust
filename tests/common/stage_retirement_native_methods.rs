//! Actual Policy/ProxyBridge regression for the startup mix observed in 0.165.0.
use super::*;
use crate::task_runtime::tui::method_fixtures as cases;

#[test]
fn native_165_startup_mix_and_terminal_command_result_allow_stage_close() {
    let active = Arc::new(AtomicBool::new(false));
    let cwd = Arc::new(Mutex::new(J::Null));
    let (service, incoming) = native_service(active, cwd.clone());
    let mut fixture = Fixture::new(&service.path);
    *cwd.lock().unwrap() = json!(fixture.clone_path());
    let herdr = PresentationServer::new(&fixture.temp.path().join("herdr.sock"), None);
    let mut frontend = Frontend::new();
    install_presentation(&fixture, &herdr, &frontend);
    let mut peer = NativePeer::new(&fixture, incoming);
    install_presentation(&fixture, &herdr, &frontend);
    for (id, case) in cases::metadata(WORKER).into_iter().enumerate() {
        let request = json!({"id":id,"method":case.method,"params":case.params});
        peer.client(&request, true);
        assert_eq!(peer.upstream.request(), request);
        let response = json!({"id":id,"result":case.result});
        peer.server(&response);
        assert_eq!(peer.frontend_message(), response);
    }
    let command = cases::command();
    let request = json!({"id":"exec","method":command.method,"params":command.params});
    peer.client(&request, true);
    assert_eq!(peer.upstream.request(), request);
    fixture.pending_tick();
    assert!(fixture.root().join(MARKER).exists());
    let delta = json!({"method":"command/exec/outputDelta","params":{"processId":"catalog-command","stream":"stdout","deltaBase64":"b2sK"}});
    peer.server(&delta);
    assert_eq!(peer.frontend_message(), delta);
    assert!(drive_owner(&fixture.owner).is_err());
    assert_pane_open(&herdr);
    let response = json!({"id":"exec","result":command.result});
    peer.server(&response);
    assert_eq!(peer.frontend_message(), response);
    assert!(drive_owner(&fixture.owner).is_err());
    assert_eq!(
        herdr
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter(|call| call["method"] == "pane.close")
            .count(),
        1
    );
    assert!(peer.bridge.is_connected());
    assert!(frontend.0.try_wait().unwrap().is_none());
    assert!(fixture.broker.try_wait().unwrap().is_none());
    assert!(fixture.clone_path().exists());
}

#[test]
fn empty_command_ack_keeps_original_native_channel_after_retirement_fence() {
    let active = Arc::new(AtomicBool::new(false));
    let cwd = Arc::new(Mutex::new(J::Null));
    let (service, incoming) = native_service(active, cwd.clone());
    let mut fixture = Fixture::new(&service.path);
    *cwd.lock().unwrap() = json!(fixture.clone_path());
    let herdr = PresentationServer::new(&fixture.temp.path().join("herdr.sock"), None);
    let mut frontend = Frontend::new();
    install_presentation(&fixture, &herdr, &frontend);
    let mut peer = NativePeer::new(&fixture, incoming);
    install_presentation(&fixture, &herdr, &frontend);
    peer.client(
        &json!({"id":7,"method":"command/exec","params":cases::command().params}),
        true,
    );
    assert_eq!(peer.upstream.request()["id"], 7);
    peer.server(&json!({"id":7,"result":{}}));
    assert_eq!(peer.frontend_message()["result"], json!({}));
    fixture.pending_tick();
    assert!(drive_owner(&fixture.owner).is_err());
    assert_pane_open(&herdr);
    peer.client(
        &json!({"id":8,"method":"command/exec","params":cases::command().params}),
        true,
    );
    assert!(peer.upstream_empty());
    peer.bridge.test_transfer(0, false, true).unwrap();
    assert!(peer.frontend_message()["error"].is_object());
    assert!(peer.bridge.is_connected());
    assert!(frontend.0.try_wait().unwrap().is_none());
    assert!(fixture.broker.try_wait().unwrap().is_none());
}
