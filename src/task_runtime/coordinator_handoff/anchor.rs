//! Historical load is pure; first admission additionally verifies the original
//! protected runtime/guard journals, without acquiring SH inside a caller's EX.
use super::*;

fn terminal(task: &J) -> bool {
    !task["final_outcome"].is_null() || !task["cleanup_obligation"].is_null()
}

pub(super) fn open(task: &J) -> Result<()> {
    ensure(
        !task.is_null() && !terminal(task),
        "cannot add or advance a handoff on a terminal parent",
    )
}

fn scope(queue: &J, target: &J) -> Result<()> {
    ensure(
        queue["guard_dir"] == target["guard"] && queue["receiver"] == target["scope"],
        "coordinator handoff is not anchored to the queue receiver",
    )
}

pub(super) fn current(queue: &J, task: &J, last: &J) -> Result<()> {
    if terminal(task) {
        return Ok(());
    }
    let target = if last["binding"].is_null() {
        &last["from"]
    } else {
        &last["to"]
    };
    scope(queue, target)
}

pub(super) fn initial(before: &J, after: &J, name: &str, record: &J) -> Result<()> {
    open(&before["tasks"][name])?;
    open(&after["tasks"][name])?;
    scope(before, &record["from"])?;
    scope(after, &record["from"])?;
    let owner: Owner = serde_json::from_value(after["tasks"][name][KEY]["owner"].clone())?;
    original_runtime(record, &owner)
}

fn protected(root: &Path, owner: &Owner) -> Result<()> {
    ensure(
        util::protected::is_runtime_namespace(root)?,
        "handoff runtime namespace is not protected",
    )?;
    ensure(
        util::read_state_json(root, "lifecycle-owner.json")? == json!(owner),
        "handoff runtime namespace Owner changed",
    )
}

fn original_runtime(record: &J, owner: &Owner) -> Result<()> {
    let from = &record["from"];
    let root = path(from, "runtime")?;
    protected(&root, owner)?;
    let runtime = util::read_state_json_bounded(&root, "runtime.json", RECORD_BYTES)?;
    runtime_identity(&runtime, record, owner)?;
    original_guard(&root, record, owner)?;
    original_session(&root, record)?;
    original_recovery(record, owner)
}

fn runtime_identity(runtime: &J, record: &J, owner: &Owner) -> Result<()> {
    ensure(
        runtime["lifecycle_owner"] == json!(owner)
            && runtime["instance"] == owner.instance_id
            && runtime["root"] == record["from"]["runtime"],
        "original coordinator runtime Owner changed",
    )?;
    ensure(
        runtime["adapter"] == "codex" && runtime["configuration"] == "upstream",
        "handoff requires the original upstream Codex runtime",
    )?;
    let config = &record["configuration"];
    for (stored, frozen) in [
        ("model", "model"),
        ("cwd", "cwd"),
        ("upstream_socket", "upstream_endpoint"),
        ("role_models", "role_models"),
    ] {
        ensure(
            runtime[stored] == config[frozen],
            "handoff configuration differs from original runtime",
        )?;
    }
    fixed_runtime_options(runtime, config)
}

fn fixed_runtime_options(runtime: &J, config: &J) -> Result<()> {
    let approval = runtime
        .get("approval_policy")
        .cloned()
        .unwrap_or(json!("untrusted"));
    let driver = runtime
        .get("driver_binary")
        .cloned()
        .unwrap_or(json!(std::env::current_exe()?));
    ensure(
        config["approval"] == approval && config["driver"] == driver,
        "handoff original approval policy or driver changed",
    )
}

fn original_guard(root: &Path, record: &J, owner: &Owner) -> Result<()> {
    let from = &record["from"];
    let guard = path(from, "guard")?;
    protected(&guard, owner)?;
    let state = util::read_state_json_bounded(&guard, "state.json", RECORD_BYTES)?;
    ensure(
        crate::guard::scope(&state) == from["scope"]
            && state["session"] == from["session"]
            && state["control_protocol"] == "queue-tool-v2",
        "original coordinator guard identity changed",
    )?;
    ensure(
        state["approval_policy"] == record["configuration"]["approval"],
        "original coordinator guard permissions changed",
    )?;
    receiver_journal(root, record)
}

fn receiver_journal(root: &Path, record: &J) -> Result<()> {
    let from = &record["from"];
    let receiver = util::read_state_json_bounded(root, "receiver.json", RECORD_BYTES)?;
    ensure(
        receiver["phase"] == "bound"
            && receiver["guard_dir"] == from["guard"]
            && receiver["state"]["approval_policy"] == record["configuration"]["approval"]
            && crate::guard::scope(&receiver["state"]) == from["scope"],
        "original coordinator receiver journal changed",
    )
}

fn original_session(root: &Path, record: &J) -> Result<()> {
    let journal = util::read_state_json_bounded(root, "upstream-session.json", RECORD_BYTES)?;
    ensure(
        journal["phase"] == "created" && journal["session"] == record["from"]["session"],
        "original coordinator session journal changed",
    )?;
    ensure(
        journal["session_configuration"]
            == record["configuration"]["business_session_configuration"],
        "handoff business configuration differs from original live configuration",
    )
}

fn original_recovery(record: &J, owner: &Owner) -> Result<()> {
    if record["recovery"].is_null() {
        return Ok(());
    }
    let root = path(&record["recovery"], "observer")?;
    protected(&root, owner)?;
    let context = util::read_state_json_bounded(&root, "observer.json", RECORD_BYTES)?;
    ensure(
        context["owner"] == json!(owner)
            && context["observer"] == json!(root)
            && context["coordinator"] == record["from"]["runtime"],
        "original Recovery observer authority changed",
    )
}
