//! Queue bootstrap entrypoints; all resources are private fixtures, no models.
use herdr_flow::{
    lifecycle::{self, Owner},
    model::J,
    progress, queue, task_env, util,
};
use serde_json::json;
use std::{
    fs,
    os::unix::{fs::PermissionsExt, process::CommandExt},
    path::{Path, PathBuf},
    process::Command,
    sync::{Mutex, MutexGuard},
};

static LOCK: Mutex<()> = Mutex::new(());
const SESSION: &str = "11111111-1111-1111-1111-111111111111";

struct Fixture {
    temp: tempfile::TempDir,
    queue: PathBuf,
    old: Option<std::ffi::OsString>,
    commit: String,
    _lock: MutexGuard<'static, ()>,
}

impl Fixture {
    fn new() -> Self {
        let lock = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let temp = tempfile::Builder::new()
            .permissions(fs::Permissions::from_mode(0o700))
            .tempdir()
            .unwrap();
        let repo = temp.path().join("repo");
        private(&repo);
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
        let queue = temp.path().join("queue");
        queue::init_isolated(&queue, None, 30).unwrap();
        let old = std::env::var_os("HERDR_TASK_ENV_ROOT");
        std::env::set_var(
            "HERDR_TASK_ENV_ROOT",
            queue.join("queue.json/task-environments"),
        );
        let receiver = temp.path().join("receiver");
        private(&receiver);
        let guard = json!({"version":2,"task":"fixture","role":"coordinator","pane":"w1:p1",
            "terminal":"term1","socket":"/fixture","cwd":repo,"session":SESSION,"phase":"monitoring"});
        util::atomic_write_json(&receiver.join("state.json"), &guard).unwrap();
        let mut state = queue::load_queue(&queue).unwrap();
        state["guard_dir"] = json!(receiver);
        state["receiver"] = herdr_flow::guard::scope(&guard);
        queue::save(&queue, &mut state).unwrap();
        Self {
            temp,
            queue,
            old,
            commit,
            _lock: lock,
        }
    }
    fn state(&self) -> J {
        queue::load_queue(&self.queue).unwrap()
    }
    fn add(&self, name: &str) {
        let _lock = queue::locked(&self.queue, false).unwrap();
        let mut s = self.state();
        s["tasks"][name] = json!({"task":name,"epoch":1,"state_version":1,"candidate":self.commit,
            "phase":"waiting","control":"running","writer":null,"pending":[],"operations":{}});
        queue::save(&self.queue, &mut s).unwrap();
    }
    fn spec(&self, name: &str) -> J {
        let s = self.state();
        json!({"action":"create","task":name,"queue_id":s["id"],"epoch":1,"state_version":s["tasks"][name]["state_version"],
            "environment":{"workspace_root":self.temp.path().join(format!("{name}-work")),
            "temp_root":self.temp.path().join(format!("{name}-tmp")),"delivery_targets":[],
            "repository":self.temp.path().join("repo"),"commit":self.commit}})
    }
    fn call(&self, action: &str, spec: &J) -> herdr_flow::Result<J> {
        queue::finalization::task_action(&self.queue, action, spec, Some(SESSION), &|_| {
            panic!("no native or model calls")
        })
    }
    fn create(&self, name: &str) -> Owner {
        self.add(name);
        self.call("task-environment", &self.spec(name)).unwrap();
        lifecycle::task_owner(&self.state(), name).unwrap().unwrap()
    }
    fn seed(&self, name: &str) -> Owner {
        self.add(name);
        let spec = self.spec(name);
        let binding = task_env::instance::InstanceBinding {
            queue_id: self.state()["id"].as_str().unwrap().into(),
            task_id: name.into(),
            epoch: 1,
        };
        let owner = Owner {
            queue_root: self.queue.clone(),
            queue_id: binding.queue_id.clone(),
            task_id: name.into(),
            epoch: 1,
            instance_id: String::new(),
        };
        let _queue = queue::locked(&self.queue, false).unwrap();
        let permit = std::sync::Arc::new(lifecycle::acquire(&owner, true).unwrap());
        let env = task_env::instance::prepare_isolated(
            &binding,
            &serde_json::from_value(spec["environment"].clone()).unwrap(),
            &self.queue,
            permit,
        )
        .unwrap();
        assert!(!env.state_file().exists());
        let mut state = self.state();
        progress::environment::checkpoint(&mut state, &binding, &env, "creating", util::now())
            .unwrap();
        queue::save(&self.queue, &mut state).unwrap();
        let owner = lifecycle::task_owner(&state, name).unwrap().unwrap();
        assert_eq!(
            state["tasks"][name]["environment"]["creation_seed"],
            json!(env)
        );
        assert!(state["tasks"][name]["environment"]["bootstrap"].is_null());
        owner
    }
    fn finish(&self, owner: &Owner) {
        let s = self.state();
        self.call("task-finalize", &json!({"task":owner.task_id,"queue_id":owner.queue_id,"epoch":owner.epoch,"instance_id":owner.instance_id,
            "state_version":s["tasks"][&owner.task_id]["state_version"],"outcome":"failure","reason":"explicit fixture terminal"})).unwrap();
        self.assert_clean(owner);
    }
    fn assert_clean(&self, owner: &Owner) {
        let name = &owner.task_id;
        assert!(self.state()["tasks"][name].is_null());
        assert!(!self.temp.path().join(format!("{name}-work")).exists());
        assert!(!self.temp.path().join(format!("{name}-tmp")).exists());
        assert!(!task_env::is_registered(name).unwrap());
        assert!(!task_env::environment_state_path(name).unwrap().exists());
        assert!(!task_env::cleanup_state_path(name).unwrap().exists());
        assert!(!queue::artifacts(&self.queue)
            .unwrap()
            .join("gates")
            .join(format!("{name}.lock"))
            .exists());
        assert!(!git(
            &self.temp.path().join("repo"),
            &["worktree", "list", "--porcelain"]
        )
        .contains(&format!("{name}-work/checkout")));
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        match &self.old {
            Some(v) => std::env::set_var("HERDR_TASK_ENV_ROOT", v),
            None => std::env::remove_var("HERDR_TASK_ENV_ROOT"),
        }
    }
}
fn private(path: &Path) {
    fs::create_dir(path).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}
