use super::*;
use crate::ci::GitHubApi;
use crate::progress::{self, acceptance, lineage};
use crate::task_env::{self, DeliveryTarget};
use std::cell::{Cell, RefCell};
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::thread::JoinHandle;
use std::time::Duration;
use transport::Endpoint;

const CHILD: &str = "publish-runtime";
const SESSION: &str = "11111111-1111-1111-1111-111111111111";

#[derive(Clone, Copy)]
enum Ci {
    Pending,
    Success,
    FailedWithoutJobs,
    FailedJob,
}

struct Remote {
    queue: PathBuf,
    root: PathBuf,
    repository: PathBuf,
    head: String,
    pull: RefCell<J>,
    ci: Cell<Ci>,
    landing_ci: Cell<Ci>,
    pushes: Cell<usize>,
    creates: Cell<usize>,
    merges: Cell<usize>,
    lost_push_reply: Cell<bool>,
    lost_create_reply: Cell<bool>,
    lost_merge_reply: Cell<bool>,
    verify_open_calls: Cell<usize>,
    fail_verify_at: Cell<Option<usize>>,
    branch_unavailable: Cell<bool>,
    observations: RefCell<Vec<String>>,
}

impl Remote {
    fn landing_reads(&self) -> usize {
        self.observations
            .borrow()
            .iter()
            .filter(|suffix| {
                suffix.contains("Landing.yml") || suffix.starts_with("actions/runs/81")
            })
            .count()
    }

    fn before_verify(&self) -> Result<()> {
        let call = self.verify_open_calls.get() + 1;
        self.verify_open_calls.set(call);
        if self.fail_verify_at.get() == Some(call) {
            return Err(FlowError::new(
                crate::ErrorKind::Transient,
                "final PR GET temporarily unavailable",
            ));
        }
        Ok(())
    }

    fn before_send(&self, key: &str) {
        let _lock = queue::locked(&self.queue, false).unwrap();
        let operation = read(&self.root, "operation.json").unwrap();
        assert_eq!(operation["submissions"][key]["phase"], "sending");
    }

    fn run(&self) -> J {
        workflow_run(self.ci.get(), 41, &self.head, "pull_request")
    }

    fn landing_run(&self) -> J {
        let sha = git(&self.repository, &["rev-parse", "refs/heads/main"]);
        workflow_run(self.landing_ci.get(), 81, &sha, "push")
    }

    fn observed_pull(&self, record: &J) -> J {
        let repo = json!({"full_name":"owner/repo","html_url":"https://github.com/owner/repo"});
        json!({"number":7,"state":"open","merged":false,"draft":false,"mergeable":true,"auto_merge":null,
            "head":{"sha":record["head"],"ref":record["head_ref"],"repo":repo},
            "base":{"sha":record["target_base"],"ref":"main","repo":repo}})
    }
}

impl Remote {
    fn ci_payload(&self, suffix: &str) -> Result<J> {
        if suffix.starts_with("actions/workflows/CI.yml/runs?") {
            return Ok(json!({"workflow_runs":[self.run()]}));
        }
        if suffix.starts_with("actions/runs/41/attempts/1/jobs?") {
            let jobs = match self.ci.get() {
                Ci::FailedWithoutJobs => json!([]),
                Ci::FailedJob => {
                    json!([{"id":51,"run_id":41,"name":"test","head_sha":self.head,"status":"completed","conclusion":"failure"}])
                }
                Ci::Pending => {
                    json!([{"id":51,"run_id":41,"name":"test","head_sha":self.head,"status":"in_progress","conclusion":null}])
                }
                Ci::Success => {
                    json!([{"id":51,"run_id":41,"name":"test","head_sha":self.head,"status":"completed","conclusion":"success"}])
                }
            };
            return Ok(json!({"jobs":jobs}));
        }
        if suffix == "actions/runs/41" {
            return Ok(self.run());
        }
        self.landing_payload(suffix)
    }

    fn landing_payload(&self, suffix: &str) -> Result<J> {
        let run = self.landing_run();
        if suffix.starts_with("actions/workflows/Landing.yml/runs?") {
            return Ok(json!({"workflow_runs":[run]}));
        }
        if suffix.starts_with("actions/runs/81/attempts/1/jobs?") {
            return Ok(
                json!({"jobs":[{"id":82,"run_id":81,"name":"publish","head_sha":run["head_sha"],"status":run["status"],"conclusion":run["conclusion"]}]}),
            );
        }
        if suffix == "actions/runs/81" {
            return Ok(run);
        }
        panic!("unexpected remote observation: {suffix}");
    }
}

fn workflow_run(ci: Ci, id: u64, head: &str, event: &str) -> J {
    let (status, conclusion) = match ci {
        Ci::Pending => ("in_progress", J::Null),
        Ci::Success => ("completed", json!("success")),
        Ci::FailedWithoutJobs | Ci::FailedJob => ("completed", json!("failure")),
    };
    json!({"id":id,"run_attempt":1,"head_sha":head,"event":event,"status":status,"conclusion":conclusion,"html_url":format!("https://github.com/owner/repo/actions/runs/{id}")})
}

