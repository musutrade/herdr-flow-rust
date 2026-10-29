//! Rejoin the recorded thread on each new broker connection before business RPC.
//! Metadata reads alone do not subscribe; no turn or tool request is replayed.
use super::*;
use crate::task_runtime::native::DirectClient;

const REQUIRED: [&str; 5] = ["model", "modelProvider", "cwd", "approvalPolicy", "sandbox"];

pub(in crate::task_runtime) fn configuration(result: &J) -> J {
    let mut config = serde_json::Map::new();
    for key in REQUIRED.into_iter().chain([
        "reasoningEffort",
        "approvalsReviewer",
        "activePermissionProfile",
        "runtimeWorkspaceRoots",
        "collaborationMode",
        "multiAgentMode",
        "disabledPluginIds",
    ]) {
        if let Some(value) = result.get(key).filter(|value| !value.is_null()) {
            config.insert(key.to_owned(), value.clone());
        }
    }
    J::Object(config)
}

fn comparable(config: &J) -> J {
    let mut config = config.clone();
    let cwd = config["cwd"].clone();
    if let Some(roots) = config["sandbox"]["writableRoots"].as_array_mut() {
        // Codex always includes cwd implicitly and omits that redundant root
        // from its effective policy response. Other roots remain exact.
        roots.retain(|root| root != &cwd);
    }
    config
}

fn verifiable(journal: &J) -> bool {
    REQUIRED
        .iter()
        .all(|key| !journal["session_configuration"][key].is_null())
}

fn readonly(method: &str) -> bool {
    matches!(method, "thread/read" | "thread/turns/list")
}

pub(super) fn validate_turn(runtime: &J, params: &J) -> Result<()> {
    for (field, expected) in [
        ("model", &runtime["model"]["id"]),
        ("effort", &runtime["model"]["reasoning_effort"]),
        ("cwd", &runtime["cwd"]),
    ] {
        if let Some(value) = params.get(field).filter(|value| !value.is_null()) {
            ensure(
                value == expected,
                "turn changed the task's fixed role configuration",
            )?;
        }
    }
    ensure(
        params["collaborationMode"].is_null(),
        "task broker does not permit collaboration mode overrides",
    )
}

pub(in crate::task_runtime) fn prepare(
    root: &Path,
    client: &mut DirectClient,
    method: &str,
) -> Result<()> {
    let runtime = util::read_state_json(root, "runtime.json")?;
    if !selected(&runtime) || method == "thread/start" {
        return Ok(());
    }
    let journal = util::read_state_json(root, "upstream-session.json")?;
    if client.subscribed_session.as_deref() == journal["session"].as_str() {
        return Ok(());
    }
    if !automatic_resume(&journal, method)? {
        return Ok(());
    }
    rejoin(client, &runtime, &journal)
}

fn automatic_resume(journal: &J, method: &str) -> Result<bool> {
    let recorded = verifiable(journal);
    ensure(recorded || readonly(method),
        "recorded session configuration is unavailable; metadata reads remain allowed, business RPC requires verified subscription")?;
    Ok(recorded && method != "thread/resume")
}

fn rejoin(client: &mut DirectClient, runtime: &J, journal: &J) -> Result<()> {
    let result = client.call("thread/resume", &json!({"threadId":journal["session"]}))?;
    validate(runtime, journal, &result)?;
    client.subscribed_session = Some(required(journal, "session")?);
    Ok(())
}

pub(in crate::task_runtime) fn completed(
    root: &Path,
    client: &mut DirectClient,
    method: &str,
    params: &J,
    result: &J,
) -> Result<()> {
    if !matches!(method, "thread/start" | "thread/resume" | "turn/start") {
        return Ok(());
    }
    let runtime = util::read_state_json(root, "runtime.json")?;
    if !selected(&runtime) {
        return Ok(());
    }
    if method == "turn/start" {
        return turn_configuration(root, params, result);
    }
    let journal = util::read_state_json(root, "upstream-session.json")?;
    if method == "thread/resume" {
        validate(&runtime, &journal, result)?;
    }
    client.subscribed_session = Some(required(&journal, "session")?);
    Ok(())
}

