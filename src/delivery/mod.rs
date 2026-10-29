//! Customer execution and disposable recovery state. Completion leaves no record.
use crate::{
    model::{self, J},
    progress::{artifact::ArtifactAdapter, lineage},
    task_env::{EnvironmentState, TaskEnvironment},
    util, FlowError, Result,
};
use serde_json::json;
use std::path::{Path, PathBuf};
mod cleanup;
pub mod cli;
pub mod local;
pub mod publication;
mod stage;
pub use stage::complete as complete_stage;

/// Only a customer adapter may establish an external outcome, by inspecting the
/// actual endpoint. A normal process exit or a coordinator's claim is not proof.
/// Implementations must make preflight/observe non-submitting checks and report
/// uncertainty as Unknown. Only execute may submit. An execute error may still
/// mean the endpoint acted; observe must query external state before any retry.
/// These semantic requirements are supplied by the trusted adapter, not enforced
/// by the Rust type system.
pub trait Executor {
    fn preflight(&self, intent: &J) -> Result<()>;
    fn execute(&self, intent: &J) -> Result<()>;
    fn observe(&self, intent: &J) -> Result<Observation>;
}

#[derive(Debug)]
pub enum Observation {
    Delivered(J),
    Absent,
    Failed(String),
    Unknown(String),
}

fn ensure(ok: bool, message: &str) -> Result<()> {
    if ok {
        Ok(())
    } else {
        Err(FlowError::progress(message))
    }
}
fn field(value: &J, key: &str) -> Result<String> {
    model::string(value, key)
        .filter(|s| !s.is_empty() && s.len() <= 4096)
        .ok_or_else(|| FlowError::progress(format!("delivery requires {key}")))
}

/// Persist first, then freeze the parent under its queue lock. Retrying this
/// preparation is safe; it never calls Executor::execute.
/// Requires an owned active environment and validated artifact/target/approval.
/// Acquires the queue lock internally; callers must not hold that same lock.
/// Artifact verification and adapter preflight run outside the queue lock. The
/// same parent and fresh cleanup references must still validate before commit.
/// Journal and queue writes are separate: an error can leave intent persisted
/// before the queue is frozen. Keep ownership and reconcile/retry preparation;
/// do not infer submission from preparation. Executor preflight must be read-only.
pub fn prepare(
    env: &mut TaskEnvironment,
    queue: &Path,
    spec: &J,
    executor: &dyn Executor,
) -> Result<J> {
    let _permit = crate::task_env::isolation::mutation_permit(env)?;
    ensure(
        env.owns_lock() && env.state == EnvironmentState::Active,
        "delivery requires owned active environment",
    )?;
    stage_intent(env, queue, spec, executor)?;
    Ok(env.delivery.clone())
}
fn stage_intent(
    env: &mut TaskEnvironment,
    queue: &Path,
    spec: &J,
    executor: &dyn Executor,
) -> Result<()> {
    crate::task_runtime::human::admit_workspace(env, &env.workspace_root.join("checkout"))?;
    if !env.delivery.is_null() {
        ensure(
            env.delivery["request"] == *spec && env.delivery["queue"] == json!(queue),
            "delivery intent already exists",
        )?;
        return freeze_queue(env, queue);
    }
    let state = queue_snapshot(queue)?;
    let intent = new_intent(env, queue, spec, &state)?;
    executor.preflight(&intent)?;
    save_prepared(env, queue, &state, intent)
}

fn queue_snapshot(queue: &Path) -> Result<J> {
    let _lock = crate::queue::locked(queue, false)?;
    crate::queue::load_queue(queue)
}

fn save_prepared(env: &mut TaskEnvironment, queue: &Path, before: &J, intent: J) -> Result<()> {
    let _lock = crate::queue::locked(queue, false)?;
    let mut state = crate::queue::load_queue(queue)?;
    ensure(
        state["id"] == before["id"]
            && state["tasks"][&env.task_id] == before["tasks"][&env.task_id],
        "requirement changed during delivery preflight",
    )?;
    cleanup::validate_sources(env, queue, &state)?;
    validate_freeze(&state, &intent)?;
    env.delivery = intent;
    env.persist()?;
    freeze(&mut state, env)?;
    crate::queue::save(queue, &mut state)
}