impl GitHubApi for Remote {
    fn get(&self, suffix: &str) -> Result<J> {
        let _lock = queue::locked(&self.queue, false)?;
        self.observations.borrow_mut().push(suffix.to_string());
        if suffix == "branches/main" {
            return Ok(
                json!({"commit":{"sha":git(&self.repository, &["rev-parse", "refs/heads/main"])}}),
            );
        }
        if suffix.starts_with("compare/") {
            let range = suffix.strip_prefix("compare/").unwrap();
            let (before, after) = range.split_once("...").unwrap();
            return Ok(
                json!({"status":if before == after { "identical" } else { "ahead" }, "merge_base_commit":{"sha":git(&self.repository, &["merge-base", before, after])}}),
            );
        }
        if suffix == "pulls/7" {
            return Ok(self.pull.borrow().clone());
        }
        if suffix.starts_with("pulls?") {
            return Ok(if self.pull.borrow().is_null() {
                json!([])
            } else {
                json!([self.pull.borrow().clone()])
            });
        }
        self.ci_payload(suffix)
    }
}

impl Endpoint for Remote {
    fn push(&self, record: &J) -> Result<()> {
        self.before_send("push");
        self.pushes.set(self.pushes.get() + 1);
        let reference = format!("refs/heads/{}", record["head_ref"].as_str().unwrap());
        git(
            &self.repository,
            &[
                "update-ref",
                &reference,
                record["head"].as_str().unwrap(),
                "",
            ],
        );
        if self.lost_push_reply.get() {
            return Err(FlowError::guard(
                "push response lost after actual local ref creation",
            ));
        }
        Ok(())
    }

    fn create(&self, record: &J, title: &str, body: &str) -> Result<()> {
        self.before_send("create");
        self.creates.set(self.creates.get() + 1);
        assert_eq!(title, "Reviewed change");
        assert_eq!(body, "Existing authorized publication");
        *self.pull.borrow_mut() = self.observed_pull(record);
        if self.lost_create_reply.get() {
            return Err(FlowError::guard("PR response lost after accepted creation"));
        }
        Ok(())
    }

    fn merge(&self, record: &J) -> Result<()> {
        self.before_send("merge");
        self.merges.set(self.merges.get() + 1);
        let head = record["head"].as_str().unwrap();
        let base = record["target_base"].as_str().unwrap();
        let tree = format!("{head}^{{tree}}");
        let commit = git(
            &self.repository,
            &[
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "-c",
                "commit.gpgsign=false",
                "commit-tree",
                &tree,
                "-p",
                base,
                "-p",
                head,
                "-m",
                "merge accepted head",
            ],
        );
        git(
            &self.repository,
            &["update-ref", "refs/heads/main", &commit, base],
        );
        let mut pull = self.pull.borrow_mut();
        pull["state"] = json!("closed");
        pull["merged"] = json!(true);
        pull["merge_commit_sha"] = json!(commit);
        if self.lost_merge_reply.get() {
            return Err(FlowError::new(
                crate::ErrorKind::Transport,
                "merge response lost after actual ref update",
            ));
        }
        Ok(())
    }

    fn verify_open(&self, record: &J) -> Result<J> {
        self.before_verify()?;
        let pull = self.get("pulls/7")?;
        ensure(
            pull["head"]["sha"] == record["head"]
                && pull["base"]["sha"] == record["target_base"]
                && pull["state"] == "open",
            "actual PR identity changed",
        )?;
        Ok(pull)
    }

    fn branch(&self, record: &J) -> Result<J> {
        if self.branch_unavailable.get() {
            return Err(FlowError::guard("remote ref temporarily unavailable"));
        }
        let reference = format!("refs/heads/{}", record["head_ref"].as_str().unwrap());
        let sha = git(&self.repository, &["rev-parse", &reference]);
        Ok(json!({"ref":reference,"object":{"type":"commit","sha":sha}}))
    }
}

