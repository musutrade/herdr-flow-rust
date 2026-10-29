//! Only task attachments filter frames. Approvals and terminal I/O stay native;
//! the broker remains the sole responder to its dynamic queue tools.
use super::*;
use crate::codex::rpc::{
    decode_native_client_message_with_limit, decode_native_server_message_with_limit, encode_frame,
    encode_frame_with_limit, FrameDecoder, MAX_HEADERS, MAX_MESSAGE,
};
use std::{cell::RefCell, rc::Rc};

// Matches Codex remote.rs REMOTE_APP_SERVER_MAX_WEBSOCKET_MESSAGE_SIZE.
// Real native catalog responses can exceed the broker's 16 MiB RPC limit.
const NATIVE_MESSAGE_LIMIT: usize = crate::codex::tui::NATIVE_SERVER_MESSAGE_LIMIT;

struct Direction {
    client: bool,
    limit: usize,
    headers: Option<Vec<u8>>,
    frames: FrameDecoder,
    responses: Vec<u8>,
    selection: J,
    drain: Option<Rc<RefCell<drain::Drain>>>,
}
impl Direction {
    fn buffered(&self) -> bool {
        self.headers.as_ref().is_some_and(|h| !h.is_empty()) || self.frames.has_pending()
    }

    fn new(client: bool) -> Self {
        let limit = if client {
            MAX_MESSAGE
        } else {
            NATIVE_MESSAGE_LIMIT
        };
        Self {
            client,
            limit,
            headers: Some(Vec::new()),
            frames: FrameDecoder::new(client, limit),
            responses: Vec::new(),
            selection: J::Null,
            drain: None,
        }
    }
    fn feed(&mut self, data: &[u8], session: &J) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        let remaining;
        let data = if let Some(headers) = &mut self.headers {
            headers.extend_from_slice(data);
            let boundary = headers.windows(4).position(|w| w == b"\r\n\r\n");
            ensure(
                boundary.unwrap_or(headers.len()) <= MAX_HEADERS,
                "native handshake exceeds bound",
            )?;
            let Some(boundary) = boundary else {
                return Ok(out);
            };
            out.extend_from_slice(&headers[..boundary + 4]);
            remaining = headers[boundary + 4..].to_vec();
            self.headers = None;
            remaining.as_slice()
        } else {
            data
        };
        out.extend(self.forward_frames(data, session)?);
        Ok(out)
    }

    fn forward_frames(&mut self, data: &[u8], session: &J) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        for (opcode, payload) in self.frames.feed(data)? {
            if opcode == 1 && !self.forward_message(&self.decode_message(&payload)?, session)? {
                continue;
            }
            out.extend(encode_frame_with_limit(
                &payload,
                opcode,
                self.client,
                true,
                self.limit,
            )?);
        }
        Ok(out)
    }

    fn decode_message(&self, payload: &[u8]) -> Result<J> {
        if self.client {
            decode_native_client_message_with_limit(payload, self.limit)
        } else {
            decode_native_server_message_with_limit(payload, self.limit)
        }
    }

    fn forward_message(&mut self, message: &J, session: &J) -> Result<bool> {
        match self.decision(message, session) {
            Ok(forward) => Ok(forward),
            Err(error) if self.client && message["method"].is_string() => {
                // A native auxiliary request or unsupported slash command must
                // receive a proper RPC refusal, not tear down the task socket.
                if message["id"].is_string() || message["id"].is_number() {
                    self.refuse(message, &error)?;
                }
                Ok(false)
            }
            Err(error) => Err(error),
        }
    }

    fn decision(&mut self, message: &J, session: &J) -> Result<bool> {
        let forward = allowed(self.client, message, session)?;
        if self.client && !self.selection.is_null() {
            selected_runtime(message, &self.selection)?;
        }
        if forward {
            if let Some(drain) = &self.drain {
                drain.borrow_mut().message(self.client, message)?;
            }
        }
        Ok(forward)
    }

    fn refuse(&mut self, message: &J, error: &FlowError) -> Result<()> {
        let response =
            json!({"id":message["id"],"error":{"code":-32600,"message":error.to_string()}});
        self.responses.extend(encode_frame(
            &serde_json::to_vec(&response)?,
            1,
            false,
            true,
        )?);
        ensure(
            self.responses.len() <= MAX_MESSAGE,
            "native refusal buffer exceeds bound",
        )
    }
}

