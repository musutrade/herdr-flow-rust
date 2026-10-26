//! Command-line routing for the wait queue.

use std::os::fd::RawFd;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{json, Value};

use crate::error::{ErrorKind, FlowError, Result};
use crate::herdr::ReceiverFactory;
use crate::model::{self, J};
use crate::progress;
use crate::runtime;
use crate::util;
use crate::watch_service;

use super::{queue_error, run_command, summary, validate_watch};

const ACTIONS: &[&str] = &[
    "init",
    "init-isolated",
    "add",
    "signal",
    "status",
    "cleanup-preflight",
    "metrics",
    "watch",
    "enable",
    "disable",
    "ack",
    "close",
    "stop",
    "resolve",
    "run",
    "readiness",
    "task-add",
    "task-environment",
    "task-finalize",
    "task-authorize",
    "task-lineage",
    "task-apply",
    "task-begin",
    "task-sent",
    "task-accept",
    "task-control",
    "task-resume",
    "task-release",
    "event-current",
    "pr-prepare",
    "pr-begin",
    "pr-reconcile",
    "pr-observe",
    "pr-update",
    "batch-init",
    "batch-offer",
    "batch-freeze",
    "batch-integrate",
    "batch-snapshot",
    "batch-gate-begin",
    "batch-gate-record",
    "batch-ci-observe",
    "batch-reconcile",
    "batch-repair",
    "batch-repair-finish",
    "batch-rebase",
];

#[derive(Default)]
struct Args {
    action: String,
    state_dir: PathBuf,
    guard_state_dir: Option<PathBuf>,
    poll_seconds: u64,
    spec: Option<PathBuf>,
    wait: Option<String>,
    outcome: Option<String>,
    command_file: Option<PathBuf>,
    cwd: Option<PathBuf>,
    resolution: Option<String>,
    reason: Option<String>,
    summary: Option<String>,
    evidence: Option<String>,
    delivery: Option<String>,
    event: Option<String>,
    json: bool,
    once: bool,
    background: bool,
    ready_fd: Option<RawFd>,
}

fn parse_value(args: &[String], index: &mut usize) -> Result<String> {
    *index += 1;
    args.get(*index)
        .cloned()
        .ok_or_else(|| FlowError::usage("missing option value"))
}

type ArgHandler = fn(&[String], &mut usize, &mut Args) -> Result<()>;

const ARG_HANDLERS: &[(&str, ArgHandler)] = &[
    ("--state-dir", flag_state_dir),
    ("--guard-state-dir", flag_guard_state_dir),
    ("--poll-seconds", flag_poll_seconds),
    ("--spec", flag_spec),
    ("--wait", flag_wait),
    ("--outcome", flag_outcome),
    ("--command-file", flag_command_file),
    ("--cwd", flag_cwd),
    ("--resolution", flag_resolution),
    ("--reason", flag_reason),
    ("--summary", flag_summary),
    ("--evidence", flag_evidence),
    ("--delivery", flag_delivery),
    ("--event", flag_event),
    ("--json", flag_json),
    ("--once", flag_once),
    ("--background", flag_background),
    ("--ready-fd", flag_ready_fd),
    ("-h", flag_help),
    ("--help", flag_help),
];

fn flag_handler(argument: &str) -> Option<ArgHandler> {
    ARG_HANDLERS
        .iter()
        .find(|(name, _)| *name == argument)
        .map(|(_, handler)| *handler)
}

fn path_value(args: &[String], index: &mut usize) -> Result<PathBuf> {
    Ok(PathBuf::from(parse_value(args, index)?))
}

fn u64_value(args: &[String], index: &mut usize, message: &str) -> Result<u64> {
    let value = parse_value(args, index)?;
    value.parse().map_err(|_| FlowError::usage(message))
}

fn i32_value(args: &[String], index: &mut usize, message: &str) -> Result<i32> {
    let value = parse_value(args, index)?;
    value.parse().map_err(|_| FlowError::usage(message))
}

fn flag_state_dir(args: &[String], index: &mut usize, parsed: &mut Args) -> Result<()> {
    parsed.state_dir = path_value(args, index)?;
    Ok(())
}

fn flag_guard_state_dir(args: &[String], index: &mut usize, parsed: &mut Args) -> Result<()> {
    parsed.guard_state_dir = Some(path_value(args, index)?);
    Ok(())
}

fn flag_poll_seconds(args: &[String], index: &mut usize, parsed: &mut Args) -> Result<()> {
    parsed.poll_seconds = u64_value(args, index, "invalid --poll-seconds")?;
    Ok(())
}

fn flag_spec(args: &[String], index: &mut usize, parsed: &mut Args) -> Result<()> {
    parsed.spec = Some(path_value(args, index)?);
    Ok(())
}

fn flag_wait(args: &[String], index: &mut usize, parsed: &mut Args) -> Result<()> {
    parsed.wait = Some(parse_value(args, index)?);
    Ok(())
}

fn flag_outcome(args: &[String], index: &mut usize, parsed: &mut Args) -> Result<()> {
    parsed.outcome = Some(parse_value(args, index)?);
    Ok(())
}

fn flag_command_file(args: &[String], index: &mut usize, parsed: &mut Args) -> Result<()> {
    parsed.command_file = Some(path_value(args, index)?);
    Ok(())
}

