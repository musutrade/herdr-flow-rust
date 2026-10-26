//! Focused Herdr boundaries: app-server transport, TUI lifecycle and runtime
//! binding. The facade owns no process I/O beyond explicit adapter methods.

use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{json, Map, Value};

use crate::codex::rpc::{extract_server_info, Client};
use crate::error::{ErrorKind, FlowError, Result};
use crate::model::{self, J};
use crate::util;
use crate::util::process::{command, current_exe, shell_join};

pub const RUNTIME_KEYS: &[&str] = &[
    "model",
    "modelProvider",
    "reasoningEffort",
    "cwd",
    "approvalPolicy",
    "approvalsReviewer",
    "sandbox",
    "activePermissionProfile",
    "serviceTier",
    "runtimeWorkspaceRoots",
    "disabledPluginIds",
    "collaborationMode",
    "multiAgentMode",
];

pub trait ReceiverAdapter {
    fn agent(&self) -> Result<J>;
    fn prompt(&self, text: &str) -> Result<String>;
    fn call(&self, args: &[String]) -> Result<J>;
}

pub type ReceiverFactory<'a> = dyn Fn(&J) -> Result<Box<dyn ReceiverAdapter + 'a>> + 'a;

/// The wider boundary used by the context guard.
pub trait GuardAdapter {
    fn set_state(&mut self, state: &J);
    fn state(&self) -> J;
    fn call(&self, args: &[String]) -> Result<J>;
    fn rpc(&self, method: &str, params: &J) -> Result<J>;
    fn check_capabilities(&self) -> Result<J>;
    fn agent(&self) -> Result<J>;
    fn idle(&self) -> Result<bool>;
    fn session(&self) -> Result<String>;
    fn snapshot(&self, session: Option<&str>) -> Result<J>;
    fn prompt(&self, text: &str) -> Result<String>;
    fn exit_tui(&self) -> Result<()>;
    fn shell_ready(&self) -> Result<bool>;
    fn launch_tui(&self, root: &Path, nonce: &str) -> Result<()>;
    fn ready_tui(&self) -> Result<Option<String>>;
    fn verify_tui(&self, expected: &str) -> Result<J>;
    fn marker(&self) -> Result<Option<J>>;
    fn target(&self) -> Result<String>;
    fn restore_runtime(&self, expected: &J, session: &str) -> Result<()>;
    fn processes(&self) -> Result<J>;
    fn pane(&self) -> Result<J>;
}

pub struct Herdr {
    pub state: J,
}

impl Herdr {
    pub fn new(state: &J) -> Result<Self> {
        validate_herdr_environment()?;
        Self::from_validated_state(state)
    }

    fn from_validated_state(state: &J) -> Result<Self> {
        validate_herdr_state(state)?;
        Ok(Self {
            state: state.clone(),
        })
    }

    fn app_server_socket(&self) -> Result<String> {
        model::string(&self.state, "app_server_socket")
            .ok_or_else(|| FlowError::guard("app-server socket is not registered"))
    }

    fn cwd(&self) -> Result<String> {
        model::string(&self.state, "cwd")
            .ok_or_else(|| FlowError::guard("registered cwd is unavailable"))
    }

    fn call_impl(&self, args: &[String]) -> Result<J> {
        validate_arguments(args)?;
        finish_call_args(args)
    }

    fn rpc_impl(&self, method: &str, params: &J) -> Result<J> {
        let socket = self.app_server_socket()?;
        rpc_call(&socket, method, params)
    }

    fn check_capabilities_impl(&self) -> Result<J> {
        let target = self.target_impl()?;
        self.capabilities_for_target(&target)
    }

    fn capabilities_for_target(&self, target: &str) -> Result<J> {
        let socket = self.app_server_socket()?;
        Ok(check_capabilities(&socket, target))
    }

    fn pane_impl(&self) -> Result<J> {
        let pane = self.registered_pane()?;
        self.check_pane_terminal(pane)
    }

    fn registered_pane(&self) -> Result<J> {
        let pane_id = required_pane_id(&self.state)?;
        self.fetch_pane(&pane_id)
    }

    fn fetch_pane(&self, pane_id: &str) -> Result<J> {
        let response =
            self.call_impl(&["pane".to_string(), "get".to_string(), pane_id.to_string()])?;
        extract_pane(&response)
    }

    fn check_pane_terminal(&self, pane: J) -> Result<J> {
        if model::string(&pane, "terminal_id") != model::string(&self.state, "terminal") {
            return Err(FlowError::guard("pane terminal changed"));
        }
        Ok(pane)
    }

    fn processes_impl(&self) -> Result<J> {
        self.ensure_registered_pane()?;
        self.fetch_process_info()
    }

    fn ensure_registered_pane(&self) -> Result<()> {
        self.pane_impl().map(|_| ())
    }

    fn fetch_process_info(&self) -> Result<J> {
        let response = self.process_info_response()?;
        extract_process_info(&response)
    }

    fn process_info_response(&self) -> Result<J> {
        self.call_impl(&[
            "pane".to_string(),
            "process-info".to_string(),
            "--pane".to_string(),
            state_pane_id(&self.state),
        ])
    }

    fn marker_impl(&self) -> Result<Option<J>> {
        let path = self.marker_path()?;
        self.read_existing_marker(&path)
    }

    fn read_existing_marker(&self, path: &Path) -> Result<Option<J>> {
        if !path.exists() {
            return Ok(None);
        }
        self.read_marker(path)
    }

    fn marker_path(&self) -> Result<PathBuf> {
        let state_dir = model::string(&self.state, "state_dir")
            .ok_or_else(|| FlowError::guard("guard state directory is unavailable"))?;
        Ok(Path::new(&state_dir).join("tui.json"))
    }

    fn read_marker(&self, path: &Path) -> Result<Option<J>> {
        let marker = util::read_json(path)?;
        self.validate_marker(marker)
    }

    fn validate_marker(&self, marker: J) -> Result<Option<J>> {
        if model::get(&marker, "nonce") != model::get(&self.state, "bridge_nonce") {
            return Ok(None);
        }
        self.check_marker_socket(marker)
    }

    fn check_marker_socket(&self, marker: J) -> Result<Option<J>> {
        if model::string(&marker, "server_socket")
            != model::string(&self.state, "app_server_socket")
        {
            return Err(FlowError::guard("TUI bridge server identity changed"));
        }
        Ok(Some(marker))
    }

    fn verify_tui_impl(&self, expected: &str) -> Result<J> {
        let processes = self.processes_impl()?;
        self.verify_tui_processes(expected, &processes)
    }

    fn verify_tui_processes(&self, expected: &str, processes: &J) -> Result<J> {
        let identity = self.observed_tui_identity(expected, processes)?;
        verify_live_identity(identity, processes)
    }

