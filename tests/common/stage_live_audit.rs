//! Bounded, read-only evidence of actual stage publication and visible roles.
use super::*;
#[path = "stage_live_reclamation.rs"]
mod reclamation;

pub(super) struct StageAudit {
    root: PathBuf,
    env_file: PathBuf,
    target: PathBuf,
    owner: J,
    requirement: J,
    models: J,
    parent: J,
    views: BTreeMap<String, J>,
    verdicts: BTreeMap<String, J>,
    processes: BTreeMap<String, J>,
    stage: J,
    terminal: J,
    early: J,
    socket: String,
    store: PathBuf,
}

fn read(root: &Path, name: &str) -> Option<J> {
    if !root.exists() {
        return None;
    }
    let value = match util::read_state_json_bounded_if_exists(root, name, 8 * 1024 * 1024) {
        Ok(value) => value,
        Err(_) if !root.exists() => None,
        Err(e) => panic!(
            "audit op=read runtime={} document={}: {e}",
            root.display(),
            root.join(name).display()
        ),
    };
    if name == "presentation.json" && value.is_none() && document_present(root, name) {
        panic!("audit op=read runtime={} document={}: protected presentation has no authority payload at quiescent publication boundary",
            root.display(), root.join(name).display());
    }
    value
}

fn document_present(root: &Path, name: &str) -> bool {
    match fs::symlink_metadata(root.join(name)) {
        Ok(_) => true,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => panic!(
            "audit op=stat runtime={} document={}: {error}",
            root.display(),
            root.join(name).display()
        ),
    }
}

const WORKERS: &[&str] = &[
    "dev-a1",
    "stage-review-a1",
    "final-dev-a1",
    "final-review-a1",
];

fn coordinator_identity(coordinator: &Path, owner: &J) {
    assert_eq!(coordinator.file_name().unwrap(), "coordinator");
    assert_eq!(
        read(coordinator, "lifecycle-owner.json"),
        Some(owner.clone()),
        "audit op=identity coordinator={} Owner changed",
        coordinator.display()
    );
}

pub(super) fn observation_gate(
    coordinator: &Path,
    owner: &J,
    worker: &Path,
) -> Option<util::FileLock> {
    assert_eq!(
        worker.parent(),
        coordinator.parent(),
        "audit worker escaped runtime parent"
    );
    if !WORKERS.contains(&worker.file_name()?.to_str()?) {
        return None;
    }
    coordinator_identity(coordinator, owner);
    let path = coordinator.join("control-service.lock");
    let probe = || match util::probe_existing_lock(&path) {
        Ok(probe) => Some(probe),
        Err(_) if !coordinator.exists() => None,
        Err(error) => panic!(
            "audit op=probe runtime={} coordinator={} lock={}: {error}",
            worker.display(),
            coordinator.display(),
            path.display()
        ),
    };
    let mut state = probe()?;
    if matches!(state, util::LockProbe::Missing) {
        if !document_present(worker, "presentation.json") {
            return None;
        }
        state = probe()?;
    }
    match state {
        util::LockProbe::Acquired(lock) => Some(lock),
        util::LockProbe::Busy => None,
        util::LockProbe::Missing => panic!(
            "audit op=probe runtime={} document={}: coordinator service gate missing: {}",
            worker.display(),
            worker.join("presentation.json").display(),
            path.display()
        ),
    }
}

