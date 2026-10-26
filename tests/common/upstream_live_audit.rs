//! Read-only, bounded evidence retained outside the disposable task roots.
use super::*;
use std::collections::BTreeMap;

pub(super) struct LiveAudit {
    scenario: Scenario,
    previous: J,
    sequence: usize,
    verdicts: BTreeMap<String, J>,
    terminal: J,
    restarted: bool,
    turn: Option<J>,
}
impl LiveAudit {
    pub(super) fn new(f: &Fixture, scenario: Scenario) -> Self {
        common::create_dir(f.temp.path().join("audit")).unwrap();
        Self {
            scenario,
            previous: J::Null,
            sequence: 0,
            verdicts: BTreeMap::new(),
            terminal: J::Null,
            restarted: false,
            turn: None,
        }
    }
    pub(super) fn observe(&mut self, f: &Fixture, state: &J) {
        let parent = &state["tasks"]["task"];
        if let Some(verdicts) = parent["verdicts"].as_object() {
            for (name, verdict) in verdicts {
                if let Some(old) = self.verdicts.insert(name.clone(), verdict.clone()) {
                    assert_eq!(&old, verdict, "immutable verdict changed");
                }
            }
        }
        if parent["final_outcome"].is_string() {
            self.terminal = parent.clone();
        }
        let mut business = state.clone();
        for field in ["authority_version", "heartbeat_at", "updated_at"] {
            business.as_object_mut().unwrap().remove(field);
        }
        if business != self.previous {
            assert!(self.sequence < 256, "bounded audit exceeded; retain state");
            let bytes = serde_json::to_vec(state).unwrap();
            assert!(bytes.len() <= 1024 * 1024, "bounded state exceeded");
            util::atomic_write_json(
                &f.temp
                    .path()
                    .join(format!("audit/state-{:03}.json", self.sequence)),
                state,
            )
            .unwrap();
            self.sequence += 1;
            self.previous = business;
        }
    }
    pub(super) fn restart_if_submitted(
        &mut self,
        f: &Fixture,
        coord: &Path,
        watch: &mut FixtureChild,
    ) {
        if self.scenario != Scenario::WatcherRestart {
            return;
        }
        let root = coord.parent().unwrap().join("dev-a1");
        if !root.try_exists().unwrap() {
            return;
        }
        let turn =
            util::read_state_json_bounded_if_exists(&root, "turn.json", 1024 * 1024).unwrap();
        let Some(turn) = turn else {
            return;
        };
        if let Some(before) = &self.turn {
            assert_eq!(turn["session"], before["session"]);
            assert_eq!(turn["turn"], before["turn"]);
            assert_eq!(turn["binding"], before["binding"]);
        }
        if self.restarted || turn["phase"] != "submitted" {
            return;
        }
        // Kill/reap only this fixture's queue watcher. Keep all brokers and the
        // existing shared app-server alive; no second dispatch or task mutation.
        let old_pid = watch.0.id();
        watch.0.kill().unwrap();
        watch.0.wait().unwrap();
        *watch = start_watch(f, "restarted-watch.log");
        util::atomic_write_json(
            &f.temp.path().join("audit/watcher-restart.json"),
            &json!({"old_pid":old_pid,"new_pid":watch.0.id(),"submitted_turn":turn}),
        )
        .unwrap();
        self.turn = Some(turn);
        self.restarted = true;
    }
    pub(super) fn verify(&self, scenario: Scenario) {
        assert_eq!(
            self.terminal["final_outcome"],
            if scenario == Scenario::FinalFailure {
                "failure"
            } else {
                "success"
            }
        );
        let conclusion =
            |name: &str| self.verdicts[name]["envelope"]["report"]["conclusion"].clone();
        match scenario {
            Scenario::Repair => {
                assert_eq!(conclusion("review"), "failed");
                assert_eq!(conclusion("retest"), "passed");
                assert!(self.terminal["open_gaps"].as_object().unwrap().is_empty());
                assert_ne!(
                    self.verdicts["review"]["envelope"]["reviewer"],
                    self.verdicts["retest"]["envelope"]["reviewer"]
                );
            }
            Scenario::FinalFailure => assert_eq!(conclusion("review"), "failed"),
            _ => assert_eq!(conclusion("review"), "passed"),
        }
        if scenario == Scenario::WatcherRestart {
            assert!(self.restarted);
        }
    }
}

#[test]
fn audit_preserves_private_evidence_outside_runtime_roots() {
    let f = Fixture::new();
    let mut audit = LiveAudit::new(&f, Scenario::Success);
    audit.observe(&f, &f.state());
    let path = f.temp.path().join("audit/state-000.json");
    assert_eq!(
        fs::metadata(f.temp.path().join("audit"))
            .unwrap()
            .permissions()
            .mode()
            & 0o077,
        0
    );
    assert_eq!(util::read_json(&path).unwrap(), f.state());
    assert!(!f.temp.path().join("scratch").exists());
    let mut heartbeat = f.state();
    for tick in 0..1000 {
        heartbeat["authority_version"] = json!(tick);
        heartbeat["heartbeat_at"] = json!(tick);
        heartbeat["updated_at"] = json!(tick);
        audit.observe(&f, &heartbeat);
    }
    assert_eq!(
        audit.sequence, 1,
        "heartbeat-only writes must not exhaust audit"
    );
    heartbeat["tasks"]["task"]["state_version"] = json!(2);
    audit.observe(&f, &heartbeat);
    assert_eq!(
        audit.sequence, 2,
        "business transitions must remain recorded"
    );
}