fn selected_runtime(message: &J, runtime: &J) -> Result<()> {
    let params = &message["params"];
    if runtime["recovery_role"] == true || runtime["publication_role"] == true {
        readonly_recovery(params)?;
    }
    for (key, expected) in [
        ("cwd", &runtime["cwd"]),
        ("model", &runtime["model"]["id"]),
        ("modelProvider", &runtime["model"]["providerID"]),
        ("effort", &runtime["model"]["reasoning_effort"]),
    ] {
        same_selection(&params[key], expected)?;
    }
    let collaboration = &params["collaborationMode"]["settings"];
    same_selection(&collaboration["model"], &runtime["model"]["id"])?;
    same_selection(
        &collaboration["reasoning_effort"],
        &runtime["model"]["reasoning_effort"],
    )?;
    ensure(
        params["config"].is_null(),
        "native task configuration is fixed by the task runtime",
    )
}

fn readonly_recovery(params: &J) -> Result<()> {
    ensure(
        params["permissions"].is_null(),
        "recovery cannot switch permission profiles",
    )?;
    ensure(
        params["sandbox"].is_null() || params["sandbox"] == "read-only",
        "recovery cannot acquire a workspace writer through the TUI",
    )?;
    ensure(
        params["sandboxPolicy"].is_null()
            || params["sandboxPolicy"] == json!({"type":"readOnly","networkAccess":false}),
        "recovery sandbox is read-only; writing requires a separate authorized handoff",
    )
}

fn same_selection(actual: &J, expected: &J) -> Result<()> {
    ensure(
        actual.is_null() || expected.is_null() || actual == expected,
        "native input cannot change the task cwd, model, provider or configured effort",
    )
}

fn allowed(client: bool, message: &J, session: &J) -> Result<bool> {
    let method = message["method"].as_str().unwrap_or_default();
    if !client {
        return Ok(method != "item/tool/call");
    }
    if let Some(thread) = message["params"].get("threadId") {
        ensure(
            thread.is_null() || thread == session,
            &format!("native TUI rejected {method}: foreign thread {}", thread),
        )?;
    }
    ensure(
        !matches!(
            method,
            "thread/start"
                | "thread/fork"
                | "thread/delete"
                | "thread/archive"
                | "thread/unarchive"
                | "thread/settings/update"
        ) && !method.starts_with("account/login")
            && !method.starts_with("account/logout")
            && !method.starts_with("config/value/write")
            && !method.starts_with("config/batchWrite"),
        &format!("task TUI rejected {method}: cannot create/switch sessions or change account/global/model configuration"),
    )?;
    Ok(true)
}

pub(super) struct Policy {
    binding: J,
    runtime: J,
    client: Direction,
    server: Direction,
    reported: bool,
    drain: Option<Rc<RefCell<drain::Drain>>>,
    deferred: [Vec<u8>; 2],
    forwarding: [Option<crate::lifecycle::Permit>; 2],
    output_pending: [bool; 2],
}
impl Policy {
    pub fn new(binding: J, runtime: J) -> Self {
        let mut client = Direction::new(true);
        client.selection = runtime.clone();
        // Production attachments always carry their validated runtime binding.
        let drain = binding
            .get("runtime")
            .map(|_| Rc::new(RefCell::new(drain::Drain::new(binding.clone()))));
        client.drain = drain.clone();
        let mut server = Direction::new(false);
        server.drain = drain.clone();
        Self {
            binding,
            runtime,
            client,
            server,
            reported: false,
            drain,
            deferred: [vec![], vec![]],
            forwarding: [None, None],
            output_pending: [false, false],
        }
    }

    fn forward_permitted(&mut self, client: bool, data: &[u8]) -> Result<Vec<u8>> {
        let index = usize::from(!client);
        self.deferred[index].extend_from_slice(data);
        ensure(
            self.deferred[index].len()
                <= if client {
                    MAX_MESSAGE
                } else {
                    NATIVE_MESSAGE_LIMIT
                } + MAX_HEADERS,
            "native deferred input exceeds bound",
        )?;
        if self.deferred[index].is_empty() {
            return Ok(vec![]);
        }
        let root = self.drain.as_ref().map(|d| d.borrow().root()).transpose()?;
        let permit = match root
            .as_deref()
            .map(crate::lifecycle::runtime_observer_permit)
            .transpose()
        {
            Ok(permit) => permit,
            Err(error) if error.kind == crate::ErrorKind::StateBusy => return Ok(vec![]),
            Err(error) => return Err(error),
        };
        let forwarded = self.forward_buffered(client, index)?;
        self.retain_forwarding(index, permit.flatten(), &forwarded);
        Ok(forwarded)
    }

