//! Deterministic ownership/recovery checks; no installed Herdr or account needed.
use super::*;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::{fs::PermissionsExt, net::UnixListener};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};

struct Server {
    state: Arc<Mutex<J>>,
    stopped: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
    socket: J,
}
impl Server {
    fn new(path: &Path, state: J) -> Self {
        let listener = UnixListener::bind(path).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let state = Arc::new(Mutex::new(state));
        let stopped = Arc::new(AtomicBool::new(false));
        let (data, stop) = (state.clone(), stopped.clone());
        let thread = std::thread::spawn(move || {
            while !stop.load(Ordering::SeqCst) {
                let Ok((mut stream, _)) = listener.accept() else {
                    std::thread::sleep(Duration::from_millis(1));
                    continue;
                };
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut line = String::new();
                BufReader::new(stream.try_clone().unwrap())
                    .read_line(&mut line)
                    .unwrap();
                // Deliberately lacks the upstream WebSocket capability: config
                // discovery must fail instead of guessing/rewriting hooks.
                if line.starts_with("GET ") {
                    continue;
                }
                let request: J = serde_json::from_str(&line).unwrap();
                let mut state = data.lock().unwrap();
                state["methods"]
                    .as_array_mut()
                    .unwrap()
                    .push(request["method"].clone());
                let mut reply = reply(&request, &mut state);
                reply["id"] = request["id"].clone();
                writeln!(stream, "{reply}").unwrap();
            }
        });
        Self {
            state,
            stopped,
            thread: Some(thread),
            socket: upstream::socket_identity(path).unwrap(),
        }
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::SeqCst);
        self.thread.take().unwrap().join().unwrap();
    }
}
fn reply(request: &J, state: &mut J) -> J {
    if request["method"] == "thread/read" {
        return materialized_reply(state);
    }
    match request["method"].as_str().unwrap() {
        "pane.get" if request["params"]["pane_id"] == "parent-pane" => {
            json!({"result":{"pane":state["parent"]}})
        }
        "pane.get" | "agent.get" if state["present"] == false => {
            json!({"error":{"code":"pane_not_found","message":"removed"}})
        }
        "agent.get" if state["agent_pending"] == true => {
            state["agent_pending"] = json!(false);
            json!({"error":{"code":"agent_not_found","message":"still starting"}})
        }
        "pane.get" => json!({"result":{"pane":state["pane"]}}),
        "tab.create" => {
            assert_eq!(request["params"]["workspace_id"], "parent-workspace");
            assert_eq!(request["params"]["cwd"], state["cwd"]);
            assert_eq!(request["params"]["focus"], false);
            assert_eq!(request["params"]["label"], "flow:native");
            json!({"result":{"tab":{"tab_id":"owned-tab","workspace_id":"parent-workspace","number":42},"root_pane":state["pane"]}})
        }
        "agent.get" => json!({"result":{"agent":state["pane"]}}),
        "pane.process_info" => {
            json!({"result":{"process_info":{"shell_pid":std::process::id(),"foreground_processes":[]}}})
        }
        _ => mutation_reply(request, state),
    }
}

fn mutation_reply(request: &J, state: &mut J) -> J {
    match request["method"].as_str().unwrap() {
        "pane.report_agent_session" | "pane.rename" => json!({"result":{}}),
        "pane.close" => {
            state["present"] = json!(false);
            json!({"result":{}})
        }
        method => panic!("unexpected Herdr mutation: {method}"),
    }
}

fn materialized_reply(state: &mut J) -> J {
    if state["empty_reads"] == true {
        state["empty_reads"] = json!(false);
        return json!({"error":{"code":-32600,"message":"no rollout"}});
    }
    json!({"result":{"thread":{"id":"owned"}}})
}

#[test]
#[ignore = "private native entry subprocess; never accesses a real account"]
fn attachment_serve_worker() {
    let path = std::env::var("HERDR_TUI_FIXTURE_DIRECTORY").unwrap();
    assert!(serve(Path::new(&path)).is_err()); // Invalid synthetic thread is refused.
}
struct Vars(Vec<(String, Option<std::ffi::OsString>)>);

