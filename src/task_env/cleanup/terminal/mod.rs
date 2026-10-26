//! Unified terminal cleanup for fresh isolated task instances. Each entry is a
//! bounded attempt under the owner's EX gate; the existing watch retries errors.
mod git;
mod process;
mod storage;
pub(in crate::task_env) use process::registered_identity;

use super::super::{instance::CreationIntent, ProcessRecord};
use super::{registry, CleanupState, PendingDirectory};
use crate::util::protected::DocumentRemoval;
use crate::{
    lifecycle::{Owner, Permit},
    model::J,
    util, FlowError, Result,
};
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Prepared,
    GitRemoving,
    GitRemoved,
    ResourcesRemoved,
    Releasing,
    Released,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TerminalState {
    pub owner: Owner,
    pub associations: J,
    pub phase: Phase,
    pub creation: CreationIntent,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_removal_identity: Option<crate::task_env::instance::GitIdentity>,
    pub environment: DocumentRemoval,
    pub environment_digest: String,
    pub registry: Option<registry::RegistryEntry>,
    pub targets: Vec<ProcessRecord>,
    #[serde(default)]
    pub upstream_sessions: Vec<J>,
}

fn require(ok: bool, message: &str) -> Result<()> {
    if ok {
        Ok(())
    } else {
        Err(FlowError::progress(message))
    }
}
fn terminal(plan: &CleanupState) -> Result<&TerminalState> {
    plan.terminal
        .as_ref()
        .ok_or_else(|| FlowError::guard("not a terminal cleanup plan"))
}
fn check(owner: &Owner, permit: &Permit) -> Result<()> {
    require(
        permit.exclusive() && permit.owner() == owner,
        "cleanup requires exact exclusive owner",
    )?;
    let q = crate::queue::load_queue(&owner.queue_root)?;
    permit.check(&q, true)?;
    require(
        q["tasks"][&owner.task_id]["cleanup_obligation"]["owner"] == serde_json::to_value(owner)?
            && matches!(
                q["tasks"][&owner.task_id]["final_outcome"].as_str(),
                Some("success" | "failure")
            ),
        "cleanup requires current terminal obligation",
    )
}
fn lock(owner: &Owner) -> Result<std::fs::File> {
    registry::try_lock_task(&owner.task_id)?
        .ok_or_else(|| FlowError::guard("environment busy; retry cleanup"))
}

/// Freeze associations once; callers must hold the queue lock as well as EX.
pub fn prepare(owner: &Owner, permit: &Permit, associations: J) -> Result<()> {
    check(owner, permit)?;
    if let Some(plan) = storage::load_optional(owner)? {
        require(
            terminal(&plan)?.associations == associations,
            "cleanup associations changed",
        )?;
        return Ok(());
    }
    let (env, creation) = prepared_environment(owner, permit)?;
    let (environment, entry, roots) = prepare_resources(&env, owner, &associations)?;
    let now = util::now();
    let plan = CleanupState {
        terminal: Some(TerminalState {
            owner: owner.clone(),
            associations,
            phase: Phase::Prepared,
            creation,
            git_removal_identity: None,
            environment,
            environment_digest: environment_digest(env.state_file())?,
            registry: entry,
            targets: vec![],
            upstream_sessions: crate::task_runtime::upstream::obligations(&env)?,
        }),
        task_id: owner.task_id.clone(),
        instance_id: owner.instance_id.clone(),
        delivery_confirmed: env.delivery["phase"] == "confirmed",
        pending_root_removals: roots.iter().map(|r| r.path.clone()).collect(),
        roots,
        pending_processes: env.child_processes.clone(),
        pending_worktrees: env.worktree_registrations.clone(),
        pending_files: env.open_files.clone(),
        quarantine: None,
        started_at: now,
        updated_at: now,
        attempts: 0,
        last_error: None,
    };
    save_prepared(owner, &plan)
}

pub fn association_plan(owner: &Owner, permit: &Permit) -> Result<J> {
    check(owner, permit)?;
    Ok(terminal(&storage::load(owner)?)?.associations.clone())
}

/// Read the immutable association snapshot of another pending owner. The caller
/// holds the shared queue transaction; no owner gate or environment lock is
/// taken, and no missing control path or plan is created.
pub fn frozen_association_plan(owner: &Owner) -> Result<Option<J>> {
    let queue = crate::queue::load_queue(&owner.queue_root)?;
    validate_frozen_pending(&queue, owner)?;
    storage::load_optional(owner)?
        .map(|plan| Ok(terminal(&plan)?.associations.clone()))
        .transpose()
}

fn validate_frozen_pending(queue: &J, owner: &Owner) -> Result<()> {
    require(
        queue["id"] == owner.queue_id
            && crate::lifecycle::task_owner(queue, &owner.task_id)?.as_ref() == Some(owner),
        "frozen association owner changed",
    )?;
    let task = &queue["tasks"][&owner.task_id];
    require(
        task["cleanup_obligation"]["owner"] == serde_json::to_value(owner)?
            && task["cleanup_obligation"]["status"] == "pending"
            && matches!(task["final_outcome"].as_str(), Some("success" | "failure")),
        "frozen associations require current pending owner",
    )
}

/// Queue lock is not held during process/Git/root work. EX remains held.
pub fn resume_terminal(owner: &Owner, permit: &Permit) -> Result<()> {
    check(owner, permit)?;
    let _lock = lock(owner)?;
    let mut plan = storage::load(owner)?;
    if matches!(
        terminal(&plan)?.phase,
        Phase::ResourcesRemoved | Phase::Releasing | Phase::Released
    ) {
        return storage::verify_absent(&plan);
    }
    storage::verify_environment(&plan)?;
    let result = resources(&mut plan);
    plan.updated_at = util::now();
    plan.attempts = plan.attempts.saturating_add(1);
    plan.last_error = result.as_ref().err().map(ToString::to_string);
    storage::save(&plan)?;
    result
}
fn resources(plan: &mut CleanupState) -> Result<()> {
    process::quiesce(plan)?;
    reclaim_upstream(plan)?;
    storage::verify_pending_roots(plan)?;
    remove_git_registration(plan)?;
    // Files and runtime journals are part of the whole owned roots, not a
    // filename/content classification. No outside target is touched.
    plan.pending_files.clear();
    storage::remove_roots(plan)?;
    storage::verify_absent(plan)?;
    plan.terminal.as_mut().unwrap().phase = Phase::ResourcesRemoved;
    storage::save(plan)
}

fn reclaim_upstream(plan: &mut CleanupState) -> Result<()> {
    let count = terminal(plan)?.upstream_sessions.len();
    for index in 0..count {
        let journal = &mut plan.terminal.as_mut().unwrap().upstream_sessions[index];
        if journal["phase"] == "created" {
            journal["phase"] = serde_json::json!("deleting");
            storage::save(plan)?;
        }
        crate::task_runtime::upstream::reclaim(
            &mut plan.terminal.as_mut().unwrap().upstream_sessions[index],
        )?;
        storage::save(plan)?;
    }
    Ok(())
}

/// Retain the plan until the queue has persisted its removal proof.
pub fn release_terminal(owner: &Owner, permit: &Permit) -> Result<()> {
    check(owner, permit)?;
    let _lock = lock(owner)?;
    let mut plan = storage::load(owner)?;
    require(
        matches!(
            terminal(&plan)?.phase,
            Phase::ResourcesRemoved | Phase::Releasing | Phase::Released
        ),
        "resources remain",
    )?;
    storage::verify_absent(&plan)?;
    if terminal(&plan)?.phase == Phase::Released {
        return Ok(());
    }
    checkpoint_releasing(owner, &mut plan)?;
    release_records(owner, &mut plan)
}

pub fn plan_removal_proof(owner: &Owner, permit: &Permit) -> Result<DocumentRemoval> {
    check(owner, permit)?;
    let plan = storage::load(owner)?;
    require(
        terminal(&plan)?.phase == Phase::Released,
        "plan not released",
    )?;
    util::protected::prepare_document_removal(&storage::plan_path(owner)?)?
        .ok_or_else(|| FlowError::guard("released plan missing before checkpoint"))
}

/// Caller holds queue lock. The released queue obligation survives a half-erased
/// plan and authorizes removal only at the persisted inode, never replacement.
pub fn forget_terminal(owner: &Owner, permit: &Permit) -> Result<()> {
    check(owner, permit)?;
    let q = crate::queue::load_queue(&owner.queue_root)?;
    let obligation = &q["tasks"][&owner.task_id]["cleanup_obligation"];
    require(
        obligation["status"] == "released",
        "queue release checkpoint missing",
    )?;
    let proof: DocumentRemoval = serde_json::from_value(obligation["plan_removal"].clone())?;
    require(
        proof.path == storage::plan_path(owner)?,
        "foreign cleanup plan proof",
    )?;
    util::protected::resume_document_removal(&proof)?;
    registry::retire_terminal_lock(owner)
}

