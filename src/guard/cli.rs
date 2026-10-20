//! Guard CLI parsing and command coordination.

use super::{
    ack, advice_status, advise, bind_runtime, checkpoint, configure_review, diagnosis,
    external_delivery_pending, guard_error, init, locked, read_state, reconcile, reconcile_review,
    review_events, save, validate_watch, wait_advice, watch, InitRequest,
};
use crate::error::ErrorKind;
use crate::error::FlowError;
use crate::error::Result;
use crate::herdr::GuardAdapter;
use crate::herdr::Herdr;
use crate::model;
use crate::model::J;
use crate::util;
use crate::watch_service;
use serde_json::json;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;

struct CmdInitInputs<'a> {
    pane: &'a str,
    session: &'a str,
    transcript: &'a Path,
    task: &'a str,
}

fn cmd_init(args: &Args, root: &Path) -> Result<J> {
    let inputs = cmd_init_inputs(args)?;
    cmd_init_run(args, root, &inputs)
}

fn cmd_init_inputs(args: &Args) -> Result<CmdInitInputs<'_>> {
    let Some(pane) = args.pane.as_deref() else {
        return Err(guard_error(
            "init requires pane, session, transcript and task",
        ));
    };
    let Some(session) = args.session.as_deref() else {
        return Err(guard_error(
            "init requires pane, session, transcript and task",
        ));
    };
    let Some(transcript) = args.transcript.as_deref() else {
        return Err(guard_error(
            "init requires pane, session, transcript and task",
        ));
    };
    let Some(task) = args.task.as_deref() else {
        return Err(guard_error(
            "init requires pane, session, transcript and task",
        ));
    };
    cmd_init_thresholds(args)?;
    Ok(CmdInitInputs {
        pane,
        session,
        transcript,
        task,
    })
}

fn cmd_init_thresholds(args: &Args) -> Result<()> {
    let tokens_ok = [
        0.0 < args.warn_tokens as f64,
        (args.warn_tokens as f64) < args.rotate_tokens as f64,
    ]
    .into_iter()
    .all(|valid| valid);
    let ratios_ok = [
        0.0 < args.warn_ratio,
        args.warn_ratio < args.rotate_ratio,
        args.rotate_ratio < 1.0,
    ]
    .into_iter()
    .all(|valid| valid);
    [tokens_ok, ratios_ok]
        .into_iter()
        .all(|valid| valid)
        .then_some(())
        .ok_or_else(|| guard_error("require positive ordered token thresholds and ratios"))
}

fn cmd_init_run(args: &Args, root: &Path, inputs: &CmdInitInputs<'_>) -> Result<J> {
    let s = cmd_init_create(args, root, inputs)?;
    cmd_init_save(args, root, s)
}

fn cmd_init_create(args: &Args, root: &Path, inputs: &CmdInitInputs<'_>) -> Result<J> {
    init(&InitRequest {
        root,
        pane: inputs.pane,
        session: inputs.session,
        transcript_path: inputs.transcript,
        task: inputs.task,
        epoch: args.epoch,
        role: &args.role,
        sessions_dir: &args.sessions_dir,
        app_socket: args.app_server_socket.as_deref(),
        codex_binary: args.codex_binary.as_deref(),
    })
}

fn cmd_init_save(args: &Args, root: &Path, mut s: J) -> Result<J> {
    cmd_init_fields(args, &mut s);
    save(root, &mut s)?;
    cmd_init_review(args, root, s)
}

fn cmd_init_fields(args: &Args, s: &mut J) {
    s["warn_tokens"] = json!(args.warn_tokens);
    s["rotate_tokens"] = json!(args.rotate_tokens);
    s["warn_ratio"] = json!(args.warn_ratio);
    s["rotate_ratio"] = json!(args.rotate_ratio);
    s["advisor_enabled"] = json!(!args.no_advisor);
}

fn cmd_init_review(args: &Args, root: &Path, s: J) -> Result<J> {
    if args.auto_review {
        return cmd_init_auto_review(args, root, s);
    }
    Ok(s)
}

fn cmd_init_auto_review(args: &Args, root: &Path, s: J) -> Result<J> {
    let mut h = Herdr::new(&s)?;
    configure_review(root, args.handoff.as_deref(), &mut h, false)
}

fn cmd_migrate(root: &Path, args: &Args) -> Result<J> {
    let s = read_state(root)?;
    cmd_migrate_settled(root, args, s)
}

