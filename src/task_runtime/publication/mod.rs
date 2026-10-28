//! A separately configured publisher with task-owned, asynchronous observation.
//! The model requests typed actions; only host checks establish remote facts.
use super::*;
use crate::{lifecycle::Owner, queue};
mod agent;
mod drive;
#[cfg(test)]
mod tests;
mod transport;

const LIMIT: usize = 1024 * 1024;

fn read(root: &Path, name: &str) -> Result<J> {
    util::read_state_json_bounded(root, name, LIMIT)
}

fn save(root: &Path, name: &str, value: &J) -> Result<()> {
    ensure(
        serde_json::to_vec(value)?.len() <= LIMIT,
        "publication record exceeds bound",
    )?;
    util::atomic_write_json(&root.join(name), value)
}

/// A repeated call can finish a local preparation prefix, never resend a turn
/// or a remote mutation. All such sends have their own durable attempt record.
pub fn start(env: &mut TaskEnvironment, coordinator: &Path, spec: &J) -> Result<J> {
    let _permit = crate::task_env::isolation::mutation_permit(env)?;
    let source = super::verify(env, coordinator)?;
    validate_start(&source, spec)?;
    if let Some(context) = current_context(env, coordinator, spec)? {
        initialize(env, &context)?;
    } else {
        start_new(env, coordinator, spec, &source)?;
    }
    status(coordinator)
}

fn validate_start(source: &J, spec: &J) -> Result<()> {
    ensure(
        upstream::selected(source),
        "publisher requires the existing upstream service",
    )?;
    role_models::validate_model(&source["role_models"]["publisher"])?;
    let fields = spec
        .as_object()
        .ok_or_else(|| FlowError::guard("publication spec must be an object"))?;
    ensure(
        fields.keys().all(|k| {
            [
                "state_version",
                "authorization",
                "publication_authorization",
                "merge_authorization",
                "issue",
                "title",
                "body",
            ]
            .contains(&k.as_str())
        }),
        "unsupported publication field",
    )?;
    for key in ["title", "body"] {
        let text = required(spec, key)?;
        ensure(text.len() <= 65536, "publication text exceeds bound")?;
    }
    Ok(())
}

fn current_context(env: &TaskEnvironment, coordinator: &Path, spec: &J) -> Result<Option<J>> {
    let current =
        util::read_state_json_bounded_if_exists(coordinator, "publication-current.json", LIMIT)?;
    let Some(current) = current else {
        ensure(env.delivery.is_null(), "another delivery is active")?;
        return Ok(None);
    };
    let owner = crate::task_env::isolation::owner(env)
        .ok_or_else(|| FlowError::guard("publisher Owner missing"))?;
    if env.delivery.is_null() && prior_stage(&owner, &current)? {
        return Ok(None);
    }
    validate_current(&current, spec, &env.delivery)?;
    owned_context(&current, coordinator, &owner).map(Some)
}

fn owned_context(current: &J, coordinator: &Path, owner: &Owner) -> Result<J> {
    let root = PathBuf::from(required(current, "root")?);
    let context = read(&root, "publisher.json")?;
    ensure(
        context["owner"] == json!(owner) && context["coordinator"] == json!(coordinator),
        "publication Owner changed",
    )?;
    Ok(context)
}

fn prior_stage(owner: &Owner, current: &J) -> Result<bool> {
    let state = snapshot(owner)?;
    Ok(state["tasks"][&owner.task_id]["stage_delivery"]["delivery"]["id"] == current["id"])
}

fn validate_current(current: &J, spec: &J, delivery: &J) -> Result<()> {
    ensure(
        current["input"] == *spec,
        "another publication is active; inspect its original input",
    )?;
    ensure(
        delivery.is_null() || current["id"] == delivery["id"],
        "another delivery is active; inspect its existing operation",
    )
}