/// Read-only scan, including released tails. No control root/registry creation.
pub fn pending_terminal_owners(queue_root: &Path) -> Result<Vec<Owner>> {
    storage::pending(queue_root)
}

fn prepare_resources(
    env: &crate::task_env::TaskEnvironment,
    owner: &Owner,
    associations: &J,
) -> Result<(
    DocumentRemoval,
    Option<registry::RegistryEntry>,
    Vec<PendingDirectory>,
)> {
    let creation = env
        .creation
        .as_ref()
        .ok_or_else(|| FlowError::guard("missing creation intent"))?;
    let environment = util::protected::prepare_document_removal(env.state_file())?
        .ok_or_else(|| FlowError::guard("missing environment authority"))?;
    let entry = registry::load_registry()?.get(&owner.task_id).cloned();
    storage::validate_entry(env, entry.as_ref())?;
    let roots: Vec<_> = env
        .roots()
        .into_iter()
        .map(|(path, identity)| PendingDirectory { path, identity })
        .collect();
    validate_resource_ownership(env, owner, associations, &roots, creation)?;
    Ok((environment, entry, roots))
}

fn validate_resource_ownership(
    env: &crate::task_env::TaskEnvironment,
    owner: &Owner,
    associations: &J,
    roots: &[PendingDirectory],
    creation: &CreationIntent,
) -> Result<()> {
    storage::validate_roots(roots, &env.delivery_targets)?;
    require(
        roots
            .iter()
            .all(|r| !util::overlap(&r.path, &owner.queue_root)),
        "owned root overlaps shared lifecycle queue",
    )?;
    storage::validate_receiver(env, associations, owner)?;
    validate_registered_resources(env, creation)?;
    Ok(())
}

fn validate_registered_resources(
    env: &crate::task_env::TaskEnvironment,
    creation: &CreationIntent,
) -> Result<()> {
    require(
        env.worktree_registrations
            .iter()
            .all(|w| w.path == creation.checkout && w.repo.as_ref() == Some(&creation.common_dir)),
        "unknown registered worktree ownership",
    )?;
    require(
        env.open_files.iter().all(|p| env.owns_path(p)),
        "registered file outside owned roots",
    )?;
    Ok(())
}

fn remove_git_registration(plan: &mut CleanupState) -> Result<()> {
    checkpoint_git_removing(plan)?;
    if terminal(plan)?.phase == Phase::GitRemoving {
        git::remove(plan)?;
        plan.pending_worktrees.clear();
        plan.terminal.as_mut().unwrap().phase = Phase::GitRemoved;
        storage::save(plan)?;
    }
    Ok(())
}

fn checkpoint_releasing(owner: &Owner, plan: &mut CleanupState) -> Result<()> {
    let q = crate::queue::load_queue(&owner.queue_root)?;
    require(
        q["tasks"][&owner.task_id]["cleanup_obligation"]["status"] == "associations_removed",
        "queue associations remain",
    )?;
    if terminal(plan)?.phase == Phase::ResourcesRemoved {
        storage::verify_environment(plan)?;
        plan.terminal.as_mut().unwrap().phase = Phase::Releasing;
        storage::save(plan)?;
    }
    Ok(())
}

fn release_records(owner: &Owner, plan: &mut CleanupState) -> Result<()> {
    registry::release_terminal_claim(owner, terminal(plan)?.registry.as_ref())?;
    util::protected::resume_document_removal(&terminal(plan)?.environment)?;
    plan.terminal.as_mut().unwrap().phase = Phase::Released;
    storage::save(plan)
}

fn prepared_environment(
    owner: &Owner,
    permit: &Permit,
) -> Result<(crate::task_env::TaskEnvironment, CreationIntent)> {
    let mut env = super::super::restore_for_terminal(owner, permit)?;
    crate::task_env::instance::allocation::settle_terminal(&mut env, owner, permit)?;
    let creation = creation_snapshot(&env)?;
    Ok((env, creation))
}

fn save_prepared(owner: &Owner, plan: &CleanupState) -> Result<()> {
    let dir = registry::plan_dir()?;
    require(
        storage::plan_path(owner)?.parent() == Some(dir.as_path()),
        "plan namespace changed",
    )?;
    storage::save(plan)
}

fn checkpoint_git_removing(plan: &mut CleanupState) -> Result<()> {
    if terminal(plan)?.phase == Phase::Prepared {
        git::prepare_removal(plan)?;
        plan.terminal.as_mut().unwrap().phase = Phase::GitRemoving;
        storage::save(plan)?;
    }
    Ok(())
}

fn environment_digest(path: &Path) -> Result<String> {
    Ok(util::pyjson::sha256_json_sorted(&util::read_json(path)?))
}

fn creation_snapshot(env: &crate::task_env::TaskEnvironment) -> Result<CreationIntent> {
    env.creation
        .clone()
        .ok_or_else(|| FlowError::guard("missing creation intent"))
}

#[cfg(test)]
mod tests {
    // No models or production resources: assertions precede fixture destruction.
    use crate::{
        guard,
        lifecycle::{self, Owner},
        model::J,
        queue, task_env, util,
    };
    use serde_json::json;
    use std::{
        fs,
        os::unix::{fs::PermissionsExt, process::CommandExt},
        path::{Path, PathBuf},
        process::Command,
    };

    const SESSION: &str = "11111111-1111-1111-1111-111111111111";

    struct Fixture {
        temp: tempfile::TempDir,
        queue: PathBuf,
        old: Vec<(&'static str, Option<std::ffi::OsString>)>,
        _root: crate::task_env::TestEnvRootGuard,
    }