fn publication_view(coordinator: &Path, owner: &J, root: &Path) -> Option<J> {
    let name = root.file_name()?.to_str()?;
    if root != coordinator && !WORKERS.contains(&name) {
        return None;
    }
    // Only ordinary five-role worker dispatch is serialized by this gate.
    // The coordinator's initial presentation finished before the audit loop.
    let _service = if root == coordinator {
        coordinator_identity(coordinator, owner);
        None
    } else {
        Some(observation_gate(coordinator, owner, root)?)
    };
    let view = read(root, "presentation.json")?;
    if view["phase"] != "ready" {
        return None;
    }
    assert_eq!(
        read(root, "lifecycle-owner.json"),
        Some(owner.clone()),
        "audit op=identity runtime={} Owner changed",
        root.display()
    );
    let runtime = read(root, "runtime.json").unwrap();
    assert_eq!(runtime["lifecycle_owner"], *owner);
    assert!(
        runtime["recovery_role"] != true && runtime["publication_role"] != true,
        "audit op=scope runtime={}: independent presentation is outside the ordinary five-role audit", root.display()
    );
    let state_root = read(root, "receiver.json")
        .and_then(|receiver| receiver["guard_dir"].as_str().map(PathBuf::from))
        .unwrap_or_else(|| root.to_path_buf());
    let state = read(&state_root, "state.json").unwrap();
    // Returning only owned JSON releases the gate before Herdr/RPC/git work.
    Some(json!({"presentation":view,"runtime":runtime,"state":state}))
}

impl StageAudit {
    pub(super) fn new(
        f: &Fixture,
        env: &task_env::TaskEnvironment,
        target: &Path,
        model: &J,
        roles: &J,
        parent: &J,
        upstream: (&str, &Path),
    ) -> Self {
        let q = f.state();
        Self {
            root: env.temp_root.join("rt"),
            env_file: env.state_file().to_path_buf(),
            target: target.to_owned(),
            owner: serde_json::to_value(lifecycle::task_owner(&q, "task").unwrap().unwrap())
                .unwrap(),
            requirement: q["tasks"]["task"]["requirements"].clone(),
            models: json!({"coordinator":model,"development":roles["developer"],"acceptance":roles["reviewer"]}),
            parent: parent.clone(),
            views: BTreeMap::new(),
            verdicts: BTreeMap::new(),
            processes: BTreeMap::new(),
            stage: J::Null,
            terminal: J::Null,
            early: J::Null,
            socket: upstream.0.into(),
            store: upstream.1.into(),
        }
    }

    pub(super) fn observe(&mut self, f: &Fixture, q: &J) {
        let parent = &q["tasks"]["task"];
        if parent.is_null() {
            return;
        }
        assert_eq!(
            serde_json::to_value(lifecycle::task_owner(q, "task").unwrap().unwrap()).unwrap(),
            self.owner
        );
        assert_eq!(
            parent["requirements"], self.requirement,
            "agreement changed"
        );
        if let Some(verdicts) = parent["verdicts"].as_object() {
            for (name, verdict) in verdicts {
                if let Some(old) = self.verdicts.insert(name.clone(), verdict.clone()) {
                    assert_eq!(old, *verdict, "immutable verdict changed");
                }
            }
        }
        if parent["stage_delivery"]["phase"] == "complete" && self.stage.is_null() {
            self.capture_stage(parent);
        }
        if parent["final_outcome"].is_string() {
            self.terminal = parent.clone();
        } else {
            self.runtime_views();
        }
        if let Some(env) = read(
            self.env_file.parent().unwrap(),
            self.env_file.file_name().unwrap().to_str().unwrap(),
        ) {
            if parent["final_outcome"].is_null() && self.views.contains_key("final-dev-a1") {
                if self.early.is_null() {
                    self.early = reclamation::observe(self, f, q, &env, &self.socket, &self.store);
                } else if self.views.contains_key("final-review-a1")
                    && self.early["retained_through_final_review"] != true
                {
                    reclamation::retained_review(self, &env);
                    self.early["retained_through_final_review"] = json!(true);
                }
            }
            for process in env["child_processes"].as_array().unwrap() {
                self.processes.insert(
                    format!("{}:{}", process["pid"], process["start_time"]),
                    process.clone(),
                );
            }
        }
        assert!(
            self.views.len() <= 16 && self.verdicts.len() <= 16 && self.processes.len() <= 128,
            "bounded audit capacity"
        );
        util::atomic_write_json(&f.temp.path().join("stage-audit.json"), &self.summary()).unwrap();
    }

