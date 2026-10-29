use super::*;
use std::collections::BTreeSet;

/// Absent/null is an optional extension, including on legacy/activity/terminal
/// tasks. Only a non-null record opts into these checks. This does no I/O and
/// cannot resurrect a retired runtime while loading historical task state.
pub(crate) fn validate_queue(queue: &J) -> Result<()> {
    let Some(tasks) = queue["tasks"].as_object() else {
        return Ok(());
    };
    for (name, task) in tasks.iter().filter(|(_, t)| !t[KEY].is_null()) {
        validate_parent(queue, name, task)?;
    }
    Ok(())
}

fn validate_parent(queue: &J, name: &str, task: &J) -> Result<()> {
    ensure(
        crate::progress::lineage::is_parent(task) && task["requirements"].is_object(),
        "coordinator handoff requires a versioned parent requirement",
    )?;
    let state = &task[KEY];
    let owner = state_owner(queue, name, state)?;
    let records = state_records(state)?;
    validate_records(task, state, records, &owner)
}

fn state_owner(queue: &J, name: &str, state: &J) -> Result<Owner> {
    fields(
        state,
        &[
            "version",
            "owner",
            "next_generation",
            "active_generation",
            "records",
        ],
    )?;
    bounded(state, STATE_BYTES)?;
    let owner: Owner = serde_json::from_value(state["owner"].clone())?;
    ensure(
        state["version"] == 1 && owner.task_id == name,
        "coordinator handoff version or parent changed",
    )?;
    ensure(
        crate::lifecycle::task_owner(queue, name)?.as_ref() == Some(&owner),
        "coordinator handoff Owner changed",
    )?;
    Ok(owner)
}

fn state_records(state: &J) -> Result<&Vec<J>> {
    let records = state["records"]
        .as_array()
        .ok_or_else(|| FlowError::guard("coordinator handoff records must be an array"))?;
    ensure(
        !records.is_empty()
            && records.len() <= GENERATIONS
            && state["next_generation"] == json!(records.len() + 1),
        "coordinator handoff generation capacity or sequence changed",
    )?;
    Ok(records)
}

fn validate_records(task: &J, state: &J, records: &[J], owner: &Owner) -> Result<()> {
    let mut chain = Chain::new(&records[0])?;
    for (index, record) in records.iter().enumerate() {
        validate_record(task, record, owner, index + 1)?;
        chain.advance(record, index + 1 == records.len())?;
    }
    ensure(
        state["active_generation"] == chain.active,
        "coordinator handoff active generation changed",
    )
}

struct Chain<'a> {
    ids: BTreeSet<String>,
    runtimes: BTreeSet<String>,
    sessions: BTreeSet<String>,
    current_scope: &'a J,
    active: J,
}

impl<'a> Chain<'a> {
    fn new(first: &'a J) -> Result<Self> {
        Ok(Self {
            ids: BTreeSet::new(),
            runtimes: BTreeSet::from([required(&first["from"], "runtime")?]),
            sessions: BTreeSet::from([required(&first["from"], "session")?]),
            current_scope: &first["from"]["scope"],
            active: J::Null,
        })
    }

    fn unique(&mut self, record: &J) -> Result<()> {
        ensure(
            self.ids.insert(required(record, "request_id")?),
            "duplicate handoff request ID",
        )?;
        ensure(
            self.runtimes.insert(required(&record["to"], "runtime")?),
            "coordinator runtime reservation repeats an ancestor",
        )?;
        if !record["to"]["session"].is_null() {
            ensure(
                self.sessions.insert(required(&record["to"], "session")?),
                "coordinator session repeats an ancestor",
            )?;
        }
        Ok(())
    }

    fn advance(&mut self, record: &'a J, last: bool) -> Result<()> {
        self.unique(record)?;
        ensure(
            self.current_scope == &record["from"]["scope"],
            "coordinator predecessor chain is disconnected",
        )?;
        if !settled(record) {
            ensure(
                last,
                "unfinished coordinator handoff precedes another request",
            )?;
            self.active = record["generation"].clone();
        }
        self.current_scope = if record["phase"] == "complete" {
            &record["to"]["scope"]
        } else {
            &record["from"]["scope"]
        };
        Ok(())
    }
}

