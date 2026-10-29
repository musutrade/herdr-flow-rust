//! Admission is fenced before these observations. No interrupt, deletion,
//! resend, process signal or shared service restart is performed here.
use super::*;
use crate::task_env::ProcessRecord;
use std::collections::BTreeSet;
use std::time::{Duration, Instant};

pub(super) fn quiescent(env: &TaskEnvironment, record: &J, state: &J) -> Result<J> {
    let checkout = checkout_binding(env, record)?;
    queue_operations(state, record)?;
    git_locks(&checkout)?;
    let mut observation = Observation::default();
    runtimes(env, &checkout, record, &mut observation)?;
    let owner: Owner = serde_json::from_value(record["owner"].clone())?;
    add_watch(
        &queue::artifacts(&owner.queue_root)?,
        &mut observation.allowed,
    )?;
    crate::task_env::terminal::human_quiescence(env, &checkout, &observation.allowed)?;
    Ok(
        json!({"at":util::now(),"sessions":observation.sessions,"retained_processes":observation.allowed,
        "fingerprint":fingerprint(record)?,"operation_replay_allowed":false}),
    )
}

#[derive(Default)]
struct Observation {
    allowed: Vec<ProcessRecord>,
    sessions: Vec<J>,
    sockets: BTreeSet<PathBuf>,
}

fn checkout_binding(env: &TaskEnvironment, record: &J) -> Result<PathBuf> {
    let checkout = PathBuf::from(required(record, "checkout")?);
    ensure(
        checkout == env.workspace_root.join("checkout")
            && directory(&checkout)? == record["identity"],
        "human checkout binding changed",
    )?;
    ensure(
        env.delivery.is_null(),
        "settle the existing delivery and stage continuation before handing over its checkout",
    )?;
    Ok(checkout)
}

fn runtimes(
    env: &TaskEnvironment,
    checkout: &Path,
    record: &J,
    observation: &mut Observation,
) -> Result<()> {
    let coordinator = PathBuf::from(required(record, "coordinator")?);
    for (index, entry) in std::fs::read_dir(env.temp_root.join("rt"))?.enumerate() {
        ensure(index < 128, "human runtime inventory exceeds bound")?;
        runtime(env, &entry?.path(), checkout, &coordinator, observation)?;
    }
    Ok(())
}

fn queue_operations(state: &J, record: &J) -> Result<()> {
    let tasks = state["tasks"]
        .as_object()
        .ok_or_else(|| FlowError::guard("queue tasks missing"))?;
    let owner: Owner = serde_json::from_value(record["owner"].clone())?;
    for (name, task) in tasks {
        if crate::lifecycle::task_owner(state, name)?.as_ref() != Some(&owner) {
            continue;
        }
        task_operations(task)?;
    }
    Ok(())
}

fn task_operations(task: &J) -> Result<()> {
    ensure(
        task["writer"].is_null(),
        "original logical writer has not been released by its result application",
    )?;
    ensure(
        task["pr"]["publication"]["phase"] != "sending",
        "original Git/PR publication outcome is unresolved",
    )?;
    if let Some(operations) = task["operations"].as_object() {
        ensure(
            operations.values().all(|op| op["phase"] == "accepted"),
            "original dispatch operation is unresolved",
        )?;
    }
    Ok(())
}

fn git_locks(checkout: &Path) -> Result<()> {
    for name in [
        "index.lock",
        "HEAD.lock",
        "config.lock",
        "packed-refs.lock",
        "shallow.lock",
    ] {
        let bytes = util::process::git_output_bounded(
            checkout,
            &["rev-parse", "--path-format=absolute", "--git-path", name],
            Duration::from_secs(5),
        )?;
        let path = PathBuf::from(
            String::from_utf8(bytes)
                .map_err(|_| FlowError::guard("Git path is not UTF-8"))?
                .trim(),
        );
        ensure(
            path.is_absolute() && !path.try_exists()?,
            "Git lock still exists; inspect its original operation",
        )?;
    }
    Ok(())
}

