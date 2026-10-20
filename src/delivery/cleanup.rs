//! Remove task associations under the normal queue lock, without queue backups.
use super::*;
use std::collections::BTreeSet;

pub(super) fn validate_sources(env: &TaskEnvironment, queue: &Path, state: &J) -> Result<()> {
    // Native runtime registration already requires all producer state within
    // this environment. Refuse legacy shared producer stores instead of
    // pretending a workspace deletion can erase their sessions or logs.
    for (_, wait) in entries(&state["waits"]) {
        if belongs(state, &env.task_id, &wait["task"]) {
            if let Some(source) = wait["source"].as_str() {
                ensure(
                    crate::task_env::is_within_disposable(env, Path::new(source)),
                    "task producer state must belong to disposable environment",
                )?;
            }
        }
    }
    validate_guard(env, state)?;
    validate_foreign_tasks(env, queue, state)?;
    ensure(queue.is_absolute(), "queue path must be absolute")
}
fn validate_guard(env: &TaskEnvironment, state: &J) -> Result<()> {
    let Some(guard) = state["guard_dir"].as_str() else {
        return Ok(());
    };
    let g = util::read_state_json(Path::new(guard), "state.json")?;
    if belongs(state, &env.task_id, &g["task"]) || !g["session"].is_null() {
        ensure(
            crate::task_env::is_within_disposable(env, Path::new(guard)),
            "native receiver sessions and task-exclusive guards must be disposable",
        )?;
    }
    Ok(())
}
fn validate_foreign_tasks(env: &TaskEnvironment, queue: &Path, state: &J) -> Result<()> {
    if crate::task_env::is_within_disposable(env, queue) {
        return Ok(());
    }
    let owned = ids(&state["tasks"], |t| {
        belongs(state, &env.task_id, &t["task"])
    });
    for (id, task) in entries(&state["tasks"]) {
        if !owned.contains(id) {
            ensure(!references_owned(task, &owned),
                "shared task/lane retains this task: use a task-owned queue or reconcile ownership first")?;
        }
    }
    Ok(())
}
fn references_owned(value: &J, owned: &BTreeSet<String>) -> bool {
    match value {
        J::String(s) => owned.contains(s),
        J::Array(a) => a.iter().any(|v| references_owned(v, owned)),
        J::Object(o) => o
            .iter()
            .any(|(k, v)| owned.contains(k) || references_owned(v, owned)),
        _ => false,
    }
}
fn entries(value: &J) -> impl Iterator<Item = (&String, &J)> {
    value.as_object().into_iter().flat_map(|m| m.iter())
}
fn belongs(state: &J, parent: &str, task: &J) -> bool {
    task == parent
        || state["tasks"][task.as_str().unwrap_or("")]["lineage"]["requirement"] == parent
}
fn ids(value: &J, predicate: impl Fn(&J) -> bool) -> BTreeSet<String> {
    entries(value)
        .filter(|(_, v)| predicate(v))
        .map(|(k, _)| k.clone())
        .collect()
}
pub(super) fn run(env: &mut TaskEnvironment) -> Result<()> {
    // Stop all task-exclusive writers, servers and watchers before touching
    // queue sidecars. Shared receivers remain under their own normal lock.
    crate::task_env::quiesce_environment(env)?;
    if env.delivery["shared_removed"] == true {
        return Ok(());
    }
    let queue = field(&env.delivery, "queue")?;
    let root = Path::new(&queue);
    let _lock = crate::queue::locked(root, false)?;
    remove_shared(env, root)?;
    env.delivery["shared_removed"] = json!(true);
    env.persist()
}
fn remove_shared(env: &mut TaskEnvironment, root: &Path) -> Result<()> {
    let mut state = crate::queue::load_queue(root)?;
    validate_foreign_tasks(env, root, &state)?;
    prepare_removal(env, &state)?;
    remove_sidecars(root, &env.delivery["removal"], &mut state)?;
    prune(&mut state, &env.delivery["removal"]);
    clear_receiver(root, &state)?;
    clear_owned_receiver(&mut state, &env.delivery["removal"]);
    crate::queue::save(root, &mut state)
}
fn clear_owned_receiver(state: &mut J, plan: &J) {
    if contains(plan, "tasks", &state["receiver"]["task"]) {
        state["receiver"] = J::Null;
        state["guard_dir"] = J::Null;
        state["notify"] = json!(false);
        state["delivery_status"] = json!("disabled");
    }
}
fn clear_receiver(queue: &Path, state: &J) -> Result<()> {
    let Some(path) = state["guard_dir"].as_str() else {
        return Ok(());
    };
    let root = Path::new(path);
    if !root.join("state.json").exists() {
        return Ok(());
    }
    let _lock = crate::guard::locked(root)?;
    let mut guard = util::read_state_json(root, "state.json")?;
    let id = guard["external_delivery"]["delivery"]
        .as_str()
        .unwrap_or("");
    if guard["external_delivery"]["queue"] == json!(queue)
        && !id.is_empty()
        && state["deliveries"][id].is_null()
    {
        guard["external_delivery"] = J::Null;
        crate::guard::save(root, &mut guard)?;
    }
    Ok(())
}
fn prepare_removal(env: &mut TaskEnvironment, state: &J) -> Result<()> {
    if !env.delivery["removal"].is_null() {
        return Ok(());
    }
    let tasks = ids(&state["tasks"], |t| {
        belongs(state, &env.task_id, &t["task"])
    });
    let waits = ids(&state["waits"], |w| {
        belongs(state, &env.task_id, &w["task"])
    });
    let events = ids(&state["events"], |e| {
        belongs(state, &env.task_id, &e["task"])
    });
    env.delivery["removal"] = json!({"tasks":tasks,"waits":waits,"events":events});
    env.persist()
}
fn contains(plan: &J, key: &str, id: &J) -> bool {
    plan[key].as_array().is_some_and(|a| a.contains(id))
}
fn safe_id(id: &str) -> Result<()> {
    ensure(
        !id.is_empty()
            && id != "."
            && id != ".."
            && id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b)),
        "unsafe queue sidecar identity",
    )
}
fn remove_sidecars(root: &Path, plan: &J, state: &mut J) -> Result<()> {
    for id in plan["waits"].as_array().unwrap() {
        let id = id.as_str().unwrap();
        safe_id(id)?;
        util::remove_state_file(&root.join("results"), &format!("{id}.json"))?;
    }
    let deliveries = state["deliveries"].as_object_mut();
    if let Some(deliveries) = deliveries {
        for (id, delivery) in deliveries {
            filter_delivery(root, plan, id, delivery)?;
        }
    }
    Ok(())
}
fn filter_delivery(root: &Path, plan: &J, id: &str, delivery: &mut J) -> Result<()> {
    let Some(events) = delivery["events"].as_array_mut() else {
        return Ok(());
    };
    if !events.iter().any(|e| contains(plan, "events", e)) {
        return Ok(());
    }
    safe_id(id)?;
    events.retain(|e| !contains(plan, "events", e));
    let path = root.join("deliveries").join(format!("{id}.json"));
    if events.is_empty() {
        return util::remove_state_file(&root.join("deliveries"), &format!("{id}.json"));
    }
    if path.exists() {
        let mut sidecar = util::read_state_json(&root.join("deliveries"), &format!("{id}.json"))?;
        if let Some(events) = sidecar["events"].as_array_mut() {
            events.retain(|e| !contains(plan, "events", &e["id"]));
        }
        sidecar["delivery"] = delivery.clone();
        util::atomic_write_json(&path, &sidecar)?;
    }
    Ok(())
}
fn prune(state: &mut J, plan: &J) {
    for key in ["tasks", "waits", "events"] {
        if let Some(map) = state[key].as_object_mut() {
            map.retain(|id, _| !contains(plan, key, &json!(id)));
        }
    }
    if let Some(deliveries) = state["deliveries"].as_object_mut() {
        deliveries.retain(|_, d| d["events"].as_array().is_some_and(|a| !a.is_empty()));
    }
    let active = state["active"].as_str().unwrap_or("");
    if state["deliveries"][active].is_null() {
        state["active"] = J::Null;
    }
}
