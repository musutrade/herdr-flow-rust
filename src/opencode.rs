//! OpenCode 1.19.3 native sessions and legacy process monitoring.
//!
//! [`session`] implements the native HTTP receiver and durable producer path.
//! The legacy [`OpenCodeAdapter`] below checks the shell PID and `/proc`
//! process identity only; it does not control a native session.
//!
//! The adapter is read-only: the only Herdr commands it issues are
//! `pane get` and `pane process-info`. Actions such as prompting, exiting,
//! launching a successor and restoring runtime settings intentionally report a
//! `GuardError` instead of performing a side effect.

use std::borrow::Borrow;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

use serde_json::{json, Value};

use crate::error::{ErrorKind, FlowError, Result};
use crate::model::{self, J};
use crate::util;
use crate::util::process::command;

mod http;
pub mod session;

/// Human-readable adapter identity from the original capability baseline.
pub const ADAPTER_NAME: &str = "OpenCode 1.19.3";

/// Timeout used for every herdr CLI invocation (`runner(..., timeout=30)`).
pub const HERDR_TIMEOUT: Duration = Duration::from_secs(30);

/// A herdr CLI runner: `(argv, timeout, transient) -> stdout`.
///
/// It mirrors the Python factory's runner signature so callers can inject a
/// fake command boundary in tests without touching the network or the host.
pub type HerdrRunner<'a> = dyn Fn(&[String], Duration, bool) -> Result<Vec<u8>> + 'a;

/// Resolve a PID to a stable process identity (`pid` and `start_ticks`).
pub type ProcessIdentityFn<'a> = dyn Fn(u32) -> Result<J> + 'a;

/// Minimal OpenCode 1.19.3 adapter: process identity + shell readiness only.
pub struct OpenCodeAdapter<'a> {
    /// The guard state document (`pane`, `shell_pid`, `cwd`, `terminal`).
    pub state: J,
    /// Herdr CLI runner, normally backed by [`crate::util::process::command`].
    pub runner: Box<HerdrRunner<'a>>,
    /// Process identity resolver, normally [`crate::util::process_identity`].
    pub process_identity: Box<ProcessIdentityFn<'a>>,
    capabilities: OnceLock<J>,
}

impl<'a> OpenCodeAdapter<'a> {
    /// Build an adapter from a state document and explicit boundaries.
    ///
    /// The runner may return either bytes or text, matching the Python
    /// factory's `text=True` subprocess boundary while keeping the Rust
    /// `command` helper as the default implementation.
    pub fn new<S, F, P, O>(state: S, runner: F, process_identity: P) -> Self
    where
        S: Borrow<J>,
        F: Fn(&[String], Duration, bool) -> Result<O> + 'a,
        O: Into<Vec<u8>> + 'a,
        P: Fn(u32) -> Result<J> + 'a,
    {
        Self {
            state: state.borrow().clone(),
            runner: Box::new(move |argv: &[String], timeout: Duration, transient: bool| {
                runner(argv, timeout, transient).map(Into::into)
            }),
            process_identity: Box::new(process_identity),
            capabilities: OnceLock::new(),
        }
    }

    fn pane_id(&self) -> Result<String> {
        model::string(&self.state, "pane").ok_or_else(|| FlowError::guard("pane is not registered"))
    }

    fn shell_pid(&self) -> Result<u64> {
        model::u64_field(&self.state, "shell_pid")
            .ok_or_else(|| FlowError::guard("shell PID is not registered"))
    }