fn cmd_migrate_settled(root: &Path, args: &Args, s: J) -> Result<J> {
    cmd_migrate_check(&s)?;
    cmd_migrate_lock(root, args, s)
}

fn cmd_migrate_check(s: &J) -> Result<()> {
    [
        matches!(
            model::string(s, "phase").as_deref(),
            Some("monitoring" | "parked")
        ),
        !external_delivery_pending(s),
        !model::bool_or(s, "auto_rotate", false),
    ]
    .into_iter()
    .all(|settled| settled)
    .then_some(())
    .ok_or_else(|| {
        guard_error("migration requires a settled, disabled guard without pending delivery")
    })
}

fn cmd_migrate_lock(root: &Path, args: &Args, s: J) -> Result<J> {
    let watcher = cmd_migrate_watcher(root)?;
    cmd_migrate_watcher_check(root, args, s, watcher)
}

fn cmd_migrate_watcher(root: &Path) -> Result<Option<util::FileLock>> {
    let watcher_path = util::state_dir(root, false)?.join("watch.lock");
    util::FileLock::acquire(&watcher_path, false)
}

fn cmd_migrate_watcher_check(
    root: &Path,
    args: &Args,
    s: J,
    watcher: Option<util::FileLock>,
) -> Result<J> {
    if watcher.is_none() {
        return Err(guard_error(
            "stop the existing watcher before migrating its state",
        ));
    }
    cmd_migrate_bind(root, args, s)
}

fn cmd_migrate_bind(root: &Path, args: &Args, mut s: J) -> Result<J> {
    let old = s.clone();
    s = bind_runtime(
        &mut s,
        root,
        args.app_server_socket.as_deref(),
        args.codex_binary.as_deref(),
    )?;
    cmd_migrate_history(root, s, old)
}

fn cmd_migrate_history(root: &Path, s: J, old: J) -> Result<J> {
    let history = util::resolve_path(root)
        .join("history")
        .join(format!("before-migration-{}.json", util::now_ns()));
    cmd_migrate_history_write(&history, &old)?;
    cmd_migrate_save(root, s)
}

fn cmd_migrate_history_write(history: &Path, old: &J) -> Result<()> {
    util::atomic_write_json(history, old)
}

fn cmd_migrate_save(root: &Path, mut s: J) -> Result<J> {
    save(root, &mut s)?;
    Ok(s)
}

fn cmd_release(root: &Path) -> Result<J> {
    let s = read_state(root)?;
    cmd_release_delivery(root, s)
}

fn cmd_release_delivery(root: &Path, s: J) -> Result<J> {
    if external_delivery_pending(&s) {
        return Err(guard_error(
            "resolve wait-queue delivery before releasing receiver",
        ));
    }
    cmd_release_phase(root, s)
}

fn cmd_release_phase(root: &Path, s: J) -> Result<J> {
    if !matches!(
        model::string(&s, "phase").as_deref(),
        Some("monitoring" | "ready" | "parked" | "released")
    ) {
        return Err(guard_error(
            "resolve in-flight handoff before releasing pane registration",
        ));
    }
    cmd_release_save(root, s)
}

fn cmd_release_save(root: &Path, mut s: J) -> Result<J> {
    s["auto_rotate"] = json!(false);
    s["phase"] = json!("released");
    save(root, &mut s)?;
    cmd_release_binding(root, s)
}

fn cmd_release_binding(root: &Path, s: J) -> Result<J> {
    let Some(binding) = model::string(&s, "binding") else {
        return Ok(s);
    };
    cmd_release_unbind(root, &binding);
    Ok(s)
}

fn cmd_release_unbind(root: &Path, binding: &str) {
    let path = Path::new(binding);
    if !path.exists() {
        return;
    }
    cmd_release_remove_owned(root, path);
}

fn cmd_release_remove_owned(root: &Path, path: &Path) {
    let Ok(registration) = util::read_json(path) else {
        return;
    };
    cmd_release_remove_if_owned(root, &registration, path);
}

fn cmd_release_remove_if_owned(root: &Path, registration: &J, path: &Path) {
    if model::string(registration, "state_dir")
        == Some(util::resolve_path(root).display().to_string())
    {
        let _ = std::fs::remove_file(path);
    }
}

fn cmd_toggle(root: &Path, enable: bool) -> Result<J> {
    let s = read_state(root)?;
    cmd_toggle_branch(root, enable, s)
}