fn git(root: &Path, args: &[&str]) -> String {
    let mut c = Command::new("git");
    c.arg("-C").arg(root).args(args);
    // SAFETY: scalar child-only umask, no mutation of the test process.
    unsafe {
        c.pre_exec(|| {
            libc::umask(0o077);
            Ok(())
        });
    }
    let out = c.output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap().trim().into()
}

#[test]
fn fresh_control_and_seed_precede_publication_and_are_reused_by_another_task() {
    let f = Fixture::new();
    assert!(!f.queue.join("queue.json/task-environments").exists());
    let a = f.create("a");
    let s = f.state();
    let control = s["lifecycle"]["control_bootstrap"].clone();
    assert!(!control["reservation"].is_null());
    assert!(!control["ready"].is_null());
    let record = &s["tasks"]["a"]["environment"];
    assert_eq!(record["creation_seed"]["instance_id"], a.instance_id);
    assert!(record["creation_seed"]["creation"]["allocation"]["envelope"].is_null());
    assert!(!record["bootstrap"].is_null());
    assert_eq!(record["phase"], "active");
    let b = f.create("b");
    assert_eq!(f.state()["lifecycle"]["control_bootstrap"], control);
    assert_eq!(
        f.state()["tasks"]["a"]["environment"]["creation_seed"],
        record["creation_seed"]
    );
    f.finish(&a);
    f.finish(&b);
    assert_eq!(queue::finalization::drive(&f.queue).unwrap().completed, 0);
}

#[test]
fn ordinary_save_cannot_change_seed_initialized_envelope_or_control_reservation() {
    let f = Fixture::new();
    let owner = f.create("a");
    for field in [
        "seed-remove",
        "seed-replace",
        "bootstrap-remove",
        "bootstrap-replace",
        "control",
    ] {
        let before = f.state();
        let mut next = before.clone();
        match field {
            "seed-remove" => {
                next["tasks"]["a"]["environment"]
                    .as_object_mut()
                    .unwrap()
                    .remove("creation_seed");
            }
            "seed-replace" => {
                next["tasks"]["a"]["environment"]["creation_seed"]["instance_id"] = json!("foreign")
            }
            "bootstrap-remove" => next["tasks"]["a"]["environment"]["bootstrap"] = J::Null,
            "bootstrap-replace" => {
                next["tasks"]["a"]["environment"]["bootstrap"]["mount"] = json!(0)
            }
            _ => next["lifecycle"]["control_bootstrap"]["ready"] = J::Null,
        }
        let _queue = queue::locked(&f.queue, false).unwrap();
        assert!(queue::save(&f.queue, &mut next).is_err(), "{field}");
        assert_eq!(f.state(), before);
    }
    f.finish(&owner);
}

