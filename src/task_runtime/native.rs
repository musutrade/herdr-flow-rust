//! Fresh native sessions tied to one activity and an explicit input package.
use super::*;
pub mod receiver;
mod rollout;
use crate::opencode::http::HttpApi;
use crate::{
    codex::rpc::Client,
    identity,
    opencode::session::{Api, SessionAdapter},
    progress, queue,
};

/// The packet contains current requirement references, specifications, an
/// explicit instruction and evidence. No session transcript/fork is accepted.
pub fn packet(queue: &J, wait: &J, instruction: &str, evidence: &[String]) -> Result<J> {
    ensure(
        !instruction.is_empty() && instruction.len() <= 65536,
        "explicit instruction required",
    )?;
    ensure(
        evidence.len() <= 64 && evidence.iter().all(|s| s.len() <= 4096),
        "input evidence exceeds bounds",
    )?;
    let binding = progress::binding(wait)?;
    let task = &queue["tasks"][wait["task"].as_str().unwrap_or_default()];
    progress::lineage::ensure_current(queue, task)?;
    ensure(progress::same(task, &binding), "activity binding changed")?;
    let parent = &queue["tasks"][binding["lineage"]["requirement"]
        .as_str()
        .unwrap_or_default()];
    let revision = binding["lineage"]["revision"].to_string();
    let req = &parent["requirements"][revision];
    ensure(
        req.is_object(),
        "fresh sessions require linked requirements",
    )?;
    Ok(
        json!({"binding":binding,"requirement":req,"instruction":instruction,"evidence":evidence,"acceptance_origin":task["acceptance_origin"],"open_gaps":parent["open_gaps"],"acceptance_protocol":acceptance_protocol(req)}),
    )
}

fn acceptance_protocol(requirement: &J) -> J {
    json!({"role":"acceptance", "report_file":".herdr-verdict.json",
        "instruction":"For acceptance roles, derive checks only from the supplied requirement basis. Write the independent JSON report in report_file inside cwd before finishing. Each source MUST exactly copy one string from requirement.basis.original, changes or specifications; never use a JSON pointer or paraphrase. Each criterion MUST match an ID from requirement.criteria. An execution response is not a verdict. Inspect agreements before declaring ambiguity; do not ask again for existing authorization.",
        "shape":{"conclusion":"passed|failed|unable|ambiguous","scope":"checked scope","checks":[{"id":"check-id","criterion":requirement["criteria"][0]["id"],"source":requirement["basis"]["original"][0],"status":"passed|failed|blocked","evidence":"evidence reference"}],
        "gaps":[{"id":"gap-id","criterion":requirement["criteria"][0]["id"],"source":requirement["basis"]["original"][0],"evidence":"evidence reference","release_condition":"what resolves the gap"}],"reuses":[],"resolutions":[]},
        "unable_fields":["blocker","restore_condition"],"ambiguous_fields":["question","unresolved_after_review","agreements_checked"],"agreement_rule":"agreements_checked must include all basis original/changes/specifications references, basis.confirmation, and every existing authorization source",
        "resolution_shape":{"activity":"previous reviewer activity","gap":"previous gap id","evidence":"release evidence reference","explanation":"why the condition is met"}})
}

/// One create attempt per private runtime. A failed/ambiguous response leaves
/// the journal; recovery inspects it and never creates a replacement implicitly.
pub fn create(env: &TaskEnvironment, root: &Path, spec: &J) -> Result<J> {
    let _permit = crate::task_env::isolation::mutation_permit(env)?;
    let _runtime_permit = crate::lifecycle::runtime_permit(root)?;
    create_owned(env, root, spec)
}
fn create_owned(env: &TaskEnvironment, root: &Path, spec: &J) -> Result<J> {
    let runtime = super::verify(env, root)?;
    let queue_root = PathBuf::from(required(spec, "queue_dir")?);
    crate::task_env::validate_path(env, &queue_root)?;
    let _lock = queue::locked(&queue_root, false)?;
    let _session_lock = crate::guard::locked(root)?;
    validate_unused_runtime(root)?;
    let mut q = queue::load_queue(&queue_root)?;
    let journal = creation_journal(env, &runtime, &q, spec)?;
    finish_creation(root, &mut q, journal)
}

fn validate_unused_runtime(root: &Path) -> Result<()> {
    receiver::ensure_absent(&root.join("receiver.json"))?;
    ensure(
        !root.join("creation.json").exists(),
        "session creation already attempted; reconcile without replay",
    )?;
    Ok(())
}

fn creation_journal(env: &TaskEnvironment, runtime: &J, q: &J, spec: &J) -> Result<J> {
    let wait_id = required(spec, "wait")?;
    let wait = &q["waits"][&wait_id];
    let input = creation_input(env, runtime, q, wait, spec)?;
    let digest = util::pyjson::sha256_hex(serde_json::to_string(&input)?.as_bytes());
    let mut journal = json!({"phase":"creating","instance":env.instance_id,"runtime":runtime,
        "queue_dir":spec["queue_dir"],"queue_id":q["id"],"wait":wait_id,"packet":input,"packet_sha256":digest});
    if let Some(owner) = crate::task_env::instance::owner(env) {
        journal["owner"] = owner;
    }
    journal["lifecycle_owner"] = serde_json::to_value(crate::task_env::isolation::owner(env))?;
    Ok(journal)
}

