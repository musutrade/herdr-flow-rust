use super::*;
fn field(value: &J, key: &str) -> Result<String> {
    text(model::get(value, key), key, 1000)
}
fn rows<'a>(value: &'a J, key: &str) -> Result<&'a Vec<J>> {
    let items = model::array_field(value, key)
        .ok_or_else(|| progress_error(format!("{key} must be an array")))?;
    ensure(items.len() <= 64, "acceptance report exceeds bounds")?;
    Ok(items)
}
pub(super) fn report(s: &J, child: &J, value: &J) -> Result<()> {
    let conclusion = field(value, "conclusion")?;
    ensure(
        ["passed", "failed", "unable", "ambiguous"].contains(&conclusion.as_str()),
        "invalid conclusion",
    )?;
    field(value, "scope")?;
    let parent = &s["tasks"][child["lineage"]["requirement"].as_str().unwrap_or_default()];
    let revision = child["lineage"]["revision"].to_string();
    let requirement = &parent["requirements"][revision];
    let basis = &requirement["basis"];
    validate_checks(child, basis, value)?;
    validate_gaps(child, basis, value)?;
    validate_reuse(parent, child, value)?;
    validate_resolution(parent, value)?;
    validate_conclusion(parent, child, requirement, value, &conclusion)
}
fn validate_conclusion(
    parent: &J,
    child: &J,
    requirement: &J,
    value: &J,
    conclusion: &str,
) -> Result<()> {
    match conclusion {
        "passed" => {
            validate_open_gaps(parent, value)?;
            validate_pass(child, value)
        }
        "failed" => ensure(
            !rows(value, "gaps")?.is_empty(),
            "failed verdict needs grounded gaps",
        ),
        "unable" => {
            field(value, "blocker")?;
            field(value, "restore_condition").map(|_| ())
        }
        _ => validate_ambiguity(requirement, value),
    }
}
fn source(basis: &J, item: &J) -> Result<()> {
    let reference = field(item, "source")?;
    ensure(
        ["original", "changes", "specifications"].iter().any(|key| {
            basis[*key]
                .as_array()
                .is_some_and(|v| v.contains(&json!(reference)))
        }),
        "check source is not an agreed requirement reference",
    )
}
fn criterion(child: &J, item: &J) -> Result<()> {
    let key = field(item, "criterion")?;
    ensure(
        child["acceptance"][key].is_object(),
        "check invents a completion criterion",
    )
}
fn validate_checks(child: &J, basis: &J, value: &J) -> Result<()> {
    let mut ids = BTreeSet::new();
    for check in rows(value, "checks")? {
        ensure(ids.insert(field(check, "id")?), "duplicate check")?;
        criterion(child, check)?;
        source(basis, check)?;
        field(check, "evidence")?;
        ensure(
            ["passed", "failed", "blocked"].contains(&check["status"].as_str().unwrap_or_default()),
            "invalid check status",
        )?;
    }
    Ok(())
}
fn validate_gaps(child: &J, basis: &J, value: &J) -> Result<()> {
    let mut ids = BTreeSet::new();
    for gap in rows(value, "gaps")? {
        ensure(ids.insert(field(gap, "id")?), "duplicate gap")?;
        criterion(child, gap)?;
        source(basis, gap)?;
        field(gap, "evidence")?;
        field(gap, "release_condition")?;
    }
    Ok(())
}
fn validate_pass(child: &J, value: &J) -> Result<()> {
    ensure(
        rows(value, "gaps")?.is_empty(),
        "passed verdict has unresolved gaps",
    )?;
    let checks = rows(value, "checks")?;
    ensure(
        checks.iter().all(|c| c["status"] == "passed"),
        "passed verdict has nonpassing checks",
    )?;
    for (id, item) in object(&child["acceptance"], "criteria")? {
        if item["required"] != false {
            ensure(
                checks.iter().any(|c| c["criterion"] == *id),
                "required criterion is unchecked",
            )?;
        }
    }
    Ok(())
}
fn validate_ambiguity(requirement: &J, value: &J) -> Result<()> {
    field(value, "question")?;
    field(value, "unresolved_after_review")?;
    let checked = rows(value, "agreements_checked")?;
    for key in ["original", "changes", "specifications"] {
        let agreed = rows(&requirement["basis"], key)?;
        ensure(
            agreed.iter().all(|r| checked.contains(r)),
            "check existing agreements before asking for clarification",
        )?;
    }
    validate_checked_authorization(requirement, checked)
}
fn prior<'a>(parent: &'a J, item: &J) -> Result<&'a J> {
    let activity = field(item, "activity")?;
    let envelope = &parent["verdicts"][activity]["envelope"];
    ensure(envelope.is_object(), "previous verdict is unavailable")?;
    Ok(envelope)
}
fn validate_reuse(parent: &J, child: &J, value: &J) -> Result<()> {
    for item in rows(value, "reuses")? {
        validate_reused_item(parent, child, item)?;
    }
    Ok(())
}
fn validate_reused_item(parent: &J, child: &J, item: &J) -> Result<()> {
    let old = prior(parent, item)?;
    ensure(
        old["binding"]["lineage"]["revision"] == child["lineage"]["revision"],
        "old requirements cannot supply current acceptance evidence",
    )?;
    field(item, "applicability")?;
    validate_prior_check(old, item)?;
    if old["binding"]["candidate"] != child["candidate"] {
        field(item, "equivalence_evidence")?;
    }
    Ok(())
}
fn validate_prior_check(old: &J, item: &J) -> Result<()> {
    let check = field(item, "check")?;
    ensure(
        rows(&old["report"], "checks")?
            .iter()
            .any(|c| c["id"] == check && c["evidence"] == item["evidence"]),
        "reused evidence was not recorded",
    )?;
    Ok(())
}
fn validate_resolution(parent: &J, value: &J) -> Result<()> {
    for item in rows(value, "resolutions")? {
        let old = prior(parent, item)?;
        let gap = field(item, "gap")?;
        ensure(
            rows(&old["report"], "gaps")?.iter().any(|g| g["id"] == gap),
            "resolved gap was not recorded",
        )?;
        field(item, "evidence")?;
        field(item, "explanation")?;
    }
    Ok(())
}
pub(super) fn completion_evidence(report: &J) -> Result<J> {
    let mut evidence = json!({});
    for check in rows(report, "checks")? {
        evidence[field(check, "criterion")?] = json!(field(check, "evidence")?);
    }
    Ok(evidence)
}

