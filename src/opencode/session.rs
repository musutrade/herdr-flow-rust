//! OpenCode 1.19.3 native HTTP session binding and durable submissions.
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::error::{ErrorKind, FlowError, Result};
use crate::herdr::ReceiverAdapter;
use crate::model::{self, J};
use crate::util;
use crate::util::pyjson::{sha256_hex, uuid_hex};

pub use super::http::Api;
use super::http::{required, HttpApi};

mod cli;
mod producer;
pub use cli::cli;
pub use producer::{collect, dispatch};

pub const VERSION: &str = "1.19.3";

pub fn is_registered(state: &J) -> bool {
    model::string(state, "adapter").as_deref() == Some("opencode")
}

pub struct SessionAdapter {
    pub state: J,
    api: Box<dyn Api>,
}

impl SessionAdapter {
    pub fn new(state: &J) -> Result<Self> {
        Self::with_api(state, Box::new(HttpApi::from_state(state)?))
    }

    pub fn with_api(state: &J, api: Box<dyn Api>) -> Result<Self> {
        validate_binding(state)?;
        Ok(Self {
            state: state.clone(),
            api,
        })
    }

    fn session(&self) -> &str {
        self.state["session"].as_str().unwrap_or_default()
    }

    fn root(&self) -> PathBuf {
        PathBuf::from(self.state["state_dir"].as_str().unwrap_or_default())
    }

    fn route(&self, suffix: &str) -> String {
        format!("/session/{}{suffix}", self.session())
    }

    pub fn verify_binding(&self) -> Result<J> {
        let health = self.api.get("/global/health")?;
        ensure(
            health["healthy"] == true && health["version"] == VERSION,
            "OpenCode server must be healthy and exactly 1.19.3; 2.0 integration is deferred",
        )?;
        let session = self.api.get(&self.route(""))?;
        ensure(
            session["id"] == self.state["session"],
            "OpenCode session identity changed",
        )?;
        ensure(
            session["directory"] == self.state["cwd"],
            "OpenCode session directory changed",
        )?;
        ensure(
            same_model(&session["model"], &self.state["model"]),
            "OpenCode session model changed or cannot be verified",
        )?;
        Ok(session)
    }

    fn native_status(&self) -> Result<String> {
        let statuses = self.api.get("/session/status")?;
        ensure(
            statuses.is_object(),
            "invalid OpenCode session status response",
        )?;
        // 1.19.3 omits idle sessions from this map; verify_binding establishes
        // existence first. Pending requests and submissions are checked too.
        match statuses.get(self.session()) {
            Some(value) => required(value, "type"),
            None => Ok("idle".into()),
        }
    }

    fn pending_requests(&self) -> Result<bool> {
        let permissions = self.api.get("/permission")?;
        let questions = self.api.get("/question")?;
        Ok(pending_for(&permissions, self.session())? || pending_for(&questions, self.session())?)
    }

    fn live_state(&self) -> Result<String> {
        self.verify_binding()?;
        if self.pending_requests()? {
            return Ok("blocked".into());
        }
        self.native_status()
    }

    pub fn status(&self) -> Result<J> {
        let live = self.live_state()?;
        let submission = load_submission(&self.root())?;
        let available = live == "idle" && submission_allows_next(submission.as_ref());
        let label = agent_label(&live, available);
        Ok(
            json!({"agent_session":{"value":self.session()},"agent_status":label,
            "interactive_ready":available,"native_status":live,"submission":submission,
            "context_telemetry":false,"safe_rotation":false,"server_version":VERSION}),
        )
    }

    pub fn submit(&self, text: &str, context: Option<J>, timeout: f64) -> Result<String> {
        validate_submission(text, timeout)?;
        let root = self.root();
        let _lock = submission_lock(&root)?;
        self.reconcile_locked()?;
        ensure(
            self.status()?["interactive_ready"] == true,
            "OpenCode session is busy, blocked or has an unresolved submission",
        )?;
        let id = format!("msg{}", uuid_hex());
        let text = receiver_instructions(text, self.session());
        let body = submission_body(&id, &text, &self.state["model"]);
        let mut journal = json!({"message_id":id,"session":self.session(),"model":self.state["model"],"cwd":self.state["cwd"],
            "prompt_sha256":sha256_hex(text.as_bytes()),"phase":"sending","created_at":util::now(),
            "deadline":util::now()+timeout,"context":context});
        save_submission(&root, &journal)?;
        // Exactly one non-idempotent request. An error leaves the sending
        // journal intact for inspection; absence from history never authorizes replay.
        self.api.post(&self.route("/prompt_async"), &body)?;
        journal["phase"] = json!("submitted");
        save_submission(&root, &journal)?;
        Ok(id)
    }

