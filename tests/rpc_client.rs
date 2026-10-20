//! Production Client over real Unix sockets, without a model or remote service.
pub mod common;
#[path = "common/rpc_server.rs"]
mod server;

use std::io::Write;
use std::thread;
use std::time::{Duration, Instant};

use herdr_flow::codex::rpc::Client;
use herdr_flow::error::ErrorKind;
use serde_json::json;
use server::Server;

fn connection(server: &Server) -> Client {
    Client::with_limits(server.path.to_str().unwrap(), 1.0, 16 * 1024, 8)
}

#[test]
fn client_initializes_matches_responses_preserves_events_and_answers_ping() {
    let server = Server::start(|mut peer| {
        peer.initialize();
        let request = peer.request();
        assert_eq!(request["method"], "thread/read");
        assert_eq!(request["params"], json!({"threadId":"exact"}));
        peer.json(&json!({"method":"turn/completed","params":{"threadId":"other"}}));
        peer.json(&json!({"id":999,"result":"unrelated response"}));
        peer.send(9, b"probe");
        assert_eq!(peer.frame(), (10, b"probe".to_vec()));
        peer.json(&json!({"id":request["id"],"result":{"thread":{"id":"exact"}}}));
    });
    let mut client = connection(&server);
    assert_eq!(
        client
            .call("thread/read", &json!({"threadId":"exact"}))
            .unwrap(),
        json!({"thread":{"id":"exact"}})
    );
    assert_eq!(client.server_info.as_ref().unwrap()["name"], "fixture");
    let events = client.pop_events();
    assert_eq!(events.len(), 2);
    assert_eq!(events[0]["method"], "turn/completed");
    assert_eq!(events[1]["id"], 999);
    assert!(client.pop_events().is_empty());
    server.finish();
}

#[test]
fn client_reports_rpc_errors_without_resending() {
    let server = Server::start(|mut peer| {
        peer.initialize();
        let request = peer.request();
        peer.json(&json!({"id":request["id"],"error":{"code":-32001,"message":"busy"}}));
        let next = peer.request();
        assert_eq!(next["method"], "next");
        peer.json(&json!({"id":next["id"],"result":true}));
    });
    let mut client = connection(&server);
    let error = client.call("turn/start", &json!({})).unwrap_err();
    assert_eq!(error.kind, ErrorKind::Rpc);
    assert_eq!(error.rpc_code, Some(-32001));
    assert_eq!(error.method.as_deref(), Some("turn/start"));
    assert_eq!(client.call("next", &json!({})).unwrap(), true);
    server.finish();
}

#[test]
fn client_rejects_bad_handshake_and_invalid_json() {
    let server = Server::start(|mut peer| {
        peer.headers();
        peer.stream.write_all(b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: invalid\r\n\r\n").unwrap();
    });
    assert_eq!(
        connection(&server).connect().unwrap_err().kind,
        ErrorKind::RpcProtocol
    );
    server.finish();
    let server = Server::start(|mut peer| {
        peer.initialize();
        peer.request();
        peer.send(1, b"not JSON");
    });
    assert_eq!(
        connection(&server)
            .call("probe", &json!({}))
            .unwrap_err()
            .kind,
        ErrorKind::RpcProtocol
    );
    server.finish();
}

#[test]
fn client_bounds_timeouts_and_reports_disconnects_and_close_codes() {
    let server = Server::start(|mut peer| {
        peer.initialize();
        peer.request();
        thread::sleep(Duration::from_millis(150));
    });
    let mut client = connection(&server);
    client.connect().unwrap();
    let started = Instant::now();
    let error = client
        .call_with_timeout("probe", &json!({}), Some(0.03))
        .unwrap_err();
    assert_eq!(error.kind, ErrorKind::RpcTimeout);
    assert!(started.elapsed() < Duration::from_secs(1));
    server.finish();
    let server = Server::start(|mut peer| {
        peer.initialize();
        peer.request();
    });
    assert_eq!(
        connection(&server)
            .call("probe", &json!({}))
            .unwrap_err()
            .kind,
        ErrorKind::Rpc
    );
    server.finish();
    let server = Server::start(|mut peer| {
        peer.initialize();
        peer.request();
        peer.send(8, &1000u16.to_be_bytes());
        assert_eq!(peer.frame(), (8, 1000u16.to_be_bytes().to_vec()));
    });
    assert_eq!(
        connection(&server)
            .call("probe", &json!({}))
            .unwrap_err()
            .rpc_code,
        Some(1000)
    );
    server.finish();
}

#[test]
fn client_bounds_unsolicited_events_and_rejects_missing_endpoint() {
    let server = Server::start(|mut peer| {
        peer.initialize();
        peer.request();
        for i in 0..3 {
            peer.json(&json!({"method":"event","params":{"n":i}}));
        }
    });
    let mut client = Client::with_limits(server.path.to_str().unwrap(), 1.0, 16 * 1024, 2);
    assert_eq!(
        client.call("probe", &json!({})).unwrap_err().kind,
        ErrorKind::RpcProtocol
    );
    assert_eq!(client.events.len(), 2);
    server.finish();
    assert_eq!(
        Client::new("/nonexistent/herdr-fixture.sock")
            .connect()
            .unwrap_err()
            .kind,
        ErrorKind::Rpc
    );
}
