//! Bounded receiver creation journal/recovery regressions; synthetic HTTP only.
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use herdr_flow::opencode::session::{self, Api};
use herdr_flow::util;
use herdr_flow::{FlowError, Result};
use serde_json::{json, Value as J};
use tempfile::TempDir;

#[path = "common/runtime_http.rs"]
mod runtime_http;

#[derive(Clone, Copy, PartialEq, Eq)]
enum PostMode {
    Success,
    ErrorBeforeCreate,
    ErrorAfterCreate,
}

struct FakeState {
    health: J,
    session: J,
    messages: J,
    posts: usize,
    mode: PostMode,
}

#[derive(Clone)]
struct Fake(Arc<Mutex<FakeState>>);

impl Fake {
    fn new(cwd: &Path, model: &J) -> Self {
        Self(Arc::new(Mutex::new(FakeState {
            health: json!({"healthy":true,"version":"1.19.3"}),
            session: json!({"id":"sesreceiver","directory":cwd,"model":model}),
            messages: json!([]),
            posts: 0,
            mode: PostMode::Success,
        })))
    }

    fn posts(&self) -> usize {
        self.0.lock().unwrap().posts
    }

    fn set_mode(&self, mode: PostMode) {
        self.0.lock().unwrap().mode = mode;
    }

    fn set_health(&self, health: J) {
        self.0.lock().unwrap().health = health;
    }

    fn set_messages(&self, messages: J) {
        self.0.lock().unwrap().messages = messages;
    }

    fn session_mut(&self, update: impl FnOnce(&mut J)) {
        update(&mut self.0.lock().unwrap().session);
    }
}

impl Api for Fake {
    fn get(&self, path: &str) -> Result<J> {
        let state = self.0.lock().unwrap();
        match path {
            "/global/health" => Ok(state.health.clone()),
            path if path.ends_with("/message") => Ok(state.messages.clone()),
            path if path.starts_with("/session/") => Ok(state.session.clone()),
            _ => Err(FlowError::guard("unexpected fake GET")),
        }
    }

    fn post(&self, path: &str, _body: &J) -> Result<J> {
        assert_eq!(path, "/session");
        let mut state = self.0.lock().unwrap();
        state.posts += 1;
        match state.mode {
            PostMode::Success => Ok(json!({"id":"sesreceiver"})),
            PostMode::ErrorBeforeCreate => Err(FlowError::guard("fake creation rejected")),
            PostMode::ErrorAfterCreate => Err(FlowError::guard("fake ambiguous creation")),
        }
    }
}

struct Harness {
    dir: TempDir,
    guard: PathBuf,
    cwd: PathBuf,
    model: J,
}

impl Harness {
    fn new() -> Self {
        let dir = tempfile::Builder::new()
            .permissions(std::fs::Permissions::from_mode(0o700))
            .tempdir()
            .unwrap();
        let cwd = dir.path().join("work");
        std::fs::create_dir(&cwd).unwrap();
        let guard = dir.path().join("guard");
        let model = json!({"providerID":"openai","id":"fixture"});
        Self {
            dir,
            guard,
            cwd,
            model,
        }
    }

    fn spec(&self) -> J {
        json!({
            "endpoint": "http://127.0.0.1:12345",
            "cwd": self.cwd,
            "model": self.model,
            "title": "Receiver"
        })
    }

    fn journal(&self) -> J {
        util::read_json(&self.guard.join("creation.json")).unwrap()
    }

    fn state_exists(&self) -> bool {
        self.guard.join("state.json").exists()
    }

    fn write_journal(&self, value: &J) {
        util::atomic_write_json(&self.guard.join("creation.json"), value).unwrap();
    }
}

use std::os::unix::fs::PermissionsExt;

#[test]
fn create_fresh_registers_once_and_refuses_replay() {
    let harness = Harness::new();
    let fake = Fake::new(&harness.cwd, &harness.model);
    let state =
        session::create_fresh_with_api(&harness.guard, &harness.spec(), Box::new(fake.clone()))
            .unwrap();
    assert_eq!(state["session"], "sesreceiver");
    assert_eq!(state["cwd"], json!(harness.cwd));
    assert_eq!(state["phase"], "monitoring");
    assert!(harness.state_exists());
    assert_eq!(harness.journal()["phase"], "created");
    assert_eq!(fake.posts(), 1);

    let again =
        session::create_fresh_with_api(&harness.guard, &harness.spec(), Box::new(fake.clone()));
    assert!(again.is_err());
    assert_eq!(fake.posts(), 1);

    let recovered =
        session::recover_fresh_with_api(&harness.guard, Box::new(fake.clone())).unwrap();
    assert_eq!(recovered["session"], "sesreceiver");
    assert_eq!(fake.posts(), 1);
}

#[test]
fn ambiguous_creation_keeps_journal_and_never_replays() {
    let harness = Harness::new();
    let fake = Fake::new(&harness.cwd, &harness.model);
    fake.set_mode(PostMode::ErrorAfterCreate);
    let error =
        session::create_fresh_with_api(&harness.guard, &harness.spec(), Box::new(fake.clone()))
            .unwrap_err();
    assert!(error.message.contains("uncertain"));
    assert_eq!(harness.journal()["phase"], "uncertain");
    assert!(!harness.state_exists());
    assert_eq!(fake.posts(), 1);

    assert!(session::create_fresh_with_api(
        &harness.guard,
        &harness.spec(),
        Box::new(fake.clone())
    )
    .is_err());
    assert_eq!(fake.posts(), 1);
    assert!(session::recover_fresh_with_api(&harness.guard, Box::new(fake.clone())).is_err());
    assert_eq!(fake.posts(), 1);
    assert!(!harness.state_exists());
}

