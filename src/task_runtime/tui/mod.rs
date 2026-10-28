//! Native Codex presentation for isolated task runtimes. The upstream service
//! and broker keep their existing identities; each attachment owns a new tab.
use super::*;
use std::time::{Duration, Instant};
#[cfg(test)]
mod fixture;
mod herdr;
mod hooks;
mod persistence;
mod protocol;

pub(super) fn configure(runtime: &mut J) -> Result<()> {
    if runtime.get("native_tui").is_none() {
        return Ok(());
    }
    validate_config(&runtime["native_tui"])?;
    configure_socket(runtime)?;
    configure_parent(runtime)?;
    persistence::configure(&mut runtime["native_tui"])?;
    runtime["native_tui_hooks"] = hooks::discover(runtime)?;
    Ok(())
}

fn validate_config(config: &J) -> Result<()> {
    ensure(
        config.as_object().is_some_and(|m| {
            m.keys().all(|k| {
                [
                    "binary",
                    "parent_pane",
                    "direction",
                    "herdr_socket",
                    "parent_terminal",
                    "session_store",
                ]
                .contains(&k.as_str())
            })
        }),
        "invalid native_tui configuration",
    )?;
    // Accept old launch specs, but all new attachments now use separate tabs.
    if let Some(direction) = config.get("direction") {
        ensure(
            matches!(direction.as_str(), Some("right" | "down")),
            "legacy native_tui direction must be right or down",
        )?;
    }
    let binary = PathBuf::from(required(config, "binary")?);
    ensure(
        binary.is_absolute() && binary.is_file(),
        "native_tui requires the matching installed Codex binary",
    )
}

fn configure_socket(runtime: &mut J) -> Result<()> {
    ensure(
        std::env::var("HERDR_ENV").as_deref() == Ok("1"),
        "native TUI requires Herdr",
    )?;
    let socket =
        std::env::var("HERDR_SOCKET_PATH").map_err(|_| FlowError::guard("Herdr socket missing"))?;
    let socket = upstream::socket_identity(Path::new(&socket))?;
    if let Some(expected) = runtime["native_tui"].get("herdr_socket") {
        ensure(*expected == socket, "Herdr server changed")?;
    }
    runtime["native_tui"]["herdr_socket"] = socket;
    runtime["native_tui"]["binary"] = json!(std::fs::canonicalize(required(
        &runtime["native_tui"],
        "binary"
    )?)?);
    Ok(())
}

fn configure_parent(runtime: &mut J) -> Result<()> {
    let parent = herdr::call(
        &runtime["native_tui"],
        "pane.get",
        json!({"pane_id":runtime["native_tui"]["parent_pane"]}),
    )?;
    let terminal = &parent["pane"]["terminal_id"];
    ensure(terminal.is_string(), "Herdr parent terminal missing")?;
    if let Some(expected) = runtime["native_tui"].get("parent_terminal") {
        ensure(expected == terminal, "Herdr parent terminal changed")?;
    }
    runtime["native_tui"]["parent_terminal"] = terminal.clone();
    Ok(())
}

pub(super) fn thread_config(root: &Path, model: &J) -> Result<J> {
    let mut config = role_models::thread_config(model);
    let runtime = util::read_state_json(root, "runtime.json")?;
    if upstream::selected(&runtime) {
        // Codex otherwise persists project trust when thread/start requests a
        // writable sandbox in a fresh worktree. Keep task-local project config
        // disabled through a request-only override, without editing user config.
        config["projects"] = json!({required(&runtime, "cwd")?:{"trust_level":"untrusted"}});
    }
    if runtime.get("native_tui").is_some() {
        config["hooks"] = json!({"state":runtime["native_tui_hooks"]});
        config["history"] = json!({"persistence":"none"});
    }
    Ok(config)
}

/// A presentation failure must never turn a submitted task into a retryable
/// dispatch error. The recorded turn is still authoritative and is not replayed.
pub(super) fn after_submission(env: &TaskEnvironment, root: &Path) -> J {
    match util::read_state_json(root, "runtime.json") {
        Ok(runtime) if runtime.get("native_tui").is_none() => json!({"phase":"not_configured"}),
        Err(error) => {
            json!({"phase":"failed","error":error.to_string(),"task_replay_allowed":false})
        }
        _ => match open(env, root) {
            Ok(value) => value,
            Err(error) => {
                json!({"phase":"failed","error":error.to_string(),"task_replay_allowed":false})
            }
        },
    }
}