fn flag_cwd(args: &[String], index: &mut usize, parsed: &mut Args) -> Result<()> {
    parsed.cwd = Some(path_value(args, index)?);
    Ok(())
}

fn flag_resolution(args: &[String], index: &mut usize, parsed: &mut Args) -> Result<()> {
    parsed.resolution = Some(parse_value(args, index)?);
    Ok(())
}

fn flag_reason(args: &[String], index: &mut usize, parsed: &mut Args) -> Result<()> {
    parsed.reason = Some(parse_value(args, index)?);
    Ok(())
}

fn flag_summary(args: &[String], index: &mut usize, parsed: &mut Args) -> Result<()> {
    parsed.summary = Some(parse_value(args, index)?);
    Ok(())
}

fn flag_evidence(args: &[String], index: &mut usize, parsed: &mut Args) -> Result<()> {
    parsed.evidence = Some(parse_value(args, index)?);
    Ok(())
}

fn flag_delivery(args: &[String], index: &mut usize, parsed: &mut Args) -> Result<()> {
    parsed.delivery = Some(parse_value(args, index)?);
    Ok(())
}

fn flag_event(args: &[String], index: &mut usize, parsed: &mut Args) -> Result<()> {
    parsed.event = Some(parse_value(args, index)?);
    Ok(())
}

fn flag_json(_args: &[String], _index: &mut usize, parsed: &mut Args) -> Result<()> {
    parsed.json = true;
    Ok(())
}

fn flag_once(_args: &[String], _index: &mut usize, parsed: &mut Args) -> Result<()> {
    parsed.once = true;
    Ok(())
}

fn flag_background(_args: &[String], _index: &mut usize, parsed: &mut Args) -> Result<()> {
    parsed.background = true;
    Ok(())
}

fn flag_ready_fd(args: &[String], index: &mut usize, parsed: &mut Args) -> Result<()> {
    parsed.ready_fd = Some(i32_value(args, index, "invalid --ready-fd")?);
    Ok(())
}

fn flag_help(_args: &[String], _index: &mut usize, _parsed: &mut Args) -> Result<()> {
    Err(FlowError::usage("help requested"))
}

fn parse_positional(argument: &str, parsed: &mut Args) -> Result<()> {
    if argument.starts_with('-') {
        return Err(FlowError::usage(format!(
            "unrecognized argument: {argument}"
        )));
    }
    set_action(argument, parsed)
}

fn set_action(argument: &str, parsed: &mut Args) -> Result<()> {
    if !parsed.action.is_empty() {
        return Err(FlowError::usage(format!(
            "unrecognized argument: {argument}"
        )));
    }
    parsed.action = argument.to_string();
    Ok(())
}

fn parse_one(args: &[String], index: &mut usize, parsed: &mut Args, argument: &str) -> Result<()> {
    match flag_handler(argument) {
        Some(handler) => handler(args, index, parsed),
        _ => parse_positional(argument, parsed),
    }
}

fn parse_all(args: &[String], index: &mut usize, parsed: &mut Args) -> Result<()> {
    match args.get(*index) {
        Some(argument) => parse_next(args, index, parsed, argument),
        _ => Ok(()),
    }
}

fn parse_next(args: &[String], index: &mut usize, parsed: &mut Args, argument: &str) -> Result<()> {
    parse_one(args, index, parsed, argument)?;
    *index += 1;
    parse_all(args, index, parsed)
}

fn parse_args(args: &[String]) -> Result<Args> {
    let mut parsed = Args {
        poll_seconds: 30,
        ..Default::default()
    };
    let mut index = 0;
    parse_all(args, &mut index, &mut parsed)?;
    validate_args(parsed)
}

fn ensure_action_present(parsed: &Args) -> Result<()> {
    if parsed.action.is_empty() {
        return Err(FlowError::usage("an action is required"));
    }
    Ok(())
}

fn ensure_action_known(parsed: &Args) -> Result<()> {
    if !ACTIONS.contains(&parsed.action.as_str()) {
        return Err(FlowError::usage(format!(
            "invalid action: {}",
            parsed.action
        )));
    }
    Ok(())
}

fn validate_action(parsed: &Args) -> Result<()> {
    ensure_action_present(parsed)?;
    ensure_action_known(parsed)
}

fn ensure_state_dir(parsed: &Args) -> Result<()> {
    if parsed.state_dir.as_os_str().is_empty() {
        return Err(FlowError::usage("--state-dir is required"));
    }
    Ok(())
}

type OptionsValidator = fn(&Args) -> Result<()>;

const ACTION_VALIDATORS: &[(&str, OptionsValidator)] = &[
    ("signal", validate_signal_options),
    ("ack", validate_ack_options),
    ("resolve", validate_resolve_options),
    ("run", validate_run_options),
];

fn validate_named_options(parsed: &Args) -> Result<()> {
    let validator = ACTION_VALIDATORS
        .iter()
        .find(|(name, _)| *name == parsed.action.as_str())
        .map(|(_, validator)| *validator);
    match validator {
        Some(validator) => validator(parsed),
        _ => Ok(()),
    }
}

fn ensure_present<T>(value: Option<&T>, message: &str) -> Result<()> {
    if value.is_none() {
        return Err(FlowError::usage(message));
    }
    Ok(())
}

