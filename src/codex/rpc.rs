//! Small synchronous Codex JSON-RPC client over a local UNIX WebSocket.
//!
//! No approval requests are answered by this client. Notifications and server
//! requests remain in `events` for the caller to inspect. A client owns one
//! connection and does not multiplex concurrent calls.

use std::collections::{BTreeMap, VecDeque};
use std::fs;
use std::io::{ErrorKind as IoErrorKind, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use sha1::{Digest, Sha1};

use crate::error::{ErrorKind, FlowError, Result};
use crate::logging;
use crate::util::pyjson::{random_bytes, to_hex};

mod frame;

pub use frame::{apply_mask, encode_frame, validate_close, FrameDecoder};

pub const MAX_MESSAGE: usize = 16 * 1024 * 1024;
pub const MAX_HEADERS: usize = 16 * 1024;
pub const MAX_JSON_DEPTH: usize = 10;
const WS_GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

pub fn rpc_error(message: impl Into<String>) -> FlowError {
    FlowError::rpc(message)
}

pub fn protocol_error(message: impl Into<String>) -> FlowError {
    FlowError::rpc_protocol(message)
}

fn timeout_error(message: impl Into<String>) -> FlowError {
    FlowError::rpc_timeout(message)
}

pub fn decode_message(payload: &[u8]) -> Result<Value> {
    if payload.len() > MAX_MESSAGE {
        return Err(protocol_error("message exceeds size limit"));
    }
    let text = std::str::from_utf8(payload)
        .map_err(|_| protocol_error("invalid JSON-RPC text message"))?;
    let message: Value =
        serde_json::from_str(text).map_err(|_| protocol_error("invalid JSON-RPC text message"))?;
    if !message.is_object() {
        return Err(protocol_error("JSON-RPC message must be an object"));
    }
    check_depth(&message, 0)?;
    Ok(message)
}

fn check_depth(value: &Value, depth: usize) -> Result<()> {
    if depth > MAX_JSON_DEPTH {
        return Err(protocol_error("message exceeds nesting limit"));
    }
    match value {
        Value::Object(map) => {
            for item in map.values() {
                check_depth(item, depth + 1)?;
            }
        }
        Value::Array(items) => {
            for item in items {
                check_depth(item, depth + 1)?;
            }
        }
        _ => {}
    }
    Ok(())
}

pub fn extract_server_info(initialization: Option<&Value>) -> Value {
    let Some(initialization) = initialization else {
        return json!({"version": null, "capabilities": {}, "error": "invalid response type"});
    };
    if !initialization.is_object() {
        return json!({"version": null, "capabilities": {}, "error": "invalid response type"});
    }
    json!({
        "version": initialization.get("serverInfo").and_then(|info| info.get("version")).cloned().unwrap_or(Value::Null),
        "name": initialization.get("serverInfo").and_then(|info| info.get("name")).cloned().unwrap_or(Value::Null),
        "capabilities": initialization.get("capabilities").cloned().unwrap_or_else(|| json!({})),
        "raw_response": initialization
    })
}

fn base64(data: &[u8]) -> String {
    data.chunks(3).map(encode_base64_chunk).collect()
}

fn encode_base64_chunk(chunk: &[u8]) -> String {
    let value = base64_chunk_value(chunk);
    let mut output = String::with_capacity(4);
    output.push(base64_digit(value >> 18));
    output.push(base64_digit(value >> 12));
    output.push(pad_or(base64_digit(value >> 6), chunk.len() > 1));
    output.push(pad_or(base64_digit(value), chunk.len() > 2));
    output
}

fn base64_chunk_value(chunk: &[u8]) -> u32 {
    let first = chunk[0] as u32;
    let second = chunk.get(1).copied().unwrap_or(0) as u32;
    let third = chunk.get(2).copied().unwrap_or(0) as u32;
    (first << 16) | (second << 8) | third
}

fn base64_digit(value: u32) -> char {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    TABLE[(value & 0x3f) as usize] as char
}

fn pad_or(character: char, present: bool) -> char {
    if present {
        return character;
    }
    '='
}

fn websocket_accept(key: &str) -> String {
    let mut digest = Sha1::new();
    digest.update(key.as_bytes());
    digest.update(WS_GUID.as_bytes());
    base64(&digest.finalize())
}

struct Headers {
    lines: Vec<String>,
    fields: BTreeMap<String, String>,
}

fn parse_headers(head: &[u8]) -> Result<Headers> {
    let text = header_text(head)?;
    build_headers(text)
}

fn header_text(head: &[u8]) -> Result<&str> {
    std::str::from_utf8(head).map_err(|_| protocol_error("malformed WebSocket handshake"))
}

fn build_headers(text: &str) -> Result<Headers> {
    let lines: Vec<String> = text.split("\r\n").map(ToString::to_string).collect();
    let fields = collect_header_fields(&lines)?;
    Ok(Headers { lines, fields })
}

fn collect_header_fields(lines: &[String]) -> Result<BTreeMap<String, String>> {
    let mut fields = BTreeMap::new();
    lines.iter().skip(1).try_for_each(|line| {
        let (name, value) = parse_header_line(line)?;
        if fields.insert(name, value).is_some() {
            return Err(protocol_error("malformed WebSocket handshake"));
        }
        Ok(())
    })?;
    Ok(fields)
}

fn parse_header_line(line: &str) -> Result<(String, String)> {
    let (name, value) = line
        .split_once(':')
        .ok_or_else(|| protocol_error("malformed WebSocket handshake"))?;
    Ok((name.to_lowercase(), value.trim().to_string()))
}

pub(crate) fn validate_socket(socket_path: &str) -> Result<()> {
    validate_socket_metadata(&socket_metadata(socket_path)?)
}

fn socket_metadata(socket_path: &str) -> Result<fs::Metadata> {
    fs::metadata(socket_path)
        .map_err(|error| rpc_error(format!("cannot access app-server socket: {}", error.kind())))
}

fn validate_socket_metadata(metadata: &fs::Metadata) -> Result<()> {
    check_socket_file_type(metadata)?;
    check_socket_owner(metadata)
}

fn check_socket_file_type(metadata: &fs::Metadata) -> Result<()> {
    if !metadata.file_type().is_socket() {
        return reject_socket(
            "app-server endpoint is not a UNIX socket",
            "app-server endpoint is not a UNIX socket",
        );
    }
    Ok(())
}

fn check_socket_owner(metadata: &fs::Metadata) -> Result<()> {
    // SAFETY: geteuid takes no pointers and has no caller preconditions.
    if metadata.uid() != unsafe { libc::geteuid() } {
        return reject_socket(
            "app-server socket owner mismatch",
            "app-server socket is owned by another user",
        );
    }
    check_socket_permissions(metadata)
}

fn check_socket_permissions(metadata: &fs::Metadata) -> Result<()> {
    if metadata.mode() & 0o002 != 0 {
        return reject_socket(
            "app-server socket is world-writable",
            "app-server socket is world-writable",
        );
    }
    Ok(())
}

fn reject_socket(log_message: &'static str, error_message: &'static str) -> Result<()> {
    logging::log_event("rpc_socket_rejected", "codex_rpc", log_message);
    Err(rpc_error(error_message))
}

pub struct Client {
    pub socket_path: String,
    pub timeout: f64,
    pub max_message: usize,
    pub max_events: usize,
    pub events: Vec<Value>,
    pub initialization: Option<Value>,
    pub server_info: Option<Value>,
    stream: Option<UnixStream>,
    decoder: FrameDecoder,
    messages: VecDeque<(u8, Vec<u8>)>,
    sequence: u64,
}

impl Client {
    pub fn new(socket_path: &str) -> Self {
        Self::with_limits(socket_path, 10.0, MAX_MESSAGE, 4096)
    }

    pub fn with_limits(
        socket_path: &str,
        timeout: f64,
        max_message: usize,
        max_events: usize,
    ) -> Self {
        Self {
            socket_path: socket_path.to_string(),
            timeout,
            max_message,
            max_events,
            events: Vec::new(),
            initialization: None,
            server_info: None,
            stream: None,
            decoder: FrameDecoder::new(false, max_message),
            messages: VecDeque::new(),
            sequence: 0,
        }
    }

    pub fn connect(&mut self) -> Result<()> {
        self.connect_if_needed()
    }

    fn connect_if_needed(&mut self) -> Result<()> {
        if self.stream.is_some() {
            return Ok(());
        }
        self.open_stream()
    }

    fn open_stream(&mut self) -> Result<()> {
        validate_socket(&self.socket_path)?;
        self.open_validated_stream()
    }

    fn open_validated_stream(&mut self) -> Result<()> {
        self.reset_transport();
        self.attach_stream()
    }

    fn reset_transport(&mut self) {
        self.decoder = FrameDecoder::new(false, self.max_message);
        self.messages.clear();
    }

    fn attach_stream(&mut self) -> Result<()> {
        let stream = crate::util::unix_socket::connect(std::path::Path::new(&self.socket_path))
            .map_err(|error| rpc_error(format!("WebSocket connection failed: {error}")))?;
        super::peer::verify(&stream)?;
        self.stream = Some(stream);
        self.start_session()
    }

    fn start_session(&mut self) -> Result<()> {
        let deadline = Instant::now() + Duration::from_secs_f64(self.timeout);
        self.perform_handshake(deadline)?;
        self.perform_initialize(deadline)
    }

    fn perform_handshake(&mut self, deadline: Instant) -> Result<()> {
        let key = self.handshake_key()?;
        self.negotiate(&key, deadline)
    }

    fn handshake_key(&self) -> Result<String> {
        Ok(base64(&random_bytes(16)?))
    }

    fn negotiate(&mut self, key: &str, deadline: Instant) -> Result<()> {
        let extra = self.handshake_or_close(key, deadline)?;
        self.feed_decoder(&extra)
    }

    fn handshake_or_close(&mut self, key: &str, deadline: Instant) -> Result<Vec<u8>> {
        self.handshake(key, deadline).inspect_err(|_| {
            self.close(false);
        })
    }

    fn perform_initialize(&mut self, deadline: Instant) -> Result<()> {
        let initialization = self.initialize_call(deadline)?;
        self.finish_initialize(initialization, deadline)
    }

    fn initialize_call(&mut self, deadline: Instant) -> Result<Value> {
        self.call_internal("initialize", &initialize_params(), deadline)
            .inspect_err(|_| {
                self.close(false);
            })
    }

    fn finish_initialize(&mut self, initialization: Value, deadline: Instant) -> Result<()> {
        self.initialization = Some(initialization.clone());
        self.server_info = Some(extract_server_info(Some(&initialization)));
        log_initialized(self.server_info.as_ref(), &self.socket_path);
        self.send_message(&json!({"method": "initialized"}), deadline)
    }

    fn handshake(&mut self, key: &str, deadline: Instant) -> Result<Vec<u8>> {
        self.write_all(upgrade_request(key).as_bytes(), deadline)?;
        self.read_handshake(key, deadline)
    }

    fn read_handshake(&mut self, key: &str, deadline: Instant) -> Result<Vec<u8>> {
        let mut headers = Vec::new();
        let mut progress = HandshakeProgress::Continue;
        while progress.continues() {
            progress = self.handshake_step(&mut headers, deadline);
        }
        finish_handshake(headers, progress, key)
    }

    fn handshake_step(&mut self, headers: &mut Vec<u8>, deadline: Instant) -> HandshakeProgress {
        self.read_some(deadline)
            .map_or_else(HandshakeProgress::Failed, |data| {
                append_handshake_chunk(headers, data)
            })
    }

    fn set_timeouts(&mut self, remaining: Duration) -> Result<()> {
        let stream = self.stream_ref()?;
        set_stream_timeouts(stream, remaining)
    }

    fn write_all(&mut self, data: &[u8], deadline: Instant) -> Result<()> {
        let remaining = remaining_until(deadline, "WebSocket write timed out")?;
        self.write_with_remaining(data, remaining)
    }

    fn write_with_remaining(&mut self, data: &[u8], remaining: Duration) -> Result<()> {
        self.set_timeouts(remaining)?;
        self.write_to_stream(data)
    }

    fn write_to_stream(&mut self, data: &[u8]) -> Result<()> {
        let stream = self.stream_mut()?;
        stream.write_all(data).map_err(write_error)
    }

    fn read_some(&mut self, deadline: Instant) -> Result<Vec<u8>> {
        let remaining = remaining_until(deadline, "WebSocket read timed out")?;
        self.read_with_remaining(remaining)
    }

    fn read_with_remaining(&mut self, remaining: Duration) -> Result<Vec<u8>> {
        self.set_timeouts(remaining)?;
        self.read_from_stream()
    }

    fn read_from_stream(&mut self) -> Result<Vec<u8>> {
        let mut buffer = [0u8; 65_536];
        let read = self.read_into(&mut buffer)?;
        read_payload(read, &buffer)
    }

    fn read_into(&mut self, buffer: &mut [u8]) -> Result<usize> {
        let stream = self.stream_mut()?;
        stream.read(buffer).map_err(read_error)
    }

    fn send_message(&mut self, message: &Value, deadline: Instant) -> Result<()> {
        let payload = encode_message(message)?;
        self.send_payload(&payload, deadline)
    }

    fn send_payload(&mut self, payload: &[u8], deadline: Instant) -> Result<()> {
        self.check_outgoing_size(payload)?;
        self.write_frame(payload, deadline)
    }

    fn write_frame(&mut self, payload: &[u8], deadline: Instant) -> Result<()> {
        let frame = encode_frame(payload, 1, true, true)?;
        self.write_all(&frame, deadline)
    }

    fn check_outgoing_size(&self, payload: &[u8]) -> Result<()> {
        if payload.len() > self.max_message {
            return Err(protocol_error("outgoing JSON-RPC message exceeds limit"));
        }
        Ok(())
    }

    fn receive(&mut self, deadline: Instant) -> Result<Value> {
        let mut outcome = ReceiveOutcome::Pending;
        while outcome.is_pending() {
            outcome = self.receive_step(deadline);
        }
        outcome.into_result()
    }

    fn receive_step(&mut self, deadline: Instant) -> ReceiveOutcome {
        if self.messages.is_empty() {
            return self.fill_messages(deadline);
        }
        self.handle_message(deadline)
    }

    fn fill_messages(&mut self, deadline: Instant) -> ReceiveOutcome {
        self.read_some(deadline)
            .map_or_else(ReceiveOutcome::Failed, |data| self.feed_incoming(data))
    }

    fn feed_incoming(&mut self, data: Vec<u8>) -> ReceiveOutcome {
        self.decoder
            .feed(&data)
            .map_or_else(ReceiveOutcome::Failed, |messages| {
                self.messages.extend(messages);
                ReceiveOutcome::Pending
            })
    }

    fn handle_message(&mut self, deadline: Instant) -> ReceiveOutcome {
        let (opcode, payload) = self.messages.pop_front().unwrap_or((8, Vec::new()));
        self.dispatch_message(opcode, payload, deadline)
    }

    fn dispatch_message(
        &mut self,
        opcode: u8,
        payload: Vec<u8>,
        deadline: Instant,
    ) -> ReceiveOutcome {
        if opcode >= 8 {
            return self.handle_control(opcode, payload, deadline);
        }
        handle_data_message(opcode, &payload)
    }

    fn handle_control(
        &mut self,
        opcode: u8,
        payload: Vec<u8>,
        deadline: Instant,
    ) -> ReceiveOutcome {
        if opcode == 8 {
            return self.handle_close(payload, deadline);
        }
        self.handle_ping_control(opcode, payload, deadline)
    }

    fn handle_ping_control(
        &mut self,
        opcode: u8,
        payload: Vec<u8>,
        deadline: Instant,
    ) -> ReceiveOutcome {
        if opcode == 9 {
            return ping_result(self.send_pong(&payload, deadline));
        }
        ReceiveOutcome::Pending
    }

    fn send_pong(&mut self, payload: &[u8], deadline: Instant) -> Result<()> {
        let frame = encode_frame(payload, 10, true, true)?;
        self.write_all(&frame, deadline)
    }

    fn handle_close(&mut self, payload: Vec<u8>, deadline: Instant) -> ReceiveOutcome {
        ReceiveOutcome::Done(self.close_exchange(&payload, deadline))
    }

    fn close_exchange(&mut self, payload: &[u8], deadline: Instant) -> Result<Value> {
        let frame = encode_frame(payload, 8, true, true)?;
        let _ = self.write_all(&frame, deadline);
        let code = close_code(payload);
        self.close(false);
        Err(rpc_error("WebSocket peer sent close").with_rpc(code, None))
    }

    fn record(&mut self, message: Value) -> Result<()> {
        if self.events.len() >= self.max_events {
            return Err(protocol_error("unconsumed JSON-RPC events exceed limit"));
        }
        self.events.push(message);
        Ok(())
    }

    fn call_internal(&mut self, method: &str, params: &Value, deadline: Instant) -> Result<Value> {
        let identity = self.next_sequence();
        self.exchange(identity, method, params, deadline)
    }

    fn next_sequence(&mut self) -> u64 {
        self.sequence += 1;
        self.sequence
    }

    fn exchange(
        &mut self,
        identity: u64,
        method: &str,
        params: &Value,
        deadline: Instant,
    ) -> Result<Value> {
        self.send_request(identity, method, params, deadline)?;
        self.await_response(identity, method, deadline)
    }

    fn send_request(
        &mut self,
        identity: u64,
        method: &str,
        params: &Value,
        deadline: Instant,
    ) -> Result<()> {
        self.send_message(
            &json!({"id": identity, "method": method, "params": params}),
            deadline,
        )
    }

    fn await_response(&mut self, identity: u64, method: &str, deadline: Instant) -> Result<Value> {
        let mut wait = ResponseWait::Continue;
        while wait.continues() {
            wait = self.response_step(identity, method, deadline);
        }
        wait.into_result()
    }

    fn response_step(&mut self, identity: u64, method: &str, deadline: Instant) -> ResponseWait {
        self.receive(deadline)
            .map_or_else(ResponseWait::Failed, |message| {
                self.classify_response(message, identity, method)
            })
    }

    fn classify_response(&mut self, message: Value, identity: u64, method: &str) -> ResponseWait {
        if let Some(response) = response_for(&message, identity) {
            return ResponseWait::Done(response_value(response, method));
        }
        continue_result(self.record(message))
    }

    fn stream_ref(&self) -> Result<&UnixStream> {
        self.stream
            .as_ref()
            .ok_or_else(|| rpc_error("Socket not connected"))
    }

    fn stream_mut(&mut self) -> Result<&mut UnixStream> {
        self.stream
            .as_mut()
            .ok_or_else(|| rpc_error("Socket not connected"))
    }

    pub fn call(&mut self, method: &str, params: &Value) -> Result<Value> {
        self.call_with_timeout(method, params, None)
    }

    pub fn call_with_timeout(
        &mut self,
        method: &str,
        params: &Value,
        timeout: Option<f64>,
    ) -> Result<Value> {
        self.connect()?;
        let deadline = Instant::now() + Duration::from_secs_f64(timeout.unwrap_or(self.timeout));
        self.call_internal(method, params, deadline)
    }

    /// Collect available events, optionally waiting up to `timeout` for
    /// ingress. A partial frame remains buffered.
    pub fn drain(&mut self, timeout: f64) -> Result<Vec<Value>> {
        let start = self.begin_drain()?;
        self.drain_with_deadline(start, drain_deadline(timeout))
    }

    fn begin_drain(&mut self) -> Result<usize> {
        self.connect()?;
        Ok(self.events.len())
    }

    fn drain_with_deadline(&mut self, start: usize, deadline: Instant) -> Result<Vec<Value>> {
        self.drain_until(deadline)?;
        Ok(self.events[start..].to_vec())
    }

    fn drain_until(&mut self, deadline: Instant) -> Result<()> {
        let mut outcome = DrainOutcome::Continue;
        while outcome.continues() {
            outcome = self.drain_step(deadline);
        }
        outcome.into_result()
    }

    fn drain_step(&mut self, deadline: Instant) -> DrainOutcome {
        if let Err(error) = self.drain_ready(deadline) {
            return DrainOutcome::Failed(error);
        }
        self.pump(deadline)
    }

    fn drain_ready(&mut self, deadline: Instant) -> Result<()> {
        let mut outcome = ReadyOutcome::Continue;
        while self.ready_continues(&outcome) {
            outcome = self.drain_message(deadline);
        }
        outcome.into_result()
    }

    fn ready_continues(&self, outcome: &ReadyOutcome) -> bool {
        if !outcome.continues() {
            return false;
        }
        !self.messages.is_empty()
    }

    fn drain_message(&mut self, deadline: Instant) -> ReadyOutcome {
        let short = Instant::now() + Duration::from_millis(1);
        self.receive(short.max_deadline(deadline))
            .map_or_else(classify_drain_error, |message| self.record_ready(message))
    }

    fn record_ready(&mut self, message: Value) -> ReadyOutcome {
        if let Err(error) = self.record(message) {
            return ReadyOutcome::Failed(error);
        }
        ReadyOutcome::Continue
    }

    fn pump(&mut self, deadline: Instant) -> DrainOutcome {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let ready = self.poll_readable(remaining);
        if matches!(&ready, Ok(false)) {
            return DrainOutcome::Stop;
        }
        self.pump_after_poll(ready, remaining)
    }

    fn pump_after_poll(&mut self, ready: Result<bool>, remaining: Duration) -> DrainOutcome {
        if let Err(error) = ready {
            return DrainOutcome::Failed(error);
        }
        self.read_ingress(remaining)
    }

    fn read_ingress(&mut self, remaining: Duration) -> DrainOutcome {
        let read_deadline = Instant::now() + remaining.max(Duration::from_millis(1));
        if let Err(error) = self.buffer_ingress(read_deadline) {
            return DrainOutcome::Failed(error);
        }
        DrainOutcome::Continue
    }

    fn buffer_ingress(&mut self, deadline: Instant) -> Result<()> {
        let data = self.read_some(deadline)?;
        self.feed_decoder(&data)
    }

    fn feed_decoder(&mut self, data: &[u8]) -> Result<()> {
        let messages = self.decoder.feed(data)?;
        self.messages.extend(messages);
        Ok(())
    }

    fn poll_readable(&mut self, timeout: Duration) -> Result<bool> {
        let stream = self.stream_ref()?;
        poll_stream(stream, timeout)
    }

    pub fn pop_events(&mut self) -> Vec<Value> {
        std::mem::take(&mut self.events)
    }

    /// Consume a matching notification, retaining all unrelated ingress.
    pub fn wait_event(
        &mut self,
        method: &str,
        thread_id: Option<&str>,
        timeout: f64,
    ) -> Result<Value> {
        self.connect()?;
        let deadline = Instant::now() + Duration::from_secs_f64(timeout);
        self.wait_event_until(method, thread_id, deadline)
    }

    fn wait_event_until(
        &mut self,
        method: &str,
        thread_id: Option<&str>,
        deadline: Instant,
    ) -> Result<Value> {
        let mut wait = EventWait::Continue;
        while wait.continues() {
            wait = self.event_step(method, thread_id, deadline);
        }
        wait.into_result()
    }

    fn event_step(
        &mut self,
        method: &str,
        thread_id: Option<&str>,
        deadline: Instant,
    ) -> EventWait {
        if let Some(index) = self.find_event_index(method, thread_id) {
            return EventWait::Done(self.events.remove(index));
        }
        self.receive(deadline)
            .map_or_else(EventWait::Failed, |message| self.record_event(message))
    }

    fn find_event_index(&self, method: &str, thread_id: Option<&str>) -> Option<usize> {
        self.events
            .iter()
            .position(|event| matches_event(event, method, thread_id))
    }

    fn record_event(&mut self, message: Value) -> EventWait {
        if let Err(error) = self.record(message) {
            return EventWait::Failed(error);
        }
        EventWait::Continue
    }

    pub fn close(&mut self, send_close: bool) {
        let Some(mut stream) = self.stream.take() else {
            return;
        };
        if send_close {
            send_close_frame(&mut stream);
        }
        let _ = stream.shutdown(std::net::Shutdown::Both);
    }
}

fn log_initialized(server_info: Option<&Value>, socket_path: &str) {
    logging::log_event_fields(
        "rpc_initialized",
        "codex_rpc",
        "RPC client initialized",
        logging::Fields {
            metadata: Some(&json!({
                "server_name": server_info.and_then(|info| info.get("name")),
                "server_version": server_info.and_then(|info| info.get("version")),
                "socket_path": socket_path
            })),
            ..Default::default()
        },
    );
}

fn initialize_params() -> Value {
    json!({
        "clientInfo": {"name": "herdr_issue_flow", "version": "1.0.0"},
        "capabilities": {"experimentalApi": true}
    })
}

fn upgrade_request(key: &str) -> String {
    format!(
        "GET / HTTP/1.1\r\nHost: localhost\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: {key}\r\n\r\n"
    )
}

enum HandshakeProgress {
    Continue,
    Boundary(usize),
    Failed(FlowError),
}

impl HandshakeProgress {
    fn continues(&self) -> bool {
        matches!(self, Self::Continue)
    }

    fn into_result(self) -> Result<usize> {
        if let Self::Boundary(boundary) = self {
            return Ok(boundary);
        }
        self.into_error()
    }

    fn into_error(self) -> Result<usize> {
        if let Self::Failed(error) = self {
            return Err(error);
        }
        Err(protocol_error("WebSocket handshake exceeds limit"))
    }
}

fn append_handshake_chunk(headers: &mut Vec<u8>, data: Vec<u8>) -> HandshakeProgress {
    headers.extend_from_slice(&data);
    handshake_progress(headers)
}

fn handshake_progress(headers: &[u8]) -> HandshakeProgress {
    let Some(boundary) = find_subslice(headers, b"\r\n\r\n") else {
        return handshake_growth(headers);
    };
    validate_handshake_boundary(boundary)
}

fn handshake_growth(headers: &[u8]) -> HandshakeProgress {
    if headers.len() > MAX_HEADERS {
        return HandshakeProgress::Failed(protocol_error("WebSocket handshake exceeds limit"));
    }
    HandshakeProgress::Continue
}

fn validate_handshake_boundary(boundary: usize) -> HandshakeProgress {
    if boundary > MAX_HEADERS {
        return HandshakeProgress::Failed(protocol_error("WebSocket handshake exceeds limit"));
    }
    HandshakeProgress::Boundary(boundary)
}

fn finish_handshake(headers: Vec<u8>, progress: HandshakeProgress, key: &str) -> Result<Vec<u8>> {
    let boundary = progress.into_result()?;
    accept_handshake(&headers, boundary, key)
}

fn accept_handshake(headers: &[u8], boundary: usize, key: &str) -> Result<Vec<u8>> {
    validate_handshake_response(headers, boundary, key)?;
    Ok(headers[boundary + 4..].to_vec())
}

fn validate_handshake_response(headers: &[u8], boundary: usize, key: &str) -> Result<()> {
    let parsed = parse_headers(&headers[..boundary])?;
    check_upgrade_headers(&parsed, key)
}

fn check_upgrade_headers(parsed: &Headers, key: &str) -> Result<()> {
    let accept = websocket_accept(key);
    if !is_valid_upgrade(parsed, &accept) {
        return Err(protocol_error("WebSocket upgrade rejected or invalid"));
    }
    Ok(())
}

fn is_valid_upgrade(parsed: &Headers, accept: &str) -> bool {
    headers_identify_websocket(parsed) && accept_and_no_extensions(parsed, accept)
}

fn headers_identify_websocket(parsed: &Headers) -> bool {
    valid_status_line(parsed) && valid_upgrade_and_connection(parsed)
}

fn valid_upgrade_and_connection(parsed: &Headers) -> bool {
    valid_upgrade_header(parsed) && valid_connection_header(parsed)
}

fn accept_and_no_extensions(parsed: &Headers, accept: &str) -> bool {
    valid_accept_header(parsed, accept) && no_websocket_extensions(parsed)
}

fn valid_status_line(parsed: &Headers) -> bool {
    parsed
        .lines
        .first()
        .map(|line| line.starts_with("HTTP/1.1 101 "))
        .unwrap_or(false)
}

fn valid_upgrade_header(parsed: &Headers) -> bool {
    parsed
        .fields
        .get("upgrade")
        .map(|value| value.to_lowercase())
        == Some("websocket".to_string())
}

fn valid_connection_header(parsed: &Headers) -> bool {
    parsed
        .fields
        .get("connection")
        .map(|value| {
            value
                .split(',')
                .any(|item| item.trim().to_lowercase() == "upgrade")
        })
        .unwrap_or(false)
}

fn valid_accept_header(parsed: &Headers, accept: &str) -> bool {
    parsed
        .fields
        .get("sec-websocket-accept")
        .map(String::as_str)
        == Some(accept)
}

fn no_websocket_extensions(parsed: &Headers) -> bool {
    !parsed.fields.contains_key("sec-websocket-extensions")
}

fn set_stream_timeouts(stream: &UnixStream, remaining: Duration) -> Result<()> {
    set_read_timeout(stream, remaining)?;
    set_write_timeout(stream, remaining)
}

fn set_read_timeout(stream: &UnixStream, remaining: Duration) -> Result<()> {
    stream.set_read_timeout(Some(timeout_value(remaining)))?;
    Ok(())
}

fn set_write_timeout(stream: &UnixStream, remaining: Duration) -> Result<()> {
    stream.set_write_timeout(Some(timeout_value(remaining)))?;
    Ok(())
}

fn timeout_value(remaining: Duration) -> Duration {
    remaining.max(Duration::from_millis(1))
}

fn remaining_until(deadline: Instant, message: &'static str) -> Result<Duration> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Err(timeout_error(message));
    }
    Ok(remaining)
}

