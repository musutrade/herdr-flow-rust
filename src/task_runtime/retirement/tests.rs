use super::*;
use crate::{lifecycle, task_env};
use std::{
    fs,
    os::unix::process::CommandExt,
    process::{Child, Command},
    sync::Mutex,
};

// Reuse the existing broker protocol peer; no real service or model is used.
use crate::task_runtime::broker::tests::rpc_fixture as rpc_server;

const SESSION: &str = "11111111-1111-1111-1111-111111111111";
const WORKER: &str = "22222222-2222-2222-2222-222222222222";
const GUARDIAN: &str = "33333333-3333-3333-3333-333333333333";
const ASSIGNMENT: &str = "accept-one";

struct Fixture {
    temp: tempfile::TempDir,
    owner: Owner,
    broker: Child,
    unrelated: Child,
    _control: task_env::TestEnvRootGuard,
}

fn git(root: &Path, args: &[&str]) -> String {
    let mut command = Command::new("git");
    command.arg("-C").arg(root).args(args);
    // SAFETY: scalar child-only umask; no process-global mutation.
    unsafe {
        command.pre_exec(|| {
            libc::umask(0o077);
            Ok(())
        });
    }
    let out = command.output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap().trim().into()
}

impl Fixture {
    fn new(socket: &Path) -> Self {
        let temp = util::fs::test_tempdir();
        let queue = temp.path().join("queue");
        let control = task_env::test_env_root(&queue.join("queue.json/task-environments"));
        let repo = temp.path().join("repo");
        util::ensure_private_dir(&repo, true).unwrap();
        git(&repo, &["init", "-q"]);
        git(
            &repo,
            &[
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.test",
                "commit",
                "--allow-empty",
                "-qm",
                "base",
            ],
        );
        let commit = git(&repo, &["rev-parse", "HEAD"]);
        queue::init_isolated(&queue, None, 30).unwrap();
        let guard = temp.path().join("receiver");
        util::ensure_private_dir(&guard, true).unwrap();
        util::protected::namespace(&guard).unwrap();
        let coordinator = temp.path().join("scratch/rt/coordinator");
        let receiver = json!({"version":2,"task":"coordinator","role":"coordinator",
            "pane":"w1:p1","terminal":"term1","socket":"/fixture","cwd":repo,
            "session":SESSION,"phase":"monitoring","runtime":coordinator});
        util::atomic_write_json(&guard.join("state.json"), &receiver).unwrap();
        let mut state = queue::load_queue(&queue).unwrap();
        state["guard_dir"] = json!(guard);
        state["receiver"] = crate::guard::scope(&receiver);
        state["tasks"] = json!({"parent":{"task":"parent","epoch":1,"candidate":commit,
            "state_version":1,"phase":"waiting","control":"running","writer":null,
            "pending":[],"operations":{}}});
        queue::save(&queue, &mut state).unwrap();
        queue::finalization::task_action(
            &queue,
            "task-environment",
            &json!({
            "action":"create","task":"parent","queue_id":state["id"],"epoch":1,
            "state_version":1,"environment":{"workspace_root":temp.path().join("workspace"),
            "temp_root":temp.path().join("scratch"),"delivery_targets":[],"repository":repo,
            "commit":commit}}),
            Some(SESSION),
            &|_| panic!("no model calls"),
        )
        .unwrap();
        let owner = lifecycle::task_owner(&queue::load_queue(&queue).unwrap(), "parent")
            .unwrap()
            .unwrap();
        let broker = Command::new("sleep").arg("60").spawn().unwrap();
        let unrelated = Command::new("sleep").arg("60").spawn().unwrap();
        let fixture = Self {
            temp,
            owner,
            broker,
            unrelated,
            _control: control,
        };
        fixture.prepare(socket);
        fixture
    }

    fn active(&self) -> TaskEnvironment {
        let permit = Arc::new(lifecycle::acquire(&self.owner, true).unwrap());
        task_env::isolation::restore_active(&self.owner, permit).unwrap()
    }

    fn root(&self) -> PathBuf {
        self.temp.path().join("scratch/rt").join(ASSIGNMENT)
    }
    fn clone_path(&self) -> PathBuf {
        self.temp.path().join("workspace/reviews").join(ASSIGNMENT)
    }
    fn state(&self) -> J {
        queue::load_queue(&self.owner.queue_root).unwrap()
    }
    fn plan(&self) -> Plan {
        load(&self.active()).unwrap()
    }