fn validate_signal_options(parsed: &Args) -> Result<()> {
    ensure_present(parsed.wait.as_ref(), "signal requires --wait")?;
    validate_signal_outcome(parsed)
}

fn validate_signal_outcome(parsed: &Args) -> Result<()> {
    ensure_present(parsed.outcome.as_ref(), "signal requires --outcome")?;
    ensure_present(parsed.summary.as_ref(), "signal requires --summary")
}

fn validate_ack_options(parsed: &Args) -> Result<()> {
    ensure_present(parsed.delivery.as_ref(), "ack requires --delivery")
}

fn validate_resolve_options(parsed: &Args) -> Result<()> {
    ensure_present(parsed.delivery.as_ref(), "resolve requires --delivery")?;
    validate_resolve_extra(parsed)
}

fn validate_resolve_extra(parsed: &Args) -> Result<()> {
    ensure_present(parsed.resolution.as_ref(), "resolve requires --resolution")?;
    ensure_present(parsed.reason.as_ref(), "resolve requires --reason")
}

fn validate_run_options(parsed: &Args) -> Result<()> {
    match (
        parsed.wait.is_none(),
        parsed.command_file.is_none(),
        parsed.cwd.is_none(),
    ) {
        (false, false, false) => Ok(()),
        _ => Err(FlowError::usage(
            "run requires --wait, --command-file and --cwd",
        )),
    }
}

fn spec_missing(parsed: &Args) -> bool {
    parsed.spec.is_none() && parsed.action != "event-current"
}

fn spec_required(parsed: &Args) -> bool {
    parsed.action_requires_spec() && spec_missing(parsed)
}

fn validate_spec_option(parsed: Args) -> Result<Args> {
    if spec_required(&parsed) {
        return Err(FlowError::usage("--spec required"));
    }
    Ok(parsed)
}

fn validate_action_options(parsed: Args) -> Result<Args> {
    validate_named_options(&parsed)?;
    validate_spec_option(parsed)
}

fn validate_common(parsed: Args) -> Result<Args> {
    ensure_state_dir(&parsed)?;
    validate_action_options(parsed)
}

fn validate_args(parsed: Args) -> Result<Args> {
    validate_action(&parsed)?;
    validate_common(parsed)
}

impl Args {
    fn action_requires_spec(&self) -> bool {
        self.action == "cleanup-preflight"
            || self.action.starts_with("task-")
            || action_is_pr_or_batch(&self.action)
    }
}

fn action_is_pr_or_batch(action: &str) -> bool {
    action.starts_with("pr-") || action.starts_with("batch-")
}

fn read_spec(path: &Path) -> Result<J> {
    util::read_json(path)
}

fn print_json(value: &J) -> Result<()> {
    println!(
        "{}",
        serde_json::to_string_pretty(value)
            .map_err(|error| FlowError::new(ErrorKind::Json, error.to_string()))?
    );
    Ok(())
}

fn print_state(s: &J, as_json: bool) -> Result<()> {
    if as_json {
        print_json(s)
    } else {
        println!("{}", summary(s));
        Ok(())
    }
}

struct Outcome {
    state: Option<J>,
    code: Option<i32>,
    printed: bool,
}

impl Outcome {
    fn state(state: J) -> Self {
        Self {
            state: Some(state),
            code: None,
            printed: false,
        }
    }

    fn code(code: i32) -> Self {
        Self {
            state: None,
            code: Some(code),
            printed: true,
        }
    }
}

fn linked_task_open(s: &J, wait_id: &str) -> bool {
    model::get(s, "tasks")
        .and_then(Value::as_object)
        .map(|tasks| tasks.values().any(|task| linked_task_blocks(task, wait_id)))
        .unwrap_or(false)
}

fn linked_task_blocks(task: &J, wait_id: &str) -> bool {
    model::string(task, "wait").as_deref() == Some(wait_id) && task_open(task)
}

fn task_open(task: &J) -> bool {
    model::string(task, "phase").as_deref() != Some("completed")
        && model::string(task, "control").as_deref() != Some("cancelled")
}

fn ensure_no_linked_task(s: &J, wait_id: &str) -> Result<()> {
    if linked_task_open(s, wait_id) {
        return Err(queue_error(
            "apply or cancel the linked task before closing its current wait",
        ));
    }
    Ok(())
}

fn event_mentions_wait(s: &J, event_id: &J, wait_id: &str) -> bool {
    event_id
        .as_str()
        .and_then(|id| model::dig(s, &["events", id]))
        .and_then(|event| model::string(event, "wait"))
        .as_deref()
        == Some(wait_id)
}

fn active_delivery_covers(s: &J, active: &str, wait_id: &str) -> bool {
    let Some(delivery) = model::dig(s, &["deliveries", active]) else {
        return false;
    };
    model::array_field(delivery, "events")
        .map(|events| {
            events
                .iter()
                .any(|event_id| event_mentions_wait(s, event_id, wait_id))
        })
        .unwrap_or(false)
}

fn active_notification_covers(s: &J, wait_id: &str) -> bool {
    let Some(active) = model::string(s, "active") else {
        return false;
    };
    active_delivery_covers(s, &active, wait_id)
}