fn start_new(env: &mut TaskEnvironment, coordinator: &Path, spec: &J, source: &J) -> Result<()> {
    let id = util::uuid_hex();
    let root = super::prepare(env, &format!("publish-{id}"))?;
    let owner = crate::task_env::isolation::owner(env)
        .ok_or_else(|| FlowError::guard("publisher requires an isolated Owner"))?;
    let request = request(env, spec, source, &owner, &id)?;
    let context = json!({"owner":owner,"coordinator":coordinator,"root":root,"request":request,"source":source});
    save(&root, "publisher.json", &context)?;
    save(
        coordinator,
        "publication-current.json",
        &json!({"id":id,"root":root,"input":spec}),
    )?;
    initialize(env, &context)
}

fn initialize(env: &mut TaskEnvironment, context: &J) -> Result<()> {
    let root = PathBuf::from(required(context, "root")?);
    let _lock = util::FileLock::acquire(&root.join("publication.lock"), false)?
        .ok_or_else(|| FlowError::new(crate::ErrorKind::StateBusy, "publication is updating"))?;
    let saved = util::read_state_json_bounded_if_exists(&root, "operation.json", LIMIT)?;
    if saved.as_ref().is_some_and(|op| op["phase"] != "preparing") {
        return resume_prepared(env, &root, context, saved.as_ref().unwrap());
    }
    initialize_bound(env, &root, context)
}

fn resume_prepared(
    env: &mut TaskEnvironment,
    root: &Path,
    context: &J,
    operation: &J,
) -> Result<()> {
    if operation["phase"] != "planned" || !unsent_watch(root)? {
        return Ok(());
    }
    ensure(
        read(root, "publisher-agent.json")?["phase"] == "ready",
        "prepared publisher session is unavailable",
    )?;
    agent::prepare(env, root, context)?;
    bind_producer(root, context)?;
    launch_watch(env, root, context)
}

fn unsent_watch(root: &Path) -> Result<bool> {
    if util::read_state_json_bounded_if_exists(root, "watch-start.json", LIMIT)?.is_some() {
        return Ok(false);
    }
    native::receiver::ensure_absent(&root.join("watch.json"))?;
    Ok(true)
}

fn initialize_bound(env: &mut TaskEnvironment, root: &Path, context: &J) -> Result<()> {
    prepare_delivery(env, root, context)?;
    agent::prepare(env, root, context)?;
    bind_producer(root, context)?;
    let planned = json!({"phase":"planned","delivery":context["request"]["id"]});
    save(root, "operation.json", &planned)?;
    // Startup is itself at most once. If the response is lost, inspect the
    // original registered watcher instead of launching a second supervisor.
    resume_prepared(env, root, context, &planned)
}

fn launch_watch(env: &mut TaskEnvironment, root: &Path, context: &J) -> Result<()> {
    save(root, "watch-start.json", &json!({"phase":"sending"}))?;
    let pid = spawn(env, root, &context["source"])?;
    save(
        root,
        "watch-start.json",
        &json!({"phase":"started","pid":pid}),
    )
}

fn prepare_delivery(env: &mut TaskEnvironment, root: &Path, context: &J) -> Result<()> {
    let owner: Owner = serde_json::from_value(context["owner"].clone())?;
    let request = &context["request"];
    let remote = transport::Remote::new(&request["policy"]);
    let delivery =
        crate::delivery::publication::prepare_remote(env, &owner.queue_root, request, &remote)?;
    save(
        root,
        "operation.json",
        &json!({"phase":"preparing","delivery":delivery["id"]}),
    )
}

fn request(env: &TaskEnvironment, spec: &J, source: &J, owner: &Owner, id: &str) -> Result<J> {
    let state = snapshot(owner)?;
    let previous = &state["tasks"][&owner.task_id]["stage_delivery"]["delivery"]["artifact"]["id"];
    let original = if previous.is_null() {
        json!(
            env.creation
                .as_ref()
                .ok_or_else(|| FlowError::guard("publication creation binding missing"))?
                .commit
        )
    } else {
        previous.clone()
    };
    let mut request = spec.clone();
    request["id"] = json!(id);
    request["policy"] = source["publication"].clone();
    request["original_base"] = original;
    Ok(request)
}

