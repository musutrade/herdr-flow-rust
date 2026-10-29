//! Task connections to an existing upstream service. No account files are read,
//! copied or rewritten, and the shared server is never registered as our child.
use super::*;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
mod descendants;
pub(in crate::task_runtime) mod subscription;
pub(crate) use descendants::discover;
pub(crate) use descendants::discover_bounded;

pub(super) fn selected(spec: &J) -> bool {
    spec["configuration"] == "upstream"
}

pub(super) fn socket_identity(path: &Path) -> Result<J> {
    ensure(path.is_absolute(), "upstream socket must be absolute")?;
    let meta = std::fs::symlink_metadata(path)?;
    ensure(
        meta.file_type().is_socket()
            && meta.uid() == util::process::current_uid()
            && meta.mode() & 0o022 == 0,
        "upstream endpoint must be a same-user non-writable Unix socket",
    )?;
    ensure(
        std::fs::canonicalize(path)? == path,
        "upstream socket must be canonical",
    )?;
    Ok(json!({"path":path,"dev":meta.dev(),"ino":meta.ino(),"uid":meta.uid()}))
}

pub(super) fn endpoint(runtime: &J) -> Result<PathBuf> {
    let expected = &runtime["upstream_socket"];
    let path = PathBuf::from(required(expected, "path")?);
    ensure(
        socket_identity(&path)? == *expected,
        "upstream socket identity changed",
    )?;
    Ok(path)
}

/// Attach only in the independent lifecycle-managed queue. Only the local
/// connection broker becomes a task process; the existing server stays external.
pub(super) fn attach(
    env: &mut TaskEnvironment,
    root: &Path,
    spec: &J,
    permit: Option<&crate::lifecycle::Permit>,
) -> Result<Child> {
    validate_attachment(env, root, spec)?;
    let socket = selected_endpoint(env, spec)?;
    let mut record = starting_record(env, root, spec)?;
    record["upstream_socket"] = socket;
    record["service_owner"] = json!("upstream");
    record["configuration"] = json!("upstream");
    super::tui::configure(&mut record)?;
    util::atomic_write_json(&root.join("runtime.json"), &record)?;
    let child = broker_child(env, root, &record, permit)?;
    finish_attachment(root, record, child)
}
pub(super) fn finish_attachment(root: &Path, mut record: J, child: Child) -> Result<Child> {
    record["pid"] = json!(child.id());
    record["start_time"] = json!(crate::task_env::process_start_time(child.id() as i32));
    record["phase"] = json!("started");
    util::atomic_write_json(&root.join("runtime.json"), &record)?;
    Ok(child)
}

fn validate_attachment(env: &TaskEnvironment, root: &Path, spec: &J) -> Result<()> {
    ensure(
        env.creation.is_some(),
        "upstream attachment requires an isolated queue instance",
    )?;
    validate_spec(spec)?;
    verify_instance_owner(env)?;
    crate::task_env::validate_path(env, root)?;
    let cwd = PathBuf::from(required(spec, "cwd")?);
    validate_launch_cwd(env, root, &cwd, spec)
}
fn selected_endpoint(env: &TaskEnvironment, spec: &J) -> Result<J> {
    let socket = PathBuf::from(required(spec, "socket")?);
    ensure(
        !crate::task_env::paths_overlap(&socket, &env.temp_root)
            && !crate::task_env::paths_overlap(&socket, &env.workspace_root),
        "shared server must be outside disposable task roots",
    )?;
    socket_identity(&socket)
}

fn validate_spec(spec: &J) -> Result<()> {
    let fields = spec
        .as_object()
        .ok_or_else(|| FlowError::guard("upstream spec must be an object"))?;
    ensure(
        fields.keys().all(|key| {
            [
                "adapter",
                "configuration",
                "socket",
                "cwd",
                "model",
                "role_models",
                "driver_binary",
                "approval_policy",
                "review_snapshot",
                "native_tui",
                "recovery_role",
                "publication_role",
                "publication",
            ]
            .contains(&key.as_str())
        }),
        "unsupported upstream launch field; account configuration is not accepted",
    )?;
    role_models::validate_model(&spec["model"])?;
    role_models::validate(spec)
}