fn cmd_toggle_branch(root: &Path, enable: bool, s: J) -> Result<J> {
    if enable {
        return cmd_toggle_enable(root, s);
    }
    cmd_toggle_save(root, s, enable)
}

fn cmd_toggle_enable(root: &Path, s: J) -> Result<J> {
    validate_watch(root)?;
    cmd_toggle_attention(root, s)
}

fn cmd_toggle_attention(root: &Path, s: J) -> Result<J> {
    if model::string(&s, "phase").as_deref() == Some("needs_attention") {
        return Err(guard_error(
            "inspect and reconcile before enabling automation",
        ));
    }
    cmd_toggle_background(root, s)
}

fn cmd_toggle_background(root: &Path, s: J) -> Result<J> {
    let _ = watch_service::background(
        root,
        &[
            "guard".to_string(),
            "watch".to_string(),
            "--state-dir".to_string(),
            util::resolve_path(root).display().to_string(),
        ],
        || validate_watch(root),
        Duration::from_secs(10),
    )?;
    cmd_toggle_deadline(root, s)
}

fn cmd_toggle_deadline(root: &Path, mut s: J) -> Result<J> {
    if matches!(
        model::string(&s, "phase").as_deref(),
        Some("awaiting_shell" | "awaiting_tui")
    ) {
        s["transition_deadline"] = json!(util::now() + 120.0);
    }
    cmd_toggle_save(root, s, true)
}

fn cmd_toggle_save(root: &Path, mut s: J, enable: bool) -> Result<J> {
    s["auto_rotate"] = json!(enable);
    save(root, &mut s)?;
    Ok(s)
}

#[derive(Default)]
struct Args {
    action: String,
    state_dir: PathBuf,
    pane: Option<String>,
    session: Option<String>,
    transcript: Option<PathBuf>,
    task: Option<String>,
    role: String,
    epoch: i64,
    sessions_dir: PathBuf,
    warn_tokens: i64,
    rotate_tokens: i64,
    warn_ratio: f64,
    rotate_ratio: f64,
    handoff: Option<PathBuf>,
    once: bool,
    background: bool,
    ready_fd: Option<std::os::fd::RawFd>,
    app_server_socket: Option<String>,
    codex_binary: Option<String>,
    no_advisor: bool,
    auto_review: bool,
    wait: f64,
    shell: bool,
}

fn parse_value_at(args: &[String], index: usize) -> Result<String> {
    args.get(index + 1)
        .cloned()
        .ok_or_else(|| FlowError::usage("missing option value"))
}

fn parse_text_option(args: &[String], index: usize) -> Result<String> {
    parse_value_at(args, index)
}

fn parse_path_option(args: &[String], index: usize) -> Result<PathBuf> {
    let value = parse_value_at(args, index)?;
    Ok(PathBuf::from(value))
}

fn parse_i64_option(args: &[String], index: usize, message: &'static str) -> Result<i64> {
    let value = parse_value_at(args, index)?;
    value.parse().map_err(|_| FlowError::usage(message))
}

fn parse_f64_option(args: &[String], index: usize, message: &'static str) -> Result<f64> {
    let value = parse_value_at(args, index)?;
    value.parse().map_err(|_| FlowError::usage(message))
}

fn parse_fd_option(args: &[String], index: usize) -> Result<i32> {
    let value = parse_value_at(args, index)?;
    value
        .parse()
        .map_err(|_| FlowError::usage("invalid --ready-fd"))
}

fn parse_opt_state_dir(args: &[String], index: usize, parsed: &mut Args) -> Result<usize> {
    parsed.state_dir = parse_path_option(args, index)?;
    Ok(index + 2)
}

fn parse_opt_pane(args: &[String], index: usize, parsed: &mut Args) -> Result<usize> {
    parsed.pane = Some(parse_text_option(args, index)?);
    Ok(index + 2)
}

fn parse_opt_session(args: &[String], index: usize, parsed: &mut Args) -> Result<usize> {
    parsed.session = Some(parse_text_option(args, index)?);
    Ok(index + 2)
}

fn parse_opt_transcript(args: &[String], index: usize, parsed: &mut Args) -> Result<usize> {
    parsed.transcript = Some(parse_path_option(args, index)?);
    Ok(index + 2)
}

fn parse_opt_task(args: &[String], index: usize, parsed: &mut Args) -> Result<usize> {
    parsed.task = Some(parse_text_option(args, index)?);
    Ok(index + 2)
}

