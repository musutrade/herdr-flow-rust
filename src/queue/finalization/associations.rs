//! Remove only the associations frozen before any resource deletion.
use super::*;

pub(super) fn remove(
    root: &Path,
    s: &mut J,
    plan: &Associations,
    retirements: &receiver::Retirements,
) -> Result<()> {
    validate_records(s, plan)?;
    shared::validate(s, plan)?;
    let artifacts = super::super::artifacts(root)?;
    remove_results(&artifacts, plan)?;
    filter_deliveries(&artifacts, s, plan)?;
    clear_receiver(root, s, plan)?;
    prune(s, plan, retirements)
}

fn validate_records(s: &J, plan: &Associations) -> Result<()> {
    for (field, records) in [
        ("tasks", &plan.tasks),
        ("waits", &plan.waits),
        ("events", &plan.events),
    ] {
        for (id, frozen) in records {
            let current = &s[field][id];
            let expected = expected_record(field, id, frozen, current, plan)?;
            require(
                current.is_null() || current == &expected,
                "cleanup association was replaced",
            )?;
        }
    }
    Ok(())
}

fn expected_record(
    field: &str,
    id: &str,
    frozen: &J,
    current: &J,
    plan: &Associations,
) -> Result<J> {
    let mut expected = frozen.clone();
    if field == "tasks" && id == plan.owner.task_id.as_str() {
        receiver::normalize_task(&mut expected, current, plan)?;
        if current["cleanup_obligation"]["status"] == "associations_removed" {
            expected["cleanup_obligation"]["status"] = json!("associations_removed");
        }
    }
    Ok(expected)
}

fn remove_results(artifacts: &Path, plan: &Associations) -> Result<()> {
    for id in plan.waits.keys() {
        let path = artifacts.join("results").join(format!("{id}.json"));
        let proof = plan
            .results
            .get(id)
            .ok_or_else(|| queue_error("cleanup result proof missing"))?;
        resume_removal(&path, proof.as_ref())?;
    }
    Ok(())
}

fn filter_deliveries(artifacts: &Path, s: &J, plan: &Associations) -> Result<()> {
    for (id, frozen) in &plan.deliveries {
        let current = &s["deliveries"][id];
        validate_delivery(current, frozen)?;
        filter_sidecar(artifacts, id, current, frozen, plan)?;
    }
    Ok(())
}

fn validate_delivery(current: &J, frozen: &J) -> Result<()> {
    if current.is_null() {
        return Ok(());
    }
    require(
        current["id"] == frozen["id"] && current["session"] == frozen["session"],
        "cleanup delivery was replaced",
    )?;
    let original = frozen["events"]
        .as_array()
        .ok_or_else(|| queue_error("frozen delivery events missing"))?;
    let current = current["events"]
        .as_array()
        .ok_or_else(|| queue_error("cleanup delivery events missing"))?;
    require(
        current.iter().all(|id| original.contains(id)),
        "cleanup delivery gained foreign events",
    )
}

fn surviving_delivery(current: &J, plan: &Associations) -> Result<J> {
    let mut delivery = current.clone();
    let events = delivery["events"]
        .as_array_mut()
        .ok_or_else(|| queue_error("cleanup delivery events missing"))?;
    events.retain(|id| !plan.events.contains_key(id.as_str().unwrap_or("")));
    Ok(delivery)
}

fn filter_sidecar(
    artifacts: &Path,
    id: &str,
    current: &J,
    frozen: &J,
    plan: &Associations,
) -> Result<()> {
    let path = artifacts.join("deliveries").join(format!("{id}.json"));
    let proof = plan
        .delivery_documents
        .get(id)
        .ok_or_else(|| queue_error("cleanup delivery proof missing"))?;
    if current.is_null() || delivery_empty(current, plan)? {
        return resume_removal(&path, proof.as_ref());
    }
    if !path.try_exists()? {
        return Ok(());
    }
    verify_document(&path, proof.as_ref())?;
    rewrite_sidecar(&path, current, frozen, plan)
}

fn rewrite_sidecar(path: &Path, current: &J, frozen: &J, plan: &Associations) -> Result<()> {
    let mut payload = util::read_json(path)?;
    require(
        payload["delivery"]["id"] == frozen["id"]
            && payload["delivery"]["session"] == frozen["session"],
        "cleanup delivery sidecar identity changed",
    )?;
    let survivors = surviving_delivery(current, plan)?;
    let events = payload["events"]
        .as_array_mut()
        .ok_or_else(|| queue_error("cleanup delivery payload events missing"))?;
    events.retain(|event| !plan.events.contains_key(event["id"].as_str().unwrap_or("")));
    payload["delivery"] = survivors;
    util::atomic_write_json(path, &payload)
}

fn verify_document(path: &Path, proof: Option<&util::protected::DocumentRemoval>) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    let proof = proof.ok_or_else(|| queue_error("unplanned cleanup sidecar appeared"))?;
    let metadata = std::fs::symlink_metadata(path)?;
    require(
        proof.path == path
            && metadata.is_dir()
            && metadata.dev() == proof.dev
            && metadata.ino() == proof.ino,
        "cleanup sidecar inode changed",
    )
}