fn ensure_no_active_notification(s: &J, wait_id: &str) -> Result<()> {
    if active_notification_covers(s, wait_id) {
        return Err(queue_error(
            "ACK or resolve the active notification before closing this wait",
        ));
    }
    Ok(())
}

fn close_wait_mark(s: &mut J, wait_id: &str) -> Result<()> {
    let Some(wait) = model::dig_mut(s, &["waits", wait_id]) else {
        return Err(queue_error("wait is not registered"));
    };
    wait["closed"] = json!(true);
    Ok(())
}

fn event_closes(event: &J, wait_id: &str) -> bool {
    model::string(event, "wait").as_deref() == Some(wait_id)
        && !model::bool_or(event, "acknowledged", false)
}

fn close_wait_event(event: &mut J, wait_id: &str) {
    if event_closes(event, wait_id) {
        event["acknowledged"] = json!(true);
        event["withdrawn"] = json!(true);
    }
}

fn close_wait_event_values(events: &mut serde_json::Map<String, Value>, wait_id: &str) {
    for event in events.values_mut() {
        close_wait_event(event, wait_id);
    }
}

fn close_wait_events(s: &mut J, wait_id: &str) {
    if let Some(events) = model::get_mut(s, "events").and_then(Value::as_object_mut) {
        close_wait_event_values(events, wait_id);
    }
}

fn close_wait_save(root: &Path, mut s: J) -> Result<J> {
    super::save(root, &mut s)?;
    Ok(s)
}

fn close_wait_apply(root: &Path, mut s: J, wait_id: &str) -> Result<J> {
    close_wait_mark(&mut s, wait_id)?;
    close_wait_events(&mut s, wait_id);
    close_wait_save(root, s)
}

fn close_wait_notification(root: &Path, s: J, wait_id: &str) -> Result<J> {
    ensure_no_active_notification(&s, wait_id)?;
    close_wait_apply(root, s, wait_id)
}

fn close_wait_checked(root: &Path, s: J, wait_id: &str) -> Result<J> {
    ensure_no_linked_task(&s, wait_id)?;
    close_wait_notification(root, s, wait_id)
}

fn close_wait(root: &Path, wait_id: &str) -> Result<J> {
    let s = super::load_queue(root)?;
    close_wait_checked(root, s, wait_id)
}

fn has_active_delivery(s: &J) -> bool {
    model::get(s, "active")
        .map(|value| model::truthy(Some(value)))
        .unwrap_or(false)
}

fn ensure_no_active_delivery(s: &J) -> Result<()> {
    if has_active_delivery(s) {
        return Err(queue_error(
            "resolve or ACK in-flight delivery before stopping",
        ));
    }
    Ok(())
}

fn has_open_tasks(s: &J) -> bool {
    model::get(s, "tasks")
        .and_then(Value::as_object)
        .map(|tasks| tasks.values().any(task_open))
        .unwrap_or(false)
}

fn ensure_no_open_tasks(s: &J) -> Result<()> {
    if has_open_tasks(s) {
        return Err(queue_error(
            "complete or explicitly cancel linked tasks before stopping",
        ));
    }
    Ok(())
}

fn ensure_stop_allowed(s: &J) -> Result<()> {
    ensure_no_active_delivery(s)?;
    ensure_no_open_tasks(s)
}

fn stop_queue_save(root: &Path, mut s: J) -> Result<J> {
    s["stopped"] = json!(true);
    super::save(root, &mut s)?;
    Ok(s)
}

fn stop_queue(root: &Path) -> Result<J> {
    let s = super::load_queue(root)?;
    stop_queue_checked(root, s)
}

fn stop_queue_checked(root: &Path, s: J) -> Result<J> {
    ensure_stop_allowed(&s)?;
    stop_queue_save(root, s)
}

fn factory_for<'a>() -> impl Fn(&J) -> Result<Box<dyn crate::herdr::ReceiverAdapter + 'a>> {
    |state: &J| crate::herdr::receiver_factory(state)
}

fn readiness_code(result: &J) -> i32 {
    if model::bool_or(result, "active_wakeup", false) {
        0
    } else {
        1
    }
}

fn action_init(root: &Path, args: &Args, _factory: &ReceiverFactory) -> Result<Outcome> {
    Ok(Outcome::state(super::init(
        root,
        args.guard_state_dir.as_deref(),
        args.poll_seconds,
    )?))
}

fn action_init_isolated(root: &Path, args: &Args, _factory: &ReceiverFactory) -> Result<Outcome> {
    Ok(Outcome::state(super::init_isolated(
        root,
        args.guard_state_dir.as_deref(),
        args.poll_seconds,
    )?))
}

fn action_readiness(root: &Path, args: &Args, factory: &ReceiverFactory) -> Result<Outcome> {
    if let Some(path) = &args.spec {
        let result = runtime::role::check(root, &util::read_json(path)?)?;
        return action_readiness_print(result);
    }
    let session = crate::identity::receiver_session(root)?;
    let result = runtime::readiness(root, session.as_deref(), factory)?;
    action_readiness_report(root, result)
}

fn action_readiness_report(root: &Path, result: J) -> Result<Outcome> {
    util::atomic_write_json(&super::artifacts(root)?.join("readiness.json"), &result)?;
    action_readiness_print(result)
}

