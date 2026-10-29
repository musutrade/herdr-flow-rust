//! Isolated writer handoff protocol fixtures; no model service or user assets.
use super::*;
use crate::task_runtime::broker::tests::rpc_fixture;
use crate::{lifecycle, task_env};
use std::{
    fs,
    os::unix::process::CommandExt,
    process::{Child, Command},
    sync::Mutex,
};

const SESSION: &str = "11111111-1111-1111-1111-111111111111";
const OTHER: &str = "22222222-2222-2222-2222-222222222222";

struct Fixture {
    temp: tempfile::TempDir,
    owner: Owner,
    broker: Child,
    _control: task_env::TestEnvRootGuard,
}

fn git(root: &Path, args: &[&str]) -> String {
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(root).args(args);
    // SAFETY: child-only scalar umask, without changing the test process.
    unsafe {
        cmd.pre_exec(|| {
            libc::umask(0o077);
            Ok(())
        });
    }
    let output = cmd.output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().into()
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
        let receiver = json!({"version":2,"task":"coordinator","role":"coordinator",
            "pane":"w1:p1","terminal":"term1","socket":"/fixture","cwd":repo,
            "session":SESSION,"phase":"monitoring","runtime":temp.path().join("scratch/rt/coordinator")});
        util::atomic_write_json(&guard.join("state.json"), &receiver).unwrap();
        let mut state = queue::load_queue(&queue).unwrap();
        state["guard_dir"] = json!(guard);
        state["receiver"] = crate::guard::scope(&receiver);
        state["tasks"] = json!({"parent":{"task":"parent","epoch":1,"schema_version":2,
            "requirements":{"1":{"text":"repair the same requirement"}},"requirement_revision":1,
            "candidate":commit,"approved":{"candidate":"old"},"acceptance_round":2,
            "artifacts":{commit.clone():{"object":{"adapter":"git","id":commit,"location":repo}}},
            "verdicts":{"old":{"result":"approved"}},"state_version":1,"phase":"coordinating",
            "control":"running","writer":null,"pending":[],"operations":{}}});
        queue::save(&queue, &mut state).unwrap();
        queue::finalization::task_action(&queue, "task-environment", &json!({"action":"create",
            "task":"parent","queue_id":state["id"],"epoch":1,"state_version":1,
            "environment":{"workspace_root":temp.path().join("workspace"),"temp_root":temp.path().join("scratch"),
                "delivery_targets":[],"repository":repo,"commit":commit}}), Some(SESSION), &|_| panic!("no model calls")).unwrap();
        let owner = lifecycle::task_owner(&queue::load_queue(&queue).unwrap(), "parent")
            .unwrap()
            .unwrap();
        let broker = Command::new("sleep").arg("300").spawn().unwrap();
        let fixture = Self {
            temp,
            owner,
            broker,
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
        self.temp.path().join("scratch/rt/coordinator")
    }
    fn checkout(&self) -> PathBuf {
        self.temp.path().join("workspace/checkout")
    }
    fn state(&self) -> J {
        queue::load_queue(&self.owner.queue_root).unwrap()
    }
    fn context(&self) -> J {
        util::read_state_json(&self.root().join("recovery-observer"), "observer.json").unwrap()
    }
    fn write_state(&self, state: &mut J) {
        queue::save(&self.owner.queue_root, state).unwrap();
    }
    fn action(&self, name: &str, extra: J) -> Result<J> {
        let mut spec = json!({"id":"manual-one","actor":"operator"});
        spec.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        action(&self.root(), "parent", name, &spec)
    }
    fn request(&self) -> J {
        self.action("human-request", json!({"reason":"repair after inspection"}))
            .unwrap()
    }
    fn acquire(&self) -> J {
        self.action("human-acquire", json!({})).unwrap()
    }
    fn give_back(&self) -> J {
        self.action(
            "human-return",
            json!({"note":"edited and stopped all commands"}),
        )
        .unwrap()
    }
    fn incident(&self) -> J {
        json!({"id":util::uuid_hex(),"turn":"verification-turn","agent":{"session":"recovery-session"},
            "fault":verification_fault(&self.context(), &self.state()).unwrap().unwrap()})
    }

    fn prepare(&self, socket: &Path) {
        let mut env = self.active();
        let root = super::super::prepare(&env, "coordinator").unwrap();
        let pid = self.broker.id() as i32;
        let ticks = task_env::process_start_time(pid).unwrap();
        env.child_processes.push(task_env::RegisteredProcess {
            pid,
            pgid: pid,
            start_time: Some(ticks),
            descendants: vec![],
            direct_children: vec![],
        });
        let runtime = json!({"root":root,"instance":env.instance_id,"adapter":"codex","configuration":"upstream",
            "lifecycle_owner":self.owner,"owner":task_env::instance::owner(&env),"cwd":self.checkout(),
            "pid":pid,"start_time":ticks,"upstream_socket":upstream::socket_identity(socket).unwrap()});
        util::atomic_write_json(&root.join("runtime.json"), &runtime).unwrap();
        util::atomic_write_json(&root.join("upstream-session.json"), &json!({"phase":"created","session":SESSION,"instance":env.instance_id,"runtime":runtime})).unwrap();
        let guard = self.temp.path().join("receiver");
        let mut receiver = util::read_state_json(&guard, "state.json").unwrap();
        receiver["instance"] = json!(env.instance_id);
        util::atomic_write_json(&guard.join("state.json"), &receiver).unwrap();
        util::atomic_write_json(
            &root.join("receiver.json"),
            &json!({"phase":"bound","guard_dir":guard,"state":receiver}),
        )
        .unwrap();
        let mut state = self.state();
        state["receiver"] = crate::guard::scope(&receiver);
        self.write_state(&mut state);
        let observer = root.join("recovery-observer");
        task_env::isolation::runtime_namespace(&env, &observer).unwrap();
        util::atomic_write_json(
            &observer.join("observer.json"),
            &json!({"owner":self.owner,"coordinator":root,"observer":observer}),
        )
        .unwrap();
        util::atomic_write_json(
            &observer.join("agent.json"),
            &json!({"phase":"ready","session":"recovery-session"}),
        )
        .unwrap();
        env.persist().unwrap();
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.broker.kill();
        let _ = self.broker.wait();
    }
}

