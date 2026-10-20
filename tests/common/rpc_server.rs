//! Independent, bounded WebSocket fixture for the production RPC client.
use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use sha1::{Digest, Sha1};

pub struct Server {
    pub path: PathBuf,
    task: JoinHandle<()>,
    _directory: tempfile::TempDir,
}

impl Server {
    pub fn start(run: impl FnOnce(Peer) + Send + 'static) -> Self {
        let directory = super::common::tempdir().unwrap();
        let path = directory.path().join("rpc.sock");
        let listener = UnixListener::bind(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let task = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(5);
            let stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(Instant::now() < deadline, "client did not connect");
                        thread::sleep(Duration::from_millis(2));
                    }
                    Err(e) => panic!("accept: {e}"),
                }
            };
            stream
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            stream
                .set_write_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            run(Peer { stream });
        });
        Self {
            path,
            task,
            _directory: directory,
        }
    }

    pub fn finish(self) {
        self.task.join().unwrap();
    }
}

pub struct Peer {
    pub stream: UnixStream,
}

fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut result = String::new();
    for chunk in bytes.chunks(3) {
        let mut word = (chunk[0] as usize) << 16;
        if chunk.len() > 1 {
            word |= (chunk[1] as usize) << 8;
        }
        if chunk.len() > 2 {
            word |= chunk[2] as usize;
        }
        result.push(ALPHABET[(word >> 18) & 63] as char);
        result.push(ALPHABET[(word >> 12) & 63] as char);
        result.push(if chunk.len() > 1 {
            ALPHABET[(word >> 6) & 63] as char
        } else {
            '='
        });
        result.push(if chunk.len() > 2 {
            ALPHABET[word & 63] as char
        } else {
            '='
        });
    }
    result
}

impl Peer {
    pub fn headers(&mut self) -> String {
        let mut bytes = Vec::new();
        while !bytes.ends_with(b"\r\n\r\n") {
            let mut byte = [0];
            self.stream.read_exact(&mut byte).unwrap();
            bytes.push(byte[0]);
            assert!(bytes.len() <= 16 * 1024);
        }
        String::from_utf8(bytes).unwrap()
    }

    pub fn upgrade(&mut self) {
        let headers = self.headers();
        assert!(headers.starts_with("GET "));
        let key = headers
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("sec-websocket-key")
                    .then(|| value.trim())
            })
            .unwrap();
        let accept = base64(&Sha1::digest(format!(
            "{key}258EAFA5-E914-47DA-95CA-C5AB0DC85B11"
        )));
        let response = format!("HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n\r\n");
        self.stream.write_all(response.as_bytes()).unwrap();
    }

    pub fn initialize(&mut self) {
        self.upgrade();
        let request = self.request();
        assert_eq!(request["method"], "initialize");
        assert!(request["params"].is_object());
        self.json(
            &json!({"id":request["id"],"result":{"serverInfo":{"name":"fixture","version":"1"}}}),
        );
        assert_eq!(self.request()["method"], "initialized");
    }

    pub fn request(&mut self) -> Value {
        let (opcode, payload) = self.frame();
        assert_eq!(opcode, 1);
        serde_json::from_slice(&payload).unwrap()
    }

    pub fn frame(&mut self) -> (u8, Vec<u8>) {
        let mut head = [0; 2];
        self.stream.read_exact(&mut head).unwrap();
        assert_eq!(head[0] & 0x80, 0x80, "client frame must be final");
        assert_eq!(head[1] & 0x80, 0x80, "client frame must be masked");
        let mut length = u64::from(head[1] & 127);
        if length == 126 {
            let mut size = [0; 2];
            self.stream.read_exact(&mut size).unwrap();
            length = u64::from(u16::from_be_bytes(size));
        } else if length == 127 {
            let mut size = [0; 8];
            self.stream.read_exact(&mut size).unwrap();
            length = u64::from_be_bytes(size);
        }
        assert!(length <= 16 * 1024 * 1024);
        let mut mask = [0; 4];
        self.stream.read_exact(&mut mask).unwrap();
        let mut payload = vec![0; length as usize];
        self.stream.read_exact(&mut payload).unwrap();
        for (i, byte) in payload.iter_mut().enumerate() {
            *byte ^= mask[i % 4];
        }
        (head[0] & 15, payload)
    }

    pub fn send(&mut self, opcode: u8, payload: &[u8]) {
        let mut bytes = vec![0x80 | opcode];
        if payload.len() < 126 {
            bytes.push(payload.len() as u8);
        } else {
            bytes.push(126);
            bytes.extend_from_slice(&(payload.len() as u16).to_be_bytes());
        }
        bytes.extend_from_slice(payload);
        self.stream.write_all(&bytes).unwrap();
    }

    pub fn json(&mut self, value: &Value) {
        self.send(1, &serde_json::to_vec(value).unwrap());
    }
}