fn parse_opt_role(args: &[String], index: usize, parsed: &mut Args) -> Result<usize> {
    parsed.role = parse_text_option(args, index)?;
    Ok(index + 2)
}

fn parse_opt_epoch(args: &[String], index: usize, parsed: &mut Args) -> Result<usize> {
    parsed.epoch = parse_i64_option(args, index, "invalid --epoch")?;
    Ok(index + 2)
}

fn parse_opt_sessions_dir(args: &[String], index: usize, parsed: &mut Args) -> Result<usize> {
    parsed.sessions_dir = parse_path_option(args, index)?;
    Ok(index + 2)
}

fn parse_opt_warn_tokens(args: &[String], index: usize, parsed: &mut Args) -> Result<usize> {
    parsed.warn_tokens = parse_i64_option(args, index, "invalid --warn-tokens")?;
    Ok(index + 2)
}

fn parse_opt_rotate_tokens(args: &[String], index: usize, parsed: &mut Args) -> Result<usize> {
    parsed.rotate_tokens = parse_i64_option(args, index, "invalid --rotate-tokens")?;
    Ok(index + 2)
}

fn parse_opt_warn_ratio(args: &[String], index: usize, parsed: &mut Args) -> Result<usize> {
    parsed.warn_ratio = parse_f64_option(args, index, "invalid --warn-ratio")?;
    Ok(index + 2)
}

fn parse_opt_rotate_ratio(args: &[String], index: usize, parsed: &mut Args) -> Result<usize> {
    parsed.rotate_ratio = parse_f64_option(args, index, "invalid --rotate-ratio")?;
    Ok(index + 2)
}

fn parse_opt_handoff(args: &[String], index: usize, parsed: &mut Args) -> Result<usize> {
    parsed.handoff = Some(parse_path_option(args, index)?);
    Ok(index + 2)
}

fn parse_opt_ready_fd(args: &[String], index: usize, parsed: &mut Args) -> Result<usize> {
    parsed.ready_fd = Some(parse_fd_option(args, index)?);
    Ok(index + 2)
}

fn parse_opt_app_server_socket(args: &[String], index: usize, parsed: &mut Args) -> Result<usize> {
    parsed.app_server_socket = Some(parse_text_option(args, index)?);
    Ok(index + 2)
}

fn parse_opt_codex_binary(args: &[String], index: usize, parsed: &mut Args) -> Result<usize> {
    parsed.codex_binary = Some(parse_text_option(args, index)?);
    Ok(index + 2)
}

fn parse_opt_wait(args: &[String], index: usize, parsed: &mut Args) -> Result<usize> {
    parsed.wait = parse_f64_option(args, index, "invalid --wait")?;
    Ok(index + 2)
}

fn parse_flag_once(_args: &[String], index: usize, parsed: &mut Args) -> Result<usize> {
    parsed.once = true;
    Ok(index + 1)
}

fn parse_flag_background(_args: &[String], index: usize, parsed: &mut Args) -> Result<usize> {
    parsed.background = true;
    Ok(index + 1)
}

fn parse_flag_no_advisor(_args: &[String], index: usize, parsed: &mut Args) -> Result<usize> {
    parsed.no_advisor = true;
    Ok(index + 1)
}

fn parse_flag_auto_review(_args: &[String], index: usize, parsed: &mut Args) -> Result<usize> {
    parsed.auto_review = true;
    Ok(index + 1)
}

fn parse_flag_shell(_args: &[String], index: usize, parsed: &mut Args) -> Result<usize> {
    parsed.shell = true;
    Ok(index + 1)
}

fn parse_help(_args: &[String], _index: usize, _parsed: &mut Args) -> Result<usize> {
    Err(FlowError::usage("help requested"))
}

type OptionParser = fn(&[String], usize, &mut Args) -> Result<usize>;

struct ParsedOption {
    name: &'static str,
    parse: OptionParser,
}

