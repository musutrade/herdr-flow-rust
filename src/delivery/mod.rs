//! Customer execution and disposable recovery state. Completion leaves no record.
use crate::{
    model::{self, J},
    progress::{artifact::ArtifactAdapter, lineage},
    task_env::{EnvironmentState, TaskEnvironment},
    util, FlowError, Result,
};
use serde_json::json;
use std::path::Path;
mod cleanup;
pub mod cli;
pub mod local;
pub mod publication;

/// Only a customer adapter may establish an external outcome, by inspecting the
/// actual endpoint. A normal process exit or a coordinator's claim is not proof.
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
/// preparation is safe; it never performs an external action.
pub fn prepare(
    env: &mut TaskEnvironment,
    queue: &Path,
    spec: &J,
    executor: &dyn Executor,
) -> Result<J> {
    ensure(
        env.owns_lock() && env.state == EnvironmentState::Active,
        "delivery requires owned active environment",
    )?;
    let _lock = crate::queue::locked(queue, false)?;
    let mut state = crate::queue::load_queue(queue)?;
    stage_intent(env, queue, spec, &state, executor)?;
    freeze(&mut state, env)?;
    crate::queue::save(queue, &mut state)?;
    Ok(env.delivery.clone())
}
fn stage_intent(
    env: &mut TaskEnvironment,
    queue: &Path,
    spec: &J,
    state: &J,
    executor: &dyn Executor,
) -> Result<()> {
    if !env.delivery.is_null() {
        return ensure(
            env.delivery["request"] == *spec && env.delivery["queue"] == json!(queue),
            "delivery intent already exists",
        );
    }
    let intent = new_intent(env, queue, spec, state)?;
    executor.preflight(&intent)?;
    env.delivery = intent;
    env.persist()
}

fn new_intent(env: &TaskEnvironment, queue: &Path, spec: &J, state: &J) -> Result<J> {
    let parent = &state["tasks"][&env.task_id];
    validate_parent(parent, spec)?;
    let candidate = field(parent, "candidate")?;
    let artifact = parent["artifacts"][candidate]["object"].clone();
    crate::progress::artifact::LocalArtifacts.verify(&artifact)?;
    validate_target(env, spec)?;
    cleanup::validate_sources(env, queue, state)?;
    Ok(
        json!({"id":util::uuid_hex(),"phase":"prepared","request":spec,
        "task":env.task_id,"instance":env.instance_id,"queue":queue,
        "workspace_root":env.workspace_root,"temp_root":env.temp_root,
        "approval":parent["approved"],"artifact":artifact,
        "endpoint":requirement(parent)["basis"]["endpoint"],"receipt":null}),
    )
}

fn requirement(parent: &J) -> &J {
    &parent["requirements"][parent["requirement_revision"].to_string()]
}

fn validate_parent(parent: &J, spec: &J) -> Result<()> {
    ensure(
        lineage::is_parent(parent) && parent["control"] == "running",
        "delivery requires running requirement task",
    )?;
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

fn validate_approval(parent: &J) -> Result<()> {
    let approval = &parent["approved"];
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
    let parent = &mut state["tasks"][&env.task_id];
    ensure(
        parent["approved"] == env.delivery["approval"],
        "delivery acceptance changed",
    )?;
    validate_approval(parent)?;
    ensure(
        parent["delivery"].is_null() || parent["delivery"] == env.delivery["id"],
        "another delivery owns parent",
    )?;
    parent["delivery"] = env.delivery["id"].clone();
    parent["control"] = json!("delivering");
    Ok(())
}

/// Save uncertainty before the one allowed submission. All retries must use
/// reconcile, which performs only observation.
pub fn execute(env: &mut TaskEnvironment, executor: &dyn Executor) -> Result<J> {
    validate_execution(env)?;
    let queue = field(&env.delivery, "queue")?;
    let _lock = crate::queue::locked(Path::new(&queue), false)?;
    freeze_queue(env, Path::new(&queue))?;
    executor.preflight(&env.delivery)?;
    env.delivery["phase"] = json!("unknown");
    env.persist()?;
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
    let mut state = crate::queue::load_queue(queue)?;
    freeze(&mut state, env)?;
    crate::queue::save(queue, &mut state)
}

fn ensure_owned(env: &TaskEnvironment) -> Result<()> {
    ensure(
        env.owns_lock() && !env.delivery.is_null(),
        "owned delivery journal required",
    )
}

pub fn reconcile(env: &mut TaskEnvironment, executor: &dyn Executor) -> Result<J> {
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
pub fn retry(env: &mut TaskEnvironment, executor: &dyn Executor) -> Result<J> {
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
pub fn finish(env: &mut TaskEnvironment) -> Result<()> {
    ensure_owned(env)?;
    ensure(
        !crate::task_env::process_has_marker(std::process::id() as i32, &env.instance_id),
        "finish must run in the external lifecycle supervisor, outside task-owned processes",
    )?;
    validate_target(env, &env.delivery["request"])?;
    ensure(
        env.delivery["phase"] == "confirmed" || env.delivery["phase"] == "cleaning",
        "delivery is not confirmed",
    )?;
    env.delivery["phase"] = json!("cleaning");
    env.persist()?;
    cleanup::run(env)?;
    crate::task_env::destroy_environment(env)
}