    fn observed_tui_identity(&self, expected: &str, processes: &J) -> Result<Option<J>> {
        verify_shell_pid(&self.state, processes)?;
        self.bridge_identity(expected, processes)
    }

    fn bridge_identity(&self, expected: &str, processes: &J) -> Result<Option<J>> {
        if model::get(&self.state, "bridge_nonce").is_some() {
            return self.bridge_marker_identity(expected, processes);
        }
        Ok(model::get(&self.state, "tui_process").cloned())
    }

    fn bridge_marker_identity(&self, expected: &str, processes: &J) -> Result<Option<J>> {
        let marker = self.required_marker()?;
        self.validate_bridge_marker(&marker, expected, processes)
    }

    fn required_marker(&self) -> Result<J> {
        self.marker_impl()?
            .ok_or_else(|| FlowError::guard("TUI bridge marker is unavailable"))
    }

    fn validate_bridge_marker(
        &self,
        marker: &J,
        expected: &str,
        processes: &J,
    ) -> Result<Option<J>> {
        verify_marker_ready(marker, expected)?;
        self.verify_marker_processes(marker, processes)
    }

    fn verify_marker_processes(&self, marker: &J, processes: &J) -> Result<Option<J>> {
        verify_wrapper_process(processes, marker)?;
        self.marker_tui_identity(marker, processes)
    }

    fn marker_tui_identity(&self, marker: &J, processes: &J) -> Result<Option<J>> {
        let identity = verified_tui_identity(marker)?;
        self.finish_marker_identity(marker, processes, identity)
    }

    fn finish_marker_identity(&self, marker: &J, processes: &J, identity: J) -> Result<Option<J>> {
        verify_tui_launch(&self.state, marker, processes)?;
        Ok(Some(identity))
    }

    fn shell_ready_impl(&self) -> Result<bool> {
        let info = self.processes_impl()?;
        self.shell_ready_from_info(&info)
    }

    fn shell_ready_from_info(&self, info: &J) -> Result<bool> {
        let shell_pid = verify_process_shell_pid(&self.state, info)?;
        self.shell_ready_state(shell_pid, info)
    }

    fn shell_ready_state(&self, shell_pid: u64, info: &J) -> Result<bool> {
        if idle_shell_ready(shell_pid, info) {
            return Ok(true);
        }
        self.check_original_tui(info)
    }

    fn check_original_tui(&self, info: &J) -> Result<bool> {
        let Some(original) = original_tui_process(&self.state) else {
            return Err(shell_ready_error());
        };
        original_tui_present(info, original)
    }

    fn launch_tui_impl(&self, root: &Path, nonce: &str) -> Result<()> {
        self.require_shell_ready()?;
        self.run_tui_command(root, nonce)
    }

    fn require_shell_ready(&self) -> Result<()> {
        let ready = self.shell_ready_impl()?;
        check_shell_ready(ready)
    }

    fn run_tui_command(&self, root: &Path, nonce: &str) -> Result<()> {
        let argv = self.tui_argv(root, nonce)?;
        self.call_impl(&pane_run_arguments(&self.state, argv))
            .map(|_| ())
    }

    fn tui_argv(&self, root: &Path, nonce: &str) -> Result<Vec<String>> {
        let executable = current_exe()?;
        self.tui_argv_with_executable(executable, root, nonce)
    }

    fn tui_argv_with_executable(
        &self,
        executable: PathBuf,
        root: &Path,
        nonce: &str,
    ) -> Result<Vec<String>> {
        let socket = self.app_server_socket()?;
        self.tui_argv_with_socket(executable, socket, root, nonce)
    }

    fn tui_argv_with_socket(
        &self,
        executable: PathBuf,
        socket: String,
        root: &Path,
        nonce: &str,
    ) -> Result<Vec<String>> {
        let cwd = self.cwd()?;
        Ok(tui_arguments(
            executable,
            socket,
            cwd,
            &self.state,
            root,
            nonce,
        ))
    }

    fn ready_tui_impl(&self) -> Result<Option<String>> {
        let Some(marker) = self.marker_impl()? else {
            return Ok(None);
        };
        self.ready_from_marker(marker)
    }

    fn ready_from_marker(&self, marker: J) -> Result<Option<String>> {
        let status = marker_status(&marker);
        if matches!(status.as_str(), "error" | "exited") {
            return Err(successor_failed_error(&marker, status));
        }
        self.ready_session(marker, status)
    }

    fn ready_session(&self, marker: J, status: String) -> Result<Option<String>> {
        if status != "ready" {
            return Ok(None);
        }
        self.ready_marker_session(&marker)
    }

    fn ready_marker_session(&self, marker: &J) -> Result<Option<String>> {
        let session = ready_session_id(marker)?;
        self.verified_ready_session(session)
    }

    fn verified_ready_session(&self, session: String) -> Result<Option<String>> {
        self.verify_tui_impl(&session)?;
        Ok(Some(session))
    }

    fn target_impl(&self) -> Result<String> {
        if let Some(successor) =
            model::get(&self.state, "successor").filter(|value| model::truthy(Some(value)))
        {
            return successor
                .as_str()
                .map(ToString::to_string)
                .ok_or_else(|| FlowError::guard("invalid successor identity"));
        }
        model::string(&self.state, "session")
            .ok_or_else(|| FlowError::guard("registered session is unavailable"))
    }

    fn agent_impl(&self) -> Result<J> {
        let target = self.target_impl()?;
        self.agent_for_target(target)
    }

    fn agent_for_target(&self, target: String) -> Result<J> {
        let thread = self.agent_thread(&target)?;
        self.agent_value(&thread, target)
    }

    fn agent_value(&self, thread: &J, target: String) -> Result<J> {
        verify_agent_thread(&self.state, thread, &target)?;
        self.agent_status_value(thread, target)
    }

    fn agent_status_value(&self, thread: &J, target: String) -> Result<J> {
        let status = thread_status(thread);
        validate_agent_status(&status)?;
        Ok(agent_response(thread, target, &status))
    }

    fn agent_thread(&self, target: &str) -> Result<J> {
        self.verify_tui_impl(target)?;
        self.read_agent_thread(target)
    }

    fn read_agent_thread(&self, target: &str) -> Result<J> {
        let response = self.rpc_impl("thread/read", &thread_read_params(target))?;
        extract_agent_thread(&response)
    }

    fn idle_impl(&self) -> Result<bool> {
        let agent = self.agent_impl()?;
        Ok(agent_is_idle(&agent))
    }

    fn session_impl(&self) -> Result<String> {
        let _ = self.agent_impl()?;
        self.target_impl()
    }

    fn snapshot_impl(&self, session: Option<&str>) -> Result<J> {
        let target = self.snapshot_target(session)?;
        self.snapshot_for_target(target)
    }

    fn snapshot_target(&self, session: Option<&str>) -> Result<String> {
        if let Some(session) = session {
            return Ok(session.to_string());
        }
        self.target_impl()
    }