fn validate_open_gaps(parent: &J, value: &J) -> Result<()> {
    if let Some(gaps) = parent["open_gaps"].as_object() {
        for gap in gaps.values() {
            ensure(
                rows(value, "resolutions")?
                    .iter()
                    .any(|r| r["activity"] == gap["activity"] && r["gap"] == gap["gap"]),
                "pass must resolve previous gaps with evidence",
            )?;
        }
    }
    Ok(())
}
pub(super) fn record_gaps(parent: &mut J, child: &J) -> Result<()> {
    let report = &child["verdict"]["envelope"]["report"];
    if report.is_null() {
        return Ok(());
    }
    if parent["open_gaps"].is_null() {
        parent["open_gaps"] = json!({});
    }
    for resolution in rows(report, "resolutions")? {
        let key = json!([field(resolution, "activity")?, field(resolution, "gap")?]).to_string();
        parent["open_gaps"].as_object_mut().unwrap().remove(&key);
    }
    for gap in rows(report, "gaps")? {
        let activity = field(child, "task")?;
        let id = field(gap, "id")?;
        parent["open_gaps"][json!([activity, id]).to_string()] =
            json!({"activity":activity,"gap":id,"detail":gap});
    }
    Ok(())
}

pub(super) fn independent(s: &J, child: &J, reviewer: &J) -> Result<()> {
    let parent = &s["tasks"][child["lineage"]["requirement"].as_str().unwrap_or_default()];
    if let Some(developer) =
        parent["artifacts"][child["candidate"].as_str().unwrap_or_default()]["activity"].as_str()
    {
        let wait = &s["waits"][s["tasks"][developer]["wait"].as_str().unwrap_or_default()];
        ensure(
            crate::identity::required_producer(wait)? != *reviewer,
            "artifact developer cannot be its independent reviewer",
        )?;
    }
    Ok(())
}

fn validate_checked_authorization(requirement: &J, checked: &[J]) -> Result<()> {
    ensure(
        checked.contains(&requirement["basis"]["confirmation"]),
        "check existing confirmation before clarification",
    )?;
    for permission in object(&requirement["authorization"], "existing authorization")?.values() {
        ensure(
            checked.contains(&permission["source"]),
            "check existing authorization before clarification",
        )?;
    }
    Ok(())
}