    /// Run one herdr CLI command and unwrap its JSON envelope.
    ///
    /// As in the Python adapter, an empty `pane run` response is treated as an
    /// empty result object, an `error` envelope is a `GuardError`, and a
    /// successful envelope exposes `result` (or the whole document when no
    /// `result` key exists).
    pub fn call(&self, args: &[String]) -> Result<J> {
        let mut argv = Vec::with_capacity(args.len() + 1);
        argv.push("herdr".to_string());
        argv.extend(args.iter().cloned());
        let raw = (self.runner)(&argv, HERDR_TIMEOUT, true)?;
        if raw.iter().all(u8::is_ascii_whitespace)
            && args.len() >= 2
            && args[0] == "pane"
            && args[1] == "run"
        {
            return Ok(json!({}));
        }
        let result: J = serde_json::from_slice(&raw)
            .map_err(|error| FlowError::guard(format!("Herdr returned invalid JSON: {error}")))?;
        if model::get(&result, "error").is_some() {
            return Err(FlowError::guard("Herdr rejected the command"));
        }
        Ok(model::get(&result, "result").cloned().unwrap_or(result))
    }

    /// Report the OpenCode 1.19.3 capability baseline (cached per adapter).
    pub fn check_capabilities(&self) -> Result<J> {
        Ok(self.capabilities.get_or_init(baseline_capabilities).clone())
    }

    /// Verify the pane terminal matches the registered state.
    pub fn pane(&self) -> Result<J> {
        let pane_id = self.pane_id()?;
        let response = self.call(&["pane".to_string(), "get".to_string(), pane_id])?;
        let pane = model::get(&response, "pane")
            .cloned()
            .ok_or_else(|| FlowError::guard("Herdr pane response is incomplete"))?;
        let expected = model::get(&self.state, "terminal")
            .ok_or_else(|| FlowError::guard("terminal is not registered"))?;
        if model::get(&pane, "terminal_id") != Some(expected) {
            return Err(FlowError::guard("pane terminal changed"));
        }
        Ok(pane)
    }

    /// Get pane process information after verifying the pane identity.
    pub fn processes(&self) -> Result<J> {
        let _ = self.pane()?;
        let pane_id = self.pane_id()?;
        let response = self.call(&[
            "pane".to_string(),
            "process-info".to_string(),
            "--pane".to_string(),
            pane_id,
        ])?;
        model::get(&response, "process_info")
            .cloned()
            .ok_or_else(|| FlowError::guard("Herdr process info is incomplete"))
    }

    /// Check whether the registered shell is the only foreground process.
    pub fn shell_ready(&self) -> Result<bool> {
        let info = self.processes()?;
        let expected = self.shell_pid()?;
        if model::u64_field(&info, "shell_pid") != Some(expected) {
            return Err(FlowError::guard("pane shell changed"));
        }
        let group = model::u64_field(&info, "foreground_process_group_id")
            .ok_or_else(|| FlowError::guard("Herdr process info is incomplete"))?;
        let foreground = model::array_field(&info, "foreground_processes")
            .ok_or_else(|| FlowError::guard("Herdr process info is incomplete"))?;
        let shell_only = foreground
            .iter()
            .all(|process| model::u64_field(process, "pid") == Some(expected));
        if group == expected && shell_only {
            Ok(true)
        } else {
            Err(FlowError::guard(
                "shell not idle; foreground process detected",
            ))
        }
    }

    /// Return the minimal, always-unknown OpenCode agent status.
    pub fn agent(&self) -> Result<J> {
        let _ = self.pane()?;
        Ok(json!({
            "agent_session": {"value": null},
            "agent_status": "unknown",
            "interactive_ready": false
        }))
    }

    /// The process monitor checks shell readiness only, not native session idle.
    pub fn idle(&self) -> Result<bool> {
        self.shell_ready()
    }

    /// The process monitor has no Codex TUI bridge marker.
    pub fn marker(&self) -> Result<Option<J>> {
        Ok(None)
    }

    /// Verify the registered shell identity only.
    ///
    /// `expected` is intentionally unused, matching the Python adapter: the
    /// shell identity is this process monitor's only continuity proof.
    pub fn verify_tui(&self, _expected: &str) -> Result<J> {
        let processes = self.processes()?;
        self.verify_identity(&processes)
    }