    fn prepare(&self, socket: &Path) {
        let mut env = self.active();
        let root = super::super::prepare(&env, ASSIGNMENT).unwrap();
        let coordinator = super::super::prepare(&env, "coordinator").unwrap();
        let binding = json!({"task":"acceptance","epoch":1,"assignment":ASSIGNMENT,
            "candidate":"artifact-original","artifact_adapter":"local_git",
            "lineage":{"requirement":"parent","scope_epoch":1,"role":"acceptance",
                "revision":1,"session_epoch":1,"input":"artifact-original"}});
        let pid = self.broker.id() as i32;
        let ticks = task_env::process_start_time(pid).unwrap();
        env.child_processes.push(task_env::RegisteredProcess {
            pid,
            pgid: pid,
            start_time: Some(ticks),
            descendants: vec![],
            direct_children: vec![],
        });
        let runtime = json!({"root":root,"instance":env.instance_id,"adapter":"codex",
            "configuration":"upstream","lifecycle_owner":self.owner,"owner":task_env::instance::owner(&env),
            "cwd":self.clone_path(),"pid":pid,"start_time":ticks,
            "upstream_socket":upstream::socket_identity(socket).unwrap()});
        for (name, value) in [
            ("runtime.json", runtime.clone()),
            (
                "creation.json",
                json!({"phase":"created","wait":"original-result",
                "packet":{"binding":binding},"session":WORKER}),
            ),
            (
                "turn.json",
                json!({"phase":"signalled","session":WORKER,"binding":binding,"outcome":"complete"}),
            ),
            (
                "upstream-session.json",
                json!({"phase":"created","session":WORKER,
                "instance":env.instance_id,"runtime":runtime}),
            ),
        ] {
            util::atomic_write_json(&root.join(name), &value).unwrap();
        }
        util::ensure_private_dir(&self.clone_path(), true).unwrap();
        fs::write(self.clone_path().join("evidence"), "old candidate").unwrap();
        env.review_workspaces.insert(
            ASSIGNMENT.into(),
            task_env::review::ReviewWorkspace {
                owner: self.owner.clone(),
                binding: binding.clone(),
                wait: "original-result".into(),
                runtime: root.clone(),
                path: self.clone_path(),
                source: env.workspace_root.join("checkout"),
                phase: task_env::review::Phase::Ready,
                identity: Some(task_env::RootIdentity::of(
                    &fs::metadata(self.clone_path()).unwrap(),
                )),
                git_identity: None,
            },
        );
        let mut state = self.state();
        state["tasks"]["acceptance"] = binding.clone();
        let mut wait = binding.clone();
        wait["id"] = json!("original-result");
        wait["runtime"] = json!(root);
        wait["kind"] = json!("signal");
        state["waits"]["original-result"] = wait;
        state["events"]["result"] = json!({"id":"result","task":"acceptance","epoch":1,
            "wait":"original-result","binding":binding,
            "acknowledged":true,"applied":true,"evidence":root.join("turn.json")});
        collection(&coordinator, &self.owner, &json!({"collection":{"phase":"signalled","binding":binding},
            "result":{"contentItems":[{"text":json!({"runtime":root,"wait":"original-result"}).to_string()}]}})).unwrap();
        env.delivery = json!({"id":"stage-first","phase":"confirmed","stage":{"epoch":1}});
        // Stage checkpoint is a normal shared-permit operation; EX belongs
        // only to the independent watcher attempts after this commit.
        *env.creation_permit.lock().unwrap() = None;
        let resource_cleanup = super::prepare(&mut env, &mut state).unwrap();
        assert_eq!(resource_cleanup["selected"], 1, "{resource_cleanup}");
        state["tasks"]["parent"]["stage_delivery"] = json!({"phase":"complete",
            "delivery":env.delivery,"resource_cleanup":resource_cleanup});
        env.delivery = J::Null;
        env.persist().unwrap();
        queue::save(&self.owner.queue_root, &mut state).unwrap();
    }

    fn pending_tick(&self) {
        assert_eq!(
            drive_owner(&self.owner).unwrap_err().kind,
            crate::ErrorKind::StateBusy
        );
        // No pending attempt may retain EX or the environment lock.
        let _permit = lifecycle::write_permit(&self.owner).unwrap();
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.broker.kill();
        let _ = self.broker.wait();
        let _ = self.unrelated.kill();
        let _ = self.unrelated.wait();
    }
}

