//! V1 admission and one-use intake receipts. State lives only on tasks.
//!
//! Callers hold the existing queue lock. Registration stages all mutations;
//! these checks neither dispatch work nor retry a failed durable save.
use serde::Deserialize;
use serde_json::json;
use std::collections::BTreeSet;
use std::path::Path;

use super::{ensure, model, progress_error, J};
use crate::Result;

pub const SCHEMA: i64 = 3;

/// Fresh isolated authority cannot infer a shared writable-root claim from a
/// task name, an intake receipt, or an unbound bootstrap task.
pub fn isolated_admission(s: &J, name: &str, claim: &J) -> Result<()> {
    if s["lifecycle"].is_null() {
        return Ok(());
    }
    let source = match claim["kind"].as_str() {
        Some("member") => claim["root"].as_str(),
        Some("root") => claim["previous"]["task"].as_str(),
        _ => None,
    };
    let Some(source) = source else {
        return Ok(());
    };
    let target = crate::lifecycle::task_owner(s, name)?;
    let source = crate::lifecycle::task_owner(s, source)?;
    ensure(
        target.is_some() && target == source,
        "isolated cross-owner or unbound same-root claims are unsupported",
    )
}

#[derive(Clone, Deserialize)]
#[serde(tag = "kind", deny_unknown_fields)]
enum Claim {
    #[serde(rename = "independent")]
    Independent {
        version: i64,
        admission_evidence: String,
    },
    #[serde(rename = "root")]
    Root {
        version: i64,
        admission_evidence: String,
        lane: String,
        previous: Option<Previous>,
    },
    #[serde(rename = "member")]
    Member {
        version: i64,
        admission_evidence: String,
        root: String,
        root_epoch: i64,
    },
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Previous {
    task: String,
    epoch: i64,
    candidate: String,
    receipt_id: String,
}

fn valid_text(value: &str) -> Result<()> {
    super::text(Some(&json!(value)), "claim identity", 1000).map(|_| ())
}

fn parse(value: &J) -> Result<Claim> {
    let claim: Claim = serde_json::from_value(value.clone())
        .map_err(|e| progress_error(format!("invalid claim: {e}")))?;
    let (version, evidence) = match &claim {
        Claim::Independent {
            version,
            admission_evidence,
        }
        | Claim::Root {
            version,
            admission_evidence,
            ..
        }
        | Claim::Member {
            version,
            admission_evidence,
            ..
        } => (*version, admission_evidence),
    };
    ensure(version == 1, "unsupported claim version")?;
    valid_text(evidence)?;
    ensure(
        Path::new(evidence).is_absolute(),
        "claim admission evidence must be absolute",
    )?;
    validate_fields(value, &claim)?;
    Ok(claim)
}

fn validate_fields(value: &J, claim: &Claim) -> Result<()> {
    match claim {
        Claim::Independent { .. } => Ok(()),
        Claim::Member {
            root, root_epoch, ..
        } => {
            valid_text(root)?;
            ensure(*root_epoch > 0, "claim root epoch must be positive")
        }
        Claim::Root { lane, previous, .. } => root_fields(value, lane, previous.as_ref()),
    }
}

fn root_fields(value: &J, lane: &str, previous: Option<&Previous>) -> Result<()> {
    valid_text(lane)?;
    ensure(
        value.get("previous").is_some(),
        "claim root requires explicit previous",
    )?;
    if let Some(p) = previous {
        previous_fields(p)?;
    }
    Ok(())
}

fn previous_fields(p: &Previous) -> Result<()> {
    valid_text(&p.task)?;
    valid_text(&p.receipt_id)?;
    ensure(
        p.epoch > 0 && model::is_full_sha(&p.candidate),
        "invalid previous claim identity",
    )
}

fn stored(t: &J) -> Result<Option<Claim>> {
    match t.get("claim") {
        Some(value) => {
            ensure(
                model::int(t, "schema_version") == Some(SCHEMA),
                "claim requires task schema 3",
            )?;
            ensure(
                t.get("lineage").is_none(),
                "claim tasks cannot use V2 lineage",
            )?;
            parse(value).map(Some)
        }
        None => {
            ensure(
                model::int(t, "schema_version") != Some(SCHEMA),
                "schema 3 task is missing claim",
            )?;
            Ok(None)
        }
    }
}

fn named<'a>(s: &'a J, name: &str) -> Result<&'a J> {
    let t = model::dig(s, &["tasks", name]).ok_or_else(|| progress_error("claim task missing"))?;
    ensure(t["task"] == name, "claim task map identity mismatch")?;
    Ok(t)
}

