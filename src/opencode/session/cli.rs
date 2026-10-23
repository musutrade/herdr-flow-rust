use super::*;
use std::os::fd::RawFd;
use std::time::Duration;

use crate::watch_service;

const USAGE:&str="Usage: herdr-flow opencode <register|status|dispatch|collect|watch> --state-dir DIR [--spec FILE] [--once|--background] [--ready-fd FD]\nOpenCode 1.19.x native protocol (baseline 1.19.3). register/dispatch require --spec FILE. No automatic rotation.";

#[derive(Default)]
struct Args {
    root: PathBuf,
    spec: Option<PathBuf>,
    once: bool,
    background: bool,
    ready_fd: Option<RawFd>,
}

fn value(args: &[String], index: &mut usize) -> Result<String> {
    *index += 1;
    args.get(*index)
        .cloned()
        .ok_or_else(|| FlowError::usage("missing OpenCode option value"))
}

fn option(args: &[String], index: &mut usize, parsed: &mut Args) -> Result<()> {
    match args[*index].as_str() {
        "--once" => parsed.once = true,
        "--background" => parsed.background = true,
        key => value_option(key, &value(args, index)?, parsed)?,
    }
    Ok(())
}

fn value_option(key: &str, value: &str, parsed: &mut Args) -> Result<()> {
    match key {
        "--state-dir" => parsed.root = PathBuf::from(value),
        "--spec" => parsed.spec = Some(PathBuf::from(value)),
        "--ready-fd" => {
            parsed.ready_fd = Some(
                value
                    .parse()
                    .map_err(|_| FlowError::usage("invalid ready fd"))?,
            )
        }
        _ => return Err(FlowError::usage(USAGE)),
    }
    Ok(())
}

fn parse(args: &[String]) -> Result<Args> {
    let mut parsed = Args::default();
    let mut index = 0;
    while index < args.len() {
        option(args, &mut index, &mut parsed)?;
        index += 1;
    }
    ensure(
        !parsed.root.as_os_str().is_empty(),
        "--state-dir is required",
    )?;
    Ok(parsed)
}

fn spec(args: &Args) -> Result<J> {
    let path = args
        .spec
        .as_deref()
        .ok_or_else(|| FlowError::usage("--spec is required"))?;
    util::read_json(path)
}

fn validate_watch(root: &Path) -> Result<()> {
    let state = util::read_state_json(root, "state.json")?;
    SessionAdapter::new(&state)?.verify_binding().map(|_| ())
}

fn watch_tick(root: &Path) -> Result<bool> {
    match watch_observe(root) {
        Err(error) if error.kind == ErrorKind::StateBusy => Ok(false),
        other => other,
    }
}

fn watch_observe(root: &Path) -> Result<bool> {
    let _lock = crate::guard::locked(root)?;
    let mut state = util::read_state_json(root, "state.json")?;
    let adapter = SessionAdapter::new(&state)?;
    adapter.reconcile()?;
    state["observation"] = adapter.status()?;
    state["observed_at"] = json!(util::now());
    crate::guard::save(root, &mut state)?;
    Ok(false)
}

fn watch(args: &Args) -> Result<J> {
    let root = util::state_dir(&args.root, false)?;
    if args.background {
        ensure(!args.once, "--background and --once cannot be combined")?;
        return watch_service::background(
            &root,
            &[
                "opencode".into(),
                "watch".into(),
                "--state-dir".into(),
                root.display().to_string(),
            ],
            || validate_watch(&root),
            Duration::from_secs(15),
        );
    }
    watch_service::run(
        &root,
        || watch_tick(&root),
        || validate_watch(&root),
        10,
        args.ready_fd,
        args.once,
    )?;
    Ok(watch_service::health(&root))
}

fn action(name: &str, args: &Args) -> Result<J> {
    match name {
        "register" => action_register(args),
        "status" => action_status(args),
        "dispatch" => action_dispatch(args),
        "collect" => collect(&args.root),
        "watch" => watch(args),
        _ => Err(FlowError::usage(USAGE)),
    }
}

fn action_register(args: &Args) -> Result<J> {
    register(&args.root, &spec(args)?)
}
fn action_dispatch(args: &Args) -> Result<J> {
    dispatch(&args.root, &spec(args)?)
}
fn action_status(args: &Args) -> Result<J> {
    SessionAdapter::new(&util::read_state_json(&args.root, "state.json")?)?.status()
}

pub fn cli(args: &[String]) -> i32 {
    if args
        .iter()
        .any(|arg| matches!(arg.as_str(), "--help" | "-h"))
    {
        println!("{USAGE}");
        return 0;
    }
    let result = args
        .first()
        .ok_or_else(|| FlowError::usage(USAGE))
        .and_then(|name| parse(&args[1..]).and_then(|args| action(name, &args)));
    match result {
        Ok(value) => {
            println!(
                "{}",
                serde_json::to_string_pretty(&value).unwrap_or_default()
            );
            0
        }
        Err(error) => {
            util::write_stderr(&format!("{}: {error}\n", error.kind.type_name()));
            2
        }
    }
}