fn creation_input(env: &TaskEnvironment, runtime: &J, q: &J, wait: &J, spec: &J) -> Result<J> {
    ensure(
        identity::producer(wait)?.is_none(),
        "fresh activity already has a producer",
    )?;
    let input = make_packet(q, wait, spec)?;
    validate_creation_input(env, runtime, q, &input["binding"])?;
    Ok(input)
}

fn validate_creation_input(env: &TaskEnvironment, runtime: &J, q: &J, binding: &J) -> Result<()> {
    progress::environment::runtime_owner(q, env, binding)?;
    verify_snapshot(runtime, binding)?;
    verify_acceptance_workspace(q, runtime, binding)?;
    fresh_report_path(runtime, binding)
}

fn finish_creation(root: &Path, q: &mut J, mut journal: J) -> Result<J> {
    util::atomic_write_json(&root.join("creation.json"), &journal)?;
    let state = create_native(root, &journal["runtime"], &journal)?;
    journal["session"] = state["session"].clone();
    journal["phase"] = json!("created");
    util::atomic_write_json(&root.join("creation.json"), &journal)?;
    bind_created(root, q, &journal, &state)?;
    Ok(state)
}

fn make_packet(q: &J, wait: &J, spec: &J) -> Result<J> {
    let evidence: Vec<String> =
        serde_json::from_value(spec.get("evidence").cloned().unwrap_or(json!([])))?;
    packet(q, wait, &required(spec, "instruction")?, &evidence)
}

fn base_state(root: &Path, runtime: &J, journal: &J) -> J {
    json!({"version":2,"adapter":runtime["adapter"],"state_dir":root,"cwd":runtime["cwd"],
        "model":runtime["model"],"approval_policy":runtime.get("approval_policy").cloned().unwrap_or(json!("untrusted")),"auto_rotate":false,"task":journal["packet"]["binding"]["task"],
        "role":journal["packet"]["binding"]["lineage"]["role"],"server_version":null,
        "endpoint":format!("http://127.0.0.1:{}",runtime["port"]),"phase":"monitoring"})
}

fn create_native(root: &Path, runtime: &J, journal: &J) -> Result<J> {
    let mut state = base_state(root, runtime, journal);
    match runtime["adapter"].as_str() {
        Some("opencode") => create_opencode(&mut state)?,
        Some("codex") => create_codex(root, &mut state)?,
        _ => return Err(FlowError::guard("unsupported native adapter")),
    }
    util::atomic_write_json(&root.join("state.json"), &state)?;
    Ok(state)
}

fn create_opencode(state: &mut J) -> Result<()> {
    let api = HttpApi::from_state(state)?;
    let health = healthy_opencode(&api)?;
    let server_version = crate::opencode::session::health_version(&health)?;
    state["server_version"] = json!(server_version);
    let created = api
        .post(
            "/session",
            &json!({"title":"Independent task activity","model":state["model"]}),
        )
        .map_err(|e| FlowError::guard(format!("native session creation: {e}")))?;
    state["session"] = created["id"].clone();
    SessionAdapter::new(state)?.verify_binding()?;
    let rows = api.get(&format!("/session/{}/message", required(state, "session")?))?;
    ensure(
        rows.as_array().is_some_and(|r| r.is_empty()),
        "new OpenCode session contains prior conversation",
    )
}

pub(super) struct DirectClient {
    rpc: Client,
    _directory: std::fs::File,
}
impl std::ops::Deref for DirectClient {
    type Target = Client;
    fn deref(&self) -> &Client {
        &self.rpc
    }
}
impl std::ops::DerefMut for DirectClient {
    fn deref_mut(&mut self) -> &mut Client {
        &mut self.rpc
    }
}
pub(super) fn direct_client(root: &Path) -> Result<DirectClient> {
    use std::os::fd::AsRawFd;
    let socket = native_socket(root)?;
    let parent = socket
        .parent()
        .ok_or_else(|| FlowError::guard("socket has no directory"))?;
    let directory = std::fs::File::open(parent)?;
    let name = socket
        .file_name()
        .ok_or_else(|| FlowError::guard("socket has no name"))?;
    // A directory fd keeps long task paths below sockaddr_un's length limit.
    let endpoint = format!(
        "/proc/self/fd/{}/{}",
        directory.as_raw_fd(),
        name.to_string_lossy()
    );
    Ok(DirectClient {
        rpc: Client::new(&endpoint),
        _directory: directory,
    })
}
struct NativeClient {
    root: PathBuf,
}
impl NativeClient {
    fn call(&mut self, method: &str, params: &J) -> Result<J> {
        super::broker::call(&self.root, method, params)
    }
    fn call_observed(&mut self, method: &str, params: &J) -> Result<super::broker::Observation> {
        super::broker::call_observed(&self.root, method, params)
    }
}
fn client(root: &Path) -> Result<NativeClient> {
    ensure(
        root.join("broker.sock").exists(),
        "task-owned Codex broker is unavailable; do not recreate the session",
    )?;
    Ok(NativeClient {
        root: root.to_path_buf(),
    })
}

/// Observe the task-owned socket entry, resolving Codex's private /tmp alias.
/// This proves only socket presence, not protocol readiness or delivery.
pub fn socket_present(root: &Path) -> bool {
    use std::os::unix::fs::FileTypeExt;
    native_socket(root)
        .and_then(|path| std::fs::metadata(path).map_err(Into::into))
        .is_ok_and(|metadata| metadata.file_type().is_socket())
}

