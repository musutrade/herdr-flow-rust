//! Queue-side lifecycle permits and protected layout. Legacy queues retain their contract.
use super::*;
use crate::lifecycle::{self, Owner, Permit};

pub fn isolated(root: &Path) -> Result<bool> {
    let document = root.join("queue.json");
    match std::fs::symlink_metadata(&document) {
        Ok(meta) if meta.is_dir() => {
            required_isolated_layout(&document)?;
            Ok(true)
        }
        Ok(meta) if meta.file_type().is_symlink() => {
            Err(queue_error("queue authority cannot be a symlink"))
        }
        Ok(_) => Ok(false),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e.into()),
    }
}

fn required_isolated_layout(document: &Path) -> Result<()> {
    if !util::protected::is_document(document)? {
        return Err(queue_error("isolated queue document marker is missing"));
    }
    require_namespace(document)?;
    require_namespace(&document.join("results"))?;
    require_namespace(&document.join("deliveries"))
}

fn require_namespace(path: &Path) -> Result<()> {
    if !util::protected::is_namespace(path)? {
        return Err(queue_error(
            "isolated queue namespace is missing; refusing fallback or recreation",
        ));
    }
    Ok(())
}

pub fn require_legacy_execution(root: &Path) -> Result<()> {
    isolated(root).and_then(legacy_execution_allowed)
}

fn legacy_execution_allowed(isolated: bool) -> Result<()> {
    if isolated {
        return Err(queue_error(
            "isolated execution requires the task-owned runtime spawn path",
        ));
    }
    Ok(())
}

pub fn artifacts(root: &Path) -> Result<PathBuf> {
    if isolated(root)? {
        Ok(root.join("queue.json"))
    } else {
        Ok(root.to_path_buf())
    }
}

pub fn validate_layout(root: &Path, s: &J) -> Result<()> {
    if isolated(root)? {
        if s["lifecycle"]["queue_root"] != json!(util::resolve_path(root))
            || s["lifecycle"]["format"] != "isolated-v1"
        {
            return Err(queue_error("isolated authority root/format changed"));
        }
    } else if !s["lifecycle"].is_null() {
        return Err(queue_error(
            "isolated authority lost its protected document",
        ));
    }
    Ok(())
}

pub fn permit(s: &J, name: &str, exclusive: bool, allow_terminal: bool) -> Result<Option<Permit>> {
    if s["lifecycle"].is_null() {
        return Ok(None);
    }
    let owner = lifecycle::task_owner(s, name)?
        .ok_or_else(|| queue_error("isolated task has no bound lifecycle owner"))?;
    let permit = if exclusive {
        lifecycle::acquire(&owner, true)?
    } else {
        lifecycle::write_permit(&owner)?
    };
    permit.check(s, allow_terminal)?;
    Ok(Some(permit))
}

pub fn closed(s: &J, name: &str) -> bool {
    progress::terminal::ensure_open(s, name).is_err()
}

pub fn task_permit(s: &J, action: &str, spec: &J) -> Result<Option<Permit>> {
    if action == "task-add" {
        progress::claim::isolated_admission(
            s,
            spec["task"].as_str().unwrap_or(""),
            &spec["claim"],
        )?;
    }
    if s["lifecycle"].is_null() || bootstrap(s, action, spec)? {
        return Ok(None);
    }
    let name = spec["task"]
        .as_str()
        .ok_or_else(|| queue_error("task required"))?;
    permit(s, name, identity_change(action, spec), false)
}

fn identity_change(action: &str, spec: &J) -> bool {
    action == "task-resume" || (action == "task-lineage" && spec["kind"] == "migrate")
}

fn bootstrap(s: &J, action: &str, spec: &J) -> Result<bool> {
    if action == "task-environment" || terminal_action(s, action, spec) {
        return Ok(true);
    }
    unbound_bootstrap(s, action, spec)
}

fn unbound_bootstrap(s: &J, action: &str, spec: &J) -> Result<bool> {
    let name = spec["task"].as_str().unwrap_or_default();
    if lifecycle::task_owner(s, name)?.is_some() {
        return Ok(false);
    }
    bootstrap_action(action, spec)
}

fn bootstrap_action(action: &str, spec: &J) -> Result<bool> {
    let allowed = action == "task-add"
        || (action == "task-lineage"
            && matches!(
                spec["kind"].as_str(),
                Some("plan" | "artifact" | "candidate")
            ));
    if !allowed {
        return Ok(false);
    }
    if !spec["workspace"].is_null() {
        return Err(queue_error(
            "bind an isolated environment before reserving a writer",
        ));
    }
    Ok(true)
}

