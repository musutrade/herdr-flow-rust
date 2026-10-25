//! Instance admission and crash checkpoints over isolated Git/queue fixtures.
use herdr_flow::{guard, model::J, progress, queue, task_env, task_runtime, util};
use serde_json::json;
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::Command,
    sync::{Mutex, MutexGuard},
};
use tempfile::TempDir;

static LOCK: Mutex<()> = Mutex::new(());
const SESSION: &str = "11111111-1111-1111-1111-111111111111";

struct Fixture {
    temp: TempDir,
    queue: PathBuf,
    commit: String,
    old_root: Option<std::ffi::OsString>,
    _lock: MutexGuard<'static, ()>,
}

impl Fixture {
    fn new() -> Self {
        let lock = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let temp = tempfile::Builder::new()
            .permissions(fs::Permissions::from_mode(0o700))
            .tempdir()
            .unwrap();
        let old_root = std::env::var_os("HERDR_TASK_ENV_ROOT");
        std::env::set_var("HERDR_TASK_ENV_ROOT", temp.path().join("control"));
        let repo = temp.path().join("repo");
        fs::create_dir(&repo).unwrap();
        fs::set_permissions(&repo, fs::Permissions::from_mode(0o700)).unwrap();
        git(&repo, &["init", "-q"]);
        git(
            &repo,
            &[
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.test",
                "commit",
                "-q",
                "--allow-empty",
                "-m",
                "fixture",
            ],
        );
        let commit = git(&repo, &["rev-parse", "HEAD"]);
        let queue = temp.path().join("queue");
        queue::init(&queue, None, 30).unwrap();
        let receiver_dir = temp.path().join("receiver");
        fs::create_dir(&receiver_dir).unwrap();
        fs::set_permissions(&receiver_dir, fs::Permissions::from_mode(0o700)).unwrap();
        let receiver = json!({"version":2,"task":"fixture","role":"coordinator","pane":"w1:p1",
            "terminal":"term1","socket":"/fixture","cwd":repo,"session":SESSION,"phase":"monitoring"});
        util::atomic_write_json(&receiver_dir.join("state.json"), &receiver).unwrap();
        let mut s = queue::load_queue(&queue).unwrap();
        s["guard_dir"] = json!(receiver_dir);
        s["receiver"] = guard::scope(&receiver);
        s["tasks"] = json!({"task":{"task":"task","epoch":1,"state_version":1,"phase":"waiting",
            "candidate":commit,"control":"running","writer":null,"pending":[],"operations":{}}});
        queue::save(&queue, &mut s).unwrap();
        Self {
            temp,
            queue,
            commit,
            old_root,
            _lock: lock,
        }
    }

    fn state(&self) -> J {
        queue::load_queue(&self.queue).unwrap()
    }

    fn create_spec(&self) -> J {
        let s = self.state();
        json!({"action":"create","queue_id":s["id"],"task":"task","epoch":1,
            "state_version":s["tasks"]["task"]["state_version"],"environment":{
                "workspace_root":self.temp.path().join("workspace"),"temp_root":self.temp.path().join("scratch"),
                "delivery_targets":[],"repository":self.temp.path().join("repo"),"commit":self.commit}})
    }

    fn resume_spec(&self, id: &str) -> J {
        let mut spec = self.create_spec();
        spec["action"] = json!("resume");
        spec["instance_id"] = json!(id);
        spec.as_object_mut().unwrap().remove("environment");
        spec
    }

    fn call(&self, spec: &J) -> herdr_flow::Result<J> {
        let _lock = queue::locked(&self.queue, false)?;
        let factory = |_: &J| -> herdr_flow::Result<Box<dyn herdr_flow::herdr::ReceiverAdapter>> {
            panic!("environment admission must not contact Herdr")
        };
        queue::task_action(
            &self.queue,
            "task-environment",
            spec,
            Some(SESSION),
            &factory,
        )
    }

    fn binding(&self) -> task_env::instance::InstanceBinding {
        task_env::instance::InstanceBinding {
            queue_id: self.state()["id"].as_str().unwrap().into(),
            task_id: "task".into(),
            epoch: 1,
        }
    }

