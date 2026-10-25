//! Read-only environment snapshots. These are observations, not ownership grants.
mod boundary;
mod read;

use super::{CleanupState, EnvironmentState, Registry, TaskEnvironment};
use crate::{FlowError, Result};
use serde_json::{json, Value};
use std::path::Path;

/// Observe control documents without creating directories, taking ownership or
/// invoking lifecycle path helpers. Invalid or changing state is `unknown`.
pub fn status(task: &str) -> Result<Value> {
    super::validate_task_id(task)?;
    let root = super::registry::control_path()?;
    let result = observe(&root, task).unwrap_or_else(|e| {
        json!({
            "state":"unknown", "errors":[e.to_string()], "occupied_slots":null,
            "interrupted":null, "recovery":"unknown"
        })
    });
    let mut result = result;
    result["task_id"] = json!(task);
    result["control_root"] = json!(root);
    result["snapshot"] = json!(result["state"] != "unknown");
    result["ownership_checked"] = json!(false);
    result["cleanup_authorized"] = json!(false);
    result["occupied_slots_accounting"] =
        json!("observed envs/*.json + plans/*.json; not deduplicated");
    Ok(result)
}

fn observe(root: &Path, task: &str) -> Result<Value> {
    let snapshot = read::capture(root)?;
    // Only normalize after capture has rejected symlinks in existing prefixes.
    let registry = validate(&boundary::normalize(root), &snapshot)?;
    let result = describe(task, &snapshot, &registry)?;
    unchanged(&snapshot, &read::capture(root)?)?;
    Ok(result)
}

fn unchanged(first: &read::Snapshot, second: &read::Snapshot) -> Result<()> {
    require(first == second, "status readback changed; retry explicitly")
}

fn require(ok: bool, message: &str) -> Result<()> {
    if ok {
        Ok(())
    } else {
        Err(FlowError::progress(message))
    }
}

fn validate(root: &Path, snapshot: &read::Snapshot) -> Result<Registry> {
    let registry: Registry = snapshot
        .registry
        .clone()
        .map(serde_json::from_value)
        .transpose()?
        .unwrap_or_default();
    for (task, value) in &snapshot.envs {
        validate_env(root, task, value, &registry)?;
    }
    for (task, value) in &snapshot.plans {
        validate_plan(root, task, value, snapshot, &registry)?;
    }
    for entry in registry.entries() {
        validate_entry(root, snapshot, entry)?;
    }
    validate_keys(snapshot)?;
    Ok(registry)
}

fn validate_entry(
    root: &Path,
    snapshot: &read::Snapshot,
    entry: &super::RegistryEntry,
) -> Result<()> {
    super::validate_task_id(&entry.task_id)?;
    boundary::roots(root, &entry.claimed_roots())?;
    require(
        !entry.instance_id.is_empty()
            && entry.workspace_root.is_absolute()
            && entry.temp_root.is_absolute()
            && entry.state_file == root.join("envs").join(format!("{}.json", entry.task_id)),
        "registry identity or paths are inconsistent",
    )?;
    require(
        snapshot.envs.contains_key(&entry.task_id) || snapshot.plans.contains_key(&entry.task_id),
        "registry claim has no environment or cleanup plan",
    )?;
    Ok(())
}

fn validate_keys(snapshot: &read::Snapshot) -> Result<()> {
    if let Some(value) = &snapshot.registry {
        for (key, entry) in value["entries"]
            .as_object()
            .ok_or_else(|| FlowError::progress("invalid registry entries"))?
        {
            require(
                entry["task_id"] == *key,
                "registry key and task identity disagree",
            )?;
        }
    }
    Ok(())
}

fn validate_env(root: &Path, task: &str, value: &Value, registry: &Registry) -> Result<()> {
    let env: TaskEnvironment = serde_json::from_value(value.clone())?;
    validate_environment_identity(root, task, &env)?;
    validate_creation(&env)?;
    let roots = vec![env.workspace_root.clone(), env.temp_root.clone()];
    boundary::roots(root, &roots)?;
    boundary::targets(&roots, &env.delivery_targets)?;
    validate_environment_binding(task, &env, registry)
}