struct Fixture {
    env: TaskEnvironment,
    context: J,
    remote: Remote,
    frozen_parent: J,
    _registry: task_env::TestEnvRootGuard,
    _directory: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        let directory = util::fs::test_tempdir();
        let registry = task_env::test_env_root(&directory.path().join("registry"));
        let mut env = task_env::create_environment(
            "job",
            &directory.path().join("work"),
            vec![DeliveryTarget::new(
                directory.path().join("formal"),
                "formal".into(),
            )],
            &directory.path().join("tmp"),
        )
        .unwrap();
        git(&env.workspace_root, &["init", "--quiet", "-b", "main"]);
        git(
            &env.workspace_root,
            &["config", "user.name", "Runtime fixture"],
        );
        git(
            &env.workspace_root,
            &["config", "user.email", "fixture@example.invalid"],
        );
        git(
            &env.workspace_root,
            &[
                "-c",
                "commit.gpgsign=false",
                "commit",
                "--allow-empty",
                "-qm",
                "base",
            ],
        );
        let base = git(&env.workspace_root, &["rev-parse", "HEAD"]);
        fs::write(env.workspace_root.join("reviewed"), "accepted content\n").unwrap();
        git(&env.workspace_root, &["add", "reviewed"]);
        git(
            &env.workspace_root,
            &[
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-qm",
                "reviewed change",
            ],
        );
        let head = git(&env.workspace_root, &["rev-parse", "HEAD"]);
        let repository = directory.path().join("observed-remote.git");
        git(
            directory.path(),
            &[
                "clone",
                "--bare",
                "--quiet",
                env.workspace_root.to_str().unwrap(),
                repository.to_str().unwrap(),
            ],
        );
        git(&repository, &["update-ref", "refs/heads/main", &base]);
        git(
            &env.workspace_root,
            &[
                "remote",
                "add",
                "origin",
                "https://github.com/owner/repo.git",
            ],
        );
        let queue_path = directory.path().join("queue");
        let mut state = accepted(&env.workspace_root, &head);
        queue::save(&queue_path, &mut state).unwrap();
        let root = directory.path().join("publisher");
        util::ensure_private_dir(&root, true).unwrap();
        let remote = Remote {
            queue: queue_path.clone(),
            root: root.clone(),
            repository,
            head,
            pull: RefCell::new(J::Null),
            ci: Cell::new(Ci::Pending),
            landing_ci: Cell::new(Ci::Pending),
            pushes: Cell::new(0),
            creates: Cell::new(0),
            merges: Cell::new(0),
            lost_push_reply: Cell::new(false),
            lost_create_reply: Cell::new(false),
            lost_merge_reply: Cell::new(false),
            verify_open_calls: Cell::new(0),
            fail_verify_at: Cell::new(None),
            branch_unavailable: Cell::new(false),
            observations: RefCell::new(vec![]),
        };
        let request = json!({"id":"runtime","state_version":state["tasks"]["job"]["state_version"],
            "authorization":"deliver","publication_authorization":"publish","merge_authorization":"merge", "original_base":base,
            "issue":130,"title":"Reviewed change","body":"Existing authorized publication",
            "policy":{"host":"github.com","repo":"owner/repo","base_ref":"main","endpoint":"https://github.com/owner/repo/tree/main","merge_method":"merge","workflows":[{"file":"CI.yml","event":"pull_request","decision":"required_jobs","jobs":["test"]}]}});
        crate::delivery::publication::prepare_remote(&mut env, &queue_path, &request, &remote)
            .unwrap();
        state = queue::load_queue(&queue_path).unwrap();
        let spec = json!({"task":CHILD,"state_version":state["tasks"][CHILD]["state_version"],"authorization":"publish"});
        crate::pr::publication_begin_with_api(&mut state, &spec, util::now(), &remote).unwrap();
        queue::save(&queue_path, &mut state).unwrap();
        crate::delivery::publication::reserve_remote(&mut env).unwrap();
        state = queue::load_queue(&queue_path).unwrap();
        let owner = Owner {
            queue_root: queue_path,
            queue_id: state["id"].as_str().unwrap().into(),
            task_id: "job".into(),
            epoch: 1,
            instance_id: env.instance_id.clone(),
        };
        let context = json!({"owner":owner,"root":root,"coordinator":directory.path().join("coordinator"),"request":request});
        Self {
            env,
            context,
            remote,
            frozen_parent: state["tasks"]["job"].clone(),
            _registry: registry,
            _directory: directory,
        }
    }

    fn root(&self) -> &Path {
        &self.remote.root
    }
    fn state(&self) -> J {
        queue::load_queue(&self.remote.queue).unwrap()
    }
    fn advance(&self, operation: &mut J) -> Result<()> {
        drive::advance_with(self.root(), &self.context, operation, &self.remote)
    }
    fn created(&self) -> J {
        let mut operation = json!({"phase":"pushing"});
        self.advance(&mut operation).unwrap();
        self.advance(&mut operation).unwrap();
        assert_eq!(operation["phase"], "waiting_ci");
        operation
    }
    fn parent_unchanged(&self) {
        assert_eq!(self.state()["tasks"]["job"], self.frozen_parent);
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        task_env::destroy_environment(&self.env).unwrap();
    }
}

fn git(root: &Path, args: &[&str]) -> String {
    let argv: Vec<String> = std::iter::once("git")
        .chain(args.iter().copied())
        .map(str::to_owned)
        .collect();
    let result = util::process::run_command(
        &argv,
        &util::process::CommandOptions {
            cwd: Some(root.to_path_buf()),
            creation_mask: Some(0o077),
            ..Default::default()
        },
    )
    .unwrap();
    assert!(result.success(), "git {args:?}: {}", result.stderr_text());
    result.stdout_text().trim().into()
}

fn action(state: &mut J, mut spec: J) {
    spec["task"] = json!("job");
    spec["state_version"] = state["tasks"]["job"]["state_version"].clone();
    lineage::action(state, &spec, 1.0).unwrap();
}

fn accepted(checkout: &Path, head: &str) -> J {
    let mut state = json!({"id":"runtime-queue","tasks":{},"waits":{},"events":{},"deliveries":{},"stopped":false});
    lineage::action(&mut state, &json!({"kind":"plan","task":"job","goal":"publish reviewed change", "basis":{"original":["user request"],"changes":[],"specifications":[],"endpoint":"https://github.com/owner/repo/tree/main","confirmation":"user"},
        "authorization":[{"id":"deliver","action":"delivery","source":"user request"},{"id":"publish","action":"publication","source":"user request"},{"id":"merge","action":"merge","source":"user request"}]}), 1.0).unwrap();
    action(
        &mut state,
        json!({"kind":"artifact","artifact":{"adapter":"git","id":head,"location":checkout}}),
    );
    approve(&mut state, head, "review");
    state
}