struct Child(std::process::Child);
impl Drop for Child {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
impl Vars {
    fn set(values: &[(&str, String)]) -> Self {
        Self(
            values
                .iter()
                .map(|(k, v)| {
                    let old = std::env::var_os(k);
                    std::env::set_var(k, v);
                    (k.to_string(), old)
                })
                .collect(),
        )
    }
}
impl Drop for Vars {
    fn drop(&mut self) {
        for (key, value) in &self.0 {
            match value {
                Some(value) => std::env::set_var(key, value),
                None => std::env::remove_var(key),
            }
        }
    }
}

#[test]
fn attachment_recovery_checks_real_identity_and_never_recreates_a_pane() {
    let temp = util::fs::test_tempdir();
    let _root_guard = crate::task_env::test_env_root(&temp.path().join("registry"));
    let mut env = crate::task_env::create_environment(
        "tui-fixture",
        &temp.path().join("workspace"),
        vec![],
        &temp.path().join("scratch"),
    )
    .unwrap();
    let root = super::super::prepare(&env, "native").unwrap();
    let server = Server::new(
        &temp.path().join("herdr.sock"),
        json!({"methods":[],"present":true,"agent_pending":true,"cwd":env.workspace_root,
        "parent":{"pane_id":"parent-pane","terminal_id":"parent-terminal","workspace_id":"parent-workspace","tab_id":"parent-tab"},
        "pane":{"workspace_id":"parent-workspace","tab_id":"owned-tab",
        "pane_id":"owned-pane","terminal_id":"owned-terminal","agent":"codex","agent_status":"idle","agent_session":{"value":"owned"}}}),
    );
    let mut owned_child = Child(env.command("/bin/sleep").arg("120").spawn().unwrap());
    let child = &mut owned_child.0;
    env.register_process(child.id() as i32).unwrap();
    let mut runtime = json!({"adapter":"codex","configuration":"upstream","root":root,"instance":env.instance_id,"pid":child.id(),
        "start_time":crate::task_env::process_start_time(child.id() as i32),"cwd":env.workspace_root,"model":{"id":"selected","providerID":"existing","reasoning_effort":"high"},
        "upstream_socket":server.socket,"native_tui":{"binary":"/bin/true","parent_pane":"parent-pane","parent_terminal":"parent-terminal","herdr_socket":server.socket},"native_tui_hooks":{}});
    util::atomic_write_json(&root.join("runtime.json"), &runtime).unwrap();
    util::atomic_write_json(
        &root.join("upstream-session.json"),
        &json!({"phase":"created","instance":env.instance_id,"session":"owned"}),
    )
    .unwrap();
    assert_eq!(status(&env, &root).unwrap()["phase"], "not_opened");
    let _broker = Server::new(
        &root.join("broker.sock"),
        json!({"methods":[],"empty_reads":true}),
    );
    let bin = temp.path().join("bin");
    std::fs::create_dir(&bin).unwrap();
    std::fs::write(bin.join("herdr"), "#!/bin/sh\nexit 7\n").unwrap();
    std::fs::set_permissions(bin.join("herdr"), std::fs::Permissions::from_mode(0o700)).unwrap();
    let _path = Vars::set(&[(
        "PATH",
        format!("{}:{}", bin.display(), std::env::var("PATH").unwrap()),
    )]);
    assert!(open(&env, &root).is_err()); // Failed launch retains exactly one attempt.
    let record = binding(&root).unwrap().unwrap();
    assert_eq!(record["phase"], "launching");
    assert_eq!(record["workspace"], "parent-workspace");
    assert_eq!(record["tab"], "owned-tab");
    assert_ne!(record["tab"], record["parent_tab"]);
    assert!(!attachment_ready(&record, &runtime).unwrap());
    let ticks = util::process::process_start_ticks(child.id()).unwrap();
    let mut marker = json!({"status":"ready","nonce":record["nonce"],"session":"owned","binary":"/bin/true","server_socket":server.socket["path"],
        "pid":child.id(),"wrapper_pid":child.id(),"pid_start_ticks":ticks,"wrapper_start_ticks":ticks,
        "runtime":{"cwd":env.workspace_root,"model":"selected","modelProvider":"existing","reasoningEffort":"high"}});
    let directory = PathBuf::from(record["directory"].as_str().unwrap());
    marker["status"] = json!("error");
    util::atomic_write_json(&directory.join("tui.json"), &marker).unwrap();
    assert!(attachment_ready(&record, &runtime).is_err());
    marker["status"] = json!("ready");
    util::atomic_write_json(&directory.join("tui.json"), &marker).unwrap();
    let ready = open(&env, &root).unwrap();
    assert_eq!(ready["phase"], "ready");
    assert_eq!(open(&env, &root).unwrap(), ready);
    assert_eq!(status(&env, &root).unwrap()["processes_alive"], true);
    assert_eq!(obligations(&env).unwrap(), vec![ready.clone()]);
    recovery_notice(&root, &runtime, "recovery result ready").unwrap();
    let owner = crate::lifecycle::Owner {
        queue_root: temp.path().join("queue"),
        queue_id: "fixture".into(),
        task_id: env.task_id.clone(),
        epoch: 1,
        instance_id: env.instance_id.clone(),
    };
    let mut recovery_runtime = runtime.clone();
    assert!(verify_recovery_runtime(&root, &recovery_runtime, &owner).is_err());
    recovery_runtime["recovery_role"] = json!(true);
    verify_recovery_runtime(&root, &recovery_runtime, &owner).unwrap();
    recovery_runtime["start_time"] = json!(0);
    assert!(verify_recovery_runtime(&root, &recovery_runtime, &owner).is_err());
    {
        use crate::codex::tui::BridgePolicy;
        let mut policy = protocol::Policy::new(record.clone(), runtime.clone());
        policy.observe(&marker).unwrap();
        assert!(!policy.explicit_cwd());
        assert!(policy
            .arguments()
            .contains(&"history.persistence=\"none\"".to_string()));
        policy.connected();
        policy.forward(true, b"GET / HTTP/1.1\r\n\r\n").unwrap();
        let message = json!({"id":9,"method":"thread/start"});
        let frame =
            crate::codex::rpc::encode_frame(&serde_json::to_vec(&message).unwrap(), 1, true, true)
                .unwrap();
        assert!(policy.forward(true, &frame).unwrap().is_empty());
        assert!(!policy.take_responses().is_empty());
        policy.connected();
        let socket = PathBuf::from(server.socket["path"].as_str().unwrap());
        let original = socket.with_extension("original");
        std::fs::rename(&socket, &original).unwrap();
        let replacement = UnixListener::bind(&socket).unwrap();
        std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(policy.forward(true, b"GET / HTTP/1.1\r\n\r\n").is_err());
        drop(replacement);
        std::fs::remove_file(&socket).unwrap();
        std::fs::rename(original, socket).unwrap();
    }
    assert_eq!(
        server.state.lock().unwrap()["methods"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|m| *m == "tab.create")
            .count(),
        1
    );

    marker["runtime"]["model"] = json!("changed");
    util::atomic_write_json(&directory.join("tui.json"), &marker).unwrap();
    assert!(open(&env, &root).is_err());
    assert_eq!(after_submission(&env, &root)["task_replay_allowed"], false);
    marker["runtime"]["model"] = json!("selected");
    util::atomic_write_json(&directory.join("tui.json"), &marker).unwrap();
    server.state.lock().unwrap()["pane"]["agent_session"]["value"] = json!("user-session");
    assert!(close(&env, &root).is_err());
    assert!(recovery_notice(&root, &runtime, "recovery pending").is_err());
    assert_eq!(server.state.lock().unwrap()["present"], true);
    server.state.lock().unwrap()["pane"]["agent_session"]["value"] = json!("owned");

    let _vars = Vars::set(&[
        ("HERDR_ENV", "1".into()),
        ("HERDR_PANE_ID", "owned-pane".into()),
        (
            "HERDR_SOCKET_PATH",
            server.socket["path"].as_str().unwrap().into(),
        ),
        (crate::task_env::MARKER_ENV, env.instance_id.clone()),
    ]);
    assert!(configure(&mut runtime).is_err()); // Missing hooks/list capability.
    assert_eq!(
        thread_config(&root, &runtime["model"]).unwrap()["history"]["persistence"],
        "none"
    );
    assert!(serve(&directory).is_err()); // Launch record was superseded by ready.
    util::atomic_write_json(&root.join("presentation.json"), &record).unwrap();
    {
        let _foreign = Vars::set(&[("HERDR_PANE_ID", "user-pane".into())]);
        assert!(serve_admission(&root, &record, &runtime).is_err());
    }
    serve_admission(&root, &record, &runtime).unwrap();
    assert!(task_bridge(&directory, &record, &runtime).is_ok());
    assert!(std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "task_runtime::tui::fixture::attachment_serve_worker",
            "--ignored"
        ])
        .env("HERDR_TUI_FIXTURE_DIRECTORY", &directory)
        .status()
        .unwrap()
        .success());
    assert_eq!(close(&env, &root).unwrap()["phase"], "closed");
    assert_eq!(close(&env, &root).unwrap()["phase"], "closed");
    assert_eq!(status(&env, &root).unwrap()["pane_present"], false);
    child.kill().unwrap();
    child.wait().unwrap();
    crate::task_env::destroy_environment(&env).unwrap();
}
