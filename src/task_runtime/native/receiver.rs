//! A fresh coordinator thread is an identity, not an activated receiver.
//! No queue mutation, model turn, approval response or implicit recovery.
use super::*;

pub fn is_task_owned(state: &J) -> bool {
    state["adapter"] == "codex" && state["receiver_kind"] == "task-runtime"
}

pub fn require_adapter(state: &J) -> Result<()> {
    ensure(
        !is_task_owned(state),
        "task-owned Codex receiver is not activated: adapter/watcher unavailable",
    )
}

/// Even a dangling symlink or an unreadable marker reserves the attempt.
pub(super) fn ensure_absent(path: &Path) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
        Ok(_) => Err(FlowError::guard(
            "native identity already reserved; inspect without replay",
        )),
    }
}

/// Exactly one thread/start attempt per runtime. A failed/crashed attempt is
/// retained, including failures after the native response or guard publication.
pub fn bootstrap(env: &TaskEnvironment, root: &Path, spec: &J) -> Result<J> {
    let runtime = super::super::verify(env, root)?;
    ensure(
        runtime["adapter"] == "codex",
        "receiver bootstrap requires Codex",
    )?;
    ensure(runtime["phase"] == "started", "runtime is not started")?;
    let guard = guard_path(env, root, &runtime, spec)?;
    let _runtime_lock = crate::guard::locked(root)?;
    let _guard_lock = crate::guard::locked(&guard)?;
    for name in ["receiver.json", "creation.json", "state.json"] {
        ensure_absent(&root.join(name))?;
    }
    for name in ["state.json", "receiver.json", "creation.json", "watch.json"] {
        ensure_absent(&guard.join(name))?;
    }
    // Validate known local preconditions before reserving a native attempt.
    client(root)?;
    let mut state = initial_state(root, &guard, &runtime, spec);
    approval_policy(&state)?;
    required(&state["model"], "id")?;
    required(&state["model"], "providerID")?;
    let mut journal = json!({"version":1,"phase":"creating","runtime":runtime,
        "guard_dir":guard,"state":state});
    let path = root.join("receiver.json");
    util::atomic_write_json(&path, &journal)?;
    match finish(root, &guard, &mut state, &mut journal) {
        Ok(()) => Ok(state),
        Err(error) => {
            journal["phase"] = json!("uncertain");
            journal["state"] = state;
            journal["error_kind"] = json!(error.kind.type_name());
            // Preserve the original failure; the creating marker also blocks replay.
            let _ = util::atomic_write_json(&path, &journal);
            Err(error)
        }
    }
}

fn guard_path(env: &TaskEnvironment, root: &Path, runtime: &J, spec: &J) -> Result<PathBuf> {
    let guard = PathBuf::from(required(spec, "guard_dir")?);
    crate::task_env::validate_path(env, &guard)?;
    let resolved = util::resolve_path(&guard);
    let runtime_root = util::resolve_path(root);
    let cwd = PathBuf::from(required(runtime, "cwd")?);
    crate::task_env::validate_path(env, &cwd)?;
    ensure(
        guard.is_absolute()
            && guard == resolved
            && root == runtime_root
            && guard != runtime_root
            && util::is_relative_to(&guard, &runtime_root)
            && !crate::task_env::paths_overlap(&guard, &cwd),
        "receiver guard must be a canonical directory below runtime, outside cwd",
    )?;
    Ok(guard)
}

fn initial_state(root: &Path, guard: &Path, runtime: &J, spec: &J) -> J {
    json!({"version":2,"adapter":"codex","receiver_kind":"task-runtime",
        "state_dir":guard,"runtime":root,"instance":runtime["instance"],
        "runtime_pid":runtime["pid"],"runtime_start_time":runtime["start_time"],
        "cwd":runtime["cwd"],"model":runtime["model"],
        "approval_policy":runtime.get("approval_policy").cloned().unwrap_or(json!("untrusted")),
        "task":spec["task"],"role":spec["role"],"auto_rotate":false,"phase":"bootstrap"})
}

fn finish(root: &Path, guard: &Path, state: &mut J, journal: &mut J) -> Result<()> {
    create_codex(root, state)?;
    journal["state"] = state.clone();
    journal["phase"] = json!("created");
    util::atomic_write_json(&root.join("receiver.json"), journal)?;
    crate::guard::save(guard, state)?;
    journal["phase"] = json!("bound");
    util::atomic_write_json(&root.join("receiver.json"), journal)
}