    fn retain_forwarding(
        &mut self,
        index: usize,
        permit: Option<crate::lifecycle::Permit>,
        bytes: &[u8],
    ) {
        // Both the parser and the endpoint can retain admitted bytes. A flush
        // of older output cannot release a partial later frame's admission.
        self.output_pending[index] |= !bytes.is_empty();
        if self.forwarding[index].is_none() {
            self.forwarding[index] = permit;
        }
        self.release_if_drained(index);
    }

    fn release_if_drained(&mut self, index: usize) {
        let direction = if index == 0 {
            &self.client
        } else {
            &self.server
        };
        if !self.output_pending[index] && !direction.buffered() {
            self.forwarding[index] = None;
        }
    }

    fn discard_buffers(&mut self) {
        self.client = Direction::new(true);
        self.client.selection = self.runtime.clone();
        self.server = Direction::new(false);
        self.client.drain = self.drain.clone();
        self.server.drain = self.drain.clone();
        self.deferred = [vec![], vec![]];
        self.output_pending = [false, false];
        self.forwarding = [None, None];
    }

    fn forward_buffered(&mut self, client: bool, index: usize) -> Result<Vec<u8>> {
        if let Some(drain) = &self.drain {
            drain.borrow_mut().initialize()?;
        }
        if client && self.client.headers.is_some() {
            upstream::endpoint(&self.runtime)?;
        }
        let data = std::mem::take(&mut self.deferred[index]);
        if client {
            self.client.feed(&data, &self.binding["session"])
        } else {
            self.server.feed(&data, &self.binding["session"])
        }
    }
}
impl crate::codex::tui::BridgePolicy for Policy {
    fn forwarding_drained(&mut self, client: bool) {
        let index = usize::from(!client);
        self.output_pending[index] = false;
        self.release_if_drained(index);
    }
    fn forwarding_discarded(&mut self) {
        self.discard_buffers();
    }
    fn message_limit(&self) -> usize {
        NATIVE_MESSAGE_LIMIT
    }
    fn take_responses(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.client.responses)
    }
    fn connected(&mut self) {
        self.discard_buffers();
        if let Some(drain) = &self.drain {
            drain.borrow_mut().connected();
        }
    }
    fn explicit_cwd(&self) -> bool {
        // `resume` restores the existing thread cwd. Supplying --cd asks the
        // native remote frontend to onboard a new directory (and persist trust).
        // Never fabricate a trust decision or write the user's project config.
        false
    }
    fn forward(&mut self, client: bool, data: &[u8]) -> Result<Vec<u8>> {
        let result = self.forward_permitted(client, data);
        if let Err(error) = &result {
            let path = Path::new(self.binding["directory"].as_str().unwrap_or_default())
                .join("transport-error.json");
            if !path.exists() {
                let _ = util::atomic_write_json(
                    &path,
                    &json!({"error":error.to_string(),"client":client}),
                );
            }
        }
        result
    }
    fn observe(&mut self, marker: &J) -> Result<()> {
        if marker["status"] != "ready" {
            return Ok(());
        }
        verify_marker(&self.binding, &self.runtime, marker)?;
        if !self.reported {
            herdr::report(&self.binding)?;
            self.reported = true;
        }
        Ok(())
    }
    fn arguments(&self) -> Vec<String> {
        let mut args = vec!["-c".into(), "history.persistence=\"none\"".into()];
        if let Some(hooks) = self.runtime["native_tui_hooks"].as_object() {
            for key in hooks.keys() {
                args.extend([
                    "-c".into(),
                    format!("hooks.state.{}.enabled=false", json!(key)),
                ]);
            }
        }
        args
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codex::rpc::decode_message;
    use crate::codex::tui::BridgePolicy;
    #[test]
    fn policy_filters_server_tools_and_preserves_first_transport_error() {
        let directory = util::fs::test_tempdir();
        let mut policy = Policy::new(
            json!({"session":"owned","directory":directory.path()}),
            json!({"native_tui_hooks":{"reporter":{}}}),
        );
        assert!(policy
            .arguments()
            .iter()
            .any(|arg| arg.contains("reporter")));
        policy.forward(false, b"HTTP/1.1 101 OK\r\n\r\n").unwrap();
        let tool = json!({"id":1,"method":"item/tool/call","params":{"threadId":"owned"}});
        let frame = encode_frame(&serde_json::to_vec(&tool).unwrap(), 1, false, true).unwrap();
        assert!(policy.forward(false, &frame).unwrap().is_empty());
        assert!(policy.forward(false, &[0x83, 0]).is_err());
        let error = util::read_json(&directory.path().join("transport-error.json")).unwrap();
        assert_eq!(error["client"], false);
        assert!(policy.forward(false, &[0x83, 0]).is_err());
        assert_eq!(
            util::read_json(&directory.path().join("transport-error.json")).unwrap(),
            error
        );
    }
    #[test]
    fn native_input_preserves_the_role_selection_before_forwarding() {
        let runtime = json!({"cwd":"/task","model":{"id":"chosen","providerID":"upstream","reasoning_effort":"high"}});
        let valid = json!({"params":{"threadId":"owned","cwd":"/task","model":"chosen","effort":"high","collaborationMode":{"settings":{"model":"chosen","reasoning_effort":"high"}}}});
        selected_runtime(&valid, &runtime).unwrap();
        for key in ["cwd", "model", "modelProvider", "effort"] {
            let mut request = valid.clone();
            request["params"][key] = json!("different");
            assert!(selected_runtime(&request, &runtime).is_err());
        }
        let mut request = valid;
        request["params"]["collaborationMode"]["settings"]["model"] = json!("other");
        assert!(selected_runtime(&request, &runtime).is_err());
    }
    #[test]
    fn recovery_input_keeps_its_readonly_role() {
        let runtime = json!({"recovery_role":true});
        selected_runtime(&json!({"params":{}}), &runtime).unwrap();
        selected_runtime(
            &json!({"params":{"sandbox":"read-only",
            "sandboxPolicy":{"type":"readOnly","networkAccess":false}}}),
            &runtime,
        )
        .unwrap();
        for params in [
            json!({"sandbox":"workspace-write"}),
            json!({"sandboxPolicy":{"type":"workspaceWrite","writableRoots":["/task"]}}),
            json!({"sandboxPolicy":{"type":"readOnly","networkAccess":true}}),
        ] {
            assert!(selected_runtime(&json!({"params":params}), &runtime).is_err());
        }
    }
    #[test]
    fn refusal_keeps_connection_usable_for_owned_turn() {
        let mut direction = Direction::new(true);
        direction.headers = None;
        let denied = json!({"id":4,"method":"thread/start","params":{}});
        let allowed = json!({"id":5,"method":"turn/start","params":{"threadId":"owned"}});
        let mut input = encode_frame(&serde_json::to_vec(&denied).unwrap(), 1, true, true).unwrap();
        input.extend(encode_frame(&serde_json::to_vec(&allowed).unwrap(), 1, true, true).unwrap());
        let forwarded = direction.feed(&input, &json!("owned")).unwrap();
        let frames = FrameDecoder::new(true, MAX_MESSAGE)
            .feed(&forwarded)
            .unwrap();
        assert_eq!(frames.len(), 1);
        assert_eq!(decode_message(&frames[0].1).unwrap(), allowed);
        let replies = FrameDecoder::new(false, MAX_MESSAGE)
            .feed(&direction.responses)
            .unwrap();
        let reply = decode_message(&replies[0].1).unwrap();
        assert_eq!(reply["id"], 4);
        assert_eq!(reply["error"]["code"], -32600);
    }

    #[test]
    fn recursive_native_catalog_preserves_owned_resume_and_unknown_server_requests() {
        let catalog = super::super::catalog_fixture::catalog();
        let payload = serde_json::to_vec(&catalog).unwrap();
        assert!(decode_message(&payload).is_err());
        let frame = encode_frame(&payload, 1, false, true).unwrap();
        let mut server = Direction::new(false);
        server.headers = None;
        assert_eq!(server.feed(&frame, &json!("owned")).unwrap(), frame);
        let unknown = json!({"id":92,"method":"future/request","params":{"schema":catalog}});
        let frame = encode_frame(&serde_json::to_vec(&unknown).unwrap(), 1, false, true).unwrap();
        assert_eq!(server.feed(&frame, &json!("owned")).unwrap(), frame);
        let mut client = Direction::new(true);
        client.headers = None;
        let resume = json!({"id":93,"method":"thread/resume","params":{"threadId":"owned"}});
        let frame = encode_frame(&serde_json::to_vec(&resume).unwrap(), 1, true, true).unwrap();
        let forwarded = client.feed(&frame, &json!("owned")).unwrap();
        let decoded = FrameDecoder::new(true, MAX_MESSAGE)
            .feed(&forwarded)
            .unwrap();
        assert_eq!(decode_message(&decoded[0].1).unwrap(), resume);
        let foreign = json!({"id":94,"method":"thread/resume","params":{"threadId":"foreign"}});
        let frame = encode_frame(&serde_json::to_vec(&foreign).unwrap(), 1, true, true).unwrap();
        assert!(client.feed(&frame, &json!("owned")).unwrap().is_empty());
        assert!(!client.responses.is_empty());
    }

    #[test]
    fn native_direction_depth_limits_keep_client_bound() {
        for (client, depth, accepted) in [
            (false, 64, true),
            (false, 65, false),
            (true, 10, true),
            (true, 11, false),
        ] {
            let payload =
                serde_json::to_vec(&super::super::catalog_fixture::nested_message(depth)).unwrap();
            let frame = encode_frame(&payload, 1, client, true).unwrap();
            let mut direction = Direction::new(client);
            direction.headers = None;
            assert_eq!(direction.feed(&frame, &json!("owned")).is_ok(), accepted);
        }
    }

    #[test]
    fn native_catalog_can_exceed_broker_limit_but_remains_bounded() {
        let payload =
            serde_json::to_vec(&json!({"id":7,"result":"x".repeat(MAX_MESSAGE)})).unwrap();
        assert!(decode_message(&payload).is_err());
        assert!(encode_frame(&payload, 1, false, true).is_err());
        let frame =
            encode_frame_with_limit(&payload, 1, false, true, NATIVE_MESSAGE_LIMIT).unwrap();
        let mut direction = Direction::new(false);
        direction.headers = None;
        let forwarded = direction.feed(&frame, &json!("owned")).unwrap();
        assert_eq!(forwarded, frame);
        let mut over_limit = vec![0x81, 127];
        over_limit.extend_from_slice(&((NATIVE_MESSAGE_LIMIT + 1) as u64).to_be_bytes());
        assert!(direction.feed(&over_limit, &json!("owned")).is_err());
    }

    #[test]
    fn native_schema_depth_does_not_authorize_task_session_creation() {
        let request = super::super::catalog_fixture::tool_start(12, true);
        let frame = encode_frame(&serde_json::to_vec(&request).unwrap(), 1, true, true).unwrap();
        let mut client = Direction::new(true);
        client.headers = None;
        assert!(client.feed(&frame, &json!("owned")).unwrap().is_empty());
        let reply = FrameDecoder::new(false, MAX_MESSAGE)
            .feed(&client.responses)
            .unwrap();
        assert_eq!(
            decode_message(&reply[0].1).unwrap()["error"]["code"],
            -32600
        );
    }

    #[test]
    fn native_client_oversize_request_is_rejected_from_header() {
        let mut client = Direction::new(true);
        client.headers = None;
        let mut frame = vec![0x81, 0xff];
        frame.extend_from_slice(&((MAX_MESSAGE + 1) as u64).to_be_bytes());
        assert!(client.feed(&frame, &json!("owned")).is_err());
    }

    #[test]
    fn split_frames_preserve_approvals_and_responses_without_racing_tools() {
        let session = json!("owned");
        for client in [false, true] {
            let mut direction = Direction::new(client);
            let header = if client {
                b"GET / HTTP/1.1\r\n\r\n".as_slice()
            } else {
                b"HTTP/1.1 101 OK\r\n\r\n".as_slice()
            };
            assert_eq!(direction.feed(header, &session).unwrap(), header);
            let approval = json!({"id":42,"method":"item/tool/requestUserInput","params":{"threadId":"owned","questions":[]}});
            let response = json!({"id":42,"result":{"answers":{}}});
            let message = if client { response } else { approval };
            let bytes =
                encode_frame(&serde_json::to_vec(&message).unwrap(), 1, client, true).unwrap();
            let mut output = Vec::new();
            for byte in bytes {
                output.extend(direction.feed(&[byte], &session).unwrap());
            }
            let frames = FrameDecoder::new(client, MAX_MESSAGE)
                .feed(&output)
                .unwrap();
            assert_eq!(decode_message(&frames[0].1).unwrap(), message);
        }
        assert!(!allowed(false, &json!({"id":3,"method":"item/tool/call"}), &session).unwrap());
        for method in [
            "thread/start",
            "thread/fork",
            "account/login/start",
            "config/batchWrite",
        ] {
            assert!(allowed(true, &json!({"method":method}), &session).is_err());
        }
        assert!(allowed(
            true,
            &json!({"method":"turn/start","params":{"threadId":"foreign"}}),
            &session
        )
        .is_err());
        assert!(allowed(
            true,
            &json!({"method":"app/list","params":{"threadId":null}}),
            &session
        )
        .unwrap());
        assert!(allowed(
            true,
            &json!({"method":"thread/read","params":{"threadId":"catalog-preview"}}),
            &session
        )
        .is_err());
    }
}