fn approve(state: &mut J, head: &str, name: &str) {
    let event_id = format!("{name}-result");
    action(
        state,
        json!({"kind":"candidate","candidate":head,"reason":"fixed accepted candidate"}),
    );
    action(
        state,
        json!({"kind":"activity","activity":name,"role":"acceptance","input":head,"session_epoch":1,
        "result_wait":{"id":name,"epoch":1,"assignment":name,"kind":"signal","owner":"reviewer","reason":"independent acceptance","condition":"report","timeout_seconds":60,"producer":{"adapter":"codex","session":"independent"}}}),
    );
    let verdict = acceptance::submit(state,name,&json!({"adapter":"codex","session":"independent"}),&json!({"conclusion":"passed","scope":"original requirement","checks":[{"id":"goal","criterion":"goal","source":"user request","status":"passed","evidence":"inspected exact tracked commit"}],"gaps":[],"reuses":[],"resolutions":[]})).unwrap();
    let wait = state["waits"][name].clone();
    let mut event = json!({"id":event_id,"task":name,"wait":name,"epoch":state["tasks"][name]["epoch"],"acknowledged":true,"outcome":"complete"});
    progress::attach(state, &mut event, &wait, 2.0).unwrap();
    state["events"][&event_id] = event;
    let spec = json!({"task":name,"state_version":state["tasks"][name]["state_version"],"event":event_id,"verdict":verdict["id"],"next_action":{"kind":"delivery","owner":"host","reason":"independent exact candidate passed","condition":"existing authority"}});
    progress::apply(state, &spec, 3.0).unwrap();
}

#[test]
fn lost_push_and_create_replies_reconcile_original_effects_without_replaying() {
    let f = Fixture::new();
    f.remote.lost_push_reply.set(true);
    f.remote.lost_create_reply.set(true);
    let mut operation = f.created();
    assert_eq!(operation["submissions"]["push"]["phase"], "unknown");
    assert_eq!(operation["submissions"]["create"]["phase"], "unknown");
    assert_eq!(f.state()["tasks"][CHILD]["pr"]["number"], 7);
    operation["phase"] = json!("pushing");
    f.advance(&mut operation).unwrap();
    f.advance(&mut operation).unwrap();
    assert_eq!(f.remote.pushes.get(), 1);
    assert_eq!(f.remote.creates.get(), 1);
    assert!(operation["receipt"].is_null());
    f.parent_unchanged();
}

#[test]
fn interrupted_branch_observation_reuses_the_original_submission_after_restart() {
    let f = Fixture::new();
    f.remote.branch_unavailable.set(true);
    let mut operation = json!({"phase":"pushing"});
    assert!(f.advance(&mut operation).is_err());
    let mut recovered = read(f.root(), "operation.json").unwrap();
    assert_eq!(recovered["submissions"]["push"]["phase"], "returned");
    assert!(f.advance(&mut recovered).is_err());
    assert_eq!(f.remote.pushes.get(), 1);
    f.remote.branch_unavailable.set(false);
    f.advance(&mut recovered).unwrap();
    f.advance(&mut recovered).unwrap();
    assert_eq!(recovered["phase"], "waiting_ci");
    assert_eq!(f.remote.pushes.get(), 1);
    f.parent_unchanged();
}

#[test]
fn pending_ci_waits_then_workflow_failure_without_jobs_requires_attention() {
    let f = Fixture::new();
    let mut operation = f.created();
    f.advance(&mut operation).unwrap_or_else(|error| {
        panic!(
            "{error}; operation={operation}; child={}",
            f.state()["tasks"][CHILD]
        )
    });
    assert_eq!(operation["phase"], "waiting_ci");
    assert_eq!(operation["ci"]["state"], "pending");
    assert!(operation["next_observation"].as_f64().unwrap() > util::now());
    let count = f.remote.observations.borrow().len();
    f.advance(&mut operation).unwrap();
    assert_eq!(f.remote.observations.borrow().len(), count);
    f.remote.ci.set(Ci::FailedWithoutJobs);
    operation["next_observation"] = J::Null;
    assert!(f.advance(&mut operation).is_err());
    assert_eq!(f.state()["tasks"][CHILD]["pr"]["phase"], "needs_attention");
    assert_eq!(operation["ci"]["state"], "needs_attention");
    assert_eq!(f.remote.run()["conclusion"], "failure");
    assert_eq!(f.remote.merges.get(), 0);
    f.parent_unchanged();
}

#[test]
fn failed_required_job_keeps_the_original_publication_for_a_decision() {
    let f = Fixture::new();
    let mut operation = f.created();
    f.remote.ci.set(Ci::FailedJob);
    assert!(f.advance(&mut operation).is_err());
    assert_eq!(operation["ci"]["state"], "failure");
    let task = f.state()["tasks"][CHILD].clone();
    assert_eq!(task["pr"]["phase"], "repair_needed");
    assert!(!task["pr"]["observation"]["failures"]
        .as_array()
        .unwrap()
        .is_empty());
    assert_eq!(f.remote.merges.get(), 0);
    assert_eq!((f.remote.pushes.get(), f.remote.creates.get()), (1, 1));
    f.parent_unchanged();
}

