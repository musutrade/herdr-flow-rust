//! No-model observations made by the calling tool process, never a proxy runner.
use crate::{error::ErrorKind, model::J, util, FlowError, Result};
use serde_json::json;
use std::path::{Path, PathBuf};

fn require(ok: bool, reason: &str) -> Result<()> {
    if ok {
        Ok(())
    } else {
        Err(FlowError::usage(reason))
    }
}

fn text<'a>(spec: &'a J, key: &str) -> Result<&'a str> {
    spec[key]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| FlowError::usage("missing_configuration"))
}

fn absolute(spec: &J, key: &str) -> Result<PathBuf> {
    let path = PathBuf::from(text(spec, key)?);
    require(path.is_absolute(), "missing_configuration")?;
    Ok(path)
}

fn executable(spec: &J) -> Result<J> {
    let expected = std::fs::canonicalize(absolute(spec, "executable")?).map_err(io_error)?;
    let actual = std::env::current_exe()?;
    require(expected == actual, "executable_conflict")?;
    let digest = running_digest()?;
    require(digest == text(spec, "sha256")?, "executable_conflict")?;
    Ok(json!({"path":actual,"sha256":digest,"selection":"absolute",
        "shell_alias":"unknown","path_lookup":path_lookup(&actual)}))
}

#[cfg(target_os = "linux")]
fn running_digest() -> Result<String> {
    // Hash the executing inode, including after replacement of its disk path.
    Ok(util::pyjson::sha256_hex(&std::fs::read("/proc/self/exe")?))
}

#[cfg(not(target_os = "linux"))]
fn running_digest() -> Result<String> {
    Err(FlowError::usage("unknown"))
}

fn path_lookup(actual: &Path) -> &'static str {
    let paths = std::env::var_os("PATH").unwrap_or_default();
    let selected = std::env::split_paths(&paths)
        .map(|dir| dir.join("herdr-flow"))
        .find(|path| executable_file(path));
    match selected.and_then(|p| std::fs::canonicalize(p).ok()) {
        Some(path) if path == actual => "match",
        Some(_) => "conflict_overridden_by_absolute_selection",
        None => "missing_overridden_by_absolute_selection",
    }
}

fn executable_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.metadata()
        .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

fn namespace(name: &str) -> Option<PathBuf> {
    std::fs::read_link(format!("/proc/self/ns/{name}")).ok()
}

fn process_policy() -> J {
    let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    let mut selected = serde_json::Map::new();
    for line in status.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        if ["Uid", "NoNewPrivs", "Seccomp"].contains(&key) {
            selected.insert(key.to_string(), json!(value.trim()));
        }
    }
    J::Object(selected)
}

fn context() -> J {
    json!({"scope":"calling_process_only","pid":std::process::id(),
        "start_ticks":util::process::process_start_ticks(std::process::id()),
        "cwd":std::env::current_dir().ok(),
        "process_policy":process_policy(),
        "namespaces":{"mount":namespace("mnt"),"network":namespace("net"),
            "user":namespace("user"),"pid":namespace("pid")},
        "role_sandbox_equivalence":"unknown",
        "adapter_process_identity":"unknown",
        "repository_trust":"unknown"})
}

fn local(spec: &J) -> Result<J> {
    require(
        std::env::current_dir()?
            == std::fs::canonicalize(absolute(spec, "cwd")?).map_err(io_error)?,
        "cwd_mismatch",
    )?;
    crate::identity::environment(&spec["identity"])
        .map_err(|_| FlowError::usage("identity_mismatch"))?;
    executable(spec)
}

fn producer(root: &Path, queue: &J, spec: &J) -> Result<J> {
    let wait = text(spec, "wait")?;
    let record = &queue["waits"][wait];
    require(record["kind"] == "signal", "unsupported_channel")?;
    require(
        !crate::model::bool_or(record, "closed", true),
        "closed_wait",
    )?;
    let identity = crate::identity::required_producer(record)?;
    require(identity == spec["identity"], "identity_mismatch")?;
    let _permit = crate::queue::isolation::wait_permit(queue, record)?;
    probe_write(&crate::queue::artifacts(root)?.join("results"))?;
    Ok(
        json!({"status":"ready","path":"queue_signal_file","wait":wait,
        "write":"create_sync_remove","control_socket":"not_required",
        "signal_admission":"not_probed"}),
    )
}

fn probe_write(dir: &Path) -> Result<()> {
    // Exercise the required write path without publishing a signal or receipt.
    let probe = tempfile::Builder::new()
        .prefix(".readiness-")
        .tempfile_in(dir)
        .map_err(io_error)?;
    probe.as_file().sync_all().map_err(io_error)?;
    probe.close().map_err(io_error)?;
    Ok(())
}

fn channel(root: &Path, queue: &J, spec: &J) -> Result<J> {
    match text(spec, "role")? {
        "producer" => producer(root, queue, spec),
        _ => Err(FlowError::usage("unsupported_channel")),
    }
}

fn category(error: &FlowError) -> &str {
    match error.kind {
        ErrorKind::PolicyDenied => "policy_denied",
        ErrorKind::Timeout => "timeout",
        ErrorKind::Usage => usage_category(&error.message),
        _ => "unknown",
    }
}

fn usage_category(message: &str) -> &str {
    [
        "missing_configuration",
        "cwd_mismatch",
        "identity_mismatch",
        "endpoint_mismatch",
        "executable_conflict",
        "unsupported_channel",
        "closed_wait",
    ]
    .into_iter()
    .find(|known| *known == message)
    .unwrap_or("unknown")
}

fn io_error(error: std::io::Error) -> FlowError {
    if error.kind() == std::io::ErrorKind::NotFound {
        return FlowError::usage("missing_configuration");
    }
    let kind = match error.kind() {
        std::io::ErrorKind::PermissionDenied => ErrorKind::PolicyDenied,
        std::io::ErrorKind::TimedOut => ErrorKind::Timeout,
        _ => ErrorKind::Io,
    };
    FlowError::new(kind, "role path probe failed")
}

fn observation(root: &Path, queue: &J, spec: &J) -> J {
    let executable = match local(spec) {
        Ok(value) => value,
        Err(error) => return json!({"stage":"local","channel":{"status":category(&error)}}),
    };
    let channel =
        channel(root, queue, spec).unwrap_or_else(|error| json!({"status":category(&error)}));
    json!({"stage":"channel","executable":executable,"channel":channel})
}

/// Run directly inside the selected role's tool invocation. Successful local
/// checks never attest that another process has equivalent sandbox permissions.
pub fn check(root: &Path, spec: &J) -> Result<J> {
    let queue = crate::queue::load_queue(root)?;
    let observed = observation(root, &queue, spec);
    let status = overall_status(&observed);
    Ok(
        json!({"schema":1,"ready":false,"status":status,"active_wakeup":false,
        "context":context(),"observed":observed,
        "effective_arguments":{"action":"queue readiness","state_dir":root,
            "role":selected_role(spec),"identity":crate::identity::validate(&spec["identity"]).ok()},
        "limits":["caller context does not attest a future role tool sandbox",
            "repository trust has no supported no-model query",
            "shell aliases are not observable after exec"]}),
    )
}

fn overall_status(observed: &J) -> &'static str {
    match observed["channel"]["status"].as_str() {
        Some("ready" | "unknown") | None => "unknown",
        _ => "blocked",
    }
}

fn selected_role(spec: &J) -> Option<&str> {
    spec["role"]
        .as_str()
        .filter(|s| ["receiver", "producer"].contains(s))
}