fn runtime(
    env: &TaskEnvironment,
    root: &Path,
    checkout: &Path,
    coordinator: &Path,
    observation: &mut Observation,
) -> Result<()> {
    let Some(runtime) = read(root, "runtime.json")? else {
        return Ok(());
    };
    let presentation =
        retained_controls(env, root, &runtime, coordinator, &mut observation.allowed)?;
    if !crate::task_env::paths_overlap(Path::new(&required(&runtime, "cwd")?), checkout) {
        return Ok(());
    }
    if read(root, "stage-retirement.json")?.is_some_and(|v| v["phase"] == "retired") {
        return Ok(());
    }
    drained(root, presentation)?;
    observe_session(env, root, &runtime, checkout, observation)
}

fn retained_controls(
    env: &TaskEnvironment,
    root: &Path,
    runtime: &J,
    coordinator: &Path,
    allowed: &mut Vec<ProcessRecord>,
) -> Result<Option<J>> {
    ensure(
        runtime["lifecycle_owner"] == json!(crate::task_env::isolation::owner(env))
            && runtime["root"] == json!(root),
        "human runtime ownership changed",
    )?;
    add_process(runtime, "start_time", allowed)?;
    let (presentation, frontends) = tui::human_snapshot(env, root, coordinator)?;
    allowed.extend(frontends);
    runtime_watches(env, root, allowed)?;
    Ok(presentation)
}

pub(super) fn runtime_watches(
    env: &TaskEnvironment,
    root: &Path,
    allowed: &mut Vec<ProcessRecord>,
) -> Result<()> {
    let receiver = read(root, "receiver.json")?;
    let mut registration = J::Null;
    if let Some(receiver) = &receiver {
        let guard = PathBuf::from(required(receiver, "guard_dir")?);
        add_watch(&guard, allowed)?;
        registration = read(&guard, "activation.json")?.unwrap_or(J::Null)["recovery"].clone();
    }
    add_watch(root, allowed)?;
    observer_watch(env, root, receiver.is_some(), &registration, allowed)
}

fn observer_watch(
    env: &TaskEnvironment,
    root: &Path,
    required_observer: bool,
    registration: &J,
    allowed: &mut Vec<ProcessRecord>,
) -> Result<()> {
    let observer = root.join("recovery-observer");
    if !observer_present(&observer, required_observer || !registration.is_null())? {
        return Ok(());
    }
    observer_binding(env, root, &observer)?;
    let watch = read(&observer, "watch.json")?;
    observer_registration(&observer, registration, watch.as_ref())?;
    if let Some(watch) = watch {
        registered_observer(env, &watch, allowed)?;
    }
    Ok(())
}

fn observer_present(observer: &Path, required_observer: bool) -> Result<bool> {
    match std::fs::symlink_metadata(observer) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound && !required_observer => {
            Ok(false)
        }
        Err(error) => Err(FlowError::io(format!(
            "cannot inspect recovery observer {}: {error}",
            observer.display()
        ))),
        Ok(metadata) => {
            ensure(
                metadata.is_dir(),
                "recovery observer must be a real directory",
            )?;
            Ok(true)
        }
    }
}

fn observer_registration(observer: &Path, registration: &J, watch: Option<&J>) -> Result<()> {
    if registration.is_null() {
        return Ok(());
    }
    ensure(
        registration["phase"] == "watching"
            && registration["state_dir"] == json!(observer)
            && registration["pid"].is_u64()
            && watch.is_some_and(|w| w["pid"] == registration["pid"]),
        "registered recovery observer identity is missing or changed",
    )
}

fn observer_binding(env: &TaskEnvironment, root: &Path, observer: &Path) -> Result<()> {
    ensure(
        util::protected::is_runtime_namespace(observer)?,
        "recovery observer namespace is missing",
    )?;
    let owner = json!(crate::task_env::isolation::owner(env));
    ensure(
        util::read_state_json(observer, "lifecycle-owner.json")? == owner,
        "recovery observer namespace Owner changed",
    )?;
    let context = util::read_state_json(observer, "observer.json")?;
    ensure(
        context["owner"] == owner
            && context["coordinator"] == json!(root)
            && context["observer"] == json!(observer),
        "recovery observer registration changed",
    )
}