fn turn_configuration(root: &Path, params: &J, result: &J) -> Result<()> {
    let _journal = super::session_lock(root)?;
    let turn = required(&result["turn"], "id")?;
    let mut journal = util::read_state_json(root, "upstream-session.json")?;
    let mut overrides = serde_json::Map::new();
    for (source, target) in [
        ("model", "model"),
        ("effort", "reasoningEffort"),
        ("cwd", "cwd"),
        ("approvalPolicy", "approvalPolicy"),
        ("approvalsReviewer", "approvalsReviewer"),
        ("sandboxPolicy", "sandbox"),
        ("disabledPluginIds", "disabledPluginIds"),
    ] {
        if let Some(value) = params.get(source).filter(|value| !value.is_null()) {
            journal["session_configuration"][target] = value.clone();
            overrides.insert(target.to_owned(), value.clone());
        }
    }
    // A successful response proves these host-sent defaults took effect. An
    // unknown response never adopts observed configuration or authorizes replay.
    journal["configuration_transition"] =
        json!({"phase":"applied","turn":turn,"overrides":overrides});
    util::atomic_write_json(&root.join("upstream-session.json"), &journal)
}

fn validate(runtime: &J, journal: &J, result: &J) -> Result<()> {
    ensure(
        result["thread"]["id"] == journal["session"] && result["thread"]["cwd"] == runtime["cwd"],
        "resumed thread identity/directory changed",
    )?;
    ensure(
        result["model"] == runtime["model"]["id"]
            && result["modelProvider"] == runtime["model"]["providerID"]
            && result["thread"]["modelProvider"] == runtime["model"]["providerID"],
        "resumed thread model/provider changed",
    )?;
    role_models::verify_effort(&runtime["model"], result)?;
    ensure(
        verifiable(journal)
            && comparable(&configuration(result)) == comparable(&journal["session_configuration"]),
        "resumed thread permissions or configuration changed",
    )
}

#[cfg(test)]
mod intent_tests {
    use super::*;

    #[test]
    fn configuration_writer_reads_after_shared_lock_and_preserves_requested_intent() {
        let temp = util::fs::test_tempdir();
        let root = temp.path();
        let mut journal = json!({"session_configuration":{}, "presentation_intent":{"version":1,"phase":"not_requested"}});
        util::atomic_write_json(&root.join("upstream-session.json"), &journal).unwrap();
        let lock = super::super::session_lock(root).unwrap();
        let owned = root.to_path_buf();
        let (sent, received) = std::sync::mpsc::channel();
        let (finished, completion) = std::sync::mpsc::channel();
        let writer = std::thread::spawn(move || {
            sent.send(()).unwrap();
            turn_configuration(
                &owned,
                &json!({"model":"fixture"}),
                &json!({"turn":{"id":"turn"}}),
            )
            .unwrap();
            finished.send(()).unwrap();
        });
        received.recv().unwrap();
        assert!(
            matches!(
                completion.recv_timeout(std::time::Duration::from_millis(100)),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout)
            ),
            "configuration writer must wait for presentation's journal lock"
        );
        journal["presentation_intent"]["phase"] = json!("requested");
        util::atomic_write_json(&root.join("upstream-session.json"), &journal).unwrap();
        drop(lock);
        completion
            .recv_timeout(std::time::Duration::from_secs(3))
            .unwrap();
        writer.join().unwrap();
        let result = util::read_state_json(root, "upstream-session.json").unwrap();
        assert_eq!(result["presentation_intent"]["phase"], "requested");
        assert_eq!(result["configuration_transition"]["phase"], "applied");
        assert_eq!(result["session_configuration"]["model"], "fixture");
    }
}