fn validate_creation(env: &TaskEnvironment) -> Result<()> {
    if let Some(intent) = &env.creation {
        require(
            intent.binding.task_id == env.task_id
                && intent.binding.epoch > 0
                && !intent.binding.queue_id.is_empty()
                && intent.checkout == env.workspace_root.join("checkout"),
            "invalid instance creation binding",
        )?;
    }
    Ok(())
}

fn validate_environment_identity(root: &Path, task: &str, env: &TaskEnvironment) -> Result<()> {
    require(
        env.task_id == task
            && !env.instance_id.is_empty()
            && env.state_file == root.join("envs").join(format!("{task}.json"))
            && env.workspace_root.is_absolute()
            && env.temp_root.is_absolute()
            && env.delivery_targets.iter().all(|t| t.path.is_absolute()),
        "environment identity or paths are inconsistent",
    )
}

fn validate_environment_binding(
    task: &str,
    env: &TaskEnvironment,
    registry: &Registry,
) -> Result<()> {
    if let Some(entry) = registry.get(task) {
        boundary::subset(
            &entry.claimed_roots(),
            &[env.workspace_root.clone(), env.temp_root.clone()],
        )?;
        require(
            entry.instance_id == env.instance_id
                && entry.workspace_root == env.workspace_root
                && entry.temp_root == env.temp_root
                && entry.state_file == env.state_file,
            "registry and environment bindings disagree",
        )?;
    } else {
        require(
            env.state != EnvironmentState::Active,
            "active environment has no registry claim",
        )?;
    }
    Ok(())
}

fn validate_plan(
    root: &Path,
    task: &str,
    value: &Value,
    snapshot: &read::Snapshot,
    registry: &Registry,
) -> Result<()> {
    let plan: CleanupState = serde_json::from_value(value.clone())?;
    let paths = boundary::plan(root, &plan)?;
    require(
        plan.task_id == task && !plan.instance_id.is_empty(),
        "cleanup plan identity or roots are invalid",
    )?;
    plan_environment(snapshot.envs.get(task), &plan, &paths)?;
    plan_registry(registry.get(task), &plan, &paths)
}

fn plan_environment(
    env: Option<&Value>,
    plan: &CleanupState,
    paths: &[std::path::PathBuf],
) -> Result<()> {
    if let Some(env) = env {
        require(
            env["instance_id"] == plan.instance_id,
            "cleanup plan and environment instances disagree",
        )?;
        let env: TaskEnvironment = serde_json::from_value(env.clone())?;
        boundary::subset(paths, &env.roots().map(|(path, _)| path))?;
        boundary::targets(paths, &env.delivery_targets)?;
    }
    Ok(())
}

fn plan_registry(
    entry: Option<&super::RegistryEntry>,
    plan: &CleanupState,
    paths: &[std::path::PathBuf],
) -> Result<()> {
    if let Some(entry) = entry {
        require(
            entry.instance_id == plan.instance_id,
            "cleanup plan and registry instances disagree",
        )?;
        boundary::subset(paths, &entry.claimed_roots())?;
    }
    Ok(())
}

fn describe(task: &str, snapshot: &read::Snapshot, registry: &Registry) -> Result<Value> {
    let env = snapshot.envs.get(task);
    let plan = snapshot.plans.get(task);
    let state = env.map(|v| v["state"].clone()).unwrap_or_else(|| {
        json!(if plan.is_some() {
            "plan_only"
        } else {
            "absent"
        })
    });
    let interrupted = plan.is_some() || env.is_some_and(|v| v["state"] != "active");
    Ok(
        json!({"state":state,"instance_id":env.or(plan).map(|v| &v["instance_id"]),
        "registered":registry.contains(task),"registry":if snapshot.registry.is_some(){"present"}else{"absent"},
        "environment":env,"plan":plan,"interrupted":interrupted,
        "recovery":if env.is_some_and(|v| v["creation"].is_object()) {"queue_instance_resume_only"}
            else if interrupted {"would_destroy"} else {"no_op"},
        "occupied_slots":snapshot.envs.len()+snapshot.plans.len(),"errors":[]}),
    )
}

#[cfg(test)]
mod tests {
    #[test]
    fn readback_change_is_unknown_not_a_guessed_snapshot() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::Builder::new()
            .permissions(std::fs::Permissions::from_mode(0o700))
            .tempdir()
            .unwrap();
        assert!(super::read::changed_snapshot(dir.path())
            .unwrap_err()
            .to_string()
            .contains("readback changed"));
    }
}