fn write_error(error: std::io::Error) -> FlowError {
    if is_timeout(&error) {
        return timeout_error("WebSocket write timed out");
    }
    rpc_error(format!("WebSocket write failed: {error}"))
}

fn read_error(error: std::io::Error) -> FlowError {
    if is_timeout(&error) {
        return timeout_error("WebSocket read timed out");
    }
    rpc_error(format!("WebSocket read failed: {error}"))
}

fn read_payload(read: usize, buffer: &[u8]) -> Result<Vec<u8>> {
    if read == 0 {
        return Err(rpc_error("WebSocket peer closed connection"));
    }
    Ok(buffer[..read].to_vec())
}

fn encode_message(message: &Value) -> Result<Vec<u8>> {
    serde_json::to_vec(message)
        .map_err(|error| protocol_error(format!("cannot encode JSON-RPC message: {error}")))
}

enum ReceiveOutcome {
    Pending,
    Done(Result<Value>),
    Failed(FlowError),
}

impl ReceiveOutcome {
    fn is_pending(&self) -> bool {
        matches!(self, Self::Pending)
    }

    fn into_result(self) -> Result<Value> {
        if let Self::Done(result) = self {
            return result;
        }
        self.failed()
    }

    fn failed(self) -> Result<Value> {
        if let Self::Failed(error) = self {
            return Err(error);
        }
        Err(protocol_error("WebSocket receive loop stalled"))
    }
}