    fn snapshot_for_target(&self, target: String) -> Result<J> {
        let response = self.resume_thread(&target)?;
        self.validate_resume(response, target)
    }

    fn resume_thread(&self, target: &str) -> Result<J> {
        self.verify_tui_impl(target)?;
        self.resume_registered_thread(target)
    }

    fn resume_registered_thread(&self, target: &str) -> Result<J> {
        let mut client = Client::new(&self.app_server_socket()?);
        let result = client.call(
            "thread/resume",
            &json!({"threadId": target, "excludeTurns": true}),
        );
        self.snapshot_response(target, result)
    }

    fn snapshot_response(&self, target: &str, result: Result<J>) -> Result<J> {
        result.map_or_else(|error| self.snapshot_fallback(target, error), Ok)
    }

    fn snapshot_fallback(&self, target: &str, error: FlowError) -> Result<J> {
        let marker = self.snapshot_marker()?;
        snapshot_fallback_value(&self.state, target, &error, marker)
    }

    fn snapshot_marker(&self) -> Result<Option<J>> {
        if model::get(&self.state, "bridge_nonce").is_some() {
            return self.marker_impl();
        }
        Ok(None)
    }

    fn validate_resume(&self, response: J, target: String) -> Result<J> {
        verify_resume_identity(&response, &target)?;
        self.resume_runtime(&response)
    }

    fn resume_runtime(&self, response: &J) -> Result<J> {
        self.validate_resume_environment(response)?;
        runtime_identity(response)
    }

    fn validate_resume_environment(&self, response: &J) -> Result<()> {
        let registered_cwd = self.cwd()?;
        verify_environments(response, &registered_cwd)
    }

    fn prompt_impl(&self, text: &str) -> Result<String> {
        self.require_idle_with("app-server thread is active or does not accept direct input")?;
        self.start_turn(text)
    }

    fn require_idle_with(&self, message: &str) -> Result<()> {
        let idle = self.idle_impl()?;
        check_idle(idle, message)
    }

    fn start_turn(&self, text: &str) -> Result<String> {
        let target = self.target_impl()?;
        self.send_turn(&target, text)
    }

    fn send_turn(&self, target: &str, text: &str) -> Result<String> {
        let response = self.rpc_impl("turn/start", &turn_params(target, text))?;
        turn_id(&response)
    }

    fn exit_tui_impl(&self) -> Result<()> {
        self.require_idle_with("thread must be idle before closing its TUI")?;
        self.quit_tui()
    }

    fn quit_tui(&self) -> Result<()> {
        self.call_impl(&quit_arguments(&self.state)).map(|_| ())
    }

    fn restore_runtime_impl(&self, expected: &J, session: &str) -> Result<()> {
        let expected = runtime_identity(expected)?;
        self.restore_snapshot(expected, session)
    }

    fn restore_snapshot(&self, expected: J, session: &str) -> Result<()> {
        let observed = self.snapshot_impl(Some(session))?;
        self.restore_observed(expected, observed, session)
    }

    fn restore_observed(&self, expected: J, observed: J, session: &str) -> Result<()> {
        if observed == expected {
            return Ok(());
        }
        self.restore_different_runtime(&expected, &observed, session)
    }

    fn restore_different_runtime(&self, expected: &J, observed: &J, session: &str) -> Result<()> {
        verify_restore_invariants(observed, expected)?;
        let params = restore_params(observed, expected, session);
        self.apply_restored_settings(&params, expected, session)
    }

    fn apply_restored_settings(&self, params: &J, expected: &J, session: &str) -> Result<()> {
        self.update_settings(params)?;
        self.wait_for_runtime(expected, session)
    }

    fn update_settings(&self, params: &J) -> Result<()> {
        self.rpc_impl("thread/settings/update", params)
            .map(|_| ())
            .map_err(|error| FlowError::guard(format!("cannot update successor settings: {error}")))
    }

    fn wait_for_runtime(&self, expected: &J, session: &str) -> Result<()> {
        let deadline = Instant::now() + Duration::from_secs(15);
        let mut wait = RuntimeWait::Continue;
        while wait.continues() {
            wait = self.runtime_wait_step(expected, session, deadline);
        }
        wait.into_result()
    }

    fn runtime_wait_step(&self, expected: &J, session: &str, deadline: Instant) -> RuntimeWait {
        self.snapshot_impl(Some(session))
            .map_or_else(RuntimeWait::Failed, |current| {
                check_runtime_step(expected, current, deadline)
            })
    }
}

fn env_var(name: &str) -> Option<String> {
    std::env::var(name).ok()
}

fn context_changed_error() -> FlowError {
    FlowError::guard("Herdr server context changed or unmanaged environment")
}

fn validate_herdr_environment() -> Result<()> {
    if env_var("HERDR_ENV").as_deref() != Some("1") {
        return Err(context_changed_error());
    }
    Ok(())
}

fn validate_herdr_state(state: &J) -> Result<()> {
    validate_herdr_socket(state)?;
    validate_guard_version(state)
}

fn validate_herdr_socket(state: &J) -> Result<()> {
    let socket = model::string(state, "socket").unwrap_or_default();
    if env_var("HERDR_SOCKET_PATH").as_deref() != Some(socket.as_str()) {
        return Err(context_changed_error());
    }
    Ok(())
}

fn validate_guard_version(state: &J) -> Result<()> {
    if model::int(state, "version") != Some(2) {
        return Err(FlowError::guard(
            "legacy guard requires explicit migrate before app-server control",
        ));
    }
    Ok(())
}

fn invalid_argument(argument: &str) -> bool {
    argument
        .chars()
        .any(|character| matches!(character, ';' | '&' | '|' | '$' | '`' | '\n' | '\r'))
}

fn validate_arguments(args: &[String]) -> Result<()> {
    if args.iter().any(|argument| invalid_argument(argument)) {
        return Err(FlowError::guard(
            "herdr command argument contains invalid characters",
        ));
    }
    Ok(())
}

fn run_herdr(args: &[String]) -> Result<Vec<u8>> {
    let mut argv = vec!["herdr".to_string()];
    argv.extend(args.iter().cloned());
    command(&argv, None, Duration::from_secs(30), true)
}

fn finish_call_args(args: &[String]) -> Result<J> {
    let raw = run_herdr(args)?;
    finish_call(raw, args)
}

fn finish_call(raw: Vec<u8>, args: &[String]) -> Result<J> {
    if is_pane_run(&raw, args) {
        return Ok(json!({}));
    }
    parse_herdr_response(&raw)
}

fn is_pane_run(raw: &[u8], args: &[String]) -> bool {
    is_blank(raw) && is_pane_run_command(args)
}

fn is_blank(raw: &[u8]) -> bool {
    raw.iter().all(|byte| byte.is_ascii_whitespace())
}