static OPTION_PARSERS: &[ParsedOption] = &[
    ParsedOption {
        name: "--state-dir",
        parse: parse_opt_state_dir,
    },
    ParsedOption {
        name: "--pane",
        parse: parse_opt_pane,
    },
    ParsedOption {
        name: "--session",
        parse: parse_opt_session,
    },
    ParsedOption {
        name: "--transcript",
        parse: parse_opt_transcript,
    },
    ParsedOption {
        name: "--task",
        parse: parse_opt_task,
    },
    ParsedOption {
        name: "--role",
        parse: parse_opt_role,
    },
    ParsedOption {
        name: "--epoch",
        parse: parse_opt_epoch,
    },
    ParsedOption {
        name: "--sessions-dir",
        parse: parse_opt_sessions_dir,
    },
    ParsedOption {
        name: "--warn-tokens",
        parse: parse_opt_warn_tokens,
    },
    ParsedOption {
        name: "--rotate-tokens",
        parse: parse_opt_rotate_tokens,
    },
    ParsedOption {
        name: "--warn-ratio",
        parse: parse_opt_warn_ratio,
    },
    ParsedOption {
        name: "--rotate-ratio",
        parse: parse_opt_rotate_ratio,
    },
    ParsedOption {
        name: "--handoff",
        parse: parse_opt_handoff,
    },
    ParsedOption {
        name: "--once",
        parse: parse_flag_once,
    },
    ParsedOption {
        name: "--background",
        parse: parse_flag_background,
    },
    ParsedOption {
        name: "--ready-fd",
        parse: parse_opt_ready_fd,
    },
    ParsedOption {
        name: "--app-server-socket",
        parse: parse_opt_app_server_socket,
    },
    ParsedOption {
        name: "--codex-binary",
        parse: parse_opt_codex_binary,
    },
    ParsedOption {
        name: "--no-advisor",
        parse: parse_flag_no_advisor,
    },
    ParsedOption {
        name: "--auto-review",
        parse: parse_flag_auto_review,
    },
    ParsedOption {
        name: "--wait",
        parse: parse_opt_wait,
    },
    ParsedOption {
        name: "--shell",
        parse: parse_flag_shell,
    },
    ParsedOption {
        name: "-h",
        parse: parse_help,
    },
    ParsedOption {
        name: "--help",
        parse: parse_help,
    },
];

const ACTIONS: &[&str] = &[
    "init",
    "migrate",
    "status",
    "watch",
    "checkpoint",
    "ack",
    "advise",
    "review-enable",
    "review-reconcile",
    "enable",
    "disable",
    "release",
    "reconcile",
];

fn parse_args(args: &[String]) -> Result<Args> {
    let mut parsed = parse_args_defaults();
    parse_args_loop(args, &mut parsed)?;
    parse_args_finish(parsed)
}

fn parse_args_defaults() -> Args {
    Args {
        role: "dev".to_string(),
        epoch: 1,
        sessions_dir: util::codex_home().join("sessions"),
        warn_tokens: 80000,
        rotate_tokens: 120000,
        warn_ratio: 0.60,
        rotate_ratio: 0.75,
        ..Default::default()
    }
}

fn parse_args_loop(args: &[String], parsed: &mut Args) -> Result<()> {
    parse_args_loop_from(args, 0, parsed)
}

fn parse_args_loop_from(args: &[String], index: usize, parsed: &mut Args) -> Result<()> {
    let Some(next) = parse_args_loop_next(args, index, parsed)? else {
        return Ok(());
    };
    parse_args_loop_from(args, next, parsed)
}

fn parse_args_loop_next(args: &[String], index: usize, parsed: &mut Args) -> Result<Option<usize>> {
    if index >= args.len() {
        return Ok(None);
    }
    parse_args_dispatch(args, index, parsed).map(Some)
}

fn parse_args_dispatch(args: &[String], index: usize, parsed: &mut Args) -> Result<usize> {
    let argument = args[index].as_str();
    let option = OPTION_PARSERS.iter().find(|option| option.name == argument);
    parse_args_selected(args, index, parsed, option)
}

fn parse_args_selected(
    args: &[String],
    index: usize,
    parsed: &mut Args,
    option: Option<&ParsedOption>,
) -> Result<usize> {
    if let Some(option) = option {
        return (option.parse)(args, index, parsed);
    }
    parse_args_fallback(args, index, parsed)
}

fn parse_args_fallback(args: &[String], index: usize, parsed: &mut Args) -> Result<usize> {
    if args[index].starts_with('-') {
        return parse_args_unknown(&args[index]);
    }
    parse_args_positional(args, index, parsed)
}

fn parse_args_unknown(argument: &str) -> Result<usize> {
    Err(FlowError::usage(format!(
        "unrecognized argument: {argument}"
    )))
}

fn parse_args_positional(args: &[String], index: usize, parsed: &mut Args) -> Result<usize> {
    if !parsed.action.is_empty() {
        return Err(FlowError::usage(format!(
            "unrecognized argument: {}",
            args[index]
        )));
    }
    parsed.action = args[index].clone();
    Ok(index + 1)
}