fn handle_data_message(opcode: u8, payload: &[u8]) -> ReceiveOutcome {
    if opcode == 1 {
        return ReceiveOutcome::Done(decode_message(payload));
    }
    binary_or_pending(opcode)
}

fn binary_or_pending(opcode: u8) -> ReceiveOutcome {
    if opcode == 2 {
        return ReceiveOutcome::Done(Err(protocol_error(
            "binary JSON-RPC messages are unsupported",
        )));
    }
    ReceiveOutcome::Pending
}

fn ping_result(result: Result<()>) -> ReceiveOutcome {
    if let Err(error) = result {
        return ReceiveOutcome::Failed(error);
    }
    ReceiveOutcome::Pending
}

fn close_code(payload: &[u8]) -> Option<i64> {
    if payload.len() < 2 {
        return None;
    }
    Some(u16::from_be_bytes([payload[0], payload[1]]) as i64)
}

fn response_for(message: &Value, identity: u64) -> Option<&Value> {
    if !model_has_no_method(message) {
        return None;
    }
    message_id_matches(message, identity).then_some(message)
}

fn message_id_matches(message: &Value, identity: u64) -> bool {
    message.get("id").and_then(Value::as_u64) == Some(identity)
}

fn response_value(response: &Value, method: &str) -> Result<Value> {
    if let Some(error) = response.get("error") {
        return Err(response_error(error, method));
    }
    response
        .get("result")
        .cloned()
        .ok_or_else(|| protocol_error("JSON-RPC response has no result"))
}

