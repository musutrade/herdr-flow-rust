//! A task-owned observer independent of the receiver watcher and broker.
//! Only an incident starts a model turn. Unknown sends are never replayed.
use super::*;
use crate::lifecycle::Owner;
mod agent;
mod observe;
mod repair;
mod report;
#[cfg(test)]
mod tests;

const LIMIT: usize = 1024 * 1024;

fn read(root: &Path, name: &str) -> Result<Option<J>> {
    util::read_state_json_bounded_if_exists(root, name, LIMIT)
}

fn save(root: &Path, name: &str, value: &J) -> Result<()> {
    ensure(
        serde_json::to_vec(value)?.len() <= LIMIT,
        "recovery state exceeds current storage bound",
    )?;
    util::atomic_write_json(&root.join(name), value)
}

/// Activation is explicit through the configured recovery role. Older tasks
/// remain usable and report the missing configuration without substituting it.
pub fn start(env: &mut TaskEnvironment, coordinator: &Path) -> Result<J> {
    let permit = crate::task_env::isolation::mutation_permit(env)?;
    let runtime = super::verify(env, coordinator)?;
    if runtime["role_models"].get("recovery").is_none() {
        return Ok(
            json!({"phase":"not_configured","reason":"explicit recovery role model required"}),
        );
    }
    let context = observer_context(env, coordinator, &runtime)?;
    let root = coordinator.join("recovery-observer");
    crate::task_env::isolation::runtime_namespace(env, &root)?;
    let agent = initialize(&root, &context, env)?;
    let pid = spawn(env, &root, &runtime, permit.as_deref())?;
    Ok(json!({"phase":"watching","pid":pid,"state_dir":root,
        "model":runtime["role_models"]["recovery"],"agent":agent}))
}

fn observer_context(env: &TaskEnvironment, coordinator: &Path, runtime: &J) -> Result<J> {
    let owner = crate::task_env::isolation::owner(env)
        .ok_or_else(|| FlowError::guard("recovery requires an isolated task owner"))?;
    ensure(
        upstream::selected(runtime),
        "recovery requires the existing upstream service",
    )?;
    role_models::validate_model(&runtime["role_models"]["recovery"])?;
    Ok(
        json!({"owner":owner,"coordinator":coordinator,"runtime":runtime,
        "runtime_parent":env.temp_root.join("rt"),"observer":coordinator.join("recovery-observer")}),
    )
}

fn initialize(root: &Path, context: &J, env: &mut TaskEnvironment) -> Result<J> {
    let _lock = crate::guard::locked(root)?;
    native::receiver::ensure_absent(&root.join("observer.json"))?;
    save(root, "observer.json", context)?;
    // Prepare only the transport/session while registration is available.
    // A later blocked Git command may hold the environment lock: incident
    // diagnosis must not depend on reacquiring that same lock.
    Ok(match agent::prepare_session(root, context, env) {
        Ok(value) => value,
        Err(error) => json!({"phase":"unavailable","error":error.to_string()}),
    })
}

fn spawn(
    env: &mut TaskEnvironment,
    root: &Path,
    runtime: &J,
    permit: Option<&crate::lifecycle::Permit>,
) -> Result<u32> {
    let binary = runtime
        .get("driver_binary")
        .cloned()
        .unwrap_or(json!(std::env::current_exe()?));
    let binary = PathBuf::from(required(&json!({"binary":binary}), "binary")?);
    // This is the trusted control process, like the queue driver: it must
    // persist environment registration outside the disposable roots. The
    // recovery model still runs through its own confined broker/read-only turn.
    let mut cmd = env.permitted_command(binary.as_os_str(), false, permit)?;
    cmd.args(["task-runtime", "recovery-watch", "--state-dir"])
        .arg(root);
    let mut child = super::spawn_logged(root, &mut cmd)?;
    super::register_child(env, &mut child)?;
    native::receiver::active::await_watch(root, &mut child)?;
    let pid = child.id();
    util::process::BackgroundChild {
        pid,
        child,
        ready: None,
    }
    .reap();
    Ok(pid)
}

pub fn status(coordinator: &Path) -> Result<J> {
    let _permit = crate::lifecycle::runtime_permit(coordinator)?
        .ok_or_else(|| FlowError::guard("recovery status requires an isolated task Owner"))?;
    let root = coordinator.join("recovery-observer");
    if !root.try_exists()? {
        return Ok(json!({"phase":"not_configured"}));
    }
    snapshot(&root)
}

fn snapshot(root: &Path) -> Result<J> {
    // First publication creates a protected directory before its marker and
    // payload. Serialize with the writer instead of inspecting that interval.
    let Some(_lock) = lock_state(root)? else {
        return Ok(json!({"phase":"updating",
            "next_step":"Read recovery status again after the current observation; do not resend any operation."}));
    };
    if read(root, "observer.json")?.is_none() {
        return Ok(json!({"phase":"not_configured"}));
    }
    Ok(json!({"observer":crate::watch_service::health(root),
        "incident":read(root,"incident.json")?,"error":read(root,"observer-error.json")?}))
}

fn lock_state(root: &Path) -> Result<Option<util::FileLock>> {
    match crate::guard::locked_existing(root) {
        Ok(lock) => Ok(Some(lock)),
        Err(error) if error.kind == crate::ErrorKind::StateBusy => Ok(None),
        Err(error) => Err(error),
    }
}

