//! Prepare a host-owned publisher and verify its actual GitHub landing.
use super::*;

/// Persist a single publication child/PR intent, then freeze the accepted parent.
/// Remote reads and local Git provenance checks happen outside the queue lock.
pub fn prepare_remote(
    env: &mut TaskEnvironment,
    queue: &Path,
    request: &J,
    api: &dyn GitHubApi,
) -> Result<J> {
    let _permit = crate::task_env::isolation::mutation_permit(env)?;
    ensure(
        env.owns_lock() && env.state == EnvironmentState::Active,
        "publication requires owned active environment",
    )?;
    let before = queue_snapshot(queue)?;
    let spec = prepare_child(env, queue, request, api, &before)?;
    let executor = Observer {
        state: &before,
        api,
    };
    super::super::prepare(env, queue, &spec, &executor)
}

fn prepare_child(
    env: &TaskEnvironment,
    queue: &Path,
    request: &J,
    api: &dyn GitHubApi,
    before: &J,
) -> Result<J> {
    let parent = &before["tasks"][&env.task_id];
    if !parent["publication_preparation"].is_null() {
        return recover_preparation(parent, request);
    }
    super::super::validate_parent(parent, request)?;
    validate_policy(parent, request)?;
    let mut staged = before.clone();
    let spec = stage_child(env, request, api, &mut staged)?;
    commit_child(queue, &env.task_id, before, &staged)?;
    Ok(spec)
}

fn recover_preparation(parent: &J, request: &J) -> Result<J> {
    let saved = &parent["publication_preparation"];
    ensure(
        saved["request"] == *request,
        "publication preparation already exists; resume its exact request",
    )?;
    Ok(saved["spec"].clone())
}

fn stage_child(
    env: &TaskEnvironment,
    request: &J,
    api: &dyn GitHubApi,
    state: &mut J,
) -> Result<J> {
    let epoch = crate::task_env::isolation::owner(env)
        .map(|o| o.epoch as i64)
        .unwrap_or(1);
    let child = lineage::publication::register(state, &env.task_id, request, epoch, util::now())?;
    let publication = field(&child, "task")?;
    let pr_spec = pr_spec(env, state, request, &child, api)?;
    crate::pr::prepare(state, &pr_spec, util::now())?;
    let mut spec = request.clone();
    spec["adapter"] = json!("github-pr");
    spec["target"] = request["policy"]["endpoint"].clone();
    spec["publication"] = json!(publication);
    spec["state_version"] = state["tasks"][&env.task_id]["state_version"].clone();
    state["tasks"][&env.task_id]["publication_preparation"] =
        json!({"request":request,"spec":spec});
    Ok(spec)
}

fn pr_spec(
    env: &TaskEnvironment,
    state: &J,
    request: &J,
    child: &J,
    api: &dyn GitHubApi,
) -> Result<J> {
    let policy = &request["policy"];
    let base_ref = field(policy, "base_ref")?;
    let remote = api.get(&format!(
        "branches/{}",
        util::pyjson::quote_component(&base_ref)
    ))?;
    let target_base = crate::pr::sha(model::dig(&remote, &["commit", "sha"]))?;
    let checkout = publication_checkout(env, state, child)?;
    let own_commits = own_commits(&checkout, request, child)?;
    Ok(
        json!({"task":child["task"],"state_version":child["state_version"],
        "checkout":checkout,"repo":policy["repo"],"host":policy["host"],
        "base_ref":base_ref,"head_ref":format!("flow/{}/{}", env.instance_id, field(request,"id")?),
        "original_base":request["original_base"],"head":child["candidate"],
        "own_commits":own_commits,
        "dependencies":[],
        "issue":request["issue"],"target_base":target_base}),
    )
}

fn own_commits(checkout: &str, request: &J, child: &J) -> Result<Vec<String>> {
    let base = crate::pr::sha(request.get("original_base"))?;
    let head = crate::pr::sha(child.get("candidate"))?;
    let range = format!("{base}..{head}");
    let commits = crate::pr::git(Path::new(checkout), &["rev-list", "--reverse", &range])?;
    Ok(commits.lines().map(str::to_owned).collect())
}