fn action_readiness_print(result: J) -> Result<Outcome> {
    print_json(&result)?;
    Ok(Outcome::code(readiness_code(&result)))
}

fn status_output(s: &J, as_json: bool) -> Result<Outcome> {
    if as_json {
        return status_json(s);
    }
    println!("{}", summary(s));
    Ok(Outcome::code(0))
}

fn status_json(s: &J) -> Result<Outcome> {
    let mut output = s.clone();
    output["diagnostics"] = super::diagnostics(s, None)?;
    status_json_print(&output)
}

fn status_json_print(output: &J) -> Result<Outcome> {
    print_json(output)?;
    Ok(Outcome::code(0))
}

fn action_status(root: &Path, args: &Args, _factory: &ReceiverFactory) -> Result<Outcome> {
    let s = super::load_queue(root)?;
    status_output(&s, args.json)
}

fn event_current_code(current: bool) -> i32 {
    if current {
        0
    } else {
        1
    }
}

fn event_current_output(event_id: &str, current: bool) -> Result<Outcome> {
    print_json(&json!({"event": event_id, "current": current}))?;
    Ok(Outcome::code(event_current_code(current)))
}

fn event_current_print(s: &J, event_id: &str, event: &J) -> Result<Outcome> {
    let current = progress::current_event(s, event);
    event_current_output(event_id, current)
}

fn event_current_loaded(s: &J, event_id: &str) -> Result<Outcome> {
    let Some(event) = model::dig(s, &["events", event_id]).cloned() else {
        return Err(queue_error("event is not registered"));
    };
    event_current_print(s, event_id, &event)
}

fn event_current(root: &Path, event_id: &str) -> Result<Outcome> {
    let s = super::load_queue(root)?;
    event_current_loaded(&s, event_id)
}

fn action_event_current(root: &Path, args: &Args, _factory: &ReceiverFactory) -> Result<Outcome> {
    let Some(event_id) = args.event.as_deref() else {
        return Err(queue_error("event-current requires --event"));
    };
    event_current(root, event_id)
}

fn read_required_spec(args: &Args) -> Result<J> {
    let path = args
        .spec
        .as_deref()
        .ok_or_else(|| FlowError::usage("--spec required"))?;
    read_spec(path)
}

fn add_state(root: &Path) -> Result<Outcome> {
    Ok(Outcome::state(super::load_queue(root)?))
}

fn add_spec(root: &Path, spec: &J) -> Result<Outcome> {
    super::add(root, spec)?;
    add_state(root)
}

fn action_add(root: &Path, args: &Args, _factory: &ReceiverFactory) -> Result<Outcome> {
    let spec = read_required_spec(args)?;
    add_spec(root, &spec)
}

fn signal_outcome(root: &Path) -> Result<Outcome> {
    Ok(Outcome::state(super::load_queue(root)?))
}

fn action_signal(root: &Path, args: &Args, _factory: &ReceiverFactory) -> Result<Outcome> {
    super::signal(
        root,
        args.wait.as_deref().unwrap_or_default(),
        args.outcome.as_deref().unwrap_or_default(),
        args.summary.as_deref().unwrap_or_default(),
        args.evidence.as_deref(),
    )?;
    signal_outcome(root)
}

fn action_resolve(root: &Path, args: &Args, _factory: &ReceiverFactory) -> Result<Outcome> {
    let state = super::resolve(
        root,
        args.delivery.as_deref().unwrap_or_default(),
        args.resolution.as_deref().unwrap_or_default(),
        args.reason.as_deref().unwrap_or_default(),
    )?;
    Ok(Outcome::state(state))
}

fn action_ack(root: &Path, args: &Args, _factory: &ReceiverFactory) -> Result<Outcome> {
    let Some(session) = crate::identity::receiver_session(root)?.filter(|value| !value.is_empty())
    else {
        return Err(queue_error(
            "ACK requires the registered receiving session and --delivery",
        ));
    };
    let state = super::acknowledge(root, args.delivery.as_deref().unwrap_or_default(), &session)?;
    Ok(Outcome::state(state))
}

fn enable_state(root: &Path, s: &mut J, factory: &ReceiverFactory) -> Result<Outcome> {
    super::enable(root, s, factory)?;
    Ok(Outcome::state(s.clone()))
}

fn action_enable(root: &Path, _args: &Args, factory: &ReceiverFactory) -> Result<Outcome> {
    let mut s = super::load_queue(root)?;
    enable_state(root, &mut s, factory)
}

fn disable_state(root: &Path, s: &mut J) -> Result<Outcome> {
    s["notify"] = json!(false);
    super::save(root, s)?;
    Ok(Outcome::state(s.clone()))
}

fn action_disable(root: &Path, _args: &Args, _factory: &ReceiverFactory) -> Result<Outcome> {
    let mut s = super::load_queue(root)?;
    disable_state(root, &mut s)
}

fn action_close(root: &Path, args: &Args, _factory: &ReceiverFactory) -> Result<Outcome> {
    let Some(wait_id) = args.wait.as_deref() else {
        return Err(queue_error("--wait required"));
    };
    Ok(Outcome::state(close_wait(root, wait_id)?))
}

fn action_stop(root: &Path, _args: &Args, _factory: &ReceiverFactory) -> Result<Outcome> {
    Ok(Outcome::state(stop_queue(root)?))
}