fn is_pane_run_command(args: &[String]) -> bool {
    args.len() >= 2 && pane_run_args(args)
}

fn pane_run_args(args: &[String]) -> bool {
    args[0] == "pane" && args[1] == "run"
}

fn parse_herdr_response(raw: &[u8]) -> Result<J> {
    let result: J = serde_json::from_slice(raw)
        .map_err(|error| FlowError::guard(format!("Herdr returned invalid JSON: {error}")))?;
    validate_herdr_response(&result)
}

fn validate_herdr_response(result: &J) -> Result<J> {
    if model::get(result, "error").is_some() {
        return Err(FlowError::guard("Herdr rejected the command"));
    }
    model::get(result, "result")
        .cloned()
        .ok_or_else(|| FlowError::guard("Herdr response has no result"))
}

fn rpc_call(socket: &str, method: &str, params: &J) -> Result<J> {
    let mut client = Client::new(socket);
    client
        .call(method, params)
        .map_err(|error| map_rpc_error(method, error))
}

fn map_rpc_error(method: &str, error: FlowError) -> FlowError {
    if error.kind == ErrorKind::Rpc {
        return map_rpc_kind_error(method, error);
    }
    map_non_rpc_error(method, error)
}

fn map_non_rpc_error(method: &str, error: FlowError) -> FlowError {
    if error.kind == ErrorKind::RpcTimeout {
        return transient_rpc_error(method, &error);
    }
    map_protocol_or_guard(method, error)
}

fn map_protocol_or_guard(method: &str, error: FlowError) -> FlowError {
    if error.kind == ErrorKind::RpcProtocol {
        return FlowError::guard(format!("app-server {method}: protocol error: {error}"));
    }
    FlowError::guard(format!("app-server {method}: {error}"))
}

fn map_rpc_kind_error(method: &str, error: FlowError) -> FlowError {
    if error.rpc_error.is_some() {
        return FlowError::guard(format!("app-server {method}: {error}"));
    }
    transient_rpc_error(method, &error)
}

fn transient_rpc_error(method: &str, error: &FlowError) -> FlowError {
    FlowError::new(
        ErrorKind::Transient,
        format!("app-server {method}: {error}"),
    )
}

struct CapabilityReport {
    info: Value,
    missing: Vec<String>,
    hints: Vec<String>,
}

impl CapabilityReport {
    fn from_info(info: Value) -> Self {
        let mut report = Self {
            info,
            missing: Vec::new(),
            hints: Vec::new(),
        };
        report.add_version_hint();
        report
    }

    fn add_version_hint(&mut self) {
        if let Some(version) = model::string(&self.info, "version") {
            self.hints.push(format!("app-server version: {version}"));
        } else {
            self.hints.push(
                "app-server version unknown; operations verified by actual response".to_string(),
            );
        }
    }

    fn check_thread(&mut self, client: &mut Client, target: &str) {
        let outcome = client
            .call("thread/read", &thread_read_params(target))
            .map(|thread| self.record_thread_response(&thread));
        if let Err(error) = outcome {
            self.record_thread_error(error);
        }
    }

    fn record_thread_response(&mut self, thread: &J) {
        if !thread_complete(thread) {
            self.missing
                .push("thread/read response incomplete".to_string());
        }
    }

    fn record_thread_error(&mut self, error: FlowError) {
        if error.kind == ErrorKind::RpcProtocol {
            self.missing
                .push(format!("thread/read protocol error: {error}"));
            return;
        }
        self.record_thread_rpc_error(error);
    }

    fn record_thread_rpc_error(&mut self, error: FlowError) {
        if error.kind == ErrorKind::Rpc {
            self.record_thread_rpc_unsupported(error);
            return;
        }
        self.missing
            .push(format!("thread/read check failed: {error}"));
    }

    fn record_thread_rpc_unsupported(&mut self, error: FlowError) {
        if error.rpc_code == Some(-32601) {
            self.missing
                .push("thread/read method not supported".to_string());
            return;
        }
        self.hints
            .push(format!("thread/read check inconclusive: {error}"));
    }

    fn into_value(self) -> J {
        json!({
            "available": self.missing.is_empty(),
            "server_info": self.info,
            "missing": self.missing,
            "hints": self.hints
        })
    }
}

fn thread_complete(thread: &J) -> bool {
    thread.is_object() && model::get(thread, "thread").is_some()
}

fn thread_read_params(target: &str) -> J {
    json!({"threadId": target, "includeTurns": false})
}

fn check_capabilities(socket: &str, target: &str) -> J {
    let mut client = Client::new(socket);
    if let Err(error) = client.connect() {
        return capability_failure(format!("capability check failed: {error}"));
    }
    let Some(initialization) = client.initialization.clone() else {
        return capability_failure("initialization failed".to_string());
    };
    check_initialized_capabilities(&mut client, target, &initialization)
}

fn check_initialized_capabilities(client: &mut Client, target: &str, initialization: &J) -> J {
    let info = extract_server_info(Some(initialization));
    let mut report = CapabilityReport::from_info(info);
    report.check_thread(client, target);
    report.into_value()
}

fn capability_failure(message: String) -> J {
    json!({
        "available": false,
        "server_info": null,
        "missing": [message],
        "hints": []
    })
}

fn required_pane_id(state: &J) -> Result<String> {
    model::string(state, "pane").ok_or_else(|| FlowError::guard("pane is not registered"))
}

fn state_pane_id(state: &J) -> String {
    model::string(state, "pane").unwrap_or_default()
}

fn extract_pane(response: &J) -> Result<J> {
    model::get(response, "pane")
        .cloned()
        .ok_or_else(|| FlowError::guard("Herdr pane response is incomplete"))
}

fn extract_process_info(response: &J) -> Result<J> {
    model::get(response, "process_info")
        .cloned()
        .ok_or_else(|| FlowError::guard("Herdr process info is incomplete"))
}

fn verify_shell_pid(state: &J, processes: &J) -> Result<()> {
    let shell_pid = model::u64_field(processes, "shell_pid").unwrap_or(0) as u32;
    if Some(shell_pid as i64) != model::int(state, "shell_pid") {
        return Err(FlowError::guard("pane shell changed"));
    }
    Ok(())
}

fn verify_marker_ready(marker: &J, expected: &str) -> Result<()> {
    if !marker_ready_for(marker, expected) {
        return Err(FlowError::guard(
            "TUI has not confirmed the registered thread through app-server",
        ));
    }
    Ok(())
}

fn marker_ready_for(marker: &J, expected: &str) -> bool {
    model::string(marker, "status").as_deref() == Some("ready")
        && model::string(marker, "session").as_deref() == Some(expected)
}

fn verify_wrapper_process(processes: &J, marker: &J) -> Result<()> {
    let wrapper = util::process_identity(marker_wrapper_pid(marker))?;
    check_wrapper_pane(processes, marker, &wrapper)
}

