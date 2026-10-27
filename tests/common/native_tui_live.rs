//! Explicit installed Herdr/Codex acceptance. No full workflow acceptance claim.
use super::*;
use std::time::{Duration, Instant};

fn herdr(args: &[&str]) -> J {
    let output = Command::new("herdr").args(args).output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice::<J>(&output.stdout).unwrap()["result"].clone()
}
fn prompt(pane: &str, text: &str) {
    let output = Command::new("herdr")
        .args(["agent", "prompt", pane, text])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
fn await_turn(f: &Fixture, root: &Path, previous: Option<&str>) -> J {
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        {
            let _lock = queue::locked(&f.queue, false).unwrap();
            queue::tick(&f.queue, &herdr_flow::herdr::receiver_factory, None).unwrap();
        }
        let events = util::read_state_json(root, "broker-events.json").unwrap();
        let presentation = util::read_state_json(root, "presentation.json").unwrap();
        let marker = util::read_json(
            &PathBuf::from(presentation["directory"].as_str().unwrap()).join("tui.json"),
        )
        .unwrap();
        assert!(marker["status"] != "error", "{marker}");
        if let Some(turn) =
            events["turns"].as_object().unwrap().values().find(|v| {
                v["turn"]["id"].as_str() != previous && v["turn"]["status"] == "completed"
            })
        {
            return turn["turn"].clone();
        }
        assert!(
            Instant::now() < deadline,
            "acceptance observation deadline; no task replay"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn verify_tab(binding: &J, parent: &J) {
    let pane = herdr(&["pane", "get", binding["pane"].as_str().unwrap()]);
    assert_eq!(pane["pane"]["workspace_id"], parent["workspace_id"]);
    assert_eq!(pane["pane"]["tab_id"], binding["tab"]);
    assert_ne!(binding["tab"], parent["tab_id"]);
    let tab = herdr(&["tab", "get", binding["tab"].as_str().unwrap()]);
    assert_eq!(tab["tab"]["number"], binding["tab_number"]);
    assert_eq!(tab["tab"]["label"], "flow:native-coordinator");
    assert_eq!(tab["tab"]["pane_count"], 1);
}

fn reopen(env: &task_env::TaskEnvironment, root: &Path) -> J {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        match task_runtime::tui::open(env, root) {
            Ok(value) => return value,
            Err(error) => {
                assert!(error.message.contains("close is not persisted"), "{error}");
                assert!(Instant::now() < deadline, "{error}");
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    }
}

#[test]
#[ignore = "explicit existing upstream model/socket/native binary and Herdr; creates one disposable visible task"]
fn native_tui_same_thread_input_reopen_and_cleanup() {
    let socket = std::env::var("HERDR_UPSTREAM_SOCKET").unwrap();
    let model: J = serde_json::from_str(&std::env::var("HERDR_UPSTREAM_MODEL").unwrap()).unwrap();
    let binary = std::env::var("HERDR_NATIVE_BINARY").unwrap();
    let session_store = std::env::var("HERDR_SESSION_STORE").unwrap();
    let config_path = std::env::var("HERDR_UPSTREAM_CONFIG").unwrap();
    let config_before = fs::read(&config_path).unwrap();
    let parent = std::env::var("HERDR_PANE_ID").unwrap();
    let parent_before = herdr(&["pane", "get", &parent])["pane"].clone();
    let f = std::mem::ManuallyDrop::new(Fixture::new());
    eprintln!("native TUI evidence root: {}", f.temp.path().display());
    let mut env = f.ready();
    let root = task_runtime::prepare(&env, "native-coordinator").unwrap();
    let cwd = env.workspace_root.join("checkout");
    let mut broker = task_runtime::launch(
        &mut env,
        &root,
        &json!({
            "adapter":"codex","configuration":"upstream","socket":socket,"cwd":cwd,"model":model,
            "approval_policy":"on-request","driver_binary":env!("CARGO_BIN_EXE_herdr-flow"),
            "native_tui":{"binary":binary,"parent_pane":parent,"session_store":session_store}
        }),
    )
    .unwrap();
    let guard = root.join("guard");
    let state = task_runtime::native::receiver::bootstrap(
        &env,
        &root,
        &json!({"guard_dir":guard,"task":"task","role":"coordinator"}),
    )
    .unwrap();
    task_runtime::native::receiver::active::activate(
        &mut env,
        &root,
        &json!({"driver_binary":env!("CARGO_BIN_EXE_herdr-flow")}),
    )
    .unwrap();
    let active = util::read_state_json(&guard, "state.json").unwrap();
    {
        let _lock = queue::locked(&f.queue, false).unwrap();
        let mut q = f.state();
        q["guard_dir"] = json!(guard);
        q["receiver"] = guard::scope(&active);
        queue::save(&f.queue, &mut q).unwrap();
    }
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let started=task_runtime::native::receiver::start(&env,&root,&json!({
            "instruction":"This is native terminal integration acceptance. Remember passphrase VISIBLE-TASK-42. Reply exactly READY VISIBLE-TASK-42. Do not use tools or change files."
        })).unwrap();
        util::atomic_write_json(&f.temp.path().join("native-start.json"), &started).unwrap();
        if started["presentation"]["phase"] != "ready" {
            let record = util::read_state_json(&root, "presentation.json").unwrap();
            let pane = record["pane"].as_str().unwrap();
            let output = Command::new("herdr")
                .args([
                    "pane",
                    "read",
                    pane,
                    "--source",
                    "recent-unwrapped",
                    "--lines",
                    "100",
                ])
                .output()
                .unwrap();
            let marker_path = PathBuf::from(record["directory"].as_str().unwrap()).join("tui.json");
            let marker = fs::read_to_string(marker_path).unwrap_or_default();
            util::atomic_write_json(&f.temp.path().join("native-start-failure.json"),&json!({"binding":record,"marker":marker,"screen":String::from_utf8_lossy(&output.stdout)})).unwrap();
        }
        assert_eq!(started["presentation"]["phase"], "ready", "{started}");
        let first = await_turn(&f, &root, None);
        let attached = task_runtime::tui::status(&env, &root).unwrap();
        assert_eq!(attached["session"], state["session"]);
        verify_tab(&attached, &parent_before);
        let pane = attached["pane"].as_str().unwrap();
        let agent = herdr(&["agent", "get", pane]);
        util::atomic_write_json(&f.temp.path().join("native-agent.json"), &agent).unwrap();
        assert!(serde_json::to_string(&agent)
            .unwrap()
            .contains(state["session"].as_str().unwrap()));
        prompt(pane,"Call the herdr_queue tool once with action inspect, empty delivery and empty spec. Then reply exactly TOOLS VISIBLE-TASK-42. Do nothing else.");
        let second = await_turn(&f, &root, first["id"].as_str());
        let tools = util::read_state_json(&root, "control-tools.json").unwrap();
        assert!(tools
            .as_object()
            .unwrap()
            .values()
            .any(|v| v["phase"] == "complete" && v["result"]["success"] == true));
        task_runtime::tui::close(&env, &root).unwrap();
        let reopened = reopen(&env, &root);
        assert_eq!(reopened["session"], state["session"]);
        assert_ne!(reopened["terminal"], attached["terminal"]);
        assert_ne!(reopened["tab"], attached["tab"]);
        verify_tab(&reopened, &parent_before);
        // Native quit/reopen is presentation only; no repeat of the first task.
        assert_eq!(
            util::read_state_json(&root, "receiver-start.json").unwrap()["turn"],
            started["turn"]
        );
        let pane = reopened["pane"].as_str().unwrap();
        prompt(
            pane,
            "Reply exactly REOPENED followed by the passphrase from my first message. No tools.",
        );
        let deadline = Instant::now() + Duration::from_secs(120);
        loop {
            let events = util::read_state_json(&root, "broker-events.json").unwrap();
            let count = events["turns"]
                .as_object()
                .unwrap()
                .values()
                .filter(|v| v["turn"]["status"] == "completed")
                .count();
            if count == 3 {
                break;
            }
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(100));
        }
        let deadline = Instant::now() + Duration::from_secs(30);
        let screen = loop {
            let output = Command::new("herdr")
                .args([
                    "agent",
                    "read",
                    pane,
                    "--source",
                    "recent-unwrapped",
                    "--lines",
                    "100",
                ])
                .output()
                .unwrap();
            let screen = String::from_utf8_lossy(&output.stdout).into_owned();
            if output.status.success() && screen.contains("REOPENED VISIBLE-TASK-42") {
                break screen;
            }
            assert!(
                Instant::now() < deadline,
                "native display did not catch up: {screen}; {}",
                String::from_utf8_lossy(&output.stderr)
            );
            std::thread::sleep(Duration::from_millis(100));
        };
        util::atomic_write_json(&f.temp.path().join("native-result.json"),&json!({
            "session":state["session"],"same_thread_input":true,"dynamic_tool_single_owner":true,
            "reopened":reopened,"first_turn":first["id"],"second_turn":second["id"],"screen":screen,"tools":tools
        })).unwrap();
    }));
    if let Err(error) = &outcome {
        let message = error
            .downcast_ref::<String>()
            .cloned()
            .or_else(|| error.downcast_ref::<&str>().map(|s| s.to_string()));
        util::atomic_write_json(
            &f.temp.path().join("native-panic.json"),
            &json!({"message":message}),
        )
        .unwrap();
        if let Ok(record) = util::read_state_json(&root, "presentation.json") {
            let directory = PathBuf::from(record["directory"].as_str().unwrap());
            let marker = fs::read_to_string(directory.join("tui.json")).unwrap_or_default();
            let transport =
                util::read_json(&directory.join("transport-error.json")).unwrap_or(J::Null);
            let output = Command::new("herdr")
                .args([
                    "pane",
                    "read",
                    record["pane"].as_str().unwrap(),
                    "--source",
                    "visible",
                ])
                .output()
                .unwrap();
            util::atomic_write_json(&f.temp.path().join("native-failure.json"),&json!({"binding":record,"marker":marker,"transport":transport,"screen":String::from_utf8_lossy(&output.stdout)})).unwrap();
        }
    }
    util::atomic_write_json(
        &f.temp.path().join("inventory.json"),
        &json!([{
            "runtime":util::read_state_json(&root,"runtime.json").unwrap(),
            "session":util::read_state_json(&root,"upstream-session.json").unwrap(),
            "presentation":util::read_state_json(&root,"presentation.json").ok()
        }]),
    )
    .unwrap();
    drop(env);
    upstream_runtime::finalize_with_retry(&f);
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let report = queue::finalization::drive(&f.queue).unwrap();
        if report.pending == 0 {
            break;
        }
        assert!(Instant::now() < deadline, "{report:?}");
        std::thread::sleep(Duration::from_millis(50));
    }
    broker.wait().unwrap();
    assert!(!root.exists() && !cwd.exists());
    assert!(!task_env::is_registered("task").unwrap());
    assert_eq!(fs::read(&config_path).unwrap(), config_before);
    assert_eq!(
        herdr(&["pane", "get", &parent])["pane"]["terminal_id"],
        parent_before["terminal_id"]
    );
    assert_eq!(
        herdr(&["pane", "get", &parent])["pane"]["tab_id"],
        parent_before["tab_id"]
    );
    util::atomic_write_json(
        &f.temp.path().join("native-cleanup.json"),
        &json!({"task_roots_removed":true,"registry_released":true,"parent_unchanged":true}),
    )
    .unwrap();
    if let Err(error) = outcome {
        std::panic::resume_unwind(error);
    }
}