fn bind_producer(root: &Path, context: &J) -> Result<()> {
    let owner: Owner = serde_json::from_value(context["owner"].clone())?;
    let agent = read(root, "publisher-agent.json")?;
    let name = format!("publish-{}", required(&context["request"], "id")?);
    let _lock = queue::locked(&owner.queue_root, false)?;
    let mut state = queue::load_queue(&owner.queue_root)?;
    child(&state, context)?;
    let wait = &mut state["waits"][&name];
    let producer =
        crate::identity::validate(&json!({"adapter":"codex","session":agent["session"]}))?;
    if !set_producer(wait, &name, &producer, root)? {
        return Ok(());
    }
    queue::save(&owner.queue_root, &mut state)
}

fn set_producer(wait: &mut J, name: &str, producer: &J, root: &Path) -> Result<bool> {
    ensure(wait["task"] == name, "publication result wait changed")?;
    if wait["producer"] == *producer && wait["runtime"] == json!(root) {
        return Ok(false);
    }
    ensure(
        wait["producer"].is_null(),
        "publication producer already bound",
    )?;
    wait["producer"] = producer.clone();
    wait["runtime"] = json!(root);
    Ok(true)
}

fn spawn(env: &mut TaskEnvironment, root: &Path, source: &J) -> Result<u32> {
    let permit = crate::task_env::isolation::mutation_permit(env)?;
    let binary = source
        .get("driver_binary")
        .cloned()
        .unwrap_or(json!(std::env::current_exe()?));
    let path = PathBuf::from(required(&json!({"binary":binary}), "binary")?);
    let mut command = env.permitted_command(path.as_os_str(), false, permit.as_deref())?;
    command
        .args(["task-runtime", "publication-watch", "--state-dir"])
        .arg(root)
        .env("TMPDIR", root.join("tmp"));
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(root.join("publication-watch.log"))?;
    let mut child = command
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log)
        .spawn()?;
    super::register_child(env, &mut child)?;
    native::receiver::active::await_watch(root, &mut child)?;
    let pid = child.id();
    util::process::BackgroundChild {
        pid,
        child,
        ready: None,
    }
    .reap();
    Ok(pid)
}

pub fn status(coordinator: &Path) -> Result<J> {
    let permit = crate::lifecycle::runtime_permit(coordinator)?
        .ok_or_else(|| FlowError::guard("publication status requires task Owner"))?;
    let Some(current) =
        util::read_state_json_bounded_if_exists(coordinator, "publication-current.json", LIMIT)?
    else {
        return Ok(json!({"phase":"not_started"}));
    };
    let root = PathBuf::from(required(&current, "root")?);
    let context = read(&root, "publisher.json")?;
    ensure(
        context["owner"] == json!(permit.owner()) && context["coordinator"] == json!(coordinator),
        "publication owner changed",
    )?;
    let Some(_lock) = util::FileLock::acquire(&root.join("publication.lock"), false)? else {
        return Ok(json!({"phase":"updating","id":current["id"],"root":root}));
    };
    Ok(
        json!({"id":current["id"],"root":root,"operation":util::read_state_json_bounded_if_exists(&root,"operation.json",LIMIT)?,"watcher":crate::watch_service::health(&root)}),
    )
}

pub fn watch(root: &Path) -> Result<()> {
    let context = read(root, "publisher.json")?;
    let owner: Owner = serde_json::from_value(context["owner"].clone())?;
    ensure(
        context["root"] == json!(root)
            && std::env::var(crate::task_env::MARKER_ENV).ok().as_deref()
                == Some(&owner.instance_id),
        "publisher watcher identity changed",
    )?;
    crate::watch_service::run(
        root,
        || tick(root, &context, &owner),
        || Ok(()),
        2,
        None,
        false,
    )
}

fn tick(root: &Path, context: &J, owner: &Owner) -> Result<bool> {
    let Some(_lock) = util::FileLock::acquire(&root.join("publication.lock"), false)? else {
        return Ok(false);
    };
    let mut operation = read(root, "operation.json")?;
    if operation["phase"] == "signalled" {
        return Ok(true);
    }
    let permit = crate::lifecycle::runtime_permit(root)?
        .ok_or_else(|| FlowError::guard("publisher lost task ownership"))?;
    ensure(permit.owner() == owner, "publisher Owner changed")?;
    tick_owned(root, context, &mut operation)
}