fn new_intent(env: &TaskEnvironment, queue: &Path, spec: &J, state: &J) -> Result<J> {
    let parent = &state["tasks"][&env.task_id];
    validate_parent(parent, spec)?;
    if spec["adapter"] == "github-pr" {
        publication::validate_parent(state, parent, spec)?;
    }
    let candidate = field(parent, "candidate")?;
    let artifact = parent["artifacts"][candidate]["object"].clone();
    crate::progress::artifact::LocalArtifacts.verify(&artifact)?;
    validate_target(env, spec)?;
    cleanup::validate_sources(env, queue, state)?;
    Ok(
        json!({"id":intent_id(spec)?,"phase":"prepared","request":spec,
        "task":env.task_id,"instance":env.instance_id,"queue":queue,
        "workspace_root":env.workspace_root,"temp_root":env.temp_root,
        "approval":parent["approved"],"artifact":artifact,"stage":parent["stage"],
        "endpoint":requirement(parent)["basis"]["endpoint"],"receipt":null}),
    )
}

fn intent_id(spec: &J) -> Result<String> {
    if spec["adapter"] == "github-pr" {
        return field(spec, "id");
    }
    Ok(util::uuid_hex())
}

fn requirement(parent: &J) -> &J {
    &parent["requirements"][parent["requirement_revision"].to_string()]
}

fn validate_parent(parent: &J, spec: &J) -> Result<()> {
    ensure(
        lineage::is_parent(parent) && parent["control"] == "running",
        "delivery requires running requirement task",
    )?;
    validate_terms(parent, spec)
}

fn validate_terms(parent: &J, spec: &J) -> Result<()> {
    ensure(
        parent["state_version"] == spec["state_version"],
        "requirement version changed",
    )?;
    validate_approval(parent)?;
    let permission = field(spec, "authorization")?;
    let auth = &requirement(parent)["authorization"][permission];
    ensure(
        auth["action"] == "delivery",
        "existing delivery authorization required",
    )?;
    field(auth, "source")?;
    crate::progress::delivery_prerequisites(parent)
}

pub(crate) fn validate_approval(parent: &J) -> Result<()> {
    let approval = &parent["approved"];
    ensure(
        lineage::stage::current(parent, approval),
        "acceptance scope changed",
    )?;
    ensure(
        !approval.is_null()
            && approval["candidate"] == parent["candidate"]
            && approval["revision"] == parent["requirement_revision"]
            && approval["round"] == parent["acceptance_round"],
        "no current independent acceptance",
    )?;
    let activity = field(approval, "activity")?;
    let verdict = &parent["verdicts"][activity];
    ensure(
        verdict["id"] == approval["verdict"]
            && verdict["envelope"]["report"]["conclusion"] == "passed",
        "acceptance report missing",
    )?;
    ensure(
        parent["continuation"]["action"]["kind"] == "delivery",
        "delivery continuation required",
    )
}

fn validate_target(env: &TaskEnvironment, spec: &J) -> Result<()> {
    if spec["adapter"] == "github-pr" {
        return publication::validate_target(spec);
    }
    let target = field(spec, "target")?;
    let path = Path::new(&target);
    ensure(path.is_absolute(), "delivery target must be absolute")?;
    let resolved = util::resolve_path(path);
    ensure(
        env.delivery_targets.iter().any(|t| t.path == resolved),
        "target was not registered at environment creation",
    )?;
    ensure(
        !crate::task_env::is_within_disposable(env, &resolved),
        "target overlaps disposable environment",
    )
}

