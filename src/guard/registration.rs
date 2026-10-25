//! Runtime identity binding and initial guard registration.

use super::{guard_error, save, transcript_state};
use crate::error::FlowError;
use crate::error::Result;
use crate::herdr::{GuardAdapter, Herdr};
use crate::model;
use crate::model::J;
use crate::util;
use crate::util::process::command;
use crate::util::pyjson::digest;
use serde_json::json;
use serde_json::Value;
use std::fs::File;
use std::fs::OpenOptions;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;

pub(super) fn bind_runtime(
    s: &mut J,
    root: &Path,
    app_socket: Option<&str>,
    codex_binary: Option<&str>,
) -> Result<J> {
    let daemon = bind_daemon()?;
    bind_runtime_daemon(s, root, app_socket, codex_binary, daemon)
}

fn bind_daemon() -> Result<J> {
    let raw = command(
        &[
            "codex".to_string(),
            "app-server".to_string(),
            "daemon".to_string(),
            "version".to_string(),
        ],
        None,
        Duration::from_secs(15),
        false,
    )?;
    serde_json::from_slice(&raw)
        .map_err(|error| guard_error(format!("invalid app-server daemon response: {error}")))
}

fn bind_runtime_daemon(
    s: &mut J,
    root: &Path,
    app_socket: Option<&str>,
    codex_binary: Option<&str>,
    daemon: J,
) -> Result<J> {
    if model::string(&daemon, "status").as_deref() != Some("running") {
        return Err(guard_error(
            "a running local app-server is required; no automatic server startup",
        ));
    }
    bind_runtime_paths(s, root, app_socket, codex_binary, &daemon)
}

fn bind_runtime_paths(
    s: &mut J,
    root: &Path,
    app_socket: Option<&str>,
    codex_binary: Option<&str>,
    daemon: &J,
) -> Result<J> {
    let endpoint = app_socket
        .map(ToString::to_string)
        .or_else(|| model::string(daemon, "socketPath"))
        .unwrap_or_default();
    let binary = codex_binary
        .map(ToString::to_string)
        .or_else(|| model::string(daemon, "managedCodexPath"))
        .unwrap_or_default();
    if !bind_paths_ok(&endpoint, &binary) {
        return Err(guard_error(
            "explicit absolute app-server socket and Codex binary required",
        ));
    }
    bind_runtime_version(s, root, &endpoint, &binary, daemon)
}

fn bind_paths_ok(endpoint: &str, binary: &str) -> bool {
    [
        !endpoint.is_empty(),
        !binary.is_empty(),
        Path::new(endpoint).is_absolute(),
        Path::new(binary).is_absolute(),
    ]
    .into_iter()
    .all(|ok| ok)
}

fn bind_runtime_version(
    s: &mut J,
    root: &Path,
    endpoint: &str,
    binary: &str,
    daemon: &J,
) -> Result<J> {
    let actual_version = bind_binary_version(binary)?;
    bind_runtime_version_check(s, root, endpoint, binary, daemon, actual_version)
}

fn bind_binary_version(binary: &str) -> Result<String> {
    let raw = command(
        &[binary.to_string(), "--version".to_string()],
        None,
        Duration::from_secs(15),
        false,
    )?;
    Ok(String::from_utf8_lossy(&raw)
        .split_whitespace()
        .last()
        .unwrap_or_default()
        .to_string())
}

fn bind_runtime_version_check(
    s: &mut J,
    root: &Path,
    endpoint: &str,
    binary: &str,
    daemon: &J,
    actual_version: String,
) -> Result<J> {
    if Some(actual_version.as_str()) != model::string(daemon, "appServerVersion").as_deref() {
        return Err(guard_error(
            "TUI binary must match the running app-server version",
        ));
    }
    bind_runtime_register(s, root, endpoint, binary, actual_version)
}

fn bind_runtime_register(
    s: &mut J,
    root: &Path,
    endpoint: &str,
    binary: &str,
    actual_version: String,
) -> Result<J> {
    s["version"] = json!(2);
    s["state_dir"] = json!(util::resolve_path(root).display().to_string());
    s["app_server_socket"] = json!(endpoint);
    s["codex_binary"] = json!(util::resolve_path(Path::new(binary)).display().to_string());
    s["app_server_version"] = json!(actual_version);
    let marker = bind_marker(root)?;
    bind_runtime_marker(s, marker)
}

fn bind_marker(root: &Path) -> Result<J> {
    util::read_state_json(root, "tui.json")
}