fn native_socket(root: &Path) -> Result<PathBuf> {
    if let Some(runtime) =
        util::read_state_json_bounded_if_exists(root, "runtime.json", 1024 * 1024)?
    {
        if super::upstream::selected(&runtime) {
            return super::upstream::endpoint(&runtime);
        }
    }
    let socket = root.join("server.sock");
    if !std::fs::symlink_metadata(&socket)?.file_type().is_symlink() {
        return Ok(socket);
    }
    let target = std::fs::read_link(&socket)?;
    let relative = target
        .strip_prefix("/tmp")
        .map_err(|_| FlowError::guard("unexpected Codex socket fallback"))?;
    ensure(
        relative
            .components()
            .all(|v| matches!(v, std::path::Component::Normal(_))),
        "invalid socket fallback",
    )?;
    let resolved = root.join("tmp").join(relative);
    ensure(
        util::is_relative_to(&util::resolve_path(&resolved), &root.join("tmp")),
        "socket escaped private tmp",
    )?;
    Ok(resolved)
}

fn approval_policy(state: &J) -> Result<String> {
    let policy = required(state, "approval_policy")?;
    ensure(
        ["untrusted", "on-request"].contains(&policy.as_str()),
        "unsupported native approval policy",
    )?;
    Ok(policy)
}

fn create_codex(root: &Path, state: &mut J) -> Result<()> {
    let policy = approval_policy(state)?;
    let model = required(&state["model"], "id")?;
    let provider = required(&state["model"], "providerID")?;
    let instructions = if receiver::is_task_owned(state) {
        "Coordinate only the explicitly assigned task. Report evidence and blockers."
    } else {
        "Perform only the supplied task packet. Report evidence and blockers."
    };
    let observed = client(root)?.call_observed("thread/start", &json!({"cwd":state["cwd"],"model":model,
        "modelProvider":provider,"ephemeral":false,"approvalPolicy":policy,"sandbox":"workspace-write",
        "baseInstructions":instructions,
        "developerInstructions":"","config":{"project_doc_max_bytes":0,"memories":{"use_memories":false,"generate_memories":false}},
        "dynamicTools":[],"selectedCapabilityRoots":[],"runtimeWorkspaceRoots":[]}))?;
    let result = observed.result;
    state["session"] = result["thread"]["id"].clone();
    state["transcript"] = result["thread"]["path"].clone();
    verify_created_codex(state, &result, &model, &provider)?;
    state["server_version"] = observed.server_version;
    Ok(())
}

fn verify_created_codex(state: &J, result: &J, model: &str, provider: &str) -> Result<()> {
    ensure(
        result["model"] == model && result["modelProvider"] == provider,
        "Codex selected another model/provider",
    )?;
    verify_thread(state, &result["thread"])?;
    ensure(
        result["thread"]["turns"]
            .as_array()
            .is_some_and(|v| v.is_empty()),
        "new Codex thread has prior turns",
    )
}

fn verify_thread(state: &J, thread: &J) -> Result<()> {
    identity::validate(&json!({"adapter":"codex","session":state["session"]}))?;
    ensure(
        thread["id"] == state["session"] && thread["cwd"] == state["cwd"],
        "Codex thread identity/directory mismatch",
    )?;
    ensure(
        thread["modelProvider"] == state["model"]["providerID"],
        "Codex thread provider changed",
    )
}

fn bind_created(root: &Path, q: &mut J, journal: &J, state: &J) -> Result<()> {
    let id = required(journal, "wait")?;
    q["waits"][&id]["producer"] =
        identity::validate(&json!({"adapter":state["adapter"],"session":state["session"]}))?;
    q["waits"][&id]["runtime"] = json!(root);
    reserve_developer(q, &id, state)?;
    queue::save(Path::new(&required(journal, "queue_dir")?), q)?;
    let mut journal = journal.clone();
    journal["phase"] = json!("bound");
    util::atomic_write_json(&root.join("creation.json"), &journal)
}

fn reserve_developer(q: &mut J, wait_id: &str, state: &J) -> Result<()> {
    let wait = &q["waits"][wait_id];
    if wait["lineage"]["role"] != "development" {
        return Ok(());
    }
    let name = required(wait, "task")?;
    let producer = identity::required_producer(wait)?;
    if !q["tasks"][&name]["writer"].is_null() {
        return ensure(
            q["tasks"][name]["writer"]["producer"] == producer,
            "development writer belongs to another producer",
        );
    }
    let mut writer = progress::reserve_writer(
        q,
        &required(state, "cwd")?,
        &required(wait, "assignment")?,
        &required(state, "session")?,
        &name,
    )?;
    writer["producer"] = producer;
    writer["evidence"] = json!("fresh task-owned native session");
    q["tasks"][name]["writer"] = writer;
    Ok(())
}

/// Rebind only a recorded, verified fresh session; never repeat thread/start or POST /session.
pub fn recover(env: &TaskEnvironment, root: &Path) -> Result<J> {
    let _permit = crate::task_env::isolation::mutation_permit(env)?;
    let _runtime_permit = crate::lifecycle::runtime_permit(root)?;
    super::verify(env, root)?;
    let journal = util::read_state_json(root, "creation.json")?;
    super::verify_owner_record(env, &journal)?;
    let mut state = recovered_state(root, &journal)?;
    recover_binding(env, root, &journal, &mut state)?;
    Ok(state)
}

fn recover_binding(env: &TaskEnvironment, root: &Path, journal: &J, state: &mut J) -> Result<()> {
    let queue_root = PathBuf::from(required(journal, "queue_dir")?);
    crate::task_env::validate_path(env, &queue_root)?;
    let _lock = queue::locked(&queue_root, false)?;
    let mut q = queue::load_queue(&queue_root)?;
    progress::environment::runtime_owner(&q, env, &journal["packet"]["binding"])?;
    verify_queue(&q, journal, state)?;
    bind_created(root, &mut q, journal, state)?;
    refresh_observed_version(root, state)
}