fn tick_owned(root: &Path, context: &J, operation: &mut J) -> Result<bool> {
    advance_recorded(root, context, operation)?;
    save(root, "operation.json", operation)?;
    if matches!(operation["phase"].as_str(), Some("complete" | "attention")) {
        signal(root, context, operation)?;
    }
    Ok(operation["phase"] == "signalled")
}

fn advance_recorded(root: &Path, context: &J, operation: &mut J) -> Result<()> {
    if let Err(error) = drive::advance(root, context, operation) {
        record_error(operation, &error);
    }
    Ok(())
}

fn record_error(operation: &mut J, error: &FlowError) {
    if error.kind == crate::ErrorKind::StateBusy {
        return;
    }
    operation["error"] = json!(error.to_string());
    if error.kind == crate::ErrorKind::Transient {
        // Reading an unavailable remote cannot settle the sole result wait.
        // Keep the original phase and at-most-once mutation journals intact.
        operation["retry_observation_at"] = json!(util::now() + 30.0);
        operation["next_step"] = json!("Waiting to read the original remote operation again; inspect persistent observation errors. No remote mutation will be replayed.");
        return;
    }
    operation["phase"] = json!("attention");
    operation["next_step"] = json!("Inspect this original publication and its exact remote effects; do not replay push, create or merge.");
}

fn signal(root: &Path, context: &J, operation: &mut J) -> Result<()> {
    let owner: Owner = serde_json::from_value(context["owner"].clone())?;
    let agent = read(root, "publisher-agent.json")?;
    let outcome = if operation["phase"] == "complete" {
        "complete"
    } else {
        "decision"
    };
    let result = json!({"wait":format!("publish-{}",required(&context["request"],"id")?),
        "outcome":outcome,"summary":"Publisher result ready; inspect actual remote receipt and original operation",
        "evidence":root.join("operation.json"),"lifecycle_owner":owner});
    queue::signal_native(
        &owner.queue_root,
        &json!({"adapter":"codex","session":agent["session"]}),
        &result,
    )?;
    operation["result_outcome"] = json!(outcome);
    operation["phase"] = json!("signalled");
    save(root, "operation.json", operation)
}

fn snapshot(owner: &Owner) -> Result<J> {
    let _lock = queue::locked(&owner.queue_root, false)?;
    let state = queue::load_queue(&owner.queue_root)?;
    ensure(
        state["id"] == owner.queue_id,
        "publisher queue identity changed",
    )?;
    Ok(state)
}

fn child<'a>(state: &'a J, context: &J) -> Result<&'a J> {
    ensure(state["stopped"] != true, "publication queue is stopped")?;
    let name = format!("publish-{}", required(&context["request"], "id")?);
    let task = &state["tasks"][name];
    ensure(
        task["control"] == "running",
        "publication child is not running",
    )?;
    crate::progress::lineage::ensure_current(state, task)?;
    Ok(task)
}

fn commit(context: &J, before: &J, updated: &J, reserve: bool) -> Result<()> {
    let owner: Owner = serde_json::from_value(context["owner"].clone())?;
    let name = required(updated, "task")?;
    let _lock = queue::locked(&owner.queue_root, false)?;
    let mut current = queue::load_queue(&owner.queue_root)?;
    validate_snapshot(&current, before, &owner.task_id, &name)?;
    child(&current, context)?;
    if reserve {
        crate::delivery::publication::revalidate_slot(&current, updated)?;
    }
    current["tasks"][name] = updated.clone();
    queue::save(&owner.queue_root, &mut current)
}

fn validate_snapshot(current: &J, before: &J, parent: &str, name: &str) -> Result<()> {
    ensure(
        current["id"] == before["id"]
            && current["tasks"][name] == before["tasks"][name]
            && current["tasks"][parent] == before["tasks"][parent],
        "publication inputs changed during remote observation",
    )
}

#[cfg(test)]
mod invariants {
    use super::*;