    impl Fixture {
        fn new() -> Self {
            let temp = tempfile::Builder::new()
                .permissions(fs::Permissions::from_mode(0o700))
                .tempdir()
                .unwrap();
            let root_guard = crate::task_env::test_env_root(
                &temp.path().join("queue/queue.json/task-environments"),
            );
            let old = [
                "CODEX_THREAD_ID",
                task_env::MARKER_ENV,
                "HERDR_TASK_ID",
                "HERDR_TASK_QUEUE",
                "HERDR_TASK_EPOCH",
            ]
            .into_iter()
            .map(|key| (key, std::env::var_os(key)))
            .collect();
            std::env::set_var("CODEX_THREAD_ID", SESSION);
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
            let sha = git(&repo, &["rev-parse", "HEAD"]);
            let queue = temp.path().join("queue");
            queue::init_isolated(&queue, None, 30).unwrap();
            let receiver = temp.path().join("receiver");
            private(&receiver);
            util::protected::namespace(&receiver).unwrap();
            let guard = json!({"version":2,"task":"coordinator","role":"coordinator","pane":"w1:p1","terminal":"term1","socket":"/fixture","cwd":repo,"session":SESSION,"phase":"monitoring"});
            util::atomic_write_json(&receiver.join("state.json"), &guard).unwrap();
            let mut s = queue::load_queue(&queue).unwrap();
            s["guard_dir"] = json!(receiver);
            s["receiver"] = guard::scope(&guard);
            s["tasks"] = json!({"task":{"task":"task","epoch":1,"candidate":sha,"state_version":1,"phase":"waiting","control":"running","writer":null,"pending":[],"operations":{}}});
            queue::save(&queue, &mut s).unwrap();
            Self {
                temp,
                queue,
                old,
                _root: root_guard,
            }
        }
        fn state(&self) -> J {
            queue::load_queue(&self.queue).unwrap()
        }
        fn save(&self, s: &mut J) {
            let _lock = queue::locked(&self.queue, false).unwrap();
            queue::save(&self.queue, s).unwrap();
        }
        fn creation_spec(&self) -> J {
            let s = self.state();
            json!({"action":"create","task":"task","queue_id":s["id"],"epoch":1,"state_version":1,"environment":{"workspace_root":self.temp.path().join("workspace"),"temp_root":self.temp.path().join("scratch"),"delivery_targets":[],"repository":self.temp.path().join("repo"),"commit":s["tasks"]["task"]["candidate"]}})
        }
        fn create(&self) -> Owner {
            self.call("task-environment", &self.creation_spec())
                .unwrap();
            lifecycle::task_owner(&self.state(), "task")
                .unwrap()
                .unwrap()
        }
        fn call(&self, action: &str, spec: &J) -> crate::Result<J> {
            queue::finalization::task_action(&self.queue, action, spec, Some(SESSION), &|_| {
                panic!("no native or model calls")
            })
        }
        fn spec(&self, owner: &Owner, outcome: &str) -> J {
            json!({"task":"task","queue_id":owner.queue_id,"epoch":owner.epoch,"instance_id":owner.instance_id,"state_version":self.state()["tasks"]["task"]["state_version"],"outcome":outcome,"reason":"explicit final decision"})
        }
        fn record(&self, owner: &Owner) {
            let _lock = queue::locked(&self.queue, false).unwrap();
            queue::task_action(
                &self.queue,
                "task-finalize",
                &self.spec(owner, "failure"),
                Some(SESSION),
                &|_| panic!("no model calls"),
            )
            .unwrap();
        }
        fn freeze_before_resources(&self, owner: &Owner) -> J {
            let workspace = self.temp.path().join("workspace");
            let held = self.temp.path().join("held-workspace");
            fs::rename(&workspace, &held).unwrap();
            private(&workspace);
            let report = queue::finalization::drive(&self.queue).unwrap();
            assert_eq!(report.pending, 1, "{report:?}");
            let permit = lifecycle::acquire(owner, true).unwrap();
            let associations = task_env::terminal::association_plan(owner, &permit)
                .unwrap_or_else(|error| panic!("{report:?}: {error}"));
            fs::remove_dir(&workspace).unwrap();
            fs::rename(&held, &workspace).unwrap();
            associations
        }
        fn clean(&self, owner: &Owner) {
            assert!(!self.temp.path().join("workspace").exists());
            assert!(!self.temp.path().join("scratch").exists());
            assert!(self.state()["tasks"]["task"].is_null());
            assert!(!task_env::is_registered("task").unwrap());
            assert!(!task_env::environment_state_path("task").unwrap().exists());
            assert!(!task_env::cleanup_state_path("task").unwrap().exists());
            assert!(task_env::terminal::pending_terminal_owners(&self.queue)
                .unwrap()
                .is_empty());
            assert!(!git(
                &self.temp.path().join("repo"),
                &["worktree", "list", "--porcelain"]
            )
            .contains("workspace/checkout"));
            assert!(lifecycle::write_permit(owner).is_err());
            assert!(!queue::artifacts(&self.queue)
                .unwrap()
                .join("gates")
                .join(format!("{}.lock", owner.task_id))
                .exists());
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            for (key, value) in &self.old {
                match value {
                    Some(value) => std::env::set_var(key, value),
                    None => std::env::remove_var(key),
                }
            }
        }
    }
    fn private(path: &Path) {
        fs::create_dir(path).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
    }
    fn git(root: &Path, args: &[&str]) -> String {
        let mut command = Command::new("git");
        command.arg("-C").arg(root).args(args);
        // SAFETY: scalar child-only umask; no process-global mutation.
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
        String::from_utf8(output.stdout).unwrap().trim().to_string()
    }

    #[test]
    fn owned_process_and_markerless_child_exit_before_whole_root_cleanup() {
        use std::io::{BufRead, BufReader};
        use std::process::Stdio;
        let f = Fixture::new();
        let owner = f.create();
        let mut env = task_env::restore_environment("task").unwrap();
        let permit = lifecycle::write_permit(&owner).unwrap();
        let mut cmd = env
            .permitted_command(std::ffi::OsStr::new("sh"), false, Some(&permit))
            .unwrap();
        cmd.args(["-c","env -u HERDR_TASK_ENV_INSTANCE sh -c 'while :; do sleep 1; done' & printf '%s\\n' \"$!\"; wait"])
        .stdout(Stdio::piped()).stdin(Stdio::null());
        let mut child = cmd.spawn().unwrap();
        let mut line = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        let descendant = line.trim().parse::<i32>().unwrap();
        env.register_process(child.id() as i32).unwrap();
        drop(permit);
        drop(env);
        f.record(&owner);
        let report = queue::finalization::drive(&f.queue).unwrap();
        // Ensure failures cannot leave the test's worker running.
        if report.completed != 1 {
            // Fixture owns this freshly setsid process group; release test children
            // even when the product intentionally retains an unknown obligation.
            unsafe {
                libc::kill(-(child.id() as i32), libc::SIGKILL);
            }
            let _ = child.kill();
        }
        child.wait().unwrap();
        assert_eq!(report.completed, 1, "{report:?}");
        assert!(!task_env::process_record_is_alive(
            &task_env::ProcessRecord {
                pid: descendant,
                start_time: task_env::process_start_time(descendant)
            }
        ));
        f.clean(&owner);
    }

    #[test]
    fn plan_and_environment_half_erasure_resume_from_persisted_inode_proofs() {
        let f = Fixture::new();
        let owner = f.create();
        f.record(&owner);
        let permit = lifecycle::acquire(&owner, true).unwrap();
        super::prepare(&owner, &permit, json!({"frozen":true})).unwrap();
        assert!(super::prepare(&owner, &permit, json!({"frozen":false})).is_err());
        super::resume_terminal(&owner, &permit).unwrap();
        let mut plan = super::storage::load(&owner).unwrap();
        plan.terminal.as_mut().unwrap().phase = super::Phase::Releasing;
        super::storage::save(&plan).unwrap();
        let envproof = plan.terminal.as_ref().unwrap().environment.clone();
        // Simulate committed registry release and interrupted document removal.
        task_env::registry::release_terminal_claim(
            &owner,
            plan.terminal.as_ref().unwrap().registry.as_ref(),
        )
        .unwrap();
        fs::remove_file(envproof.path.join("authority.json")).unwrap();
        fs::remove_file(envproof.path.join(".flow-document-v2")).unwrap();
        // Only fixture transaction writes this stage; production uses frozen associations.
        let mut q = f.state();
        q["tasks"]["task"]["cleanup_obligation"]["status"] = json!("associations_removed");
        util::atomic_write_json(&f.queue.join("queue.json"), &q).unwrap();
        super::release_terminal(&owner, &permit).unwrap();
        assert_eq!(
            super::association_plan(&owner, &permit).unwrap(),
            json!({"frozen":true})
        );
        let proof = super::plan_removal_proof(&owner, &permit).unwrap();
        q["tasks"]["task"]["cleanup_obligation"]["status"] = json!("released");
        q["tasks"]["task"]["cleanup_obligation"]["plan_removal"] = json!(proof);
        q["tasks"]["task"]["cleanup_obligation"]["cleanup_tasks"] = json!({"task":1});
        util::atomic_write_json(&f.queue.join("queue.json"), &q).unwrap();
        fs::remove_file(proof.path.join("authority.json")).unwrap();
        fs::remove_file(proof.path.join(".flow-document-v2")).unwrap();
        assert!(super::pending_terminal_owners(&f.queue).unwrap().is_empty());
        drop(permit);
        let report = queue::finalization::drive(&f.queue).unwrap();
        assert_eq!(report.completed, 1, "{report:?}");
        f.clean(&owner);
    }

    #[test]
    fn locked_worktree_git_failure_retains_obligation_and_never_prunes_foreign_admin() {
        let f = Fixture::new();
        let owner = f.create();
        let repo = f.temp.path().join("repo");
        let checkout = f.temp.path().join("workspace/checkout");
        git(&repo, &["worktree", "lock", checkout.to_str().unwrap()]);
        let foreign = repo.join(".git/worktrees/foreign-sentinel");
        private(&foreign);
        fs::write(foreign.join("keep"), "keep").unwrap();
        f.record(&owner);
        let report = queue::finalization::drive(&f.queue).unwrap();
        assert_eq!(report.pending, 1, "{report:?}");
        assert!(checkout.exists());
        assert_eq!(fs::read_to_string(foreign.join("keep")).unwrap(), "keep");
        git(&repo, &["worktree", "unlock", checkout.to_str().unwrap()]);
        let report = queue::finalization::drive(&f.queue).unwrap();
        assert_eq!(report.completed, 1, "{report:?}");
        assert!(foreign.join("keep").exists());
        f.clean(&owner);
    }