    pub fn reconcile(&self) -> Result<Option<J>> {
        let _lock = submission_lock(&self.root())?;
        self.reconcile_locked()
    }

    fn reconcile_locked(&self) -> Result<Option<J>> {
        let Some(mut journal) = load_submission(&self.root())? else {
            return Ok(None);
        };
        if terminal(&journal) {
            return Ok(Some(journal));
        }
        verify_journal_binding(&journal, &self.state)?;
        let live = self.live_state()?;
        let messages = self.api.get(&self.route("/message?limit=200"))?;
        let rows = messages
            .as_array()
            .ok_or_else(|| FlowError::guard("invalid OpenCode message history"))?;
        update_submission(&mut journal, rows, &live, &self.state)?;
        save_submission(&self.root(), &journal)?;
        Ok(Some(journal))
    }
}

impl ReceiverAdapter for SessionAdapter {
    fn agent(&self) -> Result<J> {
        self.status()
    }
    fn prompt(&self, text: &str) -> Result<String> {
        self.submit(text, None, 600.0)
    }
    fn call(&self, _args: &[String]) -> Result<J> {
        Err(FlowError::guard(
            "OpenCode receiver does not expose Codex/Herdr command control",
        ))
    }
}

fn ensure(condition: bool, message: &str) -> Result<()> {
    if !condition {
        return Err(FlowError::guard(message));
    }
    Ok(())
}

fn same_model(a: &J, b: &J) -> bool {
    let Some(variant) = model_variant(a) else {
        return false;
    };
    ["providerID", "id"]
        .iter()
        .all(|key| a.get(*key) == b.get(*key))
        && Some(variant) == model_variant(b)
}

fn model_variant(model: &J) -> Option<&str> {
    match model.get("variant") {
        None | Some(Value::Null) => Some("default"),
        Some(value) => value.as_str().filter(|value| !value.is_empty()),
    }
}

fn validate_binding(state: &J) -> Result<()> {
    ensure(
        is_registered(state) && state["server_version"] == VERSION,
        "unsupported OpenCode registration/version",
    )?;
    ensure(
        state["version"] == 2 && !model::bool_or(state, "auto_rotate", false),
        "OpenCode automatic rotation is unsupported",
    )?;
    validate_session(&required(state, "session")?)?;
    validate_binding_fields(state)
}

fn validate_binding_fields(state: &J) -> Result<()> {
    required(state, "state_dir")?;
    let cwd = required(state, "cwd")?;
    ensure(
        Path::new(&cwd).is_absolute(),
        "OpenCode cwd must be absolute",
    )?;
    required(&state["model"], "providerID")?;
    required(&state["model"], "id")?;
    ensure(
        model_variant(&state["model"]).is_some(),
        "invalid OpenCode model variant",
    )?;
    Ok(())
}

fn validate_session(session: &str) -> Result<()> {
    ensure(
        session.starts_with("ses")
            && session.len() <= 128
            && session
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_'),
        "invalid OpenCode session id",
    )
}

fn pending_for(rows: &J, session: &str) -> Result<bool> {
    let rows = rows
        .as_array()
        .ok_or_else(|| FlowError::guard("invalid OpenCode pending request response"))?;
    ensure(
        rows.iter()
            .all(|row| row["sessionID"].as_str().is_some_and(|id| !id.is_empty())),
        "OpenCode pending request identity missing",
    )?;
    Ok(rows.iter().any(|row| row["sessionID"] == session))
}

fn agent_label(live: &str, available: bool) -> &'static str {
    match live {
        "blocked" => "blocked",
        "busy" | "retry" => "working",
        "idle" if available => "idle",
        _ => "unknown",
    }
}