fn record_identity(record: &J, generation: usize) -> Result<()> {
    fields(
        record,
        &[
            "generation",
            "request_id",
            "input_digest",
            "reason",
            "phase",
            "from",
            "to",
            "configuration",
            "snapshot",
            "fence",
            "binding",
            "notices",
            "recovery",
            "retirement",
        ],
    )?;
    bounded(record, RECORD_BYTES)?;
    ensure(
        record["generation"] == json!(generation),
        "coordinator handoff generation changed",
    )?;
    super::super::validate_assignment(&text(record, "request_id", 128)?)?;
    text(record, "reason", 1024)?;
    digest(record, "input_digest")?;
    phase(record)?;
    Ok(())
}

fn validate_record(task: &J, record: &J, owner: &Owner, generation: usize) -> Result<()> {
    record_identity(record, generation)?;
    targets(task, record, owner)?;
    frozen_configuration(record, owner)?;
    fence(record, owner)?;
    binding(record)?;
    snapshot(record)?;
    notices(record)?;
    recovery(record, owner)?;
    retirement(record)
}

fn targets(task: &J, record: &J, owner: &Owner) -> Result<()> {
    ensure(
        record["configuration"]["cwd"] == task["environment"]["checkout"],
        "coordinator handoff checkout differs from the original environment",
    )?;
    let root = path(&task["environment"]["creation_seed"], "temp_root")?.join("rt");
    target(&record["from"], &root, owner, true)?;
    let prepared = matches!(
        record["phase"].as_str(),
        Some("prepared" | "bound" | "accepted" | "retiring" | "complete")
    );
    before_creation(record)?;
    target(&record["to"], &root, owner, prepared)?;
    ensure(
        record["from"]["runtime"] != record["to"]["runtime"],
        "coordinator successor reuses predecessor runtime",
    )?;
    if !record["to"]["session"].is_null() {
        ensure(
            record["from"]["session"] != record["to"]["session"],
            "coordinator successor reuses predecessor session",
        )?;
    }
    Ok(())
}

fn before_creation(record: &J) -> Result<()> {
    if matches!(
        record["phase"].as_str(),
        Some("requested" | "draining" | "cancelled")
    ) {
        ensure(
            record["to"]["session"].is_null() && record["to"]["scope"].is_null(),
            "coordinator creation evidence precedes its creation reservation",
        )?;
    }
    Ok(())
}

fn target(target: &J, root: &Path, owner: &Owner, required_session: bool) -> Result<()> {
    target_paths(target, root)?;
    if target["session"].is_null() {
        return ensure(
            !required_session && target["scope"].is_null(),
            "coordinator session/scope is missing",
        );
    }
    crate::identity::validate(&json!({"adapter":"codex","session":target["session"]}))?;
    target_scope(target, owner)
}

fn target_paths(target: &J, root: &Path) -> Result<()> {
    fields(target, &["runtime", "guard", "session", "scope"])?;
    let runtime = path(target, "runtime")?;
    let guard = path(target, "guard")?;
    ensure(
        runtime.parent() == Some(root),
        "coordinator runtime escapes original task namespace",
    )?;
    ensure(
        guard != runtime && guard.starts_with(&runtime),
        "coordinator guard escapes runtime",
    )
}

fn target_scope(target: &J, owner: &Owner) -> Result<()> {
    let scope = &target["scope"];
    fields(
        scope,
        &[
            "task",
            "role",
            "pane",
            "terminal",
            "socket",
            "cwd",
            "codex_runtime",
        ],
    )?;
    let native = &scope["codex_runtime"];
    fields(
        native,
        &["adapter", "session", "runtime", "instance", "cwd", "model"],
    )?;
    ensure(
        scope["task"] == owner.task_id && scope["role"] == "coordinator",
        "coordinator role or task changed",
    )?;
    native_scope(native, target, owner)
}

fn native_scope(native: &J, target: &J, owner: &Owner) -> Result<()> {
    ensure(
        native["adapter"] == "codex"
            && native["runtime"] == target["runtime"]
            && native["session"] == target["session"]
            && native["instance"] == owner.instance_id
            && native["cwd"] == target["scope"]["cwd"],
        "coordinator handoff scope identity changed",
    )
}