fn parse_args_finish(mut parsed: Args) -> Result<Args> {
    parse_args_validate(&mut parsed)?;
    Ok(parsed)
}

fn parse_args_validate(parsed: &mut Args) -> Result<()> {
    parse_args_action_required(parsed)?;
    parse_args_after_action(parsed)
}

fn parse_args_action_required(parsed: &Args) -> Result<()> {
    if parsed.action.is_empty() {
        return Err(FlowError::usage("an action is required"));
    }
    Ok(())
}

fn parse_args_after_action(parsed: &mut Args) -> Result<()> {
    parse_args_action_allowed(&parsed.action)?;
    parse_args_state_dir(parsed)
}

fn parse_args_action_allowed(action: &str) -> Result<()> {
    ACTIONS
        .contains(&action)
        .then_some(())
        .ok_or_else(|| FlowError::usage(format!("invalid action: {action}")))
}

fn parse_args_state_dir(parsed: &mut Args) -> Result<()> {
    if parsed.state_dir.as_os_str().is_empty() {
        return Err(FlowError::usage("--state-dir is required"));
    }
    parse_args_session_env(parsed)
}

fn parse_args_session_env(parsed: &mut Args) -> Result<()> {
    if parsed.session.is_none() {
        parsed.session = std::env::var("CODEX_THREAD_ID").ok();
    }
    parse_args_wait(parsed)
}

fn parse_args_wait(parsed: &Args) -> Result<()> {
    let valid = [
        (0.0..=60.0).contains(&parsed.wait),
        parse_args_wait_action_ok(parsed),
    ]
    .into_iter()
    .all(|valid| valid);
    if !valid {
        return Err(guard_error(
            "--wait requires advise and a duration from 0 to 60 seconds",
        ));
    }
    parse_args_no_advisor(parsed)
}

fn parse_args_wait_action_ok(parsed: &Args) -> bool {
    parsed.wait == 0.0 || parsed.action == "advise"
}

fn parse_args_no_advisor(parsed: &Args) -> Result<()> {
    if [parsed.no_advisor, parsed.action != "init"]
        .into_iter()
        .all(|invalid| invalid)
    {
        return Err(guard_error("--no-advisor is an init option"));
    }
    parse_args_auto_review(parsed)
}

fn parse_args_auto_review(parsed: &Args) -> Result<()> {
    if [parsed.auto_review, parsed.action != "init"]
        .into_iter()
        .all(|invalid| invalid)
    {
        return Err(guard_error(
            "--auto-review is an init option; existing guards use review-enable",
        ));
    }
    parse_args_background(parsed)
}

fn parse_args_background(parsed: &Args) -> Result<()> {
    let blocking = [parsed.action != "watch", parsed.once]
        .into_iter()
        .any(|blocked| blocked);
    if [parsed.background, blocking]
        .into_iter()
        .all(|invalid| invalid)
    {
        return Err(guard_error("--background requires watch without --once"));
    }
    parse_args_session_owner(parsed)
}

fn parse_args_session_owner(parsed: &Args) -> Result<()> {
    if !parse_args_owned_action(&parsed.action) {
        return Ok(());
    }
    parse_args_owned_env(parsed)
}

fn parse_args_owned_action(action: &str) -> bool {
    matches!(action, "checkpoint" | "ack" | "advise")
}

fn parse_args_owned_env(parsed: &Args) -> Result<()> {
    let env_session = std::env::var("CODEX_THREAD_ID").ok();
    let mismatch = [env_session.is_none(), parsed.session != env_session]
        .into_iter()
        .any(|mismatch| mismatch);
    if mismatch {
        return Err(guard_error(
            "checkpoint/ack/advise must be called by the identified Codex session itself",
        ));
    }
    Ok(())
}

fn print_status(root: &Path) -> Result<()> {
    let s = read_state(root)?;
    let output = cli_output(root, &s);
    cli_print_output(&output)
}

fn cli_error(error: FlowError) -> i32 {
    util::write_stderr(&format!("{}: {}\n", error.kind.type_name(), error));
    2
}

fn cli_pretty(value: &J) -> Result<String> {
    serde_json::to_string_pretty(value)
        .map_err(|error| FlowError::new(ErrorKind::Json, error.to_string()))
}

fn cli_print_output(output: &J) -> Result<()> {
    let text = cli_pretty(output)?;
    println!("{text}");
    Ok(())
}

