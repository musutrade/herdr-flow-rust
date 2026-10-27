use super::*;

pub(super) fn validate(value: &J) -> Result<()> {
    ensure(
        value["version"] == 3 && value["workspaces"].is_array(),
        "only Herdr v3 session persistence is supported",
    )
}

pub(super) fn prune(value: &mut J, binding: &J) -> Result<bool> {
    validate(value)?;
    required(binding, "session")?;
    required(binding, "cwd")?;
    let mut changed = false;
    for workspace in value["workspaces"].as_array_mut().unwrap() {
        changed |= prune_workspace(workspace, binding)?;
    }
    Ok(changed)
}

fn prune_workspace(workspace: &mut J, binding: &J) -> Result<bool> {
    let scope = json!({"workspace":workspace["id"],"numbers":workspace["public_pane_numbers"]});
    let tabs = workspace["tabs"]
        .as_array_mut()
        .ok_or_else(|| FlowError::guard("Herdr tabs missing"))?;
    let mut removed = Vec::new();
    for tab in tabs {
        removed.extend(prune_tab(tab, binding, &scope)?);
    }
    if removed.is_empty() {
        return Ok(false);
    }
    let numbers = workspace["public_pane_numbers"]
        .as_object_mut()
        .ok_or_else(|| FlowError::guard("Herdr pane map missing"))?;
    for id in removed {
        numbers.remove(&id);
    }
    Ok(true)
}

fn owned_ids(tab: &J, binding: &J, scope: &J) -> Result<Vec<String>> {
    let panes = tab["panes"]
        .as_object()
        .ok_or_else(|| FlowError::guard("Herdr panes missing"))?;
    let mut ids = Vec::new();
    for (id, pane) in panes {
        if matches_saved_pane(id, pane, binding, scope)? {
            ids.push(id.clone());
        }
    }
    Ok(ids)
}

fn prune_tab(tab: &mut J, binding: &J, scope: &J) -> Result<Vec<String>> {
    let ids = owned_ids(tab, binding, scope)?;
    for id in &ids {
        remove_pane(tab, id)?;
    }
    Ok(ids)
}

fn matches_saved_pane(id: &str, pane: &J, binding: &J, scope: &J) -> Result<bool> {
    let session = &pane["agent_session"]["value"];
    let captured = binding["persisted_panes"].as_array().is_some_and(|ids| {
        ids.contains(&json!({"workspace":scope["workspace"],"id":id,"number":scope["numbers"][id]}))
    });
    if *session != binding["session"] && !captured {
        return Ok(false);
    }
    ensure(
        pane["cwd"] == binding["cwd"],
        "Herdr saved task pane directory changed",
    )?;
    validate_session(pane, binding)?;
    Ok(true)
}

fn validate_session(pane: &J, binding: &J) -> Result<()> {
    if pane["agent_session"].is_null() {
        return Ok(());
    }
    ensure(
        pane["agent_session"]["value"] == binding["session"]
            && pane["agent_session"]["agent"] == "codex",
        "Herdr saved pane now belongs to another session",
    )
}

pub(super) fn identify(value: &J, binding: &J) -> Result<Vec<J>> {
    validate(value)?;
    let mut found = Vec::new();
    for workspace in value["workspaces"].as_array().unwrap() {
        identify_workspace(workspace, binding, &mut found)?;
    }
    Ok(found)
}

fn identify_workspace(workspace: &J, binding: &J, found: &mut Vec<J>) -> Result<()> {
    let scope = json!({"workspace":workspace["id"],"numbers":workspace["public_pane_numbers"]});
    let tabs = workspace["tabs"]
        .as_array()
        .ok_or_else(|| FlowError::guard("Herdr tabs missing"))?;
    for tab in tabs {
        for id in owned_ids(tab, binding, &scope)? {
            ensure(
                scope["numbers"][&id].is_u64() && scope["workspace"].is_string(),
                "Herdr persisted pane mapping missing",
            )?;
            found.push(
                json!({"workspace":scope["workspace"],"id":id,"number":scope["numbers"][&id]}),
            );
        }
    }
    Ok(())
}

fn remove_pane(tab: &mut J, id: &str) -> Result<()> {
    let number: u64 = id
        .parse()
        .map_err(|_| FlowError::guard("invalid Herdr internal pane id"))?;
    ensure(
        tab["root_pane"] != number,
        "cannot reclaim another tab's root from a recovery copy",
    )?;
    tab["layout"] = without(&tab["layout"], number)?
        .ok_or_else(|| FlowError::guard("cannot remove an entire user tab"))?;
    tab["panes"].as_object_mut().unwrap().remove(id);
    if tab["focused"] == number {
        tab["focused"] = tab["root_pane"].clone();
        tab["zoomed"] = json!(false);
    }
    Ok(())
}

fn without(node: &J, id: u64) -> Result<Option<J>> {
    if let Some(number) = node["Pane"].as_u64() {
        return Ok((number != id).then(|| node.clone()));
    }
    ensure(node["Split"].is_object(), "unknown Herdr layout node")?;
    let first = without(&node["Split"]["first"], id)?;
    let second = without(&node["Split"]["second"], id)?;
    match (first, second) {
        (Some(first), Some(second)) => {
            let mut result = node.clone();
            result["Split"]["first"] = first;
            result["Split"]["second"] = second;
            Ok(Some(result))
        }
        (a, b) => Ok(a.or(b)),
    }
}