    fn verify_identity(&self, processes: &J) -> Result<J> {
        let expected = self.shell_pid()?;
        if model::u64_field(processes, "shell_pid") != Some(expected) {
            return Err(FlowError::guard("pane shell changed"));
        }
        let pid = u32::try_from(expected)
            .map_err(|_| FlowError::guard("shell PID is not a valid process id"))?;
        let identity = (self.process_identity)(pid)?;
        if model::u64_field(&identity, "pid") != Some(expected) {
            return Err(FlowError::guard("shell process died"));
        }
        Ok(identity)
    }

    /// The process monitor cannot snapshot runtime settings.
    pub fn snapshot(&self, _session: Option<&str>) -> Result<J> {
        Err(FlowError::guard(
            "OpenCode 1.19.3 does not support runtime snapshots",
        ))
    }

    /// The process monitor cannot invoke Codex app-server RPC methods.
    pub fn rpc(&self, method: &str, _params: &J) -> Result<J> {
        Err(FlowError::guard(format!(
            "OpenCode 1.19.3 does not support RPC method: {method}"
        )))
    }

    /// Prompting requires the separately registered native session adapter.
    pub fn prompt(&self, _text: &str) -> Result<String> {
        Err(FlowError::guard(
            "OpenCode 1.19.3 prompt requires manual input or file-based handoff",
        ))
    }

    /// OpenCode exit is not automated.
    pub fn exit_tui(&self) -> Result<()> {
        Err(FlowError::guard(
            "OpenCode 1.19.3 exit requires manual Ctrl+C or process signal",
        ))
    }

    /// OpenCode successor launch is not automated.
    pub fn launch_successor<P, Q>(&self, _root: P, _handoff_path: Q) -> Result<()>
    where
        P: AsRef<Path>,
        Q: AsRef<Path>,
    {
        if !self.shell_ready()? {
            return Err(FlowError::guard("OpenCode launch requires idle shell"));
        }
        Err(FlowError::guard(
            "OpenCode 1.19.3 successor launch not automated; manual restart required",
        ))
    }

    /// OpenCode cannot restore runtime settings programmatically.
    pub fn restore_runtime(&self, _expected: &J, _session: &str) -> Result<()> {
        Err(FlowError::guard(
            "OpenCode 1.19.3 does not support runtime restoration",
        ))
    }
}

/// The exact OpenCode 1.19.3 capability baseline from `opencode_adapter.py`.
pub fn baseline_capabilities() -> J {
    json!({
        "available": false,
        "probe_required": true,
        "adapter": ADAPTER_NAME,
        "server_info": {"name": "opencode", "version": "1.19.3"},
        "missing": [
            "app-server RPC (no thread/read, thread/resume, thread/settings/update)",
            "TUI bridge protocol (no session probing)",
            "runtime snapshot (no exact setting restoration)",
            "automatic review policy control",
            "context telemetry"
        ],
        "hints": [
            "Monitoring diagnostics verify the shell only, not OpenCode session continuity",
            "Safe handoff: monitoring phase only (no active writes, no pending reviews)",
            "Recovery: manual inspection if session dies during handoff",
            "Context management: OpenCode handles compression internally"
        ],
        "supported_operations": [
            "pane process identity verification",
            "shell readiness check"
        ]
    })
}

fn default_runner(argv: &[String], timeout: Duration, transient: bool) -> Result<Vec<u8>> {
    match command(argv, None, timeout, transient) {
        Ok(stdout) => Ok(stdout),
        // The Python factory reports timeouts as TransientError, not as the
        // generic TimeoutExpired boundary.
        Err(error) if error.kind == ErrorKind::Timeout => Err(FlowError::new(
            ErrorKind::Transient,
            format!(
                "{} timed out",
                argv.first().map(String::as_str).unwrap_or("command")
            ),
        )),
        Err(error) => Err(error),
    }
}

/// Factory: create an adapter with the standard herdr and `/proc` boundaries.
pub fn create_opencode_adapter(state: &J) -> Result<OpenCodeAdapter<'static>> {
    Ok(OpenCodeAdapter::new(
        state,
        default_runner,
        util::process_identity,
    ))
}