fn runtime(env: &TaskEnvironment, root: &Path) -> Result<J> {
    let runtime = super::verify(env, root)?;
    ensure(
        upstream::selected(&runtime) && runtime["native_tui"].is_object(),
        "runtime has no native_tui configuration",
    )?;
    upstream::endpoint(&runtime)?;
    Ok(runtime)
}

fn binding(root: &Path) -> Result<Option<J>> {
    util::read_state_json_bounded_if_exists(root, "presentation.json", 1024 * 1024)
}

pub fn open(env: &TaskEnvironment, root: &Path) -> Result<J> {
    let _permit = crate::task_env::isolation::mutation_permit(env)?;
    super::retirement::admit(root)?;
    let runtime = runtime(env, root)?;
    open_bound(root, &runtime)
}

/// The recovery session was registered before primary work started. Opening
/// its presentation must not require the environment lock held by a stuck
/// primary operation. The same live Owner, broker and pane checks still apply.
pub(in crate::task_runtime) fn open_recovery(root: &Path) -> Result<J> {
    open_readonly(root)
}

pub(in crate::task_runtime) fn open_readonly(root: &Path) -> Result<J> {
    let permit = crate::lifecycle::runtime_permit(root)?
        .ok_or_else(|| FlowError::guard("recovery presentation requires a task Owner"))?;
    let runtime = util::read_state_json_bounded(root, "runtime.json", 1024 * 1024)?;
    verify_recovery_runtime(root, &runtime, permit.owner())?;
    if runtime.get("native_tui").is_none() {
        return Ok(json!({"phase":"not_configured"}));
    }
    upstream::endpoint(&runtime)?;
    open_bound(root, &runtime)
}

fn verify_recovery_runtime(
    root: &Path,
    runtime: &J,
    owner: &crate::lifecycle::Owner,
) -> Result<()> {
    ensure(
        (runtime["recovery_role"] == true || runtime["publication_role"] == true)
            && runtime["root"] == json!(root)
            && util::resolve_path(root) == root,
        "recovery presentation runtime changed",
    )?;
    let pid = runtime["pid"].as_u64().unwrap_or(0) as i32;
    ensure(
        crate::task_env::process_has_marker(pid, &owner.instance_id)
            && !runtime["start_time"].is_null()
            && runtime["start_time"] == json!(crate::task_env::process_start_time(pid)),
        "recovery presentation broker identity changed",
    )
}

fn open_bound(root: &Path, runtime: &J) -> Result<J> {
    let _lock = presentation_lock(root)?;
    if let Some(previous) = existing_attachment(root, runtime)? {
        // Reconcile an existing attempt, never issue a second pane/run.
        return await_ready(root, runtime, previous);
    }
    let record = reserve_attachment(root, runtime)?;
    let record = create_tab(root, runtime, record)?;
    launch_frontend(runtime, &record)?;
    await_ready(root, runtime, record)
}

fn existing_attachment(root: &Path, runtime: &J) -> Result<Option<J>> {
    let Some(previous) = binding(root)? else {
        return Ok(None);
    };
    validate_binding(runtime, &previous)?;
    if previous["phase"] == "closed" {
        // Finish the old presentation's saved copies before replacing its
        // identity. Reopening preserves the upstream session, not an old tab.
        persistence::reclaim(&previous)?;
        return Ok(None);
    }
    Ok(Some(previous))
}

fn reserve_attachment(root: &Path, runtime: &J) -> Result<J> {
    let session = materialized_session(root, runtime)?;
    let config = &runtime["native_tui"];
    let parent = parent_pane(config)?;
    let workspace = required(&parent, "workspace_id")?;
    let parent_tab = required(&parent, "tab_id")?;
    let nonce = util::pyjson::uuid_hex();
    let directory = root.join(format!("tui-{nonce}"));
    util::ensure_private_dir(&directory, true)?;
    let record = json!({"phase":"creating","instance":runtime["instance"],"session":session,
        "nonce":nonce,"directory":directory,"herdr_socket":config["herdr_socket"],
        "runtime":root,"pane":null,"terminal":null,"cwd":runtime["cwd"],
        "workspace":workspace,"parent_tab":parent_tab,"tab":null,"tab_number":null,
        "session_store":config.get("session_store")});
    util::atomic_write_json(&root.join("presentation.json"), &record)?;
    Ok(record)
}