#[test]
fn merge_rechecks_current_ci_and_exact_pr_head_before_any_submission() {
    let f = Fixture::new();
    let mut operation = f.created();
    operation["phase"] = json!("merging");
    operation["ci"] = json!({"state":"success"});
    assert!(f.advance(&mut operation).is_err());
    assert_eq!(f.remote.merges.get(), 0);
    f.remote.ci.set(Ci::Success);
    f.remote.pull.borrow_mut()["head"]["sha"] = json!("d".repeat(40));
    assert!(f.advance(&mut operation).is_err());
    assert_eq!(f.remote.merges.get(), 0);
    let actual_record = f.state()["tasks"][CHILD]["pr"].clone();
    *f.remote.pull.borrow_mut() = f.remote.observed_pull(&actual_record);
    f.advance(&mut operation).unwrap();
    assert_eq!(f.remote.merges.get(), 1);
    assert_eq!(operation["phase"], "confirming");
    assert_eq!(operation["merge_ci"]["state"], "success");
    operation["phase"] = json!("merging");
    f.advance(&mut operation).unwrap();
    assert_eq!(f.remote.merges.get(), 1);
    f.parent_unchanged();
}

#[test]
fn merge_ready_get_outage_waits_without_reserving_or_losing_the_original_merge() {
    let f = Fixture::new();
    let mut operation = f.created();
    f.remote.ci.set(Ci::Success);
    operation["phase"] = json!("merging");
    // refresh_ci performs the first verification, and merge_ready performs the
    // final verification immediately before the durable mutation reservation.
    let first = f.remote.verify_open_calls.get();
    f.remote.fail_verify_at.set(Some(first + 2));
    let error = f.advance(&mut operation).unwrap_err();
    assert_eq!(error.kind, crate::ErrorKind::Transient);
    assert_eq!(operation["merge_ci"]["state"], "success");
    assert_eq!(operation["phase"], "merging");
    assert!(operation["submissions"]["merge"].is_null());
    assert_eq!(f.remote.verify_open_calls.get(), first + 2);
    assert_eq!(f.remote.merges.get(), 0);
    record_error(&mut operation, &error);
    assert_eq!(operation["phase"], "merging");
    assert!(operation["retry_observation_at"].as_f64().unwrap() > util::now());
    save(f.root(), "operation.json", &operation).unwrap();
    let mut recovered = read(f.root(), "operation.json").unwrap();
    let reads = f.remote.observations.borrow().len();
    f.advance(&mut recovered).unwrap();
    assert_eq!(f.remote.observations.borrow().len(), reads);
    assert_eq!(f.remote.verify_open_calls.get(), first + 2);
    assert!(recovered["submissions"]["merge"].is_null());
    // Advance the saved retry deadline instead of sleeping for the policy's
    // 30-second interval. The endpoint only fails the one designated read.
    recovered["retry_observation_at"] = json!(util::now() - 1.0);
    f.advance(&mut recovered).unwrap();
    assert_eq!(recovered["phase"], "confirming");
    assert_eq!(recovered["submissions"]["merge"]["phase"], "returned");
    assert_eq!(f.remote.merges.get(), 1);
    assert_eq!(f.remote.verify_open_calls.get(), first + 4);
    recovered["phase"] = json!("merging");
    f.advance(&mut recovered).unwrap();
    assert_eq!(f.remote.merges.get(), 1);
    f.parent_unchanged();
}

#[test]
fn merge_reply_lost_after_the_real_effect_stays_unknown_and_never_replays() {
    let f = Fixture::new();
    let mut operation = f.created();
    f.remote.ci.set(Ci::Success);
    f.remote.lost_merge_reply.set(true);
    operation["phase"] = json!("merging");
    f.advance(&mut operation).unwrap();
    assert_eq!(operation["phase"], "confirming");
    assert_eq!(operation["submissions"]["merge"]["phase"], "unknown");
    assert_eq!(f.remote.merges.get(), 1);
    let landed = git(&f.remote.repository, &["rev-parse", "refs/heads/main"]);
    assert_eq!(f.remote.pull.borrow()["merge_commit_sha"], landed);
    assert_eq!(f.remote.pull.borrow()["merged"], true);
    // submit saved uncertainty before its caller could checkpoint confirming.
    // Restarting this earlier prefix must observe, never repeat the mutation.
    let mut recovered = read(f.root(), "operation.json").unwrap();
    assert_eq!(recovered["phase"], "merging");
    assert_eq!(recovered["submissions"]["merge"]["phase"], "unknown");
    let verifications = f.remote.verify_open_calls.get();
    f.advance(&mut recovered).unwrap();
    assert_eq!(recovered["phase"], "confirming");
    assert_eq!(f.remote.merges.get(), 1);
    assert_eq!(f.remote.verify_open_calls.get(), verifications);
    assert_eq!(
        git(&f.remote.repository, &["rev-parse", "refs/heads/main"]),
        landed
    );
    assert!(recovered["receipt"].is_null());
    f.parent_unchanged();
}