fn refresh_observed_version(root: &Path, state: &mut J) -> Result<()> {
    if state["adapter"] == "opencode" {
        let observed = SessionAdapter::new(state)?.server_version()?;
        state["server_version"] = json!(observed);
    }
    // Codex metadata was observed while verifying this thread on the broker's
    // existing connection; persist only after queue/Owner validation above.
    util::atomic_write_json(&root.join("state.json"), state)
}

fn recovered_state(root: &Path, journal: &J) -> Result<J> {
    ensure(
        matches!(journal["phase"].as_str(), Some("created" | "bound")),
        "creation outcome unknown; inspect dedicated server, do not replay",
    )?;
    let mut state = util::read_state_json(root, "state.json")?;
    ensure(
        state["session"] == journal["session"],
        "recovery session changed",
    )?;
    let version = verify_native(root, &state)?;
    if state["adapter"] == "codex" {
        state["server_version"] = version;
    }
    Ok(state)
}

fn verify_native(root: &Path, state: &J) -> Result<J> {
    if state["adapter"] == "opencode" {
        SessionAdapter::new(state)?
            .verify_binding()
            .map(|_| J::Null)
    } else {
        let response = client(root)?.call_observed(
            "thread/read",
            &json!({"threadId":state["session"],"includeTurns":false}),
        )?;
        verify_thread(state, &response.result["thread"])?;
        Ok(response.server_version)
    }
}

fn verify_queue(q: &J, journal: &J, state: &J) -> Result<()> {
    verify_state(journal, state)?;
    verify_queue_owner(q, journal)?;
    let wait = queue_wait(q, journal)?;
    let task = &q["tasks"][wait["task"].as_str().unwrap_or_default()];
    progress::lineage::ensure_current(q, task)?;
    ensure(
        task["control"] == "running" && task["phase"] != "completed",
        "activity is not running",
    )?;
    verify_producer(wait, state)
}

fn verify_queue_owner(q: &J, journal: &J) -> Result<()> {
    let owner = &journal["owner"];
    ensure(
        owner == &journal["runtime"]["owner"],
        "native runtime owner tuple changed",
    )?;
    if owner.is_null() {
        return Ok(());
    }
    verify_bound_queue_owner(q, journal, owner)
}

fn verify_bound_queue_owner(q: &J, journal: &J, owner: &J) -> Result<()> {
    let binding = &owner["binding"];
    let name = required(binding, "task_id")?;
    let task = &q["tasks"][&name];
    let mapping = &task["environment"];
    let activity = &journal["packet"]["binding"];
    ensure(
        current_owner_task(q, task, binding, &name)
            && current_owner_mapping(mapping, owner)
            && owner_activity(activity, &name),
        "native runtime owner is no longer the current task instance",
    )
}

fn current_owner_task(q: &J, task: &J, binding: &J, name: &str) -> bool {
    q["id"] == binding["queue_id"] && task["task"] == name && task["epoch"] == binding["epoch"]
}

fn current_owner_mapping(mapping: &J, owner: &J) -> bool {
    mapping_binding(mapping, &owner["binding"])
        && mapping["instance_id"] == owner["instance_id"]
        && mapping["phase"] == "active"
}

fn mapping_binding(mapping: &J, binding: &J) -> bool {
    mapping["queue_id"] == binding["queue_id"]
        && mapping["task_id"] == binding["task_id"]
        && mapping["epoch"] == binding["epoch"]
}

fn owner_activity(activity: &J, name: &str) -> bool {
    activity["task"] == name || activity["lineage"]["requirement"] == name
}
fn verify_state(journal: &J, state: &J) -> Result<()> {
    ensure(
        ["adapter", "cwd", "model"]
            .iter()
            .all(|key| state[*key] == journal["runtime"][*key])
            && state["session"] == journal["session"],
        "native session registration changed after creation",
    )
}

fn queue_wait<'a>(q: &'a J, journal: &J) -> Result<&'a J> {
    ensure(
        q["id"] == journal["queue_id"] && q["stopped"] != true,
        "queue changed/stopped",
    )?;
    let wait = &q["waits"][required(journal, "wait")?];
    ensure(
        wait["closed"] != true && progress::binding(wait)? == journal["packet"]["binding"],
        "activity changed/closed",
    )?;
    Ok(wait)
}

fn verify_producer(wait: &J, state: &J) -> Result<()> {
    if let Some(id) = identity::producer(wait)? {
        ensure(
            id == json!({"adapter":state["adapter"],"session":state["session"]}),
            "producer changed",
        )?;
    }
    Ok(())
}

fn fresh_report_path(state: &J, binding: &J) -> Result<()> {
    if binding["lineage"]["role"] == "acceptance" {
        ensure(
            !Path::new(&required(state, "cwd")?)
                .join(".herdr-verdict.json")
                .exists(),
            "fresh acceptance cannot inherit an existing verdict file",
        )?;
    }
    Ok(())
}

fn verify_snapshot(state: &J, binding: &J) -> Result<()> {
    ensure(
        binding["artifact_adapter"] == "git",
        "native task runtime currently supports git snapshots only",
    )?;
    let cwd = required(state, "cwd")?;
    let head = crate::pr::git(Path::new(&cwd), &["rev-parse", "HEAD"])?;
    ensure(
        head == required(binding, "candidate")?,
        "runtime snapshot HEAD changed",
    )?;
    let diff = crate::pr::git(Path::new(&cwd), &["diff", "HEAD", "--"])?;
    ensure(diff.is_empty(), "runtime snapshot tracked contents changed")
}