fn parent_pane(config: &J) -> Result<J> {
    let parent = herdr::call(config, "pane.get", json!({"pane_id":config["parent_pane"]}))?;
    ensure(
        parent["pane"]["terminal_id"] == config["parent_terminal"],
        "parent terminal changed",
    )?;
    Ok(parent["pane"].clone())
}

fn presentation_lock(root: &Path) -> Result<util::FileLock> {
    util::FileLock::acquire(&root.join("presentation.lock"), false)?
        .ok_or_else(|| FlowError::guard("native TUI operation already in progress"))
}

fn materialized_session(root: &Path, runtime: &J) -> Result<String> {
    let journal = util::read_state_json(root, "upstream-session.json")?;
    ensure(
        journal["phase"] == "created" && journal["instance"] == runtime["instance"],
        "native task thread is not recorded",
    )?;
    let session = required(&journal, "session")?;
    await_materialized(root, &session)?;
    Ok(session)
}

fn create_tab(root: &Path, runtime: &J, mut record: J) -> Result<J> {
    let label = format!(
        "flow:{}",
        root.file_name().unwrap_or_default().to_string_lossy()
    );
    let created = herdr::call(
        &record,
        "tab.create",
        json!({"workspace_id":record["workspace"],"cwd":runtime["cwd"],
        "label":label,"focus":false}),
    )?;
    bind_created_tab(&mut record, &created)?;
    record["phase"] = json!("launching");
    util::atomic_write_json(&root.join("presentation.json"), &record)?;
    util::atomic_write_json(
        &PathBuf::from(required(&record, "directory")?).join("attachment.json"),
        &record,
    )?;
    herdr::call(
        &record,
        "pane.rename",
        json!({"pane_id":record["pane"],"label":label}),
    )?;
    Ok(record)
}

fn bind_created_tab(record: &mut J, created: &J) -> Result<()> {
    record["pane"] = created["root_pane"]["pane_id"].clone();
    record["terminal"] = created["root_pane"]["terminal_id"].clone();
    record["tab"] = created["tab"]["tab_id"].clone();
    record["tab_number"] = created["tab"]["number"].clone();
    required(record, "pane")?;
    required(record, "terminal")?;
    required(record, "tab")?;
    ensure(
        created["root_pane"]["workspace_id"] == record["workspace"]
            && created["tab"]["workspace_id"] == record["workspace"]
            && created["root_pane"]["tab_id"] == record["tab"]
            && record["tab"] != record["parent_tab"]
            && record["tab_number"].is_u64(),
        "Herdr did not create a separate tab in the parent workspace",
    )
}

fn launch_frontend(runtime: &J, record: &J) -> Result<()> {
    let executable = runtime
        .get("driver_binary")
        .cloned()
        .unwrap_or(json!(std::env::current_exe()?));
    let argv = vec![
        "env".into(),
        format!(
            "{}={}",
            crate::task_env::MARKER_ENV,
            required(runtime, "instance")?
        ),
        required(&json!({"binary":executable}), "binary")?,
        "task-runtime".into(),
        "tui-serve".into(),
        "--state-dir".into(),
        required(record, "directory")?,
    ];
    herdr::run(record, util::process::shell_join(&argv))
}

fn await_materialized(root: &Path, session: &str) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let result = super::broker::call(
            root,
            "thread/read",
            &json!({"threadId":session,"includeTurns":true}),
        );
        match result {
            Ok(_) => return Ok(()),
            Err(error) if unmaterialized(&error) => {
                ensure(
                    Instant::now() < deadline,
                    "native TUI awaits first real task message; no seed turn or automatic replay",
                )?;
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(error) => return Err(error),
        }
    }
}

fn unmaterialized(error: &FlowError) -> bool {
    error.rpc_code.is_some()
        && ["not materialized", "no rollout", "empty rollout"]
            .iter()
            .any(|text| error.message.contains(text))
}