fn task_action_state(
    root: &Path,
    args: &Args,
    spec: &J,
    factory: &ReceiverFactory,
) -> Result<Outcome> {
    let session = if args.action == "task-accept" {
        let queue = super::load_queue(root)?;
        let task = &queue["tasks"][spec["task"].as_str().unwrap_or_default()];
        let op = &task["operations"][spec["operation"].as_str().unwrap_or_default()];
        let identity = json!({"adapter":op.get("adapter").cloned().unwrap_or(json!("codex")),"session":op["session"]});
        Some(crate::identity::environment(&identity)?)
    } else {
        crate::identity::receiver_session(root)?
    };
    let state =
        super::finalization::task_action(root, &args.action, spec, session.as_deref(), factory)?;
    Ok(Outcome::state(state))
}

fn action_task(root: &Path, args: &Args, factory: &ReceiverFactory) -> Result<Outcome> {
    let spec = read_required_spec(args)?;
    task_action_state(root, args, &spec, factory)
}

fn action_default(root: &Path) -> Result<Outcome> {
    Ok(Outcome::state(super::load_queue(root)?))
}

type ActionHandler = fn(&Path, &Args, &ReceiverFactory) -> Result<Outcome>;

const ACTION_HANDLERS: &[(&str, ActionHandler)] = &[
    ("init", action_init),
    ("init-isolated", action_init_isolated),
    ("readiness", action_readiness),
    ("status", action_status),
    ("event-current", action_event_current),
    ("add", action_add),
    ("signal", action_signal),
    ("resolve", action_resolve),
    ("ack", action_ack),
    ("enable", action_enable),
    ("disable", action_disable),
    ("close", action_close),
    ("stop", action_stop),
    ("task-add", action_task),
    ("task-environment", action_task),
    ("task-finalize", action_task),
    ("task-authorize", action_task),
    ("task-lineage", action_task),
    ("task-apply", action_task),
    ("task-begin", action_task),
    ("task-sent", action_task),
    ("task-accept", action_task),
    ("task-control", action_task),
    ("task-resume", action_task),
    ("task-release", action_task),
    ("pr-prepare", action_task),
    ("pr-begin", action_task),
    ("pr-reconcile", action_task),
    ("pr-observe", action_task),
    ("pr-update", action_task),
    ("batch-init", action_task),
    ("batch-offer", action_task),
    ("batch-freeze", action_task),
    ("batch-integrate", action_task),
    ("batch-snapshot", action_task),
    ("batch-gate-begin", action_task),
    ("batch-gate-record", action_task),
    ("batch-ci-observe", action_task),
    ("batch-reconcile", action_task),
    ("batch-repair", action_task),
    ("batch-repair-finish", action_task),
    ("batch-rebase", action_task),
];

fn action_handler(action: &str) -> Option<ActionHandler> {
    ACTION_HANDLERS
        .iter()
        .find(|(name, _)| *name == action)
        .map(|(_, handler)| *handler)
}

fn run_locked(root: &Path, args: &Args, factory: &ReceiverFactory) -> Result<Outcome> {
    match action_handler(&args.action) {
        Some(handler) => handler(root, args, factory),
        _ => action_default(root),
    }
}

fn run_watch(root: &Path, args: &Args) -> Result<()> {
    let state = validate_watch(root)?;
    let interval = model::u64_field(&state, "poll_seconds").unwrap_or(30);
    let factory = factory_for();
    watch_service::run(
        &super::artifacts(root)?,
        || watch_tick(root, args, &factory),
        || validate_watch(root).map(|_| ()),
        interval,
        args.ready_fd,
        args.once,
    )
}

fn watch_tick(root: &Path, args: &Args, factory: &ReceiverFactory) -> Result<bool> {
    let Some(report) = cleanup_before_tick(root, args.once)? else {
        return Ok(false);
    };
    let Some(lock) = watch_queue_lock(root, args.once)? else {
        return Ok(false);
    };
    watch_locked_tick(root, args, factory, report.pending, lock)
}

fn cleanup_before_tick(
    root: &Path,
    once: bool,
) -> Result<Option<super::finalization::CleanupReport>> {
    let report = match super::finalization::drive(root) {
        Ok(report) => report,
        Err(error) if !once => {
            eprintln!("terminal cleanup observation failed; retrying next tick: {error}");
            return Ok(None);
        }
        Err(error) => return Err(error),
    };
    for error in &report.errors {
        eprintln!("terminal cleanup pending: {error}");
    }
    Ok(Some(report))
}

fn watch_queue_lock(root: &Path, once: bool) -> Result<Option<util::FileLock>> {
    match super::locked(root, false) {
        Ok(lock) => Ok(Some(lock)),
        Err(error) if error.kind == ErrorKind::StateBusy => {
            if once {
                return Err(error);
            }
            Ok(None)
        }
        Err(error) => Err(error),
    }
}

fn watch_locked_tick(
    root: &Path,
    args: &Args,
    factory: &ReceiverFactory,
    pending: usize,
    lock: util::FileLock,
) -> Result<bool> {
    let current = super::tick(root, factory, None)?;
    if args.once || model::bool_or(&current, "stopped", false) {
        println!("{}", summary(&current));
    }
    drop(lock);
    Ok(model::bool_or(&current, "stopped", false) && pending == 0)
}