fn response_error(error: &Value, method: &str) -> FlowError {
    if !error.is_object() {
        return protocol_error("invalid JSON-RPC error");
    }
    rpc_error(error_text(error))
        .with_rpc(
            error.get("code").and_then(Value::as_i64),
            Some(error.clone()),
        )
        .with_method(method)
}

fn error_text(error: &Value) -> String {
    error
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("JSON-RPC request failed")
        .to_string()
}

enum ResponseWait {
    Continue,
    Done(Result<Value>),
    Failed(FlowError),
}

impl ResponseWait {
    fn continues(&self) -> bool {
        matches!(self, Self::Continue)
    }

    fn into_result(self) -> Result<Value> {
        if let Self::Done(result) = self {
            return result;
        }
        self.failed()
    }

    fn failed(self) -> Result<Value> {
        if let Self::Failed(error) = self {
            return Err(error);
        }
        Err(protocol_error("JSON-RPC response wait stalled"))
    }
}

fn continue_result(result: Result<()>) -> ResponseWait {
    if let Err(error) = result {
        return ResponseWait::Failed(error);
    }
    ResponseWait::Continue
}

fn drain_deadline(timeout: f64) -> Instant {
    Instant::now() + Duration::from_secs_f64(timeout.max(0.0))
}

enum DrainOutcome {
    Continue,
    Stop,
    Failed(FlowError),
}

