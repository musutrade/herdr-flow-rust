use super::{array, object, overlap, overlaps, path, require, text, Findings};
use crate::{model::J, progress, Result};
use serde_json::json;
use std::{collections::BTreeSet, path::Path};

pub(super) fn inspect(s: &J, name: &str, checkout: &Path, f: &mut Findings) -> Result<()> {
    require(
        object(&s["tasks"], "tasks")?.len() <= 4096,
        "queue task observation limit",
    )?;
    validate_active(s)?;
    let mut related = related_tasks(s, name, checkout, f)?;
    let markers = expand_related(s, &mut related)?;
    let before = f.blockers.len();
    inspect_set(s, &related, f)?;
    if f.blockers.len() == before {
        for marker in markers {
            f.history("settled_claim_marker", marker);
        }
    }
    Ok(())
}

fn related_tasks(s: &J, name: &str, checkout: &Path, f: &mut Findings) -> Result<BTreeSet<String>> {
    let mut related = BTreeSet::from([name.to_owned()]);
    for (key, t) in object(&s["tasks"], "tasks")? {
        require(t["task"] == *key, "task identity mismatch")?;
        if touches(t, checkout)? {
            related.insert(key.clone());
        }
        lane(s, t, name, checkout, f)?;
        lane_related(t, checkout, &mut related)?;
    }
    Ok(related)
}

fn inspect_set(s: &J, related: &BTreeSet<String>, f: &mut Findings) -> Result<()> {
    for id in related {
        let t = &s["tasks"][id];
        task(s, t, f)?;
        results(s, t, f)?;
        publication(t, f)?;
        source_delivery(s, t, f)?;
    }
    Ok(())
}

fn validate_active(s: &J) -> Result<()> {
    if s["active"].is_null() {
        return Ok(());
    }
    let id = text(&s["active"], "active delivery")?;
    require(
        s["deliveries"][id].is_object(),
        "active delivery record missing",
    )
}

fn lane_related(t: &J, p: &Path, related: &mut BTreeSet<String>) -> Result<()> {
    let l = &t["lane"];
    if l.is_null() {
        return Ok(());
    }
    let mut groups = vec![array(&l["pending"], "lane.pending")?];
    if !l["active"].is_null() {
        groups.push(array(&l["active"]["members"], "active.members")?);
    }
    for b in array(&l["history"], "lane.history")? {
        groups.push(array(&b["members"], "history.members")?);
    }
    related_members(groups, p, related)
}

fn related_members(groups: Vec<&Vec<J>>, p: &Path, related: &mut BTreeSet<String>) -> Result<()> {
    for members in groups {
        for m in members {
            if overlaps(&path(&m["checkout"])?, p) {
                related.insert(text(&m["task"], "member task")?.to_owned());
            }
        }
    }
    Ok(())
}

fn touches(t: &J, p: &Path) -> Result<bool> {
    let mut paths = Vec::new();
    if !t["writer"].is_null() {
        paths.push(path(&t["writer"]["workspace"])?);
    }
    if !t["pr"].is_null() {
        paths.push(path(&t["pr"]["checkout"])?);
    }
    for op in object(&t["operations"], "operations")?.values() {
        if !op["intent"]["workspace"].is_null() {
            paths.push(path(&op["intent"]["workspace"])?);
        }
    }
    Ok(paths.iter().any(|other| overlaps(p, other)))
}

fn task(s: &J, t: &J, f: &mut Findings) -> Result<()> {
    task_identity(t)?;
    findings(t, f)?;
    current_wait(s, t)?;
    next_action(t)?;
    work_state(t, f)?;
    operations(s, t, f)
}

fn task_identity(t: &J) -> Result<()> {
    text(&t["task"], "task identity")?;
    require(
        t["epoch"].as_u64().is_some_and(|v| v > 0),
        "task epoch missing",
    )?;
    require(
        matches!(
            t["control"].as_str(),
            Some("running" | "paused" | "cancelled")
        ),
        "unknown task control",
    )
}

fn work_state(t: &J, f: &mut Findings) -> Result<()> {
    let id = text(&t["task"], "task identity")?;
    if !t["delivery"].is_null() {
        f.block("owned_delivery_lifecycle", id);
    }
    if !t["writer"].is_null() {
        f.block("writer", id);
    }
    if !array(&t["pending"], "task.pending")?.is_empty() {
        f.block("pending_results", id);
    }
    task_phase(t, id, f);
    Ok(())
}

