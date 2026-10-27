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
    let scope = json!({"workspace":workspace["id"],"numbers":workspace["public_pane_numbers"],
        "tabs":workspace["public_tab_numbers"]});
    let tabs = workspace["tabs"]
        .as_array_mut()
        .ok_or_else(|| FlowError::guard("Herdr tabs missing"))?;
    let mut removed = Vec::new();
    let mut retired = Vec::new();
    for (index, tab) in tabs.iter_mut().enumerate() {
        let (ids, whole_tab) = prune_tab(tab, binding, &scope, index)?;
        removed.extend(ids);
        if whole_tab {
            retired.push(index);
        }
    }
    finish_workspace(workspace, removed, retired)
}

fn finish_workspace(workspace: &mut J, removed: Vec<String>, retired: Vec<usize>) -> Result<bool> {
    if removed.is_empty() {
        return Ok(false);
    }
    let numbers = workspace["public_pane_numbers"]
        .as_object_mut()
        .ok_or_else(|| FlowError::guard("Herdr pane map missing"))?;
    for id in removed {
        numbers.remove(&id);
    }
    for index in retired.into_iter().rev() {
        remove_tab(workspace, index)?;
    }
    Ok(true)
}

fn remove_tab(workspace: &mut J, index: usize) -> Result<()> {
    let length = workspace["tabs"].as_array().unwrap().len();
    ensure(
        length > 1,
        "cannot remove the last tab from a user workspace",
    )?;
    let active = workspace["active_tab"]
        .as_u64()
        .ok_or_else(|| FlowError::guard("Herdr active tab missing"))? as usize;
    ensure(active < length, "Herdr active tab is out of range")?;
    let numbers = workspace["public_tab_numbers"]
        .as_array_mut()
        .ok_or_else(|| FlowError::guard("Herdr tab map missing"))?;
    ensure(numbers.len() == length, "Herdr tab map length changed")?;
    numbers.remove(index);
    workspace["tabs"].as_array_mut().unwrap().remove(index);
    workspace["active_tab"] = json!(if active > index {
        active - 1
    } else {
        active.min(length - 2)
    });
    Ok(())
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

fn prune_tab(tab: &mut J, binding: &J, scope: &J, index: usize) -> Result<(Vec<String>, bool)> {
    let ids = owned_ids(tab, binding, scope)?;
    if ids.contains(&tab["root_pane"].to_string()) {
        owned_tab(binding, scope, index)?;
        if tab["panes"].as_object().unwrap().len() == ids.len() {
            return Ok((ids, true));
        }
    }
    for id in &ids {
        remove_pane(tab, id)?;
    }
    Ok((ids, false))
}

fn owned_tab(binding: &J, scope: &J, index: usize) -> Result<()> {
    ensure(
        binding["workspace"] == scope["workspace"]
            && binding["tab_number"].is_u64()
            && binding["tab_number"] == scope["tabs"][index],
        "cannot reclaim another tab's root from a recovery copy",
    )
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
    tab["layout"] = without(&tab["layout"], number)?
        .ok_or_else(|| FlowError::guard("cannot remove an entire user tab"))?;
    tab["panes"].as_object_mut().unwrap().remove(id);
    if tab["root_pane"] == number {
        tab["root_pane"] = first_pane(&tab["layout"])?;
    }
    if tab["focused"] == number {
        tab["focused"] = tab["root_pane"].clone();
        tab["zoomed"] = json!(false);
    }
    Ok(())
}

fn first_pane(node: &J) -> Result<J> {
    if node["Pane"].is_u64() {
        return Ok(node["Pane"].clone());
    }
    ensure(node["Split"].is_object(), "unknown Herdr layout node")?;
    first_pane(&node["Split"]["first"])
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
