//! Register a fixed acceptance workspace under the existing task Owner.
use super::*;
use crate::{
    progress, queue,
    task_env::review::{Phase, ReviewWorkspace},
};
use serde::Deserialize;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    queue_dir: PathBuf,
    wait: String,
}

struct Input {
    owner: crate::lifecycle::Owner,
    request: Request,
    queue: J,
    _lock: util::FileLock,
}

pub fn prepare(env: &mut TaskEnvironment, root: &Path, spec: &J) -> Result<J> {
    let _permit = crate::task_env::isolation::mutation_permit(env)?;
    verify_instance_owner(env)?;
    let _runtime_permit = crate::lifecycle::runtime_permit(root)?;
    let input = read_queue(env, spec)?;
    let record = requested(env, root, &input)?;
    let key = required(&record.binding, "assignment")?;
    let record = registration(env, root, &key, record)?;
    let ready = crate::task_env::review::materialize(env, record)?;
    Ok(
        json!({"cwd":ready.path,"review_snapshot":key,"candidate":ready.binding["candidate"],"phase":ready.phase}),
    )
}

fn read_queue(env: &TaskEnvironment, spec: &J) -> Result<Input> {
    let owner = crate::task_env::isolation::owner(env)
        .ok_or_else(|| FlowError::guard("review registration requires an isolated Owner"))?;
    let request: Request = serde_json::from_value(spec.clone())?;
    native::validate_queue_control(env, &request.queue_dir)?;
    let lock = queue::locked(&request.queue_dir, false)?;
    let q = queue::load_queue(&request.queue_dir)?;
    Ok(Input {
        owner,
        request,
        queue: q,
        _lock: lock,
    })
}

fn requested(env: &TaskEnvironment, root: &Path, input: &Input) -> Result<ReviewWorkspace> {
    let (binding, source) = current(env, &input.queue, &input.request.wait)?;
    let key = required(&binding, "assignment")?;
    validate_assignment(&key)?;
    ensure(
        root == env.temp_root.join("rt").join(&key),
        "review runtime does not match assignment",
    )?;
    Ok(ReviewWorkspace {
        owner: input.owner.clone(),
        binding,
        wait: input.request.wait.clone(),
        runtime: root.to_path_buf(),
        path: env.workspace_root.join("reviews").join(&key),
        source,
        phase: Phase::Reserved,
        identity: None,
        git_identity: None,
    })
}

fn registration(
    env: &mut TaskEnvironment,
    root: &Path,
    key: &str,
    record: ReviewWorkspace,
) -> Result<ReviewWorkspace> {
    if let Some(saved) = env.review_workspaces.get(key) {
        ensure(
            same_registration(saved, &record),
            "review registration changed",
        )?;
        return Ok(saved.clone());
    }
    register_fresh(env, root, key, record)
}

fn same_registration(saved: &ReviewWorkspace, requested: &ReviewWorkspace) -> bool {
    saved.owner == requested.owner
        && saved.binding == requested.binding
        && saved.wait == requested.wait
        && saved.source == requested.source
        && saved.path == requested.path
        && saved.runtime == requested.runtime
}

fn register_fresh(
    env: &mut TaskEnvironment,
    root: &Path,
    key: &str,
    record: ReviewWorkspace,
) -> Result<ReviewWorkspace> {
    ensure(
        env.review_workspaces.len() < 64,
        "too many review workspaces",
    )?;
    ensure(
        !root.join("runtime.json").try_exists()?,
        "register review workspace before runtime launch",
    )?;
    ensure(
        !record.path.try_exists()?,
        "review workspace must be absent; no adoption",
    )?;
    env.review_workspaces.insert(key.into(), record.clone());
    env.persist()?;
    Ok(record)
}

fn current(env: &TaskEnvironment, q: &J, wait: &str) -> Result<(J, PathBuf)> {
    let binding = current_binding(env, q, wait)?;
    let source = artifact_source(q, &binding)?;
    let key = required(&binding, "assignment")?;
    native::verify_acceptance_workspace(
        q,
        &json!({"cwd":env.workspace_root.join("reviews").join(key)}),
        &binding,
    )?;
    Ok((binding, source))
}

fn current_binding(env: &TaskEnvironment, q: &J, wait: &str) -> Result<J> {
    let w = &q["waits"][wait];
    let binding = progress::binding(w)?;
    let task = &q["tasks"][required(&binding, "task")?];
    progress::environment::runtime_owner(q, env, &binding)?;
    progress::lineage::ensure_current(q, task)?;
    open_acceptance(q, task, w, &binding)?;
    Ok(binding)
}

fn open_acceptance(q: &J, task: &J, w: &J, binding: &J) -> Result<()> {
    ensure(progress::same(task, binding), "review wait binding changed")?;
    ensure(q["stopped"] != true, "review queue is stopped")?;
    ensure(
        binding["lineage"]["role"] == "acceptance"
            && task["control"] == "running"
            && task["phase"] != "completed"
            && w["closed"] != true,
        "review requires a current open acceptance activity",
    )
}

fn artifact_source(q: &J, binding: &J) -> Result<PathBuf> {
    let parent = &q["tasks"][required(&binding["lineage"], "requirement")?];
    let artifact = &parent["artifacts"][required(binding, "candidate")?]["object"];
    ensure(
        artifact["adapter"] == "git",
        "review requires a Git artifact",
    )?;
    Ok(util::resolve_path(Path::new(&required(
        artifact, "location",
    )?)))
}

pub(super) fn binding(env: &TaskEnvironment, runtime: &J, binding: &J) -> Result<()> {
    if crate::task_env::isolation::owner(env).is_some()
        && binding["lineage"]["role"] == "acceptance"
    {
        let key = required(runtime, "review_snapshot")?;
        let record = env
            .review_workspaces
            .get(&key)
            .ok_or_else(|| FlowError::guard("acceptance snapshot is not registered"))?;
        return ensure(
            record.binding == *binding,
            "snapshot belongs to another acceptance activity",
        );
    }
    ensure(
        runtime.get("review_snapshot").is_none(),
        "review runtime cannot execute another role",
    )
}

pub(super) fn selected(env: &TaskEnvironment, root: &Path, cwd: &Path, spec: &J) -> Result<bool> {
    if spec.get("review_snapshot").is_none() {
        return Ok(false);
    }
    let key = required(spec, "review_snapshot")?;
    let record = selected_record(env, root, cwd, &key)?;
    verify_current(env, record)?;
    crate::task_env::review::verify(record)?;
    Ok(true)
}

fn selected_record<'a>(
    env: &'a TaskEnvironment,
    root: &Path,
    cwd: &Path,
    key: &str,
) -> Result<&'a ReviewWorkspace> {
    validate_assignment(key)?;
    let record = env
        .review_workspaces
        .get(key)
        .ok_or_else(|| FlowError::guard("review workspace is not registered"))?;
    ensure(
        record.runtime == root && record.path == cwd && record.phase == Phase::Ready,
        "runtime review selection changed",
    )?;
    crate::task_env::review::validate(env, record)?;
    Ok(record)
}

fn verify_current(env: &TaskEnvironment, record: &ReviewWorkspace) -> Result<()> {
    let _queue = queue::locked(&record.owner.queue_root, false)?;
    let q = queue::load_queue(&record.owner.queue_root)?;
    let (binding, source) = current(env, &q, &record.wait)?;
    ensure(
        binding == record.binding && source == record.source,
        "review activity changed",
    )
}