    #[test]
    fn gate_retirement_crash_resumes_without_stale_snapshot_resurrection() {
        let f = Fixture::new();
        let owner = f.create();
        f.record(&owner);
        let permit = lifecycle::acquire(&owner, true).unwrap();
        super::prepare(&owner, &permit, json!({})).unwrap();
        super::resume_terminal(&owner, &permit).unwrap();
        let mut q = f.state();
        q["tasks"]["task"]["cleanup_obligation"]["status"] = json!("associations_removed");
        util::atomic_write_json(&f.queue.join("queue.json"), &q).unwrap();
        super::release_terminal(&owner, &permit).unwrap();
        q["tasks"]["task"]["cleanup_obligation"]["status"] = json!("released");
        q["tasks"]["task"]["cleanup_obligation"]["plan_removal"] =
            json!(super::plan_removal_proof(&owner, &permit).unwrap());
        q["tasks"]["task"]["cleanup_obligation"]["cleanup_tasks"] = json!({"task":1});
        util::atomic_write_json(&f.queue.join("queue.json"), &q).unwrap();
        super::forget_terminal(&owner, &permit).unwrap();
        lifecycle::retire(&owner, &permit).unwrap();
        assert!(permit.check(&q, true).is_err());
        drop(permit);
        assert!(lifecycle::acquire(&owner, true).is_err());
        let report = queue::finalization::drive(&f.queue).unwrap();
        assert_eq!(report.completed, 1, "{report:?}");
        f.clean(&owner);
        assert!(queue::save(&f.queue, &mut q).is_err());
        assert!(queue::finalization::drive(&f.queue).unwrap().pending == 0);
    }

    struct ReadyWorker(std::process::Child);
    impl ReadyWorker {
        fn new(marker: Option<&str>) -> Self {
            use std::io::Read;
            use std::os::fd::AsRawFd;
            use std::process::Stdio;
            let mut c = Command::new("sh");
            c.args(["-c", "printf 'ready\\n'; IFS= read -r stop"])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .process_group(0);
            if let Some(id) = marker {
                c.env(task_env::MARKER_ENV, id);
            }
            let mut worker = Self(c.spawn().unwrap());
            let out = worker.0.stdout.as_mut().unwrap();
            let mut poll = libc::pollfd {
                fd: out.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: a single live pollfd, fixed bounded timeout.
            assert_eq!(unsafe { libc::poll(&mut poll, 1, 5000) }, 1);
            let mut ready = [0; 6];
            out.read_exact(&mut ready).unwrap();
            assert_eq!(&ready, b"ready\n");
            worker
        }
        fn pid(&self) -> i32 {
            self.0.id() as i32
        }
    }
    impl Drop for ReadyWorker {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    #[test]
    fn unknown_new_pid_observation_blocks_roots_then_automatic_retry_finishes() {
        let f = Fixture::new();
        let owner = f.create();
        let mut worker = ReadyWorker::new(None);
        f.record(&owner);
        let before = f.state()["tasks"]["task"]["final_outcome"].clone();
        for environ in [true, false] {
            let _fault = super::process::faults::inject(worker.pid(), environ, libc::EACCES);
            let report = queue::finalization::drive(&f.queue).unwrap();
            assert_eq!(report.pending, 1, "{report:?}");
            assert!(f.temp.path().join("workspace/checkout/.git").exists());
            assert_eq!(f.state()["tasks"]["task"]["final_outcome"], before);
            assert!(worker.0.try_wait().unwrap().is_none());
        }
        let report = queue::finalization::drive(&f.queue).unwrap();
        assert_eq!(report.completed, 1, "{report:?}");
        // This unmarked, unregistered independent process was never signalled.
        assert!(worker.0.try_wait().unwrap().is_none());
        f.clean(&owner);
    }

    #[test]
    fn precreation_process_exclusion_never_applies_to_known_or_equal_tick_records() {
        let worker = ReadyWorker::new(None);
        // Ensure the unrelated worker is strictly older than the captured floor.
        std::thread::sleep(std::time::Duration::from_millis(30));
        let f = Fixture::new();
        let owner = f.create();
        f.record(&owner);
        let permit = lifecycle::acquire(&owner, true).unwrap();
        super::prepare(&owner, &permit, json!({})).unwrap();
        let mut plan = super::storage::load(&owner).unwrap();
        let original = plan
            .terminal
            .as_ref()
            .unwrap()
            .creation
            .process_boundary
            .clone();
        let record = task_env::ProcessRecord {
            pid: worker.pid(),
            start_time: task_env::process_start_time(worker.pid()),
        };
        let _fault = super::process::faults::inject(worker.pid(), true, libc::EACCES);
        // Strictly older and not known: positive exclusion before reading environ.
        super::process::ensure_gone(&plan).unwrap();
        plan.terminal
            .as_mut()
            .unwrap()
            .creation
            .process_boundary
            .as_mut()
            .unwrap()
            .start_ticks = record.start_time.unwrap();
        assert!(super::process::ensure_gone(&plan).is_err()); // equal is not excluded
        plan.terminal.as_mut().unwrap().creation.process_boundary = original;
        plan.pending_processes.push(task_env::RegisteredProcess {
            pid: i32::MAX,
            pgid: i32::MAX,
            start_time: Some(1),
            descendants: vec![record.clone()],
            direct_children: vec![],
        });
        assert!(super::process::ensure_gone(&plan).is_err()); // historical record is not invisible
        plan.pending_processes.clear();
        plan.terminal.as_mut().unwrap().targets.push(record);
        assert!(super::process::ensure_gone(&plan).is_err()); // frozen target is never excluded
    }

    #[test]
    fn boot_namespace_or_missing_birth_boundary_refuses_before_signal_and_deletion() {
        let f = Fixture::new();
        let owner = f.create();
        f.record(&owner);
        f.freeze_before_resources(&owner);
        let mut worker = ReadyWorker::new(Some(&owner.instance_id));
        let permit = lifecycle::acquire(&owner, true).unwrap();
        let original = super::storage::load(&owner).unwrap();
        for which in 0..5 {
            let mut plan = original.clone();
            let boundary = &mut plan.terminal.as_mut().unwrap().creation.process_boundary;
            match which {
                0 => *boundary = None,
                1 => boundary.as_mut().unwrap().boot_id = "other-boot".into(),
                2 => boundary.as_mut().unwrap().pid_namespace.ino += 1,
                3 => boundary.as_mut().unwrap().time_namespace.ino += 1,
                _ => boundary.as_mut().unwrap().start_ticks = u64::MAX,
            }
            super::storage::save(&plan).unwrap();
            assert!(super::resume_terminal(&owner, &permit).is_err());
            assert!(worker.0.try_wait().unwrap().is_none());
            let stat = fs::read_to_string(format!("/proc/{}/stat", worker.pid())).unwrap();
            assert_ne!(
                stat.rsplit_once(')').unwrap().1.split_whitespace().next(),
                Some("T")
            );
            assert!(f.temp.path().join("workspace/checkout/.git").exists());
        }
        super::storage::save(&original).unwrap();
        drop(permit);
        // The marked process is owned, despite not being explicitly registered.
        let report = queue::finalization::drive(&f.queue).unwrap();
        assert_eq!(report.completed, 1, "{report:?}");
        worker.0.wait().unwrap();
        f.clean(&owner);
    }

    #[test]
    fn git_partial_removal_continues_only_exact_surviving_components() {
        for missing in [
            "checkout",
            "admin",
            "gitfile",
            "gitdir",
            "commondir",
            "HEAD",
        ] {
            let f = Fixture::new();
            let owner = f.create();
            f.record(&owner);
            f.freeze_before_resources(&owner);
            let permit = lifecycle::acquire(&owner, true).unwrap();
            let mut plan = super::storage::load(&owner).unwrap();
            let creation = plan.terminal.as_ref().unwrap().creation.clone();
            plan.terminal.as_mut().unwrap().phase = super::Phase::GitRemoving;
            super::storage::save(&plan).unwrap();
            let foreign = creation.common_dir.join("worktrees/foreign-sentinel");
            private(&foreign);
            fs::write(foreign.join("keep"), b"foreign").unwrap();
            match missing {
                "checkout" => fs::remove_dir_all(&creation.checkout).unwrap(),
                "admin" => {
                    fs::remove_dir_all(&creation.git_identity.as_ref().unwrap().admin_dir).unwrap()
                }
                "gitfile" => fs::remove_file(creation.checkout.join(".git")).unwrap(),
                name => {
                    fs::remove_file(creation.git_identity.as_ref().unwrap().admin_dir.join(name))
                        .unwrap()
                }
            }
            drop(permit);
            let report = queue::finalization::drive(&f.queue).unwrap();
            assert_eq!(report.completed, 1, "{missing}: {report:?}");
            assert_eq!(fs::read(foreign.join("keep")).unwrap(), b"foreign");
            assert!(!creation.git_identity.as_ref().unwrap().admin_dir.exists());
            f.clean(&owner);
        }
    }

