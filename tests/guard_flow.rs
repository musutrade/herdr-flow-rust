//! Guard transcript, checkpoint, rotation and review state-machine tests.

pub mod common;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use herdr_flow::guard;
use herdr_flow::herdr::GuardAdapter;
use herdr_flow::model::{self, J};
use herdr_flow::util;
use serde_json::{json, Value};
use tempfile::TempDir;

const OLD: &str = "11111111-1111-1111-1111-111111111111";
const NEW: &str = "22222222-2222-2222-2222-222222222222";
const SHA: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

#[test]
fn init_request_rejects_invalid_identity_before_reading_or_writing_state() {
    let temp = common::tempdir().unwrap();
    let transcript = temp.path().join("missing-transcript.jsonl");
    for (session, epoch) in [("invalid-session", 1), (OLD, -1)] {
        let request = guard::InitRequest {
            root: temp.path(),
            pane: "w1:p1",
            session,
            transcript_path: &transcript,
            task: "o/r#1",
            epoch,
            role: "developer",
            sessions_dir: temp.path(),
            app_socket: None,
            codex_binary: None,
        };
        let error = guard::init(&request).unwrap_err();
        assert_eq!(error.message, "invalid session or epoch");
        assert!(!temp.path().join("state.json").exists());
        assert!(!transcript.exists());
    }
}

#[derive(Clone)]
struct FakeState {
    runtime: J,
    current: String,
    busy: bool,
    shell: bool,
    ready: bool,
    fail: Option<String>,
    messages: Vec<String>,
    exits: usize,
    launches: usize,
}

struct FakeGuard {
    state: J,
    shared: Arc<Mutex<FakeState>>,
}

impl FakeGuard {
    fn new(shared: Arc<Mutex<FakeState>>) -> Self {
        Self {
            state: json!({}),
            shared,
        }
    }
    fn shared(&self) -> std::sync::MutexGuard<'_, FakeState> {
        self.shared.lock().unwrap()
    }
}

impl GuardAdapter for FakeGuard {
    fn set_state(&mut self, state: &J) {
        self.state = state.clone();
    }
    fn state(&self) -> J {
        self.state.clone()
    }
    fn call(&self, _args: &[String]) -> herdr_flow::Result<J> {
        Ok(json!({}))
    }
    fn rpc(&self, _method: &str, params: &J) -> herdr_flow::Result<J> {
        let mut shared = self.shared.lock().unwrap();
        if shared.fail.as_deref() == Some("policy") {
            return Err(herdr_flow::FlowError::guard(
                "ambiguous settings submission",
            ));
        }
        let mut runtime = shared.runtime.clone();
        runtime["approvalPolicy"] = params.get("approvalPolicy").cloned().unwrap_or(Value::Null);
        runtime["approvalsReviewer"] = params
            .get("approvalsReviewer")
            .cloned()
            .unwrap_or(Value::Null);
        if let Some(sandbox) = params.get("sandboxPolicy") {
            runtime["sandbox"] = sandbox.clone();
        }
        shared.runtime = runtime;
        Ok(json!({}))
    }
    fn check_capabilities(&self) -> herdr_flow::Result<J> {
        Ok(json!({"available": true, "missing": []}))
    }
    fn agent(&self) -> herdr_flow::Result<J> {
        Ok(json!({
            "agent_session": {"value": self.shared().current},
            "agent_status": if self.shared().busy { "working" } else { "idle" },
            "interactive_ready": !self.shared().busy
        }))
    }
    fn idle(&self) -> herdr_flow::Result<bool> {
        Ok(!self.shared().busy)
    }
    fn session(&self) -> herdr_flow::Result<String> {
        Ok(self.shared().current.clone())
    }
    fn snapshot(&self, _session: Option<&str>) -> herdr_flow::Result<J> {
        Ok(self.shared().runtime.clone())
    }
    fn prompt(&self, text: &str) -> herdr_flow::Result<String> {
        let mut shared = self.shared.lock().unwrap();
        shared.messages.push(text.to_string());
        if matches!(shared.fail.as_deref(), Some("prompt") | Some("true")) {
            return Err(herdr_flow::FlowError::guard("ambiguous submission"));
        }
        Ok(format!("turn-{}", shared.messages.len()))
    }
    fn exit_tui(&self) -> herdr_flow::Result<()> {
        let mut shared = self.shared.lock().unwrap();
        shared.exits += 1;
        if shared.fail.as_deref() == Some("exit") {
            return Err(herdr_flow::FlowError::guard("ambiguous exit"));
        }
        Ok(())
    }
    fn shell_ready(&self) -> herdr_flow::Result<bool> {
        Ok(self.shared().shell)
    }
    fn launch_tui(&self, _root: &Path, _nonce: &str) -> herdr_flow::Result<()> {
        let mut shared = self.shared.lock().unwrap();
        shared.launches += 1;
        shared.current = NEW.to_string();
        if shared.fail.as_deref() == Some("launch") {
            return Err(herdr_flow::FlowError::guard("ambiguous launch"));
        }
        Ok(())
    }
    fn ready_tui(&self) -> herdr_flow::Result<Option<String>> {
        let shared = self.shared.lock().unwrap();
        Ok(if shared.ready {
            Some(shared.current.clone())
        } else {
            None
        })
    }
    fn verify_tui(&self, _expected: &str) -> herdr_flow::Result<J> {
        Ok(json!({"pid": 100, "start_ticks": "1"}))
    }
    fn marker(&self) -> herdr_flow::Result<Option<J>> {
        Ok(None)
    }
    fn target(&self) -> herdr_flow::Result<String> {
        Ok(self.shared().current.clone())
    }
    fn restore_runtime(&self, expected: &J, _session: &str) -> herdr_flow::Result<()> {
        let mut shared = self.shared.lock().unwrap();
        if shared.fail.as_deref() == Some("settings") {
            return Err(herdr_flow::FlowError::guard("ambiguous settings"));
        }
        shared.runtime = expected.clone();
        Ok(())
    }
    fn processes(&self) -> herdr_flow::Result<J> {
        Ok(json!({"shell_pid": 1, "foreground_processes": []}))
    }
    fn pane(&self) -> herdr_flow::Result<J> {
        Ok(json!({"terminal_id": "term"}))
    }
}

