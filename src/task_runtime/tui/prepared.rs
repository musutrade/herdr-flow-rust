//! Explicit fresh-Recovery display intent, stored in its existing creation
//! journal. A missing/older proof is never inferred from absent display files.
use super::*;

const LIMIT: usize = 1024 * 1024;

pub(super) fn request(root: &Path, runtime: &J) -> Result<()> {
    let _journal = super::super::upstream::session_lock(root)?;
    let mut journal = util::read_state_json_bounded(root, "upstream-session.json", LIMIT)?;
    let Some(intent) = journal.get("presentation_intent") else {
        return Ok(());
    };
    valid_journal(&journal, runtime)?;
    ensure(
        intent["version"] == 1
            && matches!(
                intent["phase"].as_str(),
                Some("not_requested" | "requested")
            ),
        "presentation intent is unknown",
    )?;
    if intent["phase"] == "not_requested" {
        journal["presentation_intent"]["phase"] = json!("requested");
        util::atomic_write_json(&root.join("upstream-session.json"), &journal)?;
    }
    Ok(())
}

fn valid_journal(journal: &J, runtime: &J) -> Result<()> {
    ensure(
        journal["phase"] == "created"
            && journal["instance"] == runtime["instance"]
            && journal["runtime"] == *runtime
            && journal["session"].is_string(),
        "prepared presentation creation binding changed",
    )
}

pub(super) fn human_proof(
    env: &TaskEnvironment,
    root: &Path,
    coordinator: &Path,
    runtime: &J,
) -> Result<()> {
    // Only human census uses this proof, under its exclusive Owner lifecycle
    // permit. Display/incident writers require the shared permit. Do not create
    // a presentation lock here: even an existing lock is attempt evidence.
    ensure(
        runtime["recovery_role"] == true && upstream::selected(runtime),
        "configured presentation has no settled binding",
    )?;
    let journal = util::read_state_json_bounded(root, "upstream-session.json", LIMIT)?;
    valid_journal(&journal, runtime)?;
    ensure(
        journal["presentation_intent"] == json!({"version":1,"phase":"not_requested"}),
        "presentation was requested or its preparation proof is missing",
    )?;
    ready_agent(env, root, coordinator, runtime, &journal["session"])?;
    no_attempt(root)?;
    let observer = coordinator.join("recovery-observer");
    ensure(
        util::read_state_json_bounded_if_exists(&observer, "incident.json", LIMIT)?.is_none(),
        "recovery already has an incident; retain its presentation evidence",
    )
}

fn ready_agent(
    env: &TaskEnvironment,
    root: &Path,
    coordinator: &Path,
    runtime: &J,
    session: &J,
) -> Result<()> {
    let observer = coordinator.join("recovery-observer");
    let owner = json!(crate::task_env::isolation::owner(env));
    ensure(
        util::read_state_json(&observer, "lifecycle-owner.json")? == owner,
        "prepared Recovery observer Owner changed",
    )?;
    let context = util::read_state_json_bounded(&observer, "observer.json", LIMIT)?;
    ready_context(&context, coordinator, &observer, &owner, runtime)?;
    let agent = util::read_state_json_bounded(&observer, "agent.json", LIMIT)?;
    ensure(
        agent["phase"] == "ready"
            && agent["runtime"] == json!(root)
            && agent["session"] == *session
            && agent["model"] == runtime["model"],
        "prepared Recovery agent binding changed",
    )
}

fn ready_context(
    context: &J,
    coordinator: &Path,
    observer: &Path,
    owner: &J,
    runtime: &J,
) -> Result<()> {
    ensure(
        context["owner"] == *owner
            && context["coordinator"] == json!(coordinator)
            && context["observer"] == json!(observer)
            && context["runtime"]["native_tui"] == runtime["native_tui"],
        "prepared Recovery context changed",
    )
}

fn no_attempt(root: &Path) -> Result<()> {
    for (index, entry) in std::fs::read_dir(root)?.enumerate() {
        ensure(index < 256, "prepared presentation inventory exceeds bound")?;
        let name = entry?.file_name();
        let name = name.to_string_lossy();
        ensure(
            !attempt_name(&name),
            "prepared presentation has existing attempt evidence",
        )?;
    }
    Ok(())
}
fn attempt_name(name: &str) -> bool {
    name.starts_with("tui-")
        || matches!(
            name,
            "presentation.json"
                | "presentation.lock"
                | "broker-turn.json"
                | "turn.json"
                | "receiver-start.json"
                | "native-client.json"
                | "native-requests.json"
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn journal(root: &Path, runtime: &J) -> J {
        let value = json!({"phase":"created","instance":runtime["instance"],
            "runtime":runtime,"session":"recorded-session",
            "presentation_intent":{"version":1,"phase":"not_requested"}});
        util::atomic_write_json(&root.join("upstream-session.json"), &value).unwrap();
        value
    }

    #[test]
    fn display_attempt_is_monotonic_before_binding_or_external_actions() {
        let temp = util::fs::test_tempdir();
        let root = temp.path();
        let runtime = json!({"instance":"fixture"});
        journal(root, &runtime);
        // The malformed binding fails before any Herdr/socket action. The
        // attempted display must nevertheless permanently consume the proof.
        util::atomic_write_json(&root.join("presentation.json"), &json!({})).unwrap();
        assert!(super::super::open_bound(root, &runtime).is_err());
        let first = util::read_state_json(root, "upstream-session.json").unwrap();
        assert_eq!(first["presentation_intent"]["phase"], "requested");
        assert!(super::super::open_bound(root, &runtime).is_err());
        assert_eq!(
            util::read_state_json(root, "upstream-session.json").unwrap(),
            first
        );
    }

    #[test]
    fn ordinary_and_publication_journals_without_intent_remain_compatible() {
        let temp = util::fs::test_tempdir();
        for runtime in [
            json!({"instance":"ordinary"}),
            json!({"instance":"publisher","publication_role":true}),
        ] {
            let mut value = journal(temp.path(), &runtime);
            value.as_object_mut().unwrap().remove("presentation_intent");
            util::atomic_write_json(&temp.path().join("upstream-session.json"), &value).unwrap();
            request(temp.path(), &runtime).unwrap();
            assert_eq!(
                util::read_state_json(temp.path(), "upstream-session.json").unwrap(),
                value
            );
        }
    }

    #[test]
    fn unknown_display_proof_never_mutates_or_becomes_fresh() {
        let temp = util::fs::test_tempdir();
        let runtime = json!({"instance":"fixture"});
        let original = journal(temp.path(), &runtime);
        for intent in [
            J::Null,
            json!({"version":2,"phase":"not_requested"}),
            json!({"version":1,"phase":"unknown"}),
        ] {
            let mut changed = original.clone();
            changed["presentation_intent"] = intent;
            util::atomic_write_json(&temp.path().join("upstream-session.json"), &changed).unwrap();
            assert!(request(temp.path(), &runtime).is_err());
            assert_eq!(
                util::read_state_json(temp.path(), "upstream-session.json").unwrap(),
                changed
            );
        }
    }
}