    fn env_spec(&self) -> task_env::instance::CreateSpec {
        task_env::instance::CreateSpec {
            workspace_root: self.temp.path().join("workspace"),
            temp_root: self.temp.path().join("scratch"),
            delivery_targets: vec![],
            repository: self.temp.path().join("repo"),
            commit: self.commit.clone(),
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        match &self.old_root {
            Some(value) => std::env::set_var("HERDR_TASK_ENV_ROOT", value),
            None => std::env::remove_var("HERDR_TASK_ENV_ROOT"),
        }
    }
}

fn git(repo: &Path, args: &[&str]) -> String {
    use std::os::unix::process::CommandExt;
    let mut command = Command::new("git");
    command.arg("-C").arg(repo).args(args);
    // SAFETY: umask takes a scalar and only changes this fork child's mask.
    unsafe {
        command.pre_exec(|| {
            libc::umask(0o077);
            Ok(())
        });
    }
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

#[test]
fn creates_queue_owned_checkout_and_resumes_the_same_instance() {
    let f = Fixture::new();
    let s = f.call(&f.create_spec()).unwrap();
    let mapping = &s["tasks"]["task"]["environment"];
    assert_eq!(mapping["phase"], "active");
    assert_eq!(s["tasks"]["task"]["state_version"], 3);
    let env = task_env::restore_environment("task").unwrap();
    task_env::instance::verify_instance(&env, &f.binding(), &env.instance_id).unwrap();
    assert_eq!(mapping["state_file"], json!(env.state_file()));
    assert_eq!(
        mapping["checkout"],
        json!(env.workspace_root.join("checkout"))
    );
    assert_eq!(
        git(&env.workspace_root.join("checkout"), &["rev-parse", "HEAD"]),
        f.commit
    );
    let id = env.instance_id.clone();
    let roots = (env.workspace_identity, env.temp_identity);
    drop(env);
    let s = f.call(&f.resume_spec(&id)).unwrap();
    assert_eq!(s["tasks"]["task"]["environment"]["instance_id"], id);
    let env = task_env::restore_environment("task").unwrap();
    assert_eq!((env.workspace_identity, env.temp_identity), roots);
    assert!(f.call(&f.create_spec()).is_err());
}

#[test]
fn resumes_intent_left_before_the_first_queue_checkpoint() {
    let f = Fixture::new();
    let env = task_env::instance::begin_instance(&f.binding(), &f.env_spec()).unwrap();
    let id = env.instance_id.clone();
    drop(env);
    assert!(f.state()["tasks"]["task"]["environment"].is_null());
    assert!(f.call(&f.create_spec()).is_err());
    let s = f.call(&f.resume_spec(&id)).unwrap();
    assert_eq!(s["tasks"]["task"]["environment"]["instance_id"], id);
    assert_eq!(s["tasks"]["task"]["environment"]["phase"], "active");
}

#[test]
fn admission_rejects_busy_tasks_before_allocating_an_environment() {
    let f = Fixture::new();
    let original = f.state();
    for (key, value) in [
        ("writer", json!({"workspace":"/shared"})),
        ("pending", json!(["event"])),
        ("operations", json!({"op":{"phase":"prepared"}})),
        ("lane", json!({"active":{}})),
        ("pr", json!({"publication":{}})),
        ("control", json!("paused")),
    ] {
        let mut s = original.clone();
        s["tasks"]["task"][key] = value;
        queue::save(&f.queue, &mut s).unwrap();
        assert!(f.call(&f.create_spec()).is_err(), "accepted busy {key}");
        assert!(!f.temp.path().join("workspace").exists());
        assert!(!f.temp.path().join("control/envs/task.json").exists());
    }
}

#[test]
fn admission_rejects_wrong_queue_epoch_and_stale_version() {
    let f = Fixture::new();
    for (key, value) in [
        ("queue_id", json!("foreign")),
        ("epoch", json!(2)),
        ("state_version", json!(0)),
        ("task", json!("other")),
    ] {
        let mut spec = f.create_spec();
        spec[key] = value;
        assert!(f.call(&spec).is_err(), "accepted wrong {key}");
    }
    let mut wrong_commit = f.create_spec();
    wrong_commit["environment"]["commit"] = json!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
    assert!(f
        .call(&wrong_commit)
        .unwrap_err()
        .to_string()
        .contains("candidate"));
    let mut unknown = f.create_spec();
    unknown["environment"]["unknown_option"] = json!(true);
    assert!(f.call(&unknown).is_err());
    assert!(!f.temp.path().join("workspace").exists());
    assert!(!f.temp.path().join("control/envs/task.json").exists());
}

#[test]
fn resume_rejects_replacement_identity_and_changed_intent() {
    let f = Fixture::new();
    let s = f.call(&f.create_spec()).unwrap();
    let id = s["tasks"]["task"]["environment"]["instance_id"]
        .as_str()
        .unwrap();
    assert!(f.call(&f.resume_spec("different-instance")).is_err());
    let mut spec = f.resume_spec(id);
    spec["environment"] = f.create_spec()["environment"].clone();
    assert!(f.call(&spec).is_err());
    let mut state = f.state();
    state["tasks"]["task"]["environment"]["checkout"] = json!(f.temp.path().join("other"));
    queue::save(&f.queue, &mut state).unwrap();
    assert!(f.call(&f.resume_spec(id)).is_err());
    assert_eq!(
        f.state()["tasks"]["task"]["environment"]["checkout"],
        state["tasks"]["task"]["environment"]["checkout"]
    );
}

#[test]
fn runtime_owner_rejects_cross_queue_epoch_and_instance_without_a_model() {
    let f = Fixture::new();
    let mut s = f.call(&f.create_spec()).unwrap();
    let env = task_env::restore_environment("task").unwrap();
    let activity = json!({"task":"task"});
    progress::environment::runtime_owner(&s, &env, &activity).unwrap();
    let initial = s.clone();
    s["id"] = json!("another-queue");
    assert!(progress::environment::runtime_owner(&s, &env, &activity).is_err());
    s = initial.clone();
    s["tasks"]["task"]["epoch"] = json!(2);
    assert!(progress::environment::runtime_owner(&s, &env, &activity).is_err());
    s = initial.clone();
    s["tasks"]["task"]["environment"]["instance_id"] = json!("replacement");
    assert!(progress::environment::runtime_owner(&s, &env, &activity).is_err());
    assert!(
        progress::environment::runtime_owner(&initial, &env, &json!({"task":"other"})).is_err()
    );
    let runtime = task_runtime::prepare(&env, "assignment").unwrap();
    assert!(runtime.is_dir());
    assert!(env.child_processes.is_empty());
}