fn runtime() -> J {
    herdr_flow::herdr::runtime_identity(&json!({
        "model": "gpt-6-astra",
        "modelProvider": "openai",
        "reasoningEffort": "high",
        "cwd": "/checkout",
        "approvalPolicy": "on-request",
        "approvalsReviewer": "user",
        "sandbox": {"type": "readOnly", "networkAccess": false},
        "runtimeWorkspaceRoots": ["/checkout"],
        "multiAgentMode": "explicitRequestOnly"
    }))
    .unwrap()
}

fn git(repo: &Path, args: &[&str]) {
    let status = std::process::Command::new("git")
        .args(args)
        .current_dir(repo)
        .status()
        .unwrap();
    assert!(status.success());
}

fn write_log(path: &Path, session: &str, cwd: &Path, tokens: u64) {
    let rows = [
        json!({"type": "session_meta", "payload": {"id": session, "cwd": cwd.display().to_string()}}),
        json!({"type": "event_msg", "payload": {"type": "token_count", "info": {
            "last_token_usage": {"total_tokens": tokens},
            "total_token_usage": {"total_tokens": 99999999},
            "model_context_window": 258400
        }}}),
    ];
    let mut text = String::new();
    for row in rows {
        text.push_str(&serde_json::to_string(&row).unwrap());
        text.push('\n');
    }
    std::fs::write(path, text).unwrap();
}

struct Harness {
    _tmp: TempDir,
    base: PathBuf,
    repo: PathBuf,
    root: PathBuf,
    handoff: PathBuf,
    log: PathBuf,
    shared: Arc<Mutex<FakeState>>,
}