fn validate_binding(runtime: &J, record: &J) -> Result<()> {
    ensure(
        record["instance"] == runtime["instance"]
            && record["runtime"] == runtime["root"]
            && record["herdr_socket"] == runtime["native_tui"]["herdr_socket"],
        "native TUI ownership changed",
    )?;
    let journal = util::read_state_json(
        Path::new(&required(runtime, "root")?),
        "upstream-session.json",
    )?;
    ensure(
        journal["instance"] == runtime["instance"] && journal["session"] == record["session"],
        "native TUI session ownership changed",
    )
}

fn marker(record: &J) -> Result<Option<J>> {
    util::read_state_json_bounded_if_exists(
        Path::new(&required(record, "directory")?),
        "tui.json",
        1024 * 1024,
    )
}

fn verify_marker(record: &J, runtime: &J, marker: &J) -> Result<()> {
    ensure(
        marker["nonce"] == record["nonce"]
            && marker["session"] == record["session"]
            && marker["binary"] == runtime["native_tui"]["binary"]
            && marker["server_socket"] == runtime["upstream_socket"]["path"],
        "native TUI session/socket changed",
    )?;
    let actual = &marker["runtime"];
    ensure(
        actual["cwd"] == runtime["cwd"]
            && actual["model"] == runtime["model"]["id"]
            && actual["modelProvider"] == runtime["model"]["providerID"],
        "native TUI changed cwd/model/provider",
    )?;
    role_models::verify_effort(&runtime["model"], actual)
}

fn alive(marker: &J) -> bool {
    [
        ("pid", "pid_start_ticks"),
        ("wrapper_pid", "wrapper_start_ticks"),
    ]
    .iter()
    .all(|(pid, ticks)| {
        marker[*pid]
            .as_u64()
            .and_then(|p| util::process::process_start_ticks(p as u32))
            .is_some_and(|actual| {
                marker[*ticks]
                    .as_str()
                    .map(str::to_owned)
                    .or_else(|| marker[*ticks].as_u64().map(|value| value.to_string()))
                    == Some(actual)
            })
    })
}