/// Dispatch the immutable packet once. OpenCode's existing durable collector is
/// reused; Codex retains a turn journal and never resumes an uncertain send.
pub fn dispatch(env: &TaskEnvironment, root: &Path) -> Result<J> {
    let _permit = crate::task_env::isolation::mutation_permit(env)?;
    let _runtime_permit = crate::lifecycle::runtime_permit(root)?;
    super::verify(env, root)?;
    let (journal, state) = dispatch_state(root)?;
    super::verify_owner_record(env, &journal)?;
    dispatch_record(root, &journal, &state)
}

fn dispatch_record(root: &Path, journal: &J, state: &J) -> Result<J> {
    let queue_root = PathBuf::from(required(journal, "queue_dir")?);
    let prompt = dispatch_packet(&queue_root, journal, state)?;
    if state["adapter"] == "opencode" {
        return dispatch_opencode(root, journal, state);
    }
    dispatch_codex_in_queue(root, &queue_root, journal, state, &prompt)
}

fn dispatch_codex_in_queue(
    root: &Path,
    queue_root: &Path,
    journal: &J,
    state: &J,
    prompt: &str,
) -> Result<J> {
    let _queue_lock = queue::locked(queue_root, false)?;
    let q = queue::load_queue(queue_root)?;
    verify_queue(&q, journal, state)?;
    ensure_receiver(queue_root, &q)?;
    dispatch_codex(root, state, journal, prompt)
}
fn dispatch_state(root: &Path) -> Result<(J, J)> {
    let journal = util::read_state_json(root, "creation.json")?;
    ensure(journal["phase"] == "bound", "session is not bound")?;
    let state = util::read_state_json(root, "state.json")?;
    Ok((journal, state))
}

fn dispatch_packet(queue_root: &Path, journal: &J, state: &J) -> Result<String> {
    let q = queue::load_queue(queue_root)?;
    verify_queue(&q, journal, state)?;
    verify_snapshot(state, &journal["packet"]["binding"])?;
    fresh_report_path(state, &journal["packet"]["binding"])?;
    let prompt = serde_json::to_string(&journal["packet"])?;
    ensure(
        util::pyjson::sha256_hex(prompt.as_bytes()) == journal["packet_sha256"],
        "input packet changed",
    )?;
    Ok(prompt)
}
fn dispatch_opencode(root: &Path, journal: &J, state: &J) -> Result<J> {
    let queue_root = PathBuf::from(required(journal, "queue_dir")?);
    ensure_receiver(&queue_root, &queue::load_queue(&queue_root)?)?;
    let _lock = crate::guard::locked(root)?;
    ensure(
        !root.join("dispatch.json").exists(),
        "native dispatch already attempted; reconcile without replay",
    )?;
    util::atomic_write_json(
        &root.join("dispatch.json"),
        &json!({"phase":"sending","session":state["session"],"packet_sha256":journal["packet_sha256"]}),
    )?;
    let path = root.join("input.txt");
    util::atomic_write_bytes(&path, serde_json::to_string(&journal["packet"])?.as_bytes())?;
    // Keep this runtime's guard lock while retrying only pre-submission lock
    // contention. The adapter durably writes submission.json before its POST;
    // once that file exists, even a StateBusy error must never trigger a resend.
    retry_unsent(root, std::time::Duration::from_secs(15), || {
        crate::opencode::session::dispatch(
            root,
            &json!({"queue_dir":journal["queue_dir"],"wait":journal["wait"],"prompt_file":path,"lifecycle_owner":journal["lifecycle_owner"]}),
        )
    })
}

fn retry_unsent(
    root: &Path,
    timeout: std::time::Duration,
    mut send: impl FnMut() -> Result<J>,
) -> Result<J> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        match send() {
            Err(error) if unsent_contention(root, &error, deadline) => {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            result => return result,
        }
    }
}

fn unsent_contention(root: &Path, error: &FlowError, deadline: std::time::Instant) -> bool {
    error.kind == crate::error::ErrorKind::StateBusy
        && std::time::Instant::now() < deadline
        && matches!(std::fs::symlink_metadata(root.join("submission.json")),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound)
}

fn dispatch_codex(root: &Path, state: &J, journal: &J, prompt: &str) -> Result<J> {
    let _lock = crate::guard::locked(root)?;
    ensure(
        !root.join("turn.json").exists(),
        "Codex turn already attempted; reconcile without replay",
    )?;
    verify_native(root, state)?;
    let mut turn = json!({"phase":"sending","session":state["session"],"binding":journal["packet"]["binding"],"created_at":util::now(),"deadline":util::now()+600.0});
    util::atomic_write_json(&root.join("turn.json"), &turn)?;
    let mut connection = client(root)?;
    let result = connection.call(
        "turn/start",
        &json!({"threadId":state["session"],
        "input":[{"type":"text","text":prompt,"text_elements":[]}],"model":state["model"]["id"],
        "sandboxPolicy":{"type":"externalSandbox","networkAccess":"enabled"}}),
    )?;
    turn["turn"] = result["turn"]["id"].clone();
    ensure(
        turn["turn"].as_str().is_some_and(|v| !v.is_empty()),
        "Codex turn identity missing",
    )?;
    turn["phase"] = json!("submitted");
    util::atomic_write_json(&root.join("turn.json"), &turn)?;
    Ok(turn)
}