fn server(connections: usize, active: bool, calls: Arc<Mutex<Vec<J>>>) -> rpc_server::Server {
    server_status(connections, if active { "active" } else { "idle" }, calls)
}

fn server_status(
    connections: usize,
    status: &'static str,
    calls: Arc<Mutex<Vec<J>>>,
) -> rpc_server::Server {
    rpc_server::Server::start_connections(connections, move |mut peer| {
        peer.initialize();
        let request = peer.request();
        calls.lock().unwrap().push(request.clone());
        match request["method"].as_str().unwrap() {
            "thread/read" => {
                // The fixture sends its exact cwd separately through the row
                // identity stored in the request observer below.
                let cwd = calls.lock().unwrap()[0]["fixture_cwd"].clone();
                peer.json(&json!({"id":request["id"],"result":{"thread":{"id":request["params"]["threadId"],
                    "cwd":cwd,"status":{"type":status,"activeFlags":[]}}}}));
            }
            "thread/delete" => peer.json(&json!({"id":request["id"],"result":{}})),
            "thread/list" => {
                peer.json(&json!({"id":request["id"],"result":{"data":[],"nextCursor":null}}));
                let archived = peer.request();
                calls.lock().unwrap().push(archived.clone());
                assert_eq!(archived["params"]["archived"], true);
                peer.json(&json!({"id":archived["id"],"result":{"data":[],"nextCursor":null}}));
            }
            method => panic!("unexpected method {method}"),
        }
    })
}

#[test]
fn already_unloaded_original_thread_can_retire_without_resuming_it() {
    let calls = Arc::new(Mutex::new(vec![json!({})]));
    let service = server_status(1, "notLoaded", calls.clone());
    let fixture = Fixture::new(&service.path);
    calls.lock().unwrap()[0]["fixture_cwd"] = json!(fixture.clone_path());
    fixture.pending_tick();
    assert_eq!(fixture.plan().entries[0].phase, "retiring");
    assert_eq!(calls.lock().unwrap()[1]["method"], "thread/read");
    assert_eq!(calls.lock().unwrap().len(), 2);
    service.finish();
}

#[test]
fn completed_stage_releases_only_selected_execution_and_keeps_parent_evidence_and_notification() {
    let calls = Arc::new(Mutex::new(vec![json!({})]));
    let service = server(5, false, calls.clone());
    let mut fixture = Fixture::new(&service.path);
    calls.lock().unwrap()[0]["fixture_cwd"] = json!(fixture.clone_path());
    let before = fixture.state();
    for phase in ["retiring", "sessions", "sessions"] {
        fixture.pending_tick();
        assert_eq!(fixture.plan().entries[0].phase, phase);
    }
    drive_owner(&fixture.owner).unwrap();
    service.finish();
    let state = fixture.state();
    assert_eq!(fixture.plan().phase, "complete");
    assert!(!fixture.clone_path().exists());
    assert!(fixture.broker.try_wait().unwrap().is_some());
    assert!(fixture.unrelated.try_wait().unwrap().is_none());
    assert_eq!(
        lifecycle::task_owner(&state, "parent").unwrap(),
        Some(fixture.owner.clone())
    );
    assert_eq!(
        state["tasks"]["parent"]["stage_delivery"]["delivery"],
        before["tasks"]["parent"]["stage_delivery"]["delivery"]
    );
    assert_eq!(state["events"]["result"], before["events"]["result"]);
    assert!(read(&fixture.root(), "turn.json").is_ok());
    assert_eq!(state["waits"]["original-result"]["closed"], true);
    let plan = fixture.plan();
    let id = notification_id(&plan);
    assert_eq!(state["waits"][&id]["closed"], true);
    assert_eq!(state["events"].as_object().unwrap().len(), 2);
    drive(&fixture.owner.queue_root).unwrap();
    assert_eq!(fixture.state()["events"], state["events"]);
    let mut env = fixture.active();
    assert!(super::super::prepare(&env, ASSIGNMENT).is_err());
    assert!(super::super::command(&env, &fixture.root(), Path::new("/bin/true")).is_err());
    assert!(super::super::prepare(&env, "fresh-next-stage").is_ok());
    assert!(env.review_workspaces.is_empty());
    assert!(env.workspace_root.join("checkout/.git").exists());
    *env.creation_permit.lock().unwrap() = None;
    let mut current = fixture.state();
    env.delivery = current["tasks"]["parent"]["stage_delivery"]["delivery"].clone();
    assert_eq!(
        super::prepare(&mut env, &mut current).unwrap()["phase"],
        "complete"
    );
    assert_eq!(current["events"], state["events"]);
    env.delivery = json!({"id":"next-stage","stage":{"epoch":2}});
    let next = super::prepare(&mut env, &mut current).unwrap();
    assert_eq!(next["selected"], 0);
    assert_eq!(next["delivery"], "next-stage");
}