fn task_phase(t: &J, id: &str, f: &mut Findings) {
    match t["phase"].as_str() {
        Some("blocked" | "completed") => (),
        Some("waiting" | "next_pending" | "executing" | "result_pending") => {
            f.block("task_not_settled", id)
        }
        _ => f.unknown("task_phase_unknown", id),
    }
}

fn next_action(t: &J) -> Result<()> {
    let action = &t["next_action"];
    match text(&action["kind"], "next action")? {
        "dispatch" => {
            let id = text(&action["operation"], "dispatch operation")?;
            require(
                t["operations"][id].is_object(),
                "dispatch operation missing",
            )
        }
        "complete" | "blocked" | "wait" => Ok(()),
        _ => Err(crate::FlowError::queue("unknown next action")),
    }
}

fn operations(s: &J, t: &J, f: &mut Findings) -> Result<()> {
    for (key, op) in object(&t["operations"], "operations")? {
        if op["phase"] == "accepted" && applied_operation(s, t, op)? {
            f.history(
                "accepted_operation",
                json!({"task":t["task"],"operation":key}),
            );
        } else {
            f.block("unfinished_operation", format!("{}/{}", t["task"], key));
        }
    }
    Ok(())
}

fn findings(t: &J, f: &mut Findings) -> Result<()> {
    for (id, item) in object(&t["findings"], "task.findings")? {
        require(
            item["blocking"].is_boolean(),
            "finding lacks blocking classification",
        )?;
        if item["blocking"] == true && item["resolved"].is_null() {
            f.block(
                "unresolved_blocking_finding",
                format!("{}/{}", t["task"], id),
            );
        }
        if !item["resolved"].is_null() {
            text(&item["resolved"], "finding resolution")?;
        }
    }
    Ok(())
}

fn current_wait(s: &J, t: &J) -> Result<()> {
    let id = text(&t["wait"], "task wait")?;
    let w = &s["waits"][id];
    require(w.is_object(), "task wait record missing")?;
    require(
        ["task", "epoch", "candidate", "assignment"]
            .iter()
            .all(|key| !t[key].is_null() && w[key] == t[key]),
        "current wait identity mismatch",
    )
}

fn applied_operation(s: &J, t: &J, op: &J) -> Result<bool> {
    let wait = text(&op["intent"]["wait"], "operation wait")?;
    let assignment = text(&op["assignment"], "operation assignment")?;
    Ok(object(&s["events"], "events")?.values().any(|e| {
        e["wait"] == wait
            && e["binding"]["assignment"] == assignment
            && binding(e, t)
            && complete(e)
    }))
}

fn binding(e: &J, t: &J) -> bool {
    let b = &e["binding"];
    b["task"] == t["task"] && b["epoch"] == t["epoch"] && b["candidate"] == t["candidate"]
}

fn complete(e: &J) -> bool {
    e["applied"] == true && e["acknowledged"] == true && e["outcome"] == "complete"
}

fn results(s: &J, t: &J, f: &mut Findings) -> Result<()> {
    let ids = related_events(s, t, f)?;
    related_waits(s, t, f)?;
    related_deliveries(s, &ids, f)
}

fn related_events(s: &J, t: &J, f: &mut Findings) -> Result<BTreeSet<String>> {
    let mut ids = BTreeSet::new();
    for (id, e) in object(&s["events"], "events")? {
        if e["task"] != t["task"] && e["binding"]["task"] != t["task"] {
            continue;
        }
        ids.insert(id.clone());
        if binding(e, t) && complete(e) {
            f.history("applied_complete_result", json!(id));
        } else {
            f.block("unsettled_result", id);
        }
    }
    Ok(ids)
}

fn related_waits(s: &J, t: &J, f: &mut Findings) -> Result<()> {
    for (id, w) in object(&s["waits"], "waits")? {
        if w["task"] == t["task"] {
            wait(s, t, id, w, f)?;
        }
    }
    Ok(())
}

fn related_deliveries(s: &J, ids: &BTreeSet<String>, f: &mut Findings) -> Result<()> {
    for (id, d) in object(&s["deliveries"], "deliveries")? {
        let members = array(&d["events"], "delivery.events")?;
        if members
            .iter()
            .any(|e| e.as_str().is_some_and(|e| ids.contains(e)))
        {
            delivery(s, id, d, f);
        }
    }
    Ok(())
}