pub fn collect(env: &TaskEnvironment, root: &Path) -> Result<J> {
    let _permit = crate::task_env::isolation::mutation_permit(env)?;
    let _runtime_permit = crate::lifecycle::runtime_permit(root)?;
    super::verify(env, root)?;
    let creation = util::read_state_json(root, "creation.json")?;
    super::verify_owner_record(env, &creation)?;
    let state = util::read_state_json(root, "state.json")?;
    if state["adapter"] == "opencode" {
        let creation = util::read_state_json(root, "creation.json")?;
        verify_state(&creation, &state)?;
        return crate::opencode::session::collect(root);
    }
    collect_codex(root, &state)
}

fn collect_codex(root: &Path, state: &J) -> Result<J> {
    let creation = util::read_state_json(root, "creation.json")?;
    let queue_root = PathBuf::from(required(&creation, "queue_dir")?);
    let _queue_lock = queue::locked(&queue_root, false)?;
    let _lock = crate::guard::locked(root)?;
    let q = queue::load_queue(&queue_root)?;
    verify_queue(&q, &creation, state)?;
    let mut journal = util::read_state_json(root, "turn.json")?;
    verify_turn(&journal, &creation, state)?;
    observe_codex(root, state, &mut journal)?;
    publish_codex(root, &queue_root, state, &creation, journal)
}
fn verify_turn(journal: &J, creation: &J, state: &J) -> Result<()> {
    ensure(
        journal["session"] == state["session"]
            && journal["binding"] == creation["packet"]["binding"],
        "Codex turn binding changed",
    )?;
    if journal["binding"]["lineage"]["role"] != "development" {
        verify_snapshot(state, &journal["binding"])?;
    }
    Ok(())
}

fn observe_codex(root: &Path, state: &J, journal: &mut J) -> Result<()> {
    let mut connection = client(root)?;
    let response = connection.call(
        "thread/read",
        &json!({"threadId":state["session"],"includeTurns":false}),
    )?;
    verify_thread(state, &response["thread"])?;
    if journal["phase"] == "signalled" {
        return Ok(());
    }
    if approval_pending(&response["thread"]) {
        journal["phase"] = json!("blocked");
        return Ok(());
    }
    let actual = read_codex_turn(root, state, &journal["turn"])?;
    update_codex(journal, actual.as_ref(), state)
}
fn read_codex_turn(root: &Path, state: &J, turn: &J) -> Result<Option<J>> {
    let page = client(root)?.call(
        "thread/turns/list",
        &json!({"threadId":state["session"],"limit":20,"itemsView":"full"}),
    );
    match page {
        Ok(page) => find_turn(&page, turn),
        Err(error) if error.rpc_code == Some(-32601) => {
            if let Some(actual) = super::broker::turn(root, state, turn)? {
                return Ok(Some(actual));
            }
            let runtime = util::read_state_json(root, "runtime.json")?;
            if super::upstream::selected(&runtime) {
                let result = client(root)?.call(
                    "thread/read",
                    &json!({"threadId":state["session"],"includeTurns":true}),
                )?;
                verify_thread(state, &result["thread"])?;
                find_turn(&json!({"data":result["thread"]["turns"]}), turn)
            } else {
                rollout::read(root, state, turn)
            }
        }
        Err(error) => Err(error),
    }
}
fn find_turn(page: &J, turn: &J) -> Result<Option<J>> {
    let turns = page["data"]
        .as_array()
        .ok_or_else(|| FlowError::guard("Codex turn page missing"))?;
    Ok(turns
        .iter()
        .find(|t| !turn.is_null() && t["id"] == *turn)
        .cloned())
}

fn approval_pending(thread: &J) -> bool {
    thread["status"]["activeFlags"]
        .as_array()
        .is_some_and(|flags| {
            flags
                .iter()
                .any(|v| v == "waitingOnApproval" || v == "waitingOnUserInput")
        })
}
fn publish_codex(
    root: &Path,
    queue_root: &Path,
    state: &J,
    creation: &J,
    mut journal: J,
) -> Result<J> {
    util::atomic_write_json(&root.join("turn.json"), &journal)?;
    if journal["outcome"].is_null() {
        return Ok(journal);
    }
    progress::acceptance::native::collect(
        queue_root,
        &required(creation, "wait")?,
        &json!({"adapter":"codex","session":state["session"]}),
        Path::new(&required(state, "cwd")?),
        journal["outcome"].as_str().unwrap_or_default(),
    )?;
    queue::signal_native(
        queue_root,
        &json!({"adapter":"codex","session":state["session"]}),
        &json!({"wait":creation["wait"],"outcome":journal["outcome"],"summary":"Codex turn finished; inspect the bound report","evidence":root.join("turn.json"),"lifecycle_owner":creation["lifecycle_owner"]}),
    )?;
    journal["phase"] = json!("signalled");
    util::atomic_write_json(&root.join("turn.json"), &journal)?;
    Ok(journal)
}

fn update_codex(journal: &mut J, actual: Option<&J>, state: &J) -> Result<()> {
    let Some(turn) = actual else {
        journal["phase"] = json!("unknown");
        return Ok(());
    };
    match turn["status"].as_str() {
        Some("completed") => journal["outcome"] = json!("complete"),
        Some("failed") => journal["outcome"] = json!("failed"),
        Some("interrupted") => journal["outcome"] = json!("decision"),
        _ => {
            journal["phase"] = json!(
                if util::now() >= journal["deadline"].as_f64().unwrap_or(0.0) {
                    "timed_out"
                } else {
                    "pending"
                }
            );
            return Ok(());
        }
    }
    journal["report"] = turn.clone();
    if journal["binding"]["lineage"]["role"] == "development" {
        let cwd = required(state, "cwd")?;
        let head = crate::pr::git(Path::new(&cwd), &["rev-parse", "HEAD"])?;
        journal["output"] = json!({"adapter":"git","id":head,"location":cwd,"input":journal["binding"]["candidate"]});
    }
    Ok(())
}