fn bind_runtime_marker(s: &mut J, marker: J) -> Result<J> {
    if bind_marker_bad(s, &marker) {
        return Err(guard_error(
            "registration requires a ready native TUI bridge for this exact thread",
        ));
    }
    s["bridge_nonce"] = marker.get("nonce").cloned().unwrap_or(Value::Null);
    bind_runtime_connect(s, &marker)
}

fn bind_marker_bad(s: &J, marker: &J) -> bool {
    [
        model::string(marker, "status").as_deref() != Some("ready"),
        model::string(marker, "session") != model::string(s, "session"),
        model::string(marker, "server_socket") != model::string(s, "app_server_socket"),
        !model::truthy(model::get(marker, "nonce")),
    ]
    .into_iter()
    .any(|bad| bad)
}

fn bind_runtime_connect(s: &mut J, marker: &J) -> Result<J> {
    let mut h = Herdr::new(s)?;
    bind_runtime_processes(s, &mut h, marker)
}

fn bind_runtime_processes(s: &mut J, h: &mut Herdr, marker: &J) -> Result<J> {
    let processes = h.processes()?;
    bind_runtime_identity(s, h, marker, &processes)
}

fn bind_runtime_identity(s: &mut J, h: &mut Herdr, marker: &J, processes: &J) -> Result<J> {
    s["shell_pid"] = processes.get("shell_pid").cloned().unwrap_or(Value::Null);
    let pid = model::u64_field(marker, "pid").unwrap_or(0) as u32;
    s["tui_process"] = util::process_identity(pid)?;
    bind_runtime_verify(s, h)
}

fn bind_runtime_verify(s: &mut J, h: &mut Herdr) -> Result<J> {
    h.set_state(s);
    let session = model::string(s, "session").unwrap_or_default();
    bind_runtime_verify_tui(s, h, &session)
}

fn bind_runtime_verify_tui(s: &mut J, h: &mut Herdr, session: &str) -> Result<J> {
    h.verify_tui(session)?;
    bind_runtime_agent(s, h)
}

fn bind_runtime_agent(s: &mut J, h: &mut Herdr) -> Result<J> {
    h.agent()?;
    bind_runtime_snapshot(s, h)
}

fn bind_runtime_snapshot(s: &mut J, h: &mut Herdr) -> Result<J> {
    h.snapshot(None)?;
    Ok(s.clone())
}

/// Registration identity, transcript and runtime settings for one guard.
pub struct InitRequest<'a> {
    pub root: &'a Path,
    pub pane: &'a str,
    pub session: &'a str,
    pub transcript_path: &'a Path,
    pub task: &'a str,
    pub epoch: i64,
    pub role: &'a str,
    pub sessions_dir: &'a Path,
    pub app_socket: Option<&'a str>,
    pub codex_binary: Option<&'a str>,
}

/// Validate and initialize a guard using an explicit registration request.
pub fn init(request: &InitRequest<'_>) -> Result<J> {
    let _permit = crate::lifecycle::runtime_permit(request.root)?;
    init_session(request)?;
    init_transcript(request)
}

fn init_session(request: &InitRequest<'_>) -> Result<()> {
    let session_ok = regex::Regex::new(r"^[0-9a-f-]{36}$")
        .expect("static session regex")
        .is_match(request.session);
    [session_ok, request.epoch >= 0]
        .into_iter()
        .all(|valid| valid)
        .then_some(())
        .ok_or_else(|| guard_error("invalid session or epoch"))
}

fn init_transcript(request: &InitRequest<'_>) -> Result<J> {
    let data = transcript_state(request.transcript_path, None)?;
    init_transcript_session(request, data)
}

fn init_transcript_session(request: &InitRequest<'_>, data: J) -> Result<J> {
    if model::string(&data, "session").as_deref() != Some(request.session) {
        return Err(guard_error("transcript session mismatch"));
    }
    init_root(request, data)
}

fn init_root(request: &InitRequest<'_>, data: J) -> Result<J> {
    let root = util::state_dir(request.root, true)?;
    if root.join("state.json").exists() {
        return Err(guard_error(
            "state exists; use it or choose a new state directory",
        ));
    }
    init_checkout(request, data, root)
}

fn init_checkout(request: &InitRequest<'_>, data: J, root: PathBuf) -> Result<J> {
    let cwd = model::string(&data, "cwd").unwrap_or_default();
    if util::inside_checkout(&root, Path::new(&cwd)) {
        return Err(guard_error("keep guard state outside the checkout"));
    }
    init_agent(request, root, cwd)
}

fn init_agent(request: &InitRequest<'_>, root: PathBuf, cwd: String) -> Result<J> {
    let raw = init_agent_raw(request.pane)?;
    init_agent_response(request, root, cwd, raw)
}