fn root<'a>(s: &'a J, t: &'a J) -> Result<Option<&'a J>> {
    match stored(t)? {
        None | Some(Claim::Independent { .. }) => Ok(None),
        Some(Claim::Root { .. }) => Ok(Some(t)),
        Some(Claim::Member {
            root, root_epoch, ..
        }) => {
            let r = named(s, &root)?;
            ensure(
                model::int(r, "epoch") == Some(root_epoch),
                "claim root epoch changed",
            )?;
            ensure(
                matches!(stored(r)?, Some(Claim::Root { .. })),
                "member must refer directly to a root",
            )?;
            Ok(Some(r))
        }
    }
}

fn lane_name(r: &J) -> Result<String> {
    match stored(r)? {
        Some(Claim::Root { lane, .. }) => Ok(lane),
        _ => Err(progress_error("claim root required")),
    }
}

fn lane<'a>(s: &'a J, owner: &str) -> Result<&'a J> {
    let t = named(s, owner)?;
    let lane = t
        .get("lane")
        .filter(|v| v.is_object())
        .ok_or_else(|| progress_error("claim lane missing"))?;
    for field in ["repo", "host", "base_ref"] {
        super::text(lane.get(field), "claim lane target", 1000)?;
    }
    ensure(lane["pending"].is_array(), "invalid claim lane pending")?;
    let capacity = model::int(lane, "capacity").unwrap_or(0);
    ensure((1..=16).contains(&capacity), "invalid claim lane capacity")?;
    Ok(lane)
}

fn root_lane<'a>(s: &'a J, r: &J) -> Result<&'a J> {
    lane(s, &lane_name(r)?)
}

fn marker_matches(source: &J, next: &J) -> bool {
    source["batch_delivery"]["claimed_by"] == json!({"task":next["task"],"epoch":next["epoch"]})
}

fn previous_matches(p: &Previous, source: &J) -> bool {
    source["task"] == p.task
        && source["epoch"] == p.epoch
        && source["candidate"] == p.candidate
        && source["batch_delivery"]["receipt_id"] == p.receipt_id
}

fn receipt<'a>(s: &'a J, source: &J, owner: &str) -> Result<&'a J> {
    let l = lane(s, owner)?;
    receipt_identity(source, owner)?;
    match source["batch_delivery"]["phase"].as_str() {
        Some("accepted" | "frozen") => member_receipt(l, source)?,
        Some("landed") => landing_receipt(l, source)?,
        _ => return Err(progress_error("claim source is not accepted")),
    }
    Ok(l)
}

fn receipt_identity(source: &J, owner: &str) -> Result<()> {
    let d = &source["batch_delivery"];
    ensure(d["owner"] == owner, "claim receipt lane mismatch")?;
    super::text(d.get("receipt_id"), "accepted receipt ID", 128)?;
    ensure(
        model::is_full_sha(source["candidate"].as_str().unwrap_or("")),
        "claim source candidate invalid",
    )?;
    ensure(
        d["head"] == source["candidate"],
        "claim source candidate changed",
    )
}

fn member_receipt(l: &J, source: &J) -> Result<()> {
    let m = unique_member(l, source)?;
    member_location(l, source, m)?;
    member_identity(m, source)?;
    member_provenance(l, m)
}

fn unique_member<'a>(l: &'a J, source: &J) -> Result<&'a J> {
    let pending = l["pending"].as_array().into_iter().flatten();
    let active = l["active"]["members"].as_array().into_iter().flatten();
    let members: Vec<_> = pending
        .chain(active)
        .filter(|m| m["task"] == source["task"])
        .collect();
    ensure(
        members.len() == 1,
        "claim receipt requires exact unique lane member",
    )?;
    Ok(members[0])
}