    #[test]
    fn administrative_quarantine_crash_keeps_exact_identity_and_recovers() {
        let f = Fixture::new();
        let owner = f.create();
        f.record(&owner);
        f.freeze_before_resources(&owner);
        let permit = lifecycle::acquire(&owner, true).unwrap();
        let mut plan = super::storage::load(&owner).unwrap();
        let admin = plan
            .terminal
            .as_ref()
            .unwrap()
            .creation
            .git_identity
            .as_ref()
            .unwrap()
            .admin_dir
            .clone();
        fs::remove_file(
            plan.terminal
                .as_ref()
                .unwrap()
                .creation
                .checkout
                .join(".git"),
        )
        .unwrap();
        let quarantine = task_env::Quarantine {
            root: admin.clone(),
            name: ".herdr-rm-fixture".into(),
            unlink_committed: false,
        };
        plan.quarantine = Some(quarantine.clone());
        plan.terminal.as_mut().unwrap().phase = super::Phase::GitRemoving;
        super::storage::save(&plan).unwrap();
        fs::rename(&admin, admin.parent().unwrap().join(&quarantine.name)).unwrap();
        drop(permit);
        let report = queue::finalization::drive(&f.queue).unwrap();
        assert_eq!(report.completed, 1, "{report:?}");
        f.clean(&owner);
    }

    #[test]
    fn partial_admin_wrong_surviving_identity_is_never_removal_authority() {
        for change in ["gitdir", "commondir", "inode"] {
            let f = Fixture::new();
            let owner = f.create();
            f.record(&owner);
            f.freeze_before_resources(&owner);
            let permit = lifecycle::acquire(&owner, true).unwrap();
            let mut plan = super::storage::load(&owner).unwrap();
            let creation = plan.terminal.as_ref().unwrap().creation.clone();
            let admin = &creation.git_identity.as_ref().unwrap().admin_dir;
            plan.terminal.as_mut().unwrap().phase = super::Phase::GitRemoving;
            super::storage::save(&plan).unwrap();
            fs::remove_file(admin.join("HEAD")).unwrap();
            let held = admin.with_file_name("held-admin");
            if change == "inode" {
                fs::rename(admin, &held).unwrap();
                private(admin);
            } else {
                fs::write(admin.join(change), b"/foreign/path\n").unwrap();
            }
            drop(permit);
            let report = queue::finalization::drive(&f.queue).unwrap();
            assert_eq!(report.pending, 1, "{change}: {report:?}");
            assert!(admin.exists());
            assert!(creation.checkout.exists());
            assert!(f.temp.path().join("scratch").exists());
            assert_eq!(f.state()["tasks"]["task"]["final_outcome"], "failure");
        }
    }

    #[test]
    #[ignore = "private cleanup-supervisor subprocess fixture"]
    fn removal_supervisor_worker() {
        let input = PathBuf::from(std::env::var_os("REMOVAL_FIXTURE_INPUT").unwrap());
        let (intent, owner): (task_env::instance::CreationIntent, Owner) =
            serde_json::from_slice(&fs::read(input).unwrap()).unwrap();
        task_env::instance::git::remove_registered(&intent, &owner).unwrap();
    }

    #[test]
    fn native_removal_child_carries_owner_and_dies_with_supervisor() {
        let f = Fixture::new();
        let owner = f.create();
        f.record(&owner);
        f.freeze_before_resources(&owner);
        let permit = lifecycle::acquire(&owner, true).unwrap();
        let mut plan = super::storage::load(&owner).unwrap();
        plan.terminal.as_mut().unwrap().phase = super::Phase::GitRemoving;
        super::storage::save(&plan).unwrap();
        let input = f.temp.path().join("remove-input.json");
        fs::write(
            &input,
            serde_json::to_vec(&(plan.terminal.as_ref().unwrap().creation.clone(), &owner))
                .unwrap(),
        )
        .unwrap();
        let bin = f.temp.path().join("bin");
        private(&bin);
        // This replacement runs only in the fixture supervisor. Config is read-only;
        // the exact mutation argv waits as the same PID so the parent can be killed.
        fs::write(
            bin.join("git"),
            br#"#!/bin/sh
if [ "$3" = config ]; then exit 0; fi
[ "$3" = worktree ] && [ "$4" = remove ] || exit 91
printf '%s\n' "$$" > "$REMOVAL_FIXTURE_PID"
read -r ignored < "$REMOVAL_FIXTURE_FIFO"
"#,
        )
        .unwrap();
        fs::set_permissions(bin.join("git"), fs::Permissions::from_mode(0o700)).unwrap();
        let fifo = f.temp.path().join("remove-fifo");
        let name = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
        // SAFETY: fixture-owned path, no writer; shell blocks in its own PID.
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        let pidfile = f.temp.path().join("remove-pid");
        let mut supervisor = ReadyWorker(
            Command::new(std::env::current_exe().unwrap())
                .args([
                    "--ignored",
                    "--exact",
                    "task_env::cleanup::terminal::tests::removal_supervisor_worker",
                ])
                .env("PATH", &bin)
                .env("REMOVAL_FIXTURE_INPUT", &input)
                .env("REMOVAL_FIXTURE_PID", &pidfile)
                .env("REMOVAL_FIXTURE_FIFO", &fifo)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .unwrap(),
        );
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let pid = loop {
            if let Ok(data) = fs::read_to_string(&pidfile) {
                if let Ok(pid) = data.trim().parse::<i32>() {
                    break pid;
                }
            }
            assert!(
                supervisor.0.try_wait().unwrap().is_none(),
                "supervisor exited before remove"
            );
            assert!(std::time::Instant::now() < deadline, "remove did not start");
            std::thread::sleep(std::time::Duration::from_millis(5));
        };
        let record = task_env::ProcessRecord {
            pid,
            start_time: task_env::process_start_time(pid),
        };
        assert!(record.start_time.is_some());
        assert!(task_env::process_has_marker(pid, &owner.instance_id));
        let environ = fs::read(format!("/proc/{pid}/environ")).unwrap();
        for (key, value) in [
            ("HERDR_TASK_QUEUE", owner.queue_id.clone()),
            ("HERDR_TASK_ID", owner.task_id.clone()),
            ("HERDR_TASK_EPOCH", owner.epoch.to_string()),
        ] {
            let expected = format!("{key}={value}");
            assert!(
                environ
                    .split(|b| *b == 0)
                    .any(|row| row == expected.as_bytes()),
                "owner marker absent: {key}"
            );
        }
        supervisor.0.kill().unwrap();
        supervisor.0.wait().unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while task_env::process_record_is_alive(&record) && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(
            !task_env::process_record_is_alive(&record),
            "owned remove survived supervisor"
        );
        assert!(plan.terminal.as_ref().unwrap().creation.checkout.exists());
        drop(permit);
        assert_eq!(queue::finalization::drive(&f.queue).unwrap().completed, 1);
        f.clean(&owner);
    }

    #[test]
    fn prepared_associations_are_immutable_and_missing_quarantine_identity_is_rejected() {
        let f = Fixture::new();
        let owner = f.create();
        f.record(&owner);
        let associations = f.freeze_before_resources(&owner);
        let permit = lifecycle::acquire(&owner, true).unwrap();
        let _queue = queue::locked(&f.queue, false).unwrap();
        let path = super::storage::plan_path(&owner).unwrap();
        let before = util::read_json(&path).unwrap();
        super::prepare(&owner, &permit, associations.clone()).unwrap();
        assert!(super::prepare(&owner, &permit, json!({"changed":true})).is_err());
        assert_eq!(util::read_json(&path).unwrap(), before);
        assert_eq!(
            super::association_plan(&owner, &permit).unwrap(),
            associations
        );
        let mut plan = super::storage::load(&owner).unwrap();
        let root = super::PendingDirectory {
            path: f.temp.path().join("workspace"),
            identity: None,
        };
        let error = super::super::quarantine_before_clear(&mut plan, &root).unwrap_err();
        assert!(error.to_string().contains("quarantine identity absent"));
        assert_eq!(util::read_json(&path).unwrap(), before);
        assert!(root.path.join("checkout/.git").is_file());
        drop(_queue);
        drop(permit);
        assert_eq!(queue::finalization::drive(&f.queue).unwrap().completed, 1);
        f.clean(&owner);
    }