impl Harness {
    fn new() -> Self {
        let tmp = common::tempdir().unwrap();
        let base = tmp.path().to_path_buf();
        let repo = base.join("repo");
        common::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "-q"]);
        git(&repo, &["config", "user.email", "test@example.invalid"]);
        git(&repo, &["config", "user.name", "Test"]);
        std::fs::write(repo.join("a"), "initial").unwrap();
        git(&repo, &["add", "a"]);
        git(&repo, &["commit", "-qm", "initial"]);
        let root = base.join("state");
        common::create_dir_all(&root).unwrap();
        let log = base.join(format!("rollout-{OLD}.jsonl"));
        write_log(&log, OLD, &repo, 130000);
        let mut state = json!({
            "version": 2,
            "task": "test#1",
            "epoch": 1,
            "role": "dev",
            "pane": "w1:p1",
            "session": OLD,
            "transcript": log.display().to_string(),
            "sessions_dir": base.display().to_string(),
            "cwd": repo.display().to_string(),
            "phase": "monitoring",
            "auto_rotate": true,
            "warn_tokens": 80000,
            "rotate_tokens": 120000,
            "warn_ratio": 0.60,
            "rotate_ratio": 0.75
        });
        state["app_server_socket"] = json!("/socket");
        util::atomic_write_json(&root.join("state.json"), &state).unwrap();
        let handoff = base.join("input.json");
        util::atomic_write_json(
            &handoff,
            &json!({
                "task": "test#1",
                "epoch": 1,
                "cwd": repo.display().to_string(),
                "goal": "finish",
                "next_action": "finish",
                "authorization": [],
                "done_when": ["done"],
                "completed": [],
                "blockers": [],
                "preserve": [],
                "writer": null,
                "quiescent": true,
                "running_commands": []
            }),
        )
        .unwrap();
        let shared = Arc::new(Mutex::new(FakeState {
            runtime: runtime(),
            current: OLD.to_string(),
            busy: false,
            shell: true,
            ready: true,
            fail: None,
            messages: Vec::new(),
            exits: 0,
            launches: 0,
        }));
        Self {
            _tmp: tmp,
            base,
            repo,
            root,
            handoff,
            log,
            shared,
        }
    }

    fn fake(&self) -> FakeGuard {
        FakeGuard::new(self.shared.clone())
    }

    fn state(&self) -> J {
        util::read_json(&self.root.join("state.json")).unwrap()
    }

    fn complete_assignment(&self) -> J {
        let mut handoff = util::read_json(&self.handoff).unwrap();
        let evidence = self.base.join("result.json");
        util::atomic_write_json(&evidence, &json!({"result": "PASS:0"})).unwrap();
        handoff["assignment"] = json!({
            "id": "review-e1",
            "status": "completed",
            "evidence": evidence.display().to_string()
        });
        util::atomic_write_json(&self.handoff, &handoff).unwrap();
        guard::checkpoint(&self.root, &self.handoff, OLD).unwrap()
    }

    fn rotate(&self) -> FakeGuard {
        let mut h = self.fake();
        guard::checkpoint(&self.root, &self.handoff, OLD).unwrap();
        for phase in [
            "awaiting_shell",
            "awaiting_tui",
            "successor_ready",
            "awaiting_ack",
        ] {
            let s = guard::once(&self.root, &mut h).unwrap();
            assert_eq!(model::string(&s, "phase").as_deref(), Some(phase));
        }
        h
    }
}

#[test]
fn init_and_reconcile_reject_untrusted_roots_before_using_an_agent() {
    use std::os::unix::fs::PermissionsExt;
    let h = Harness::new();
    let request = guard::InitRequest {
        root: &h.root,
        pane: "w1:p1",
        session: OLD,
        transcript_path: &h.log,
        task: "test#1",
        epoch: 1,
        role: "dev",
        sessions_dir: &h.base,
        app_socket: None,
        codex_binary: None,
    };
    assert!(guard::init(&request)
        .unwrap_err()
        .message
        .contains("state exists"));
    std::fs::set_permissions(&h.root, std::fs::Permissions::from_mode(0o777)).unwrap();
    assert!(guard::init(&request)
        .unwrap_err()
        .message
        .contains("writable"));
    let mut adapter = FakeGuard::new(h.shared.clone());
    assert!(guard::reconcile(&h.root, OLD, &mut adapter, false)
        .unwrap_err()
        .message
        .contains("writable"));
    assert!(h.shared.lock().unwrap().messages.is_empty());
}

#[test]
fn transcript_tracks_usage_and_partial_records() {
    let h = Harness::new();
    let first = guard::transcript(&h.log).unwrap();
    assert_eq!(model::dig_int(&first, &["usage", "tokens"]), Some(130000));
    assert!(model::bool_or(&first, "caught_up", false));
    let cursor = first["cursor"].clone();
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&h.log)
        .unwrap();
    let record = format!(
        "{}\n",
        serde_json::to_string(&json!({
            "type": "response_item",
            "payload": {"type": "message", "role": "user"}
        }))
        .unwrap()
    );
    file.write_all(&record.as_bytes()[..10]).unwrap();
    file.flush().unwrap();
    let second = guard::transcript_state(&h.log, Some(&cursor)).unwrap();
    assert_eq!(model::dig_int(&second, &["usage", "tokens"]), Some(130000));
    assert!(!model::bool_or(&second, "caught_up", true));
    assert_eq!(model::int(&second, "user_messages"), Some(0));
    let cursor2 = second["cursor"].clone();
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&h.log)
        .unwrap();
    file.write_all(&record.as_bytes()[10..]).unwrap();
    file.flush().unwrap();
    let third = guard::transcript_state(&h.log, Some(&cursor2)).unwrap();
    assert_eq!(model::int(&third, "user_messages"), Some(1));
    assert!(model::bool_or(&third, "caught_up", false));
}

