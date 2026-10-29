//! Real task ProxyBridge and Unix sockets; frontend/upstream/Herdr are local
//! protocol peers. No installed frontend, model, or external service is used.
use super::*;
use crate::codex::rpc::{encode_frame, FrameDecoder, MAX_MESSAGE};
use crate::codex::tui::ProxyBridge;
use std::io::Read;
use std::os::unix::net::UnixStream;
use std::sync::mpsc;
use std::time::Instant;

struct NativeServer {
    path: PathBuf,
    stopped: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
    _directory: tempfile::TempDir,
}

impl Drop for NativeServer {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::SeqCst);
        self.thread.take().unwrap().join().unwrap();
    }
}

struct NativePeer {
    bridge: ProxyBridge,
    frontend: UnixStream,
    upstream: rpc_server::Peer,
    client_frames: FrameDecoder,
}

impl NativePeer {
    fn new(fixture: &Fixture, incoming: mpsc::Receiver<rpc_server::Peer>) -> Self {
        let record = read(&fixture.root(), "presentation.json").unwrap();
        let runtime = read(&fixture.root(), "runtime.json").unwrap();
        let mut bridge = tui::test_bridge(&record, &runtime).unwrap();
        bridge.prepare().unwrap();
        let frontend = UnixStream::connect(bridge.proxy_socket()).unwrap();
        frontend
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        bridge.accept().unwrap();
        let upstream = incoming.recv_timeout(Duration::from_secs(2)).unwrap();
        let mut peer = Self {
            bridge,
            frontend,
            upstream,
            client_frames: FrameDecoder::new(false, MAX_MESSAGE),
        };
        peer.handshake();
        peer
    }

    fn handshake(&mut self) {
        self.frontend.write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n").unwrap();
        self.bridge.test_transfer(0, true, false).unwrap();
        self.bridge.test_transfer(1, false, true).unwrap();
        self.upstream.upgrade();
        self.bridge.test_transfer(1, true, false).unwrap();
        self.bridge.test_transfer(0, false, true).unwrap();
        let mut headers = vec![];
        while !headers.ends_with(b"\r\n\r\n") {
            let mut byte = [0];
            self.frontend.read_exact(&mut byte).unwrap();
            headers.push(byte[0]);
        }
        assert!(headers.starts_with(b"HTTP/1.1 101"));
    }

    fn client(&mut self, value: &J, flush: bool) {
        let frame = encode_frame(&serde_json::to_vec(value).unwrap(), 1, true, true).unwrap();
        self.frontend.write_all(&frame).unwrap();
        self.bridge.test_transfer(0, true, false).unwrap();
        if flush {
            self.bridge.test_transfer(1, false, true).unwrap();
        }
    }

    fn server(&mut self, value: &J) {
        self.upstream.json(value);
        self.bridge.test_transfer(1, true, false).unwrap();
        self.bridge.test_transfer(0, false, true).unwrap();
    }

    fn frontend_message(&mut self) -> J {
        let mut bytes = [0; 4096];
        loop {
            let count = self.frontend.read(&mut bytes).unwrap();
            assert!(count > 0, "existing native socket was closed");
            let frames = self.client_frames.feed(&bytes[..count]).unwrap();
            if let Some((opcode, payload)) = frames.into_iter().next() {
                assert_eq!(opcode, 1);
                return serde_json::from_slice(&payload).unwrap();
            }
        }
    }

    fn upstream_empty(&mut self) -> bool {
        self.upstream.stream.set_nonblocking(true).unwrap();
        let result = self.upstream.stream.read(&mut [0; 1]);
        self.upstream.stream.set_nonblocking(false).unwrap();
        matches!(result, Err(e) if e.kind() == std::io::ErrorKind::WouldBlock)
    }
}

fn native_service(
    active: Arc<AtomicBool>,
    cwd: Arc<Mutex<J>>,
) -> (NativeServer, mpsc::Receiver<rpc_server::Peer>) {
    let directory = util::fs::test_tempdir();
    let path = directory.path().join("native.sock");
    let listener = UnixListener::bind(&path).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    listener.set_nonblocking(true).unwrap();
    let (sender, receiver) = mpsc::channel();
    let mut handoff = Some(sender);
    let stopped = Arc::new(AtomicBool::new(false));
    let stop = stopped.clone();
    let thread = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(20);
        while !stop.load(Ordering::SeqCst) {
            assert!(
                Instant::now() < deadline,
                "native protocol fixture exceeded deadline"
            );
            let Some(mut peer) = accept_peer(&listener) else {
                continue;
            };
            if let Some(sender) = handoff.take() {
                sender.send(peer).unwrap();
                continue;
            }
            peer.initialize();
            let request = peer.request();
            assert_eq!(request["method"], "thread/read");
            peer.json(&json!({"id":request["id"],"result":{"thread":{
                "id":WORKER,"cwd":*cwd.lock().unwrap(),"status":{
                    "type":if active.load(Ordering::SeqCst) {"active"} else {"idle"},
                    "activeFlags":[]}}}}));
        }
    });
    (
        NativeServer {
            path,
            stopped,
            thread: Some(thread),
            _directory: directory,
        },
        receiver,
    )
}