    fn capture_stage(&mut self, parent: &J) {
        let checkpoint = &parent["stage_delivery"];
        let intent = &checkpoint["delivery"];
        assert_eq!(intent["phase"], "confirmed");
        assert_eq!(intent["stage"]["criteria"], json!(["answer"]));
        assert_eq!(intent["approval"]["scope_epoch"], 1);
        assert_eq!(parent["scope_epoch"], 2);
        assert!(parent["final_outcome"].is_null());
        assert!(parent["stage"].is_null());
        let sha = intent["artifact"]["id"].as_str().unwrap();
        assert_eq!(
            git(&self.target, &["rev-parse", "refs/heads/accepted"]),
            sha,
            "first stage was not actually published"
        );
        bytes(&self.target, sha, "answer.txt", b"42\n");
        assert!(
            !Command::new("git")
                .arg("-C")
                .arg(&self.target)
                .args(["cat-file", "-e", &format!("{sha}:explanation.txt")])
                .output()
                .unwrap()
                .status
                .success(),
            "stage must leave remaining overall work"
        );
        self.stage = checkpoint.clone();
    }

    fn runtime_views(&mut self) {
        let entries = match fs::read_dir(&self.root) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
            Err(e) => panic!("runtime inventory: {e}"),
        };
        for entry in entries {
            let path = entry.unwrap().path();
            let name = path.file_name().unwrap().to_str().unwrap();
            if self.views.contains_key(name) {
                continue;
            }
            let Some(snapshot) =
                publication_view(&self.root.join("coordinator"), &self.owner, &path)
            else {
                continue;
            };
            let view = &snapshot["presentation"];
            let runtime = &snapshot["runtime"];
            let state = &snapshot["state"];
            let role = state["role"].as_str().unwrap();
            assert_eq!(
                runtime["model"], self.models[role],
                "wrong configured role model"
            );
            assert_eq!(
                state["model"], self.models[role],
                "native model selection changed"
            );
            let pane = view["pane"].as_str().unwrap();
            let agent = herdr(&["agent", "get", pane]);
            assert!(
                agent
                    .to_string()
                    .contains(view["session"].as_str().unwrap()),
                "Herdr agent does not show this native session"
            );
            assert_eq!(
                agent["agent"]["workspace_id"],
                self.parent["agent"]["workspace_id"]
            );
            assert_ne!(agent["agent"]["tab_id"], self.parent["agent"]["tab_id"]);
            let tab = herdr(&["tab", "get", view["tab"].as_str().unwrap()]);
            assert_eq!(tab["tab"]["label"], format!("flow:{name}"));
            self.views.insert(name.to_owned(),json!({"presentation":view,"role":role,"model":state["model"],"session":state["session"],"cwd":runtime["cwd"]}));
        }
    }

    pub(super) fn summary(&self) -> J {
        json!({"scope":"one real answer stage then new development and independent full acceptance; local Git only",
            "owner":self.owner,"views":self.views,"verdicts":self.verdicts,"processes":self.processes,
            "stage":self.stage,"early_reclamation":self.early,"terminal":self.terminal,"harness_business_mutations_after_start":false})
    }

    pub(super) fn verify(&self, f: &Fixture, target: &Path, store: &Path) {
        assert_eq!(
            self.early["plan"]["phase"], "complete",
            "early reclamation evidence missing"
        );
        assert_eq!(self.early["retained_through_final_review"], true);
        assert_eq!(self.stage["phase"], "complete");
        assert_eq!(self.terminal["final_outcome"], "success");
        assert!(self.terminal["stage"].is_null());
        assert_eq!(self.terminal["scope_epoch"], 2);
        assert_eq!(self.terminal["approved"]["scope_epoch"], 2);
        for (name, criteria, epoch) in [
            ("stage-review", vec!["answer"], 1),
            ("final-review", vec!["answer", "explanation"], 2),
        ] {
            let verdict = &self.verdicts[name];
            assert_eq!(verdict["envelope"]["report"]["conclusion"], "passed");
            assert_eq!(
                verdict["envelope"]["binding"]["lineage"]["scope_epoch"],
                epoch
            );
            let actual: std::collections::BTreeSet<_> = verdict["envelope"]["report"]["checks"]
                .as_array()
                .unwrap()
                .iter()
                .map(|c| c["criterion"].as_str().unwrap())
                .collect();
            assert_eq!(actual, criteria.into_iter().collect());
        }
        assert_ne!(
            self.verdicts["stage-review"]["envelope"]["reviewer"],
            self.verdicts["final-review"]["envelope"]["reviewer"]
        );
        let final_sha = git(target, &["rev-parse", "refs/heads/accepted"]);
        assert_eq!(json!(final_sha), self.terminal["candidate"]);
        assert_ne!(json!(final_sha), self.stage["delivery"]["artifact"]["id"]);
        bytes(target, &final_sha, "answer.txt", b"42\n");
        bytes(target, &final_sha, "explanation.txt", b"6 * 7 = 42\n");
        for name in [
            "coordinator",
            "dev-a1",
            "stage-review-a1",
            "final-dev-a1",
            "final-review-a1",
        ] {
            assert!(
                self.views.contains_key(name),
                "role was not visibly attached: {name}"
            );
        }
        let sessions: std::collections::BTreeSet<_> = self
            .views
            .values()
            .map(|v| v["session"].as_str().unwrap())
            .collect();
        assert_eq!(sessions.len(), 5, "roles reused a native session");
        assert!(!self.processes.is_empty());
        for process in self.processes.values() {
            let registered: task_env::RegisteredProcess =
                serde_json::from_value(process.clone()).unwrap();
            assert!(
                !task_env::process_is_alive(&registered),
                "owned registered process survived"
            );
        }
        let panes = herdr(&[
            "pane",
            "list",
            "--workspace",
            self.parent["agent"]["workspace_id"].as_str().unwrap(),
        ]);
        for view in self.views.values() {
            assert!(
                !panes
                    .to_string()
                    .contains(view["presentation"]["terminal"].as_str().unwrap()),
                "owned Herdr terminal survived"
            );
        }
        saved_presentations_absent(store, &sessions);
        let q = f.state();
        for name in ["task", "dev", "stage-review", "final-dev", "final-review"] {
            assert!(q["tasks"][name].is_null());
        }
    }
}