    #[test]
    fn observation_outage_keeps_the_original_pending_operation_and_sends_intact() {
        let mut operation =
            json!({"phase":"confirming","submissions":{"merge":{"phase":"unknown"}}});
        let submissions = operation["submissions"].clone();
        record_error(
            &mut operation,
            &FlowError::new(crate::ErrorKind::Transient, "GET timed out"),
        );
        assert_eq!(operation["phase"], "confirming");
        assert_eq!(operation["submissions"], submissions);
        assert!(operation["retry_observation_at"].as_f64().unwrap() > util::now());
        assert!(operation["result_outcome"].is_null());
        let before = operation.clone();
        record_error(
            &mut operation,
            &FlowError::new(crate::ErrorKind::StateBusy, "owner updating"),
        );
        assert_eq!(operation, before);
        record_error(
            &mut operation,
            &FlowError::guard("actual PR identity changed"),
        );
        assert_eq!(operation["phase"], "attention");
        assert_eq!(operation["submissions"], submissions);
    }

    #[test]
    fn interrupted_planned_prefix_can_launch_only_a_provably_unsent_watcher() {
        let root = util::fs::test_tempdir();
        save(
            root.path(),
            "operation.json",
            &json!({"phase":"planned","delivery":"fixed"}),
        )
        .unwrap();
        assert!(unsent_watch(root.path()).unwrap());
        for phase in ["sending", "started"] {
            save(root.path(), "watch-start.json", &json!({"phase":phase})).unwrap();
            assert!(!unsent_watch(root.path()).unwrap());
        }
        let unknown = util::fs::test_tempdir();
        save(unknown.path(), "watch.json", &json!({"status":"stopped"})).unwrap();
        assert!(unsent_watch(unknown.path()).is_err());
    }

    #[test]
    fn producer_recovery_preserves_binding_and_rejects_replacement() {
        let producer = json!({"adapter":"codex","session":"original"});
        let root = Path::new("/task/runtime");
        let mut wait = json!({"task":"publish-one","producer":null});
        assert!(set_producer(&mut wait, "publish-one", &producer, root).unwrap());
        let before = wait.clone();
        assert!(!set_producer(&mut wait, "publish-one", &producer, root).unwrap());
        assert_eq!(wait, before);
        assert!(set_producer(&mut wait, "other", &producer, root).is_err());
        assert!(set_producer(
            &mut wait,
            "publish-one",
            &json!({"adapter":"codex","session":"different"}),
            root
        )
        .is_err());
        assert_eq!(wait, before);
    }

    #[test]
    fn unrelated_progress_survives_but_parent_or_publisher_changes_reject_commit() {
        let before = json!({"id":"q","tasks":{"parent":{"version":1},"publisher":{"version":1}}});
        let mut current = before.clone();
        current["tasks"]["unrelated"] = json!({"version":2});
        validate_snapshot(&current, &before, "parent", "publisher").unwrap();
        for name in ["parent", "publisher"] {
            let mut changed = current.clone();
            changed["tasks"][name]["version"] = json!(2);
            assert!(validate_snapshot(&changed, &before, "parent", "publisher").is_err());
        }
        current["id"] = json!("foreign");
        assert!(validate_snapshot(&current, &before, "parent", "publisher").is_err());
    }

    #[test]
    fn publication_input_cannot_supply_a_model_or_retarget_an_existing_attempt() {
        let source = json!({"configuration":"upstream","role_models":{"publisher":{"providerID":"existing","id":"publisher"}}});
        let input = json!({"title":"fixed change","body":"agreed scope"});
        validate_start(&source, &input).unwrap();
        let mut changed = input.clone();
        changed["model"] = json!("foreign");
        assert!(validate_start(&source, &changed).is_err());
        assert!(validate_start(&json!({}), &input).is_err());
        let current = json!({"id":"one","input":input});
        validate_current(&current, &input, &J::Null).unwrap();
        validate_current(&current, &input, &json!({"id":"one"})).unwrap();
        assert!(validate_current(&current, &changed, &J::Null).is_err());
        assert!(validate_current(&current, &input, &json!({"id":"other"})).is_err());
    }
}