fn server(count: usize, mode: &'static str, config: Arc<Mutex<J>>) -> rpc_fixture::Server {
    rpc_fixture::Server::start_connections(count, move |mut peer| {
        peer.initialize();
        let request = peer.request();
        assert_eq!(request["method"], "thread/read");
        let cwd = config.lock().unwrap()["cwd"].clone();
        let status = if mode == "busy" { "active" } else { "idle" };
        peer.json(&json!({"id":request["id"],"result":{"thread":{"id":SESSION,"cwd":cwd,"status":{"type":status}}}}));
        if mode == "busy" {
            return;
        }
        let request = peer.request();
        assert_eq!(request["method"], "thread/backgroundTerminals/list");
        if mode == "unobservable" {
            peer.json(
                &json!({"id":request["id"],"error":{"code":-32601,"message":"method unavailable"}}),
            );
            return;
        }
        let terminals = if mode == "background" {
            json!([{"processId":"still-running"}])
        } else {
            json!([])
        };
        peer.json(&json!({"id":request["id"],"result":{"data":terminals,"nextCursor":null}}));
        if mode == "background" {
            return;
        }
        let request = peer.request();
        assert_eq!(request["method"], "thread/loaded/list");
        let ids = if mode == "foreign" {
            vec![SESSION, OTHER]
        } else {
            vec![SESSION]
        };
        peer.json(&json!({"id":request["id"],"result":{"data":ids,"nextCursor":null}}));
        for id in ids {
            let request = peer.request();
            assert_eq!(request["method"], "thread/read");
            assert_eq!(request["params"]["threadId"], id);
            peer.json(&json!({"id":request["id"],"result":{"thread":{"id":id,"cwd":cwd,"status":{"type":"idle"}}}}));
        }
    })
}

fn verdict(incident: &J, disposition: &str) -> J {
    json!({"incident":incident["id"],"disposition":disposition,"requested_action":"none",
        "facts":"inspected original records and returned checkout","actions":"read only",
        "current_state":"observed","next_step":"inspect and continue under the original requirement"})
}

// Share the real isolated queue/worktree with the recovery observer's tests.
pub(crate) fn returned_scenario(test: impl FnOnce(&Path, &J, &J)) {
    let config = Arc::new(Mutex::new(json!({})));
    let service = server(2, "idle", config.clone());
    let f = Fixture::new(&service.path);
    config.lock().unwrap()["cwd"] = json!(f.checkout());
    f.request();
    assert_eq!(f.acquire()["phase"], "human");
    f.give_back();
    test(
        &f.root().join("recovery-observer"),
        &f.context(),
        &f.state(),
    );
    service.finish();
}