fn verify_acceptance_workspace(q: &J, runtime: &J, binding: &J) -> Result<()> {
    if binding["lineage"]["role"] != "acceptance" {
        return Ok(());
    }
    let parent = &q["tasks"][binding["lineage"]["requirement"]
        .as_str()
        .unwrap_or_default()];
    let object = &parent["artifacts"][binding["candidate"].as_str().unwrap_or_default()]["object"];
    let source = util::resolve_path(Path::new(&required(object, "location")?));
    let cwd = util::resolve_path(Path::new(&required(runtime, "cwd")?));
    ensure(
        !crate::task_env::paths_overlap(&source, &cwd),
        "acceptance requires a separate snapshot workspace",
    )?;
    if let Some(tasks) = q["tasks"].as_object() {
        for task in tasks.values() {
            if let Some(path) = task["writer"]["workspace"].as_str() {
                ensure(
                    !crate::task_env::paths_overlap(Path::new(path), &cwd),
                    "acceptance workspace has an active writer",
                )?;
            }
        }
    }
    Ok(())
}

fn ensure_receiver(root: &Path, q: &J) -> Result<()> {
    let guard = util::read_state_json(Path::new(&required(q, "guard_dir")?), "state.json")?;
    let ready = crate::runtime::readiness(
        root,
        Some(&required(&guard, "session")?),
        &crate::herdr::receiver_factory,
    )?;
    ensure(
        ready["active_wakeup"] == true,
        "coordinator receiver must be ready before dispatch",
    )
}

fn healthy_opencode(api: &dyn Api) -> Result<J> {
    // The supported OpenCode 1.19.x family can accept TCP before its first HTTP handler is ready.
    // Only this read-only readiness probe is retried, never session creation.
    let mut last = None;
    for _ in 0..3 {
        match api.get("/global/health") {
            Ok(value) => return Ok(value),
            Err(error) => last = Some(error),
        }
    }
    Err(last.unwrap())
}

#[cfg(test)]
mod tests {
    fn bound_owner_fixture() -> (super::J, super::J) {
        let binding = serde_json::json!({"queue_id":"queue","task_id":"task","epoch":7});
        let owner = serde_json::json!({"binding":binding,"instance_id":"instance"});
        let queue = serde_json::json!({"id":"queue","tasks":{"task":{"task":"task","epoch":7,
            "environment":{"queue_id":"queue","task_id":"task","epoch":7,
                "instance_id":"instance","phase":"active"}}}});
        let journal = serde_json::json!({"owner":owner,"runtime":{"owner":owner},
            "packet":{"binding":{"task":"task"}}});
        (queue, journal)
    }

    #[test]
    fn queue_owner_accepts_direct_activity_lineage_and_legacy() {
        let (q, mut journal) = bound_owner_fixture();
        super::verify_queue_owner(&q, &journal).unwrap();
        journal["packet"]["binding"] =
            serde_json::json!({"task":"review","lineage":{"requirement":"task"}});
        super::verify_queue_owner(&q, &journal).unwrap();
        super::verify_queue_owner(&q, &serde_json::json!({})).unwrap();
    }

    #[test]
    fn queue_owner_rejects_stale_queue_task_and_mapping() {
        let (q, journal) = bound_owner_fixture();
        let mut changed = q.clone();
        changed["id"] = serde_json::json!("foreign");
        assert!(super::verify_queue_owner(&changed, &journal).is_err());
        for (key, value) in [
            ("task", serde_json::json!("foreign")),
            ("epoch", serde_json::json!(8)),
        ] {
            let mut changed = q.clone();
            changed["tasks"]["task"][key] = value;
            assert!(
                super::verify_queue_owner(&changed, &journal).is_err(),
                "accepted task {key}"
            );
        }
        for (key, value) in [
            ("queue_id", serde_json::json!("foreign")),
            ("task_id", serde_json::json!("foreign")),
            ("epoch", serde_json::json!(8)),
            ("instance_id", serde_json::json!("replacement")),
            ("phase", serde_json::json!("creating")),
        ] {
            let mut changed = q.clone();
            changed["tasks"]["task"]["environment"][key] = value;
            assert!(
                super::verify_queue_owner(&changed, &journal).is_err(),
                "accepted mapping {key}"
            );
        }
    }