fn terminal(journal: &J) -> bool {
    matches!(
        journal["phase"].as_str(),
        Some("complete" | "failed" | "decision" | "signalled")
    )
}

fn submission_allows_next(journal: Option<&J>) -> bool {
    let Some(journal) = journal else {
        return true;
    };
    if journal["phase"] == "signalled" {
        return true;
    }
    terminal(journal) && journal["context"].is_null()
}

fn submission_lock(root: &Path) -> Result<util::FileLock> {
    util::ensure_private_dir(root, false)?;
    util::FileLock::acquire(&root.join("submission.lock"), false)?
        .ok_or_else(|| FlowError::new(ErrorKind::StateBusy, "OpenCode submission is busy"))
}

fn load_submission(root: &Path) -> Result<Option<J>> {
    let path = root.join("submission.json");
    if !path.exists() {
        return Ok(None);
    }
    util::read_state_json(root, "submission.json").map(Some)
}

fn save_submission(root: &Path, journal: &J) -> Result<()> {
    util::atomic_write_json(&root.join("submission.json"), journal)?;
    let id = required(journal, "message_id")?;
    ensure(
        id.starts_with("msg") && id.bytes().all(|b| b.is_ascii_alphanumeric()),
        "invalid submission message id",
    )?;
    let reports = root.join("submissions");
    util::ensure_private_dir(&reports, true)?;
    util::atomic_write_json(&reports.join(format!("{id}.json")), journal)
}

fn receiver_instructions(text: &str, session: &str) -> String {
    format!("{text}\n\nFor this OpenCode receiver, set OPENCODE_SESSION_ID={session} when invoking herdr-flow queue ack, readiness or task commands. This session is not a CODEX_THREAD_ID. A received prompt alone is not task success.")
}

fn update_submission(journal: &mut J, rows: &[J], live: &str, state: &J) -> Result<()> {
    let id = required(journal, "message_id")?;
    let user = rows.iter().find(|row| row["info"]["id"] == id);
    if let Some(user) = user {
        verify_user(user, journal, state)?;
        journal["phase"] = json!("running");
        if live == "idle" {
            finish_submission(journal, rows, state)?;
        }
    }
    mark_deadline(journal, live);
    journal["observed_at"] = json!(util::now());
    Ok(())
}

fn verify_user(row: &J, journal: &J, state: &J) -> Result<()> {
    let info = &row["info"];
    verify_variant(&info["model"], &state["model"])?;
    ensure(
        info["role"] == "user" && info["sessionID"] == state["session"],
        "OpenCode user message identity mismatch",
    )?;
    ensure(
        info["model"]["providerID"] == state["model"]["providerID"]
            && info["model"]["modelID"] == state["model"]["id"],
        "OpenCode submitted model mismatch",
    )?;
    ensure(
        sha256_hex(message_text(row).as_bytes()) == journal["prompt_sha256"],
        "OpenCode submitted prompt mismatch",
    )
}

fn message_text(row: &J) -> String {
    row["parts"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|part| part["type"] == "text")
        .filter_map(|part| part["text"].as_str())
        .collect::<Vec<_>>()
        .join("")
}

fn finish_submission(journal: &mut J, rows: &[J], state: &J) -> Result<()> {
    let reply = rows.iter().rev().find(|row| {
        row["info"]["role"] == "assistant" && row["info"]["parentID"] == journal["message_id"]
    });
    let Some(reply) = reply else {
        return Ok(());
    };
    let info = &reply["info"];
    if info["time"]["completed"].is_null() {
        return Ok(());
    }
    verify_reply(info, state)?;
    let outcome = reply_outcome(info);
    journal["phase"] = json!(outcome);
    journal["reply_id"] = info["id"].clone();
    journal["report"] = json!(message_text(reply));
    journal["error"] = info["error"].clone();
    Ok(())
}

fn verify_reply(info: &J, state: &J) -> Result<()> {
    verify_variant(info, &state["model"])?;
    ensure(
        info["sessionID"] == state["session"] && info["path"]["cwd"] == state["cwd"],
        "OpenCode reply session/directory mismatch",
    )?;
    ensure(
        info["providerID"] == state["model"]["providerID"]
            && info["modelID"] == state["model"]["id"],
        "OpenCode reply model mismatch",
    )
}