fn background_allowed(args: &Args) -> bool {
    ["watch", "run"].contains(&args.action.as_str()) && !args.once
}

fn ensure_background_allowed(args: &Args) -> Result<()> {
    if !background_allowed(args) {
        return Err(queue_error(
            "--background is only for watch/run without --once",
        ));
    }
    Ok(())
}

fn print_background_state(state: &J) -> Result<()> {
    println!(
        "{}",
        serde_json::to_string(state)
            .map_err(|error| FlowError::new(ErrorKind::Json, error.to_string()))?
    );
    Ok(())
}

fn run_background_watch(root: &Path) -> Result<()> {
    let state = background_watch_state(root)?;
    print_background_state(&state)
}

fn background_watch_state(root: &Path) -> Result<J> {
    start_background_watch(root, &super::artifacts(root)?)
}

fn start_background_watch(root: &Path, watch_root: &Path) -> Result<J> {
    watch_service::background(
        watch_root,
        &[
            "queue".to_string(),
            "watch".to_string(),
            "--state-dir".to_string(),
            root.display().to_string(),
        ],
        || validate_watch(root).map(|_| ()),
        Duration::from_secs(10),
    )
}

fn background_cwd<'a>(
    args: &'a Args,
    wait: &'a str,
    command_file: &'a Path,
) -> Result<(&'a str, &'a Path, &'a Path)> {
    let cwd = args
        .cwd
        .as_deref()
        .ok_or_else(|| FlowError::usage("run requires --cwd"))?;
    Ok((wait, command_file, cwd))
}

fn background_command_file<'a>(
    args: &'a Args,
    wait: &'a str,
) -> Result<(&'a str, &'a Path, &'a Path)> {
    let command_file = args
        .command_file
        .as_deref()
        .ok_or_else(|| FlowError::usage("run requires --command-file"))?;
    background_cwd(args, wait, command_file)
}

fn background_run_paths(args: &Args) -> Result<(&str, &Path, &Path)> {
    let wait = args
        .wait
        .as_deref()
        .ok_or_else(|| FlowError::usage("run requires --wait"))?;
    background_command_file(args, wait)
}

fn background_run_output(root: &Path, child: &std::process::Child) -> Result<()> {
    println!(
        "{}",
        json!({"pid": child.id(), "state_dir": root.display().to_string()})
    );
    Ok(())
}

fn background_spawn_with_exe(
    root: &Path,
    wait: &str,
    command_file: &Path,
    cwd: &Path,
    exe: &Path,
) -> Result<()> {
    let argv = background_command_argv(root, wait, command_file, cwd, exe);
    let child = background_command_process(root, &argv)?;
    background_run_output(root, &child)
}

fn background_command_argv(
    root: &Path,
    wait: &str,
    command_file: &Path,
    cwd: &Path,
    exe: &Path,
) -> Vec<String> {
    vec![
        exe.display().to_string(),
        "queue".to_string(),
        "run".to_string(),
        "--state-dir".to_string(),
        root.display().to_string(),
        "--wait".to_string(),
        wait.to_string(),
        "--command-file".to_string(),
        util::resolve_path(command_file).display().to_string(),
        "--cwd".to_string(),
        util::resolve_path(cwd).display().to_string(),
    ]
}

fn background_command_process(root: &Path, argv: &[String]) -> Result<std::process::Child> {
    util::spawn_detached(argv, &super::artifacts(root)?.join("watch.log"), None)
}

fn background_spawn_detached(
    root: &Path,
    wait: &str,
    command_file: &Path,
    cwd: &Path,
) -> Result<()> {
    let exe = util::current_exe()?;
    background_spawn_with_exe(root, wait, command_file, cwd, &exe)
}

fn background_spawn(root: &Path, wait: &str, command_file: &Path, cwd: &Path) -> Result<()> {
    let _ = super::load_queue(root)?;
    background_spawn_detached(root, wait, command_file, cwd)
}

fn run_background_run(root: &Path, args: &Args) -> Result<()> {
    let (wait, command_file, cwd) = background_run_paths(args)?;
    background_spawn(root, wait, command_file, cwd)
}

fn run_background(root: &Path, args: &Args) -> Result<()> {
    validate_background_authority(root, args)?;
    run_legacy_background(root, args)
}

fn validate_background_authority(root: &Path, args: &Args) -> Result<()> {
    if args.action != "run" {
        return Ok(());
    }
    super::isolation::require_legacy_execution(root)
}

fn run_legacy_background(root: &Path, args: &Args) -> Result<()> {
    ensure_background_allowed(args)?;
    run_background_checked(root, args)
}

fn run_background_checked(root: &Path, args: &Args) -> Result<()> {
    if args.action == "watch" {
        return run_background_watch(root);
    }
    run_background_run(root, args)
}

fn read_command_spec(args: &Args) -> Result<J> {
    let path = args
        .command_file
        .as_deref()
        .ok_or_else(|| FlowError::usage("run requires --command-file"))?;
    read_spec(path)
}

fn run_cwd(args: &Args) -> Result<String> {
    let cwd = args
        .cwd
        .as_deref()
        .ok_or_else(|| FlowError::usage("run requires --cwd"))?;
    Ok(cwd.display().to_string())
}