#[test]
fn rejected_creation_is_also_fail_closed() {
    let harness = Harness::new();
    let fake = Fake::new(&harness.cwd, &harness.model);
    fake.set_mode(PostMode::ErrorBeforeCreate);
    assert!(session::create_fresh_with_api(
        &harness.guard,
        &harness.spec(),
        Box::new(fake.clone())
    )
    .is_err());
    assert_eq!(harness.journal()["phase"], "uncertain");
    assert_eq!(fake.posts(), 1);
    assert!(session::create_fresh_with_api(
        &harness.guard,
        &harness.spec(),
        Box::new(fake.clone())
    )
    .is_err());
    assert_eq!(fake.posts(), 1);
}

#[test]
fn health_and_created_identity_mismatches_do_not_register() {
    let harness = Harness::new();
    let fake = Fake::new(&harness.cwd, &harness.model);
    fake.set_health(json!({"healthy":true,"version":"1.18.0"}));
    assert!(session::create_fresh_with_api(
        &harness.guard,
        &harness.spec(),
        Box::new(fake.clone())
    )
    .is_err());
    assert!(!harness.guard.join("creation.json").exists());
    assert!(!harness.state_exists());
    assert_eq!(fake.posts(), 0);

    fake.set_health(json!({"healthy":true,"version":"1.19.3"}));
    fake.session_mut(|session| session["directory"] = json!(harness.dir.path()));
    assert!(session::create_fresh_with_api(
        &harness.guard,
        &harness.spec(),
        Box::new(fake.clone())
    )
    .is_err());
    assert_eq!(harness.journal()["phase"], "created_unverified");
    assert!(!harness.state_exists());
    assert_eq!(fake.posts(), 1);
}

#[test]
fn prior_conversation_is_rejected_without_registering() {
    let harness = Harness::new();
    let fake = Fake::new(&harness.cwd, &harness.model);
    fake.set_messages(json!([{"info":{"role":"user"}}]));
    assert!(session::create_fresh_with_api(
        &harness.guard,
        &harness.spec(),
        Box::new(fake.clone())
    )
    .is_err());
    assert_eq!(harness.journal()["phase"], "created_unverified");
    assert!(!harness.state_exists());
}

#[test]
fn recovery_binds_a_created_journal_without_posting() {
    let harness = Harness::new();
    let fake = Fake::new(&harness.cwd, &harness.model);
    harness.write_journal(&json!({
        "phase": "created",
        "adapter": "opencode",
        "endpoint": "http://127.0.0.1:12345",
        "cwd": harness.cwd,
        "model": harness.model,
        "session": "sesreceiver",
        "title": "Receiver",
        "created_at": 1.0
    }));
    let state = session::recover_fresh_with_api(&harness.guard, Box::new(fake.clone())).unwrap();
    assert_eq!(state["session"], "sesreceiver");
    assert!(harness.state_exists());
    assert_eq!(fake.posts(), 0);

    let again = session::recover_fresh_with_api(&harness.guard, Box::new(fake.clone())).unwrap();
    assert_eq!(again["session"], "sesreceiver");
    assert_eq!(fake.posts(), 0);
}

#[test]
fn recovery_refuses_uncertain_journal() {
    let harness = Harness::new();
    let fake = Fake::new(&harness.cwd, &harness.model);
    harness.write_journal(&json!({
        "phase": "uncertain",
        "endpoint": "http://127.0.0.1:12345",
        "cwd": harness.cwd,
        "model": harness.model,
        "session": "sesreceiver"
    }));
    assert!(session::recover_fresh_with_api(&harness.guard, Box::new(fake.clone())).is_err());
    assert!(!harness.state_exists());
    assert_eq!(fake.posts(), 0);
}

#[test]
fn production_entry_creates_and_recovers_over_fixed_routes() {
    let harness = Harness::new();
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let cwd = harness.cwd.clone();
    std::thread::spawn(move || runtime_http::serve(listener, &cwd));
    let spec = json!({
        "endpoint": format!("http://127.0.0.1:{port}"),
        "cwd": harness.cwd,
        "model": harness.model
    });
    let state = session::create_fresh(&harness.guard, &spec).unwrap();
    assert_eq!(state["session"], "sesfixture");
    assert_eq!(state["phase"], "monitoring");
    assert_eq!(state["server_version"], "1.19.4");
    assert_eq!(harness.journal()["phase"], "created");

    let recovered = session::recover_fresh(&harness.guard).unwrap();
    assert_eq!(recovered["session"], "sesfixture");
    assert_eq!(recovered["server_version"], "1.19.4");
}

#[test]
fn patch_upgrade_persists_observed_version_and_recovers_old_state() {
    let harness = Harness::new();
    let fake = Fake::new(&harness.cwd, &harness.model);
    fake.set_health(json!({"healthy":true,"version":"1.19.4"}));
    let state =
        session::create_fresh_with_api(&harness.guard, &harness.spec(), Box::new(fake.clone()))
            .unwrap();
    assert_eq!(state["server_version"], "1.19.4");
    assert_eq!(harness.journal()["phase"], "created");
    assert_eq!(fake.posts(), 1);

    let mut persisted = util::read_json(&harness.guard.join("state.json")).unwrap();
    persisted["server_version"] = json!("1.19.3");
    util::atomic_write_json(&harness.guard.join("state.json"), &persisted).unwrap();
    let recovered =
        session::recover_fresh_with_api(&harness.guard, Box::new(fake.clone())).unwrap();
    assert_eq!(recovered["server_version"], "1.19.4");
    assert_eq!(fake.posts(), 1);
}