fn bytes(target: &Path, sha: &str, name: &str, expected: &[u8]) {
    let out = Command::new("git")
        .arg("-C")
        .arg(target)
        .args(["show", &format!("{sha}:{name}")])
        .output()
        .unwrap();
    assert!(out.status.success());
    assert_eq!(out.stdout, expected);
}

fn saved_presentations_absent(store: &Path, sessions: &std::collections::BTreeSet<&str>) {
    let mut files = vec![store.to_path_buf()];
    let snapshots = store.parent().unwrap().join("session-snapshots");
    for entry in fs::read_dir(snapshots).unwrap() {
        let entry = entry.unwrap();
        if entry.file_type().unwrap().is_file() {
            files.push(entry.path());
        }
    }
    for path in files {
        let data = fs::read(&path).unwrap();
        for session in sessions {
            assert!(
                !data.windows(session.len()).any(|w| w == session.as_bytes()),
                "owned native session remains in Herdr saved state: {}",
                path.display()
            );
        }
    }
}

#[test]
fn audit_reads_actual_protected_document_storage() {
    let f = Fixture::new();
    let env = setup(&f, &f.temp.path().join("output.git"));
    let root = task_runtime::prepare(&env, "audit-fixture").unwrap();
    let expected = json!({"phase":"recorded","instance":env.instance_id});
    util::atomic_write_json(&root.join("audit.json"), &expected).unwrap();
    assert!(root.join("audit.json").is_dir());
    assert_eq!(read(&root, "audit.json"), Some(expected));
    assert!(read(&root, "missing.json").is_none());
    drop(env);
    upstream_runtime::finalize_with_retry(&f);
    assert_eq!(queue::finalization::drive(&f.queue).unwrap().pending, 0);
}

#[path = "stage_live_publication_tests.rs"]
mod publication_tests;