fn await_ready(root: &Path, runtime: &J, mut record: J) -> Result<J> {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        ensure(
            herdr::pane(&record)?.is_some(),
            "native TUI pane was closed; use tui-close before reopening",
        )?;
        if attachment_ready(&record, runtime)? {
            record["phase"] = json!("ready");
            util::atomic_write_json(&root.join("presentation.json"), &record)?;
            return Ok(record);
        }
        ensure(
            Instant::now() < deadline,
            "native TUI startup pending; inspect pane, do not replay task",
        )?;
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn attachment_ready(record: &J, runtime: &J) -> Result<bool> {
    if let Some(marker) = marker(record)? {
        ensure(
            !matches!(marker["status"].as_str(), Some("error" | "exited")),
            "native TUI stopped; inspect pane and close attachment before reopening",
        )?;
        if marker["status"] == "ready" {
            verify_marker(record, runtime, &marker)?;
            ensure(alive(&marker), "native TUI process exited")?;
            // Re-check the protocol-derived identity before publishing UI identity.
            herdr::report(record)?;
            // The resume response precedes native history hydration and
            // Herdr's process detection. Do not promise an addressable
            // agent while `agent prompt` still returns agent_not_found.
            return herdr::agent_ready(record);
        }
    }
    Ok(false)
}

pub fn status(env: &TaskEnvironment, root: &Path) -> Result<J> {
    let runtime = runtime(env, root)?;
    let Some(mut record) = binding(root)? else {
        return Ok(json!({"phase":"not_opened"}));
    };
    validate_binding(&runtime, &record)?;
    record["pane_present"] = json!(if record["pane"].is_null() {
        false
    } else {
        herdr::pane(&record)?.is_some()
    });
    if let Some(marker) = marker(&record)? {
        record["native_status"] = marker["status"].clone();
        record["processes_alive"] = json!(alive(&marker));
        if marker["status"] == "ready" {
            verify_marker(&record, &runtime, &marker)?;
        }
    }
    Ok(record)
}

pub(in crate::task_runtime) fn recovery_notice(
    root: &Path,
    runtime: &J,
    message: &str,
) -> Result<()> {
    if runtime.get("native_tui").is_none() {
        return Ok(());
    }
    let record = binding(root)?.ok_or_else(|| FlowError::guard("task pane not opened"))?;
    validate_binding(runtime, &record)?;
    ensure(herdr::pane(&record)?.is_some(), "task pane missing")?;
    herdr::call(
        &record,
        "pane.rename",
        json!({"pane_id":record["pane"],
        "label":format!("flow:{} · {message}", root.file_name().unwrap_or_default().to_string_lossy())}),
    )?;
    Ok(())
}

pub fn close(env: &TaskEnvironment, root: &Path) -> Result<J> {
    let _permit = crate::task_env::isolation::mutation_permit(env)?;
    super::retirement::admit(root)?;
    let runtime = runtime(env, root)?;
    let _lock = presentation_lock(root)?;
    close_bound(root, &runtime)
}

fn close_bound(root: &Path, runtime: &J) -> Result<J> {
    let record = binding(root)?.ok_or_else(|| FlowError::guard("native TUI was not opened"))?;
    validate_binding(runtime, &record)?;
    let mut record = capture_presentation(root, &record)?;
    herdr::close(&mut record)?;
    util::atomic_write_json(&root.join("presentation.json"), &record)?;
    Ok(record)
}

fn capture_presentation(root: &Path, record: &J) -> Result<J> {
    let record = persistence::capture(record)?;
    util::atomic_write_json(&root.join("presentation.json"), &record)?;
    Ok(record)
}

pub fn serve(directory: &Path) -> Result<i32> {
    let record = util::read_state_json(directory, "attachment.json")?;
    ensure(
        record["directory"] == json!(directory),
        "attachment directory changed",
    )?;
    let root = PathBuf::from(required(&record, "runtime")?);
    let _permit = crate::lifecycle::runtime_permit(&root)?;
    let runtime = util::read_state_json(&root, "runtime.json")?;
    serve_admission(&root, &record, &runtime)?;
    let bridge = task_bridge(directory, &record, &runtime)?;
    // Only admission holds the lifecycle permit; cleanup must be able to stop
    // a visible, long-running frontend. It is tracked by its task marker/pane.
    drop(_permit);
    let code = bridge
        .with_policy(Box::new(protocol::Policy::new(record, runtime)))
        .run();
    ensure(code == 0, &format!("native TUI exited with status {code}"))?;
    Ok(code)
}

fn serve_admission(root: &Path, record: &J, runtime: &J) -> Result<()> {
    validate_binding(runtime, record)?;
    ensure(
        std::env::var(crate::task_env::MARKER_ENV).ok().as_deref() == runtime["instance"].as_str(),
        "unowned native TUI process",
    )?;
    ensure(
        std::env::var("HERDR_PANE_ID").ok().as_deref() == record["pane"].as_str(),
        "native TUI is running in another pane",
    )?;
    ensure(
        binding(root)?.as_ref() == Some(record),
        "native TUI launch was superseded",
    )?;
    ensure(herdr::pane(record)?.is_some(), "native TUI pane missing")
}

fn task_bridge(
    directory: &Path,
    record: &J,
    runtime: &J,
) -> Result<crate::codex::tui::ProxyBridge> {
    let socket = upstream::endpoint(runtime)?;
    Ok(crate::codex::tui::ProxyBridge::new(
        directory,
        socket,
        &required(&runtime["native_tui"], "binary")?,
        &required(runtime, "cwd")?,
        &required(record, "nonce")?,
        Some(&required(record, "session")?),
    ))
}

pub(crate) fn obligations(env: &TaskEnvironment) -> Result<Vec<J>> {
    let directory = env.temp_root.join("rt");
    if !directory.try_exists()? {
        return Ok(vec![]);
    }
    let mut out = Vec::new();
    for (index, entry) in std::fs::read_dir(directory)?.enumerate() {
        ensure(index < 128, "native TUI cleanup scan exceeds bound")?;
        out.extend(entry_obligations(env, entry)?);
    }
    Ok(out)
}

fn entry_obligations(
    env: &TaskEnvironment,
    entry: std::io::Result<std::fs::DirEntry>,
) -> Result<Vec<J>> {
    let root = entry?.path();
    let frozen = super::retirement::presentation(env, &root)?;
    let current = binding(&root)?;
    let mut records = frozen_obligations(env, &root, frozen.as_ref())?;
    if let Some(current) = current {
        validate_obligation(env, &root, &current)?;
        if frozen
            .as_ref()
            .is_none_or(|f| !same_attachment(f, &current))
        {
            records.push(persistence::capture(&current)?);
        }
    }
    Ok(records)
}

fn frozen_obligations(env: &TaskEnvironment, root: &Path, frozen: Option<&J>) -> Result<Vec<J>> {
    if let Some(frozen) = frozen {
        validate_obligation(env, root, frozen)?;
        return Ok(vec![frozen.clone()]);
    }
    Ok(vec![])
}

fn same_attachment(left: &J, right: &J) -> bool {
    [
        "instance",
        "runtime",
        "session",
        "herdr_socket",
        "nonce",
        "pane",
        "terminal",
        "directory",
    ]
    .iter()
    .all(|key| left[key] == right[key])
}

fn validate_obligation(env: &TaskEnvironment, root: &Path, record: &J) -> Result<()> {
    let runtime = util::read_state_json(root, "runtime.json")?;
    super::verify_owner_record(env, &runtime)?;
    validate_binding(&runtime, record)
}

pub(crate) fn reclaim(record: &mut J) -> Result<()> {
    herdr::close(record)?;
    persistence::reclaim(record)
}

/// Freeze only the exact recorded presentation and frontend identities. This
/// does not close a pane or make native calls while the queue selects resources.
pub(crate) fn retirement_snapshot(
    env: &TaskEnvironment,
    root: &Path,
) -> Result<(Option<J>, Vec<crate::task_env::ProcessRecord>)> {
    let runtime = util::read_state_json(root, "runtime.json")?;
    super::verify_owner_record(env, &runtime)?;
    let Some(record) = binding(root)? else {
        ensure(
            runtime.get("native_tui").is_none(),
            "configured presentation has no settled binding",
        )?;
        return Ok((None, vec![]));
    };
    let processes = retirement_frontends(&runtime, &record)?;
    Ok((Some(record), processes))
}

fn retirement_frontends(runtime: &J, record: &J) -> Result<Vec<crate::task_env::ProcessRecord>> {
    validate_binding(runtime, record)?;
    ensure(
        matches!(record["phase"].as_str(), Some("ready" | "closed")),
        "presentation creation is not settled",
    )?;
    let marker =
        marker(record)?.ok_or_else(|| FlowError::guard("presentation process proof missing"))?;
    verify_marker(record, runtime, &marker)?;
    [
        ("pid", "pid_start_ticks"),
        ("wrapper_pid", "wrapper_start_ticks"),
    ]
    .iter()
    .map(|(pid, ticks)| retirement_process(&marker[*pid], &marker[*ticks]))
    .collect()
}

fn retirement_process(pid: &J, ticks: &J) -> Result<crate::task_env::ProcessRecord> {
    let pid = pid
        .as_u64()
        .and_then(|p| i32::try_from(p).ok())
        .filter(|p| *p > 1)
        .ok_or_else(|| FlowError::guard("frontend PID missing"))?;
    let start_time = ticks
        .as_u64()
        .or_else(|| ticks.as_str().and_then(|s| s.parse().ok()))
        .ok_or_else(|| FlowError::guard("frontend start identity missing"))?;
    Ok(crate::task_env::ProcessRecord {
        pid,
        start_time: Some(start_time),
    })
}

pub(crate) fn capture_retirement(record: &J) -> Result<J> {
    persistence::capture(record)
}

/// A native user turn has the same queue-tool ownership checks as a broker
/// prompt. Only the upstream event for the recorded receiver can advance it.
/// Called under the broker's lifecycle permit, before recording tool requests.
pub(super) fn observe_turn(root: &Path, event: &J) -> Result<()> {
    if event["method"] != "turn/started" {
        return Ok(());
    }
    let runtime = util::read_state_json(root, "runtime.json")?;
    if runtime.get("native_tui").is_none() {
        return Ok(());
    }
    let receiver = util::read_state_json_bounded_if_exists(root, "receiver.json", 1024 * 1024)?;
    let Some(receiver) = receiver else {
        return Ok(());
    };
    let session = &receiver["state"]["session"];
    if receiver["phase"] != "bound" || event["params"]["threadId"] != *session {
        return Ok(());
    }
    let turn = required(&event["params"]["turn"], "id")?;
    util::atomic_write_json(
        &root.join("receiver-turn.json"),
        &json!({"thread":session,"turn":turn}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn separate_tab_binding_rejects_parent_tab_or_other_workspace() {
        let config = json!({"binary":"/bin/true","parent_pane":"parent"});
        validate_config(&config).unwrap();
        let mut legacy = config.clone();
        legacy["direction"] = json!("right");
        validate_config(&legacy).unwrap();
        let binding = json!({"workspace":"workspace","parent_tab":"original"});
        let created = json!({"tab":{"tab_id":"new","workspace_id":"workspace","number":42},
            "root_pane":{"pane_id":"pane","terminal_id":"terminal","workspace_id":"workspace","tab_id":"new"}});
        bind_created_tab(&mut binding.clone(), &created).unwrap();
        let mut wrong = created.clone();
        wrong["tab"]["workspace_id"] = json!("other");
        assert!(bind_created_tab(&mut binding.clone(), &wrong).is_err());
        wrong = created;
        wrong["tab"]["tab_id"] = json!("original");
        wrong["root_pane"]["tab_id"] = json!("original");
        assert!(bind_created_tab(&mut binding.clone(), &wrong).is_err());
    }

    #[test]
    fn upstream_project_trust_is_explicitly_scoped_to_the_thread() {
        let root = util::fs::test_tempdir();
        util::atomic_write_json(
            &root.path().join("runtime.json"),
            &json!({"configuration":"upstream","cwd":"/task/checkout"}),
        )
        .unwrap();
        let config = thread_config(root.path(), &json!({"reasoning_effort":"high"})).unwrap();
        assert_eq!(
            config["projects"],
            json!({"/task/checkout":{"trust_level":"untrusted"}})
        );
        assert_eq!(config["model_reasoning_effort"], "high");
        util::atomic_write_json(&root.path().join("runtime.json"), &json!({})).unwrap();
        assert!(thread_config(root.path(), &json!({})).unwrap()["projects"].is_null());
    }
    #[test]
    fn native_turn_can_advance_control_but_foreign_turn_cannot() {
        let root = util::fs::test_tempdir();
        util::atomic_write_json(&root.path().join("runtime.json"), &json!({"native_tui":{}}))
            .unwrap();
        util::atomic_write_json(
            &root.path().join("receiver.json"),
            &json!({"phase":"bound","state":{"session":"owned"}}),
        )
        .unwrap();
        util::atomic_write_json(
            &root.path().join("receiver-turn.json"),
            &json!({"thread":"owned","turn":"broker-turn"}),
        )
        .unwrap();
        let foreign = json!({"method":"turn/started","params":{"threadId":"foreign","turn":{"id":"other-turn"}}});
        observe_turn(root.path(), &foreign).unwrap();
        assert_eq!(
            util::read_state_json(root.path(), "receiver-turn.json").unwrap()["turn"],
            "broker-turn"
        );
        let mut native = foreign;
        native["params"]["threadId"] = json!("owned");
        native["params"]["turn"]["id"] = json!("native-turn");
        observe_turn(root.path(), &native).unwrap();
        assert_eq!(
            util::read_state_json(root.path(), "receiver-turn.json").unwrap(),
            json!({"thread":"owned","turn":"native-turn"})
        );
    }
    #[test]
    fn live_identity_accepts_native_numeric_ticks_but_rejects_pid_reuse() {
        let pid = std::process::id();
        let ticks = util::process::process_start_ticks(pid).unwrap();
        let mut marker = json!({"pid":pid,"wrapper_pid":pid,"pid_start_ticks":ticks.parse::<u64>().unwrap(),"wrapper_start_ticks":ticks});
        assert!(alive(&marker));
        marker["pid_start_ticks"] = json!(0);
        assert!(!alive(&marker));
    }
}