    #[test]
    fn queue_owner_rejects_journal_replacement_and_foreign_activity() {
        let (q, journal) = bound_owner_fixture();
        let mut changed = journal.clone();
        changed["owner"]["instance_id"] = serde_json::json!("replacement");
        assert!(super::verify_queue_owner(&q, &changed).is_err());
        let mut changed = journal.clone();
        changed["owner"]["binding"]["task_id"] = serde_json::Value::Null;
        changed["runtime"]["owner"] = changed["owner"].clone();
        assert!(super::verify_queue_owner(&q, &changed).is_err());
        let mut changed = journal;
        changed["packet"]["binding"] = serde_json::json!({"task":"foreign"});
        assert!(super::verify_queue_owner(&q, &changed).is_err());
    }
    #[test]
    fn dispatch_retries_only_before_the_submission_journal() {
        use crate::error::ErrorKind;
        use std::time::Duration;
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("dispatch.json"), "sending").unwrap();
        let mut calls = 0;
        let value = retry_unsent(dir.path(), Duration::from_secs(1), || {
            calls += 1;
            if calls == 1 {
                return Err(FlowError::new(ErrorKind::StateBusy, "queue busy"));
            }
            std::fs::write(dir.path().join("submission.json"), "sending").unwrap();
            Ok(json!({"sent":true}))
        })
        .unwrap();
        assert_eq!(value["sent"], true);
        assert_eq!(calls, 2);
        assert_eq!(
            std::fs::read(dir.path().join("dispatch.json")).unwrap(),
            b"sending"
        );
        calls = 0;
        assert!(retry_unsent(dir.path(), Duration::from_secs(1), || {
            calls += 1;
            Err(FlowError::new(
                ErrorKind::StateBusy,
                "busy after possible send",
            ))
        })
        .is_err());
        assert_eq!(calls, 1);
        std::fs::remove_file(dir.path().join("submission.json")).unwrap();
        calls = 0;
        assert!(retry_unsent(dir.path(), Duration::from_secs(1), || {
            calls += 1;
            Err(FlowError::io("transport outcome unknown"))
        })
        .is_err());
        assert_eq!(calls, 1);
        calls = 0;
        assert!(retry_unsent(dir.path(), Duration::ZERO, || {
            calls += 1;
            Err(FlowError::new(ErrorKind::StateBusy, "queue busy"))
        })
        .is_err());
        assert_eq!(calls, 1);
        assert_eq!(
            std::fs::read(dir.path().join("dispatch.json")).unwrap(),
            b"sending"
        );
    }

    use super::*;
    #[test]
    fn native_development_claims_one_writer_and_recovers_only_that_identity() {
        let dir = tempfile::tempdir().unwrap();
        let mut q = json!({"tasks":{"d":{"writer":null}},"waits":{"w":{"task":"d","assignment":"a","lineage":{"role":"development"},"producer":{"adapter":"opencode","session":"sesdev"}}}});
        let state = json!({"cwd":dir.path(),"session":"sesdev"});
        reserve_developer(&mut q, "w", &state).unwrap();
        assert_eq!(q["tasks"]["d"]["writer"]["producer"]["adapter"], "opencode");
        reserve_developer(&mut q, "w", &state).unwrap();
        q["tasks"]["d"]["writer"]["producer"]["session"] = json!("other");
        assert!(reserve_developer(&mut q, "w", &state).is_err());
        assert_eq!(
            find_turn(
                &json!({"data":[{"id":"t","status":"completed"}]}),
                &json!("t")
            )
            .unwrap()
            .unwrap()["status"],
            "completed"
        );
        assert!(find_turn(&json!({"data":[]}), &json!("t"))
            .unwrap()
            .is_none());
        assert!(find_turn(&json!({}), &json!("t")).is_err());
    }
    #[test]
    fn codex_partial_blocked_timeout_and_terminal_outcomes_are_distinct() {
        let state = json!({"cwd":"/unused"});
        for (status, outcome) in [
            ("completed", "complete"),
            ("failed", "failed"),
            ("interrupted", "decision"),
        ] {
            let mut j = json!({"deadline":util::now()+60.0});
            update_codex(&mut j, Some(&json!({"status":status,"items":[]})), &state).unwrap();
            assert_eq!(j["outcome"], outcome);
        }
        let mut j = json!({"deadline":util::now()+60.0});
        update_codex(&mut j, None, &state).unwrap();
        assert_eq!(j["phase"], "unknown");
        update_codex(&mut j, Some(&json!({"status":"inProgress"})), &state).unwrap();
        assert_eq!(j["phase"], "pending");
        assert!(j["outcome"].is_null());
        j["deadline"] = json!(0);
        update_codex(&mut j, Some(&json!({"status":"inProgress"})), &state).unwrap();
        assert_eq!(j["phase"], "timed_out");
        assert!(approval_pending(
            &json!({"status":{"activeFlags":["waitingOnApproval"]}})
        ));
        assert!(approval_pending(
            &json!({"status":{"activeFlags":["waitingOnUserInput"]}})
        ));
        assert!(!approval_pending(&json!({"status":{"activeFlags":[]}})));
    }
    #[test]
    fn private_tmp_socket_mapping_rejects_escape_and_keeps_long_paths_usable() {
        use std::os::unix::{fs::symlink, net::UnixListener};
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let target = root.join("tmp/codex-daemon-1000");
        assert!(!socket_present(root));
        std::fs::create_dir_all(&target).unwrap();
        let _socket = UnixListener::bind(target.join("socket")).unwrap();
        symlink("/tmp/codex-daemon-1000/socket", root.join("server.sock")).unwrap();
        assert_eq!(native_socket(root).unwrap(), target.join("socket"));
        assert!(socket_present(root));
        assert!(direct_client(root).is_ok());
        std::fs::remove_file(root.join("server.sock")).unwrap();
        symlink("/tmp/../outside", root.join("server.sock")).unwrap();
        assert!(native_socket(root).is_err());
        assert!(!socket_present(root));
        std::fs::remove_file(root.join("server.sock")).unwrap();
        symlink("/outside", root.join("server.sock")).unwrap();
        assert!(native_socket(root).is_err());
        assert!(!socket_present(root));
        std::fs::remove_file(root.join("server.sock")).unwrap();
        std::fs::write(root.join("server.sock"), "not a socket").unwrap();
        assert!(!socket_present(root));
    }
}