    #[test]
    fn failed_creation_before_git_removes_only_recorded_empty_intent() {
        let f = Fixture::new();
        let mut state = f.state();
        let binding = task_env::instance::InstanceBinding {
            queue_id: state["id"].as_str().unwrap().into(),
            task_id: "task".into(),
            epoch: 1,
        };
        let create = serde_json::from_value(json!({
            "workspace_root": f.temp.path().join("workspace"),
            "temp_root": f.temp.path().join("scratch"),
            "delivery_targets": [], "repository": f.temp.path().join("repo"),
            "commit": state["tasks"]["task"]["candidate"]
        }))
        .unwrap();
        let prospective = Owner {
            queue_root: f.queue.clone(),
            queue_id: binding.queue_id.clone(),
            task_id: "task".into(),
            epoch: 1,
            instance_id: String::new(),
        };
        let _queue = queue::locked(&f.queue, false).unwrap();
        let request = task_env::instance::allocation::control_request(&f.queue, &create).unwrap();
        state["lifecycle"]["control_bootstrap"] = json!({"reservation":request,"ready":null});
        // This fixture performs the same narrow bootstrap transaction as queue/control.
        state["authority_version"] = json!(state["authority_version"].as_u64().unwrap() + 1);
        util::atomic_write_json(&f.queue.join("queue.json"), &state).unwrap();
        let ready = task_env::instance::allocation::initialize_control(&request).unwrap();
        state["lifecycle"]["control_bootstrap"]["ready"] = ready;
        state["authority_version"] = json!(state["authority_version"].as_u64().unwrap() + 1);
        util::atomic_write_json(&f.queue.join("queue.json"), &state).unwrap();
        let permit = std::sync::Arc::new(lifecycle::acquire(&prospective, true).unwrap());
        let mut env =
            task_env::instance::prepare_isolated(&binding, &create, &f.queue, permit).unwrap();
        let owner = task_env::isolation::owner(&env).unwrap();
        crate::progress::environment::checkpoint(
            &mut state,
            &binding,
            &env,
            "creating",
            util::now(),
        )
        .unwrap();
        queue::save(&f.queue, &mut state).unwrap();
        task_env::instance::initialize_prepared(&mut env).unwrap();
        crate::progress::environment::checkpoint(
            &mut state,
            &binding,
            &env,
            "creating",
            util::now(),
        )
        .unwrap();
        queue::save(&f.queue, &mut state).unwrap();
        drop(_queue);
        let intent = env.creation.as_ref().unwrap().clone();
        // No checkout or Git registration was ever created. Verify the optional
        // pre-add snapshot branch and refuse an unexpected Git pointer as well.
        let mut snapshotted = intent.clone();
        snapshotted.git_admin_before = Some(Default::default());
        task_env::instance::git::verify_removal(&snapshotted).unwrap();
        private(&env.workspace_root);
        private(&intent.checkout);
        fs::write(intent.checkout.join(".git"), "foreign pointer").unwrap();
        assert!(task_env::instance::git::verify_removal(&snapshotted).is_err());
        assert_eq!(
            fs::read_to_string(intent.checkout.join(".git")).unwrap(),
            "foreign pointer"
        );
        // These are fixture-created, deliberately unowned objects; remove them
        // explicitly before asking the product to retire the still-empty intent.
        fs::remove_file(intent.checkout.join(".git")).unwrap();
        fs::remove_dir(&intent.checkout).unwrap();
        fs::remove_dir(&env.workspace_root).unwrap();
        drop(env);
        f.record(&owner);
        let report = queue::finalization::drive(&f.queue).unwrap();
        assert_eq!(report.completed, 1, "{report:?}");
        f.clean(&owner);
    }

    #[test]
    #[ignore = "private lifecycle crash-supervisor subprocess fixture"]
    fn lifecycle_recovery_worker() {
        let input = PathBuf::from(std::env::var_os("RECOVERY_FIXTURE_INPUT").unwrap());
        let data: J = serde_json::from_slice(&fs::read(input).unwrap()).unwrap();
        let root = Path::new(data["queue"].as_str().unwrap());
        configure_stop_checkpoint(&data);
        if data["action"] == "remove" {
            let report = queue::finalization::drive(root).unwrap();
            assert_eq!(report.completed, 1, "{report:?}");
            return;
        }
        queue::finalization::task_action(
            root,
            "task-environment",
            &data["spec"],
            Some(SESSION),
            &|_| panic!("fixture must not invoke a native agent"),
        )
        .unwrap();
    }

    fn configure_stop_checkpoint(data: &J) {
        if let Some(path) = data["stop_ready"].as_str() {
            super::process::faults::pause_after_freeze(PathBuf::from(path));
        }
    }