fn wait(s: &J, t: &J, id: &str, w: &J, f: &mut Findings) -> Result<()> {
    let matched = object(&s["events"], "events")?.values().any(|e| {
        e["wait"] == id
            && binding(e, t)
            && e["binding"]["assignment"] == w["assignment"]
            && complete(e)
    });
    if matched
        && w["outcome"] == "complete"
        && w["epoch"] == t["epoch"]
        && w["candidate"] == t["candidate"]
    {
        f.history("completed_wait", json!(id));
    } else {
        f.block("unresolved_wait", id);
    }
    Ok(())
}

fn delivery(s: &J, id: &str, d: &J, f: &mut Findings) {
    if s["active"] == id || d["phase"] != "acknowledged" {
        f.block("active_or_unresolved_delivery", id);
    } else {
        f.history("acknowledged_delivery", json!(id));
    }
}

fn publication(t: &J, f: &mut Findings) -> Result<()> {
    let pr = &t["pr"];
    if pr.is_null() {
        return Ok(());
    }
    require(
        pr["head"] == t["candidate"],
        "publication candidate mismatch",
    )?;
    if landed(pr) && quality_settled(&pr["observation"]) {
        f.history(
            "verified_landing",
            json!({"task":t["task"],"landing":pr["landing"]}),
        );
    } else {
        f.block("unresolved_publication", t["task"].to_string());
    }
    Ok(())
}

fn landed(pr: &J) -> bool {
    pr["phase"] == "merged"
        && pr["publication"]["phase"] == "observed"
        && progress::ownership::confirmed_landing(pr)
}

fn lane(s: &J, t: &J, name: &str, p: &Path, f: &mut Findings) -> Result<()> {
    let lane = &t["lane"];
    if lane.is_null() {
        return Ok(());
    }
    pending_members(lane, name, p, f)?;
    active_batch(lane, name, p, f)?;
    historical_batches(s, t, name, p, f)
}

fn pending_members(lane: &J, name: &str, p: &Path, f: &mut Findings) -> Result<()> {
    for m in array(&lane["pending"], "lane.pending")? {
        if member_ref(m, name, p)? {
            f.block("pending_lane_member", m["task"].to_string());
        }
    }
    Ok(())
}

fn active_batch(lane: &J, name: &str, p: &Path, f: &mut Findings) -> Result<()> {
    let active = &lane["active"];
    if active.is_null() {
        return Ok(());
    }
    if !active["workspace"].is_null() {
        overlap(p, &path(&active["workspace"])?, "active integration", f);
    }
    if batch_ref(active, name, p)? {
        f.block("active_batch", active["id"].to_string());
    }
    Ok(())
}

fn historical_batches(s: &J, t: &J, name: &str, p: &Path, f: &mut Findings) -> Result<()> {
    for b in array(&t["lane"]["history"], "lane.history")? {
        if !batch_ref(b, name, p)? {
            continue;
        }
        if history_landed(b) {
            history_members(s, t, b, name, p)?;
            f.history("landed_batch", json!({"owner":t["task"],"batch":b["id"]}));
        } else {
            f.block("unverified_batch_history", b["id"].to_string());
        }
    }
    Ok(())
}

fn history_landed(b: &J) -> bool {
    b["phase"] == "landed"
        && landed(&b["pr"])
        && b["landing"] == b["pr"]["landing"]
        && b["pr"]["head"] == b["snapshot"]["head"]
        && b["landing"]["head"] == b["snapshot"]["head"]
        && quality_settled(&b["ci"])
        && quality_settled(&b["pr"]["observation"])
}

// No remote CI request here. An existing unresolved observation is a hold;
// absence is not converted into invented CI acceptance evidence.
fn quality_settled(observation: &J) -> bool {
    if observation.is_null() {
        return true;
    }
    if observation["state"] != "success" {
        return false;
    }
    match observation.get("failures") {
        None => true,
        Some(items) => items.as_array().is_some_and(Vec::is_empty),
    }
}

fn history_members(s: &J, owner: &J, b: &J, name: &str, p: &Path) -> Result<()> {
    for m in array(&b["members"], "history.members")? {
        if !member_ref(m, name, p)? {
            continue;
        }
        let id = text(&m["task"], "history member task")?;
        let t = &s["tasks"][id];
        history_member_identity(t, m, owner)?;
        history_landing_binding(&t["batch_delivery"], b)?;
    }
    Ok(())
}

