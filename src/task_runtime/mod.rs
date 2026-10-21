//! Task-owned native runtimes. Shared servers cannot provide deletion guarantees.
use crate::{
    error::FlowError,
    model::{self, J},
    task_env::{EnvironmentState, TaskEnvironment},
    util, Result,
};
use serde_json::json;
use std::{
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
};
pub mod broker;
pub mod cli;
pub mod native;

fn ensure(ok: bool, message: &str) -> Result<()> {
    if ok {
        Ok(())
    } else {
        Err(FlowError::guard(message))
    }
}

/// Redirect default storage before starting a runtime; inherited configuration,
/// credentials, plugins and remote server addresses are intentionally not copied.
/// Callers may provision explicit configuration inside the returned private root.
pub fn prepare(env: &TaskEnvironment, assignment: &str) -> Result<PathBuf> {
    ensure(
        env.owns_lock() && env.state == EnvironmentState::Active,
        "runtime requires an owned active environment",
    )?;
    validate_assignment(assignment)?;
    let root = env.temp_root.join("rt").join(assignment);
    crate::task_env::validate_path(env, &root)?;
    for name in [
        "home", "config", "data", "cache", "state", "tmp", "c", "opencode", "runtime",
    ] {
        util::ensure_private_dir(&root.join(name), true)?;
    }
    Ok(root)
}

fn validate_assignment(assignment: &str) -> Result<()> {
    ensure(
        !assignment.is_empty()
            && assignment.len() <= 128
            && assignment
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
        "invalid runtime assignment",
    )
}

pub fn command(env: &TaskEnvironment, root: &Path, binary: &Path) -> Result<Command> {
    ensure(
        env.owns_lock() && env.state == EnvironmentState::Active,
        "runtime environment is not active/owned",
    )?;
    crate::task_env::validate_path(env, root)?;
    ensure(
        binary.is_absolute() && binary.is_file(),
        "runtime binary must be an absolute file",
    )?;
    configure_environment(env, root, env.command(binary))
}

fn configure_environment(
    env: &TaskEnvironment,
    root: &Path,
    mut command: Command,
) -> Result<Command> {
    let path = std::env::var_os("PATH").unwrap_or_default();
    command
        .env_clear()
        .env("PATH", path)
        .env(crate::task_env::MARKER_ENV, &env.instance_id);
    for (key, dir) in [
        ("HOME", "home"),
        ("XDG_CONFIG_HOME", "config"),
        ("XDG_DATA_HOME", "data"),
        ("XDG_CACHE_HOME", "cache"),
        ("XDG_STATE_HOME", "state"),
        ("XDG_RUNTIME_DIR", "runtime"),
        ("TMPDIR", "tmp"),
        ("CODEX_HOME", "c"),
        ("OPENCODE_CONFIG_DIR", "opencode"),
    ] {
        let target = root.join(dir);
        crate::task_env::validate_path(env, &target)?;
        ensure(target.is_dir(), "prepare runtime directories first")?;
        command.env(key, target);
    }
    for key in [
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "ALL_PROXY",
        "NO_PROXY",
        "http_proxy",
        "https_proxy",
        "all_proxy",
        "no_proxy",
        "SSL_CERT_FILE",
        "SSL_CERT_DIR",
    ] {
        if let Some(value) = std::env::var_os(key) {
            command.env(key, value);
        }
    }
    command.env("OPENCODE_DISABLE_PROJECT_CONFIG", "true");
    command.env("OPENCODE_DISABLE_CLAUDE_CODE", "true");
    command.env("OPENCODE_DISABLE_AUTOUPDATE", "true");
    command.env("OPENCODE_DISABLE_MODELS_FETCH", "true");
    command.env("OPENCODE_DISABLE_DEFAULT_PLUGINS", "true");
    Ok(command)
}

/// Start only a dedicated server. Its process and all defaults belong to env.
/// A persisted starting marker makes ambiguous launch non-replayable.
pub fn launch(env: &mut TaskEnvironment, root: &Path, spec: &J) -> Result<Child> {
    let _lock = util::FileLock::acquire(&root.join("runtime.lock"), false)?
        .ok_or_else(|| FlowError::guard("runtime is busy"))?;
    ensure(
        !root.join("runtime.json").exists(),
        "runtime launch already attempted; inspect/recover, do not replay",
    )?;
    let mut cmd = launch_command(env, root, spec)?;
    let mut record = spec.clone();
    record["instance"] = json!(env.instance_id);
    record["phase"] = json!("starting");
    record["root"] = json!(root);
    util::atomic_write_json(&root.join("runtime.json"), &record)?;
    let mut child = spawn_logged(root, &mut cmd)?;
    register_child(env, &mut child)?;
    finish_launch(env, root, spec, record, child)
}
fn finish_launch(
    env: &mut TaskEnvironment,
    root: &Path,
    spec: &J,
    mut record: J,
    child: Child,
) -> Result<Child> {
    record["pid"] = json!(child.id());
    record["start_time"] = json!(crate::task_env::process_start_time(child.id() as i32));
    record["phase"] = json!("started");
    util::atomic_write_json(&root.join("runtime.json"), &record)?;
    if spec["adapter"] == "codex" {
        start_broker(env, root, spec)?;
    }
    Ok(child)
}

fn spawn_logged(root: &Path, cmd: &mut Command) -> Result<Child> {
    let log = std::fs::File::create(root.join("server.log"))?;
    let child = cmd
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log)
        .spawn()?;
    Ok(child)
}