/// Persist creation intent before sending, and restrict all later operations to
/// the exact response identity. A missing response never authorizes a retry.
pub(super) fn before(root: &Path, method: &str, params: &J) -> Result<()> {
    let runtime = util::read_state_json(root, "runtime.json")?;
    if !selected(&runtime) {
        return Ok(());
    }
    endpoint(&runtime)?;
    if method == "thread/resume" {
        ensure(params == &json!({"threadId":params["threadId"]}),
            "broker resume only rejoins the recorded thread; configuration overrides are not allowed")?;
    }
    if method == "turn/start" {
        subscription::validate_turn(&runtime, params)?;
    }
    if method == "thread/start" {
        reserve_creation(root, &runtime, params)
    } else {
        validate_request(root, &runtime, params)
    }
}
fn reserve_creation(root: &Path, runtime: &J, params: &J) -> Result<()> {
    let _journal = session_lock(root)?;
    let path = root.join("upstream-session.json");
    native::receiver::ensure_absent(&path)?;
    ensure(
        params["cwd"] == runtime["cwd"],
        "upstream creation cwd changed",
    )?;
    util::atomic_write_json(
        &path,
        &json!({"phase":"creating","instance":runtime["instance"],
        "runtime":runtime,"session":null}),
    )
}
fn validate_request(root: &Path, runtime: &J, params: &J) -> Result<()> {
    let journal = util::read_state_json(root, "upstream-session.json")?;
    ensure(
        journal["phase"] == "created"
            && journal["instance"] == runtime["instance"]
            && !journal["session"].is_null()
            && params["threadId"] == journal["session"],
        "upstream request does not belong to the recorded task thread",
    )
}

pub(super) fn after(root: &Path, method: &str, result: &J) -> Result<()> {
    if method != "thread/start" {
        return Ok(());
    }
    let runtime = util::read_state_json(root, "runtime.json")?;
    if !selected(&runtime) {
        return Ok(());
    }
    crate::identity::validate(&json!({"adapter":"codex","session":result["thread"]["id"]}))?;
    let _journal = session_lock(root)?;
    let mut journal = util::read_state_json(root, "upstream-session.json")?;
    seed_presentation_intent(&runtime, &mut journal);
    // Register even a response whose cwd/model validation will later fail: that
    // newly created thread still has to be reclaimed.
    journal["session"] = result["thread"]["id"].clone();
    journal["phase"] = json!("created");
    journal["session_configuration"] = subscription::configuration(result);
    util::atomic_write_json(&root.join("upstream-session.json"), &journal)
}

// Fresh creation publishes this proof with its successful session response.
// A replay/prepare of an already-created journal never backfills the proof.
fn seed_presentation_intent(runtime: &J, journal: &mut J) {
    if journal["phase"] == "creating"
        && journal.get("presentation_intent").is_none()
        && runtime["recovery_role"] == true
        && runtime.get("native_tui").is_some()
    {
        journal["presentation_intent"] = json!({"version":1,"phase":"not_requested"});
    }
}