fn context(root: &Path) -> Result<J> {
    let context = util::read_state_json_bounded(root, "observer.json", LIMIT)?;
    ensure(
        context["observer"] == json!(root),
        "recovery observer path changed",
    )?;
    let owner: Owner = serde_json::from_value(context["owner"].clone())?;
    ensure(
        std::env::var(crate::task_env::MARKER_ENV).ok().as_deref() == Some(&owner.instance_id),
        "recovery observer must be a registered task process",
    )?;
    Ok(context)
}

pub fn watch(root: &Path) -> Result<()> {
    let context = context(root)?;
    crate::watch_service::run(root, || tick(root, &context), || Ok(()), 2, None, false)
}

fn tick(root: &Path, context: &J) -> Result<bool> {
    let owner: Owner = serde_json::from_value(context["owner"].clone())?;
    let q = crate::queue::load_queue(&owner.queue_root)?;
    if terminal(&q, &owner) {
        return Ok(true);
    }
    let permit = match crate::lifecycle::write_permit(&owner) {
        Ok(permit) => permit,
        Err(error) if error.kind == crate::ErrorKind::StateBusy => return Ok(false),
        Err(error) => return Err(error),
    };
    // A status reader may briefly own this lock. Keep the observer alive and
    // leave all operations untouched until the next tick.
    let Some(_lock) = lock_state(root)? else {
        return Ok(false);
    };
    observe_current(root, context, &permit)?;
    Ok(false)
}

fn observe_current(root: &Path, context: &J, permit: &crate::lifecycle::Permit) -> Result<()> {
    // A human request may have committed between the first read and SH.
    let q = crate::queue::load_queue(&permit.owner().queue_root)?;
    record_cycle(root, context, &q, permit)
}

fn record_cycle(root: &Path, context: &J, q: &J, permit: &crate::lifecycle::Permit) -> Result<()> {
    match cycle(root, context, q, permit) {
        Ok(()) => (),
        Err(error) if error.kind == crate::ErrorKind::StateBusy => (),
        Err(error) => {
            let failure = json!({"phase":"unavailable","message":error.to_string(),
                "next_step":"Inspect the saved incident and original operation; do not resend it."});
            save(root, "observer-error.json", &failure)?;
            report::notice(context, "recovery unavailable");
        }
    }
    Ok(())
}

fn terminal(q: &J, owner: &Owner) -> bool {
    let task = &q["tasks"][&owner.task_id];
    task.is_null() || !task["final_outcome"].is_null() || !task["cleanup_obligation"].is_null()
}

fn cycle(root: &Path, context: &J, q: &J, permit: &crate::lifecycle::Permit) -> Result<()> {
    permit.check(q, false)?;
    let incident = read(root, "incident.json")?;
    if let Some(mut incident) = incident.filter(|i| i["phase"] != "handled") {
        return advance(root, context, &mut incident, q);
    }
    scan_idle(root, context, q)
}

fn scan_idle(root: &Path, context: &J, q: &J) -> Result<()> {
    if let Some(fault) = super::human::verification_fault(context, q)? {
        return record_fault(root, context, fault);
    }
    if super::human::pending(context, q)? {
        return Ok(());
    }
    match observe::scan(context, q)? {
        Some(fault) => record_fault(root, context, fault),
        None => clear_seen(root),
    }
}

fn clear_seen(root: &Path) -> Result<()> {
    let Some(mut incident) = read(root, "incident.json")? else {
        return Ok(());
    };
    if incident["seen"] != json!({}) {
        // A later outage is a new episode only after a healthy observation.
        incident["seen"] = json!({});
        save(root, "incident.json", &incident)?;
    }
    Ok(())
}

fn record_fault(root: &Path, context: &J, fault: J) -> Result<()> {
    let previous = read(root, "incident.json")?.unwrap_or(json!({}));
    let mut seen = previous.get("seen").cloned().unwrap_or(json!({}));
    let key = required(&fault, "source")?;
    if seen[&key] == fault["id"] {
        return Ok(());
    }
    seen[key] = fault["id"].clone();
    // The incident and its deduplication marker commit together. A crash must
    // not leave a diagnosable event without the marker preventing another send.
    let incident = json!({"id":util::pyjson::uuid_hex(),"phase":"pending","fault":fault,
        "observed_at":util::now(),"handoff":{"phase":"pending"},"seen":seen});
    save(root, "incident.json", &incident)?;
    report::notice(context, "recovery pending");
    Ok(())
}

fn advance(root: &Path, context: &J, incident: &mut J, q: &J) -> Result<()> {
    advance_operation(root, context, incident)?;
    if incident["phase"] == "reported" {
        report::handoff(root, context, incident, q)?;
    }
    Ok(())
}

fn advance_operation(root: &Path, context: &J, incident: &mut J) -> Result<()> {
    match incident["phase"].as_str() {
        Some("pending") => agent::begin(root, context, incident)?,
        Some("submitted") => agent::collect(root, context, incident)?,
        Some("repairing") => repair::advance(root, context, incident)?,
        Some("sending") => {
            incident["phase"] = json!("unavailable");
            incident["next_step"] = json!("Recovery send outcome is unknown; inspect the original recovery session before any retry.");
            save(root, "incident.json", incident)?;
            report::notice(context, "recovery unavailable");
        }
        _ => (),
    }
    Ok(())
}