pub fn terminal_action(s: &J, action: &str, spec: &J) -> bool {
    action == "task-finalize"
        || (!s["lifecycle"].is_null()
            && action == "task-control"
            && spec["control"] == "cancelled"
            && s["tasks"][spec["task"].as_str().unwrap_or_default()]["lineage"]["requirement"]
                .is_null())
}

pub fn finalize_control(root: &Path, s: &mut J, spec: &J, now: f64) -> Result<()> {
    let name = spec["task"].as_str().unwrap_or_default();
    let owner = lifecycle::task_owner(s, name)?
        .ok_or_else(|| queue_error("cancelled owner has no instance"))?;
    let mut terminal = spec.clone();
    terminal["queue_id"] = json!(owner.queue_id);
    terminal["epoch"] = json!(owner.epoch);
    terminal["instance_id"] = json!(owner.instance_id);
    terminal["outcome"] = json!("failure");
    finalize(root, s, &terminal, now)
}

pub fn validate_save(root: &Path, s: &J) -> Result<()> {
    if s["lifecycle"].is_null() {
        return Ok(());
    }
    let current = super::load_queue(root)?;
    validate_version(&current, s)?;
    if current["id"] != s["id"] || current["lifecycle"] != s["lifecycle"] {
        return Err(queue_error("isolated queue authority identity changed"));
    }
    if let Some(tasks) = current["tasks"].as_object() {
        for (_name, old) in tasks {
            validate_task_save(old, &s["tasks"][_name])?;
        }
    }
    crate::progress::environment::validate_new_seed_reservations(&current, s)
}

pub(super) fn validate_version(current: &J, next: &J) -> Result<()> {
    if current["authority_version"].as_u64().is_none()
        || current["authority_version"] != next["authority_version"]
    {
        return Err(queue_error("isolated queue snapshot is obsolete"));
    }
    Ok(())
}

pub(super) fn advance_version(s: &mut J) -> Result<()> {
    if s["lifecycle"].is_null() {
        return Ok(());
    }
    let version = s["authority_version"]
        .as_u64()
        .and_then(|v| v.checked_add(1))
        .ok_or_else(|| queue_error("invalid queue authority version"))?;
    s["authority_version"] = json!(version);
    Ok(())
}

fn validate_task_save(old: &J, next: &J) -> Result<()> {
    if (!old["final_outcome"].is_null() || !old["cleanup_obligation"].is_null()) && old != next {
        return Err(queue_error(
            "terminal task is immutable until cleanup implementation",
        ));
    }
    if old["environment"].is_null() {
        return Ok(());
    }
    let a = &old["environment"];
    let b = &next["environment"];
    validate_environment_identity(a, b)?;
    validate_bootstrap_checkpoint(a, b)?;
    if old["epoch"] != next["epoch"]
        || old["lineage"]["requirement"] != next["lineage"]["requirement"]
    {
        return Err(queue_error("bound lifecycle ownership cannot be replaced"));
    }
    Ok(())
}

fn validate_environment_identity(a: &J, b: &J) -> Result<()> {
    for key in [
        "queue_id",
        "task_id",
        "epoch",
        "instance_id",
        "state_file",
        "checkout",
        "creation_seed",
    ] {
        if a[key] != b[key] {
            return Err(queue_error(
                "bound instance identity cannot be replaced or removed",
            ));
        }
    }
    Ok(())
}

fn validate_bootstrap_checkpoint(a: &J, b: &J) -> Result<()> {
    if !a["bootstrap"].is_null() {
        if a["bootstrap"] != b["bootstrap"] {
            return Err(queue_error(
                "initialized environment bootstrap cannot be replaced or removed",
            ));
        }
        return Ok(());
    }
    if b["bootstrap"].is_null() {
        return Ok(());
    }
    let mut allocation = a["creation_seed"]["creation"]["allocation"].clone();
    if !allocation.is_object() {
        return Err(queue_error("environment bootstrap has no allocation seed"));
    }
    allocation["envelope"] = b["bootstrap"].clone();
    let _: crate::task_env::instance::allocation::Allocation = serde_json::from_value(allocation)?;
    if b["bootstrap"]["path"] != b["state_file"] {
        return Err(queue_error(
            "environment bootstrap path differs from its reserved document",
        ));
    }
    Ok(())
}