fn publication_checkout(env: &TaskEnvironment, state: &J, child: &J) -> Result<String> {
    let parent = &state["tasks"][&env.task_id];
    let candidate = field(child, "candidate")?;
    let artifact = &parent["artifacts"][candidate]["object"];
    ensure(
        artifact["adapter"] == "git",
        "remote publication requires a Git artifact",
    )?;
    let checkout = field(artifact, "location")?;
    ensure(
        crate::task_env::is_within_disposable(env, Path::new(&checkout)),
        "publication checkout must belong to the task environment",
    )?;
    Ok(checkout)
}

fn commit_child(queue: &Path, parent: &str, before: &J, staged: &J) -> Result<()> {
    let _lock = crate::queue::locked(queue, false)?;
    let mut current = crate::queue::load_queue(queue)?;
    validate_preparation_snapshot(&current, before, parent)?;
    let spec = &staged["tasks"][parent]["publication_preparation"]["spec"];
    let name = field(spec, "publication")?;
    ensure(
        current["tasks"].get(&name).is_none() && current["waits"].get(&name).is_none(),
        "publication child or result wait already exists",
    )?;
    let child = &staged["tasks"][&name];
    crate::progress::ownership::publication(&current, child, &child["pr"])?;
    current["tasks"][parent] = staged["tasks"][parent].clone();
    current["tasks"][&name] = child.clone();
    current["waits"][&name] = staged["waits"][&name].clone();
    crate::queue::save(queue, &mut current)
}

fn validate_preparation_snapshot(current: &J, before: &J, parent: &str) -> Result<()> {
    ensure(
        current["id"] == before["id"]
            && current["stopped"] != true
            && current["tasks"][parent] == before["tasks"][parent],
        "requirement changed during publication preparation; retry the same request",
    )
}

fn permission(parent: &J, request: &J, key: &str, action: &str) -> Result<()> {
    let id = field(request, key)?;
    let record = &requirement(parent)["authorization"][id];
    ensure(
        record["action"] == action,
        "existing explicit remote publication authorization required",
    )?;
    field(record, "source").map(|_| ())
}

fn validate_policy(parent: &J, request: &J) -> Result<()> {
    validate_target(&json!({"target":request["policy"]["endpoint"],"policy":request["policy"]}))?;
    ensure(
        requirement(parent)["basis"]["endpoint"] == request["policy"]["endpoint"],
        "remote target differs from the immutable requirement endpoint",
    )?;
    permission(parent, request, "authorization", "delivery")?;
    permission(parent, request, "publication_authorization", "publication")?;
    permission(parent, request, "merge_authorization", "merge")
}

pub(in crate::delivery) fn validate_target(spec: &J) -> Result<()> {
    let policy = &spec["policy"];
    validate_policy_fields(policy)?;
    ensure(
        policy["host"] == "github.com",
        "remote publication requires github.com",
    )?;
    let repo = field(policy, "repo")?;
    ensure(valid_repository(&repo), "invalid publication repository")?;
    let branch = field(policy, "base_ref")?;
    let endpoint = format!("https://github.com/{repo}/tree/{branch}");
    ensure(
        spec["target"] == endpoint && policy["endpoint"] == endpoint,
        "remote publication target does not match its frozen policy",
    )?;
    ensure(
        policy["merge_method"] == "merge",
        "remote publication requires the authorized merge method",
    )?;
    validate_ci_policy(policy)
}

fn validate_ci_policy(policy: &J) -> Result<()> {
    validate_ci_inventory(policy, &policy["workflows"], json!(1))?;
    validate_premerge_inventory(&policy["workflows"])?;
    let landing = &policy["landing_workflows"];
    if landing.as_array().is_some_and(|rows| !rows.is_empty()) {
        validate_ci_inventory(policy, landing, J::Null)?;
    }
    Ok(())
}