fn resume_removal(path: &Path, proof: Option<&util::protected::DocumentRemoval>) -> Result<()> {
    if let Some(proof) = proof {
        require(proof.path == path, "cleanup sidecar path changed")?;
        return util::protected::resume_document_removal(proof);
    }
    require(!path.try_exists()?, "unplanned cleanup sidecar appeared")
}

fn clear_receiver(root: &Path, s: &J, plan: &Associations) -> Result<()> {
    if plan.owned_receiver || plan.deliveries.is_empty() {
        return Ok(());
    }
    let Some(path) = &plan.guard_dir else {
        return Ok(());
    };
    if receiver::retired(s, plan)? {
        return Ok(());
    }
    verify_receiver_binding(s, path, plan)?;
    let _lock = crate::guard::locked_existing(path)?;
    let mut guard = verify_receiver(path, plan)?;
    clear_external_delivery(root, s, plan, path, &mut guard)
}

fn verify_receiver_binding(s: &J, path: &Path, plan: &Associations) -> Result<()> {
    require(
        s["receiver"] == plan.receiver && s["guard_dir"] == json!(path),
        "cleanup receiver changed",
    )?;
    verify_receiver(path, plan)?;
    Ok(())
}

fn verify_receiver(path: &Path, plan: &Associations) -> Result<J> {
    require(
        super::plan::receiver_owner(Some(path))? == plan.receiver_owner,
        "cleanup receiver owner changed",
    )?;
    let guard = util::read_state_json(path, "state.json")?;
    require(
        crate::guard::scope(&guard) == plan.receiver,
        "cleanup receiver scope changed",
    )?;
    Ok(guard)
}

fn clear_external_delivery(
    root: &Path,
    s: &J,
    plan: &Associations,
    path: &Path,
    guard: &mut J,
) -> Result<()> {
    let marker = &guard["external_delivery"];
    let id = marker["delivery"].as_str().unwrap_or("");
    if !plan.deliveries.contains_key(id) {
        return Ok(());
    }
    let frozen = &plan.deliveries[id];
    let expected =
        json!({"queue":root.display().to_string(),"delivery":id,"session":frozen["session"]});
    require(
        marker == &expected,
        "another delivery owns cleanup receiver",
    )?;
    if !delivery_empty(&s["deliveries"][id], plan)? {
        return Ok(());
    }
    guard
        .as_object_mut()
        .ok_or_else(|| queue_error("cleanup guard malformed"))?
        .remove("external_delivery");
    crate::guard::save(path, guard)
}

fn delivery_empty(delivery: &J, plan: &Associations) -> Result<bool> {
    if delivery.is_null() {
        return Ok(true);
    }
    Ok(surviving_delivery(delivery, plan)?["events"]
        .as_array()
        .is_some_and(Vec::is_empty))
}

pub(super) fn prune(
    s: &mut J,
    plan: &Associations,
    retirements: &receiver::Retirements,
) -> Result<()> {
    validate_records(s, plan)?;
    shared::prune(s, plan)?;
    remove_entries(s, "waits", plan.waits.keys());
    remove_entries(s, "events", plan.events.keys());
    for (id, frozen) in &plan.deliveries {
        prune_delivery(s, id, frozen, plan)?;
    }
    if plan.owned_receiver {
        clear_owned_receiver(s, plan)?;
    }
    receiver::apply(s, retirements)?;
    if !s["tasks"][&plan.owner.task_id].is_null() {
        s["tasks"][&plan.owner.task_id]["cleanup_obligation"]["status"] =
            json!("associations_removed");
    }
    Ok(())
}

fn remove_entries<'a>(s: &mut J, field: &str, ids: impl Iterator<Item = &'a String>) {
    if let Some(map) = s[field].as_object_mut() {
        for id in ids {
            map.remove(id);
        }
    }
}

fn prune_delivery(s: &mut J, id: &str, frozen: &J, plan: &Associations) -> Result<()> {
    let current = &s["deliveries"][id];
    validate_delivery(current, frozen)?;
    if current.is_null() {
        return Ok(());
    }
    let survivors = surviving_delivery(current, plan)?;
    if !survivors["events"].as_array().is_some_and(Vec::is_empty) {
        s["deliveries"][id] = survivors;
        return Ok(());
    }
    s["deliveries"].as_object_mut().unwrap().remove(id);
    if s["active"] == id {
        s["active"] = J::Null;
    }
    Ok(())
}

fn clear_owned_receiver(s: &mut J, plan: &Associations) -> Result<()> {
    if s["receiver"].is_null() {
        return Ok(());
    }
    require(
        s["receiver"] == plan.receiver,
        "owned cleanup receiver changed",
    )?;
    s["receiver"] = J::Null;
    s["guard_dir"] = J::Null;
    s["notify"] = json!(false);
    s["delivery_status"] = json!("disabled");
    Ok(())
}