fn reply_outcome(info: &J) -> &'static str {
    if !info["error"].is_null() {
        return "failed";
    }
    if info["finish"] == "stop" {
        return "complete";
    }
    "decision"
}

fn mark_deadline(journal: &mut J, live: &str) {
    if terminal(journal) {
        return;
    }
    if util::now() >= model::number(journal, "deadline").unwrap_or(0.0) {
        journal["phase"] = json!("needs_attention");
        journal["reason"] = json!(
            "submission deadline expired; reconcile exact messages before any further action"
        );
    }
    journal["native_status"] = json!(live);
}

pub fn receiver_session(root: &Path) -> Result<Option<String>> {
    let queue = crate::queue::load_queue(root)?;
    let Some(guard_root) = model::string(&queue, "guard_dir") else {
        return Ok(std::env::var("CODEX_THREAD_ID").ok());
    };
    let state = util::read_state_json(Path::new(&guard_root), "state.json")?;
    if !is_registered(&state) {
        return Ok(std::env::var("CODEX_THREAD_ID").ok());
    }
    let session = std::env::var("OPENCODE_SESSION_ID").ok();
    if let Some(session) = &session {
        ensure(
            state["session"] == session.as_str(),
            "OpenCode receiver session mismatch",
        )?;
    }
    Ok(session)
}

pub fn register(root: &Path, spec: &J) -> Result<J> {
    let (root, cwd) = registration_paths(root, spec)?;
    let _lock = crate::guard::locked(&root)?;
    ensure(
        !root.join("state.json").exists(),
        "OpenCode registration already exists",
    )?;
    let mut state = spec.clone();
    ensure(state.is_object(), "OpenCode registration must be an object")?;
    state["version"] = json!(2);
    state["adapter"] = json!("opencode");
    state["server_version"] = json!(VERSION);
    state["state_dir"] = json!(root);
    state["cwd"] = json!(cwd);
    state["phase"] = json!("monitoring");
    state["auto_rotate"] = json!(false);
    state["level"] = json!("unknown");
    state["usage"] = Value::Null;
    SessionAdapter::new(&state)?.verify_binding()?;
    crate::guard::save(&root, &mut state)?;
    Ok(state)
}

fn registration_paths(root: &Path, spec: &J) -> Result<(PathBuf, PathBuf)> {
    let cwd = util::resolve_path(Path::new(&required(spec, "cwd")?));
    ensure(cwd.is_dir(), "OpenCode cwd must be an existing directory")?;
    let root = util::resolve_path(root);
    ensure(
        !util::inside_checkout(&root, &cwd) && !util::is_relative_to(&root, &cwd),
        "keep OpenCode state outside its workspace",
    )?;
    Ok((root, cwd))
}

fn validate_submission(text: &str, timeout: f64) -> Result<()> {
    ensure(
        !text.is_empty() && text.len() <= 1024 * 1024,
        "OpenCode prompt must be 1..1048576 bytes",
    )?;
    ensure(
        timeout.is_finite() && (1.0..=86400.0).contains(&timeout),
        "invalid OpenCode submission timeout",
    )
}

fn verify_journal_binding(journal: &J, state: &J) -> Result<()> {
    ensure(
        journal["session"] == state["session"]
            && journal["cwd"] == state["cwd"]
            && journal["model"] == state["model"],
        "submission belongs to a different OpenCode binding",
    )
}

fn submission_body(id: &str, text: &str, model: &J) -> J {
    let mut body = json!({"messageID":id,"model":{"providerID":model["providerID"],"modelID":model["id"]},"parts":[{"type":"text","text":text}]});
    if let Some(variant) = model["variant"].as_str() {
        body["variant"] = json!(variant);
    }
    body
}

fn verify_variant(actual: &J, model: &J) -> Result<()> {
    ensure(
        model_variant(actual).is_some() && model_variant(actual) == model_variant(model),
        "OpenCode model variant mismatch",
    )
}