fn member_identity(m: &J, source: &J) -> Result<()> {
    let d = &source["batch_delivery"];
    for field in ["receipt_id", "head", "own_commits"] {
        ensure(
            m.get(field).is_some() && m[field] == d[field],
            "claim member receipt mismatch",
        )?;
    }
    ensure(m["epoch"] == source["epoch"], "claim member epoch changed")
}

fn member_provenance(l: &J, m: &J) -> Result<()> {
    ensure(
        m["repo"] == l["repo"] && m["host"] == l["host"],
        "claim member repository changed",
    )?;
    crate::pr::sha(m.get("original_base"))?;
    let checkout = super::text(m.get("checkout"), "source checkout", 1000)?;
    ensure(
        Path::new(&checkout).is_absolute(),
        "source checkout must be absolute",
    )?;
    valid_commits(&m["own_commits"])
}

fn member_location(l: &J, source: &J, member: &J) -> Result<()> {
    let d = &source["batch_delivery"];
    let items = if d["phase"] == "accepted" {
        &l["pending"]
    } else {
        super::text(d.get("batch"), "accepted batch", 128)?;
        ensure(
            l["active"]["id"] == d["batch"],
            "claim frozen batch changed",
        )?;
        &l["active"]["members"]
    };
    ensure(
        items
            .as_array()
            .map(|items| items.contains(member))
            .unwrap_or(false),
        "claim member phase mismatch",
    )
}

fn valid_commits(commits: &J) -> Result<()> {
    let items = commits
        .as_array()
        .ok_or_else(|| progress_error("invalid receipt commits"))?;
    ensure(
        !items.is_empty() && items.len() <= 256,
        "invalid receipt commit count",
    )?;
    for item in items {
        crate::pr::sha(Some(item))?;
    }
    Ok(())
}

fn landing_receipt(l: &J, source: &J) -> Result<()> {
    let d = &source["batch_delivery"];
    let landing = &d["landing"];
    ensure(
        landing["head"] == d["head"] && landing["batch"] == d["batch"],
        "claim landing mismatch",
    )?;
    super::text(d.get("batch"), "accepted batch", 128)?;
    ensure(
        landing["repo"] == l["repo"] && landing["base_ref"] == l["base_ref"],
        "claim landing target mismatch",
    )?;
    for key in ["commit", "batch_head", "tree"] {
        crate::pr::sha(landing.get(key))?;
    }
    valid_commits(&d["own_commits"])
}

/// Validate a saved identity without consuming again or consulting capacity.
pub fn validate(s: &J, t: &J) -> Result<()> {
    ensure(
        super::lineage::supported_schema(t),
        "unsupported task schema version",
    )?;
    if let Some(r) = root(s, t)? {
        validate_chain(s, r)?;
    }
    validate_marker(s, t)
}

fn validate_chain(s: &J, first: &J) -> Result<()> {
    let mut cursor = Some(first);
    let mut seen = BTreeSet::new();
    while let Some(r) = cursor {
        let name = super::text(r.get("task"), "claim root", 500)?;
        ensure(
            seen.insert(name) && seen.len() <= 100,
            "cyclic or oversized claim ancestry",
        )?;
        cursor = chain_step(s, r)?;
    }
    Ok(())
}

fn chain_step<'a>(s: &'a J, r: &J) -> Result<Option<&'a J>> {
    let Some(Claim::Root {
        lane: owner,
        previous,
        ..
    }) = stored(r)?
    else {
        return Err(progress_error("claim root required"));
    };
    lane(s, &owner)?;
    previous
        .map(|p| previous_root(s, r, &owner, &p))
        .transpose()
}

fn previous_root<'a>(s: &'a J, next: &J, owner: &str, p: &Previous) -> Result<&'a J> {
    let source = named(s, &p.task)?;
    ensure(
        previous_matches(p, source),
        "previous receipt identity changed",
    )?;
    ensure(
        marker_matches(source, next),
        "claim receipt consumption mismatch",
    )?;
    receipt(s, source, owner)?;
    let r = root(s, source)?
        .ok_or_else(|| progress_error("legacy/independent source cannot authorize claim"))?;
    ensure(
        lane_name(r)? == owner,
        "previous root belongs to another lane",
    )?;
    Ok(r)
}

