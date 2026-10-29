//! Bounded read-only evidence, retained outside disposable task roots.
use super::*;
use std::collections::{BTreeMap, BTreeSet};

pub(super) struct Configuration {
    pub socket: String,
    endpoint: J,
    pub model: J,
    pub roles: J,
    pub binary: PathBuf,
    pub store: PathBuf,
    config: PathBuf,
    config_bytes: Vec<u8>,
    pub parent: String,
    pub parent_before: J,
    pub service: J,
    pub databases: J,
    pub native_data: PathBuf,
}
impl Configuration {
    pub fn load() -> Self {
        let required =
            |name| std::env::var(name).unwrap_or_else(|_| panic!("explicit {name} required"));
        let endpoint = resume::socket(Path::new(&required("HERDR_UPSTREAM_SOCKET")));
        let model = serde_json::from_str(&required("HERDR_UPSTREAM_MODEL")).unwrap();
        let roles: J = serde_json::from_str(&required("HERDR_UPSTREAM_ROLE_MODELS")).unwrap();
        for role in ["developer", "reviewer", "recovery"] {
            assert!(roles[role].is_object(), "explicit {role} required");
        }
        let binary = fs::canonicalize(required("HERDR_NATIVE_BINARY")).unwrap();
        let store = fs::canonicalize(required("HERDR_SESSION_STORE")).unwrap();
        let config = fs::canonicalize(required("HERDR_UPSTREAM_CONFIG")).unwrap();
        let parent = required("HERDR_PANE_ID");
        let parent_before = herdr(&["agent", "get", &parent]);
        let pid: i32 = required("HERDR_UPSTREAM_PID").parse().unwrap();
        let service = json!({"pid":pid,"start_time":task_env::process_start_time(pid).unwrap()});
        assert!(audit::exact_process_alive(&service));
        let databases: J = serde_json::from_str(&required("HERDR_HUMAN_AUDIT_DATABASES")).unwrap();
        assert!(!databases.as_array().unwrap().is_empty());
        for path in databases.as_array().unwrap() {
            assert!(Path::new(
                path.as_str()
                    .unwrap_or_else(|| path["path"].as_str().unwrap())
            )
            .is_file());
        }
        let native_data = fs::canonicalize(required("HERDR_NATIVE_DATA_ROOT")).unwrap();
        assert!(native_data.is_dir());
        Self {
            socket: endpoint["path"].as_str().unwrap().into(),
            endpoint,
            model,
            roles,
            binary,
            store,
            config_bytes: fs::read(&config).unwrap(),
            config,
            parent,
            parent_before,
            service,
            databases,
            native_data,
        }
    }
    pub fn launch(&self, workspace: &Path) -> J {
        json!({"adapter":"codex","configuration":"upstream","socket":self.socket,
            "cwd":workspace.join("checkout"),"model":self.model,"role_models":self.roles,
            "approval_policy":"on-request","driver_binary":env!("CARGO_BIN_EXE_herdr-flow"),
            "native_tui":{"binary":self.binary,"parent_pane":self.parent,"session_store":self.store}})
    }
    pub fn verify_unchanged(&self) {
        assert_eq!(resume::socket(Path::new(&self.socket)), self.endpoint);
        assert!(
            audit::exact_process_alive(&self.service),
            "shared service changed"
        );
        assert_eq!(fs::read(&self.config).unwrap(), self.config_bytes);
        let after = herdr(&["agent", "get", &self.parent]);
        for key in ["terminal_id", "workspace_id", "tab_id", "agent_session"] {
            assert_eq!(after["agent"][key], self.parent_before["agent"][key]);
        }
    }
}