fn init_agent_raw(pane: &str) -> Result<Vec<u8>> {
    command(
        &[
            "herdr".to_string(),
            "agent".to_string(),
            "get".to_string(),
            pane.to_string(),
        ],
        None,
        Duration::from_secs(30),
        true,
    )
}

fn init_agent_response(
    request: &InitRequest<'_>,
    root: PathBuf,
    cwd: String,
    raw: Vec<u8>,
) -> Result<J> {
    let response: J = serde_json::from_slice(&raw)?;
    init_agent_parse(request, root, cwd, response)
}

fn init_agent_parse(
    request: &InitRequest<'_>,
    root: PathBuf,
    cwd: String,
    response: J,
) -> Result<J> {
    let agent = model::dig(&response, &["result", "agent"])
        .cloned()
        .ok_or_else(|| guard_error("Herdr agent response is incomplete"))?;
    init_state(request, root, cwd, agent)
}

fn init_state(request: &InitRequest<'_>, root: PathBuf, cwd: String, agent: J) -> Result<J> {
    let socket = std::env::var("HERDR_SOCKET_PATH").unwrap_or_default();
    let s = json!({
        "version": 1,
        "task": request.task,
        "epoch": request.epoch,
        "role": request.role,
        "pane": request.pane,
        "terminal": agent.get("terminal_id").cloned().unwrap_or(Value::Null),
        "session": request.session,
        "transcript": util::resolve_path(request.transcript_path).display().to_string(),
        "sessions_dir": util::resolve_path(request.sessions_dir).display().to_string(),
        "cwd": cwd,
        "socket": socket,
        "phase": "monitoring",
        "warn_tokens": 80000,
        "rotate_tokens": 120000,
        "warn_ratio": 0.60,
        "rotate_ratio": 0.75,
        "auto_rotate": false
    });
    if !model::truthy(model::get(&s, "socket")) {
        return Err(guard_error("explicit Herdr socket identity required"));
    }
    init_bind(request, s, root)
}

fn init_bind(request: &InitRequest<'_>, mut s: J, root: PathBuf) -> Result<J> {
    s = bind_runtime(&mut s, &root, request.app_socket, request.codex_binary)?;
    init_register(request, s, root)
}

fn init_register(request: &InitRequest<'_>, s: J, root: PathBuf) -> Result<J> {
    let registry = util::xdg_state_home().join("herdr-issue-flow/pane-guards");
    util::ensure_private_dir(&registry, true)?;
    init_binding(request, s, root, registry)
}

fn init_binding(
    request: &InitRequest<'_>,
    mut s: J,
    root: PathBuf,
    registry: PathBuf,
) -> Result<J> {
    let binding_path = init_binding_path(&registry, &s, request.pane);
    let registration = json!({
        "state_dir": root.display().to_string(),
        "terminal": s.get("terminal").cloned().unwrap_or(Value::Null)
    });
    init_write_binding(&binding_path, &registration)?;
    s["binding"] = json!(binding_path.display().to_string());
    init_save(root, s)
}

fn init_binding_path(registry: &Path, s: &J, pane: &str) -> PathBuf {
    registry.join(format!(
        "{}.json",
        digest(format!("{}:{pane}", model::string(s, "socket").unwrap_or_default()).as_bytes())
    ))
}

fn init_write_binding(binding_path: &Path, registration: &J) -> Result<()> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true).mode(0o600);
    let file = init_open_binding(&mut options, binding_path)?;
    init_write_file(file, registration)
}

fn init_open_binding(options: &mut OpenOptions, path: &Path) -> Result<File> {
    options.open(path).map_err(init_binding_io_error)
}

fn init_binding_io_error(error: std::io::Error) -> FlowError {
    if error.kind() == std::io::ErrorKind::AlreadyExists {
        return guard_error(
            "pane already registered; release its existing guard before registering again",
        );
    }
    error.into()
}

fn init_write_file(mut file: File, registration: &J) -> Result<()> {
    let mut text = serde_json::to_string(registration)?;
    init_write_text(&mut file, &mut text)
}

fn init_write_text(file: &mut File, text: &mut String) -> Result<()> {
    text.push('\n');
    init_write_bytes(file, text.as_bytes())
}

fn init_write_bytes(file: &mut File, bytes: &[u8]) -> Result<()> {
    file.write_all(bytes)?;
    file.sync_all().map_err(FlowError::from)
}

fn init_save(root: PathBuf, mut s: J) -> Result<J> {
    save(&root, &mut s)?;
    Ok(s)
}
