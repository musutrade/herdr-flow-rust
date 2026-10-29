//! The queue is authority. This original-runtime mirror can only be repaired
//! from an exact, already committed request prefix under the same Owner EX.
use super::*;

const FILE: &str = "receiver-handoff.json";

pub(super) fn unregistered(root: &Path) -> Result<()> {
    // Ordinary no-handoff work must not depend on opening a nonexistent
    // namespace. Any reserved entry, including a dangling link, still fences.
    native::receiver::ensure_absent(&root.join(FILE))
}

fn expected(state: &J) -> Result<J> {
    let record = state["records"]
        .as_array()
        .and_then(|r| r.last())
        .ok_or_else(|| FlowError::guard("handoff mirror has no original request"))?;
    let value = json!({"version":1,"owner":state["owner"],"generation":record["generation"],
        "request_id":record["request_id"],"phase":record["phase"],"fence":record["fence"],
        "history_sha256":util::pyjson::sha256_json_compact(state)});
    bounded(&value, RECORD_BYTES)?;
    Ok(value)
}

fn protected(root: &Path, owner: &J) -> Result<()> {
    ensure(
        util::protected::is_runtime_namespace(root)?,
        "handoff mirror runtime is not protected",
    )?;
    ensure(
        util::read_state_json(root, "lifecycle-owner.json")? == *owner,
        "handoff mirror runtime Owner changed",
    )
}

pub(super) fn cancelled(task: &J) -> Result<()> {
    let state = &task[KEY];
    let record = state["records"]
        .as_array()
        .and_then(|r| r.last())
        .ok_or_else(|| FlowError::guard("cancelled handoff history missing"))?;
    let root = path(&record["from"], "runtime")?;
    protected(&root, &state["owner"])?;
    ensure(
        util::read_state_json_bounded_if_exists(&root, FILE, RECORD_BYTES)?
            == Some(expected(state)?),
        "handoff cancellation mirror is pending; retry the original request/cancel before new work",
    )
}

pub(super) fn reconcile(root: &Path, permit: &crate::lifecycle::Permit, state: &J) -> Result<()> {
    let saved = current(root, permit, state)?;
    if state.is_null() {
        return ensure(
            saved.is_none(),
            "unregistered handoff mirror cannot be adopted",
        );
    }
    target(root, state)?;
    write(root, state, saved.as_ref())
}

fn current(root: &Path, permit: &crate::lifecycle::Permit, state: &J) -> Result<Option<J>> {
    crate::lifecycle::check_runtime_ex(root, permit)?;
    let owner = permit.owner();
    let queue = crate::queue::load_queue(&owner.queue_root)?;
    ensure(
        queue["tasks"][&owner.task_id][KEY] == *state,
        "handoff authority changed before mirror reconciliation",
    )?;
    util::read_state_json_bounded_if_exists(root, FILE, RECORD_BYTES)
}

fn target(root: &Path, state: &J) -> Result<()> {
    let record = state["records"].as_array().unwrap().last().unwrap();
    ensure(
        record["from"]["runtime"] == json!(root),
        "handoff mirror is not the original coordinator",
    )?;
    phase_count(record).map(|_| ())
}

fn write(root: &Path, state: &J, saved: Option<&J>) -> Result<()> {
    let next = expected(state)?;
    if let Some(saved) = saved {
        ensure(
            compatible(state, saved)?,
            "handoff mirror does not match a committed request prefix",
        )?;
        if saved == &next {
            return Ok(());
        }
    }
    util::atomic_write_json(&root.join(FILE), &next)
}

fn phase_count(record: &J) -> Result<usize> {
    match record["phase"].as_str() {
        Some("requested") => Ok(1),
        Some("draining") => Ok(2),
        Some("cancelled") => Ok(3),
        _ => Err(FlowError::guard(
            "creation/continuation mirror driver is not implemented",
        )),
    }
}

fn compatible(state: &J, saved: &J) -> Result<bool> {
    let records = state["records"].as_array().unwrap();
    for (index, record) in records.iter().enumerate() {
        let mut prefix = state.clone();
        prefix["records"]
            .as_array_mut()
            .unwrap()
            .truncate(index + 1);
        prefix["next_generation"] = json!(index + 2);
        for phase in ["requested", "draining", "cancelled"]
            .iter()
            .take(phase_count(record)?)
        {
            prefix["records"][index]["phase"] = json!(phase);
            prefix["active_generation"] = if *phase == "cancelled" {
                J::Null
            } else {
                json!(index + 1)
            };
            if saved == &expected(&prefix)? {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absent_extension_needs_no_namespace_but_orphan_entries_block() {
        let temp = util::fs::test_tempdir();
        let root = temp.path().join("not-created");
        let mut state = super::super::tests::queue();
        state["tasks"]["parent"][KEY] = J::Null;
        state["receiver"]["codex_runtime"]["runtime"] = json!(root);
        let checkout = Path::new("/workspace/checkout");
        check_workspace(&state, checkout).unwrap();
        assert!(!root.exists());
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join(FILE), "unknown reservation").unwrap();
        assert!(check_workspace(&state, checkout).is_err());
        check_workspace(&state, Path::new("/independent/checkout")).unwrap();
        std::fs::remove_file(root.join(FILE)).unwrap();
        std::os::unix::fs::symlink(root.join("missing"), root.join(FILE)).unwrap();
        assert!(check_workspace(&state, checkout).is_err());
    }
}