fn run_inner_execute(root: &Path, args: &Args, spec: &J, cwd: &str) -> Result<Option<i32>> {
    let code = run_command(root, args.wait.as_deref().unwrap_or_default(), spec, cwd)?;
    Ok(Some(code))
}

fn run_inner_command(root: &Path, args: &Args, spec: &J) -> Result<Option<i32>> {
    let cwd = run_cwd(args)?;
    run_inner_execute(root, args, spec, &cwd)
}

fn run_inner_run(root: &Path, args: &Args) -> Result<Option<i32>> {
    let spec = read_command_spec(args)?;
    run_inner_command(root, args, &spec)
}

fn run_inner_watch(root: &Path, args: &Args) -> Result<Option<i32>> {
    run_watch(root, args)?;
    Ok(None)
}

fn metrics_json(report: &J) -> Result<Option<i32>> {
    print_json(report)?;
    Ok(None)
}

fn metrics_output(report: &J, as_json: bool) -> Result<Option<i32>> {
    if as_json {
        return metrics_json(report);
    }
    println!("{}", crate::metrics::format_report(report));
    Ok(None)
}

fn run_inner_metrics(root: &Path, args: &Args) -> Result<Option<i32>> {
    let report = crate::metrics::report(root, None)?;
    metrics_output(&report, args.json)
}

fn run_locked_guarded(root: &Path, args: &Args, factory: &ReceiverFactory) -> Result<Outcome> {
    if args.action == "init-isolated" || task_action_owns_lock(&args.action) {
        return run_locked(root, args, factory);
    }
    let _lock = super::locked(root, false)?;
    run_locked(root, args, factory)
}

fn task_action_owns_lock(action: &str) -> bool {
    action.starts_with("task-") || action.starts_with("pr-") || action.starts_with("batch-")
}

fn print_outcome_and_code(state: &J, as_json: bool, code: Option<i32>) -> Result<Option<i32>> {
    print_state(state, as_json)?;
    Ok(code)
}

fn print_outcome_state(outcome: Outcome, as_json: bool) -> Result<Option<i32>> {
    if let Some(state) = &outcome.state {
        return print_outcome_and_code(state, as_json, outcome.code);
    }
    Ok(outcome.code)
}

fn run_inner_outcome(outcome: Outcome, as_json: bool) -> Result<Option<i32>> {
    if outcome.printed {
        return Ok(outcome.code);
    }
    print_outcome_state(outcome, as_json)
}

fn run_inner_locked(root: &Path, args: &Args) -> Result<Option<i32>> {
    let factory = factory_for();
    let outcome = run_locked_guarded(root, args, &factory)?;
    run_inner_outcome(outcome, args.json)
}

fn run_inner_other(root: &Path, args: &Args) -> Result<Option<i32>> {
    if args.action == "metrics" {
        return run_inner_metrics(root, args);
    }
    run_inner_locked(root, args)
}

fn run_inner_non_run(root: &Path, args: &Args) -> Result<Option<i32>> {
    if args.action == "watch" {
        return run_inner_watch(root, args);
    }
    run_inner_other(root, args)
}

fn run_inner_action(root: &Path, args: &Args) -> Result<Option<i32>> {
    if args.action == "run" {
        return run_inner_run(root, args);
    }
    run_inner_non_run(root, args)
}

fn run_inner_background(root: &Path, args: &Args) -> Result<Option<i32>> {
    run_background(root, args)?;
    Ok(None)
}

fn run_inner_root(root: &Path, args: &Args) -> Result<Option<i32>> {
    if args.background {
        return run_inner_background(root, args);
    }
    run_inner_action(root, args)
}

fn run_inner(args: &Args) -> Result<Option<i32>> {
    if args.action == "cleanup-preflight" {
        return run_cleanup_preflight(args);
    }
    let root = util::state_dir(&args.state_dir, true)?;
    run_inner_root(&root, args)
}

fn run_cleanup_preflight(args: &Args) -> Result<Option<i32>> {
    if args.background || args.once || args.ready_fd.is_some() {
        return Err(FlowError::usage("cleanup-preflight does not run a watcher"));
    }
    let report = super::cleanup_preflight::inspect(&args.state_dir, &read_required_spec(args)?)?;
    if args.json {
        print_json(&report)?;
    } else {
        println!("{}", super::cleanup_preflight::summary(&report));
    }
    Ok(Some(super::cleanup_preflight::exit_code(&report)))
}

fn cli_error_code(error: FlowError) -> Option<i32> {
    Some(cli_error(error))
}

fn cli_run(parsed: Args) -> i32 {
    run_inner(&parsed)
        .unwrap_or_else(cli_error_code)
        .unwrap_or(0)
}

fn print_queue_help() {
    println!(
        "Usage: herdr-flow queue ACTION --state-dir DIR [options]\nActions: {}",
        ACTIONS.join(", ")
    );
}

fn cli_parse_error(error: FlowError) -> i32 {
    if error.message == "help requested" {
        print_queue_help();
        return 0;
    }
    cli_error(error)
}

pub fn cli(args: &[String]) -> i32 {
    parse_args(args).map_or_else(cli_parse_error, cli_run)
}

fn cli_error(error: FlowError) -> i32 {
    util::write_stderr(&format!("{}: {}\n", error.kind.type_name(), error));
    2
}