pub(super) struct Audit {
    pub owner: J,
    pub workspace: PathBuf,
    pub scratch: PathBuf,
    environment: PathBuf,
    agreement: J,
    pub start: J,
    pub acquire: J,
    pub returned: J,
    pub human_sha: J,
    pub original: J,
    pub invalidated: J,
    pub recovery: J,
    pub terminal: J,
    pub delivery: J,
    pub views: BTreeMap<String, J>,
    pub processes: BTreeMap<String, J>,
    pub protocol: J,
    pub sessions: BTreeMap<String, PathBuf>,
    pub inventories: Vec<J>,
    pub cleanup: J,
    pub cleanup_plans: Vec<J>,
    pub process_start: u64,
    baseline: J,
    checkpoints: BTreeSet<String>,
}
impl Audit {
    pub fn new(f: &Fixture, env: &task_env::TaskEnvironment, config: &Configuration) -> Self {
        let q = f.state();
        Self {
            owner: serde_json::to_value(lifecycle::task_owner(&q, "task").unwrap().unwrap())
                .unwrap(),
            workspace: env.workspace_root.clone(),
            scratch: env.temp_root.clone(),
            environment: env.state_file().into(),
            agreement: q["tasks"]["task"]["requirements"].clone(),
            start: J::Null,
            acquire: J::Null,
            returned: J::Null,
            human_sha: J::Null,
            original: J::Null,
            invalidated: J::Null,
            recovery: J::Null,
            terminal: J::Null,
            delivery: J::Null,
            checkpoints: BTreeSet::new(),
            views: BTreeMap::new(),
            processes: BTreeMap::new(),
            protocol: json!({"events":{},"deliveries":{},"tasks":{}}),
            sessions: BTreeMap::new(),
            inventories: vec![],
            cleanup: J::Null,
            cleanup_plans: vec![],
            process_start: serde_json::to_value(env).unwrap()["creation"]["process_boundary"]
                ["start_ticks"]
                .as_u64()
                .unwrap(),
            baseline: json!({"model":config.model,"role_models":config.roles,"service":config.service,
                "socket":config.endpoint,"parent":config.parent_before,"config":config.config,
                "config_sha256":util::pyjson::sha256_hex(&config.config_bytes),"native_binary":config.binary,
                "operator":"scripted CLI operator, not a human UI session",
                "audit_databases":config.databases,"native_data_root":config.native_data}),
        }
    }
    pub fn persist(&self, f: &Fixture) {
        util::atomic_write_json(&f.temp.path().join("human-live-audit.json"), &json!({
            "owner":self.owner,"agreement":self.agreement,"baseline":self.baseline,"start":self.start,"process_start_cutoff":self.process_start,
            "original":self.original,"acquire":self.acquire,"human_sha":self.human_sha,"returned":self.returned,
            "invalidated":self.invalidated,"recovery":self.recovery,"terminal":self.terminal,"delivery":self.delivery,"views":self.views,
            "processes":self.processes,"protocol":self.protocol,"native_sessions":self.sessions,"native_inventories":self.inventories,"cleanup":self.cleanup,"cleanup_plans":self.cleanup_plans})).unwrap();
    }
    pub fn bound(&self, supervisor: &mut FixtureChild, deadline: Instant) {
        assert!(
            supervisor.0.try_wait().unwrap().is_none(),
            "supervisor exited; preserve original Owner"
        );
        assert!(
            Instant::now() < deadline,
            "observation limit only; preserve original task, do not replay"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    pub fn observe(&mut self, f: &Fixture, config: &Configuration) {
        let q = f.state();
        let task = &q["tasks"]["task"];
        for section in ["events", "deliveries", "tasks"] {
            for (id, record) in q[section].as_object().unwrap() {
                self.protocol[section][id] = record.clone();
            }
        }
        let plan = task_env::cleanup_state_path("task").unwrap();
        match util::read_json(&plan) {
            Ok(plan) => {
                for session in plan["terminal"]["upstream_sessions"]
                    .as_array()
                    .into_iter()
                    .flatten()
                {
                    if let (Some(id), Some(cwd)) = (
                        session["session"].as_str(),
                        session["runtime"]["cwd"].as_str(),
                    ) {
                        self.sessions.insert(id.into(), cwd.into());
                    }
                }
                if self.cleanup_plans.last() != Some(&plan) {
                    self.cleanup_plans.push(plan);
                }
            }
            Err(_) if !plan.exists() => (),
            Err(error) => panic!(
                "cannot observe terminal cleanup plan {}: {error}",
                plan.display()
            ),
        }
        if task.is_null() {
            self.persist(f);
            return;
        }
        assert_eq!(
            serde_json::to_value(lifecycle::task_owner(&q, "task").unwrap().unwrap()).unwrap(),
            self.owner
        );
        assert_eq!(task["requirements"], self.agreement);
        if task["final_outcome"].is_string() {
            self.terminal = task.clone();
        }
        if matches!(
            task["human_writer"]["phase"].as_str(),
            Some("verifying" | "released")
        ) && task["final_outcome"].is_null()
        {
            if task["human_writer"]["phase"] == "released"
                && self.invalidated.is_null()
                && task["candidate"].is_null()
                && task["approved"].is_null()
            {
                self.invalidated = task.clone();
            }
            let incident =
                task_runtime::recovery::status(&self.scratch.join("rt/coordinator")).unwrap();
            if incident["incident"].is_object() {
                self.recovery = incident["incident"].clone();
            }
            assert_ne!(
                incident["incident"]["phase"], "unavailable",
                "original recovery failed: {incident}"
            );
        }
        assert_ne!(
            task["human_writer"]["phase"], "blocked",
            "original human return blocked: {}",
            task["human_writer"]
        );
        if task["final_outcome"].is_null() {
            self.capture_views(config);
            for name in ["old-review", "fresh-review"] {
                if !task["verdicts"][name].is_null() && self.checkpoints.insert(name.into()) {
                    self.capture_threads(config);
                    self.inventory(config);
                }
            }
            if self.recovery["phase"] == "handled" && self.checkpoints.insert("recovery".into()) {
                self.capture_threads(config);
                let current = runtime_records(&self.scratch.join("rt/coordinator"));
                assert_eq!(
                    current["receiver.json"],
                    self.original["handshake"]["receiver.json"]
                );
                assert_eq!(
                    current["receiver-start.json"],
                    self.original["handshake"]["receiver-start.json"]
                );
            }
        }
        if let Some(env) = audit::read(
            self.environment.parent().unwrap(),
            self.environment.file_name().unwrap().to_str().unwrap(),
        ) {
            if env["delivery"]["phase"] == "confirmed" {
                self.delivery = env["delivery"].clone();
            }
            for process in env["child_processes"].as_array().unwrap() {
                self.processes.insert(
                    format!("{}:{}", process["pid"], process["start_time"]),
                    process.clone(),
                );
            }
        }
        self.persist(f);
    }
    fn capture_views(&mut self, config: &Configuration) {
        let coordinator = self.scratch.join("rt/coordinator");
        let roots = fs::read_dir(self.scratch.join("rt")).unwrap();
        for entry in roots {
            let root = entry.unwrap().path();
            let name = root.file_name().unwrap().to_str().unwrap().to_owned();
            if self.views.contains_key(&name) {
                continue;
            }
            // Model worker presentation publication is protected by its existing
            // coordinator service gate. Recovery has its own presentation lock.
            let lock_path = if name.starts_with("recovery-") {
                root.join("presentation.lock")
            } else {
                coordinator.join("control-service.lock")
            };
            let _gate = if name == "coordinator" {
                None
            } else {
                match util::probe_existing_lock(&lock_path).unwrap() {
                    util::LockProbe::Acquired(lock) => Some(lock),
                    util::LockProbe::Busy | util::LockProbe::Missing => continue,
                }
            };
            let Some(view) = audit::read(&root, "presentation.json") else {
                continue;
            };
            if view["phase"] != "ready" {
                continue;
            }
            let runtime = audit::read(&root, "runtime.json").unwrap();
            assert_eq!(runtime["lifecycle_owner"], self.owner);
            let marker =
                audit::read(Path::new(view["directory"].as_str().unwrap()), "tui.json").unwrap();
            assert_eq!(marker["status"], "ready");
            assert_eq!(marker["session"], view["session"]);
            assert_eq!(marker["runtime"]["cwd"], runtime["cwd"]);
            assert_eq!(marker["runtime"]["model"], runtime["model"]["id"]);
            assert_eq!(
                marker["runtime"]["modelProvider"],
                runtime["model"]["providerID"]
            );
            if !runtime["model"]["reasoning_effort"].is_null() {
                assert_eq!(
                    marker["runtime"]["reasoningEffort"],
                    runtime["model"]["reasoning_effort"]
                );
            }
            let role = if runtime["recovery_role"] == true {
                "recovery"
            } else if name == "coordinator" {
                "coordinator"
            } else if name == "dev-a1" {
                "developer"
            } else {
                "reviewer"
            };
            let expected = if role == "coordinator" {
                &config.model
            } else {
                &config.roles[role]
            };
            assert_eq!(runtime["model"], *expected);
            let state_root = audit::read(&root, "receiver.json")
                .map(|v| PathBuf::from(v["guard_dir"].as_str().unwrap()))
                .unwrap_or_else(|| root.clone());
            let state = audit::read(&state_root, "state.json");
            let agent = herdr(&["agent", "get", view["pane"].as_str().unwrap()]);
            assert!(agent
                .to_string()
                .contains(view["session"].as_str().unwrap()));
            assert_eq!(
                agent["agent"]["workspace_id"],
                config.parent_before["agent"]["workspace_id"]
            );
            assert_ne!(
                agent["agent"]["tab_id"],
                config.parent_before["agent"]["tab_id"]
            );
            let tab = herdr(&["tab", "get", view["tab"].as_str().unwrap()]);
            assert_eq!(tab["tab"]["label"], format!("flow:{name}"));
            self.sessions.insert(
                view["session"].as_str().unwrap().into(),
                PathBuf::from(runtime["cwd"].as_str().unwrap()),
            );
            self.views.insert(name,json!({"presentation":view,"runtime":runtime,"state":state,"agent":agent,"role":role,
                "session":view["session"],"native_marker":marker,"journals":runtime_records(&root)}));
        }
    }
    pub fn ready(&self, f: &Fixture, config: &Configuration) -> bool {
        if f.state()["tasks"]["task"]["approved"].is_null()
            || !self.views.contains_key("coordinator")
        {
            return false;
        }
        let thread = thread(config, &self.views["coordinator"]["session"]);
        thread["thread"]["status"]["type"] == "idle"
            && thread["thread"]["turns"]
                .as_array()
                .unwrap()
                .last()
                .is_some_and(|turn| {
                    turn["status"] == "completed"
                        && turn["items"].as_array().unwrap().iter().any(|item| {
                            item["type"] == "agentMessage"
                                && item["text"].as_str().is_some_and(|s| s.trim() == READY)
                        })
                })
    }
    pub fn capture_original(&mut self, f: &Fixture, config: &Configuration) {
        let task = f.state()["tasks"]["task"].clone();
        assert!(!task["approved"].is_null());
        assert!(task["writer"].is_null() && task["delivery"].is_null());
        let checkout = self.workspace.join("checkout");
        let head = git(&checkout, &["rev-parse", "HEAD"]);
        assert_eq!(json!(head), task["candidate"]);
        audit::bytes(&checkout, &head, "answer.txt", b"42\n");
        audit::bytes(&checkout, &head, "explanation.txt", b"6 * 7 = 42\n");
        assert!(!checkout.join("operator-note.txt").exists());
        self.original = json!({"task":task,"handshake":runtime_records(&self.scratch.join("rt/coordinator")),
            "handshake_sha256":util::pyjson::sha256_hex(&serde_json::to_vec(&runtime_records(&self.scratch.join("rt/coordinator"))).unwrap()),
            "checkout_head":head,
            "checkout_fingerprint":util::fingerprint(&self.workspace.join("checkout")).unwrap()});
        self.capture_threads(config);
        self.inventory(config);
        self.persist(f);
    }
    pub fn assert_acquired(&self, f: &Fixture) {
        assert_eq!(self.acquire["phase"], "human");
        assert_eq!(self.acquire["owner"], self.owner);
        assert_eq!(
            self.acquire["before"],
            self.original["checkout_fingerprint"]
        );
        assert_eq!(self.acquire["proof"]["operation_replay_allowed"], false);
        assert!(!self.acquire["proof"]["sessions"]
            .as_array()
            .unwrap()
            .is_empty());
        assert_eq!(
            f.state()["tasks"]["task"]["approved"],
            self.original["task"]["approved"]
        );
    }
    fn capture_threads(&mut self, config: &Configuration) {
        for view in self.views.values_mut() {
            view["thread"] = thread(config, &view["session"]);
            let root = Path::new(view["runtime"]["root"].as_str().unwrap());
            view["journals"] = runtime_records(root);
        }
    }
    pub fn inventory(&mut self, config: &Configuration) {
        let parents: Vec<_> = self.sessions.keys().cloned().collect();
        let cwds: BTreeSet<_> = self.sessions.values().cloned().collect();
        for cwd in cwds {
            let pages = audit::native_snapshot(&config.socket, &cwd, &parents);
            for page in pages.as_array().unwrap() {
                for row in page["rows"].as_array().unwrap() {
                    let rollout = Path::new(
                        row["path"]
                            .as_str()
                            .expect("native inventory must locate its actual rollout"),
                    );
                    assert!(
                        rollout.is_absolute()
                            && fs::canonicalize(rollout)
                                .unwrap()
                                .starts_with(&config.native_data),
                        "configured native data root does not contain the actual owned rollout"
                    );
                    self.sessions
                        .insert(row["id"].as_str().unwrap().into(), cwd.clone());
                }
            }
            self.inventories.push(pages);
        }
        assert!(
            self.views.len() <= 16
                && self.processes.len() <= 128
                && self.sessions.len() <= 256
                && self.inventories.len() <= 32,
            "live audit inventory capacity exceeded"
        );
    }
    pub fn verify(&mut self, f: &Fixture, config: &Configuration, target: &Path) {
        cleanup::verify(self, f, config, target);
        config.verify_unchanged();
        self.persist(f);
    }
}
fn thread(config: &Configuration, session: &J) -> J {
    let mut client = herdr_flow::codex::rpc::Client::new(&config.socket);
    client.timeout = 5.0;
    client
        .call(
            "thread/read",
            &json!({"threadId":session,"includeTurns":true}),
        )
        .unwrap()
}
pub(super) fn runtime_records(root: &Path) -> J {
    let mut records = json!({});
    for name in [
        "runtime.json",
        "upstream-session.json",
        "receiver.json",
        "receiver-start.json",
        "state.json",
        "turn.json",
        "broker-events.json",
        "broker-tools.json",
        "control-tools.json",
        "native-client.json",
        "native-requests.json",
        "tui.json",
        "presentation.json",
    ] {
        records[name] = audit::read(root, name).unwrap_or(J::Null);
    }
    if let Some(guard) = records["receiver.json"]["guard_dir"].as_str() {
        records["receiver_state"] = audit::read(Path::new(guard), "state.json").unwrap();
    }
    records
}