fn accept_peer(listener: &UnixListener) -> Option<rpc_server::Peer> {
    match listener.accept() {
        Ok((stream, _)) => {
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            stream
                .set_write_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            Some(rpc_server::Peer { stream })
        }
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
            std::thread::sleep(Duration::from_millis(1));
            None
        }
        Err(error) => panic!("native fixture accept: {error}"),
    }
}

#[test]
fn existing_native_socket_refuses_post_marker_turn_without_disconnect() {
    let active = Arc::new(AtomicBool::new(false));
    let cwd = Arc::new(Mutex::new(J::Null));
    let (service, incoming) = native_service(active, cwd.clone());
    let fixture = Fixture::new(&service.path);
    *cwd.lock().unwrap() = json!(fixture.clone_path());
    let herdr = PresentationServer::new(&fixture.temp.path().join("herdr.sock"), None);
    let frontend = Frontend::new();
    install_presentation(&fixture, &herdr, &frontend);
    let mut peer = NativePeer::new(&fixture, incoming);
    install_presentation(&fixture, &herdr, &frontend);
    fixture.pending_tick();
    assert!(fixture.root().join(MARKER).exists());
    peer.client(
        &json!({"id":"new-turn","method":"turn/start","params":{"threadId":WORKER}}),
        true,
    );
    assert!(
        peer.upstream_empty(),
        "post-marker turn/start reached the actual upstream socket"
    );
    peer.bridge.test_transfer(0, false, true).unwrap();
    let refusal = peer.frontend_message();
    assert_eq!(refusal["id"], "new-turn");
    assert!(refusal["error"].is_object());
    assert!(peer.bridge.is_connected());
    peer.client(&json!({"id":"interrupt","method":"turn/interrupt","params":{"threadId":WORKER,"turnId":"old-turn"}}), true);
    assert_eq!(peer.upstream.request()["id"], "interrupt");
}

#[test]
fn active_approval_or_input_after_marker_keeps_the_original_pane() {
    for method in [
        "item/commandExecution/requestApproval",
        "item/tool/requestUserInput",
    ] {
        let active = Arc::new(AtomicBool::new(false));
        let cwd = Arc::new(Mutex::new(J::Null));
        let (service, incoming) = native_service(active.clone(), cwd.clone());
        let mut fixture = Fixture::new(&service.path);
        *cwd.lock().unwrap() = json!(fixture.clone_path());
        let herdr = PresentationServer::new(&fixture.temp.path().join("herdr.sock"), None);
        let mut frontend = Frontend::new();
        install_presentation(&fixture, &herdr, &frontend);
        let mut peer = NativePeer::new(&fixture, incoming);
        install_presentation(&fixture, &herdr, &frontend);
        fixture.pending_tick();
        active.store(true, Ordering::SeqCst);
        peer.server(
            &json!({"id":71,"method":method,"params":{"threadId":WORKER,"turnId":"needs-human"}}),
        );
        assert_eq!(peer.frontend_message()["id"], 71);
        assert!(drive_owner(&fixture.owner).is_err());
        assert!(
            herdr
                .calls
                .lock()
                .unwrap()
                .iter()
                .all(|call| call["method"] != "pane.close"),
            "cleanup closed the active approval/input UI before checking native work"
        );
        assert!(frontend.0.try_wait().unwrap().is_none());
        assert!(fixture.broker.try_wait().unwrap().is_none());
        assert!(fixture.clone_path().is_dir());
        peer.client(&json!({"id":71,"result":{"decision":"accept"}}), true);
        assert_eq!(peer.upstream.request()["id"], 71);
        assert!(peer.bridge.is_connected());
        let completed = json!({"method":"turn/completed","params":{"threadId":WORKER,
            "turn":{"id":"needs-human","status":"completed"}}});
        peer.server(&completed);
        assert_eq!(peer.frontend_message(), completed);
        // Even after the admitted exchange drains, a fresh active thread
        // observation independently forbids closing the pane.
        assert!(drive_owner(&fixture.owner).is_err());
        assert_pane_open(&herdr);
        active.store(false, Ordering::SeqCst);
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
    }
}

fn assert_pane_open(herdr: &PresentationServer) {
    assert!(herdr
        .calls
        .lock()
        .unwrap()
        .iter()
        .all(|call| call["method"] != "pane.close"));
}