fn check_wrapper_pane(processes: &J, marker: &J, wrapper: &J) -> Result<()> {
    if !wrapper_in_pane(processes, marker, wrapper) {
        return Err(FlowError::guard(
            "TUI protocol observer process no longer belongs to this pane",
        ));
    }
    Ok(())
}

fn wrapper_in_pane(processes: &J, marker: &J, wrapper: &J) -> bool {
    wrapper_start_matches(marker, wrapper)
        && foreground_contains(processes, marker_wrapper_pid(marker) as u64)
}

fn wrapper_start_matches(marker: &J, wrapper: &J) -> bool {
    scalar_text(model::get(wrapper, "start_ticks"))
        == scalar_text(model::get(marker, "wrapper_start_ticks"))
}

fn marker_wrapper_pid(marker: &J) -> u32 {
    model::u64_field(marker, "wrapper_pid").unwrap_or(0) as u32
}

fn marker_tui_pid(marker: &J) -> u32 {
    model::u64_field(marker, "pid").unwrap_or(0) as u32
}

fn verified_tui_identity(marker: &J) -> Result<J> {
    let identity = util::process_identity(marker_tui_pid(marker))?;
    check_pid_start_ticks(marker, identity)
}

fn check_pid_start_ticks(marker: &J, identity: J) -> Result<J> {
    if scalar_text(model::get(&identity, "start_ticks"))
        != scalar_text(model::get(marker, "pid_start_ticks"))
    {
        return Err(FlowError::guard("TUI process was replaced"));
    }
    Ok(identity)
}

fn verify_tui_launch(state: &J, marker: &J, processes: &J) -> Result<()> {
    let argv = matched_tui_argv(marker, processes)?;
    verify_tui_argv(state, marker, &argv)
}

fn matched_tui_argv(marker: &J, processes: &J) -> Result<Vec<String>> {
    let matches = matching_foreground(processes, marker_tui_pid(marker) as u64);
    if matches.len() != 1 {
        return Err(FlowError::guard(
            "registered TUI is not in this pane foreground",
        ));
    }
    Ok(matched_process_argv(matches[0]))
}

fn matched_process_argv(process: &J) -> Vec<String> {
    model::array_field(process, "argv")
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str().map(ToString::to_string))
                .collect()
        })
        .unwrap_or_default()
}

fn matching_foreground(processes: &J, pid: u64) -> Vec<&J> {
    foreground_processes(processes)
        .iter()
        .filter(|item| model::u64_field(item, "pid") == Some(pid))
        .collect()
}

fn verify_tui_argv(state: &J, marker: &J, argv: &[String]) -> Result<()> {
    if !tui_argv_matches(state, marker, argv) {
        return Err(FlowError::guard("TUI launch identity changed"));
    }
    Ok(())
}

fn tui_argv_matches(state: &J, marker: &J, argv: &[String]) -> bool {
    valid_tui_binary(state, argv) && valid_remote_proxy(marker, argv)
}

fn valid_tui_binary(state: &J, argv: &[String]) -> bool {
    !argv.is_empty() && binary_matches(state, argv)
}

fn binary_matches(state: &J, argv: &[String]) -> bool {
    let expected_binary = model::string(state, "codex_binary")
        .map(|value| util::resolve_path(Path::new(&value)))
        .unwrap_or_default();
    util::resolve_path(Path::new(&argv[0])) == expected_binary
}

fn valid_remote_proxy(marker: &J, argv: &[String]) -> bool {
    let Some(remote_position) = argv.iter().position(|value| value == "--remote") else {
        return false;
    };
    remote_value_matches(marker, argv, remote_position)
}

fn remote_value_matches(marker: &J, argv: &[String], remote_position: usize) -> bool {
    if remote_position + 1 >= argv.len() {
        return false;
    }
    let Some(endpoint) = model::string(marker, "proxy_endpoint") else {
        return false;
    };
    argv[remote_position + 1] == endpoint
}

fn verify_live_identity(identity: Option<J>, processes: &J) -> Result<J> {
    let Some(identity) = identity else {
        return Err(FlowError::guard("registered TUI process no longer exists"));
    };
    verify_process_still_registered(identity, processes)
}

fn verify_process_still_registered(identity: J, processes: &J) -> Result<J> {
    verify_identity_current(&identity)?;
    finish_live_identity(identity, processes)
}

fn finish_live_identity(identity: J, processes: &J) -> Result<J> {
    verify_foreground_presence(processes, identity_pid(&identity))?;
    Ok(identity)
}

fn verify_identity_current(identity: &J) -> Result<()> {
    let current = util::process_identity(identity_pid(identity))?;
    check_identity_equal(&current, identity)
}

fn check_identity_equal(current: &J, identity: &J) -> Result<()> {
    if current != identity {
        return Err(FlowError::guard("registered TUI process no longer exists"));
    }
    Ok(())
}

fn identity_pid(identity: &J) -> u32 {
    model::u64_field(identity, "pid").unwrap_or(0) as u32
}

fn verify_foreground_presence(processes: &J, pid: u32) -> Result<()> {
    if !foreground_contains(processes, pid as u64) {
        return Err(FlowError::guard(
            "another foreground process occupies the registered pane",
        ));
    }
    Ok(())
}

fn foreground_processes(processes: &J) -> &[J] {
    model::array_field(processes, "foreground_processes")
        .map(Vec::as_slice)
        .unwrap_or(&[])
}

fn foreground_pids(processes: &J) -> Vec<u64> {
    foreground_processes(processes)
        .iter()
        .filter_map(|item| model::u64_field(item, "pid"))
        .collect()
}

fn foreground_contains(processes: &J, pid: u64) -> bool {
    foreground_pids(processes).contains(&pid)
}

fn verify_process_shell_pid(state: &J, info: &J) -> Result<u64> {
    let shell_pid = model::u64_field(info, "shell_pid").unwrap_or(0);
    if Some(shell_pid as i64) != model::int(state, "shell_pid") {
        return Err(FlowError::guard("pane shell changed"));
    }
    Ok(shell_pid)
}

fn idle_shell_ready(shell_pid: u64, info: &J) -> bool {
    foreground_group_is_shell(shell_pid, info) && only_shell_foreground(shell_pid, info)
}

fn foreground_group_is_shell(shell_pid: u64, info: &J) -> bool {
    model::u64_field(info, "foreground_process_group_id").unwrap_or(0) == shell_pid
}

fn only_shell_foreground(shell_pid: u64, info: &J) -> bool {
    foreground_processes(info)
        .iter()
        .all(|process| model::u64_field(process, "pid") == Some(shell_pid))
}

fn original_tui_process(state: &J) -> Option<&J> {
    model::get(state, "tui_process").filter(|value| model::truthy(Some(value)))
}