#[test]
fn stopped_paused_and_cancelled_publications_do_not_submit_any_remote_operation() {
    let f = Fixture::new();
    let baseline = f.state();
    for change in ["stopped", "paused", "cancelled", "child_paused"] {
        let mut changed = baseline.clone();
        match change {
            "stopped" => changed["stopped"] = json!(true),
            "paused" | "cancelled" => changed["tasks"]["job"]["control"] = json!(change),
            "child_paused" => changed["tasks"][CHILD]["control"] = json!("paused"),
            _ => unreachable!(),
        }
        queue::save(&f.remote.queue, &mut changed).unwrap();
        for phase in ["pushing", "creating", "merging"] {
            let mut operation = json!({"phase":phase});
            assert!(f.advance(&mut operation).is_err(), "{change}/{phase}");
            assert!(operation["submissions"].is_null());
        }
    }
    assert_eq!(
        (
            f.remote.pushes.get(),
            f.remote.creates.get(),
            f.remote.merges.get()
        ),
        (0, 0, 0)
    );
    let mut restored = baseline;
    queue::save(&f.remote.queue, &mut restored).unwrap();
}

struct ModelBroker {
    thread: Option<JoinHandle<Vec<J>>>,
}

impl ModelBroker {
    fn start(root: &Path, actions: &[&str]) -> Self {
        let actions: Vec<String> = actions.iter().map(|value| (*value).to_owned()).collect();
        let listener = UnixListener::bind(root.join("broker.sock")).unwrap();
        listener.set_nonblocking(true).unwrap();
        let thread = std::thread::spawn(move || {
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            let mut calls = vec![];
            for action in actions {
                let turn_id = format!("{action}-turn");
                for method in ["thread/read", "turn/start", "thread/read"] {
                    let mut stream = accept_model(&listener, deadline);
                    stream
                        .set_read_timeout(Some(Duration::from_secs(3)))
                        .unwrap();
                    let mut line = String::new();
                    BufReader::new(stream.try_clone().unwrap())
                        .read_line(&mut line)
                        .unwrap();
                    let request: J = serde_json::from_str(&line).unwrap();
                    assert_eq!(request["method"], method);
                    let result = model_reply(&request, method, &action, &turn_id);
                    calls.push(request);
                    writeln!(stream, "{}", json!({"result":result})).unwrap();
                }
            }
            calls
        });
        Self {
            thread: Some(thread),
        }
    }
    fn finish(mut self) -> Vec<J> {
        self.thread.take().unwrap().join().unwrap()
    }
}

fn accept_model(
    listener: &UnixListener,
    deadline: std::time::Instant,
) -> std::os::unix::net::UnixStream {
    loop {
        match listener.accept() {
            Ok((stream, _)) => return stream,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                assert!(
                    std::time::Instant::now() < deadline,
                    "broker request was not sent"
                );
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(error) => panic!("{error}"),
        }
    }
}

fn model_reply(request: &J, method: &str, action: &str, turn_id: &str) -> J {
    if method == "turn/start" {
        assert_eq!(request["params"]["sandboxPolicy"]["networkAccess"], false);
        assert_eq!(
            request["params"]["outputSchema"]["properties"]["action"]["enum"][0],
            action
        );
        return json!({"turn":{"id":turn_id}});
    }
    let report = json!({"delivery":"runtime","action":action,"facts":"requested from fixed host evidence","next_step":"host must requery"});
    json!({"thread":{"id":SESSION,"status":{"type":"idle"},"turns":[{"id":turn_id,"status":"completed","items":[{"type":"agentMessage","text":report.to_string()}]}]}})
}

#[test]
fn successful_ci_requests_independent_merge_then_collects_original_model_turn() {
    let f = Fixture::new();
    let mut operation = json!({"phase":"planned"});
    f.remote.ci.set(Ci::Success);
    save(f.root(), "publisher-agent.json", &json!({"phase":"ready","session":SESSION,"model":{"providerID":"openai","id":"fixture-publisher","reasoning_effort":"high"}})).unwrap();
    let broker = ModelBroker::start(f.root(), &["publish", "merge"]);
    f.advance(&mut operation).unwrap();
    assert_eq!(operation["phase"], "planning");
    f.advance(&mut operation).unwrap();
    assert_eq!(operation["phase"], "reserving");
    assert_eq!(operation["requests"]["publish"]["action"], "publish");
    // The business fixture already prepared/reserved the real delivery before
    // its injectable remote writes. Continue at that actual durable boundary.
    operation["phase"] = json!("pushing");
    f.advance(&mut operation).unwrap();
    f.advance(&mut operation).unwrap();
    f.advance(&mut operation).unwrap();
    assert_eq!(operation["phase"], "merge_planning");
    assert_eq!(operation["model_turn"]["phase"], "submitted");
    assert_eq!(f.remote.merges.get(), 0);
    f.advance(&mut operation).unwrap();
    assert_eq!(operation["phase"], "merging");
    assert_eq!(operation["requests"]["merge"]["action"], "merge");
    let calls = broker.finish();
    assert_eq!(calls.len(), 6);
    assert_eq!(calls[1]["params"]["model"], "fixture-publisher");
    f.advance(&mut operation).unwrap();
    assert_eq!(f.remote.merges.get(), 1);
    f.parent_unchanged();
}