const USAGE: &str = "Usage: herdr-flow opencode <action> [options]\n\
Actions:\n\
  register --state-dir DIR --spec FILE  bind a native OpenCode 1.19.3 session\n\
  status --state-dir DIR                verify native identity and readiness\n\
  dispatch --state-dir DIR --spec FILE  submit a bound managed task once\n\
  collect --state-dir DIR               collect the bound result and signal\n\
  watch --state-dir DIR [--once]        reconcile native submissions\n\
  capabilities [--state-dir DIR]  print the OpenCode 1.19.3 capability baseline\n\
  verify --state-dir DIR --pane PANE --terminal TERM --shell-pid PID\n\
                                  verify pane, process and shell identity (read-only)";

fn parse_value(args: &[String], index: &mut usize) -> Result<String> {
    *index += 1;
    args.get(*index)
        .cloned()
        .ok_or_else(|| FlowError::usage("missing option value"))
}

fn cli_error(error: FlowError) -> i32 {
    util::write_stderr(&format!("{}: {}\n", error.kind.type_name(), error));
    2
}

fn print_usage() -> i32 {
    println!("{USAGE}");
    0
}

fn capabilities_cli(args: &[String]) -> Result<()> {
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            // Accepted for interface symmetry with `verify`; the baseline is
            // independent of the on-disk state.
            "--state-dir" => {
                let _ = parse_value(args, &mut index)?;
            }
            other => return Err(FlowError::usage(format!("unrecognized argument: {other}"))),
        }
        index += 1;
    }
    let text = serde_json::to_string_pretty(&baseline_capabilities())
        .map_err(|error| FlowError::new(ErrorKind::Json, error.to_string()))?;
    println!("{text}");
    Ok(())
}

fn load_state_context(root: &Path) -> Result<J> {
    let path = root.join("state.json");
    match util::read_json_if_exists(&path)? {
        Some(Value::Object(map)) => Ok(Value::Object(map)),
        Some(_) => Err(FlowError::guard(format!(
            "{} must contain a JSON object",
            path.display()
        ))),
        None => Ok(json!({})),
    }
}

#[derive(Default)]
struct VerifyArgs {
    state_dir: Option<PathBuf>,
    pane: Option<String>,
    terminal: Option<String>,
    shell_pid: Option<u64>,
}

fn verify_cli(args: &[String]) -> Result<()> {
    let parsed = parse_verify_args(args)?;
    let (state_dir, pane, terminal, shell_pid) = require_verify_args(parsed)?;
    let root = util::resolve_path(&state_dir);
    let mut state = load_state_context(&root)?;
    configure_verify_state(&mut state, &root, pane, terminal, shell_pid);
    let adapter = create_opencode_adapter(&state)?;
    let output = verify_output(&adapter)?;
    print_verify_output(&output)
}

fn parse_verify_args(args: &[String]) -> Result<VerifyArgs> {
    let mut parsed = VerifyArgs::default();
    let mut index = 0;
    while index < args.len() {
        parsed = parse_verify_option(parsed, args, &mut index)?;
        index += 1;
    }
    Ok(parsed)
}

fn parse_verify_option(
    parsed: VerifyArgs,
    args: &[String],
    index: &mut usize,
) -> Result<VerifyArgs> {
    let update = match args[*index].as_str() {
        "--state-dir" => set_state_dir(parsed, args, index),
        "--pane" => set_pane(parsed, args, index),
        "--terminal" => set_terminal(parsed, args, index),
        "--shell-pid" => set_shell_pid(parsed, args, index),
        other => Err(FlowError::usage(format!("unrecognized argument: {other}"))),
    };
    update
}

fn set_state_dir(mut parsed: VerifyArgs, args: &[String], index: &mut usize) -> Result<VerifyArgs> {
    parsed.state_dir = Some(PathBuf::from(parse_value(args, index)?));
    Ok(parsed)
}

fn set_pane(mut parsed: VerifyArgs, args: &[String], index: &mut usize) -> Result<VerifyArgs> {
    parsed.pane = Some(parse_value(args, index)?);
    Ok(parsed)
}