pub fn wait_permit(s: &J, w: &J) -> Result<Option<Permit>> {
    if !s["lifecycle"].is_null()
        && s["tasks"][w["task"].as_str().unwrap_or_default()]["epoch"] != w["epoch"]
    {
        return Err(queue_error("wait belongs to an obsolete task epoch"));
    }
    permit(s, w["task"].as_str().unwrap_or_default(), false, false)
}

pub fn registration_permit(s: &J, spec: &J) -> Result<Option<Permit>> {
    if s["lifecycle"].is_null() {
        return Ok(None);
    }
    bound_registration_permit(s, spec)
}

fn bound_registration_permit(s: &J, spec: &J) -> Result<Option<Permit>> {
    let name = spec["task"].as_str().unwrap_or_default();
    progress::terminal::ensure_open(s, name)?;
    if lifecycle::task_owner(s, name)?.is_none() {
        return Ok(None);
    }
    permit(s, name, false, false)
}

pub fn creation_permit(
    root: &Path,
    s: &J,
    spec: &J,
    binding: &crate::task_env::instance::InstanceBinding,
) -> Result<Option<std::sync::Arc<Permit>>> {
    if s["lifecycle"].is_null() {
        return Ok(None);
    }
    progress::terminal::ensure_open(s, &binding.task_id)?;
    if !s["tasks"][&binding.task_id]["lineage"]["requirement"].is_null() {
        return Err(queue_error(
            "create the explicit lifecycle owner environment, not an activity root",
        ));
    }
    let owner = Owner {
        queue_root: util::resolve_path(root),
        queue_id: binding.queue_id.clone(),
        task_id: binding.task_id.clone(),
        epoch: binding.epoch,
        instance_id: spec["instance_id"].as_str().unwrap_or_default().to_string(),
    };
    Ok(Some(std::sync::Arc::new(lifecycle::acquire(&owner, true)?)))
}

pub fn git_owner_candidate(task: &J, commit: &str) -> Result<()> {
    if task["schema_version"] != 2 {
        return Ok(());
    }
    let artifact = &task["artifacts"][commit]["object"];
    if artifact["adapter"] != "git" || artifact["id"] != commit {
        return Err(queue_error(
            "isolated V2 owner requires an explicitly selected Git artifact",
        ));
    }
    crate::progress::artifact::ArtifactAdapter::verify(
        &crate::progress::artifact::LocalArtifacts,
        artifact,
    )
}

pub fn delivery_permits(s: &J, id: &str) -> Result<Vec<Permit>> {
    let mut permits = Vec::new();
    if s["lifecycle"].is_null() {
        return Ok(permits);
    }
    let delivery = &s["deliveries"][id];
    let events = delivery["events"]
        .as_array()
        .ok_or_else(|| queue_error("delivery events missing"))?;
    let mut seen = BTreeSet::new();
    for id in events {
        let name = s["events"][id.as_str().unwrap_or_default()]["task"]
            .as_str()
            .unwrap_or_default();
        if seen.insert(name.to_string()) {
            if let Some(permit) = permit(s, name, false, false)? {
                permits.push(permit);
            }
        }
    }
    Ok(permits)
}

pub(super) fn delivery_closing(s: &J, delivery: &J) -> bool {
    if s["lifecycle"].is_null() {
        return false;
    }
    delivery["events"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|id| {
            let name = s["events"][id.as_str().unwrap_or("")]["task"]
                .as_str()
                .unwrap_or("");
            closed(s, name)
        })
}

pub fn active_permits(s: &J) -> Result<Vec<Permit>> {
    let mut permits = Vec::new();
    if s["lifecycle"].is_null() {
        return Ok(permits);
    }
    collect_active_permits(s, &mut permits)?;
    Ok(permits)
}

fn collect_active_permits(s: &J, permits: &mut Vec<Permit>) -> Result<()> {
    let mut seen = BTreeSet::new();
    for name in s["tasks"]
        .as_object()
        .into_iter()
        .flat_map(|tasks| tasks.keys())
    {
        collect_active_permit(s, name, &mut seen, permits)?;
    }
    Ok(())
}

fn collect_active_permit(
    s: &J,
    name: &str,
    seen: &mut BTreeSet<String>,
    permits: &mut Vec<Permit>,
) -> Result<()> {
    match active_owner(s, name)? {
        Some(owner) => retain_owner_permit(s, &owner, seen, permits),
        None => Ok(()),
    }
}

