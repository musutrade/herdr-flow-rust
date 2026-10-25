//! Fixed slots owned by a durable parent reservation, then inode-pinned handoff.
mod bootstrap;
mod control;
mod fs;
mod transfer;
use super::{require, RootIdentity, TaskEnvironment};
use crate::{model::J, Result};
pub(crate) use bootstrap::{initialize_prepared, restore};
pub(crate) use control::{
    initialize as initialize_control, request as control_request, verify as verify_control,
    verify_current as verify_control_current,
};
use serde::{Deserialize, Serialize};
pub(crate) use transfer::settle_terminal;
pub(super) use transfer::{materialize, verify_recorded};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Allocation {
    pub parent: fs::Directory,
    pub mount_namespace: RootIdentity,
    pub workspace_parent: fs::Directory,
    pub temp_parent: fs::Directory,
    pub envelope: Option<fs::Directory>,
    pub transfer: Option<Transfer>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Transfer {
    pub role: Role,
    pub target_parent: fs::Directory,
    pub identity: Option<RootIdentity>,
    pub discarding: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Workspace,
    Temporary,
    Checkout,
}

pub(crate) fn prepare(env: &mut TaskEnvironment) -> Result<()> {
    let q = super::intent(env)?.lifecycle_queue.as_ref().unwrap();
    control::verify_current(q)?;
    let parent = fs::Directory::capture(env.state_file().parent().unwrap())?;
    let workspace_parent = fs::Directory::capture(env.workspace_root.parent().unwrap())?;
    let temp_parent = fs::Directory::capture(env.temp_root.parent().unwrap())?;
    require(
        workspace_parent.mount == parent.mount && temp_parent.mount == parent.mount,
        "no managed allocator on target mount",
    )?;
    env.creation.as_mut().unwrap().allocation = Some(Allocation {
        parent,
        workspace_parent,
        temp_parent,
        mount_namespace: RootIdentity::of(&std::fs::metadata("/proc/self/ns/mnt")?),
        envelope: None,
        transfer: None,
    });
    Ok(())
}

fn allocation(env: &TaskEnvironment) -> Result<&Allocation> {
    super::intent(env)?
        .allocation
        .as_ref()
        .ok_or_else(|| crate::FlowError::guard("creation allocation intent missing"))
}

fn verify(env: &TaskEnvironment) -> Result<()> {
    let a = allocation(env)?;
    verify_parent(env, a)?;
    if let Some(envelope) = &a.envelope {
        require(
            envelope.path == env.state_file(),
            "allocation envelope path changed",
        )?;
        envelope.verify()?;
    }
    Ok(())
}
fn verify_parent(env: &TaskEnvironment, a: &Allocation) -> Result<()> {
    require(
        a.mount_namespace == RootIdentity::of(&std::fs::metadata("/proc/self/ns/mnt")?),
        "allocation mount namespace changed",
    )?;
    control::verify_current(super::intent(env)?.lifecycle_queue.as_ref().unwrap())?;
    require(
        env.state_file().parent() == Some(a.parent.path.as_path()),
        "allocation control parent changed",
    )?;
    a.parent.verify()?;
    Ok(())
}

fn seed(env: &TaskEnvironment) -> Result<J> {
    let owner = crate::task_env::isolation::owner(env)
        .ok_or_else(|| crate::FlowError::guard("allocation owner missing"))?;
    let q = crate::queue::load_queue(&owner.queue_root)?;
    require(
        crate::lifecycle::task_owner(&q, &owner.task_id)?.as_ref() == Some(&owner),
        "allocation queue owner changed",
    )?;
    let mapping = &q["tasks"][&owner.task_id]["environment"];
    require(
        mapping["state_file"] == serde_json::json!(env.state_file()),
        "allocation state path changed",
    )?;
    Ok(mapping.clone())
}

pub(crate) fn persist(env: &TaskEnvironment) -> Result<()> {
    verify(env)?;
    let proof = allocation(env)?
        .envelope
        .as_ref()
        .ok_or_else(|| crate::FlowError::guard("envelope not initialized"))?;
    let directory = proof.verify()?;
    fs::write_document(&directory, &serde_json::to_vec(env)?)?;
    proof.verify()?;
    Ok(())
}

/// No lazy parent creation: ready control proofs are checked before opening
/// the registry and task lock in those existing, descriptor-pinned directories.
pub(crate) fn lock_seed(env: &TaskEnvironment) -> Result<std::fs::File> {
    use std::os::fd::AsRawFd;
    let (_root, _registry_lock) = seed_registry_lock(env)?;
    verify(env)?;
    let control = super::super::registry::control_path()?;
    let locks = fs::Directory::capture(&control.join("registry.json/locks"))?.verify()?;
    let lock = fs::lock_file(&locks, &format!("{}.lock", env.task_id))?;
    require(
        crate::util::flock_try_exclusive(lock.as_raw_fd())?,
        "environment already owned",
    )?;
    Ok(lock)
}
fn seed_registry_lock(env: &TaskEnvironment) -> Result<(std::fs::File, std::fs::File)> {
    use std::os::fd::AsRawFd;
    verify(env)?;
    let control = super::super::registry::control_path()?;
    let root = fs::Directory::capture(&control)?.verify()?;
    let lock = fs::lock_file(&root, "registry.lock")?;
    crate::util::flock_exclusive(lock.as_raw_fd())?;
    Ok((root, lock))
}

#[cfg(test)]
mod tests {
    //! Real subprocess interruption, not fabricated creation checkpoints. These
    //! fixtures cover process death, not storage-cache loss or power failure.
    use crate::{guard, lifecycle, model::J, queue, task_env, util};
    use serde_json::json;
    use std::{
        fs,
        os::unix::{fs::PermissionsExt, process::CommandExt},
        path::{Path, PathBuf},
        process::{Child, Command, Stdio},
        time::{Duration, Instant},
    };
    const SESSION: &str = "11111111-1111-1111-1111-111111111111";

    pub(super) fn checkpoint(point: &str) {
        if std::env::var("FLOW_ALLOCATION_PAUSE").ok().as_deref() != Some(point) {
            return;
        }
        let path = PathBuf::from(std::env::var_os("FLOW_ALLOCATION_READY").unwrap());
        fs::write(path, point).unwrap();
        loop {
            std::thread::park_timeout(Duration::from_secs(60));
        }
    }

    struct Fixture {
        temp: tempfile::TempDir,
        queue: PathBuf,
        _root: task_env::TestEnvRootGuard,
    }
    impl Fixture {
        fn new() -> Self {
            let temp = tempfile::Builder::new()
                .permissions(fs::Permissions::from_mode(0o700))
                .tempdir()
                .unwrap();
            let queue = temp.path().join("queue");
            let root = task_env::test_env_root(&queue.join("queue.json/task-environments"));
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
            queue::init_isolated(&queue, None, 30).unwrap();
            let receiver = temp.path().join("receiver");
            private(&receiver);
            let receiver_state = json!({"version":2,"task":"coordinator","role":"coordinator","pane":"w1:p1","terminal":"term1","socket":"/fixture","cwd":repo,"session":SESSION,"phase":"monitoring"});
            util::atomic_write_json(&receiver.join("state.json"), &receiver_state).unwrap();
            let mut s = queue::load_queue(&queue).unwrap();
            s["guard_dir"] = json!(receiver);
            s["receiver"] = guard::scope(&receiver_state);
            s["tasks"] = json!({"task":{"task":"task","epoch":1,"state_version":1,"candidate":commit,"phase":"waiting","control":"running","writer":null,"pending":[],"operations":{}}});
            queue::save(&queue, &mut s).unwrap();
            fs::write(temp.path().join("formal-delivery"), b"preserve").unwrap();
            Self {
                temp,
                queue,
                _root: root,
            }
        }
        fn state(&self) -> J {
            queue::load_queue(&self.queue).unwrap()
        }
        fn create_spec(&self) -> J {
            let s = self.state();
            json!({"action":"create","task":"task","state_version":s["tasks"]["task"]["state_version"],"queue_id":s["id"],"epoch":1,
                "environment":{"workspace_root":self.temp.path().join("workspace"),"temp_root":self.temp.path().join("scratch"),"delivery_targets":[],"repository":self.temp.path().join("repo"),"commit":s["tasks"]["task"]["candidate"]}})
        }
        fn call(&self, action: &str, spec: &J) -> crate::Result<J> {
            action_call(&self.queue, action, spec)
        }
        fn resume(&self) -> crate::Result<J> {
            let s = self.state();
            if s["tasks"]["task"]["environment"].is_null() {
                return self.call("task-environment", &self.create_spec());
            }
            self.call("task-environment", &json!({"action":"resume","task":"task","queue_id":s["id"],"epoch":1,
                "instance_id":s["tasks"]["task"]["environment"]["instance_id"],"state_version":s["tasks"]["task"]["state_version"]}))
        }
        fn owner(&self) -> lifecycle::Owner {
            lifecycle::task_owner(&self.state(), "task")
                .unwrap()
                .unwrap()
        }
        fn finish(&self) {
            let s = self.state();
            let o = self.owner();
            self.call("task-finalize", &json!({"task":"task","queue_id":o.queue_id,"epoch":o.epoch,"instance_id":o.instance_id,
                "state_version":s["tasks"]["task"]["state_version"],"outcome":"failure","reason":"fixture final decision"})).unwrap();
            let report = queue::finalization::drive(&self.queue).unwrap();
            assert!(self.state()["tasks"]["task"].is_null(), "{report:?}");
            self.clean();
        }
        fn clean(&self) {
            assert!(!self.temp.path().join("workspace").exists());
            assert!(!self.temp.path().join("scratch").exists());
            let control = self.queue.join("queue.json/task-environments");
            for path in [
                "registry.json/envs/task.json",
                "registry.json/plans/task.json",
                "registry.json/locks/task.lock",
            ] {
                assert!(!control.join(path).exists(), "{path}");
            }
            assert!(!queue::artifacts(&self.queue)
                .unwrap()
                .join("gates/task.lock")
                .exists());
            assert!(!task_env::is_registered("task").unwrap());
            assert!(!git(
                &self.temp.path().join("repo"),
                &["worktree", "list", "--porcelain"]
            )
            .contains("workspace/checkout"));
            assert_eq!(
                fs::read(self.temp.path().join("formal-delivery")).unwrap(),
                b"preserve"
            );
            assert!(self.temp.path().join("receiver/state.json").is_file());
            assert_eq!(
                queue::finalization::drive(&self.queue).unwrap().completed,
                0
            );
        }
        fn interrupt(&self, point: &str) {
            let ready = self.temp.path().join("pause-ready");
            let input = self.temp.path().join("worker-input.json");
            fs::write(
                &input,
                serde_json::to_vec(&json!({"queue":self.queue,"spec":self.create_spec()})).unwrap(),
            )
            .unwrap();
            let output = fs::File::create(self.temp.path().join("worker.log")).unwrap();
            let mut child = TestChild(
                Command::new(std::env::current_exe().unwrap())
                    .args([
                        "--exact",
                        "task_env::instance::allocation::tests::allocation_crash_worker",
                        "--ignored",
                        "--nocapture",
                    ])
                    .env("FLOW_ALLOCATION_PAUSE", point)
                    .env("FLOW_ALLOCATION_READY", &ready)
                    .env("FLOW_ALLOCATION_INPUT", &input)
                    .stdout(Stdio::from(output.try_clone().unwrap()))
                    .stderr(Stdio::from(output))
                    .spawn()
                    .unwrap(),
            );
            let deadline = Instant::now() + Duration::from_secs(15);
            while !ready.exists() {
                assert!(
                    child.0.try_wait().unwrap().is_none(),
                    "worker exited at {point}: {}",
                    fs::read_to_string(self.temp.path().join("worker.log")).unwrap()
                );
                assert!(Instant::now() < deadline, "worker pause timeout: {point}");
                std::thread::sleep(Duration::from_millis(5));
            }
            assert_eq!(fs::read_to_string(&ready).unwrap(), point);
            assert!(child.0.try_wait().unwrap().is_none());
            child.0.kill().unwrap();
            let status = child.0.wait().unwrap();
            use std::os::unix::process::ExitStatusExt;
            assert_eq!(status.signal(), Some(libc::SIGKILL));
        }
    }
    struct TestChild(Child);
    impl Drop for TestChild {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    fn private(p: &Path) {
        fs::create_dir(p).unwrap();
        fs::set_permissions(p, fs::Permissions::from_mode(0o700)).unwrap();
    }
    fn git(p: &Path, args: &[&str]) -> String {
        let mut c = Command::new("git");
        c.arg("-C").arg(p).args(args);
        // SAFETY: scalar, child-only umask.
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
    fn action_call(root: &Path, action: &str, spec: &J) -> crate::Result<J> {
        queue::finalization::task_action(root, action, spec, Some(SESSION), &|_| {
            panic!("no model/native calls")
        })
    }

    #[test]
    #[ignore = "subprocess worker; invoked only by allocation process interruption fixtures"]
    fn allocation_crash_worker() {
        let input = PathBuf::from(std::env::var_os("FLOW_ALLOCATION_INPUT").unwrap());
        let data: J = serde_json::from_slice(&fs::read(input).unwrap()).unwrap();
        if data["mode"] == "read-initial" {
            let parent =
                super::fs::Directory::capture(Path::new(data["directory"].as_str().unwrap()))
                    .unwrap()
                    .verify()
                    .unwrap();
            assert!(super::fs::read_initial(&parent, "authority.json").is_err());
            return;
        }
        action_call(
            Path::new(data["queue"].as_str().unwrap()),
            "task-environment",
            &data["spec"],
        )
        .unwrap();
        panic!("requested checkpoint was not reached");
    }

    fn competing_spec(f: &Fixture) -> J {
        let _lock = queue::locked(&f.queue, false).unwrap();
        let mut state = f.state();
        state["tasks"]["competitor"] = json!({"task":"competitor","epoch":1,
            "state_version":1,"candidate":state["tasks"]["task"]["candidate"],
            "phase":"waiting","control":"running","writer":null,"pending":[],"operations":{}});
        queue::save(&f.queue, &mut state).unwrap();
        let mut spec = f.create_spec();
        spec["task"] = json!("competitor");
        spec["state_version"] = json!(1);
        spec
    }

    fn finish_competitor(f: &Fixture) {
        let state = f.state();
        let owner = lifecycle::task_owner(&state, "competitor")
            .unwrap()
            .unwrap();
        f.call(
            "task-finalize",
            &json!({"task":"competitor","queue_id":owner.queue_id,
            "epoch":owner.epoch,"instance_id":owner.instance_id,
            "state_version":state["tasks"]["competitor"]["state_version"],
            "outcome":"failure","reason":"fixture successor cleanup"}),
        )
        .unwrap();
        assert!(f.state()["tasks"]["competitor"].is_null());
        assert!(!task_env::is_registered("competitor").unwrap());
        let control = f.queue.join("queue.json/task-environments/registry.json");
        for path in [
            "envs/competitor.json",
            "plans/competitor.json",
            "locks/competitor.lock",
        ] {
            assert!(!control.join(path).exists(), "{path}");
        }
        assert!(!queue::artifacts(&f.queue)
            .unwrap()
            .join("gates/competitor.lock")
            .exists());
        f.clean();
    }

    #[test]
    fn seed_saved_process_death_reserves_roots_until_owner_removal() {
        let f = Fixture::new();
        f.interrupt("seed-saved");
        let owner = f.owner();
        assert!(!task_env::is_registered("task").unwrap());
        assert!(!task_env::environment_state_path("task").unwrap().exists());
        assert!(!f.temp.path().join("workspace").exists());
        assert!(!f.temp.path().join("scratch").exists());
        let spec = competing_spec(&f);
        let before = f.state();
        let rejected = f.call("task-environment", &spec);
        assert!(
            rejected.is_err(),
            "competitor acquired A's durable seed reservation"
        );
        assert!(rejected
            .unwrap_err()
            .to_string()
            .contains("reserved by another creation seed"));
        assert_eq!(f.state(), before);
        assert_eq!(f.owner(), owner);
        assert!(!task_env::is_registered("competitor").unwrap());
        assert!(!task_env::environment_state_path("competitor")
            .unwrap()
            .exists());
        f.finish();
        // Deleting the completed owner row releases its reservation; no tombstone.
        f.call("task-environment", &spec).unwrap();
        finish_competitor(&f);
    }

    // Pure checkpoint probes intentionally bypass filesystem preflight: a child
    // of an unmaterialized root would otherwise fail earlier for missing parent.
    fn memory_candidate(f: &Fixture, name: &str) -> (J, task_env::TaskEnvironment) {
        let mut state = f.state();
        let mut env: task_env::TaskEnvironment =
            serde_json::from_value(state["tasks"]["task"]["environment"]["creation_seed"].clone())
                .unwrap();
        env.task_id = name.into();
        env.instance_id = util::uuid_hex();
        env.workspace_root = f.temp.path().join(format!("{name}-work"));
        env.temp_root = f.temp.path().join(format!("{name}-tmp"));
        env.state_file.set_file_name(format!("{name}.json"));
        let intent = env.creation.as_mut().unwrap();
        intent.binding.task_id = name.into();
        intent.checkout = env.workspace_root.join("checkout");
        let mut task = state["tasks"]["task"].clone();
        task.as_object_mut().unwrap().remove("environment");
        task["task"] = json!(name);
        task["state_version"] = json!(1);
        state["tasks"][name] = task;
        (state, env)
    }
    fn memory_checkpoint(state: &mut J, env: &task_env::TaskEnvironment) -> crate::Result<()> {
        crate::progress::environment::checkpoint(
            state,
            &env.creation.as_ref().unwrap().binding,
            env,
            "creating",
            util::now(),
        )
    }
    fn assert_seed_conflict(mut state: J, env: &task_env::TaskEnvironment) {
        let before = state.clone();
        let error = memory_checkpoint(&mut state, env).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("reserved by another creation seed"),
            "{error}"
        );
        assert_eq!(state, before);
    }

    #[test]
    fn seed_reservations_reject_both_root_roles_and_component_ancestors() {
        let f = Fixture::new();
        f.interrupt("seed-saved");
        for reserved in [
            f.temp.path().join("workspace"),
            f.temp.path().join("scratch"),
        ] {
            for target in [
                reserved.clone(),
                reserved.join("child"),
                reserved.parent().unwrap().to_path_buf(),
            ] {
                let (state, mut env) = memory_candidate(&f, "candidate");
                env.workspace_root = target.clone();
                assert_seed_conflict(state, &env);
                let (state, mut env) = memory_candidate(&f, "candidate");
                env.temp_root = target;
                assert_seed_conflict(state, &env);
            }
        }
        let (mut state, mut env) = memory_candidate(&f, "candidate");
        env.workspace_root = f.temp.path().join("workspace2");
        env.temp_root = f.temp.path().join("scratch2");
        memory_checkpoint(&mut state, &env).unwrap();
        f.finish();
    }

    #[test]
    fn seed_reservations_do_not_expire_from_terminal_or_cleanup_status() {
        let f = Fixture::new();
        f.interrupt("seed-saved");
        for status in ["pending", "associations_removed", "released", "completed"] {
            let (mut state, mut env) = memory_candidate(&f, "candidate");
            state["tasks"]["task"]["final_outcome"] = json!("failure");
            state["tasks"]["task"]["phase"] = json!("finalizing");
            state["tasks"]["task"]["cleanup_obligation"]["status"] = json!(status);
            env.temp_root = f.temp.path().join("workspace");
            assert_seed_conflict(state, &env);
        }
        f.resume().unwrap(); // Existing seed checkpoints never conflict with self.
        f.finish();
    }

    #[test]
    fn seed_reservations_reject_malformed_or_foreign_bindings_without_checkpoint() {
        let f = Fixture::new();
        f.interrupt("seed-saved");
        for (path, value) in [
            ("/environment/creation_seed", J::Null),
            ("/environment/creation_seed", json!({})),
            ("/environment/creation_seed/workspace_root", json!("")),
            ("/environment/creation_seed/temp_root", json!("relative")),
            (
                "/environment/creation_seed/creation/binding/queue_id",
                json!("foreign"),
            ),
            (
                "/environment/creation_seed/creation/binding/task_id",
                json!("foreign"),
            ),
            (
                "/environment/creation_seed/creation/binding/epoch",
                json!(2),
            ),
            ("/environment/creation_seed/instance_id", json!("foreign")),
            (
                "/environment/creation_seed/creation/lifecycle_queue",
                json!("/foreign"),
            ),
            ("/environment/creation_seed/state_file", json!("/foreign")),
            ("/environment/epoch", json!(2)),
            ("/environment/instance_id", json!("foreign")),
            ("/task", json!("foreign")),
            ("/epoch", json!(2)),
        ] {
            let (mut state, env) = memory_candidate(&f, "candidate");
            *state["tasks"]["task"].pointer_mut(path).unwrap() = value;
            let before = state.clone();
            assert!(memory_checkpoint(&mut state, &env).is_err(), "{path}");
            assert_eq!(state, before, "{path}");
        }
        f.finish();
    }

    fn raw_candidate_record(f: &Fixture, name: &str, shared: bool) -> J {
        let (mut state, mut env) = memory_candidate(f, name);
        if shared {
            env.workspace_root = f.temp.path().join("workspace");
        } else {
            env.workspace_root = f.temp.path().join("two-new-owners");
        }
        // Build the standalone candidate without its competitor only in memory.
        state["tasks"].as_object_mut().unwrap().remove("task");
        memory_checkpoint(&mut state, &env).unwrap();
        state["tasks"][name].clone()
    }

    #[test]
    fn seed_save_allows_unregistered_queue_but_rejects_malformed_task_tables() {
        let temp = tempfile::Builder::new()
            .permissions(fs::Permissions::from_mode(0o700))
            .tempdir()
            .unwrap();
        let root = temp.path().join("queue");
        queue::init_isolated(&root, None, 30).unwrap();
        let _lock = queue::locked(&root, false).unwrap();
        let mut state = queue::load_queue(&root).unwrap();
        assert!(state.get("tasks").is_none());
        queue::save(&root, &mut state).unwrap();
        let bytes = fs::read(root.join("queue.json/authority.json")).unwrap();
        for malformed in [J::Null, json!([]), json!("invalid")] {
            let mut next = state.clone();
            next["tasks"] = malformed;
            let input = next.clone();
            assert!(queue::save(&root, &mut next).is_err());
            assert_eq!(next, input);
            assert_eq!(
                fs::read(root.join("queue.json/authority.json")).unwrap(),
                bytes
            );
        }
        state["tasks"] = json!({});
        queue::save(&root, &mut state).unwrap();
        let bytes = fs::read(root.join("queue.json/authority.json")).unwrap();
        state.as_object_mut().unwrap().remove("tasks");
        let input = state.clone();
        assert!(queue::save(&root, &mut state).is_err());
        assert_eq!(state, input);
        assert_eq!(
            fs::read(root.join("queue.json/authority.json")).unwrap(),
            bytes
        );
    }

    #[test]
    fn seed_save_rejects_malformed_current_tables_without_repair() {
        let temp = tempfile::Builder::new()
            .permissions(fs::Permissions::from_mode(0o700))
            .tempdir()
            .unwrap();
        let root = temp.path().join("queue");
        queue::init_isolated(&root, None, 30).unwrap();
        let _lock = queue::locked(&root, false).unwrap();
        let original = queue::load_queue(&root).unwrap();
        for malformed in [J::Null, json!([]), json!("invalid")] {
            let mut damaged = original.clone();
            damaged["tasks"] = malformed;
            // Corrupt only the private fixture authority, bypassing admission.
            util::atomic_write_json(&root.join("queue.json"), &damaged).unwrap();
            let bytes = fs::read(root.join("queue.json/authority.json")).unwrap();
            let mut next = damaged.clone();
            next["tasks"] = json!({});
            let input = next.clone();
            let error = queue::save(&root, &mut next).unwrap_err();
            assert!(
                error.to_string().contains("tasks must be an object"),
                "{error}"
            );
            assert_eq!(next, input);
            assert_eq!(queue::load_queue(&root).unwrap(), damaged);
            assert_eq!(
                fs::read(root.join("queue.json/authority.json")).unwrap(),
                bytes
            );
        }
    }

    #[test]
    fn seed_reservations_guard_raw_save_and_two_new_owners_in_one_transaction() {
        let f = Fixture::new();
        f.interrupt("seed-saved");
        for shared in [true, false] {
            let before = f.state();
            let bytes = fs::read(f.queue.join("queue.json/authority.json")).unwrap();
            let mut next = before.clone();
            next["tasks"]["candidate"] = raw_candidate_record(&f, "candidate", shared);
            if !shared {
                next["tasks"]["second"] = raw_candidate_record(&f, "second", false);
            }
            let input = next.clone();
            {
                let _lock = queue::locked(&f.queue, false).unwrap();
                let error = queue::save(&f.queue, &mut next).unwrap_err();
                assert!(
                    error
                        .to_string()
                        .contains("reserved by another creation seed"),
                    "{error}"
                );
            }
            assert_eq!(next, input);
            assert_eq!(f.state(), before);
            assert_eq!(
                fs::read(f.queue.join("queue.json/authority.json")).unwrap(),
                bytes
            );
            assert!(!f.temp.path().join("two-new-owners").exists());
        }
        f.finish();
    }

    #[test]
    fn control_and_envelope_process_interruptions_resume_without_new_owner() {
        for point in [
            "control-reserved",
            "mkdir:task-environments",
            "mkdir:registry.json",
            "initial:.flow-document-v2",
            "initial:authority.json",
            "mkdir:queue-binding.json",
            "mkdir:envs",
            "mkdir:plans",
            "mkdir:locks",
            "control-initialized",
            "seed-saved",
            "mkdir:task.json",
            "initial:task.json:.flow-document-v2",
            "initial:task.json:authority.json",
            "envelope-initialized",
        ] {
            for continue_creation in [false, true] {
                let f = Fixture::new();
                f.interrupt(point);
                let before = f.state()["tasks"]["task"]["environment"]["instance_id"].clone();
                if continue_creation || before.is_null() {
                    f.resume().unwrap_or_else(|e| panic!("{point}: {e}"));
                    if !before.is_null() {
                        assert_eq!(
                            f.state()["tasks"]["task"]["environment"]["instance_id"],
                            before
                        );
                    }
                }
                f.finish();
            }
        }
    }

    #[test]
    fn allocation_transfer_process_interruptions_resume_or_finalize_and_really_clear() {
        for role in [
            "allocation-workspace",
            "allocation-temporary",
            "allocation-checkout",
        ] {
            for boundary in [
                "intent",
                "mkdir",
                "identity",
                "before-transfer",
                "transferred",
                "synced",
                "published",
            ] {
                for continue_creation in [false, true] {
                    let point = format!("{boundary}:{role}");
                    let f = Fixture::new();
                    f.interrupt(&point);
                    let owner = f.owner();
                    if continue_creation {
                        f.resume().unwrap_or_else(|e| panic!("{point}: {e}"));
                        assert_eq!(f.owner(), owner);
                    }
                    f.finish();
                }
            }
        }
    }

    #[test]
    fn durable_allocation_and_target_replacements_are_not_adopted_or_deleted() {
        for point in [
            "identity:allocation-workspace",
            "transferred:allocation-workspace",
        ] {
            let f = Fixture::new();
            f.interrupt(point);
            let state = f.state();
            let original = if point.starts_with("identity") {
                PathBuf::from(
                    state["tasks"]["task"]["environment"]["state_file"]
                        .as_str()
                        .unwrap(),
                )
                .join("allocation-workspace")
            } else {
                f.temp.path().join("workspace")
            };
            let preserved = f.temp.path().join("original");
            fs::rename(&original, &preserved).unwrap();
            private(&original);
            // The replacement is deliberately empty: inode identity, rather than
            // unexpected contents, must be sufficient to reject it.
            let foreign = super::RootIdentity::of(&fs::symlink_metadata(&original).unwrap());
            assert!(f.resume().is_err());
            assert_eq!(
                super::RootIdentity::of(&fs::symlink_metadata(&original).unwrap()),
                foreign
            );
            fs::remove_dir(&original).unwrap();
            fs::rename(preserved, &original).unwrap();
            f.finish();
        }
    }

    #[test]
    fn another_mount_rejects_before_control_reservation_or_business_roots() {
        let f = Fixture::new();
        let other = tempfile::Builder::new()
            .permissions(fs::Permissions::from_mode(0o700))
            .tempdir_in("/dev/shm")
            .unwrap();
        let here = super::fs::Directory::capture(f.temp.path()).unwrap();
        let there = super::fs::Directory::capture(other.path()).unwrap();
        assert_ne!(here.mount, there.mount, "fixture requires two real mounts");
        let mut spec = f.create_spec();
        spec["environment"]["temp_root"] = json!(other.path().join("scratch"));
        let before = f.state();
        assert!(f
            .call("task-environment", &spec)
            .unwrap_err()
            .to_string()
            .contains("mount"));
        assert_eq!(f.state(), before);
        assert!(!other.path().join("scratch").exists());
        assert!(!f.queue.join("queue.json/task-environments").exists());
        assert!(!f.temp.path().join("workspace").exists());
    }

    #[test]
    fn bootstrap_nonregular_and_unreadable_payloads_refuse_without_blocking_or_repair() {
        use std::os::unix::ffi::OsStrExt;
        for kind in ["fifo", "directory", "symlink", "unreadable"] {
            let f = Fixture::new();
            let directory = f.temp.path().join("bad-payload");
            private(&directory);
            let path = directory.join("authority.json");
            match kind {
                "fifo" => {
                    let name = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
                    // SAFETY: fixture-owned NUL-terminated path, private FIFO mode.
                    assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
                }
                "directory" => private(&path),
                "symlink" => {
                    std::os::unix::fs::symlink(f.temp.path().join("formal-delivery"), &path)
                        .unwrap()
                }
                _ => {
                    fs::write(&path, b"private").unwrap();
                    fs::set_permissions(&path, fs::Permissions::from_mode(0o0)).unwrap();
                }
            }
            let before = fs::symlink_metadata(&path).unwrap();
            let input = f.temp.path().join("read-worker.json");
            fs::write(
                &input,
                serde_json::to_vec(&json!({"mode":"read-initial","directory":directory})).unwrap(),
            )
            .unwrap();
            let mut child = TestChild(
                Command::new(std::env::current_exe().unwrap())
                    .args([
                        "--exact",
                        "task_env::instance::allocation::tests::allocation_crash_worker",
                        "--ignored",
                    ])
                    .env("FLOW_ALLOCATION_INPUT", input)
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn()
                    .unwrap(),
            );
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                if let Some(status) = child.0.try_wait().unwrap() {
                    assert!(status.success(), "{kind}");
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "bootstrap open blocked on {kind}"
                );
                std::thread::sleep(Duration::from_millis(5));
            }
            assert_eq!(
                super::RootIdentity::of(&before),
                super::RootIdentity::of(&fs::symlink_metadata(&path).unwrap())
            );
            assert_eq!(
                fs::read(f.temp.path().join("formal-delivery")).unwrap(),
                b"preserve"
            );
            if kind == "unreadable" {
                fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
                assert_eq!(fs::read(path).unwrap(), b"private");
            }
        }
    }

    #[test]
    fn allocation_document_scratch_process_interruptions_resume_or_finalize() {
        for point in [
            "checkpoint-half-written",
            "checkpoint-scratch-synced",
            "checkpoint-renamed",
        ] {
            for resume in [false, true] {
                let f = Fixture::new();
                f.interrupt(point);
                let owner = f.owner();
                if resume {
                    f.resume().unwrap();
                    assert_eq!(f.owner(), owner);
                }
                f.finish();
            }
        }
    }

    #[test]
    fn public_registry_entries_never_rebuild_missing_ready_control() {
        let f = Fixture::new();
        f.resume().unwrap();
        let control = f.queue.join("queue.json/task-environments");
        for suffix in [
            "",
            "registry.json",
            "registry.json/authority.json",
            "registry.json/envs",
            "registry.json/plans",
            "registry.json/locks",
            "registry.json/.flow-document-v2",
            "registry.json/envs/.flow-namespace-v2",
        ] {
            let path = if suffix.is_empty() {
                control.clone()
            } else {
                control.join(suffix)
            };
            let preserved = f.temp.path().join("preserved-control-component");
            fs::rename(&path, &preserved).unwrap();
            let authority = fs::read(f.queue.join("queue.json/authority.json")).unwrap();
            assert!(task_env::state_root().is_err(), "{suffix}");
            assert!(task_env::environment_state_path("task").is_err());
            assert!(task_env::cleanup_state_path("task").is_err());
            assert!(task_env::registry::try_lock_task("task").is_err());
            assert!(task_env::is_registered("task").is_err());
            assert!(task_env::restore_environment("task").is_err());
            assert!(!path.exists(), "recreated {suffix}");
            assert_eq!(
                fs::read(f.queue.join("queue.json/authority.json")).unwrap(),
                authority
            );
            fs::rename(preserved, path).unwrap();
        }
        f.finish();
    }

    #[test]
    fn target_created_after_slot_identity_never_gets_overwritten_or_adopted() {
        let f = Fixture::new();
        f.interrupt("before-transfer:allocation-workspace");
        let target = f.temp.path().join("workspace");
        private(&target);
        fs::write(target.join("foreign"), b"unchanged").unwrap();
        let before = f.state();
        assert!(f.resume().is_err());
        assert_eq!(f.state(), before);
        assert_eq!(fs::read(target.join("foreign")).unwrap(), b"unchanged");
        // The fixture removes only its own injected obstruction, then the original
        // allocation intent can finalize; product code never removes the obstruction.
        fs::remove_file(target.join("foreign")).unwrap();
        fs::remove_dir(target).unwrap();
        f.finish();
    }
}