fn history_member_identity(t: &J, m: &J, owner: &J) -> Result<()> {
    require(
        t["task"] == m["task"] && m["epoch"] == t["epoch"] && m["head"] == t["candidate"],
        "history member identity mismatch",
    )?;
    let d = &t["batch_delivery"];
    require(
        d["owner"] == owner["task"] && d["receipt_id"] == m["receipt_id"] && d["phase"] == "landed",
        "history source receipt mismatch",
    )
}

fn history_landing_binding(d: &J, b: &J) -> Result<()> {
    require(
        d["batch"] == b["id"]
            && d["landing"]["commit"] == b["landing"]["commit"]
            && d["landing"]["batch_head"] == b["snapshot"]["head"]
            && d["landing"]["tree"] == b["snapshot"]["tree"],
        "history landing binding mismatch",
    )
}

fn member_ref(m: &J, name: &str, p: &Path) -> Result<bool> {
    Ok(m["task"] == name || overlaps(&path(&m["checkout"])?, p))
}

fn batch_ref(b: &J, name: &str, p: &Path) -> Result<bool> {
    for m in array(&b["members"], "batch.members")? {
        if member_ref(m, name, p)? {
            return Ok(true);
        }
    }
    Ok(false)
}

fn expand_related(s: &J, related: &mut BTreeSet<String>) -> Result<Vec<J>> {
    let mut inspected = BTreeSet::new();
    let mut markers = Vec::new();
    loop {
        let Some(name) = related.difference(&inspected).next().cloned() else {
            return Ok(markers);
        };
        require(inspected.len() < 256, "related task observation limit")?;
        inspected.insert(name.clone());
        let t = &s["tasks"][&name];
        require(t["task"] == name, "referenced task missing")?;
        claim_relations(s, t, related, &mut markers)?;
    }
}

fn claim_relations(
    s: &J,
    t: &J,
    related: &mut BTreeSet<String>,
    markers: &mut Vec<J>,
) -> Result<()> {
    if !t["claim"].is_null() {
        progress::claim::validate(s, t)?;
    }
    if t["claim"]["kind"] == "member" {
        related.insert(text(&t["claim"]["root"], "claim root")?.to_owned());
    }
    for (id, other) in object(&s["tasks"], "tasks")? {
        if other["claim"]["root"] == t["task"] {
            related.insert(id.clone());
        }
    }
    claim_marker(s, t, related, markers)
}

fn claim_marker(s: &J, t: &J, related: &mut BTreeSet<String>, markers: &mut Vec<J>) -> Result<()> {
    let marker = &t["batch_delivery"]["claimed_by"];
    if marker.is_null() {
        return Ok(());
    }
    progress::claim::validate(s, t)?;
    let next = text(&marker["task"], "claimed successor")?;
    require(
        s["tasks"][next]["epoch"] == marker["epoch"],
        "successor epoch mismatch",
    )?;
    related.insert(next.to_owned());
    markers.push(json!({"source":t["task"],"successor":next}));
    Ok(())
}

fn source_delivery(s: &J, t: &J, f: &mut Findings) -> Result<()> {
    let d = &t["batch_delivery"];
    if d.is_null() {
        return Ok(());
    }
    let owner = text(&d["owner"], "source lane owner")?;
    require(s["tasks"][owner]["lane"].is_object(), "source lane missing")?;
    require(d["head"] == t["candidate"], "source candidate mismatch")?;
    if d["phase"] != "landed" {
        f.block("unlanded_source_receipt", t["task"].to_string());
        return Ok(());
    }
    source_landing(s, owner, d)?;
    let l = &d["landing"];
    f.history(
        "landed_source",
        json!({"task":t["task"],"receipt_id":d["receipt_id"],"landing":l}),
    );
    Ok(())
}

fn source_landing(s: &J, owner: &str, d: &J) -> Result<()> {
    let l = &d["landing"];
    require(
        l["head"] == d["head"] && l["batch"] == d["batch"],
        "source landing identity mismatch",
    )?;
    for field in ["commit", "batch_head", "tree"] {
        require(
            l[field].as_str().is_some_and(crate::model::is_full_sha),
            "source landing SHA missing",
        )?;
    }
    require(
        l["repo"] == s["tasks"][owner]["lane"]["repo"]
            && l["base_ref"] == s["tasks"][owner]["lane"]["base_ref"],
        "source landing target mismatch",
    )?;
    Ok(())
}
