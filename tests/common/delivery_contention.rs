//! Queue transactions remain available while the original delivery is pending.
use super::*;
use std::{cell::RefCell, fs, time::Duration, time::Instant};

#[derive(Clone, Copy)]
enum Change {
    OtherTask,
    Version,
    Approval,
    Authorization,
    Stopped,
    Writer,
    ForeignReference,
}

struct Probe {
    queue: PathBuf,
    journal: PathBuf,
    change: Change,
    calls: RefCell<Vec<&'static str>>,
}

impl Probe {
    fn new(f: &Fixture, change: Change) -> Self {
        Self {
            queue: f.queue.clone(),
            journal: f.env.state_file().to_path_buf(),
            change,
            calls: RefCell::new(vec![]),
        }
    }

    fn check(&self, intent: &J, phase: &'static str) -> herdr_flow::Result<()> {
        let _lock = queue::locked(&self.queue, false)?;
        self.calls.borrow_mut().push(phase);
        let mut q = queue::load_queue(&self.queue)?;
        if phase != "preflight" {
            assert_eq!(q["tasks"]["job"]["control"], "delivering");
            assert_eq!(q["tasks"]["job"]["delivery"], intent["id"]);
            let saved = util::read_json(&self.journal)?;
            assert_eq!(saved["delivery"]["id"], intent["id"]);
            assert_eq!(saved["delivery"]["phase"], "unknown");
        }
        // Test-only queue edits represent another actor committing while the
        // read-only adapter is outside the transaction.
        match self.change {
            Change::OtherTask => {
                q["tasks"]["other"]["progress"] = json!(self.calls.borrow().len());
            }
            Change::Version => q["tasks"]["job"]["state_version"] = json!(999),
            Change::Approval => q["tasks"]["job"]["approved"] = J::Null,
            Change::Authorization => {
                q["tasks"]["job"]["requirements"]["1"]["authorization"] = json!({});
            }
            Change::Stopped => q["stopped"] = json!(true),
            Change::Writer => q["tasks"]["job"]["writer"] = json!({"phase":"active"}),
            Change::ForeignReference => q["tasks"]["other"]["retains"] = json!("job"),
        }
        queue::save(&self.queue, &mut q)
    }
}

impl Executor for Probe {
    fn preflight(&self, intent: &J) -> herdr_flow::Result<()> {
        self.check(intent, "preflight")?;
        delivery::local::Local.preflight(intent)
    }
    fn execute(&self, intent: &J) -> herdr_flow::Result<()> {
        self.check(intent, "execute")?;
        delivery::local::Local.execute(intent)
    }
    fn observe(&self, intent: &J) -> herdr_flow::Result<Observation> {
        self.check(intent, "observe")?;
        delivery::local::Local.observe(intent)
    }
}

#[test]
fn all_adapter_calls_release_queue_and_preserve_other_task_progress() {
    let mut f = Fixture::new(false);
    let probe = Probe::new(&f, Change::OtherTask);
    delivery::prepare(&mut f.env, &f.queue, &f.spec, &probe).unwrap();
    assert_eq!(
        delivery::execute(&mut f.env, &probe).unwrap()["phase"],
        "confirmed"
    );
    assert_eq!(
        *probe.calls.borrow(),
        ["preflight", "preflight", "execute", "observe"]
    );
    assert_eq!(
        queue::load_queue(&f.queue).unwrap()["tasks"]["other"]["progress"],
        4
    );
    assert!(delivery::execute(&mut f.env, &probe).is_err());
    assert_eq!(probe.calls.borrow().len(), 4);
    delivery::finish(&mut f.env).unwrap();
    f.clean();
}

#[test]
fn preparation_revalidates_requirement_and_cleanup_after_external_preflight() {
    for change in [
        Change::Version,
        Change::Approval,
        Change::Authorization,
        Change::Stopped,
        Change::Writer,
        Change::ForeignReference,
    ] {
        let mut f = Fixture::new(false);
        let probe = Probe::new(&f, change);
        assert!(delivery::prepare(&mut f.env, &f.queue, &f.spec, &probe).is_err());
        assert!(f.env.delivery.is_null());
        assert_eq!(*probe.calls.borrow(), ["preflight"]);
        let q = queue::load_queue(&f.queue).unwrap();
        assert_eq!(q["tasks"]["job"]["control"], "running");
        assert!(q["tasks"]["job"]["delivery"].is_null());
        assert!(!f.dir.path().join("customer").exists());
        destroy_environment(&f.env).unwrap();
    }
}