fn original_tui_present(info: &J, original: &J) -> Result<bool> {
    let original_pid = model::u64_field(original, "pid").unwrap_or(0);
    if !foreground_contains(info, original_pid) {
        return Err(shell_ready_error());
    }
    verify_original_identity(original_pid as u32, original)
}

fn verify_original_identity(pid: u32, original: &J) -> Result<bool> {
    let current = util::process_identity(pid)?;
    original_identity_result(&current, original)
}

fn original_identity_result(current: &J, original: &J) -> Result<bool> {
    if current != original {
        return Err(shell_ready_error());
    }
    Ok(false)
}

fn shell_ready_error() -> FlowError {
    FlowError::guard("unexpected foreground process; never launch over another command")
}

fn check_shell_ready(ready: bool) -> Result<()> {
    if !ready {
        return Err(FlowError::guard(
            "TUI launch requires the original idle shell",
        ));
    }
    Ok(())
}

fn tui_arguments(
    executable: PathBuf,
    socket: String,
    cwd: String,
    state: &J,
    root: &Path,
    nonce: &str,
) -> Vec<String> {
    vec![
        executable.display().to_string(),
        "tui".to_string(),
        "--state-dir".to_string(),
        root.display().to_string(),
        "--socket".to_string(),
        socket,
        "--binary".to_string(),
        model::string(state, "codex_binary").unwrap_or_default(),
        "--cwd".to_string(),
        cwd,
        "--nonce".to_string(),
        nonce.to_string(),
    ]
}

fn pane_run_arguments(state: &J, argv: Vec<String>) -> Vec<String> {
    vec![
        "pane".to_string(),
        "run".to_string(),
        state_pane_id(state),
        shell_join(&argv),
    ]
}

fn marker_status(marker: &J) -> String {
    model::string(marker, "status").unwrap_or_default()
}

fn successor_failed_error(marker: &J, status: String) -> FlowError {
    FlowError::guard(format!(
        "successor TUI failed: {}",
        model::string(marker, "error").unwrap_or(status)
    ))
}

fn ready_session_id(marker: &J) -> Result<String> {
    let Some(session) = model::string(marker, "session") else {
        return Err(invalid_successor_error());
    };
    validate_session_id(&session)?;
    Ok(session)
}

fn validate_session_id(session: &str) -> Result<()> {
    if !session_id_valid(session) {
        return Err(invalid_successor_error());
    }
    Ok(())
}

fn session_id_valid(session: &str) -> bool {
    regex::Regex::new(r"^[0-9a-f-]{36}$")
        .expect("static session regex")
        .is_match(session)
}

fn invalid_successor_error() -> FlowError {
    FlowError::guard("invalid successor identity from TUI bridge")
}

fn verify_agent_thread(state: &J, thread: &J, target: &str) -> Result<()> {
    if !agent_thread_matches(state, thread, target) {
        return Err(FlowError::guard(
            "app-server thread identity or cwd changed",
        ));
    }
    Ok(())
}

fn agent_thread_matches(state: &J, thread: &J, target: &str) -> bool {
    agent_thread_identity(thread, target) && thread_cwd_matches(state, thread)
}

fn agent_thread_identity(thread: &J, target: &str) -> bool {
    thread_id_matches(thread, target) && thread_session_matches(thread, target)
}

fn thread_id_matches(thread: &J, target: &str) -> bool {
    model::string(thread, "id").as_deref() == Some(target)
}

fn thread_session_matches(thread: &J, target: &str) -> bool {
    model::string(thread, "sessionId").is_none_or(|session| session == target)
}

fn thread_cwd_matches(state: &J, thread: &J) -> bool {
    model::string(thread, "cwd") == model::string(state, "cwd")
}

fn thread_status(thread: &J) -> String {
    model::dig_str(thread, &["status", "type"]).unwrap_or_default()
}

fn validate_agent_status(status: &str) -> Result<()> {
    if matches!(status, "notLoaded" | "systemError") {
        return Err(FlowError::guard(format!(
            "registered app-server thread is not healthy: {status}"
        )));
    }
    validate_known_agent_status(status)
}

fn validate_known_agent_status(status: &str) -> Result<()> {
    if !matches!(status, "idle" | "active") {
        return Err(FlowError::guard("unknown app-server thread state"));
    }
    Ok(())
}

fn agent_is_idle(agent: &J) -> bool {
    model::string(agent, "agent_status").as_deref() == Some("idle")
        && model::bool_or(agent, "interactive_ready", false)
}

fn agent_response(thread: &J, target: String, status: &str) -> J {
    json!({
        "agent_session": {"value": target},
        "agent_status": agent_status_label(status),
        "interactive_ready": interactive_ready(thread, status)
    })
}

fn agent_status_label(status: &str) -> &'static str {
    if status == "idle" {
        return "idle";
    }
    "working"
}

fn interactive_ready(thread: &J, status: &str) -> bool {
    status == "idle" && model::bool_or(thread, "canAcceptDirectInput", false)
}

fn extract_agent_thread(response: &J) -> Result<J> {
    model::get(response, "thread")
        .cloned()
        .ok_or_else(|| FlowError::guard("app-server thread response is incomplete"))
}

fn check_idle(idle: bool, message: &str) -> Result<()> {
    if !idle {
        return Err(FlowError::new(ErrorKind::AgentBusy, message));
    }
    Ok(())
}

fn turn_params(target: &str, text: &str) -> J {
    json!({
        "threadId": target,
        "input": [{"type": "text", "text": text}]
    })
}

fn turn_id(response: &J) -> Result<String> {
    model::dig_str(response, &["turn", "id"])
        .ok_or_else(|| FlowError::guard("app-server turn response has no turn ID"))
}

fn quit_arguments(state: &J) -> Vec<String> {
    vec![
        "agent".to_string(),
        "prompt".to_string(),
        state_pane_id(state),
        "/quit".to_string(),
    ]
}

fn snapshot_fallback_value(
    state: &J,
    target: &str,
    error: &FlowError,
    marker: Option<J>,
) -> Result<J> {
    if !snapshot_fallback_allowed(state, target, error, marker.as_ref()) {
        return Err(FlowError::guard(format!(
            "app-server runtime snapshot failed: {error}"
        )));
    }
    bridge_runtime(&marker)
}

fn snapshot_fallback_allowed(
    state: &J,
    target: &str,
    error: &FlowError,
    marker: Option<&J>,
) -> bool {
    fallback_error_matches(error) && fallback_identity_matches(state, target, marker)
}

fn fallback_error_matches(error: &FlowError) -> bool {
    error.rpc_code == Some(-32600) && error.message.contains("no rollout found")
}

fn fallback_identity_matches(state: &J, target: &str, marker: Option<&J>) -> bool {
    fallback_successor_matches(state, target) && marker_confirms(marker, target)
}