fn freeze(state: &mut J, env: &TaskEnvironment) -> Result<()> {
    validate_freeze(state, &env.delivery)?;
    let parent = &mut state["tasks"][&env.task_id];
    parent["delivery"] = env.delivery["id"].clone();
    parent["control"] = json!("delivering");
    Ok(())
}

fn validate_freeze(state: &J, intent: &J) -> Result<()> {
    let parent = &state["tasks"][field(intent, "task")?];
    validate_frozen_parent(state, parent, intent)?;
    if intent["request"]["adapter"] == "github-pr" {
        publication::validate_parent(state, parent, &intent["request"])?;
    }
    validate_terms(parent, &intent["request"])
}

fn validate_frozen_parent(state: &J, parent: &J, intent: &J) -> Result<()> {
    ensure(parent["stage"] == intent["stage"], "delivery stage changed")?;
    ensure(
        state["stopped"] != true
            && lineage::is_parent(parent)
            && matches!(parent["control"].as_str(), Some("running" | "delivering")),
        "delivery requirement or queue is no longer active",
    )?;
    ensure(
        parent["approved"] == intent["approval"],
        "delivery acceptance changed",
    )?;
    ensure(
        parent["delivery"].is_null() || parent["delivery"] == intent["id"],
        "another delivery owns parent",
    )?;
    Ok(())
}

/// Save uncertainty before the one allowed submission. All retries must use
/// reconcile, which performs only observation.
/// Requires owned active state in phase prepared. Revalidates and freezes the
/// parent in a short queue transaction; adapter calls never hold the queue lock.
/// Persisting unknown precedes submission. Errors, including persistence errors,
/// do not prove absence of an external effect; restore/reconcile the same intent
/// rather than invoke execute again. Successful return can still report an
/// unknown/failed/absent phase; inspect the returned journal.
pub fn execute(env: &mut TaskEnvironment, executor: &dyn Executor) -> Result<J> {
    let _permit = crate::task_env::isolation::mutation_permit(env)?;
    crate::task_runtime::human::admit_workspace(env, &env.workspace_root.join("checkout"))?;
    validate_execution(env)?;
    executor.preflight(&env.delivery)?;
    reserve_execution(env)?;
    if let Err(error) = executor.execute(&env.delivery) {
        env.delivery["execution_error"] = json!(error.to_string());
        env.persist()?;
    }
    observe(env, executor)
}

fn validate_execution(env: &TaskEnvironment) -> Result<()> {
    ensure_owned(env)?;
    ensure(
        env.state == EnvironmentState::Active,
        "delivery environment is no longer active",
    )?;
    validate_target(env, &env.delivery["request"])?;
    ensure(
        env.delivery["phase"] == "prepared",
        "execution already attempted; reconcile existing operation",
    )?;
    crate::progress::artifact::LocalArtifacts.verify(&env.delivery["artifact"])
}
fn freeze_queue(env: &TaskEnvironment, queue: &Path) -> Result<()> {
    let _lock = crate::queue::locked(queue, false)?;
    let mut state = crate::queue::load_queue(queue)?;
    freeze(&mut state, env)?;
    crate::queue::save(queue, &mut state)
}

fn reserve_execution(env: &mut TaskEnvironment) -> Result<()> {
    let queue = PathBuf::from(field(&env.delivery, "queue")?);
    let _lock = crate::queue::locked(&queue, false)?;
    let mut state = crate::queue::load_queue(&queue)?;
    freeze(&mut state, env)?;
    env.delivery["phase"] = json!("unknown");
    env.persist()?;
    crate::queue::save(&queue, &mut state)
}

fn ensure_owned(env: &TaskEnvironment) -> Result<()> {
    ensure(
        env.owns_lock() && !env.delivery.is_null(),
        "owned delivery journal required",
    )
}