fn registered_observer(
    env: &TaskEnvironment,
    watch: &J,
    allowed: &mut Vec<ProcessRecord>,
) -> Result<()> {
    let mut identities = Vec::new();
    add_process(watch, "start_ticks", &mut identities)?;
    let identity = &identities[0];
    ensure(
        env.child_processes
            .iter()
            .any(|p| p.pid == identity.pid && p.start_time == identity.start_time),
        "recovery observer process is not registered to this task",
    )?;
    allowed.extend(identities);
    Ok(())
}

fn drained(root: &Path, presentation: Option<J>) -> Result<()> {
    no_pending(root)?;
    if let Some(presentation) = presentation.filter(|v| v["phase"] != "closed") {
        tui::require_retirement_drained(&presentation)?;
    }
    Ok(())
}

fn session_binding(env: &TaskEnvironment, root: &Path, runtime: &J) -> Result<(PathBuf, String)> {
    ensure(
        upstream::selected(runtime),
        "human handoff requires observable upstream execution",
    )?;
    let journal = read(root, "upstream-session.json")?
        .ok_or_else(|| FlowError::guard("upstream creation has no recorded outcome"))?;
    ensure(
        journal["phase"] == "created" && journal["instance"] == env.instance_id,
        "upstream creation is unknown",
    )?;
    Ok((upstream::endpoint(runtime)?, required(&journal, "session")?))
}

fn observe_session(
    env: &TaskEnvironment,
    root: &Path,
    runtime: &J,
    checkout: &Path,
    observation: &mut Observation,
) -> Result<()> {
    let (socket, session) = session_binding(env, root, runtime)?;
    let mut client = crate::codex::rpc::Client::new(&socket.to_string_lossy());
    client.timeout = 3.0;
    let thread = idle(&mut client, &session, Path::new(&required(runtime, "cwd")?))?;
    // One peer inventory per endpoint, including unknown native children.
    if observation.sockets.insert(socket) {
        no_other_writers(&mut client, checkout, env)?;
    }
    observation
        .sessions
        .push(json!({"runtime":root,"session":session,"status":thread["status"]}));
    Ok(())
}

fn no_pending(root: &Path) -> Result<()> {
    if let Some(events) = read(root, "broker-events.json")? {
        ensure(
            events["pending"].as_object().is_some_and(|v| v.is_empty()),
            "native tool request is unresolved",
        )?;
    }
    for file in ["broker-tools.json", "control-tools.json"] {
        if let Some(entries) = read(root, file)? {
            ensure(
                entries.as_object().is_some_and(|m| {
                    m.values()
                        .all(|e| matches!(e["phase"].as_str(), Some("complete" | "responded")))
                }),
                "original control operation is unresolved",
            )?;
        }
    }
    original_turns(root)
}

fn original_turns(root: &Path) -> Result<()> {
    if let Some(turn) = read(root, "turn.json")? {
        ensure(
            turn["phase"] == "signalled",
            "collect and apply the original worker result before human acquisition",
        )?;
    }
    if let Some(record) = read(root, "receiver-start.json")? {
        ensure(
            matches!(record["phase"].as_str(), Some("submitted" | "signalled")),
            "original turn send is unresolved",
        )?;
    }
    Ok(())
}

fn idle(client: &mut crate::codex::rpc::Client, session: &str, checkout: &Path) -> Result<J> {
    let response = client.call(
        "thread/read",
        &json!({"threadId":session,"includeTurns":false}),
    )?;
    let thread = response["thread"].clone();
    ensure(
        thread["id"] == session && thread["cwd"] == json!(checkout),
        "human observation thread identity changed",
    )?;
    ensure(
        thread["status"]["type"] == "idle",
        "original native session is not proven idle",
    )?;
    let terminals = client.call(
        "thread/backgroundTerminals/list",
        &json!({"threadId":session,"limit":1}),
    )?;
    ensure(
        terminals["data"].as_array().is_some_and(Vec::is_empty)
            && terminals["nextCursor"].is_null(),
        "native background command is still active or unobservable",
    )?;
    Ok(thread)
}