fn fallback_successor_matches(state: &J, target: &str) -> bool {
    model::get(state, "successor").and_then(Value::as_str) == Some(target)
}

fn marker_confirms(marker: Option<&J>, target: &str) -> bool {
    marker
        .map(|marker| marker_ready_for(marker, target))
        .unwrap_or(false)
}

fn bridge_runtime(marker: &Option<J>) -> Result<J> {
    model::get(marker.as_ref().unwrap_or(&Value::Null), "runtime")
        .cloned()
        .ok_or_else(|| FlowError::guard("TUI bridge runtime is unavailable"))
}

fn verify_resume_identity(response: &J, target: &str) -> Result<()> {
    if model::dig_str(response, &["thread", "id"]).as_deref() != Some(target) {
        return Err(FlowError::guard(
            "app-server resume returned another thread",
        ));
    }
    Ok(())
}

fn verify_environments(response: &J, registered_cwd: &str) -> Result<()> {
    if environments_supported(response, registered_cwd) {
        return Ok(());
    }
    Err(FlowError::guard(
        "only local environments at the registered cwd are supported",
    ))
}

fn environments_supported(response: &J, registered_cwd: &str) -> bool {
    let Some(environments) =
        model::dig(response, &["thread", "environments"]).and_then(Value::as_array)
    else {
        return true;
    };
    environments
        .iter()
        .all(|environment| environment_supported(environment, registered_cwd))
}

fn environment_supported(environment: &J, registered_cwd: &str) -> bool {
    model::string(environment, "environmentId").as_deref() == Some("local")
        && model::string(environment, "cwd").as_deref() == Some(registered_cwd)
}

fn verify_restore_invariants(observed: &J, expected: &J) -> Result<()> {
    if !restore_invariants_match(observed, expected) {
        return Err(FlowError::guard(
            "successor cwd, provider or workspace roots differ; inspect before recovery",
        ));
    }
    Ok(())
}

fn restore_invariants_match(observed: &J, expected: &J) -> bool {
    ["cwd", "modelProvider", "runtimeWorkspaceRoots"]
        .iter()
        .all(|key| model::get(observed, key) == model::get(expected, key))
}

fn restore_params(observed: &J, expected: &J, session: &str) -> J {
    let mut params = restore_base_params(expected, session);
    apply_service_tier(&mut params, observed, expected);
    apply_multi_agent_mode(&mut params, expected);
    apply_permissions(&mut params, expected);
    apply_collaboration_mode(&mut params, observed, expected);
    params
}

fn restore_base_params(expected: &J, session: &str) -> J {
    json!({
        "threadId": session,
        "model": optional_clone(expected, "model"),
        "effort": optional_clone(expected, "reasoningEffort"),
        "approvalPolicy": optional_clone(expected, "approvalPolicy"),
        "approvalsReviewer": optional_clone(expected, "approvalsReviewer"),
        "disabledPluginIds": expected.get("disabledPluginIds").cloned().unwrap_or_else(|| json!([]))
    })
}

fn apply_service_tier(params: &mut J, observed: &J, expected: &J) {
    if model::get(observed, "serviceTier") == model::get(expected, "serviceTier") {
        return;
    }
    params["serviceTier"] = optional_clone(expected, "serviceTier");
}

fn apply_multi_agent_mode(params: &mut J, expected: &J) {
    if let Some(mode) = non_null_field(expected, "multiAgentMode") {
        params["multiAgentMode"] = mode.clone();
    }
}

fn apply_permissions(params: &mut J, expected: &J) {
    let profile = truthy_field(expected, "activePermissionProfile");
    if let Some(profile) = profile {
        return apply_profile_permissions(params, expected, profile);
    }
    params["sandboxPolicy"] = optional_clone(expected, "sandbox");
}

fn apply_profile_permissions(params: &mut J, expected: &J, profile: &J) {
    if let Some(id) = model::string(profile, "id") {
        params["permissions"] = json!(id);
        return;
    }
    params["sandboxPolicy"] = optional_clone(expected, "sandbox");
}

fn apply_collaboration_mode(params: &mut J, observed: &J, expected: &J) {
    if collaboration_identity(observed) == collaboration_identity(expected) {
        return;
    }
    params["collaborationMode"] = optional_clone(expected, "collaborationMode");
}

fn collaboration_identity(runtime: &J) -> (String, Option<String>) {
    (
        model::dig_str(runtime, &["collaborationMode", "mode"]).unwrap_or_default(),
        model::dig_str(
            runtime,
            &["collaborationMode", "settings", "developer_instructions"],
        ),
    )
}

fn non_null_field<'a>(value: &'a J, key: &str) -> Option<&'a J> {
    model::get(value, key).filter(|field| !field.is_null())
}

fn truthy_field<'a>(value: &'a J, key: &str) -> Option<&'a J> {
    model::get(value, key).filter(|field| model::truthy(Some(field)))
}

fn optional_clone(value: &J, key: &str) -> J {
    model::get(value, key).cloned().unwrap_or(Value::Null)
}

enum RuntimeWait {
    Continue,
    Done(Result<()>),
    Failed(FlowError),
}

impl RuntimeWait {
    fn continues(&self) -> bool {
        matches!(self, Self::Continue)
    }

    fn into_result(self) -> Result<()> {
        if let Self::Done(result) = self {
            return result;
        }
        self.failed()
    }

    fn failed(self) -> Result<()> {
        if let Self::Failed(error) = self {
            return Err(error);
        }
        Err(FlowError::guard("runtime restore wait stalled"))
    }
}

fn check_runtime_step(expected: &J, current: J, deadline: Instant) -> RuntimeWait {
    if current == *expected {
        return RuntimeWait::Done(Ok(()));
    }
    deadline_runtime_step(expected, &current, deadline)
}

fn deadline_runtime_step(expected: &J, current: &J, deadline: Instant) -> RuntimeWait {
    if Instant::now() >= deadline {
        return RuntimeWait::Done(same_runtime(expected, current));
    }
    thread::sleep(Duration::from_millis(100));
    RuntimeWait::Continue
}

impl ReceiverAdapter for Herdr {
    fn agent(&self) -> Result<J> {
        self.agent_impl()
    }

    fn prompt(&self, text: &str) -> Result<String> {
        self.prompt_impl(text)
    }

    fn call(&self, args: &[String]) -> Result<J> {
        self.call_impl(args)
    }
}

impl GuardAdapter for Herdr {
    fn set_state(&mut self, state: &J) {
        self.state = state.clone();
    }

    fn state(&self) -> J {
        self.state.clone()
    }

    fn call(&self, args: &[String]) -> Result<J> {
        self.call_impl(args)
    }

    fn rpc(&self, method: &str, params: &J) -> Result<J> {
        self.rpc_impl(method, params)
    }