fn active_owner(s: &J, name: &str) -> Result<Option<Owner>> {
    if closed(s, name) {
        return Ok(None);
    }
    lifecycle::task_owner(s, name)
}

fn retain_owner_permit(
    s: &J,
    owner: &Owner,
    seen: &mut BTreeSet<String>,
    permits: &mut Vec<Permit>,
) -> Result<()> {
    if !seen.insert(owner.task_id.clone()) {
        return Ok(());
    }
    permits.push(checked_shared_permit(s, owner)?);
    Ok(())
}

fn checked_shared_permit(s: &J, owner: &Owner) -> Result<Permit> {
    let permit = lifecycle::acquire(owner, false)?;
    permit.check(s, false)?;
    Ok(permit)
}

pub fn requested_owner(root: &Path, s: &J, spec: &J) -> Result<Owner> {
    let name = progress::text(spec.get("task"), "task", 128)?;
    let owner = lifecycle::task_owner(s, &name)?
        .ok_or_else(|| queue_error("task has no isolated instance"))?;
    if owner.task_id != name
        || owner.queue_root != util::resolve_path(root)
        || s["id"] != spec["queue_id"]
        || json!(owner.epoch) != spec["epoch"]
        || json!(owner.instance_id) != spec["instance_id"]
    {
        return Err(queue_error("terminal owner identity changed"));
    }
    Ok(owner)
}

pub fn finalize(root: &Path, s: &mut J, spec: &J, now: f64) -> Result<()> {
    let owner = requested_owner(root, s, spec)?;
    let permit = lifecycle::acquire(&owner, true)?;
    permit.check(s, true)?;
    let env = crate::task_env::restore_for_terminal(&owner, &permit)?;
    progress::terminal::record(s, spec, &owner, &env, now)?;
    super::save(root, s)
}

pub fn producer_marker(s: &J, w: &J) -> Result<()> {
    if s["lifecycle"].is_null() {
        return Ok(());
    }
    let name = w["task"].as_str().unwrap_or_default();
    let owner = lifecycle::task_owner(s, name)?
        .ok_or_else(|| queue_error("producer has no bound instance"))?;
    for (key, expected) in [
        (crate::task_env::MARKER_ENV, owner.instance_id),
        ("HERDR_TASK_ID", owner.task_id),
        ("HERDR_TASK_QUEUE", owner.queue_id),
        ("HERDR_TASK_EPOCH", owner.epoch.to_string()),
    ] {
        if std::env::var(key).ok().as_deref() != Some(expected.as_str()) {
            return Err(queue_error("producer instance marker changed"));
        }
    }
    Ok(())
}

pub fn owner_value(s: &J, name: &str) -> Result<J> {
    if s["lifecycle"].is_null() {
        return Ok(J::Null);
    }
    let owner =
        lifecycle::task_owner(s, name)?.ok_or_else(|| queue_error("task has no isolated owner"))?;
    Ok(serde_json::to_value(owner)?)
}

pub fn result_owner(s: &J, w: &J, result: &J) -> Result<()> {
    if s["lifecycle"].is_null() {
        return Ok(());
    }
    let expected = owner_value(s, w["task"].as_str().unwrap_or_default())?;
    if result["lifecycle_owner"] != expected {
        return Err(queue_error("producer result lifecycle owner changed"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bootstrap_only_admits_explicit_prebinding_registration() {
        let state = json!({"lifecycle":{"queue_root":"/fixture"},"tasks":{}});
        assert!(registration_permit(&state, &json!({"task":"t"}))
            .unwrap()
            .is_none());
        assert!(active_permits(&state).unwrap().is_empty());
        for kind in ["plan", "artifact", "candidate"] {
            assert!(bootstrap(&state, "task-lineage", &json!({"task":"t","kind":kind})).unwrap());
        }
        assert!(bootstrap(&state, "task-add", &json!({"task":"t"})).unwrap());
        assert!(bootstrap(
            &state,
            "task-add",
            &json!({"task":"t","workspace":"/shared"})
        )
        .is_err());
        assert!(!bootstrap(
            &state,
            "task-lineage",
            &json!({"task":"t","kind":"activity"})
        )
        .unwrap());
        assert!(!bootstrap(&state, "task-begin", &json!({"task":"t"})).unwrap());
        assert!(bootstrap(&state, "task-environment", &json!({"task":"t"})).unwrap());
        assert!(bootstrap(&state, "task-finalize", &json!({"task":"t"})).unwrap());
    }
}