fn no_other_writers(
    client: &mut crate::codex::rpc::Client,
    checkout: &Path,
    env: &TaskEnvironment,
) -> Result<()> {
    let known = upstream::obligations(env)?
        .into_iter()
        .filter(|j| j["phase"] == "created")
        .filter_map(|j| j["session"].as_str().map(str::to_owned))
        .collect();
    let mut peers = Peers {
        known,
        seen: BTreeSet::new(),
        deadline: Instant::now() + Duration::from_secs(15),
    };
    let mut cursor = J::Null;
    for _ in 0..8 {
        let response = client.call("thread/loaded/list", &json!({"cursor":cursor,"limit":128}))?;
        peers.page(client, checkout, &response)?;
        let next = &response["nextCursor"];
        if next.is_null() {
            return Ok(());
        }
        ensure(
            next.is_string() && *next != cursor,
            "loaded thread inventory cursor is invalid",
        )?;
        cursor = next.clone();
    }
    Err(FlowError::guard("loaded thread inventory exceeds bound"))
}

struct Peers {
    known: BTreeSet<String>,
    seen: BTreeSet<String>,
    deadline: Instant,
}

impl Peers {
    fn page(
        &mut self,
        client: &mut crate::codex::rpc::Client,
        checkout: &Path,
        response: &J,
    ) -> Result<()> {
        let ids = response["data"]
            .as_array()
            .ok_or_else(|| FlowError::guard("loaded thread inventory is unavailable"))?;
        for id in ids {
            ensure(
                Instant::now() < self.deadline && self.seen.len() < 1024,
                "loaded thread observation budget exceeded",
            )?;
            let id = id
                .as_str()
                .ok_or_else(|| FlowError::guard("invalid loaded thread ID"))?;
            ensure(
                self.seen.insert(id.to_owned()),
                "duplicate loaded thread identity",
            )?;
            loaded_peer(client, id, checkout, &self.known)?;
        }
        Ok(())
    }
}

fn loaded_peer(
    client: &mut crate::codex::rpc::Client,
    id: &str,
    checkout: &Path,
    known: &BTreeSet<String>,
) -> Result<()> {
    let response = client.call("thread/read", &json!({"threadId":id,"includeTurns":false}))?;
    let thread = &response["thread"];
    ensure(thread["id"] == id, "loaded thread identity changed")?;
    let cwd = PathBuf::from(required(thread, "cwd")?);
    if crate::task_env::paths_overlap(&cwd, checkout) {
        ensure(
            known.contains(id),
            "another native session can write the checkout; inspect its ownership before handoff",
        )?;
    }
    Ok(())
}

pub(super) fn settled_recovery(root: &Path) -> Result<()> {
    if let Some(incident) = read(root, "incident.json")? {
        ensure(
            incident["phase"] == "handled",
            "original recovery incident is unresolved; inspect it without replay",
        )?;
    }
    Ok(())
}

fn add_process(record: &J, ticks: &str, allowed: &mut Vec<ProcessRecord>) -> Result<()> {
    let pid = record["pid"]
        .as_u64()
        .and_then(|p| i32::try_from(p).ok())
        .filter(|p| *p > 1)
        .ok_or_else(|| FlowError::guard("registered process PID missing"))?;
    let start = record[ticks]
        .as_u64()
        .or_else(|| record[ticks].as_str().and_then(|v| v.parse().ok()))
        .ok_or_else(|| FlowError::guard("registered process start identity missing"))?;
    allowed.push(ProcessRecord {
        pid,
        start_time: Some(start),
    });
    Ok(())
}

fn add_watch(root: &Path, allowed: &mut Vec<ProcessRecord>) -> Result<()> {
    if let Some(record) = read(root, "watch.json")? {
        add_process(&record, "start_ticks", allowed)?;
    }
    Ok(())
}