fn cli_output(root: &Path, state: &J) -> J {
    let watcher = watch_service::health(&util::resolve_path(root));
    let mut output = state.clone();
    output["watcher"] = watcher.clone();
    output["diagnosis"] = diagnosis(state, &watcher);
    output["advice"] = advice_status(root, state)
        .unwrap_or_else(|error| json!({"status": "unavailable", "error": error.to_string()}));
    output["review_events"] = review_events(root, state)
        .unwrap_or_else(|error| json!({"status": "unavailable", "error": error.to_string()}));
    output
}

fn cli_help() -> i32 {
    println!(
        "Usage: herdr-flow guard ACTION --state-dir DIR [options]\nActions: init, migrate, status, watch, checkpoint, ack, advise, review-enable, review-reconcile, enable, disable, release, reconcile"
    );
    0
}

fn cli_parse_error(error: FlowError) -> i32 {
    if error.message == "help requested" {
        return cli_help();
    }
    cli_error(error)
}

pub fn cli(args: &[String]) -> i32 {
    parse_args(args).map_or_else(cli_parse_error, |parsed| cli_parsed(&parsed))
}

fn cli_parsed(parsed: &Args) -> i32 {
    util::state_dir(&parsed.state_dir, true).map_or_else(cli_error, |root| cli_action(parsed, root))
}

fn cli_action(parsed: &Args, root: PathBuf) -> i32 {
    if parsed.action == "watch" {
        return cli_watch(parsed, &root);
    }
    cli_locked(parsed, &root)
}

fn cli_watch(parsed: &Args, root: &Path) -> i32 {
    if parsed.background {
        return cli_watch_background(root);
    }
    cli_watch_foreground(parsed, root)
}

fn cli_watch_background(root: &Path) -> i32 {
    watch_service::background(
        root,
        &[
            "guard".to_string(),
            "watch".to_string(),
            "--state-dir".to_string(),
            root.display().to_string(),
        ],
        || validate_watch(root),
        Duration::from_secs(10),
    )
    .map_or_else(cli_error, |value| {
        println!("{value}");
        0
    })
}

fn cli_watch_foreground(parsed: &Args, root: &Path) -> i32 {
    watch(root, parsed.ready_fd, parsed.once).map_or_else(cli_error, |()| 0)
}

fn cli_locked(parsed: &Args, root: &Path) -> i32 {
    cli_run(parsed, root).map_or_else(cli_error, |state| cli_finish(root, state))
}

fn cli_finish(root: &Path, state: Option<J>) -> i32 {
    state.map_or(0, |state| cli_emit_state(root, &state))
}

fn cli_emit_state(root: &Path, state: &J) -> i32 {
    let output = cli_output(root, state);
    serde_json::to_string_pretty(&output).map_or_else(
        |error| cli_error(FlowError::new(ErrorKind::Json, error.to_string())),
        |text| {
            println!("{text}");
            0
        },
    )
}

fn cli_run(parsed: &Args, root: &Path) -> Result<Option<J>> {
    let _lock = locked(root)?;
    cli_dispatch(parsed, root)
}

type CliAction = fn(&Args, &Path) -> Result<Option<J>>;

static CLI_ACTIONS: &[(&str, CliAction)] = &[
    ("init", cli_action_init),
    ("migrate", cli_action_migrate),
    ("status", cli_action_status),
    ("checkpoint", cli_action_checkpoint),
    ("ack", cli_action_ack),
    ("advise", cli_action_advise),
    ("review-enable", cli_action_review_enable),
    ("review-reconcile", cli_action_review_reconcile),
    ("enable", cli_action_toggle),
    ("disable", cli_action_toggle),
    ("release", cli_action_release),
    ("reconcile", cli_action_reconcile),
];

fn cli_dispatch(parsed: &Args, root: &Path) -> Result<Option<J>> {
    CLI_ACTIONS
        .iter()
        .find(|entry| entry.0 == parsed.action.as_str())
        .map_or_else(|| Ok(None), |entry| (entry.1)(parsed, root))
}

fn cli_action_init(parsed: &Args, root: &Path) -> Result<Option<J>> {
    cmd_init(parsed, root).map(Some)
}

fn cli_action_migrate(parsed: &Args, root: &Path) -> Result<Option<J>> {
    cmd_migrate(root, parsed).map(Some)
}

fn cli_action_status(_parsed: &Args, root: &Path) -> Result<Option<J>> {
    print_status(root)?;
    Ok(None)
}

