//! Recovery fixtures keep their persistent connections until actual shutdown.
use super::{common, rpc_server};
use serde_json::{json, Value};
use std::{
    fs,
    os::{
        fd::AsRawFd,
        unix::{fs::PermissionsExt, net::UnixListener},
    },
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

const BUDGET: Duration = Duration::from_secs(90);

pub struct Server {
    pub path: PathBuf,
    stop: Arc<AtomicBool>,
    audit: Arc<Mutex<Vec<Value>>>,
    thread: Option<JoinHandle<()>>,
    _directory: tempfile::TempDir,
}

impl Server {
    pub fn start(
        maximum: usize,
        window: Duration,
        run: impl Fn(rpc_server::Peer, Connection) + Send + Sync + 'static,
    ) -> Self {
        let directory = common::tempdir().unwrap();
        let path = directory.path().join("rpc.sock");
        let listener = UnixListener::bind(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let audit = Arc::new(Mutex::new(Vec::new()));
        let (stopped, records, run) = (stop.clone(), audit.clone(), Arc::new(run));
        let thread = thread::spawn(move || {
            let deadline = Instant::now() + BUDGET;
            let mut peers = Vec::new();
            // Join every owned peer even if accepting or a handler fails.
            let accepted = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                while !stopped.load(Ordering::SeqCst) {
                    assert!(
                        Instant::now() < deadline,
                        "recovery fixture exceeded total budget"
                    );
                    match listener.accept() {
                        Ok((stream, _)) => {
                            assert!(peers.len() < maximum, "recovery connection bound exceeded");
                            stream
                                .set_read_timeout(Some(Duration::from_secs(3)))
                                .unwrap();
                            stream
                                .set_write_timeout(Some(Duration::from_secs(3)))
                                .unwrap();
                            let connection =
                                Connection::new(peers.len(), &stopped, &records, deadline, window);
                            let run = run.clone();
                            peers.push(thread::spawn(move || {
                                run(rpc_server::Peer { stream }, connection)
                            }));
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(2))
                        }
                        Err(e) => panic!("recovery accept: {e}"),
                    }
                }
            }));
            stopped.store(true, Ordering::SeqCst);
            let mut failed = None;
            for peer in peers {
                if let Err(error) = peer.join() {
                    failed.get_or_insert(error);
                }
            }
            if let Err(error) = accepted {
                std::panic::resume_unwind(error);
            }
            if let Some(error) = failed {
                std::panic::resume_unwind(error);
            }
        });
        Self {
            path,
            stop,
            audit,
            thread: Some(thread),
            _directory: directory,
        }
    }

    pub fn snapshot(&self) -> Value {
        json!(self.audit.lock().unwrap().clone())
    }

    pub fn finish(mut self) -> Value {
        self.shutdown();
        self.snapshot()
    }

    fn shutdown(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let joined = thread.join();
            if !thread::panicking() {
                joined.unwrap();
            }
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.shutdown();
    }
}

pub struct Connection {
    id: usize,
    stop: Arc<AtomicBool>,
    audit: Arc<Mutex<Vec<Value>>>,
    deadline: Instant,
    window: Duration,
    started: Instant,
    last_request: Instant,
    record: Value,
}

impl Connection {
    fn new(
        id: usize,
        stop: &Arc<AtomicBool>,
        audit: &Arc<Mutex<Vec<Value>>>,
        deadline: Instant,
        window: Duration,
    ) -> Self {
        let record =
            json!({"connection":id,"request_count":0,"idle_observations":0,"close_reason":null});
        audit.lock().unwrap().push(record.clone());
        Self {
            id,
            stop: stop.clone(),
            audit: audit.clone(),
            deadline,
            window,
            started: Instant::now(),
            last_request: Instant::now(),
            record,
        }
    }

    pub fn next(&mut self, peer: &mut rpc_server::Peer) -> Option<Value> {
        loop {
            if self.stop.load(Ordering::SeqCst) {
                return self.close("explicit_shutdown");
            }
            let remaining = self.deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return self.close("total_budget");
            }
            let mut fd = libc::pollfd {
                fd: peer.stream.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            let timeout = self.window.min(remaining).as_millis().max(1) as i32;
            // SAFETY: poll borrows one initialized descriptor for a bounded call.
            let result = unsafe { libc::poll(&mut fd, 1, timeout) };
            if result == 0 {
                self.record["idle_observations"] =
                    json!(self.record["idle_observations"].as_u64().unwrap() + 1);
                // A quiet persistent broker is healthy. This window bounds
                // observation latency, not the lifetime of its upstream peer.
                self.publish();
                continue;
            }
            if result < 0 {
                if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                return self.close("poll_error");
            }
            return self.read(peer);
        }
    }

    fn read(&mut self, peer: &mut rpc_server::Peer) -> Option<Value> {
        let mut byte = [0u8];
        // SAFETY: the peer owns its descriptor and byte is writable.
        let read = unsafe {
            libc::recv(
                peer.stream.as_raw_fd(),
                byte.as_mut_ptr().cast(),
                1,
                libc::MSG_PEEK,
            )
        };
        if read == 0 {
            return self.close("eof");
        }
        if read < 0 {
            return self.close("peek_error");
        }
        let (opcode, payload) = peer.frame();
        if opcode == 8 {
            return self.close("peer_close");
        }
        assert_eq!(opcode, 1);
        let message: Value = serde_json::from_slice(&payload).unwrap();
        self.last_request = Instant::now();
        self.record["last_method"] = message["method"].clone();
        self.record["request_count"] = json!(self.record["request_count"].as_u64().unwrap() + 1);
        self.publish();
        Some(message)
    }

    fn close(&mut self, reason: &str) -> Option<Value> {
        self.record["close_reason"] = json!(reason);
        self.publish();
        None
    }

    fn publish(&mut self) {
        self.record["elapsed_ms"] = json!(self.started.elapsed().as_millis() as u64);
        self.record["last_request_age_ms"] = json!(self.last_request.elapsed().as_millis() as u64);
        self.audit.lock().unwrap()[self.id] = self.record.clone();
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        if self.record["close_reason"].is_null() {
            self.close(if thread::panicking() {
                "handler_panic"
            } else {
                "handler_return"
            });
        }
    }
}
