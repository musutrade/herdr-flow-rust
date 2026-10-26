//! Cooperative Codex context monitoring and acknowledged Herdr session rotation.

use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::error::{ErrorKind, FlowError, Result};
use crate::logging;
use crate::model::{self, J};
use crate::util;

mod advice;
mod checkpoint;
mod cli;
mod diagnosis;
mod handoff;
mod observation;
mod registration;
mod review;
mod rotation;
mod transcript;
mod watch;

pub use advice::{advice_status, advise, wait_advice};
pub use checkpoint::{checkpoint, validate_fresh_successor, validate_ready};
pub use cli::cli;
pub use diagnosis::diagnosis;
pub use handoff::{handoff_digest, verify_handoff};
pub use registration::{init, InitRequest};
pub use review::{configure_review, reconcile_review, review_events};
pub use rotation::{ack, once, reconcile};
pub use transcript::{find_transcript, transcript, transcript_state};
pub use watch::{guarded_once, validate_watch, watch};

use observation::observe_context;
use registration::bind_runtime;
use review::external_delivery_pending;

pub const ROTATION_PHASES: &[&str] = &[
    "tui_exit_sending",
    "awaiting_shell",
    "tui_launch_sending",
    "awaiting_tui",
    "runtime_restore_sending",
    "successor_ready",
    "resume_sending",
    "awaiting_ack",
];

pub const OBSERVATION_PHASES: &[&str] = &[
    "monitoring",
    "requested",
    "ready",
    "parked",
    "awaiting_shell",
    "awaiting_tui",
    "successor_ready",
    "awaiting_ack",
];

pub const TRANSIENT_LIMIT: u64 = 3;
pub const HEAD_BYTES: usize = 4096;
pub const STDERR_LIMIT: usize = 2048;

pub fn guard_error(message: impl Into<String>) -> FlowError {
    FlowError::guard(message)
}

pub fn scope(g: &J) -> J {
    let mut scope = json!({
        "task": g.get("task").cloned().unwrap_or(Value::Null),
        "role": g.get("role").cloned().unwrap_or(Value::Null),
        "pane": g.get("pane").cloned().unwrap_or(Value::Null),
        "terminal": g.get("terminal").cloned().unwrap_or(Value::Null),
        "socket": g.get("socket").cloned().unwrap_or(Value::Null),
        "cwd": g.get("cwd").cloned().unwrap_or(Value::Null)
    });
    if crate::task_runtime::native::receiver::is_task_owned(g) {
        scope["codex_runtime"] = json!({"adapter":g["adapter"],"session":g["session"],
            "runtime":g["runtime"],"instance":g["instance"],"cwd":g["cwd"],"model":g["model"]});
    }
    scope
}

pub fn locked(root: &Path) -> Result<util::FileLock> {
    lock_state(root, true)
}

/// Lock an existing guard without recreating a retired receiver directory.
pub fn locked_existing(root: &Path) -> Result<util::FileLock> {
    lock_state(root, false)
}

fn lock_state(root: &Path, create: bool) -> Result<util::FileLock> {
    let permit = crate::lifecycle::runtime_permit(root)?;
    let root = util::state_dir(root, create)?;
    match util::FileLock::acquire(&root.join("guard.lock"), false)? {
        Some(lock) => Ok(lock.with_lifecycle(permit)),
        None => Err(FlowError::new(
            ErrorKind::StateBusy,
            "guard state is busy; retry after this poll",
        )),
    }
}

/// Probe an existing guard lock without creating, changing or removing it.
pub fn probe(root: &Path) -> Result<util::LockProbe> {
    let root = util::state_dir(root, false)?;
    util::probe_existing_lock(&root.join("guard.lock"))
}

pub fn save(root: &Path, s: &mut J) -> Result<()> {
    let _permit = crate::lifecycle::runtime_permit(root)?;
    util::ensure_private_dir(root, true)?;
    s["updated_at"] = json!(util::now());
    util::atomic_write_json(&util::resolve_path(root).join("state.json"), s)
}

fn read_state(root: &Path) -> Result<J> {
    util::read_state_json(root, "state.json")
}

pub fn checkout_root(cwd: &Path) -> PathBuf {
    util::checkout_root(cwd)
}

pub fn inside_checkout(path: &Path, cwd: &Path) -> bool {
    util::inside_checkout(path, cwd)
}

pub fn fingerprint(cwd: &str) -> Result<String> {
    util::fingerprint(Path::new(cwd))
}

fn load_guard_state(root: &Path) -> Result<J> {
    let s = read_state(root)?;
    load_guard_version(root, s)
}

fn load_guard_version(root: &Path, s: J) -> Result<J> {
    if model::int(&s, "version") != Some(2) {
        return Err(guard_error("legacy guard requires explicit migrate"));
    }
    load_guard_binding(root, s)
}

fn load_guard_binding(root: &Path, s: J) -> Result<J> {
    let Some(binding) = model::string(&s, "binding") else {
        return Ok(s);
    };
    load_guard_binding_path(root, s, &binding)
}

fn load_guard_binding_path(root: &Path, s: J, binding: &str) -> Result<J> {
    if !Path::new(binding).exists() {
        return Ok(s);
    }
    load_guard_registration(root, s, binding)
}

fn load_guard_registration(root: &Path, s: J, binding: &str) -> Result<J> {
    let registration = util::read_json(Path::new(binding))?;
    load_guard_registration_check(root, s, &registration)
}

fn load_guard_registration_check(root: &Path, s: J, registration: &J) -> Result<J> {
    if model::string(registration, "state_dir")
        != Some(util::resolve_path(root).display().to_string())
    {
        return Err(guard_error("pane guard registration changed"));
    }
    Ok(s)
}

fn validate_path_for_shell(path: &Path) -> Result<String> {
    let text = path.display().to_string();
    if text
        .chars()
        .any(|character| matches!(character, ';' | '&' | '|' | '$' | '`' | '\n' | '\r'))
    {
        logging::log_event(
            "guard_path_rejected",
            "context_guard",
            "path contains shell metacharacters",
        );
        return Err(guard_error("path contains shell metacharacters"));
    }
    let resolved = util::resolve_path(path);
    if !resolved.is_absolute() {
        logging::log_event(
            "guard_path_rejected",
            "context_guard",
            "path must be absolute",
        );
        return Err(guard_error(format!("path must be absolute: {text}")));
    }
    Ok(resolved.display().to_string())
}