fn validate_marker(s: &J, source: &J) -> Result<()> {
    let Some(marker) = source
        .get("batch_delivery")
        .and_then(|d| d.get("claimed_by"))
    else {
        return Ok(());
    };
    let name = super::text(marker.get("task"), "claimed successor", 500)?;
    let next = named(s, &name)?;
    ensure(marker_matches(source, next), "invalid claimed_by marker")?;
    let Some(Claim::Root {
        previous: Some(p), ..
    }) = stored(next)?
    else {
        return Err(progress_error("claimed successor has no previous receipt"));
    };
    ensure(
        previous_matches(&p, source),
        "claimed successor receipt mismatch",
    )?;
    validate_chain(s, next)
}

fn related(s: &J, r: &J, t: &J) -> Result<bool> {
    Ok(root(s, t)?
        .map(|other| other["task"] == r["task"])
        .unwrap_or(false))
}

fn accepted_sources<'a>(s: &'a J, r: &J) -> Result<Vec<&'a J>> {
    let mut sources = Vec::new();
    for t in s["tasks"].as_object().into_iter().flat_map(|m| m.values()) {
        if related(s, r, t)? && model::truthy(t.get("batch_delivery")) {
            sources.push(t);
        }
    }
    ensure(
        sources.len() <= 1,
        "multiple accepted sources for one claim root",
    )?;
    Ok(sources)
}

/// New execution is forbidden after same-root intake; reads/ACK/completion are not.
pub fn execution(s: &J, t: &J) -> Result<()> {
    validate(s, t)?;
    let Some(r) = root(s, t)? else {
        return Ok(());
    };
    ensure(r["control"] == "running", "claim root is inactive")?;
    active_ancestors(s, r)?;
    ensure(
        named(s, &lane_name(r)?)?["control"] == "running",
        "claim lane is inactive",
    )?;
    ensure(
        accepted_sources(s, r)?.is_empty(),
        "claim root is held at its accepted handoff",
    )
}

/// Called only on the registration staging copy, before reserving a writer.
pub(super) fn admit(s: &mut J, t: &mut J, spec: &J, now: f64) -> Result<()> {
    isolated_admission(s, t["task"].as_str().unwrap_or(""), &spec["claim"])?;
    if t.get("lineage").is_some() {
        return ensure(
            spec.get("claim").is_none(),
            "V2 lineage cannot adopt a V1 claim",
        );
    }
    let value = spec
        .get("claim")
        .ok_or_else(|| progress_error("new V1 task requires explicit claim admission"))?;
    let claim = parse(value)?;
    ensure(
        model::int(t, "epoch").unwrap_or(0) > 0,
        "claim task epoch must be positive",
    )?;
    t["claim"] = value.clone();
    t["schema_version"] = json!(SCHEMA);
    s["tasks"][t["task"].as_str().unwrap_or("")] = t.clone();
    if let Claim::Root {
        previous: Some(p), ..
    } = claim
    {
        consume(s, t, &p, now)?;
    }
    execution(s, t)
}

fn active_ancestors(s: &J, first: &J) -> Result<()> {
    // validate_chain has already bounded and cycle-checked this walk.
    let mut current = first;
    while let Some(Claim::Root {
        previous: Some(p), ..
    }) = stored(current)?
    {
        let source = named(s, &p.task)?;
        ensure(
            source["control"] == "running",
            "previous source is inactive",
        )?;
        current = root(s, source)?.ok_or_else(|| progress_error("previous root missing"))?;
        ensure(current["control"] == "running", "previous root is inactive")?;
    }
    Ok(())
}

fn consume(s: &mut J, t: &J, p: &Previous, now: f64) -> Result<()> {
    let mut source = eligible_source(s, t, p)?.clone();
    source["batch_delivery"]["claimed_by"] = json!({"task":t["task"],"epoch":t["epoch"]});
    crate::pr::store(s, source, now, "claim_receipt_consumed")?;
    Ok(())
}