struct IsolatedFixture {
    remote: Remote,
    context: J,
    workspace: PathBuf,
    _registry: task_env::TestEnvRootGuard,
    _directory: tempfile::TempDir,
}

impl IsolatedFixture {
    fn new() -> Self {
        let directory = util::fs::test_tempdir();
        let queue_path = directory.path().join("queue");
        let registry = task_env::test_env_root(&queue_path.join("queue.json/task-environments"));
        let source = directory.path().join("source");
        util::ensure_private_dir(&source, true).unwrap();
        git(&source, &["init", "--quiet", "-b", "main"]);
        git(
            &source,
            &[
                "remote",
                "add",
                "origin",
                "https://github.com/owner/repo.git",
            ],
        );
        git(
            &source,
            &[
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "--allow-empty",
                "-qm",
                "base",
            ],
        );
        let base = git(&source, &["rev-parse", "HEAD"]);
        queue::init_isolated(&queue_path, None, 30).unwrap();
        let receiver = directory.path().join("receiver");
        util::ensure_private_dir(&receiver, true).unwrap();
        let registration = json!({"version":2,"task":"coordinator","role":"coordinator","pane":"window:pane","terminal":"terminal","socket":"/fixture","cwd":source,"session":SESSION,"phase":"monitoring"});
        util::atomic_write_json(&receiver.join("state.json"), &registration).unwrap();
        let mut state = queue::load_queue(&queue_path).unwrap();
        let seed = accepted(&source, &base);
        for key in ["tasks", "waits", "events", "deliveries"] {
            state[key] = seed[key].clone();
        }
        state["guard_dir"] = json!(receiver);
        state["receiver"] = crate::guard::scope(&registration);
        queue::save(&queue_path, &mut state).unwrap();
        isolated_action(&queue_path, "task-environment", &json!({"action":"create","task":"job","state_version":state["tasks"]["job"]["state_version"],"queue_id":state["id"],"epoch":1,
            "environment":{"workspace_root":directory.path().join("workspace"),"temp_root":directory.path().join("scratch"),"delivery_targets":[],"repository":source,"commit":base}})).unwrap();
        let mut env = task_env::restore_environment("job").unwrap();
        let checkout = env.workspace_root.join("checkout");
        fs::write(checkout.join("reviewed"), "accepted isolated content\n").unwrap();
        git(&checkout, &["add", "reviewed"]);
        git(
            &checkout,
            &[
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-qm",
                "reviewed change",
            ],
        );
        let head = git(&checkout, &["rev-parse", "HEAD"]);
        state = queue::load_queue(&queue_path).unwrap();
        // This fixture has no receiver service. Its creation registration is
        // retired before publication, rather than claiming it is a live gate.
        state.as_object_mut().unwrap().remove("guard_dir");
        state.as_object_mut().unwrap().remove("receiver");
        action(
            &mut state,
            json!({"kind":"artifact","artifact":{"adapter":"git","id":head,"location":checkout}}),
        );
        approve(&mut state, &head, "isolated-review");
        queue::save(&queue_path, &mut state).unwrap();
        let root = super::super::prepare(&env, "publisher-test").unwrap();
        let repository = directory.path().join("observed-remote.git");
        git(
            directory.path(),
            &[
                "clone",
                "--bare",
                "--quiet",
                checkout.to_str().unwrap(),
                repository.to_str().unwrap(),
            ],
        );
        git(&repository, &["update-ref", "refs/heads/main", &base]);
        let remote = Remote {
            queue: queue_path.clone(),
            root: root.clone(),
            repository,
            head,
            pull: RefCell::new(J::Null),
            ci: Cell::new(Ci::Success),
            landing_ci: Cell::new(Ci::Pending),
            pushes: Cell::new(0),
            creates: Cell::new(0),
            merges: Cell::new(0),
            lost_push_reply: Cell::new(false),
            lost_create_reply: Cell::new(false),
            lost_merge_reply: Cell::new(false),
            verify_open_calls: Cell::new(0),
            fail_verify_at: Cell::new(None),
            branch_unavailable: Cell::new(false),
            observations: RefCell::new(vec![]),
        };
        let request = json!({"id":"runtime","state_version":state["tasks"]["job"]["state_version"],
            "authorization":"deliver","publication_authorization":"publish","merge_authorization":"merge","original_base":base,
            "issue":130,"title":"Reviewed change","body":"Existing authorized publication",
            "policy":{"host":"github.com","repo":"owner/repo","base_ref":"main","endpoint":"https://github.com/owner/repo/tree/main","merge_method":"merge",
                "workflows":[{"file":"CI.yml","event":"pull_request","decision":"required_jobs","jobs":["test"]}],"landing_workflows":[{"file":"Landing.yml","event":"push"}]}});
        crate::delivery::publication::prepare_remote(&mut env, &queue_path, &request, &remote)
            .unwrap();
        let owner = task_env::isolation::owner(&env).unwrap();
        let context = json!({"owner":owner,"root":root,"coordinator":root,"request":request});
        let workspace = env.workspace_root.clone();
        // The real host runner must acquire this registered allocation itself.
        env.release_ownership();
        drop(env);
        Self {
            remote,
            context,
            workspace,
            _registry: registry,
            _directory: directory,
        }
    }