fn set_terminal(mut parsed: VerifyArgs, args: &[String], index: &mut usize) -> Result<VerifyArgs> {
    parsed.terminal = Some(parse_value(args, index)?);
    Ok(parsed)
}

fn set_shell_pid(mut parsed: VerifyArgs, args: &[String], index: &mut usize) -> Result<VerifyArgs> {
    parsed.shell_pid = Some(parse_shell_pid(args, index)?);
    Ok(parsed)
}

fn parse_shell_pid(args: &[String], index: &mut usize) -> Result<u64> {
    let raw = parse_value(args, index)?;
    let pid = raw
        .parse::<u64>()
        .map_err(|_| FlowError::usage("invalid --shell-pid"))?;
    validate_shell_pid(pid)
}

fn validate_shell_pid(pid: u64) -> Result<u64> {
    if pid == 0 || pid > u32::MAX as u64 {
        return Err(FlowError::usage("invalid --shell-pid"));
    }
    Ok(pid)
}

fn require_verify_args(parsed: VerifyArgs) -> Result<(PathBuf, String, String, u64)> {
    let Some(state_dir) = parsed.state_dir else {
        return Err(FlowError::usage(
            "--state-dir, --pane, --terminal and --shell-pid are required",
        ));
    };
    let Some(pane) = parsed.pane else {
        return Err(FlowError::usage(
            "--state-dir, --pane, --terminal and --shell-pid are required",
        ));
    };
    let Some(terminal) = parsed.terminal else {
        return Err(FlowError::usage(
            "--state-dir, --pane, --terminal and --shell-pid are required",
        ));
    };
    let Some(shell_pid) = parsed.shell_pid else {
        return Err(FlowError::usage(
            "--state-dir, --pane, --terminal and --shell-pid are required",
        ));
    };
    Ok((state_dir, pane, terminal, shell_pid))
}

fn configure_verify_state(
    state: &mut J,
    root: &Path,
    pane: String,
    terminal: String,
    shell_pid: u64,
) {
    model::set(state, "state_dir", json!(root.display().to_string()));
    model::set(state, "pane", json!(pane));
    model::set(state, "terminal", json!(terminal));
    model::set(state, "shell_pid", json!(shell_pid));
    if model::get(state, "cwd").is_none() {
        model::set(state, "cwd", json!(root.display().to_string()));
    }
}

fn verify_output(adapter: &OpenCodeAdapter) -> Result<J> {
    let pane_info = adapter.pane()?;
    let process_info = adapter.processes()?;
    let shell_identity = adapter.verify_identity(&process_info)?;
    Ok(json!({
        "pane": pane_info,
        "process_info": process_info,
        "shell_identity": shell_identity
    }))
}

fn print_verify_output(output: &J) -> Result<()> {
    let text = serde_json::to_string_pretty(output)
        .map_err(|error| FlowError::new(ErrorKind::Json, error.to_string()))?;
    println!("{text}");
    Ok(())
}

/// Native session actions and legacy process diagnostics.
///
/// The original Python module had no CLI. These two actions are read-only:
/// `capabilities` prints the baseline JSON, and `verify` checks the pane,
/// process-info and shell identity against explicit expectations.
pub fn cli(args: &[String]) -> i32 {
    if args.first().is_some_and(|action| {
        ["register", "status", "dispatch", "collect", "watch"].contains(&action.as_str())
    }) {
        return session::cli(args);
    }
    let Some(action) = args.first() else {
        return cli_error(FlowError::usage(USAGE));
    };
    if matches!(action.as_str(), "-h" | "--help" | "help") {
        return print_usage();
    }
    let rest = &args[1..];
    if rest
        .iter()
        .any(|value| matches!(value.as_str(), "-h" | "--help"))
    {
        return print_usage();
    }
    let result = match action.as_str() {
        "capabilities" => capabilities_cli(rest),
        "verify" => verify_cli(rest),
        other => Err(FlowError::usage(format!(
            "unknown opencode action: {other}"
        ))),
    };
    match result {
        Ok(()) => 0,
        Err(error) => cli_error(error),
    }
}
