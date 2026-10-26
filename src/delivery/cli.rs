use super::*;
const USAGE: &str = "Usage: herdr-flow delivery <prepare|execute|reconcile|retry|finish> --task TASK [--queue DIR --spec FILE]";

fn run(args: &[String]) -> Result<J> {
    let action = args.first().ok_or_else(|| FlowError::usage(USAGE))?;
    let options = parse_options(&args[1..])?;
    let task = options
        .get("--task")
        .ok_or_else(|| FlowError::usage(USAGE))?;
    if action == "finish" {
        if let Some(outcome) = finish_missing(task)? {
            return Ok(outcome);
        }
    }
    let mut env = crate::task_env::restore_environment(task)?;
    route(action, &mut env, &options)
}
fn route(
    action: &str,
    env: &mut TaskEnvironment,
    options: &std::collections::BTreeMap<&str, &str>,
) -> Result<J> {
    match action {
        "prepare" => prepare_cli(env, options),
        "execute" => execute(env, &local::Local),
        "reconcile" => reconcile(env, &local::Local),
        "retry" => retry(env, &local::Local),
        "finish" => {
            finish(env)?;
            Ok(json!({"finished":true}))
        }
        _ => Err(FlowError::usage(USAGE)),
    }
}
fn parse_options(args: &[String]) -> Result<std::collections::BTreeMap<&str, &str>> {
    let mut options = std::collections::BTreeMap::new();
    let (pairs, remainder) = args.as_chunks::<2>();
    for pair in pairs {
        ensure(
            ["--task", "--queue", "--spec"].contains(&pair[0].as_str()),
            USAGE,
        )?;
        ensure(
            options.insert(pair[0].as_str(), pair[1].as_str()).is_none(),
            USAGE,
        )?;
    }
    ensure(remainder.is_empty(), USAGE)?;
    Ok(options)
}
fn prepare_cli(
    env: &mut TaskEnvironment,
    options: &std::collections::BTreeMap<&str, &str>,
) -> Result<J> {
    let queue = options
        .get("--queue")
        .ok_or_else(|| FlowError::usage(USAGE))?;
    let spec = options
        .get("--spec")
        .ok_or_else(|| FlowError::usage(USAGE))?;
    prepare(
        env,
        Path::new(queue),
        &util::read_json(Path::new(spec))?,
        &local::Local,
    )
}
fn finish_missing(task: &str) -> Result<Option<J>> {
    if crate::task_env::environment_state_path(task)?.exists() {
        return Ok(None);
    }
    if crate::task_env::cleanup_state_path(task)?.exists() {
        crate::task_env::resume_delivery_cleanup(task)?;
        return Ok(Some(json!({"finished":true})));
    }
    ensure(
        !crate::task_env::is_registered(task)?,
        "environment registry is inconsistent",
    )?;
    // With no retained history, absence cannot attest a previous delivery.
    Ok(Some(json!({"state":"absent","delivery":"not_attested"})))
}
pub fn cli(args: &[String]) -> i32 {
    if args.iter().any(|v| v == "--help" || v == "-h") {
        println!("{USAGE}");
        return 0;
    }
    match run(args) {
        Ok(value) => {
            println!("{value}");
            0
        }
        Err(error) => {
            eprintln!("{error}");
            2
        }
    }
}
