//! Synthetic upstream tools through the real watcher, broker and native worker.
use super::*;
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

const COORD: &str = "22222222-2222-2222-2222-222222222222";
const GUARDIAN: &str = "55555555-5555-5555-5555-555555555555";
const WORKER: &str = "33333333-3333-3333-3333-333333333333";

#[test]
fn coordinator_dispatches_collects_and_finalizes_without_driver_task_mutations() {
    exercise(false, false);
}

#[test]
fn coordinator_develops_reviews_and_delivers_to_predeclared_local_target() {
    exercise(true, false);
}

#[test]
fn coordinator_completes_stage_then_reaccepts_the_same_parent_before_finalizing() {
    exercise(true, true);
}

fn exercise(success: bool, staged: bool) {
    let f = Fixture::new();
    let target = f.temp.path().join("output.git");
    if success {
        git(
            f.temp.path(),
            &[
                "clone",
                "--bare",
                f.temp.path().join("repo").to_str().unwrap(),
                target.to_str().unwrap(),
            ],
        );
        git(&target, &["update-ref", "refs/heads/accepted", &f.commit]);
    }
    let mut env =
        isolated_runtime::linked_delivery(&f, "fixture", success.then_some(target.as_path()));
    let coord = task_runtime::prepare(&env, "coordinator").unwrap();
    let cwd = env.workspace_root.join("checkout");
    let (send, commands) = mpsc::channel::<J>();
    let (replies, receive) = mpsc::channel::<J>();
    let commands = Arc::new(Mutex::new(commands));
    let server_cwd = cwd.clone();
    let guardian = f
        .temp
        .path()
        .join(format!("rollout-guardian-{GUARDIAN}.jsonl"));
    if success {
        fs::write(&guardian, format!("{}\n",json!({"type":"session_meta","payload":{
            "id":GUARDIAN,"parent_thread_id":WORKER,"cwd":cwd,"source":{"subagent":{"other":"guardian"}}}}))).unwrap();
    }
    let shared_guardian = guardian.clone();
    let server = parallel(
        if staged {
            15
        } else if success {
            11
        } else {
            6
        },
        move |peer| {
            peer_loop(peer, &server_cwd, &commands, &replies, &shared_guardian);
        },
    );
    let mut broker = task_runtime::launch(
        &mut env,
        &coord,
        &json!({
            "adapter":"codex","configuration":"upstream","socket":server.path,
            "cwd":cwd,"model":{"providerID":"openai","id":"fixture","reasoning_effort":"high"},
            "role_models":{"developer":{"providerID":"openai","id":"developer-fixture","reasoning_effort":"medium"},
                "reviewer":{"providerID":"openai","id":"reviewer-fixture","reasoning_effort":"high"}},
            "driver_binary":env!("CARGO_BIN_EXE_herdr-flow")
        }),
    )
    .unwrap();
    let guard = coord.join("guard");
    task_runtime::native::receiver::bootstrap(
        &env,
        &coord,
        &json!({"guard_dir":guard,"task":"task","role":"coordinator"}),
    )
    .unwrap();
    task_runtime::native::receiver::active::activate(
        &mut env,
        &coord,
        &json!({"driver_binary":env!("CARGO_BIN_EXE_herdr-flow")}),
    )
    .unwrap();
    let state = util::read_state_json(&guard, "state.json").unwrap();
    let activation =
        task_runtime::native::receiver::recovery::recover(&mut env, &coord, &json!({})).unwrap();
    assert_eq!(activation["session"], state["session"]);
    {
        let _lock = queue::locked(&f.queue, false).unwrap();
        let mut q = f.state();
        q["guard_dir"] = json!(guard);
        q["receiver"] = guard::scope(&state);
        q["notify"] = json!(true);
        q["poll_seconds"] = json!(1);
        queue::save(&f.queue, &mut q).unwrap();
    }
    let workspace = env.workspace_root.clone();
    if !success {
        broker.kill().unwrap();
        broker.wait().unwrap();
        task_runtime::start_broker(
            &mut env,
            &coord,
            &json!({"driver_binary":env!("CARGO_BIN_EXE_herdr-flow")}),
        )
        .unwrap();
    }
    drop(env);
    let log = fs::File::create(f.temp.path().join("continuation-watch.log")).unwrap();
    let mut watcher = FixtureChild(
        Command::new(env!("CARGO_BIN_EXE_herdr-flow"))
            .args(["queue", "watch", "--state-dir"])
            .arg(&f.queue)
            .stdout(log.try_clone().unwrap())
            .stderr(log)
            .spawn()
            .unwrap(),
    );
    await_condition(|| {
        herdr_flow::watch_service::health(&queue::artifacts(&f.queue).unwrap())["healthy"] == true
    });
    await_condition(|| task_runtime::native::receiver::active::observe(&guard).is_ok());
    {
        let deadline = Instant::now() + Duration::from_secs(10);
        let env = loop {
            match task_env::restore_environment("task") {
                Ok(env) => break env,
                Err(error)
                    if error.kind == herdr_flow::ErrorKind::StateBusy
                        && !coord.join("receiver-start.json").exists()
                        && Instant::now() < deadline =>
                {
                    std::thread::sleep(Duration::from_millis(25))
                }
                Err(error) => panic!("coordinator environment restore: {error}"),
            }
        };
        loop {
            match task_runtime::native::receiver::start(
                &env,
                &coord,
                &json!({"instruction":"synthetic coordinator"}),
            ) {
                Ok(_) => break,
                Err(error)
                    if error.kind == herdr_flow::ErrorKind::StateBusy
                        && !coord.join("receiver-start.json").exists()
                        && Instant::now() < deadline =>
                {
                    std::thread::sleep(Duration::from_millis(25))
                }
                Err(error) => panic!("coordinator start: {error}"),
            }
        }
        assert!(task_runtime::native::receiver::start(
            &env,
            &coord,
            &json!({"instruction":"duplicate"})
        )
        .is_err());
    }
    // Invalid caller-supplied endpoint must fail before creating a worker.
    let bad = tool(
        &send,
        &receive,
        1,
        "run",
        json!({"wait":"dev","instruction":"fixture","socket":"/foreign"}),
    );
    assert_eq!(bad["success"], false);
    let model_override = tool(
        &send,
        &receive,
        8,
        "run",
        json!({
            "wait":"dev","instruction":"fixture","model":{"providerID":"other","id":"other"}
        }),
    );
    assert_eq!(model_override["success"], false);
    assert!(!coord.parent().unwrap().join("dev-a1").exists());
    let denied = tool(
        &send,
        &receive,
        5,
        "finalize",
        json!({"task":"task",
        "state_version":f.state()["tasks"]["task"]["state_version"],"outcome":"success","reason":"unproven success"}),
    );
    assert_eq!(denied["success"], false);
    assert!(f.state()["tasks"]["task"]["final_outcome"].is_null());
    let before = f.state();
    for (id, spec) in [
        (6, json!({"task":"foreign","kind":"candidate"})),
        (7, json!({"task":"task","kind":"revise","authorization":[]})),
    ] {
        assert_eq!(tool(&send, &receive, id, "lineage", spec)["success"], false);
    }
    assert_eq!(f.state()["tasks"], before["tasks"]);
    let run = tool(
        &send,
        &receive,
        2,
        "run",
        json!({"wait":"dev","instruction":"synthetic local development","evidence":[]}),
    );
    assert_eq!(
        run["success"],
        true,
        "{run}; {}",
        fs::read_to_string(f.temp.path().join("continuation-watch.log")).unwrap()
    );
    await_condition(|| {
        f.state()["events"]
            .as_object()
            .unwrap()
            .values()
            .any(|e| e["task"] == "dev")
    });
    let view = tool(&send, &receive, 3, "inspect", json!({}));
    assert_eq!(view["success"], true);
    let view: J = serde_json::from_str(view["contentItems"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(view["runtime_results"]["2"]["phase"], "signalled");
    assert_eq!(
        fs::read_to_string(cwd.join("developed")).unwrap(),
        "synthetic output\n"
    );
    if success {
        complete_review_delivery(
            &f,
            &send,
            &receive,
            &view["runtime_results"]["2"]["output"],
            &target,
            staged,
        );
    }
    let result = tool(
        &send,
        &receive,
        40,
        "finalize",
        json!({"task":"task",
        "state_version":f.state()["tasks"]["task"]["state_version"],"outcome":if success {"success"} else {"failure"},"reason":"explicit synthetic final decision"}),
    );
    assert_eq!(result["success"], true, "{result}");
    let deadline = Instant::now() + Duration::from_secs(30);
    while !f.state()["tasks"]["task"].is_null() {
        assert!(
            Instant::now() < deadline,
            "cleanup timed out: {}",
            fs::read_to_string(f.temp.path().join("continuation-watch.log")).unwrap()
        );
        std::thread::sleep(Duration::from_millis(25));
    }
    assert!(!workspace.exists() && !coord.exists());
    if success {
        assert_eq!(
            git(&target, &["show", "refs/heads/accepted:developed"]),
            "synthetic output"
        );
    }
    assert!(server.path.exists());
    assert!(!task_env::is_registered("task").unwrap());
    // Normal exit flushes subprocess coverage. This stops only the completed
    // test queue, after the finalizer has removed all task-owned resources.
    {
        let lock = loop {
            match queue::locked(&f.queue, false) {
                Ok(lock) => break lock,
                Err(error) if error.kind == herdr_flow::ErrorKind::StateBusy => {
                    std::thread::sleep(Duration::from_millis(25))
                }
                Err(error) => panic!("{error}"),
            }
        };
        let mut q = f.state();
        q["stopped"] = json!(true);
        queue::save(&f.queue, &mut q).unwrap();
        drop(lock);
    }
    await_condition(|| watcher.0.try_wait().unwrap().is_some());
    assert!(watcher.0.wait().unwrap().success());
    broker.wait().unwrap();
    server.finish();
    assert!(!guardian.exists());
}

fn tool(send: &mpsc::Sender<J>, receive: &mpsc::Receiver<J>, id: u64, action: &str, spec: J) -> J {
    send.send(json!({"id":id,"method":"item/tool/call","params":{"threadId":COORD,
        "turnId":"coordinator-turn","tool":"herdr_queue","arguments":{"action":action,"delivery":if action == "ack" {spec["delivery"].as_str().unwrap_or("")} else {""},"spec":spec.to_string()}}})).unwrap();
    let reply = receive
        .recv_timeout(Duration::from_secs(30))
        .unwrap_or_else(|error| {
            let registry = PathBuf::from(std::env::var_os("HERDR_TASK_ENV_ROOT").unwrap());
            let fixture = registry.ancestors().nth(3).unwrap();
            panic!(
                "tool {id} ({action}): {error}; watcher: {}",
                fs::read_to_string(fixture.join("continuation-watch.log")).unwrap_or_default()
            );
        });
    assert_eq!(reply["id"], id);
    reply["result"].clone()
}

fn await_condition(mut predicate: impl FnMut() -> bool) {
    let end = Instant::now() + Duration::from_secs(30);
    while !predicate() {
        assert!(Instant::now() < end, "continuation timed out");
        std::thread::sleep(Duration::from_millis(25));
    }
}

fn peer_loop(
    mut peer: rpc_server::Peer,
    cwd: &Path,
    commands: &Mutex<mpsc::Receiver<J>>,
    replies: &mpsc::Sender<J>,
    guardian: &Path,
) {
    use std::os::fd::AsRawFd;
    peer.initialize();
    let mut coordinator = false;
    let mut cwd = cwd.to_path_buf();
    let mut turns = json!([]);
    loop {
        if coordinator {
            if let Ok(command) = commands.lock().unwrap().try_recv() {
                peer.json(&command);
            }
        }
        let mut poll = libc::pollfd {
            fd: peer.stream.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: poll owns one valid descriptor for this call.
        if unsafe { libc::poll(&mut poll, 1, 10) } <= 0 {
            continue;
        }
        let mut byte = [0u8];
        // SAFETY: byte is valid and stream owns its descriptor.
        if unsafe {
            libc::recv(
                peer.stream.as_raw_fd(),
                byte.as_mut_ptr().cast(),
                1,
                libc::MSG_PEEK,
            )
        } <= 0
        {
            break;
        }
        let request = peer.request();
        if request["method"].is_null() {
            replies.send(request).unwrap();
            continue;
        }
        if request["method"] == "thread/read" && request["params"]["threadId"] == COORD {
            coordinator = true;
        }
        if request["method"] == "thread/start" {
            cwd = PathBuf::from(request["params"]["cwd"].as_str().unwrap());
            coordinator = request["params"]["dynamicTools"]
                .as_array()
                .is_some_and(|a| !a.is_empty());
        }
        let review = cwd.to_string_lossy().contains("/reviews/");
        let id = if coordinator {
            COORD
        } else if review && cwd.to_string_lossy().contains("overall-review") {
            "66666666-6666-6666-6666-666666666666"
        } else if review {
            "44444444-4444-4444-4444-444444444444"
        } else {
            WORKER
        };
        let selected_model = if coordinator {
            "fixture"
        } else if review {
            "reviewer-fixture"
        } else {
            "developer-fixture"
        };
        if matches!(
            request["method"].as_str(),
            Some("thread/start" | "turn/start")
        ) {
            assert_eq!(request["params"]["model"], selected_model);
        }
        let selected_effort = if coordinator || review {
            "high"
        } else {
            "medium"
        };
        if request["method"] == "thread/start" {
            assert_eq!(
                request["params"]["config"]["model_reasoning_effort"],
                selected_effort
            );
        }
        if request["method"] == "turn/start" {
            assert_eq!(request["params"]["effort"], selected_effort);
        }
        let thread = json!({"id":id,"cwd":cwd,"modelProvider":"openai","model":selected_model,
            "status":{"type":"idle"},"canAcceptDirectInput":true,"turns":turns});
        let result = match request["method"].as_str().unwrap() {
            "thread/start" | "thread/read" => {
                json!({"thread":thread,"model":selected_model,"modelProvider":"openai","reasoningEffort":selected_effort})
            }
            "turn/start" if coordinator => json!({"turn":{"id":"coordinator-turn"}}),
            "turn/start" if review => {
                assert_eq!(
                    fs::read_to_string(cwd.join("developed")).unwrap(),
                    "synthetic output\n"
                );
                fs::write(cwd.join(".herdr-verdict.json"), serde_json::to_vec(&json!({"conclusion":"passed","scope":"file","checks":[{"id":"content","criterion":"goal","source":"fixture","status":"passed","evidence":"read fixed developed file"}],"gaps":[],"reuses":[],"resolutions":[]})).unwrap()).unwrap();
                turns = json!([{"id":"worker-turn","status":"completed","items":[]}]);
                json!({"turn":{"id":"worker-turn"}})
            }
            "turn/start" => {
                fs::write(cwd.join("developed"), "synthetic output\n").unwrap();
                git(&cwd, &["add", "developed"]);
                git(
                    &cwd,
                    &[
                        "-c",
                        "user.name=Fixture",
                        "-c",
                        "user.email=fixture@example.test",
                        "commit",
                        "-qm",
                        "synthetic development",
                    ],
                );
                turns = json!([{"id":"worker-turn","status":"completed","items":[]}]);
                json!({"turn":{"id":"worker-turn"}})
            }
            "thread/turns/list" => json!({"data":turns,"nextCursor":null}),
            "thread/list" => {
                let rows = if guardian.exists()
                    && request["params"]["cwd"] == json!(cwd)
                    && request["params"]["archived"] == false
                {
                    json!([{"id":GUARDIAN,"cwd":cwd,"path":guardian,"parentThreadId":null,"source":{"subAgent":{"other":"guardian"}}}])
                } else {
                    json!([])
                };
                json!({"data":rows,"nextCursor":null})
            }
            "thread/delete" => {
                if request["params"]["threadId"] == GUARDIAN {
                    fs::remove_file(guardian).unwrap();
                }
                peer.json(&json!({"id":request["id"],"result":{}}));
                break;
            }
            other => panic!("unexpected method {other}"),
        };
        peer.json(&json!({"id":request["id"],"result":result}));
    }
}

fn checked(send: &mpsc::Sender<J>, receive: &mpsc::Receiver<J>, id: u64, action: &str, spec: J) {
    let result = tool(send, receive, id, action, spec);
    assert_eq!(result["success"], true, "{action}: {result}");
}

fn complete_review_delivery(
    f: &Fixture,
    send: &mpsc::Sender<J>,
    receive: &mpsc::Receiver<J>,
    output: &J,
    target: &Path,
    staged: bool,
) {
    await_condition(|| !f.state()["active"].is_null());
    checked(
        send,
        receive,
        10,
        "ack",
        json!({"delivery":f.state()["active"]}),
    );
    let q = f.state();
    let event = q["events"]
        .as_object()
        .unwrap()
        .iter()
        .find(|(_, e)| e["task"] == "dev")
        .unwrap()
        .0;
    checked(
        send,
        receive,
        11,
        "apply",
        json!({"task":"dev","state_version":q["tasks"]["dev"]["state_version"],"event":event,
        "output":output,"acceptance":{"goal":"synthetic development completed"},"release_evidence":"bound native turn completed",
        "next_action":{"kind":"complete"}}),
    );
    if staged {
        checked(
            send,
            receive,
            18,
            "lineage",
            json!({"task":"task",
            "state_version":f.state()["tasks"]["task"]["state_version"],
            "kind":"stage","id":"draft","scope":"deliver the developed file","criteria":["goal"]}),
        );
    }
    review_delivery(f, send, receive, output, target, ("review", 12, &f.commit));
    if staged {
        let before = f.state();
        let denied = tool(
            send,
            receive,
            19,
            "finalize",
            json!({"task":"task",
            "state_version":before["tasks"]["task"]["state_version"],"outcome":"success","reason":"stage is not final"}),
        );
        assert_eq!(denied["success"], false);
        assert!(f.state()["tasks"]["task"]["final_outcome"].is_null());
        let id = before["tasks"]["task"]["delivery"].clone();
        checked(send, receive, 20, "stage-complete", json!({"delivery":id}));
        let after = f.state();
        assert_eq!(after["tasks"]["task"]["control"], "running");
        assert!(after["tasks"]["task"]["approved"].is_null());
        assert_eq!(
            after["tasks"]["task"]["stage_delivery"]["phase"],
            "complete"
        );
        assert_eq!(before["lifecycle"], after["lifecycle"]);
        assert!(!progress::lineage::current(
            &after,
            &after["tasks"]["review"]
        ));
        let denied = tool(
            send,
            receive,
            21,
            "deliver",
            json!({"adapter":"git","target":target,
            "ref":"refs/heads/accepted","expected":output["id"],"authorization":"deliver-local",
            "state_version":after["tasks"]["task"]["state_version"]}),
        );
        assert_eq!(denied["success"], false);
        review_delivery(
            f,
            send,
            receive,
            output,
            target,
            ("overall-review", 30, output["id"].as_str().unwrap()),
        );
        let final_state = f.state();
        assert!(final_state["tasks"]["task"]["stage"].is_null());
        assert_eq!(final_state["tasks"]["task"]["approved"]["scope_epoch"], 2);
        assert_ne!(
            final_state["tasks"]["review"]["verdict"]["envelope"]["reviewer"],
            final_state["tasks"]["overall-review"]["verdict"]["envelope"]["reviewer"]
        );
    }
}

fn review_delivery(
    f: &Fixture,
    send: &mpsc::Sender<J>,
    receive: &mpsc::Receiver<J>,
    output: &J,
    target: &Path,
    review: (&str, u64, &str),
) {
    let (name, id, expected) = review;
    let q = f.state();
    checked(
        send,
        receive,
        id,
        "lineage",
        json!({"task":"task","state_version":q["tasks"]["task"]["state_version"],
        "kind":"candidate","candidate":output["id"],"reason":"developed candidate"}),
    );
    let q = f.state();
    checked(
        send,
        receive,
        id + 1,
        "lineage",
        json!({"task":"task","state_version":q["tasks"]["task"]["state_version"],
        "kind":"activity","activity":name,"role":"acceptance","input":output["id"],"session_epoch":1,
        "result_wait":{"id":name,"kind":"signal","task":name,"epoch":1,"assignment":format!("{name}-a1"), "owner":"reviewer","reason":"independent review","condition":"report","timeout_seconds":600}}),
    );
    checked(
        send,
        receive,
        id + 2,
        "run",
        json!({"wait":name,"instruction":"synthetic independent review"}),
    );
    await_condition(|| {
        f.state()["events"]
            .as_object()
            .unwrap()
            .values()
            .any(|e| e["task"] == name)
    });
    await_condition(|| !f.state()["active"].is_null());
    checked(
        send,
        receive,
        id + 3,
        "ack",
        json!({"delivery":f.state()["active"]}),
    );
    let q = f.state();
    let event = q["events"]
        .as_object()
        .unwrap()
        .iter()
        .find(|(_, e)| e["task"] == name)
        .unwrap()
        .0;
    checked(
        send,
        receive,
        id + 4,
        "apply",
        json!({"task":name,"state_version":q["tasks"][name]["state_version"],"event":event,
        "verdict":q["tasks"][name]["verdict"]["id"],"next_action":{"kind":"delivery","owner":"coordinator","reason":"passed","condition":"predeclared local delivery"}}),
    );
    let q = f.state();
    checked(
        send,
        receive,
        id + 5,
        "deliver",
        json!({"adapter":"git","target":target,"ref":"refs/heads/accepted","expected":expected,
        "authorization":"deliver-local","state_version":q["tasks"]["task"]["state_version"]}),
    );
}

pub(super) fn parallel(
    count: usize,
    run: impl Fn(rpc_server::Peer) + Send + Sync + 'static,
) -> rpc_server::Server {
    let run = std::sync::Arc::new(run);
    let mut children = Vec::new();
    let mut accepted = 0;
    rpc_server::Server::start_connections_with_timeout(
        count,
        Duration::from_secs(90),
        move |peer| {
            let run = run.clone();
            children.push(std::thread::spawn(move || run(peer)));
            accepted += 1;
            if accepted == count {
                for child in children.drain(..) {
                    child.join().unwrap();
                }
            }
        },
    )
}