#[test]
fn explicit_handoff_preserves_owner_and_history_and_requires_fresh_acceptance() {
    let config = Arc::new(Mutex::new(json!({})));
    let service = server(3, "idle", config.clone());
    let f = Fixture::new(&service.path);
    config.lock().unwrap()["cwd"] = json!(f.checkout());
    assert_eq!(
        status(&f.root(), "parent").unwrap()["phase"],
        "not_requested"
    );
    assert!(f.state()["tasks"]["parent"][KEY].is_null());
    assert!(automatic_input_allowed(&f.root()).unwrap());
    let before = f.state()["tasks"]["parent"].clone();
    let request = f.request();
    assert_eq!(request["phase"], "draining");
    assert!(!automatic_input_allowed(&f.root()).unwrap());
    assert_eq!(f.request(), request);
    assert!(check_workspace(&f.state(), &f.checkout()).is_err());
    assert!(check_workspace(&f.state(), f.temp.path()).is_err());
    check_workspace(&f.state(), &f.temp.path().join("repo")).unwrap();
    let acquired = f.acquire();
    assert_eq!(acquired["phase"], "human", "{acquired}");
    assert_eq!(f.acquire(), acquired);
    {
        let env = f.active();
        let mut state = f.state();
        let before_terminal = state.clone();
        let error = crate::progress::terminal::record(&mut state, &json!({"outcome":"failure","reason":"cancel","state_version":before_terminal["tasks"]["parent"]["state_version"]}), &f.owner, &env, util::now()).unwrap_err();
        assert!(error.message.contains("human writer"));
        assert_eq!(state, before_terminal);
        assert!(admit_launch(&env, &json!({"cwd":f.checkout()})).is_err());
        assert!(
            crate::progress::ownership::writer_reservation(&state, "parent", &f.checkout())
                .is_err()
        );
    }
    fs::write(f.checkout().join("human.txt"), "operator change").unwrap();
    let returned = f.give_back();
    assert_eq!(returned["phase"], "verifying");
    assert_eq!(f.give_back(), returned);
    assert!(check_workspace(&f.state(), &f.checkout()).is_err());
    let incident = f.incident();
    let result = verdict(&incident, "recovered");
    assert_eq!(
        finish_verification(&f.context(), &incident, &result).unwrap(),
        result
    );
    let after = f.state()["tasks"]["parent"].clone();
    assert_eq!(after[KEY]["phase"], "released");
    assert!(automatic_input_allowed(&f.root()).unwrap());
    assert!(
        after["candidate"].is_null()
            && after["approved"].is_null()
            && after["delivery_target"].is_null()
    );
    assert_eq!(after["acceptance_round"], 3);
    for field in [
        "environment",
        "requirements",
        "verdicts",
        "operations",
        "epoch",
    ] {
        assert_eq!(after[field], before[field]);
    }
    check_workspace(&f.state(), &f.checkout()).unwrap();
    let mut retired = f.state();
    retired["tasks"]["parent"]["environment"] = J::Null;
    check_workspace(&retired, &f.checkout()).unwrap();
    finish_verification(&f.context(), &incident, &result).unwrap();
    assert_eq!(
        f.state()["tasks"]["parent"],
        after,
        "lost observer acknowledgement cannot invalidate twice"
    );
    f.action(
        "human-request",
        json!({"id":"manual-two","reason":"second intervention"}),
    )
    .unwrap();
    assert_eq!(
        f.state()["tasks"]["parent"]["human_handoffs"][0],
        after[KEY]
    );
    service.finish();
}

#[test]
fn active_unknown_native_and_background_work_never_grant_human_authority() {
    for mode in ["busy", "background", "unobservable", "foreign"] {
        let config = Arc::new(Mutex::new(json!({})));
        let service = server(1, mode, config.clone());
        let f = Fixture::new(&service.path);
        config.lock().unwrap()["cwd"] = json!(f.checkout());
        f.request();
        let result = f.acquire();
        assert_eq!(result["phase"], "draining", "{mode}: {result}");
        assert!(result["blocked_reason"].is_string());
        assert_eq!(result["writer"], "agent");
        assert!(f.action("human-return", json!({"note":"force"})).is_err());
        service.finish();
    }
}