/// Query the adapter and persist the observed outcome without resubmitting.
/// Requires an owned journal whose execution was attempted. Confirmed/cleaning
/// phases return the existing journal without another query. Adapter observation
/// errors become Unknown; persistence errors propagate after in-memory changes.
/// Retain the journal and retry observation, never infer success from Ok alone.
pub fn reconcile(env: &mut TaskEnvironment, executor: &dyn Executor) -> Result<J> {
    let _permit = crate::task_env::isolation::mutation_permit(env)?;
    ensure_owned(env)?;
    if env.delivery["phase"] == "confirmed" || env.delivery["phase"] == "cleaning" {
        return Ok(env.delivery.clone());
    }
    ensure(
        env.delivery["phase"] != "prepared",
        "delivery has not been attempted",
    )?;
    observe(env, executor)
}

fn observe(env: &mut TaskEnvironment, executor: &dyn Executor) -> Result<J> {
    validate_target(env, &env.delivery["request"])?;
    let outcome = executor
        .observe(&env.delivery)
        .unwrap_or_else(|error| Observation::Unknown(error.to_string()));
    match outcome {
        Observation::Delivered(receipt) => {
            ensure(
                receipt.is_object() && !receipt.as_object().unwrap().is_empty(),
                "customer executor must supply verified receipt",
            )?;
            env.delivery["phase"] = json!("confirmed");
            env.delivery["receipt"] = receipt;
        }
        Observation::Absent => env.delivery["phase"] = json!("absent"),
        Observation::Failed(reason) => record_problem(env, "failed", reason),
        Observation::Unknown(reason) => record_problem(env, "unknown", reason),
    }
    env.persist()?;
    Ok(env.delivery.clone())
}
fn record_problem(env: &mut TaskEnvironment, phase: &str, reason: String) {
    env.delivery["phase"] = json!(phase);
    env.delivery["problem"] = json!(reason);
}

/// Explicit retry only after a fresh executor query proves no external effect.
/// Requires owned absent state, observes again, persists prepared, then calls
/// execute. This may submit a new external action and is not a generic error
/// retry. Concurrent endpoint changes remain the trusted adapter's responsibility.
pub fn retry(env: &mut TaskEnvironment, executor: &dyn Executor) -> Result<J> {
    let _permit = crate::task_env::isolation::mutation_permit(env)?;
    crate::task_runtime::human::admit_workspace(env, &env.workspace_root.join("checkout"))?;
    ensure_owned(env)?;
    ensure(
        env.delivery["phase"] == "absent",
        "retry requires confirmed absence",
    )?;
    ensure(
        matches!(executor.observe(&env.delivery)?, Observation::Absent),
        "existing execution is no longer absent",
    )?;
    env.delivery["phase"] = json!("prepared");
    env.persist()?;
    execute(env, executor)
}

/// Once confirmed, interruption can only resume destruction, never publication.
/// Requires owned confirmed/cleaning state and an external lifecycle supervisor
/// without the task marker. Persists cleaning, removes shared records and destroys
/// disposable resources. An error can follow partial cleanup; retain plans/claims
/// and resume cleanup instead of resubmitting. Success leaves no reusable task
/// journal; this is not a repeatable publication call.
pub fn finish(env: &mut TaskEnvironment) -> Result<()> {
    validate_finish(env)?;
    env.delivery["phase"] = json!("cleaning");
    env.persist()?;
    cleanup::run(env)?;
    crate::task_env::destroy_environment(env)
}

fn validate_finish(env: &TaskEnvironment) -> Result<()> {
    ensure(
        env.delivery["stage"].is_null(),
        "stage delivery must continue its parent requirement",
    )?;
    if crate::task_env::isolation::owner(env).is_some() {
        return Err(FlowError::guard(
            "isolated delivery finish requires lifecycle cleanup; not implemented",
        ));
    }
    ensure_owned(env)?;
    ensure(
        !crate::task_env::process_has_marker(std::process::id() as i32, &env.instance_id),
        "finish must run in the external lifecycle supervisor, outside task-owned processes",
    )?;
    validate_target(env, &env.delivery["request"])?;
    ensure(
        env.delivery["phase"] == "confirmed" || env.delivery["phase"] == "cleaning",
        "delivery is not confirmed",
    )
}