fn eligible_source<'a>(s: &'a J, t: &J, p: &Previous) -> Result<&'a J> {
    let source = named(s, &p.task)?;
    validate(s, source)?;
    ensure(
        previous_matches(p, source),
        "previous receipt identity mismatch",
    )?;
    let r = root(s, source)?
        .ok_or_else(|| progress_error("legacy/independent receipt cannot authorize claim"))?;
    unique_source(s, r, source)?;
    source_eligible(s, t, r, source)?;
    root_released(s, r)?;
    Ok(source)
}

fn unique_source(s: &J, r: &J, source: &J) -> Result<()> {
    let sources = accepted_sources(s, r)?;
    ensure(
        sources.len() == 1 && sources[0]["task"] == source["task"],
        "claim requires one accepted source per root",
    )
}

fn source_eligible(s: &J, t: &J, r: &J, source: &J) -> Result<()> {
    ensure(
        source["control"] == "running" && r["control"] == "running",
        "previous source/root is inactive",
    )?;
    let owner = lane_name(t)?;
    ensure(lane_name(r)? == owner, "previous claim lane mismatch")?;
    admission_capacity(s, source, &owner)?;
    ensure(
        source["batch_delivery"].get("claimed_by").is_none(),
        "accepted receipt already consumed",
    )
}

fn admission_capacity(s: &J, source: &J, owner: &str) -> Result<()> {
    let l = receipt(s, source, owner)?;
    ensure(
        named(s, owner)?["control"] == "running",
        "claim lane is inactive",
    )?;
    let pending = l["pending"].as_array().unwrap().len();
    ensure(
        pending < model::int(l, "capacity").unwrap() as usize,
        "pending batch is full; claim not consumed",
    )
}

fn operation_finished(s: &J, task: &J, op: &J) -> bool {
    if op["phase"] != "accepted" {
        return false;
    }
    s["events"]
        .as_object()
        .into_iter()
        .flat_map(|m| m.values())
        .any(|e| {
            model::bool_or(e, "applied", false)
                && e["binding"]["task"] == task["task"]
                && e["binding"]["epoch"] == task["epoch"]
                && e["binding"]["assignment"] == op["assignment"]
                && e["wait"] == op["intent"]["wait"]
        })
}

fn released(s: &J, t: &J) -> Result<()> {
    ensure(
        t["pending"].is_array() && t["operations"].is_object(),
        "malformed claim work state",
    )?;
    crate::batch::validation::idle(t)?;
    ensure(
        matches!(t["phase"].as_str(), Some("blocked" | "completed")),
        "claim root has unfinished work",
    )?;
    ensure(t["control"] == "running", "claim member is inactive")?;
    for op in t["operations"]
        .as_object()
        .into_iter()
        .flat_map(|m| m.values())
    {
        ensure(
            operation_finished(s, t, op),
            "claim root has unfinished dispatch",
        )?;
    }
    Ok(())
}

/// Additional checks for new claims; old intake records retain their semantics.
pub fn offer(s: &J, owner: &J, source: &J) -> Result<()> {
    if stored(source)?.is_none() {
        return Ok(());
    }
    execution(s, source)?;
    let r = root(s, source)?
        .ok_or_else(|| progress_error("independent task cannot be a managed source"))?;
    ensure(
        lane_name(r)? == owner["task"],
        "offered claim lane mismatch",
    )?;
    root_lane(s, r)?;
    root_released(s, r)
}

fn root_released(s: &J, r: &J) -> Result<()> {
    for t in s["tasks"].as_object().into_iter().flat_map(|m| m.values()) {
        if related(s, r, t)? {
            released(s, t)?;
        }
    }
    Ok(())
}

/// Recheck same-root release at freeze/integration/publication without consuming.
pub fn held(s: &J, source: &J) -> Result<()> {
    validate(s, source)?;
    let Some(r) = root(s, source)? else {
        return Ok(());
    };
    let owner = lane_name(r)?;
    receipt(s, source, &owner)?;
    unique_source(s, r, source)?;
    root_released(s, r)
}

pub fn forbid_migration(t: &J) -> Result<()> {
    ensure(
        t.get("claim").is_none(),
        "claim tasks cannot migrate or discard their admission",
    )
}

pub fn lane_owner(s: &J, t: &J) -> Result<()> {
    ensure(
        root(s, t)?.is_none(),
        "managed claim cannot become a lane owner",
    )
}