fn launch_command(env: &TaskEnvironment, root: &Path, spec: &J) -> Result<Command> {
    let binary = PathBuf::from(required(spec, "binary")?);
    let cwd = PathBuf::from(required(spec, "cwd")?);
    crate::task_env::validate_path(env, &cwd)?;
    ensure(cwd.is_dir(), "runtime cwd is missing")?;
    let mut cmd = server_command(env, root, &cwd, &binary, spec)?;
    let roots = vec![root.to_path_buf(), cwd.clone()];
    // SAFETY: the existing Landlock helper performs only confinement syscalls.
    if spec["adapter"] != "codex" {
        unsafe {
            use std::os::unix::process::CommandExt;
            cmd.pre_exec(move || crate::task_env::confine_renames_to(&roots));
        }
    }
    let adapter = required(spec, "adapter")?;
    configure_server(&mut cmd, root, spec, &adapter)?;
    cmd.current_dir(cwd);
    Ok(cmd)
}
fn register_child(env: &mut TaskEnvironment, child: &mut Child) -> Result<()> {
    if let Err(error) = env.register_process(child.id() as i32) {
        let _ = child.kill();
        let _ = child.wait();
        return Err(error);
    }
    Ok(())
}

fn server_command(
    env: &TaskEnvironment,
    root: &Path,
    cwd: &Path,
    binary: &Path,
    spec: &J,
) -> Result<Command> {
    if spec["adapter"] != "codex" {
        return command(env, root, binary);
    }
    // Codex 0.163 uses an absolute /tmp/codex-daemon-UID fallback even with
    // CODEX_HOME/TMPDIR/XDG_RUNTIME_DIR redirected. A private mount is required.
    let mut cmd = namespace_environment(env, root)?;
    cmd.args([
        "--unshare-user",
        "--ro-bind",
        "/",
        "/",
        "--dev",
        "/dev",
        "--proc",
        "/proc",
        "--bind",
    ])
    .arg(root.join("tmp"))
    .arg("/tmp")
    .arg("--bind")
    .arg(root)
    .arg(root)
    .arg("--bind")
    .arg(cwd)
    .arg(cwd)
    .arg("--")
    .arg(binary);
    Ok(cmd)
}

fn namespace_environment(env: &TaskEnvironment, root: &Path) -> Result<Command> {
    ensure(
        env.owns_lock() && env.state == EnvironmentState::Active,
        "runtime requires active environment",
    )?;
    crate::task_env::validate_path(env, root)?;
    configure_environment(env, root, env.namespace_command())
}

fn configure_server(cmd: &mut Command, root: &Path, spec: &J, adapter: &str) -> Result<()> {
    match adapter {
        "codex" => {
            cmd.args(["app-server", "--listen"])
                .arg(format!("unix://{}", root.join("server.sock").display()));
        }
        "opencode" => {
            let port = model::u64_field(spec, "port").unwrap_or(0);
            ensure(
                (1..=65535).contains(&port),
                "explicit OpenCode loopback port required",
            )?;
            cmd.args([
                "--print-logs",
                "--log-level",
                "DEBUG",
                "serve",
                "--hostname",
                "127.0.0.1",
                "--port",
            ])
            .arg(port.to_string());
        }
        _ => return Err(FlowError::guard("unsupported runtime adapter")),
    }
    Ok(())
}

pub fn verify(env: &TaskEnvironment, root: &Path) -> Result<J> {
    crate::task_env::validate_path(env, root)?;
    ensure(
        env.owns_lock() && env.state == EnvironmentState::Active,
        "runtime environment is not active/owned",
    )?;
    let record = util::read_state_json(root, "runtime.json")?;
    ensure(
        record["instance"] == env.instance_id && record["root"] == json!(root),
        "runtime belongs to another environment",
    )?;
    let pid = model::u64_field(&record, "pid").unwrap_or(0) as i32;
    ensure(
        env.owns_process(pid),
        "runtime process is absent or unowned",
    )?;
    ensure(
        record["start_time"] == json!(crate::task_env::process_start_time(pid)),
        "runtime process identity changed",
    )?;
    Ok(record)
}

pub(super) fn required(value: &J, key: &str) -> Result<String> {
    model::string(value, key)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| FlowError::guard(format!("runtime requires {key}")))
}

/// Re-establish only the transport process; creation/turn journals still forbid
/// replay. The caller must verify the existing session before resuming work.
pub fn start_broker(env: &mut TaskEnvironment, root: &Path, spec: &J) -> Result<()> {
    // The child owns the exclusive broker lock and validates any stale socket.
    // Existing creation/turn journals remain untouched during transport restart.
    let binary = model::string(spec, "driver_binary")
        .map(PathBuf::from)
        .unwrap_or(std::env::current_exe()?);
    let mut cmd = command(env, root, &binary)?;
    cmd.args(["task-runtime-broker", "--state-dir"]).arg(root);
    let mut child = spawn_broker(root, cmd)?;
    register_child(env, &mut child)?;
    await_broker(root, &mut child)
}
fn await_broker(root: &Path, child: &mut Child) -> Result<()> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        if child.try_wait()?.is_some() {
            return Err(FlowError::guard(
                "broker startup failed; inspect broker.log, do not replay native operations",
            ));
        }
        if broker_ready(root, child.id()) {
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            util::process::terminate_child(child);
            return Err(FlowError::guard("broker startup deadline exceeded"));
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}
fn broker_ready(root: &Path, pid: u32) -> bool {
    let Some(start_ticks) = util::process::process_start_ticks(pid) else {
        return false;
    };
    util::read_state_json(root, "broker-status.json").is_ok_and(|status| {
        status["pid"] == pid
            && status["phase"] == "running"
            && status["start_ticks"].as_str() == Some(start_ticks.as_str())
    })
}

fn spawn_broker(root: &Path, mut cmd: Command) -> Result<Child> {
    let log = std::fs::File::create(root.join("broker.log"))?;
    Ok(cmd
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log)
        .spawn()?)
}