    fn check_capabilities(&self) -> Result<J> {
        self.check_capabilities_impl()
    }

    fn agent(&self) -> Result<J> {
        self.agent_impl()
    }

    fn idle(&self) -> Result<bool> {
        self.idle_impl()
    }

    fn session(&self) -> Result<String> {
        self.session_impl()
    }

    fn snapshot(&self, session: Option<&str>) -> Result<J> {
        self.snapshot_impl(session)
    }

    fn prompt(&self, text: &str) -> Result<String> {
        self.prompt_impl(text)
    }

    fn exit_tui(&self) -> Result<()> {
        self.exit_tui_impl()
    }

    fn shell_ready(&self) -> Result<bool> {
        self.shell_ready_impl()
    }

    fn launch_tui(&self, root: &Path, nonce: &str) -> Result<()> {
        self.launch_tui_impl(root, nonce)
    }

    fn ready_tui(&self) -> Result<Option<String>> {
        self.ready_tui_impl()
    }

    fn verify_tui(&self, expected: &str) -> Result<J> {
        self.verify_tui_impl(expected)
    }

    fn marker(&self) -> Result<Option<J>> {
        self.marker_impl()
    }

    fn target(&self) -> Result<String> {
        self.target_impl()
    }

    fn restore_runtime(&self, expected: &J, session: &str) -> Result<()> {
        self.restore_runtime_impl(expected, session)
    }

    fn processes(&self) -> Result<J> {
        self.processes_impl()
    }

    fn pane(&self) -> Result<J> {
        self.pane_impl()
    }
}

fn scalar_text(value: Option<&J>) -> String {
    value.map(scalar_value_text).unwrap_or_default()
}

fn scalar_value_text(value: &J) -> String {
    if let Some(text) = value.as_str() {
        return text.to_string();
    }
    scalar_non_string_text(value)
}

fn scalar_non_string_text(value: &J) -> String {
    if value.is_null() {
        return String::new();
    }
    value.to_string()
}

pub fn runtime_identity(response: &J) -> Result<J> {
    if !response.is_object() {
        return Err(FlowError::guard("missing app-server runtime response"));
    }
    for key in [
        "model",
        "modelProvider",
        "cwd",
        "approvalPolicy",
        "sandbox",
        "reasoningEffort",
    ] {
        if model::get(response, key).is_none() {
            return Err(FlowError::guard("incomplete app-server runtime response"));
        }
    }
    let model_value = model::string(response, "model").unwrap_or_default();
    if model_value.is_empty() {
        return Err(FlowError::guard("invalid app-server model"));
    }
    if !model::get(response, "sandbox")
        .map(Value::is_object)
        .unwrap_or(false)
    {
        return Err(FlowError::guard("invalid app-server permissions or cwd"));
    }
    let cwd = model::string(response, "cwd").unwrap_or_default();
    if !Path::new(&cwd).is_absolute() {
        return Err(FlowError::guard("invalid app-server permissions or cwd"));
    }
    let mut runtime = Map::new();
    for key in RUNTIME_KEYS {
        runtime.insert(
            (*key).to_string(),
            response.get(*key).cloned().unwrap_or(Value::Null),
        );
    }
    let disabled = runtime
        .get("disabledPluginIds")
        .cloned()
        .filter(|value| model::truthy(Some(value)))
        .unwrap_or_else(|| json!([]));
    runtime.insert("disabledPluginIds".to_string(), disabled);
    if runtime
        .get("collaborationMode")
        .map(Value::is_null)
        .unwrap_or(true)
    {
        runtime.insert(
            "collaborationMode".to_string(),
            json!({
                "mode": "default",
                "settings": {
                    "model": runtime.get("model").cloned().unwrap_or(Value::Null),
                    "reasoning_effort": runtime.get("reasoningEffort").cloned().unwrap_or(Value::Null),
                    "developer_instructions": null
                }
            }),
        );
    }
    Ok(Value::Object(runtime))
}

pub fn same_runtime(expected: &J, actual: &J) -> Result<()> {
    if runtime_identity(expected)? != runtime_identity(actual)? {
        return Err(FlowError::guard(
            "app-server runtime differs from the sealed predecessor settings",
        ));
    }
    Ok(())
}

pub fn auto_review_runtime(before: &J) -> Result<J> {
    let mut expected = before.clone();
    expected["approvalPolicy"] = json!("on-request");
    expected["approvalsReviewer"] = json!("auto_review");
    if let Some(profile) =
        model::get(before, "activePermissionProfile").filter(|value| model::truthy(Some(value)))
    {
        if let Some(id) = model::string(profile, "id") {
            if !matches!(
                id.as_str(),
                ":workspace" | ":read-only" | ":danger-full-access"
            ) {
                return Err(FlowError::guard(
                    "custom permission profile requires explicit profile-specific configuration",
                ));
            }
        }
    }
    let Some(mut sandbox) = model::get(before, "sandbox").cloned() else {
        return Err(FlowError::guard(
            "automatic review requires a local read-only or workspace-write sandbox",
        ));
    };
    let kind = model::string(&sandbox, "type").unwrap_or_default();
    if kind == "dangerFullAccess" {
        sandbox["type"] = json!("workspaceWrite");
        sandbox["writableRoots"] = json!([]);
        sandbox["excludeTmpdirEnvVar"] = json!(true);
        sandbox["excludeSlashTmp"] = json!(true);
    }
    let kind = model::string(&sandbox, "type").unwrap_or_default();
    if !matches!(kind.as_str(), "readOnly" | "workspaceWrite") {
        return Err(FlowError::guard(
            "automatic review requires a local read-only or workspace-write sandbox",
        ));
    }
    sandbox["networkAccess"] = json!(false);
    expected["sandbox"] = sandbox;
    Ok(expected)
}

pub fn verify_review_runtime(expected: &J, actual: &J) -> Result<()> {
    for key in RUNTIME_KEYS {
        if *key == "activePermissionProfile" {
            continue;
        }
        if model::get(expected, key) != model::get(actual, key) {
            return Err(FlowError::guard(format!(
                "automatic review runtime mismatch: {key}"
            )));
        }
    }
    Ok(())
}

pub fn check_platform() -> J {
    util::check_platform()
}

/// Production receiver factory.
pub fn receiver_factory(state: &J) -> Result<Box<dyn ReceiverAdapter>> {
    if crate::task_runtime::native::receiver::is_task_owned(state) {
        return Ok(Box::new(
            crate::task_runtime::native::receiver::active::Adapter::new(state)?,
        ));
    }
    match model::string(state, "adapter").as_deref() {
        Some("opencode") => Ok(Box::new(crate::opencode::session::SessionAdapter::new(
            state,
        )?)),
        None | Some("codex") => Ok(Box::new(Herdr::new(state)?)),
        _ => Err(FlowError::guard("unsupported receiver adapter")),
    }
}