fn cli_handoff(parsed: &Args) -> Result<&Path> {
    parsed
        .handoff
        .as_deref()
        .ok_or_else(|| guard_error("--handoff required"))
}

fn cli_action_checkpoint(parsed: &Args, root: &Path) -> Result<Option<J>> {
    let handoff = cli_handoff(parsed)?;
    cli_checkpoint_run(parsed, root, handoff)
}

fn cli_checkpoint_run(parsed: &Args, root: &Path, handoff: &Path) -> Result<Option<J>> {
    checkpoint(root, handoff, parsed.session.as_deref().unwrap_or_default()).map(Some)
}

fn cli_action_ack(parsed: &Args, root: &Path) -> Result<Option<J>> {
    ack(root, parsed.session.as_deref().unwrap_or_default()).map(Some)
}

fn cli_action_advise(parsed: &Args, root: &Path) -> Result<Option<J>> {
    let handoff = cli_handoff(parsed)?;
    cli_advise_run(parsed, root, handoff)
}

fn cli_advise_run(parsed: &Args, root: &Path, handoff: &Path) -> Result<Option<J>> {
    let advice = advise(root, handoff, parsed.session.as_deref().unwrap_or_default())?;
    cli_advise_after(parsed, root, advice)
}

fn cli_advise_after(parsed: &Args, root: &Path, advice: J) -> Result<Option<J>> {
    if cli_advise_should_wait(parsed, &advice) {
        return cli_advise_wait(parsed, root);
    }
    cli_advise_print(&advice)
}

fn cli_advise_should_wait(parsed: &Args, advice: &J) -> bool {
    [
        parsed.wait != 0.0,
        model::string(advice, "status").as_deref() != Some("disabled"),
    ]
    .into_iter()
    .all(|wait| wait)
}

fn cli_advise_wait(parsed: &Args, root: &Path) -> Result<Option<J>> {
    let advice = wait_advice(root, parsed.wait)?;
    cli_advise_print(&advice)
}

fn cli_advise_print(advice: &J) -> Result<Option<J>> {
    let text = cli_pretty(advice)?;
    println!("{text}");
    Ok(None)
}

fn cli_action_review_enable(parsed: &Args, root: &Path) -> Result<Option<J>> {
    let state = read_state(root)?;
    cli_review_enable_run(parsed, root, &state)
}

fn cli_review_enable_run(parsed: &Args, root: &Path, state: &J) -> Result<Option<J>> {
    let mut h = Herdr::new(state)?;
    configure_review(root, parsed.handoff.as_deref(), &mut h, true).map(Some)
}

fn cli_action_review_reconcile(_parsed: &Args, root: &Path) -> Result<Option<J>> {
    let state = read_state(root)?;
    cli_review_reconcile_run(root, &state)
}

fn cli_review_reconcile_run(root: &Path, state: &J) -> Result<Option<J>> {
    let mut h = Herdr::new(state)?;
    reconcile_review(root, &mut h).map(Some)
}

fn cli_action_toggle(parsed: &Args, root: &Path) -> Result<Option<J>> {
    cmd_toggle(root, parsed.action == "enable").map(Some)
}

fn cli_action_release(_parsed: &Args, root: &Path) -> Result<Option<J>> {
    cmd_release(root).map(Some)
}

fn cli_action_reconcile(parsed: &Args, root: &Path) -> Result<Option<J>> {
    let state = read_state(root)?;
    cli_reconcile_run(parsed, root, &state)
}

fn cli_reconcile_run(parsed: &Args, root: &Path, state: &J) -> Result<Option<J>> {
    let mut h = Herdr::new(state)?;
    cli_reconcile_connected(parsed, root, &mut h)
}

fn cli_reconcile_connected(
    parsed: &Args,
    root: &Path,
    h: &mut dyn GuardAdapter,
) -> Result<Option<J>> {
    let s = reconcile(
        root,
        parsed.session.as_deref().unwrap_or_default(),
        h,
        parsed.shell,
    )?;
    cli_reconcile_background(root, s)
}

fn cli_reconcile_background(root: &Path, s: J) -> Result<Option<J>> {
    let _ = watch_service::background(
        root,
        &[
            "guard".to_string(),
            "watch".to_string(),
            "--state-dir".to_string(),
            root.display().to_string(),
        ],
        || validate_watch(root),
        Duration::from_secs(10),
    );
    Ok(Some(s))
}