fn configuration_identity(config: &J) -> Result<()> {
    fields(
        config,
        &[
            "provider",
            "model",
            "effort",
            "approval",
            "sandbox",
            "cwd",
            "upstream_endpoint",
            "role_models",
            "driver",
            "business_session_configuration",
        ],
    )?;
    super::super::role_models::validate_model(&config["model"])?;
    path(config, "cwd")?;
    path(config, "driver")?;
    ensure(
        config["provider"] == config["model"]["providerID"],
        "coordinator provider changed",
    )?;
    ensure(
        config["effort"] == config["model"]["reasoning_effort"],
        "coordinator effort changed",
    )?;
    Ok(())
}

fn frozen_configuration(record: &J, owner: &Owner) -> Result<()> {
    let config = &record["configuration"];
    configuration_identity(config)?;
    business_configuration(config)?;
    for name in ["from", "to"] {
        let scope = &record[name]["scope"]["codex_runtime"];
        if !scope.is_null() {
            ensure(
                scope["cwd"] == config["cwd"]
                    && scope["model"] == config["model"]
                    && scope["instance"] == owner.instance_id,
                "coordinator target changed the frozen configuration",
            )?;
        }
    }
    Ok(())
}

fn business_configuration(config: &J) -> Result<()> {
    let original = &config["business_session_configuration"];
    ensure(
        original.is_object()
            && original["approvalPolicy"] == config["approval"]
            && original["sandbox"] == config["sandbox"]
            && original["cwd"] == config["cwd"]
            && original["model"] == config["model"]["id"]
            && original["modelProvider"] == config["provider"],
        "coordinator original business configuration changed",
    )?;
    Ok(())
}

fn fence(record: &J, owner: &Owner) -> Result<()> {
    let fence = &record["fence"];
    fields(
        fence,
        &["generation", "owner", "old_runtime", "new_runtime"],
    )?;
    ensure(
        fence["generation"] == record["generation"]
            && fence["owner"] == json!(owner)
            && fence["old_runtime"] == record["from"]["runtime"]
            && fence["new_runtime"] == record["to"]["runtime"],
        "coordinator handoff fence identity changed",
    )
}

fn binding(record: &J) -> Result<()> {
    let binding = &record["binding"];
    let bound = matches!(
        record["phase"].as_str(),
        Some("bound" | "accepted" | "retiring" | "complete")
    );
    if !bound {
        return ensure(binding.is_null(), "uncommitted coordinator has a binding");
    }
    fields(
        binding,
        &["transaction_id", "from_scope", "to_scope", "committed_at"],
    )?;
    super::super::validate_assignment(&text(binding, "transaction_id", 128)?)?;
    ensure(
        binding["from_scope"] == record["from"]["scope"]
            && binding["to_scope"] == record["to"]["scope"]
            && binding["committed_at"].is_number(),
        "coordinator scope transaction changed",
    )
}

fn snapshot(record: &J) -> Result<()> {
    let snapshot = &record["snapshot"];
    if snapshot.is_null() {
        return ensure(
            matches!(
                record["phase"].as_str(),
                Some("requested" | "draining" | "cancelled")
            ),
            "coordinator handoff snapshot missing",
        );
    }
    fields(snapshot, &["path", "sha256", "bytes"])?;
    let file = path(snapshot, "path")?;
    ensure(
        file.starts_with(path(&record["from"], "runtime")?),
        "handoff snapshot escapes original runtime",
    )?;
    digest(snapshot, "sha256")?;
    ensure(
        snapshot["bytes"]
            .as_u64()
            .is_some_and(|n| n > 0 && n <= 16384),
        "coordinator snapshot size is invalid",
    )
}

fn notices(record: &J) -> Result<()> {
    fields(&record["notices"], &["accept", "ready"])?;
    for kind in ["accept", "ready"] {
        notice_identity(&record["notices"][kind])?;
    }
    notice_stages(record)?;
    if matches!(
        record["phase"].as_str(),
        Some("accepted" | "retiring" | "complete")
    ) {
        ensure(
            record["notices"]["accept"]["phase"] == "acknowledged",
            "coordinator handoff has no exact acceptance ACK",
        )?;
    }
    Ok(())
}