#[test]
fn queue_seed_without_environment_document_resumes_the_exact_instance() {
    let f = Fixture::new();
    let keeper = f.create("keeper");
    let owner = f.seed("pending");
    let before = f.state();
    let seed = before["tasks"]["pending"]["environment"]["creation_seed"].clone();
    let spec = json!({"action":"resume","task":"pending","queue_id":owner.queue_id,"epoch":owner.epoch,"instance_id":owner.instance_id,
        "state_version":before["tasks"]["pending"]["state_version"]});
    let after = f.call("task-environment", &spec).unwrap();
    assert_eq!(
        after["tasks"]["pending"]["environment"]["instance_id"],
        owner.instance_id
    );
    assert_eq!(
        after["tasks"]["pending"]["environment"]["creation_seed"],
        seed
    );
    assert!(!after["tasks"]["pending"]["environment"]["bootstrap"].is_null());
    assert_eq!(after["tasks"]["pending"]["environment"]["phase"], "active");
    f.finish(&owner);
    f.finish(&keeper);
}

#[test]
fn queue_seed_without_environment_document_can_finalize_without_materializing_roots() {
    let f = Fixture::new();
    let keeper = f.create("keeper");
    let owner = f.seed("pending");
    assert!(!task_env::environment_state_path("pending")
        .unwrap()
        .exists());
    assert!(!f.temp.path().join("pending-work").exists());
    f.finish(&owner);
    assert_eq!(
        lifecycle::task_owner(&f.state(), "keeper").unwrap(),
        Some(keeper.clone())
    );
    f.finish(&keeper);
}

#[test]
fn known_envelope_missing_or_replaced_never_replays_the_creation_seed() {
    for replace in [false, true] {
        let f = Fixture::new();
        let owner = f.create("a");
        let path = task_env::environment_state_path("a").unwrap();
        let held = f.temp.path().join("held-envdoc");
        fs::rename(&path, &held).unwrap();
        if replace {
            util::protected::create(&path).unwrap();
            util::atomic_write_json(&path, &util::read_json(&held).unwrap()).unwrap();
        }
        let before = f.state();
        let spec = json!({"action":"resume","task":"a","queue_id":owner.queue_id,"epoch":owner.epoch,"instance_id":owner.instance_id,
            "state_version":before["tasks"]["a"]["state_version"]});
        assert!(f.call("task-environment", &spec).is_err());
        assert_eq!(f.state(), before);
        if replace {
            fs::rename(&path, f.temp.path().join("foreign-envdoc-preserved")).unwrap();
        } else {
            assert!(!path.exists());
        }
        fs::rename(&held, &path).unwrap();
        f.finish(&owner);
    }
}

#[test]
fn external_first_control_is_rejected_before_seed_or_business_directories() {
    let f = Fixture::new();
    f.add("a");
    let unsupported = f.temp.path().join("external-control");
    std::env::set_var("HERDR_TASK_ENV_ROOT", &unsupported);
    let before = f.state();
    let error = f.call("task-environment", &f.spec("a")).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("explicitly configured queue-owned"),
        "{error}"
    );
    assert_eq!(f.state(), before);
    assert!(!unsupported.exists());
    assert!(!f.temp.path().join("a-work").exists());
    assert!(!f.temp.path().join("a-tmp").exists());
    assert!(!f.queue.join("queue.json/task-environments").exists());
}

#[test]
fn unreserved_fixed_control_slot_is_not_adopted() {
    for sentinel in [false, true] {
        let f = Fixture::new();
        f.add("a");
        let path = f.queue.join("queue.json/task-environments");
        private(&path);
        if sentinel {
            fs::write(path.join("foreign"), "keep").unwrap();
        }
        let before = f.state();
        let error = f.call("task-environment", &f.spec("a")).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("unreserved control slot already exists"),
            "{error}"
        );
        assert_eq!(f.state(), before);
        assert!(path.is_dir());
        assert!(!f.temp.path().join("a-work").exists());
        assert!(!f.temp.path().join("a-tmp").exists());
        if sentinel {
            assert_eq!(fs::read_to_string(path.join("foreign")).unwrap(), "keep");
        } else {
            assert!(fs::read_dir(&path).unwrap().next().is_none());
        }
    }
}