#[test]
fn retry_reuses_original_selection_and_pending_plan_rejects_another_delivery() {
    let service = server(0, false, Arc::new(Mutex::new(vec![])));
    let fixture = Fixture::new(&service.path);
    let mut env = fixture.active();
    *env.creation_permit.lock().unwrap() = None;
    let original = env.stage_resources.clone();
    let mut state = fixture.state();
    env.delivery = state["tasks"]["parent"]["stage_delivery"]["delivery"].clone();
    super::prepare(&mut env, &mut state).unwrap();
    assert_eq!(env.stage_resources, original);
    assert_eq!(state["waits"], fixture.state()["waits"]);
    env.delivery["id"] = json!("different-delivery");
    assert!(super::prepare(&mut env, &mut state)
        .unwrap_err()
        .message
        .contains("original stage resource plan"));
    assert_eq!(env.stage_resources, original);
    service.finish();
}

#[test]
fn current_native_work_keeps_original_resources_and_error_notice_is_deduplicated() {
    let calls = Arc::new(Mutex::new(vec![json!({})]));
    let service = server(2, true, calls.clone());
    let mut fixture = Fixture::new(&service.path);
    calls.lock().unwrap()[0]["fixture_cwd"] = json!(fixture.clone_path());
    for _ in 0..2 {
        assert!(drive_owner(&fixture.owner)
            .unwrap_err()
            .message
            .contains("current native work"));
        let _permit = lifecycle::write_permit(&fixture.owner).unwrap();
    }
    service.finish();
    assert_eq!(fixture.plan().entries[0].phase, "planned");
    assert!(fixture.clone_path().is_dir());
    assert!(fixture.broker.try_wait().unwrap().is_none());
    assert!(!fixture.root().join(MARKER).exists());
    assert_eq!(fixture.state()["events"].as_object().unwrap().len(), 2);
    assert_eq!(calls.lock().unwrap().len(), 3);
}

#[test]
fn changed_presentation_or_new_recovery_incident_is_retained_before_any_native_effect() {
    let service = server(0, false, Arc::new(Mutex::new(vec![])));
    let fixture = Fixture::new(&service.path);
    let replacement = json!({"nonce":"newly-opened","session":WORKER,"phase":"ready"});
    util::atomic_write_json(&fixture.root().join("presentation.json"), &replacement).unwrap();
    assert!(drive_owner(&fixture.owner)
        .unwrap_err()
        .message
        .contains("presentation changed"));
    assert_eq!(
        read(&fixture.root(), "presentation.json").unwrap(),
        replacement
    );
    let observer = fixture
        .temp
        .path()
        .join("scratch/rt/coordinator/recovery-observer");
    util::ensure_private_dir(&observer, true).unwrap();
    util::atomic_write_json(&observer.join("incident.json"), &json!({"phase":"pending"})).unwrap();
    assert!(drive_owner(&fixture.owner)
        .unwrap_err()
        .message
        .contains("recovery incident"));
    assert!(fixture.clone_path().is_dir());
    assert!(!fixture.root().join(MARKER).exists());
    service.finish();
}