/// Session metadata writers share the existing runtime lock. Never hold this
/// lock across native RPC or take a presentation lock while it is held.
pub(in crate::task_runtime) fn session_lock(root: &Path) -> Result<util::FileLock> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    loop {
        if let Some(lock) = util::FileLock::acquire(&root.join("runtime.lock"), false)? {
            return Ok(lock);
        }
        if std::time::Instant::now() >= deadline {
            return Err(FlowError::new(crate::ErrorKind::StateBusy, "upstream session metadata lock is busy; inspect the original operation without replay"));
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

/// Freeze obligations while the task's exclusive lifecycle gate is held. The
/// cleanup plan retains them independently of the disposable runtime journals.
pub(crate) fn obligations(env: &TaskEnvironment) -> Result<Vec<J>> {
    let directory = env.temp_root.join("rt");
    if !directory.try_exists()? {
        return Ok(vec![]);
    }
    let mut out = vec![];
    for (index, entry) in std::fs::read_dir(directory)?.enumerate() {
        ensure(index < 128, "runtime cleanup scan exceeded bound")?;
        if let Some(journal) = obligation(env, &entry?.path())? {
            out.push(journal);
        }
    }
    Ok(out)
}
fn obligation(env: &TaskEnvironment, root: &Path) -> Result<Option<J>> {
    ensure(
        std::fs::symlink_metadata(root)?.is_dir(),
        "invalid runtime directory",
    )?;
    let runtime = util::read_state_json_bounded_if_exists(root, "runtime.json", 1024 * 1024)?;
    match runtime.filter(selected) {
        Some(runtime) => bound_obligation(env, root, &runtime),
        None => Ok(None),
    }
}
fn bound_obligation(env: &TaskEnvironment, root: &Path, runtime: &J) -> Result<Option<J>> {
    verify_owner_record(env, runtime)?;
    ensure(
        runtime["instance"] == env.instance_id,
        "upstream cleanup instance changed",
    )?;
    let journal =
        util::read_state_json_bounded_if_exists(root, "upstream-session.json", 1024 * 1024)?;
    if let Some(journal) = &journal {
        ensure(
            journal["instance"] == env.instance_id
                && journal["runtime"]["upstream_socket"] == runtime["upstream_socket"],
            "upstream obligation binding changed",
        )?;
    }
    Ok(journal)
}

/// Delete only IDs recorded from this task's thread/start response. A transport
/// error or an ambiguous creation retains the obligation and all local evidence.
pub(crate) fn reclaim(journal: &mut J) -> Result<()> {
    reclaim_before(journal, None)
}

/// Reclaim one recorded session within one budget shared by connection setup
/// and deletion. The shared service remains external and is never restarted.
pub(crate) fn reclaim_bounded(journal: &mut J, timeout: f64) -> Result<()> {
    let duration = std::time::Duration::try_from_secs_f64(timeout)
        .map_err(|_| FlowError::guard("invalid upstream reclamation timeout"))?;
    let deadline = upstream_deadline(duration)?;
    reclaim_before(journal, Some(deadline))
}

fn reclaim_before(journal: &mut J, deadline: Option<std::time::Instant>) -> Result<()> {
    if journal["phase"] == "deleted" {
        return Ok(());
    }
    ensure(
        journal["phase"] == "created" || journal["phase"] == "deleting",
        "upstream creation outcome unknown; inspect without replay or releasing resources",
    )?;
    let id = required(journal, "session")?;
    crate::identity::validate(&json!({"adapter":"codex","session":id}))?;
    let socket = endpoint(&journal["runtime"])?;
    let mut client = crate::codex::rpc::Client::new(&socket.to_string_lossy());
    reclaim_with(journal, &id, &mut |method, params| match deadline {
        Some(deadline) => call_before(&mut client, method, params, deadline),
        None => client.call(method, params),
    })
}

fn upstream_deadline(timeout: std::time::Duration) -> Result<std::time::Instant> {
    std::time::Instant::now()
        .checked_add(timeout)
        .ok_or_else(|| FlowError::guard("invalid upstream deadline"))
}

fn remaining_budget(deadline: std::time::Instant) -> Result<f64> {
    let remaining = deadline.saturating_duration_since(std::time::Instant::now());
    if remaining.is_zero() {
        return Err(FlowError::rpc_timeout(
            "upstream retirement deadline elapsed",
        ));
    }
    Ok(remaining.as_secs_f64())
}

fn call_before(
    client: &mut crate::codex::rpc::Client,
    method: &str,
    params: &J,
    deadline: std::time::Instant,
) -> Result<J> {
    client.timeout = remaining_budget(deadline)?;
    client.connect()?;
    // Client::call starts its own RPC clock after connecting. Recalculate the
    // remaining budget so initialization never grants another full allowance.
    client.timeout = remaining_budget(deadline)?;
    let reply = client.call(method, params)?;
    remaining_budget(deadline)?;
    Ok(reply)
}

fn reclaim_with(
    journal: &mut J,
    id: &str,
    call: &mut dyn FnMut(&str, &J) -> Result<J>,
) -> Result<()> {
    let params = json!({"threadId":id});
    match call("thread/delete", &params) {
        Ok(_) => (),
        Err(error)
            if journal["phase"] == "deleting"
                && error.rpc_code == Some(-32600)
                && error.message == format!("thread not found: {id}") => {}
        Err(error) => return Err(error),
    }
    journal["phase"] = json!("deleted");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        os::unix::{
            fs::{symlink, PermissionsExt},
            net::UnixListener,
        },
    };
    const ID: &str = "11111111-1111-1111-1111-111111111111";

    fn fixture() -> (tempfile::TempDir, UnixListener, J) {
        let temp = crate::util::fs::test_tempdir();
        let socket = temp.path().join("shared.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        fs::set_permissions(&socket, fs::Permissions::from_mode(0o600)).unwrap();
        let runtime = json!({"configuration":"upstream","instance":"instance", "cwd":"/task",
            "upstream_socket":socket_identity(&socket).unwrap()});
        util::atomic_write_json(&temp.path().join("runtime.json"), &runtime).unwrap();
        (temp, listener, runtime)
    }

    #[test]
    fn creation_is_reserved_before_rpc_and_foreign_threads_are_rejected() {
        let (temp, _socket, runtime) = fixture();
        let root = temp.path();
        assert!(before(root, "thread/start", &json!({"cwd":"/other"})).is_err());
        assert!(!root.join("upstream-session.json").exists());
        before(root, "thread/start", &json!({"cwd":"/task"})).unwrap();
        assert!(before(root, "thread/start", &json!({"cwd":"/task"})).is_err());
        assert!(before(root, "turn/start", &json!({"threadId":ID})).is_err());
        let mut unknown = util::read_json(&root.join("upstream-session.json")).unwrap();
        assert!(reclaim(&mut unknown).is_err());
        after(
            root,
            "thread/start",
            &json!({"thread":{"id":ID,"cwd":"/unexpected"}}),
        )
        .unwrap();
        before(root, "thread/read", &json!({"threadId":ID})).unwrap();
        assert!(before(
            root,
            "thread/resume",
            &json!({"threadId":ID,"model":"other"})
        )
        .is_err());
        for id in [J::Null, json!("foreign")] {
            assert!(before(root, "turn/start", &json!({"threadId":id})).is_err());
        }
        assert_eq!(
            util::read_json(&root.join("upstream-session.json")).unwrap()["runtime"],
            runtime
        );
        assert!(before(root, "thread/start", &json!({"cwd":"/task"})).is_err());
    }

    #[test]
    fn presentation_proof_is_fresh_recovery_only_and_never_backfilled() {
        for (role, expected) in [
            ("recovery_role", true),
            ("publication_role", false),
            ("ordinary", false),
        ] {
            let (temp, _socket, mut runtime) = fixture();
            runtime[role] = json!(true);
            runtime["native_tui"] = json!({});
            util::atomic_write_json(&temp.path().join("runtime.json"), &runtime).unwrap();
            before(temp.path(), "thread/start", &json!({"cwd":"/task"})).unwrap();
            after(temp.path(), "thread/start", &json!({"thread":{"id":ID}})).unwrap();
            let mut journal = util::read_state_json(temp.path(), "upstream-session.json").unwrap();
            assert_eq!(journal.get("presentation_intent").is_some(), expected);
            journal
                .as_object_mut()
                .unwrap()
                .remove("presentation_intent");
            util::atomic_write_json(&temp.path().join("upstream-session.json"), &journal).unwrap();
            after(temp.path(), "thread/start", &json!({"thread":{"id":ID}})).unwrap();
            assert!(util::read_state_json(temp.path(), "upstream-session.json")
                .unwrap()
                .get("presentation_intent")
                .is_none());
        }
    }

    #[test]
    fn busy_creation_response_keeps_original_unknown_without_replay() {
        let (temp, _socket, _) = fixture();
        before(temp.path(), "thread/start", &json!({"cwd":"/task"})).unwrap();
        let _lock = util::FileLock::acquire(&temp.path().join("runtime.lock"), false)
            .unwrap()
            .unwrap();
        let error = after(temp.path(), "thread/start", &json!({"thread":{"id":ID}})).unwrap_err();
        assert!(error.to_string().contains("without replay"), "{error}");
        let journal = util::read_state_json(temp.path(), "upstream-session.json").unwrap();
        assert_eq!(journal["phase"], "creating");
        assert_eq!(journal["session"], J::Null);
    }

    #[test]
    fn socket_replacement_and_links_never_rebind_the_shared_service() {
        let (temp, _listener, runtime) = fixture();
        let socket = temp.path().join("shared.sock");
        endpoint(&runtime).unwrap();
        fs::rename(&socket, temp.path().join("original.sock")).unwrap();
        let _replacement = UnixListener::bind(&socket).unwrap();
        fs::set_permissions(&socket, fs::Permissions::from_mode(0o600)).unwrap();
        assert!(endpoint(&runtime).is_err());
        fs::remove_file(&socket).unwrap();
        symlink(temp.path().join("original.sock"), &socket).unwrap();
        assert!(endpoint(&runtime).is_err());
        assert!(temp.path().join("original.sock").exists());
    }

    #[test]
    fn cleanup_preserves_uncertainty_and_only_deletes_the_recorded_identity() {
        let mut journal = json!({"phase":"deleting","session":ID});
        reclaim_with(&mut journal, ID, &mut |method, params| {
            assert_eq!(method, "thread/delete");
            assert_eq!(params, &json!({"threadId":ID}));
            Err(FlowError::rpc("disconnected"))
        })
        .unwrap_err();
        assert_eq!(journal["phase"], "deleting");
        reclaim_with(&mut journal, ID, &mut |_, _| {
            Err(FlowError::rpc(format!("thread not found: {ID}")).with_rpc(Some(-32600), None))
        })
        .unwrap();
        assert_eq!(journal["phase"], "deleted");
        for message in ["method not found", "thread not found: other"] {
            journal["phase"] = json!("deleting");
            assert!(reclaim_with(&mut journal, ID, &mut |_, _| {
                Err(FlowError::rpc(message).with_rpc(Some(-32600), None))
            })
            .is_err());
        }
        reclaim_with(&mut journal, ID, &mut |_, _| Ok(json!({}))).unwrap();
        assert_eq!(journal["phase"], "deleted");
    }

    #[test]
    fn bounded_reclamation_times_out_during_handshake_without_losing_the_obligation() {
        let (_temp, _listener, runtime) = fixture();
        // A live listener deliberately never services the WebSocket handshake.
        // The bounded API must not inherit the default ten-second setup wait.
        let mut journal = json!({"phase":"deleting","session":ID,"runtime":runtime});
        let start = std::time::Instant::now();
        let error = reclaim_bounded(&mut journal, 0.03).unwrap_err();
        assert_eq!(error.kind, crate::ErrorKind::RpcTimeout);
        assert!(start.elapsed() < std::time::Duration::from_secs(1));
        assert_eq!(journal["phase"], "deleting");
        for timeout in [-1.0, f64::NAN, f64::INFINITY] {
            assert!(reclaim_bounded(&mut journal, timeout).is_err());
        }
    }
}