#[test]
fn admitted_buffered_turn_and_human_replies_drain_on_the_original_socket() {
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
    let request = json!({"id":71,"method":"turn/start","params":{"threadId":WORKER,
        "input":[{"type":"text","text":"already admitted"}]}});
    peer.client(&request, false);
    assert!(
        peer.upstream_empty(),
        "test must hold admitted bytes in bridge buffer"
    );
    let waiting = drive_owner(&fixture.owner).unwrap_err();
    assert_eq!(waiting.kind, crate::ErrorKind::StateBusy);
    assert!(
        !fixture.root().join(MARKER).exists(),
        "Owner EX must not pass bytes admitted but not yet written to upstream"
    );
    assert_pane_open(&herdr);
    peer.bridge.test_transfer(1, false, true).unwrap();
    assert_eq!(peer.upstream.request(), request);
    fixture.pending_tick();
    assert!(fixture.root().join(MARKER).exists());
    peer.server(&json!({"id":71,"result":{"turn":{"id":"buffered-turn","status":"inProgress"}}}));
    assert_eq!(peer.frontend_message()["id"], 71);
    for (id, method) in [
        (json!(71), "item/commandExecution/requestApproval"),
        (json!("71"), "item/tool/requestUserInput"),
    ] {
        peer.server(
            &json!({"id":id,"method":method,"params":{"threadId":WORKER,"turnId":"buffered-turn"}}),
        );
        assert_eq!(peer.frontend_message()["id"], id);
        let reply = json!({"id":id,"result":{"decision":"accept","answers":{}}});
        let exclusive = lifecycle::acquire(&fixture.owner, true).unwrap();
        peer.client(&reply, true);
        assert!(
            peer.upstream_empty(),
            "human reply must remain buffered while source Owner EX is held"
        );
        drop(exclusive);
        peer.bridge.test_iteration().unwrap();
        assert_eq!(peer.upstream.request(), reply);
    }
    assert!(drive_owner(&fixture.owner).is_err());
    assert_pane_open(&herdr);
    let foreign = json!({"method":"turn/completed","params":{"threadId":WORKER,
        "turn":{"id":"another-turn","status":"completed"}}});
    peer.server(&foreign);
    assert_eq!(peer.frontend_message(), foreign);
    assert!(drive_owner(&fixture.owner).is_err());
    assert_pane_open(&herdr);
    let completed = json!({"method":"turn/completed","params":{"threadId":WORKER,
        "turn":{"id":"buffered-turn","status":"completed"}}});
    let exclusive = lifecycle::acquire(&fixture.owner, true).unwrap();
    peer.server(&completed);
    drop(exclusive);
    peer.bridge.test_iteration().unwrap();
    assert_eq!(peer.frontend_message(), completed);
    assert!(peer.bridge.is_connected());
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
    assert!(frontend.0.try_wait().unwrap().is_none());
    assert!(fixture.broker.try_wait().unwrap().is_none());
}

#[test]
fn discarded_native_buffer_releases_ex_without_erasing_unknown_requests() {
    let active = Arc::new(AtomicBool::new(false));
    let cwd = Arc::new(Mutex::new(J::Null));
    let (service, incoming) = native_service(active, cwd.clone());
    let fixture = Fixture::new(&service.path);
    *cwd.lock().unwrap() = json!(fixture.clone_path());
    let herdr = PresentationServer::new(&fixture.temp.path().join("herdr.sock"), None);
    let frontend = Frontend::new();
    install_presentation(&fixture, &herdr, &frontend);
    let mut peer = NativePeer::new(&fixture, incoming);
    install_presentation(&fixture, &herdr, &frontend);
    peer.client(
        &json!({"id":"unknown-buffer","method":"turn/start","params":{"threadId":WORKER}}),
        false,
    );
    assert!(lifecycle::acquire(&fixture.owner, true).is_err());
    peer.bridge.cleanup().unwrap();
    let exclusive = lifecycle::acquire(&fixture.owner, true).unwrap();
    let presentation = read(&fixture.root(), "presentation.json").unwrap();
    assert!(tui::require_retirement_drained(&presentation).is_err());
    assert_pane_open(&herdr);
    drop(exclusive);
}

#[test]
fn async_acknowledgement_keeps_queued_native_work_and_original_pane() {
    for method in [
        "thread/compact/start",
        "thread/shellCommand",
        "thread/backgroundTerminals/clean",
        "thread/futureAsyncOperation/start",
    ] {
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
        let request = json!({"id":"queued-operation","method":method,
            "params":{"threadId":WORKER}});
        peer.client(&request, true);
        assert_eq!(peer.upstream.request(), request);
        // These are protocol ACKs, with no proof that enqueued native work
        // ended. The separate production idle probe deliberately stays idle.
        let acknowledged = json!({"id":"queued-operation","result":{}});
        peer.server(&acknowledged);
        assert_eq!(peer.frontend_message(), acknowledged);
        fixture.pending_tick();
        assert!(fixture.root().join(MARKER).exists());
        assert!(drive_owner(&fixture.owner).is_err());
        assert!(
            herdr
                .calls
                .lock()
                .unwrap()
                .iter()
                .all(|call| call["method"] != "pane.close"),
            "{method} ACK discharged unknown queued work and closed the original pane"
        );
        assert!(peer.bridge.is_connected());
        assert!(frontend.0.try_wait().unwrap().is_none());
        assert!(fixture.broker.try_wait().unwrap().is_none());
        assert!(fixture.clone_path().join("evidence").is_file());
        assert_eq!(fixture.plan().entries[0].phase, "retiring");
    }
}

#[path = "stage_retirement_native_methods.rs"]
mod method_completion;