#[test]
fn frozen_native_obligations_survive_loss_of_parent_rollout_and_presentation_close() {
    let service = server(0, false, Arc::new(Mutex::new(vec![])));
    let fixture = Fixture::new(&service.path);
    let mut env = fixture.active();
    let mut plan = load(&env).unwrap();
    let mut guardian = plan.entries[0].sessions[0].clone();
    guardian["session"] = json!(GUARDIAN);
    guardian["parent"] = json!(WORKER);
    plan.entries[0].sessions[0]["phase"] = json!("deleted");
    plan.entries[0].sessions.push(guardian.clone());
    plan.entries[0].phase = "sessions".into();
    let captured = json!({"phase":"ready","session":WORKER,"nonce":"old",
        "instance":env.instance_id,"runtime":fixture.root(),
        "persisted_panes":[{"pane":"original"}]});
    plan.entries[0].presentation = Some(captured.clone());
    save(&mut env, &plan).unwrap();
    // Original journals are still at an earlier durable prefix.
    assert_eq!(
        read(&fixture.root(), "upstream-session.json").unwrap()["phase"],
        "created"
    );
    assert!(read(&fixture.root(), "presentation.json").is_err());
    assert_eq!(sessions(&env).unwrap()[1], guardian);
    assert_eq!(
        presentation(&env, &fixture.root()).unwrap(),
        Some(captured.clone())
    );
    assert_eq!(tui::obligations(&env).unwrap(), vec![captured.clone()]);
    // A pre-fence crash can be followed by a legitimate reopen. Terminal
    // takeover must retain both attachments, never overwrite the new tab.
    let mut reopened = captured.clone();
    reopened["nonce"] = json!("new");
    reopened["persisted_panes"] = J::Null;
    util::atomic_write_json(&fixture.root().join("presentation.json"), &reopened).unwrap();
    assert_eq!(tui::obligations(&env).unwrap(), vec![captured, reopened]);
    service.finish();
}

#[test]
fn terminal_finalizer_absorbs_frozen_guardian_and_removes_stage_host_wait() {
    let calls = Arc::new(Mutex::new(vec![json!({})]));
    let service = server(2, false, calls.clone());
    let mut fixture = Fixture::new(&service.path);
    let mut env = fixture.active();
    let mut plan = load(&env).unwrap();
    let mut guardian = plan.entries[0].sessions[0].clone();
    guardian["session"] = json!(GUARDIAN);
    guardian["parent"] = json!(WORKER);
    plan.entries[0].sessions[0]["phase"] = json!("deleted");
    plan.entries[0].sessions.push(guardian);
    plan.entries[0].phase = "sessions".into();
    plan.entries[0].presentation = Some(json!({"phase":"closed","session":WORKER,
        "instance":env.instance_id,"runtime":fixture.root(),"nonce":"original",
        "persisted_panes":[{"pane":"old"}]}));
    save(&mut env, &plan).unwrap();
    drop(env);
    // The frozen plan is newer than both original runtime journals. Commit
    // actual final failure, then let the normal finalizer absorb that prefix.
    let state = fixture.state();
    {
        let _lock = queue::locked(&fixture.owner.queue_root, false).unwrap();
        queue::task_action(&fixture.owner.queue_root, "task-finalize", &json!({
            "task":"parent","queue_id":fixture.owner.queue_id,"epoch":fixture.owner.epoch,
            "instance_id":fixture.owner.instance_id,"state_version":state["tasks"]["parent"]["state_version"],
            "outcome":"failure","reason":"explicit fixture final decision"}), Some(SESSION),
            &|_| panic!("no model calls")).unwrap();
    }
    let report = queue::finalization::drive(&fixture.owner.queue_root).unwrap();
    assert_eq!(report.completed, 1, "{report:?}");
    service.finish();
    let calls = calls.lock().unwrap();
    assert_eq!(calls[1]["method"], "thread/delete");
    assert_eq!(calls[1]["params"]["threadId"], GUARDIAN);
    assert_eq!(
        calls
            .iter()
            .filter(|v| v["method"] == "thread/delete")
            .count(),
        1
    );
    assert!(!fixture.temp.path().join("scratch").exists());
    assert!(!fixture.temp.path().join("workspace").exists());
    assert!(fixture.state()["tasks"]["parent"].is_null());
    assert!(fixture.state()["waits"].as_object().unwrap().is_empty());
    assert!(fixture.state()["events"].as_object().unwrap().is_empty());
    assert!(fixture.unrelated.try_wait().unwrap().is_none());
    assert!(!task_env::is_registered("parent").unwrap());
}

#[path = "execution_faults.rs"]
mod faults;

#[test]
fn retirement_fence_is_durable_and_absence_does_not_create_directories() {
    let temp = util::fs::test_tempdir();
    let root = temp.path().join("worker");
    admit(&root).unwrap();
    assert!(!root.exists());
    util::ensure_private_dir(&root, true).unwrap();
    util::atomic_write_json(&root.join(MARKER), &json!({"phase":"retiring"})).unwrap();
    assert!(admit(&root).is_err());
    util::atomic_write_json(&root.join(MARKER), &json!({"phase":"retired"})).unwrap();
    assert!(admit(&root).is_err());
}
