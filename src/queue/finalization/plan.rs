//! Frozen, typed queue associations. No path or free-text ownership inference.
use super::*;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Associations {
    pub owner: Owner,
    pub tasks: BTreeMap<String, J>,
    pub waits: BTreeMap<String, J>,
    pub events: BTreeMap<String, J>,
    pub deliveries: BTreeMap<String, J>,
    pub receiver: J,
    pub guard_dir: Option<PathBuf>,
    pub owned_receiver: bool,
    pub receiver_owner: Option<Owner>,
    pub results: BTreeMap<String, Option<util::protected::DocumentRemoval>>,
    pub delivery_documents: BTreeMap<String, Option<util::protected::DocumentRemoval>>,
}

pub(super) fn build(s: &J, owner: &Owner) -> Result<Associations> {
    let tasks = owned_tasks(s, owner)?;
    let waits = records(s, "waits", &tasks)?;
    let events = records(s, "events", &tasks)?;
    validate_event_bindings(s, &events, &waits, &tasks)?;
    let deliveries = related_deliveries(s, &events)?;
    assemble(s, owner, tasks, waits, events, deliveries)
}

fn assemble(
    s: &J,
    owner: &Owner,
    tasks: BTreeMap<String, J>,
    waits: BTreeMap<String, J>,
    events: BTreeMap<String, J>,
    deliveries: BTreeMap<String, J>,
) -> Result<Associations> {
    let guard_dir = s["guard_dir"].as_str().map(PathBuf::from);
    let receiver_owner = receiver_owner(guard_dir.as_deref())?;
    let labelled_owned = tasks.contains_key(s["receiver"]["task"].as_str().unwrap_or(""));
    require(
        !labelled_owned || receiver_owner.as_ref() == Some(owner),
        "same-name receiver lacks exact runtime ownership",
    )?;
    let owned_receiver = receiver_owner.as_ref() == Some(owner);
    let artifacts = super::super::artifacts(&owner.queue_root)?;
    let results = sidecar_proofs(&artifacts.join("results"), waits.keys())?;
    let delivery_documents = sidecar_proofs(&artifacts.join("deliveries"), deliveries.keys())?;
    validate_results(&artifacts, &waits, owner)?;
    Ok(Associations {
        owner: owner.clone(),
        tasks,
        waits,
        events,
        deliveries,
        receiver: s["receiver"].clone(),
        guard_dir,
        owned_receiver,
        receiver_owner,
        results,
        delivery_documents,
    })
}

pub(super) fn receiver_owner(root: Option<&Path>) -> Result<Option<Owner>> {
    let Some(root) = root else {
        return Ok(None);
    };
    let marker = root.join("lifecycle-owner.json");
    if !util::protected::is_runtime_namespace(root)? {
        require(
            !marker.try_exists()?,
            "receiver owner lacks runtime namespace",
        )?;
        return Ok(None);
    }
    require(
        util::protected::is_document(&marker)?,
        "receiver runtime owner is missing",
    )?;
    Ok(Some(serde_json::from_value(util::read_json(&marker)?)?))
}

fn validate_results(artifacts: &Path, waits: &BTreeMap<String, J>, owner: &Owner) -> Result<()> {
    for (id, wait) in waits {
        let path = artifacts.join("results").join(format!("{id}.json"));
        if path.try_exists()? {
            let result = util::read_json(&path)?;
            validate_result(&result, id, wait, owner)?;
        }
    }
    Ok(())
}

fn validate_result(result: &J, id: &str, wait: &J, owner: &Owner) -> Result<()> {
    require(
        result["lifecycle_owner"] == json!(owner),
        "cleanup result belongs to another instance",
    )?;
    require(
        result["queue"] == owner.queue_id
            && result["wait"] == id
            && result["task"] == wait["task"]
            && result["epoch"] == wait["epoch"],
        "cleanup result wait/task/epoch binding changed",
    )
}

fn sidecar_proofs<'a>(
    root: &Path,
    ids: impl Iterator<Item = &'a String>,
) -> Result<BTreeMap<String, Option<util::protected::DocumentRemoval>>> {
    ids.map(|id| {
        Ok((
            id.clone(),
            util::protected::prepare_document_removal(&root.join(format!("{id}.json")))?,
        ))
    })
    .collect()
}

fn owned_tasks(s: &J, owner: &Owner) -> Result<BTreeMap<String, J>> {
    let mut tasks = BTreeMap::new();
    for (id, task) in object(&s["tasks"], "tasks")? {
        if id != &owner.task_id && task["lineage"]["requirement"] != owner.task_id {
            continue;
        }
        require(task["task"] == *id, "cleanup task identity changed")?;
        require(
            lifecycle::task_owner(s, id)?.as_ref() == Some(owner),
            "cleanup activity owner changed",
        )?;
        tasks.insert(id.clone(), task.clone());
    }
    require(
        tasks.contains_key(&owner.task_id),
        "cleanup owner is missing",
    )?;
    Ok(tasks)
}