    fn advance(&self, operation: &mut J) -> Result<()> {
        drive::advance_with(&self.remote.root, &self.context, operation, &self.remote)
    }

    fn finish(&self) {
        let mut state = queue::load_queue(&self.remote.queue).unwrap();
        let receiver = self._directory.path().join("receiver");
        let registration = util::read_state_json(&receiver, "state.json").unwrap();
        state["guard_dir"] = json!(receiver);
        state["receiver"] = crate::guard::scope(&registration);
        queue::save(&self.remote.queue, &mut state).unwrap();
        let owner = &self.context["owner"];
        isolated_action(&self.remote.queue,"task-finalize",&json!({"task":"job","queue_id":owner["queue_id"],"epoch":owner["epoch"],"instance_id":owner["instance_id"],
            "state_version":state["tasks"]["job"]["state_version"],"outcome":"success","reason":"test host confirmed actual local Git landing through injected GitHub observations"})).unwrap();
        let report = queue::finalization::drive(&self.remote.queue).unwrap();
        assert!(
            queue::load_queue(&self.remote.queue).unwrap()["tasks"]["job"].is_null(),
            "{report:?}"
        );
        assert!(!self.workspace.exists());
    }
}

fn isolated_action(root: &Path, action: &str, spec: &J) -> Result<J> {
    queue::finalization::task_action(root, action, spec, Some(SESSION), &|_| {
        panic!("no model/native call is authorized by this fixture")
    })
}

#[test]
fn real_isolated_reservation_and_confirm_observe_the_actual_landing_then_reclaim() {
    let f = IsolatedFixture::new();
    let mut operation = json!({"phase":"reserving"});
    f.advance(&mut operation).unwrap();
    assert_eq!(operation["phase"], "pushing");
    let state = queue::load_queue(&f.remote.queue).unwrap();
    let frozen = state["tasks"]["job"].clone();
    assert_eq!(frozen["control"], "delivering");
    f.advance(&mut operation).unwrap();
    f.advance(&mut operation).unwrap();
    operation["phase"] = json!("merging");
    f.advance(&mut operation).unwrap();
    assert_eq!(operation["phase"], "confirming");
    f.advance(&mut operation).unwrap();
    assert_eq!(operation["phase"], "confirming");
    assert_eq!(operation["landing_ci"]["state"], "pending");
    assert!(operation["receipt"].is_null());
    let reads = f.remote.landing_reads();
    f.advance(&mut operation).unwrap();
    assert_eq!(f.remote.landing_reads(), reads);
    assert_eq!(f.remote.merges.get(), 1);
    f.remote.landing_ci.set(Ci::Success);
    operation["next_landing_observation"] = J::Null;
    f.advance(&mut operation).unwrap();
    assert_eq!(operation["phase"], "complete");
    assert_eq!(operation["landing_ci"]["state"], "success");
    let commit = git(&f.remote.repository, &["rev-parse", "refs/heads/main"]);
    assert_eq!(operation["receipt"]["commit"], commit);
    assert_eq!(operation["receipt"]["head"], f.remote.head);
    assert_eq!(
        (
            f.remote.pushes.get(),
            f.remote.creates.get(),
            f.remote.merges.get()
        ),
        (1, 1, 1)
    );
    assert_eq!(
        queue::load_queue(&f.remote.queue).unwrap()["tasks"]["job"],
        frozen
    );
    f.advance(&mut operation).unwrap();
    assert_eq!(f.remote.merges.get(), 1);
    save(
        &f.remote.root,
        "publisher-agent.json",
        &json!({"phase":"ready","session":SESSION}),
    )
    .unwrap();
    bind_producer(&f.remote.root, &f.context).unwrap();
    let bound = queue::load_queue(&f.remote.queue).unwrap();
    assert_eq!(bound["waits"][CHILD]["producer"]["session"], SESSION);
    bind_producer(&f.remote.root, &f.context).unwrap();
    assert_eq!(queue::load_queue(&f.remote.queue).unwrap(), bound);
    let mut interrupted_before_signal_checkpoint = operation.clone();
    signal(&f.remote.root, &f.context, &mut operation).unwrap();
    assert_eq!(operation["phase"], "signalled");
    let result_path = queue::artifacts(&f.remote.queue)
        .unwrap()
        .join("results")
        .join(format!("{CHILD}.json"));
    let result = util::read_json(&result_path).unwrap();
    assert_eq!(result["wait"], CHILD);
    assert_eq!(result["outcome"], "complete");
    assert_eq!(result["lifecycle_owner"], f.context["owner"]);
    let bytes = result_bytes(&result_path);
    signal(
        &f.remote.root,
        &f.context,
        &mut interrupted_before_signal_checkpoint,
    )
    .unwrap();
    assert_eq!(result_bytes(&result_path), bytes);
    assert_eq!(util::read_json(&result_path).unwrap(), result);
    f.finish();
}

fn result_bytes(path: &Path) -> Vec<u8> {
    if path.is_dir() {
        return fs::read(path.join("authority.json")).unwrap();
    }
    fs::read(path).unwrap()
}