fn validate_premerge_inventory(workflows: &J) -> Result<()> {
    let rows = workflows
        .as_array()
        .ok_or_else(|| FlowError::progress("publisher requires an explicit premerge inventory"))?;
    rows.iter().try_for_each(|row| {
        ensure(
            row["decision"] == "required_jobs"
                && row["jobs"].as_array().is_some_and(|jobs| !jobs.is_empty()),
            "publisher premerge workflows require decision=required_jobs and exact nonempty jobs",
        )
    })
}

fn validate_ci_inventory(policy: &J, workflows: &J, pr: J) -> Result<()> {
    crate::ci::validate(
        &json!({"version":1,"host":policy["host"],"repo":policy["repo"],
        "task":"publication-policy","epoch":1,"sha":"a".repeat(40),"pr":pr,"workflows":workflows}),
    )
    .map(|_| ())
}

fn validate_policy_fields(policy: &J) -> Result<()> {
    let fields = policy
        .as_object()
        .ok_or_else(|| FlowError::progress("publication policy must be an object"))?;
    ensure(
        fields.keys().all(|key| {
            [
                "host",
                "repo",
                "base_ref",
                "endpoint",
                "merge_method",
                "workflows",
                "landing_workflows",
            ]
            .contains(&key.as_str())
        }),
        "unsupported publication policy field",
    )?;
    ensure(
        policy.get("landing_workflows").is_none_or(J::is_array),
        "landing workflow policy must be an explicit array",
    )
}

fn valid_repository(repo: &str) -> bool {
    let parts: Vec<_> = repo.split('/').collect();
    parts.len() == 2
        && parts.iter().all(|part| {
            !part.is_empty()
                && *part != "."
                && *part != ".."
                && part
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || b"._-".contains(&c))
        })
}

pub(in crate::delivery) fn validate_parent(state: &J, parent: &J, spec: &J) -> Result<()> {
    validate_policy(parent, spec)?;
    ensure(
        parent["publication_preparation"]["spec"] == *spec,
        "remote publication must use its host-prepared frozen request",
    )?;
    let child = &state["tasks"][field(spec, "publication")?];
    ensure(
        child["lineage"]["requirement"] == parent["task"],
        "publication child requirement changed",
    )?;
    validate_child(state, child, spec)
}

fn validate_child(state: &J, child: &J, spec: &J) -> Result<()> {
    ensure(
        child["lineage"]["role"] == "publication" && child["lineage"]["delivery"] == spec["id"],
        "publication child delivery binding changed",
    )?;
    lineage::ensure_current(state, child)?;
    let record = &child["pr"];
    for key in ["host", "repo", "base_ref"] {
        ensure(
            record[key] == spec["policy"][key],
            "publication PR target changed",
        )?;
    }
    ensure(
        record["head"] == child["candidate"],
        "publication PR candidate changed",
    )
}

/// Recheck the existing cross-task ownership/target slot on the final locked snapshot.
pub fn revalidate_slot(state: &J, child: &J) -> Result<()> {
    lineage::ensure_current(state, child)?;
    crate::progress::ownership::publication(state, child, &child["pr"])?;
    crate::progress::ownership::publication_slot(state, child, &child["pr"])
}

/// Persist uncertainty before any remote side effect. This never submits.
pub fn reserve_remote(env: &mut TaskEnvironment) -> Result<J> {
    let _permit = crate::task_env::isolation::mutation_permit(env)?;
    ensure(
        env.delivery["request"]["adapter"] == "github-pr",
        "remote delivery journal required",
    )?;
    if env.delivery["phase"] == "unknown" {
        return recover_reservation(env);
    }
    validate_execution(env)?;
    reserve_execution(env)?;
    Ok(env.delivery.clone())
}

fn recover_reservation(env: &TaskEnvironment) -> Result<J> {
    ensure_owned(env)?;
    ensure(
        env.state == EnvironmentState::Active,
        "publication environment is no longer active",
    )?;
    let queue = PathBuf::from(field(&env.delivery, "queue")?);
    recover_frozen_queue(env, &queue)?;
    Ok(env.delivery.clone())
}