#[test]
fn unresolved_journals_and_git_locks_are_preserved_without_an_upstream_send() {
    let service = server(0, "idle", Arc::new(Mutex::new(json!({}))));
    let f = Fixture::new(&service.path);
    f.request();
    for (field, value) in [
        ("writer", json!({"workspace":f.checkout()})),
        ("operations", json!({"original":{"phase":"sending"}})),
        ("pr", json!({"publication":{"phase":"sending"}})),
    ] {
        let mut state = f.state();
        let before = state["tasks"]["parent"][field].clone();
        state["tasks"]["parent"][field] = value.clone();
        f.write_state(&mut state);
        assert_eq!(f.acquire()["phase"], "draining");
        state = f.state();
        assert_eq!(state["tasks"]["parent"][field], value);
        state["tasks"]["parent"][field] = before;
        f.write_state(&mut state);
    }
    let lock = PathBuf::from(git(
        &f.checkout(),
        &[
            "rev-parse",
            "--path-format=absolute",
            "--git-path",
            "index.lock",
        ],
    ));
    fs::write(&lock, "original git evidence").unwrap();
    assert_eq!(f.acquire()["phase"], "draining");
    assert_eq!(fs::read_to_string(lock).unwrap(), "original git evidence");
    service.finish();
}

#[test]
fn returned_checkout_changes_block_release_and_a_new_return_can_be_verified() {
    let config = Arc::new(Mutex::new(json!({})));
    let service = server(6, "idle", config.clone());
    let f = Fixture::new(&service.path);
    config.lock().unwrap()["cwd"] = json!(f.checkout());
    f.request();
    assert_eq!(f.acquire()["phase"], "human");
    f.give_back();
    let old = f.incident();
    fs::write(f.checkout().join("late.txt"), "after return").unwrap();
    let result = finish_verification(&f.context(), &old, &verdict(&old, "recovered")).unwrap();
    assert_eq!(result["disposition"], "insufficient_evidence");
    assert_eq!(status(&f.root(), "parent").unwrap()["phase"], "blocked");
    assert!(!f.state()["tasks"]["parent"]["approved"].is_null());
    assert_eq!(f.acquire()["phase"], "human");
    f.give_back();
    let new = f.incident();
    assert_ne!(old["fault"]["id"], new["fault"]["id"]);
    assert!(finish_verification(&f.context(), &old, &verdict(&old, "recovered")).is_err());
    finish_verification(&f.context(), &new, &verdict(&new, "recovered")).unwrap();
    assert_eq!(
        status(&f.root(), "parent").unwrap()["verifications"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    service.finish();
}

#[test]
fn new_process_in_checkout_blocks_transfer_until_it_exits() {
    let config = Arc::new(Mutex::new(json!({})));
    let service = server(2, "idle", config.clone());
    let f = Fixture::new(&service.path);
    config.lock().unwrap()["cwd"] = json!(f.checkout());
    f.request();
    let mut writer = Command::new("sleep")
        .arg("60")
        .current_dir(f.checkout())
        .spawn()
        .unwrap();
    let blocked = f.acquire();
    writer.kill().unwrap();
    writer.wait().unwrap();
    assert_eq!(blocked["phase"], "draining", "{blocked}");
    let acquired = f.acquire();
    assert_eq!(acquired["phase"], "human", "{acquired}");
    service.finish();
}

#[test]
fn registered_descendant_retains_writer_even_after_leaving_the_checkout() {
    let config = Arc::new(Mutex::new(json!({})));
    let service = server(2, "idle", config.clone());
    let f = Fixture::new(&service.path);
    config.lock().unwrap()["cwd"] = json!(f.checkout());
    let mut writer = Command::new("sleep")
        .arg("60")
        .current_dir(f.temp.path())
        .spawn()
        .unwrap();
    {
        let mut env = f.active();
        let pid = writer.id() as i32;
        env.child_processes[0]
            .descendants
            .push(task_env::ProcessRecord {
                pid,
                start_time: task_env::process_start_time(pid),
            });
        env.persist().unwrap();
    }
    f.request();
    let blocked = f.acquire();
    writer.kill().unwrap();
    writer.wait().unwrap();
    assert_eq!(blocked["phase"], "draining");
    assert!(blocked["blocked_reason"]
        .as_str()
        .unwrap()
        .contains("descendant"));
    assert_eq!(f.acquire()["phase"], "human");
    service.finish();
}

#[test]
fn original_tool_and_turn_journals_must_settle_before_any_human_acquisition() {
    let config = Arc::new(Mutex::new(json!({})));
    let service = server(1, "idle", config.clone());
    let f = Fixture::new(&service.path);
    config.lock().unwrap()["cwd"] = json!(f.checkout());
    f.request();
    for (file, pending, settled) in [
        (
            "broker-events.json",
            json!({"pending":{"tool":{"id":"original"}}}),
            json!({"pending":{}}),
        ),
        (
            "broker-tools.json",
            json!({"original":{"phase":"sending"}}),
            json!({"original":{"phase":"responded"}}),
        ),
        (
            "control-tools.json",
            json!({"original":{"phase":"unknown"}}),
            json!({"original":{"phase":"complete"}}),
        ),
        (
            "turn.json",
            json!({"phase":"submitted"}),
            json!({"phase":"signalled"}),
        ),
        (
            "receiver-start.json",
            json!({"phase":"sending"}),
            json!({"phase":"submitted"}),
        ),
    ] {
        util::atomic_write_json(&f.root().join(file), &pending).unwrap();
        assert_eq!(f.acquire()["phase"], "draining");
        assert_eq!(util::read_state_json(&f.root(), file).unwrap(), pending);
        // Simulate observation/application of the same original operation.
        util::atomic_write_json(&f.root().join(file), &settled).unwrap();
    }
    assert_eq!(f.acquire()["phase"], "human");
    service.finish();
}

#[test]
fn admission_is_serialized_and_allows_observation_and_original_replies() {
    let service = server(0, "idle", Arc::new(Mutex::new(json!({}))));
    let f = Fixture::new(&service.path);
    let lease = lifecycle::write_permit(&f.owner).unwrap();
    assert_eq!(
        f.action("human-request", json!({"reason":"repair"}))
            .unwrap_err()
            .kind,
        crate::ErrorKind::StateBusy
    );
    assert!(f.state()["tasks"]["parent"][KEY].is_null());
    drop(lease);
    f.request();
    for method in [
        "command/exec",
        "turn/start",
        "turn/steer",
        "thread/start",
        "thread/compact/start",
        "unknown/write",
    ] {
        assert!(
            admit_native(
                &f.root(),
                &json!({"method":method,"params":{"threadId":SESSION}})
            )
            .is_err(),
            "{method}"
        );
    }
    for method in [
        "thread/read",
        "thread/backgroundTerminals/list",
        "turn/interrupt",
        "tool/respond",
    ] {
        admit_native(
            &f.root(),
            &json!({"method":method,"params":{"threadId":SESSION}}),
        )
        .unwrap();
    }
    admit_native(&f.root(), &json!({"id":1,"result":{"decision":"decline"}})).unwrap();
    admit_native(&f.root(), &json!({"method":"thread/resume","params":{"threadId":SESSION,"model":null,"sandbox":null,"config":null,"excludeTurns":true,"initialTurnsPage":{"limit":10}}})).unwrap();
    assert!(admit_native(
        &f.root(),
        &json!({"method":"thread/resume","params":{"threadId":SESSION,"serviceTier":null}})
    )
    .is_err());
    assert!(admit_native(&f.root(), &json!({"method":"thread/resume","params":{"threadId":SESSION,"sandbox":"danger-full-access"}})).is_err());
    assert!(f
        .action("human-acquire", json!({"actor":"different"}))
        .is_err());
    assert!(f
        .action("human-request", json!({"reason":"changed"}))
        .is_err());
    assert!(f.action("human-acquire", json!({"force":true})).is_err());
    let mut runtime = util::read_state_json(&f.root(), "runtime.json").unwrap();
    runtime["recovery_role"] = json!(true);
    util::atomic_write_json(&f.root().join("runtime.json"), &runtime).unwrap();
    let mut message = json!({"method":"turn/start","params":{"threadId":SESSION,"sandboxPolicy":{"type":"readOnly","networkAccess":false},"approvalPolicy":"never"}});
    admit_native(&f.root(), &message).unwrap();
    message["params"]["permissions"] = json!("all");
    assert!(admit_native(&f.root(), &message).is_err());
    service.finish();
}