#[test]
fn execution_revalidates_authority_before_reserving_or_submitting() {
    for change in [
        Change::Version,
        Change::Approval,
        Change::Authorization,
        Change::Stopped,
        Change::Writer,
    ] {
        let mut f = Fixture::new(false);
        f.prepare();
        let prepared = f.env.delivery.clone();
        let probe = Probe::new(&f, change);
        assert!(delivery::execute(&mut f.env, &probe).is_err());
        assert_eq!(*probe.calls.borrow(), ["preflight"]);
        assert_eq!(f.env.delivery, prepared);
        assert!(!f.dir.path().join("customer").exists());
        destroy_environment(&f.env).unwrap();
    }
}

struct ReleaseHook(PathBuf);
impl Drop for ReleaseHook {
    fn drop(&mut self) {
        let _ = fs::write(&self.0, "release");
    }
}

#[test]
fn paused_real_git_hook_keeps_queue_mutable_and_original_execution_unknown() {
    use std::os::unix::fs::PermissionsExt;
    let mut f = Fixture::new(true);
    f.prepare();
    let queue = f.queue.clone();
    let journal = f.env.state_file().to_path_buf();
    let prepared = f.env.delivery.clone();
    let hooks = f.dir.path().join("customer/hooks");
    fs::write(hooks.join("pre-receive"),
        "#!/bin/sh\nprintf '%s\\n' \"$$\" >> hooks/entered\ntries=0\nwhile [ ! -f hooks/release ] && [ \"$tries\" -lt 800 ]; do\n  sleep 0.025\n  tries=$((tries + 1))\ndone\ntest -f hooks/release\n").unwrap();
    fs::set_permissions(hooks.join("pre-receive"), fs::Permissions::from_mode(0o700)).unwrap();
    std::thread::scope(|scope| {
        let env = &mut f.env;
        let worker = scope.spawn(move || delivery::execute(env, &delivery::local::Local));
        let release = ReleaseHook(hooks.join("release"));
        let deadline = Instant::now() + Duration::from_secs(10);
        while !hooks.join("entered").exists() {
            assert!(!worker.is_finished(), "Git exited before entering the hook");
            assert!(Instant::now() < deadline, "Git did not enter the hook");
            std::thread::sleep(Duration::from_millis(10));
        }
        {
            let _lock = queue::locked(&queue, false).expect("Git must not hold the queue lock");
            let mut q = queue::load_queue(&queue).unwrap();
            assert_eq!(q["tasks"]["job"]["control"], "delivering");
            assert_eq!(q["tasks"]["job"]["delivery"], prepared["id"]);
            q["tasks"]["other"]["during_git"] = json!(true);
            queue::save(&queue, &mut q).unwrap();
        }
        let current = util::read_json(&journal).unwrap();
        assert_eq!(current["delivery"]["phase"], "unknown");
        assert_eq!(current["delivery"]["id"], prepared["id"]);
        let error = restore_environment("job").err().unwrap();
        assert!(error.to_string().contains("already owned"), "{error}");
        assert!(matches!(
            delivery::local::Local
                .observe(&current["delivery"])
                .unwrap(),
            Observation::Unknown(_)
        ));
        drop(release);
        assert_eq!(worker.join().unwrap().unwrap()["phase"], "confirmed");
    });
    assert_eq!(
        fs::read_to_string(hooks.join("entered"))
            .unwrap()
            .lines()
            .count(),
        1
    );
    assert_eq!(
        queue::load_queue(&queue).unwrap()["tasks"]["other"]["during_git"],
        true
    );
    assert!(delivery::execute(&mut f.env, &delivery::local::Local).is_err());
    delivery::finish(&mut f.env).unwrap();
    f.clean();
}