fn records(s: &J, field: &str, tasks: &BTreeMap<String, J>) -> Result<BTreeMap<String, J>> {
    let mut owned = BTreeMap::new();
    for (id, record) in object(&s[field], field)? {
        validate_record_task(field, record, tasks)?;
        insert_record(id, field, record, tasks, &mut owned)?;
    }
    Ok(owned)
}

fn validate_record_task(field: &str, record: &J, tasks: &BTreeMap<String, J>) -> Result<()> {
    if field == "events" && tasks.contains_key(record["binding"]["task"].as_str().unwrap_or("")) {
        require(
            record["binding"]["task"] == record["task"],
            "cleanup event task and binding disagree",
        )?;
    }
    Ok(())
}

fn insert_record(
    id: &str,
    field: &str,
    record: &J,
    tasks: &BTreeMap<String, J>,
    owned: &mut BTreeMap<String, J>,
) -> Result<()> {
    let name = record["task"].as_str().unwrap_or("");
    if !tasks.contains_key(name) {
        return Ok(());
    }
    safe_id_if_sidecar(id, field)?;
    require(
        record["id"] == id
            && record["epoch"].as_u64().is_some()
            && record["epoch"] == tasks[name]["epoch"],
        "cleanup record belongs to another task epoch",
    )?;
    owned.insert(id.to_owned(), record.clone());
    Ok(())
}

fn validate_event_bindings(
    s: &J,
    events: &BTreeMap<String, J>,
    waits: &BTreeMap<String, J>,
    tasks: &BTreeMap<String, J>,
) -> Result<()> {
    for event in events.values() {
        validate_event_binding(event, waits, tasks)?;
        validate_event_owner(s, event)?;
    }
    Ok(())
}

fn validate_event_binding(
    event: &J,
    waits: &BTreeMap<String, J>,
    tasks: &BTreeMap<String, J>,
) -> Result<()> {
    if event["kind"] == "progress_stall" {
        return validate_stall(event, &tasks[event["task"].as_str().unwrap_or("")]);
    }
    let id = event["wait"]
        .as_str()
        .ok_or_else(|| queue_error("cleanup event lacks wait binding"))?;
    let wait = waits
        .get(id)
        .ok_or_else(|| queue_error("cleanup event references a foreign wait"))?;
    validate_event_wait(event, wait)
}

fn validate_event_owner(s: &J, event: &J) -> Result<()> {
    if !event["lifecycle_owner"].is_null() {
        require(
            event["lifecycle_owner"]
                == isolation::owner_value(s, event["task"].as_str().unwrap_or(""))?,
            "cleanup event lifecycle owner changed",
        )?;
    }
    Ok(())
}

fn validate_event_wait(event: &J, wait: &J) -> Result<()> {
    require(
        event["task"] == wait["task"] && event["epoch"] == wait["epoch"],
        "cleanup event wait incarnation changed",
    )?;
    if wait.get("assignment").is_some() {
        return require(
            event["binding"] == crate::progress::binding(wait)?,
            "cleanup event assignment binding changed",
        );
    }
    require(
        event["binding"].is_null(),
        "unassigned wait has a foreign event binding",
    )
}

fn validate_stall(event: &J, task: &J) -> Result<()> {
    require(
        event["wait"] == task["wait"],
        "cleanup stall references a foreign wait",
    )?;
    for key in ["task", "epoch", "assignment", "candidate"] {
        require(
            event["binding"][key] == task[key],
            "cleanup stall binding changed",
        )?;
    }
    Ok(())
}

fn safe_id_if_sidecar(id: &str, field: &str) -> Result<()> {
    require(!id.is_empty(), "empty cleanup identity")?;
    if field == "waits" {
        safe_id(id)?;
    }
    Ok(())
}

fn related_deliveries(s: &J, events: &BTreeMap<String, J>) -> Result<BTreeMap<String, J>> {
    let mut found = BTreeMap::new();
    for (id, delivery) in object(&s["deliveries"], "deliveries")? {
        let members = delivery["events"]
            .as_array()
            .ok_or_else(|| queue_error("cleanup delivery events missing"))?;
        if members
            .iter()
            .any(|v| events.contains_key(v.as_str().unwrap_or("")))
        {
            safe_id(id)?;
            require(delivery["id"] == *id, "cleanup delivery identity changed")?;
            found.insert(id.clone(), delivery.clone());
        }
    }
    Ok(found)
}

pub(super) fn decode(value: J, owner: &Owner) -> Result<Associations> {
    let plan: Associations = serde_json::from_value(value)?;
    require(&plan.owner == owner, "cleanup association owner changed")?;
    require(
        plan.tasks.contains_key(&owner.task_id),
        "cleanup plan lacks owner",
    )?;
    for id in plan.waits.keys().chain(plan.deliveries.keys()) {
        safe_id(id)?;
    }
    Ok(plan)
}

pub(super) fn safe_id(id: &str) -> Result<()> {
    require(
        !id.is_empty()
            && id != "."
            && id != ".."
            && id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b)),
        "unsafe cleanup sidecar identity",
    )
}

pub(super) fn object<'a>(value: &'a J, name: &str) -> Result<&'a serde_json::Map<String, J>> {
    value
        .as_object()
        .ok_or_else(|| queue_error(format!("cleanup {name} must be an object")))
}
