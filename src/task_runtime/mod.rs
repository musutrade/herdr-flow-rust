//! Task-owned runtimes and bounded attachments to existing upstream services.
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
pub mod configuration;
mod launcher;
pub use launcher::resolve as codex_namespace_launcher;
pub mod native;
pub mod review;
pub mod status;
pub(crate) mod upstream;

fn ensure(ok: bool, message: &str) -> Result<()> {
    if ok {
        Ok(())
    } else {
        Err(FlowError::guard(message))
    }
}

/// Prepare disposable runtime storage, without importing user configuration or
/// credentials. This does not establish support for an upstream login.
pub fn prepare(env: &TaskEnvironment, assignment: &str) -> Result<PathBuf> {
    let _permit = crate::task_env::isolation::mutation_permit(env)?;
    verify_instance_owner(env)?;
    ensure(
        env.owns_lock() && env.state == EnvironmentState::Active,
        "runtime requires an owned active environment",
    )?;
    validate_assignment(assignment)?;
    let root = env.temp_root.join("rt").join(assignment);
    crate::task_env::isolation::runtime_namespace(env, &root)?;
    crate::task_env::validate_path(env, &root)?;
    prepare_runtime_dirs(&root)?;
    Ok(root)
}

fn prepare_runtime_dirs(root: &Path) -> Result<()> {
    for name in [
        "home", "config", "data", "cache", "state", "tmp", "c", "runtime",
    ] {
        util::ensure_private_dir(&root.join(name), true)?;
    }
    Ok(())
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
    verify_instance_owner(env)?;
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
    if let Some(owner) = crate::task_env::isolation::owner(env) {
        command.env("HERDR_LIFECYCLE_OWNER", serde_json::to_string(&owner)?);
    }
    if let Some(creation) = &env.creation {
        command
            .env("HERDR_TASK_ID", &creation.binding.task_id)
            .env("HERDR_TASK_EPOCH", creation.binding.epoch.to_string())
            .env("HERDR_TASK_QUEUE", &creation.binding.queue_id);
    }
    for (key, dir) in [
        ("HOME", "home"),
        ("XDG_CONFIG_HOME", "config"),
        ("XDG_DATA_HOME", "data"),
        ("XDG_CACHE_HOME", "cache"),
        ("XDG_STATE_HOME", "state"),
        ("XDG_RUNTIME_DIR", "runtime"),
        ("TMPDIR", "tmp"),
        ("CODEX_HOME", "c"),
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
    Ok(command)
}

/// Start an explicitly labelled, credential-free isolated protocol fixture.
/// Real native launches require a supported upstream configuration contract.
/// A persisted starting marker makes ambiguous launch non-replayable.
pub fn launch(env: &mut TaskEnvironment, root: &Path, spec: &J) -> Result<Child> {
    configuration::validate_launch(spec)?;
    let permit = crate::task_env::isolation::mutation_permit(env)?;
    let _runtime_permit = crate::lifecycle::runtime_permit(root)?;
    let _lock = util::FileLock::acquire(&root.join("runtime.lock"), false)?
        .ok_or_else(|| FlowError::guard("runtime is busy"))?;
    launch_owned(env, root, spec, permit.as_deref())
}
fn launch_owned(
    env: &mut TaskEnvironment,
    root: &Path,
    spec: &J,
    permit: Option<&crate::lifecycle::Permit>,
) -> Result<Child> {
    ensure(
        !root.join("runtime.json").exists(),
        "runtime launch already attempted; inspect/recover, do not replay",
    )?;
    if upstream::selected(spec) {
        return upstream::attach(env, root, spec, permit);
    }
    let mut cmd = launch_command_permitted(env, root, spec, permit)?;
    let record = starting_record(env, root, spec)?;
    util::atomic_write_json(&root.join("runtime.json"), &record)?;
    let mut child = spawn_logged(root, &mut cmd)?;
    register_child(env, &mut child)?;
    finish_launch(env, root, spec, record, child)
}
fn starting_record(env: &TaskEnvironment, root: &Path, spec: &J) -> Result<J> {
    let mut record = spec.clone();
    if let Some(owner) = crate::task_env::instance::owner(env) {
        record["owner"] = owner;
    }
    record["lifecycle_owner"] = serde_json::to_value(crate::task_env::isolation::owner(env))?;
    record["instance"] = json!(env.instance_id);
    record["phase"] = json!("starting");
    record["root"] = json!(root);
    Ok(record)
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

fn launch_command_permitted(
    env: &TaskEnvironment,
    root: &Path,
    spec: &J,
    permit: Option<&crate::lifecycle::Permit>,
) -> Result<Command> {
    verify_instance_owner(env)?;
    let binary = PathBuf::from(required(spec, "binary")?);
    let cwd = PathBuf::from(required(spec, "cwd")?);
    validate_launch_cwd(env, root, &cwd, spec)?;
    configured_server_command(env, root, &cwd, &binary, spec, permit)
}

fn validate_launch_cwd(env: &TaskEnvironment, root: &Path, cwd: &Path, spec: &J) -> Result<()> {
    if review::selected(env, root, cwd, spec)? {
        return Ok(());
    }
    if env.creation.is_some() {
        ensure(
            cwd == env.workspace_root.join("checkout"),
            "instance runtime cwd must be its registered checkout",
        )?;
    }
    crate::task_env::validate_path(env, cwd)?;
    ensure(cwd.is_dir(), "runtime cwd is missing")
}

fn configured_server_command(
    env: &TaskEnvironment,
    root: &Path,
    cwd: &Path,
    binary: &Path,
    spec: &J,
    permit: Option<&crate::lifecycle::Permit>,
) -> Result<Command> {
    let mut cmd = server_command(env, root, cwd, binary, spec, permit)?;
    let adapter = required(spec, "adapter")?;
    configure_server(&mut cmd, root, &adapter)?;
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
    permit: Option<&crate::lifecycle::Permit>,
) -> Result<Command> {
    ensure(spec["adapter"] == "codex", "only Codex is supported")?;
    // Codex 0.163 uses an absolute /tmp/codex-daemon-UID fallback even with
    // CODEX_HOME/TMPDIR/XDG_RUNTIME_DIR redirected. A private mount is required.
    let launcher = launcher::resolve(spec)?;
    let mut cmd = namespace_environment(env, root, &launcher, permit)?;
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

fn namespace_environment(
    env: &TaskEnvironment,
    root: &Path,
    launcher: &Path,
    permit: Option<&crate::lifecycle::Permit>,
) -> Result<Command> {
    ensure(
        env.owns_lock() && env.state == EnvironmentState::Active,
        "runtime requires active environment",
    )?;
    crate::task_env::validate_path(env, root)?;
    configure_environment(
        env,
        root,
        env.permitted_command(launcher.as_os_str(), false, permit)?,
    )
}

fn configure_server(cmd: &mut Command, root: &Path, adapter: &str) -> Result<()> {
    match adapter {
        "codex" => {
            cmd.args(["app-server", "--listen"])
                .arg(format!("unix://{}", root.join("server.sock").display()));
        }
        _ => return Err(FlowError::guard("unsupported runtime adapter")),
    }
    Ok(())
}

pub fn verify(env: &TaskEnvironment, root: &Path) -> Result<J> {
    verify_environment(env, root)?;
    let record = util::read_state_json(root, "runtime.json")?;
    validate_runtime_record(env, root, &record)?;
    Ok(record)
}

pub(crate) fn verify_bounded(env: &TaskEnvironment, root: &Path, limit: usize) -> Result<J> {
    verify_environment(env, root)?;
    let record = util::read_state_json_bounded(root, "runtime.json", limit)?;
    validate_runtime_record(env, root, &record)?;
    Ok(record)
}

fn verify_environment(env: &TaskEnvironment, root: &Path) -> Result<()> {
    verify_instance_owner(env)?;
    crate::task_env::validate_path(env, root)?;
    ensure(
        env.owns_lock() && env.state == EnvironmentState::Active,
        "runtime environment is not active/owned",
    )
}

fn validate_runtime_record(env: &TaskEnvironment, root: &Path, record: &J) -> Result<()> {
    ensure(
        record["adapter"] == "codex",
        "unsupported runtime adapter; only Codex is supported",
    )?;
    verify_owner_record(env, record)?;
    ensure(
        record["instance"] == env.instance_id && record["root"] == json!(root),
        "runtime belongs to another environment",
    )?;
    if env.creation.is_some() || record.get("review_snapshot").is_some() {
        validate_launch_cwd(env, root, Path::new(&required(record, "cwd")?), record)?;
    }
    let pid = model::u64_field(record, "pid").unwrap_or(0) as i32;
    ensure(
        env.owns_process(pid),
        "runtime process is absent or unowned",
    )?;
    ensure(
        record["start_time"] == json!(crate::task_env::process_start_time(pid)),
        "runtime process identity changed",
    )
}

fn verify_instance_owner(env: &TaskEnvironment) -> Result<()> {
    if let Some(creation) = &env.creation {
        crate::task_env::instance::verify_instance(env, &creation.binding, &env.instance_id)?;
    }
    Ok(())
}

fn verify_owner_record(env: &TaskEnvironment, record: &J) -> Result<()> {
    ensure(
        record["lifecycle_owner"] == serde_json::to_value(crate::task_env::isolation::owner(env))?,
        "runtime lifecycle owner changed",
    )?;
    ensure(
        record.get("owner").filter(|v| !v.is_null()).cloned()
            == crate::task_env::instance::owner(env),
        "runtime owner queue/task/epoch/instance changed",
    )
}

pub(super) fn required(value: &J, key: &str) -> Result<String> {
    model::string(value, key)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| FlowError::guard(format!("runtime requires {key}")))
}

/// Re-establish only the transport process; creation/turn journals still forbid
/// replay. The caller must verify the existing session before resuming work.
pub fn start_broker(env: &mut TaskEnvironment, root: &Path, spec: &J) -> Result<()> {
    let permit = crate::task_env::isolation::mutation_permit(env)?;
    let _runtime_permit = crate::lifecycle::runtime_permit(root)?;
    let mut record = util::read_state_json(root, "runtime.json")?;
    ensure(
        record["adapter"] == "codex",
        "unsupported runtime adapter; only Codex is supported",
    )?;
    verify_owner_record(env, &record)?;
    ensure(
        record["instance"] == env.instance_id && record["root"] == json!(root),
        "broker restart runtime binding changed",
    )?;
    if let Some(binary) = spec.get("driver_binary") {
        record["driver_binary"] = binary.clone();
    }
    restart_broker(env, root, record, permit.as_deref())
}
fn restart_broker(
    env: &mut TaskEnvironment,
    root: &Path,
    record: J,
    permit: Option<&crate::lifecycle::Permit>,
) -> Result<()> {
    let child = broker_child(env, root, &record, permit)?;
    if upstream::selected(&record) {
        upstream::finish_attachment(root, record, child).map(|_| ())
    } else {
        Ok(())
    }
}
fn broker_child(
    env: &mut TaskEnvironment,
    root: &Path,
    spec: &J,
    permit: Option<&crate::lifecycle::Permit>,
) -> Result<Child> {
    let binary = model::string(spec, "driver_binary")
        .map(PathBuf::from)
        .unwrap_or(std::env::current_exe()?);
    let mut cmd = if upstream::selected(spec) {
        // Inherit the existing environment; the broker never needs a private
        // HOME or copies of upstream credentials. Landlock still confines writes.
        env.permitted_command(binary.as_os_str(), true, permit)?
    } else {
        configured_permitted_command(env, root, &binary, permit)?
    };
    cmd.args(["task-runtime-broker", "--state-dir"]).arg(root);
    let mut child = spawn_broker(root, cmd)?;
    register_child(env, &mut child)?;
    await_broker(root, &mut child)?;
    Ok(child)
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

fn configured_permitted_command(
    env: &TaskEnvironment,
    root: &Path,
    binary: &Path,
    permit: Option<&crate::lifecycle::Permit>,
) -> Result<Command> {
    verify_instance_owner(env)?;
    ensure(
        env.owns_lock() && env.state == EnvironmentState::Active,
        "runtime environment is not active/owned",
    )?;
    crate::task_env::validate_path(env, root)?;
    ensure(
        binary.is_absolute() && binary.is_file(),
        "runtime binary must be an absolute file",
    )?;
    configure_environment(
        env,
        root,
        env.permitted_command(binary.as_os_str(), true, permit)?,
    )
}

#[cfg(test)]
mod broker_start_tests {
    use super::*;
    #[test]
    fn readiness_requires_current_process_and_startup_failure_is_reported() {
        let dir = crate::util::fs::test_tempdir();
        let mut child = Command::new("true").spawn().unwrap();
        assert!(await_broker(dir.path(), &mut child)
            .unwrap_err()
            .message
            .contains("startup failed"));
        let mut child = Command::new("sleep").arg("30").spawn().unwrap();
        util::atomic_write_json(&dir.path().join("broker-status.json"), &json!({
            "pid":child.id(),"phase":"running","start_ticks":util::process::process_start_ticks(child.id())
        })).unwrap();
        let result = await_broker(dir.path(), &mut child);
        child.kill().unwrap();
        child.wait().unwrap();
        result.unwrap();
    }
    #[test]
    fn stale_readiness_times_out_and_reaps_the_spawned_process() {
        let dir = crate::util::fs::test_tempdir();
        let mut child = Command::new("sleep").arg("30").spawn().unwrap();
        util::atomic_write_json(
            &dir.path().join("broker-status.json"),
            &json!({
                "pid":child.id(),"phase":"running","start_ticks":"0"
            }),
        )
        .unwrap();
        assert!(await_broker(dir.path(), &mut child)
            .unwrap_err()
            .message
            .contains("deadline"));
        assert!(child.try_wait().unwrap().is_some());
    }
}