    fn mutation_reply_loss(f: &Fixture, action: &str) -> Owner {
        let input = f.temp.path().join(format!("{action}-input.json"));
        fs::write(
            &input,
            serde_json::to_vec(&json!({"queue":f.queue,"spec":f.creation_spec(),"action":action}))
                .unwrap(),
        )
        .unwrap();
        let bin = f.temp.path().join(format!("{action}-bin"));
        private(&bin);
        // Real Git completes the mutation. The same PID withholds its reply;
        // no exec follows the ready marker. Only this subprocess changes PATH.
        fs::write(
            bin.join("git"),
            br#"#!/bin/sh
/usr/bin/git "$@"
status=$?
[ "$status" = 0 ] || exit "$status"
if [ "$3" = worktree ] && [ "$4" = "$RECOVERY_FIXTURE_ACTION" ]; then
    printf '%s\n' "$$" > "$RECOVERY_FIXTURE_PID"
    read -r ignored < "$RECOVERY_FIXTURE_FIFO"
fi
"#,
        )
        .unwrap();
        fs::set_permissions(bin.join("git"), fs::Permissions::from_mode(0o700)).unwrap();
        let fifo = f.temp.path().join(format!("{action}-fifo"));
        let c = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
        // SAFETY: private fixture path; no production filesystem or process.
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
        let pidfile = f.temp.path().join(format!("{action}-child-pid"));
        let log = fs::File::create(f.temp.path().join(format!("{action}-worker.log"))).unwrap();
        let mut worker = ReadyWorker(
            Command::new(std::env::current_exe().unwrap())
                .args([
                    "--ignored",
                    "--exact",
                    "task_env::cleanup::terminal::tests::lifecycle_recovery_worker",
                ])
                .env("PATH", &bin)
                .env("RECOVERY_FIXTURE_INPUT", &input)
                .env("RECOVERY_FIXTURE_ACTION", action)
                .env("RECOVERY_FIXTURE_PID", &pidfile)
                .env("RECOVERY_FIXTURE_FIFO", &fifo)
                .stdout(log.try_clone().unwrap())
                .stderr(log)
                .spawn()
                .unwrap(),
        );
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let pid = loop {
            if let Ok(data) = fs::read_to_string(&pidfile) {
                if let Ok(pid) = data.trim().parse::<i32>() {
                    break pid;
                }
            }
            assert!(
                worker.0.try_wait().unwrap().is_none(),
                "creation worker exited: {}",
                fs::read_to_string(f.temp.path().join(format!("{action}-worker.log"))).unwrap()
            );
            assert!(
                std::time::Instant::now() < deadline,
                "real Git mutation did not complete"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        };
        let state = f.state();
        let owner = lifecycle::task_owner(&state, "task").unwrap().unwrap();
        assert_eq!(state["tasks"]["task"]["environment"]["phase"], "creating");
        let record = task_env::ProcessRecord {
            pid,
            start_time: task_env::process_start_time(pid),
        };
        assert!(record.start_time.is_some());
        assert!(task_env::process_has_marker(pid, &owner.instance_id));
        let doc = util::read_json(Path::new(
            state["tasks"]["task"]["environment"]["state_file"]
                .as_str()
                .unwrap(),
        ))
        .unwrap();
        assert_eq!(doc["creation"]["phase"], "git_pending");
        assert!(doc["creation"]["git_identity"].is_null());
        assert_eq!(
            f.temp.path().join("workspace/checkout/.git").is_file(),
            action == "add"
        );
        worker.0.kill().unwrap();
        worker.0.wait().unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while task_env::process_record_is_alive(&record) && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(
            !task_env::process_record_is_alive(&record),
            "Git reply wrapper survived supervisor"
        );
        owner
    }

    #[test]
    fn creation_reply_loss_then_final_failure_uses_same_instance_cleanup() {
        let f = Fixture::new();
        let owner = mutation_reply_loss(&f, "add");
        f.record(&owner);
        let before = f.state();
        let spec = json!({"action":"resume","task":"task","queue_id":owner.queue_id,
            "epoch":owner.epoch,"instance_id":owner.instance_id,
            "state_version":before["tasks"]["task"]["state_version"]});
        assert!(f.call("task-environment", &spec).is_err());
        assert_eq!(f.state(), before);
        let first = queue::finalization::drive(&f.queue).unwrap();
        let second = queue::finalization::drive(&f.queue).unwrap();
        assert_eq!(
            first.completed + second.completed,
            1,
            "first={first:?}; retry={second:?}"
        );
        f.clean(&owner);
    }

    #[test]
    fn pending_git_removal_rejects_foreign_or_incomplete_identity() {
        let f = Fixture::new();
        let owner = mutation_reply_loss(&f, "add");
        f.record(&owner);
        f.freeze_before_resources(&owner);
        let plan = super::storage::load(&owner).unwrap();
        let original = super::terminal(&plan).unwrap().creation.clone();
        let proof = task_env::instance::git::pending_removal_identity(&original)
            .unwrap()
            .unwrap();
        let before = f.state();
        let mut invalid = plan.clone();
        invalid
            .terminal
            .as_mut()
            .unwrap()
            .creation
            .git_admin_before
            .as_mut()
            .unwrap()
            .insert(
                proof
                    .admin_dir
                    .file_name()
                    .unwrap()
                    .to_str()
                    .unwrap()
                    .to_owned(),
                proof.admin_identity,
            );
        assert!(super::git::prepare_removal(&mut invalid)
            .unwrap_err()
            .to_string()
            .contains("existed before"));
        let mut invalid = plan.clone();
        invalid.terminal.as_mut().unwrap().creation.git_admin_before = None;
        assert!(super::git::prepare_removal(&mut invalid).is_err());
        let mut invalid = plan.clone();
        invalid.roots[0].identity = None;
        assert!(super::git::prepare_removal(&mut invalid)
            .unwrap_err()
            .to_string()
            .contains("recorded root"));
        for (name, foreign) in [
            ("HEAD", "0000000000000000000000000000000000000000\n"),
            ("gitdir", "/foreign/checkout/.git\n"),
            ("commondir", "/foreign/common\n"),
        ] {
            let path = proof.admin_dir.join(name);
            let bytes = fs::read(&path).unwrap();
            fs::write(&path, foreign).unwrap();
            let report = queue::finalization::drive(&f.queue).unwrap();
            assert_eq!(report.pending, 1, "{name}: {report:?}");
            assert!(original.checkout.exists());
            assert_eq!(fs::read_to_string(&path).unwrap(), foreign);
            assert_eq!(f.state(), before);
            fs::write(path, bytes).unwrap();
        }
        let held = f.temp.path().join("retained-checkout");
        fs::rename(&original.checkout, &held).unwrap();
        private(&original.checkout);
        let report = queue::finalization::drive(&f.queue).unwrap();
        assert_eq!(report.pending, 1, "{report:?}");
        assert!(held.join(".git").exists());
        assert_eq!(f.state(), before);
        fs::remove_dir(&original.checkout).unwrap();
        fs::rename(held, &original.checkout).unwrap();
        assert!(super::terminal(&super::storage::load(&owner).unwrap())
            .unwrap()
            .git_removal_identity
            .is_none());
        let report = queue::finalization::drive(&f.queue).unwrap();
        assert_eq!(report.completed, 1, "{report:?}");
        f.clean(&owner);
    }

    #[test]
    fn real_add_and_remove_reply_losses_resume_frozen_removal_proof() {
        let f = Fixture::new();
        let owner = mutation_reply_loss(&f, "add");
        let state_file = PathBuf::from(
            f.state()["tasks"]["task"]["environment"]["state_file"]
                .as_str()
                .unwrap(),
        );
        let original_env = util::read_json(&state_file).unwrap();
        f.record(&owner);
        assert_eq!(mutation_reply_loss(&f, "remove"), owner);
        let plan = super::storage::load(&owner).unwrap();
        let terminal = super::terminal(&plan).unwrap();
        assert_eq!(terminal.phase, super::Phase::GitRemoving);
        assert!(terminal.creation.git_identity.is_none());
        let proof = terminal.git_removal_identity.as_ref().unwrap();
        assert!(!proof.admin_dir.exists());
        assert!(!f.temp.path().join("workspace/checkout").exists());
        assert_eq!(util::read_json(&state_file).unwrap(), original_env);
        // The saved proof must also drive final absence checks, even with the
        // original creation identity still None. No add/populate is permitted.
        assert_eq!(
            super::git::removal_intent(&plan)
                .unwrap()
                .git_identity
                .as_ref(),
            Some(proof)
        );
        let report = queue::finalization::drive(&f.queue).unwrap();
        assert_eq!(report.completed, 1, "{report:?}");
        assert_eq!(queue::finalization::drive(&f.queue).unwrap().completed, 0);
        f.clean(&owner);
    }

    #[test]
    fn supervisor_crash_after_persisted_stop_resumes_kill_and_root_removal() {
        let f = Fixture::new();
        let owner = f.create();
        let mut child = ReadyWorker::new(Some(&owner.instance_id));
        let mut env = task_env::restore_environment("task").unwrap();
        env.register_process(child.pid()).unwrap();
        drop(env);
        f.record(&owner);
        {
            let _fault = super::process::faults::inject(child.pid(), true, libc::EACCES);
            let report = queue::finalization::drive(&f.queue).unwrap();
            assert_eq!(report.pending, 1, "{report:?}");
            assert!(child.0.try_wait().unwrap().is_none());
            assert!(f.temp.path().join("workspace/checkout/.git").is_file());
            assert_eq!(f.state()["tasks"]["task"]["final_outcome"], "failure");
        }
        let input = f.temp.path().join("stop-input.json");
        let ready = f.temp.path().join("stop-ready");
        fs::write(
            &input,
            serde_json::to_vec(&json!({"queue":f.queue,"action":"remove","stop_ready":ready}))
                .unwrap(),
        )
        .unwrap();
        let log = fs::File::create(f.temp.path().join("stop-worker.log")).unwrap();
        let mut supervisor = ReadyWorker(
            Command::new(std::env::current_exe().unwrap())
                .args([
                    "--ignored",
                    "--exact",
                    "task_env::cleanup::terminal::tests::lifecycle_recovery_worker",
                ])
                .env("RECOVERY_FIXTURE_INPUT", &input)
                .stdout(log.try_clone().unwrap())
                .stderr(log)
                .spawn()
                .unwrap(),
        );
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !ready.exists() {
            assert!(
                supervisor.0.try_wait().unwrap().is_none(),
                "supervisor exited: {}",
                fs::read_to_string(f.temp.path().join("stop-worker.log")).unwrap()
            );
            assert!(
                std::time::Instant::now() < deadline,
                "stop checkpoint not reached"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        supervisor.0.kill().unwrap();
        supervisor.0.wait().unwrap();
        let plan = super::storage::load(&owner).unwrap();
        assert!(super::terminal(&plan)
            .unwrap()
            .targets
            .iter()
            .any(|r| r.pid == child.pid()
                && r.start_time == task_env::process_start_time(child.pid())));
        assert_eq!(
            super::terminal(&plan).unwrap().phase,
            super::Phase::Prepared
        );
        let stat = fs::read_to_string(format!("/proc/{}/stat", child.pid())).unwrap();
        assert_eq!(
            stat.rsplit_once(')').unwrap().1.split_whitespace().next(),
            Some("T")
        );
        assert!(child.0.try_wait().unwrap().is_none());
        assert!(f.temp.path().join("workspace/checkout/.git").is_file());
        assert_eq!(f.state()["tasks"]["task"]["final_outcome"], "failure");
        let report = queue::finalization::drive(&f.queue).unwrap();
        assert_eq!(report.completed, 1, "{report:?}");
        child.0.wait().unwrap();
        assert!(!task_env::process_record_is_alive(
            &task_env::ProcessRecord {
                pid: child.pid(),
                start_time: plan
                    .terminal
                    .as_ref()
                    .unwrap()
                    .targets
                    .iter()
                    .find(|r| r.pid == child.pid())
                    .unwrap()
                    .start_time
            }
        ));
        f.clean(&owner);
    }

    fn lineage_step(f: &Fixture, mut spec: J) {
        let mut s = f.state();
        spec["task"] = json!("task");
        spec["state_version"] = s["tasks"]["task"]["state_version"].clone();
        crate::progress::lineage::action(&mut s, &spec, 1.0).unwrap();
        f.save(&mut s);
    }

    fn delivery_instance(f: &Fixture, target: &Path) -> Owner {
        let mut s = f.state();
        let sha = s["tasks"]["task"]["candidate"].clone();
        s["tasks"] = json!({});
        crate::progress::lineage::action(&mut s, &json!({"kind":"plan","task":"task","goal":"deliver A","basis":{"original":["fixture request"],"changes":[],"specifications":[],"endpoint":"local file","confirmation":"fixture owner"},"authorization":[{"id":"publish","action":"delivery","source":"explicit fixture authorization"}]}), 1.0).unwrap();
        f.save(&mut s);
        lineage_step(
            f,
            json!({"kind":"artifact","artifact":{"adapter":"git","id":sha,"location":f.temp.path().join("repo")}}),
        );
        lineage_step(
            f,
            json!({"kind":"candidate","candidate":sha,"reason":"initial Git source"}),
        );
        let mut spec = f.creation_spec();
        spec["state_version"] = f.state()["tasks"]["task"]["state_version"].clone();
        spec["environment"]["delivery_targets"] = json!([{"path":target,"target_type":"formal"}]);
        f.call("task-environment", &spec).unwrap();
        let owner = lifecycle::task_owner(&f.state(), "task").unwrap().unwrap();
        let mut s = f.state();
        let shared = PathBuf::from(s["guard_dir"].as_str().unwrap());
        let root = f.temp.path().join("scratch/receiver");
        private(&root);
        util::protected::runtime_namespace(&root).unwrap();
        util::atomic_write_json(&root.join("lifecycle-owner.json"), &json!(owner)).unwrap();
        let mut receiver = util::read_state_json(&shared, "state.json").unwrap();
        receiver["task"] = json!("task");
        receiver["cwd"] = json!(f.temp.path().join("workspace/checkout"));
        util::atomic_write_json(&root.join("state.json"), &receiver).unwrap();
        s["guard_dir"] = json!(root);
        s["receiver"] = guard::scope(&receiver);
        f.save(&mut s);
        owner
    }

    fn accept_file_fixture(f: &Fixture) {
        use crate::progress::{self, acceptance};
        let source = f.temp.path().join("workspace/checkout/formal.txt");
        fs::write(&source, b"formal deliverable").unwrap();
        let artifact = json!({"adapter":"file-sha256","id":util::pyjson::sha256_hex(b"formal deliverable"),"location":source});
        lineage_step(f, json!({"kind":"artifact","artifact":artifact}));
        lineage_step(
            f,
            json!({"kind":"candidate","candidate":artifact["id"],"reason":"fixture final output"}),
        );
        lineage_step(
            f,
            json!({"kind":"activity","activity":"review","role":"acceptance","input":artifact["id"],"session_epoch":1,"result_wait":{"id":"review","epoch":1,"assignment":"review","owner":"reviewer","kind":"signal","timeout_seconds":30,"reason":"review result","condition":"done","producer":{"adapter":"opencode","session":"independent"}}}),
        );
        let mut s = f.state();
        let verdict = acceptance::submit(&mut s,"review",&json!({"adapter":"opencode","session":"independent"}),&json!({"conclusion":"passed","scope":"A","checks":[{"id":"content","criterion":"goal","source":"fixture request","status":"passed","evidence":"protocol fixture only"}],"gaps":[],"reuses":[],"resolutions":[]})).unwrap();
        let mut event = json!({"id":"result","task":"review","wait":"review","acknowledged":true,"outcome":"complete"});
        let wait = s["waits"]["review"].clone();
        event["epoch"] = wait["epoch"].clone();
        progress::attach(&mut s, &mut event, &wait, 2.0).unwrap();
        s["events"]["result"] = event;
        let spec = json!({"task":"review","state_version":s["tasks"]["review"]["state_version"],"event":"result","verdict":verdict["id"],"next_action":{"kind":"delivery","owner":"customer","reason":"passed","condition":"correct endpoint"}});
        progress::apply(&mut s, &spec, 3.0).unwrap();
        f.save(&mut s);
    }

    struct LostDeliveryReply {
        executions: std::cell::Cell<usize>,
    }
    impl crate::delivery::Executor for LostDeliveryReply {
        fn preflight(&self, intent: &J) -> crate::Result<()> {
            crate::delivery::local::Local.preflight(intent)
        }
        fn execute(&self, intent: &J) -> crate::Result<()> {
            self.executions.set(self.executions.get() + 1);
            crate::delivery::local::Local.execute(intent)?;
            Err(crate::FlowError::progress("fixture lost execute reply"))
        }
        fn observe(&self, _: &J) -> crate::Result<crate::delivery::Observation> {
            Ok(crate::delivery::Observation::Unknown(
                "fixture temporary observation outage".into(),
            ))
        }
    }

    #[test]
    fn confirmed_local_delivery_after_lost_reply_survives_terminal_recovery_without_reexecution() {
        use crate::delivery;
        let f = Fixture::new();
        let target = f.temp.path().join("customer.txt");
        let owner = delivery_instance(&f, &target);
        accept_file_fixture(&f);
        let executor = LostDeliveryReply {
            executions: std::cell::Cell::new(0),
        };
        let mut env = task_env::restore_environment("task").unwrap();
        let spec = json!({"adapter":"file","target":target,"authorization":"publish","state_version":f.state()["tasks"]["task"]["state_version"]});
        delivery::prepare(&mut env, &f.queue, &spec, &executor).unwrap();
        assert_eq!(
            delivery::execute(&mut env, &executor).unwrap()["phase"],
            "unknown"
        );
        assert!(delivery::execute(&mut env, &executor).is_err());
        assert_eq!(executor.executions.get(), 1);
        assert_eq!(fs::read(&target).unwrap(), b"formal deliverable");
        drop(env);
        assert!(f.call("task-finalize", &f.spec(&owner, "success")).is_err());
        assert!(f.temp.path().join("workspace/checkout").exists());
        let mut env = task_env::restore_environment("task").unwrap();
        assert_eq!(
            delivery::reconcile(&mut env, &delivery::local::Local).unwrap()["phase"],
            "confirmed"
        );
        drop(env);
        {
            let _lock = queue::locked(&f.queue, false).unwrap();
            queue::task_action(
                &f.queue,
                "task-finalize",
                &f.spec(&owner, "success"),
                Some(SESSION),
                &|_| panic!("no native call"),
            )
            .unwrap();
        }
        f.freeze_before_resources(&owner);
        assert_eq!(f.state()["tasks"]["task"]["final_outcome"], "success");
        let report = queue::finalization::drive(&f.queue).unwrap();
        assert_eq!(report.completed, 1, "{report:?}");
        assert_eq!(queue::finalization::drive(&f.queue).unwrap().completed, 0);
        assert_eq!(executor.executions.get(), 1);
        assert_eq!(fs::read(&target).unwrap(), b"formal deliverable");
        assert!(f.state()["tasks"]["review"].is_null());
        assert!(f.state()["waits"]["review"].is_null());
        assert!(f.state()["events"]["result"].is_null());
        f.clean(&owner);
    }

    #[test]
    fn frozen_associations_are_read_only_without_taking_another_owner_gate() {
        let f = Fixture::new();
        let owner = f.create();
        assert!(super::frozen_association_plan(&owner).is_err());
        f.record(&owner);
        let queue_before = f.state();
        let path = super::storage::plan_path(&owner).unwrap();
        assert!(!path.exists());
        assert!(super::frozen_association_plan(&owner).unwrap().is_none());
        assert!(!path.exists());
        assert_eq!(f.state(), queue_before);
        let expected = f.freeze_before_resources(&owner);
        let permit = lifecycle::acquire(&owner, true).unwrap();
        let _queue = queue::locked(&f.queue, false).unwrap();
        let original = util::read_json(&path).unwrap();
        let before = fs::read(path.join("authority.json")).unwrap();
        assert_eq!(
            super::frozen_association_plan(&owner).unwrap(),
            Some(expected)
        );
        assert_eq!(fs::read(path.join("authority.json")).unwrap(), before);
        let mut wrong = owner.clone();
        wrong.epoch += 1;
        assert!(super::frozen_association_plan(&wrong).is_err());
        wrong = owner.clone();
        wrong.instance_id.push_str("-foreign");
        assert!(super::frozen_association_plan(&wrong).is_err());
        let mut malformed = original.clone();
        malformed["terminal"]["owner"]["epoch"] = json!(999);
        util::atomic_write_json(&path, &malformed).unwrap();
        let bad_before = fs::read(path.join("authority.json")).unwrap();
        assert!(super::frozen_association_plan(&owner).is_err());
        assert_eq!(fs::read(path.join("authority.json")).unwrap(), bad_before);
        assert_eq!(f.state(), queue_before);
        util::atomic_write_json(&path, &original).unwrap();
        drop(_queue);
        drop(permit);
        let report = queue::finalization::drive(&f.queue).unwrap();
        assert_eq!(report.completed, 1, "{report:?}");
        f.clean(&owner);
    }
}