fn notice_stages(record: &J) -> Result<()> {
    let accept = notice_phase(&record["notices"]["accept"])?;
    let bound = !record["binding"].is_null();
    ensure(
        (accept > 0) == bound,
        "coordinator acceptance notice is not bound to its scope transaction",
    )?;
    if record["phase"] != "complete" {
        ensure(
            record["notices"]["ready"]["phase"] == "pending",
            "coordinator ready notice precedes retirement completion",
        )?;
    }
    Ok(())
}

fn notice_identity(notice: &J) -> Result<()> {
    fields(notice, &["wait", "event", "delivery", "turn", "phase"])?;
    super::super::validate_assignment(&text(notice, "wait", 128)?)?;
    for key in ["event", "delivery", "turn"] {
        if !notice[key].is_null() {
            text(notice, key, 128)?;
        }
    }
    notice_evidence(notice)
}

fn notice_evidence(notice: &J) -> Result<()> {
    let phase = notice_phase(notice)?;
    if phase == 0 {
        return pending_notice(notice);
    }
    if phase >= 1 {
        text(notice, "event", 128)?;
    }
    if phase >= 2 {
        text(notice, "delivery", 128)?;
    }
    if phase >= 3 {
        text(notice, "turn", 128)?;
    }
    Ok(())
}

fn pending_notice(notice: &J) -> Result<()> {
    ensure(
        ["event", "delivery", "turn"]
            .iter()
            .all(|key| notice[*key].is_null()),
        "pending coordinator notice already contains a delivery effect",
    )
}

pub(super) fn notice_phase(notice: &J) -> Result<usize> {
    [
        "pending",
        "registered",
        "sending",
        "submitted",
        "acknowledged",
    ]
    .iter()
    .position(|phase| notice["phase"] == *phase)
    .ok_or_else(|| FlowError::guard("invalid coordinator notice phase"))
}

fn recovery(record: &J, owner: &Owner) -> Result<()> {
    let recovery = &record["recovery"];
    if recovery.is_null() {
        return Ok(());
    }
    fields(
        recovery,
        &[
            "observer",
            "origin_coordinator",
            "from_target",
            "to_target",
            "phase",
        ],
    )?;
    ensure(
        path(recovery, "observer")?
            == path(recovery, "origin_coordinator")?.join("recovery-observer"),
        "original recovery observer location changed",
    )?;
    recovery_target(&recovery["from_target"], &record["from"], owner)?;
    recovery_target(&recovery["to_target"], &record["to"], owner)?;
    ensure(
        matches!(recovery["phase"].as_str(), Some("pending" | "bound")),
        "invalid recovery binding phase",
    )
}

fn recovery_target(target: &J, receiver: &J, owner: &Owner) -> Result<()> {
    fields(
        target,
        &["generation", "runtime", "scope", "transaction_id"],
    )?;
    ensure(
        target["runtime"] == receiver["runtime"] && target["scope"] == receiver["scope"],
        "recovery target changed",
    )?;
    if !target["scope"].is_null() {
        ensure(
            target["scope"]["codex_runtime"]["instance"] == owner.instance_id,
            "recovery target Owner changed",
        )?;
    }
    Ok(())
}

fn retirement(record: &J) -> Result<()> {
    let plan = &record["retirement"];
    if plan.is_null() {
        return ensure(
            !matches!(record["phase"].as_str(), Some("retiring" | "complete")),
            "coordinator retirement plan missing",
        );
    }
    fields(plan, &["path", "sha256", "phase"])?;
    ensure(
        path(plan, "path")?.starts_with(path(&record["from"], "runtime")?),
        "coordinator retirement plan escapes original runtime",
    )?;
    digest(plan, "sha256")?;
    ensure(
        matches!(
            plan["phase"].as_str(),
            Some("planned" | "retiring" | "sessions" | "complete")
        ),
        "invalid coordinator retirement phase",
    )?;
    ensure(
        record["phase"] != "complete" || plan["phase"] == "complete",
        "coordinator resources are not retired",
    )
}