fn recover_frozen_queue(env: &TaskEnvironment, queue: &Path) -> Result<()> {
    let _lock = crate::queue::locked(queue, false)?;
    let mut state = crate::queue::load_queue(queue)?;
    validate_freeze(&state, &env.delivery)?;
    let parent = &state["tasks"][&env.task_id];
    if parent["control"] == "delivering" && parent["delivery"] == env.delivery["id"] {
        return Ok(());
    }
    freeze(&mut state, env)?;
    crate::queue::save(queue, &mut state)
}

/// Observe a real PR landing after the caller satisfies the frozen CI policy.
/// A confirmed host receipt releases this publication's target slot.
pub fn confirm_remote(env: &mut TaskEnvironment, queue: &Path, api: &dyn GitHubApi) -> Result<J> {
    ensure(
        env.delivery["queue"] == json!(queue),
        "publication queue changed",
    )?;
    let state = queue_snapshot(queue)?;
    validate_freeze(&state, &env.delivery)?;
    let observer = Observer { state: &state, api };
    observer.preflight(&env.delivery)?;
    let result = super::super::reconcile(env, &observer)?;
    if result["phase"] == "confirmed" {
        save_confirmation(env, queue)?;
    }
    Ok(result)
}

fn save_confirmation(env: &TaskEnvironment, queue: &Path) -> Result<()> {
    let _lock = crate::queue::locked(queue, false)?;
    let mut state = confirmation_state(env, queue)?;
    let name = field(&env.delivery["request"], "publication")?;
    let mut child = state["tasks"][&name].clone();
    let record = json!({"id":env.delivery["id"],"phase":"confirmed",
        "task":child["task"],"requirement":env.task_id,"receipt":env.delivery["receipt"]});
    validate_confirmation(&child, &record)?;
    if child["publication_delivery"] == record {
        return Ok(());
    }
    child["publication_delivery"] = record;
    crate::pr::store(
        &mut state,
        child,
        util::now(),
        "publication_delivery_confirmed",
    )?;
    crate::queue::save(queue, &mut state)
}

fn confirmation_state(env: &TaskEnvironment, queue: &Path) -> Result<J> {
    let state = crate::queue::load_queue(queue)?;
    validate_confirmation_owner(&state, env)?;
    validate_freeze(&state, &env.delivery)?;
    Ok(state)
}

fn validate_confirmation(child: &J, record: &J) -> Result<()> {
    ensure(
        crate::progress::ownership::confirmed_landing(&child["pr"]),
        "publication confirmation requires the recorded actual landing",
    )?;
    ensure(
        lineage::publication::receipt_matches(&record["receipt"], &child["pr"]["landing"]),
        "publication receipt differs from the actual landing",
    )?;
    ensure(
        child["publication_delivery"].is_null() || child["publication_delivery"] == *record,
        "publication delivery confirmation is immutable",
    )
}

fn validate_confirmation_owner(state: &J, env: &TaskEnvironment) -> Result<()> {
    ensure(
        env.delivery["task"] == env.task_id
            && env.delivery["instance"] == env.instance_id
            && env.delivery["id"] == env.delivery["request"]["id"],
        "publication confirmation owner changed",
    )?;
    let parent = &state["tasks"][&env.task_id];
    ensure(
        parent["control"] == "delivering" && parent["delivery"] == env.delivery["id"],
        "publication confirmation requires its frozen parent",
    )
}

struct Observer<'a> {
    state: &'a J,
    api: &'a dyn GitHubApi,
}

impl Executor for Observer<'_> {
    fn preflight(&self, intent: &J) -> Result<()> {
        validate_target(&intent["request"])?;
        ensure(
            intent["endpoint"] == intent["request"]["target"],
            "remote delivery endpoint changed",
        )
    }
    fn execute(&self, _intent: &J) -> Result<()> {
        Err(FlowError::progress(
            "remote submission belongs to the host publisher transport",
        ))
    }
    fn observe(&self, intent: &J) -> Result<Observation> {
        let parent = &self.state["tasks"][field(intent, "task")?];
        validate_parent(self.state, parent, &intent["request"])?;
        let child = &self.state["tasks"][field(&intent["request"], "publication")?];
        verify(
            self.state,
            parent,
            child,
            &intent["artifact"],
            Some(self.api),
        )
    }
}