#[test]
fn checkpoint_and_rotation_reach_successor_monitoring() {
    let h = Harness::new();
    let fake = h.rotate();
    let state = h.state();
    assert_eq!(
        model::string(&state, "phase").as_deref(),
        Some("awaiting_ack")
    );
    assert_eq!(model::string(&state, "successor").as_deref(), Some(NEW));
    let shared = h.shared.lock().unwrap();
    assert_eq!((shared.exits, shared.launches), (1, 1));
    assert_eq!(shared.messages.len(), 1);
    drop(shared);
    let new_log = h.base.join(format!("rollout-{NEW}.jsonl"));
    write_log(&new_log, NEW, &h.repo, 1000);
    let state = guard::ack(&h.root, NEW).unwrap();
    assert_eq!(model::string(&state, "session").as_deref(), Some(NEW));
    assert_eq!(model::int(&state, "epoch"), Some(2));
    assert_eq!(
        model::string(&state, "phase").as_deref(),
        Some("monitoring")
    );
    let _ = fake;
}

#[test]
fn completed_assignment_parks_without_prompt() {
    let h = Harness::new();
    let s = h.complete_assignment();
    assert_eq!(model::string(&s, "phase").as_deref(), Some("parked"));
    let mut fake = h.fake();
    for _ in 0..3 {
        assert_eq!(
            model::string(&guard::once(&h.root, &mut fake).unwrap(), "phase").as_deref(),
            Some("parked")
        );
    }
    assert!(h.shared.lock().unwrap().messages.is_empty());
}

#[test]
fn pending_delivery_updates_usage_but_blocks_rotation() {
    let h = Harness::new();
    let mut state = h.state();
    state["external_delivery"] = json!({"delivery": "pending"});
    util::atomic_write_json(&h.root.join("state.json"), &state).unwrap();
    let mut fake = h.fake();
    let s = guard::once(&h.root, &mut fake).unwrap();
    assert_eq!(model::string(&s, "phase").as_deref(), Some("monitoring"));
    assert_eq!(
        model::string(&s, "pause_reason").as_deref(),
        Some("wait_queue_delivery_pending")
    );
    assert!(model::bool_or(&s, "rotation_due", false));
    assert!(h.shared.lock().unwrap().messages.is_empty());
    assert!(guard::checkpoint(&h.root, &h.handoff, OLD).is_err());
}

#[test]
fn ambiguous_prompt_is_not_replayed() {
    let h = Harness::new();
    h.shared.lock().unwrap().fail = Some("prompt".to_string());
    let mut fake = h.fake();
    assert!(guard::once(&h.root, &mut fake).is_err());
    assert_eq!(
        model::string(&h.state(), "phase").as_deref(),
        Some("request_sending")
    );
    assert!(guard::once(&h.root, &mut fake).is_err());
    assert_eq!(h.shared.lock().unwrap().messages.len(), 1);
}

#[test]
fn review_configuration_verifies_auto_review_runtime() {
    let h = Harness::new();
    let mut state = h.state();
    state["auto_rotate"] = json!(false);
    util::atomic_write_json(&h.root.join("state.json"), &state).unwrap();
    h.shared.lock().unwrap().runtime["sandbox"] = json!({"type": "dangerFullAccess"});
    let mut fake = h.fake();
    let s = guard::configure_review(&h.root, None, &mut fake, false).unwrap();
    assert_eq!(model::string(&s, "phase").as_deref(), Some("monitoring"));
    let policy = model::get(&s, "review_policy").cloned().unwrap();
    assert_eq!(
        model::string(&policy, "approvalsReviewer").as_deref(),
        Some("auto_review")
    );
    assert_eq!(
        model::dig_str(&policy, &["sandbox", "type"]).as_deref(),
        Some("workspaceWrite")
    );
    assert_eq!(
        model::dig(&policy, &["sandbox", "networkAccess"]),
        Some(&json!(false))
    );
}