impl DrainOutcome {
    fn continues(&self) -> bool {
        matches!(self, Self::Continue)
    }

    fn into_result(self) -> Result<()> {
        if let Self::Failed(error) = self {
            return Err(error);
        }
        Ok(())
    }
}

enum ReadyOutcome {
    Continue,
    Stop,
    Failed(FlowError),
}

impl ReadyOutcome {
    fn continues(&self) -> bool {
        matches!(self, Self::Continue)
    }

    fn into_result(self) -> Result<()> {
        if let Self::Failed(error) = self {
            return Err(error);
        }
        Ok(())
    }
}

fn classify_drain_error(error: FlowError) -> ReadyOutcome {
    if error.kind == ErrorKind::RpcTimeout {
        return ReadyOutcome::Stop;
    }
    ReadyOutcome::Failed(error)
}

fn poll_stream(stream: &UnixStream, timeout: Duration) -> Result<bool> {
    let mut poll_fd = libc::pollfd {
        fd: stream.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    let millis = timeout.as_millis().min(i32::MAX as u128) as i32;
    // SAFETY: poll_fd is one writable element and stream remains borrowed/open
    // for the call. poll writes revents without taking descriptor ownership.
    let result = unsafe { libc::poll(&mut poll_fd, 1, millis) };
    if result < 0 {
        return poll_error();
    }
    Ok(result > 0)
}

fn poll_error() -> Result<bool> {
    let error = std::io::Error::last_os_error();
    if error.kind() == IoErrorKind::Interrupted {
        return Ok(true);
    }
    Err(rpc_error(format!("WebSocket poll failed: {error}")))
}

enum EventWait {
    Continue,
    Done(Value),
    Failed(FlowError),
}

impl EventWait {
    fn continues(&self) -> bool {
        matches!(self, Self::Continue)
    }

    fn into_result(self) -> Result<Value> {
        if let Self::Done(value) = self {
            return Ok(value);
        }
        self.failed()
    }

    fn failed(self) -> Result<Value> {
        if let Self::Failed(error) = self {
            return Err(error);
        }
        Err(protocol_error("event wait stalled"))
    }
}

fn matches_event(event: &Value, method: &str, thread_id: Option<&str>) -> bool {
    event_matches_method(event, method) && event_matches_thread(event, thread_id)
}

fn event_matches_method(event: &Value, method: &str) -> bool {
    event.get("method").and_then(Value::as_str) == Some(method) && event.get("id").is_none()
}

fn event_matches_thread(event: &Value, thread_id: Option<&str>) -> bool {
    thread_id
        .map(|expected| event_thread_id(event) == Some(expected))
        .unwrap_or(true)
}

fn event_thread_id(event: &Value) -> Option<&str> {
    event
        .get("params")
        .and_then(|params| params.get("threadId"))
        .and_then(Value::as_str)
}

fn send_close_frame(stream: &mut UnixStream) {
    let _ = stream.set_write_timeout(Some(Duration::from_millis(100)));
    if let Ok(frame) = encode_frame(&1000u16.to_be_bytes(), 8, true, true) {
        let _ = stream.write_all(&frame);
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        self.close(false);
    }
}

trait DeadlineExt {
    fn max_deadline(self, other: Instant) -> Self;
}

impl DeadlineExt for Instant {
    fn max_deadline(self, other: Instant) -> Self {
        if self > other {
            self
        } else {
            other
        }
    }
}

fn model_has_no_method(message: &Value) -> bool {
    message.get("method").is_none()
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn is_timeout(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        IoErrorKind::WouldBlock | IoErrorKind::TimedOut
    )
}

/// Hex helper retained for tests and diagnostics.
pub fn sha256_hex_text(value: &str) -> String {
    to_hex(crate::util::sha256_bytes(value.as_bytes()).as_slice())
}