#[test]
fn changed_checkout_prevents_fresh_session() {
    let h = Harness::new();
    guard::checkpoint(&h.root, &h.handoff, OLD).unwrap();
    std::fs::write(h.repo.join("a"), "changed").unwrap();
    let mut fake = h.fake();
    assert!(guard::once(&h.root, &mut fake).is_err());
    assert!(h.shared.lock().unwrap().messages.is_empty());
}

#[test]
fn handoff_completion_requires_existing_evidence_and_no_writer() {
    let h = Harness::new();
    let mut handoff = util::read_json(&h.handoff).unwrap();
    handoff["assignment"] = json!({
        "id": "review-e1",
        "status": "completed",
        "evidence": "/missing/result"
    });
    util::atomic_write_json(&h.handoff, &handoff).unwrap();
    assert!(guard::checkpoint(&h.root, &h.handoff, OLD).is_err());
    h.complete_assignment();
    let mut handoff = util::read_json(&h.handoff).unwrap();
    handoff["writer"] = json!("dev");
    util::atomic_write_json(&h.handoff, &handoff).unwrap();
    assert!(guard::checkpoint(&h.root, &h.handoff, OLD).is_err());
}

#[test]
fn diagnosis_distinguishes_telemetry_and_watcher_faults() {
    let h = Harness::new();
    let mut s = h.state();
    s["usage"] = json!({"tokens": 100, "window": 1000});
    assert_eq!(
        model::string(
            &guard::diagnosis(&s, &json!({"healthy": true})),
            "classification"
        )
        .as_deref(),
        Some("monitoring")
    );
    s["usage"] = Value::Null;
    assert_eq!(
        model::string(
            &guard::diagnosis(&s, &json!({"healthy": true})),
            "classification"
        )
        .as_deref(),
        Some("telemetry_unknown")
    );
    let fault = json!({"healthy": false, "reason": "watcher stopped", "next_step": "restart the verified watcher"});
    let diagnosis = guard::diagnosis(&s, &fault);
    assert_eq!(
        model::string(&diagnosis, "classification").as_deref(),
        Some("watcher_unhealthy")
    );
    assert_eq!(
        model::string(&diagnosis, "next_step").as_deref(),
        Some("restart the verified watcher")
    );
    s["external_delivery"] = json!({"delivery": "pending"});
    assert_eq!(
        model::string(
            &guard::diagnosis(&s, &json!({"healthy": true})),
            "classification"
        )
        .as_deref(),
        Some("external_delivery")
    );
}

#[test]
fn inside_checkout_uses_the_git_root() {
    let h = Harness::new();
    let sub = h.repo.join("pkg");
    common::create_dir_all(&sub).unwrap();
    assert!(guard::inside_checkout(&h.repo.join("state"), &sub));
    assert!(guard::inside_checkout(&sub.join("state"), &sub));
    assert!(!guard::inside_checkout(&h.base.join("state"), &sub));
    assert!(!guard::inside_checkout(
        &h.base.join("state"),
        &h.base.join("not-a-repo")
    ));
}

#[test]
fn fingerprint_binds_untracked_bytes_from_a_subdirectory() {
    let h = Harness::new();
    let sub = h.repo.join("sub");
    common::create_dir_all(&sub).unwrap();
    let before = guard::fingerprint(&sub.display().to_string()).unwrap();
    std::fs::write(h.repo.join("other"), "new").unwrap();
    let after = guard::fingerprint(&sub.display().to_string()).unwrap();
    assert_ne!(before, after);
    let _ = SHA;
}

#[test]
fn watch_unmanaged_environment_persists_needs_attention() {
    let h = Harness::new();
    let result = guard::watch(&h.root, None, true);
    assert!(result.is_ok(), "{result:?}");
    let state = h.state();
    assert_eq!(
        model::string(&state, "phase").as_deref(),
        Some("needs_attention"),
        "{state}"
    );
    assert!(model::string(&state, "error").is_some());
}

#[test]
fn watch_parked_state_does_not_require_the_adapter() {
    let h = Harness::new();
    let mut state = h.state();
    state["phase"] = json!("parked");
    util::atomic_write_json(&h.root.join("state.json"), &state).unwrap();
    let result = guard::watch(&h.root, None, true);
    assert!(result.is_ok(), "{result:?}");
    assert_eq!(
        model::string(&h.state(), "phase").as_deref(),
        Some("parked")
    );
}
