use super::*;
const USAGE: &str = "Usage: herdr-flow task-runtime <prepare|review-snapshot|launch|create|receiver-bootstrap|receiver-start|receiver-activate|receiver-recover|broker-recover|recover|dispatch|collect|status|tui-open|tui-status|tui-close> --task TASK --state-dir DIR [--spec FILE]\n       herdr-flow task-runtime configuration-status --adapter <codex>\nLogin/provider/billing belong to upstream. Native TUI resumes the same task in Herdr; closing it does not cancel the task. Codex upstream mode connects to an existing Unix app-server; isolated-fixture is only for credential-free protocol tests.";

fn run(args: &[String]) -> Result<J> {
    if let Some(result) = special(args) {
        return result;
    }
    let action = args.first().ok_or_else(|| FlowError::usage(USAGE))?;
    let options = parse_options(&args[1..])?;
    let task = options
        .get("--task")
        .ok_or_else(|| FlowError::usage(USAGE))?;
    let root = PathBuf::from(
        options
            .get("--state-dir")
            .ok_or_else(|| FlowError::usage(USAGE))?,
    );
    let spec = options
        .get("--spec")
        .map(|p| util::read_json(Path::new(p)))
        .transpose()?
        .unwrap_or(json!({}));
    let mut env = crate::task_env::restore_environment(task)?;
    action_run(action, &mut env, &root, &spec)
}
fn special(args: &[String]) -> Option<Result<J>> {
    if args
        .first()
        .is_some_and(|arg| arg == "configuration-status")
    {
        return Some(configuration_status(args));
    }
    match args {
        [action, option, path] if action == "tui-serve" && option == "--state-dir" => {
            Some(tui::serve(Path::new(path)).map(|code| json!({"exit_code":code})))
        }
        [action, option, path] if action == "receiver-watch" && option == "--state-dir" => {
            Some(native::receiver::active::watch(Path::new(path)).map(|_| json!({"stopped":true})))
        }
        _ => None,
    }
}

fn configuration_status(args: &[String]) -> Result<J> {
    match args {
        [_, option, adapter] if option == "--adapter" => configuration::status(adapter),
        _ => Err(FlowError::usage(USAGE)),
    }
}
fn parse_options(args: &[String]) -> Result<std::collections::BTreeMap<&str, &str>> {
    let mut options = std::collections::BTreeMap::new();
    let (pairs, remainder) = args.as_chunks::<2>();
    for pair in pairs {
        ensure(
            ["--task", "--state-dir", "--spec"].contains(&pair[0].as_str()),
            USAGE,
        )?;
        ensure(
            options.insert(pair[0].as_str(), pair[1].as_str()).is_none(),
            "duplicate runtime option",
        )?;
    }
    ensure(remainder.is_empty(), USAGE)?;
    Ok(options)
}

fn action_run(action: &str, env: &mut TaskEnvironment, root: &Path, spec: &J) -> Result<J> {
    match action {
        "prepare" => Ok(json!({"root":prepare(env,&required(spec,"assignment")?)?})),
        "review-snapshot" => review::prepare(env, root, spec),
        "launch" => {
            launch(env, root, spec).map(|child| json!({"pid":child.id(),"phase":"started"}))
        }
        "status" => status::status(env, root),
        "tui-open" => tui::open(env, root),
        "tui-close" => tui::close(env, root),
        "tui-status" => tui::status(env, root),
        _ => native_action(action, env, root, spec),
    }
}
fn native_action(action: &str, env: &mut TaskEnvironment, root: &Path, spec: &J) -> Result<J> {
    match action {
        "create" => native::create(env, root, spec),
        "recover" => native::recover(env, root),
        "dispatch" => native::dispatch(env, root),
        "collect" => native::collect(env, root),
        _ => receiver_action(action, env, root, spec),
    }
}

fn receiver_action(action: &str, env: &mut TaskEnvironment, root: &Path, spec: &J) -> Result<J> {
    match action {
        "receiver-bootstrap" => native::receiver::bootstrap(env, root, spec),
        "receiver-start" => native::receiver::start(env, root, spec),
        "receiver-activate" => native::receiver::active::activate(env, root, spec),
        "receiver-recover" => native::receiver::recovery::recover(env, root, spec),
        "broker-recover" => {
            start_broker(env, root, spec)?;
            verify(env, root)
        }
        _ => Err(FlowError::usage(USAGE)),
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_missing_duplicate_and_unknown_cli_fields() {
        // The valid missing-task case reaches restore, which can create control
        // directories. Isolate it under the same lock as environment fixtures.
        let temp = crate::util::fs::test_tempdir();
        let _root = crate::task_env::test_env_root(&temp.path().join("control"));
        for args in [
            vec![],
            vec!["create"],
            vec!["create", "--task"],
            vec!["create", "--unknown", "x"],
            vec!["create", "--task", "a", "--task", "b"],
            vec!["create", "--task", "a", "--state-dir", "/nonexistent"],
        ] {
            assert_eq!(
                cli(&args.into_iter().map(str::to_string).collect::<Vec<_>>()),
                2
            );
        }
        assert_eq!(cli(&["--help".into()]), 0);
        {
            let adapter = "codex";
            let report = run(&[
                "configuration-status".into(),
                "--adapter".into(),
                adapter.into(),
            ])
            .unwrap();
            assert_eq!(report["supported"], adapter == "codex");
        }
        assert!(run(&["configuration-status".into()]).is_err());
    }
}

#[cfg(test)]
mod lifecycle_tests {
    use super::*;
    #[test]
    fn routes_each_action_without_creating_unmanaged_sessions() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::Builder::new()
            .permissions(std::fs::Permissions::from_mode(0o700))
            .tempdir()
            .unwrap();
        let _env_root = crate::task_env::test_env_root(&temp.path().join("registry"));
        let mut env = crate::task_env::create_environment(
            "runtime-cli",
            &temp.path().join("work"),
            vec![],
            &temp.path().join("tmp"),
        )
        .unwrap();
        let result = action_run(
            "prepare",
            &mut env,
            temp.path(),
            &json!({"assignment":"a1"}),
        )
        .unwrap();
        let root = PathBuf::from(result["root"].as_str().unwrap());
        let namespace = super::super::server_command(
            &env,
            &root,
            &env.workspace_root,
            Path::new("/bin/true"),
            &json!({"adapter":"codex"}),
            None,
        );
        // Building the command is independent of host bwrap availability;
        // spawning it fails closed if the executable/namespaces are unavailable.
        assert!(namespace.is_ok());
        let namespace = namespace.unwrap();
        let launcher = std::fs::canonicalize("/bin/true").unwrap();
        let digest = crate::util::pyjson::sha256_hex(&std::fs::read(&launcher).unwrap());
        let mut spec = json!({"adapter":"codex", "binary":"/bin/true",
            "cwd":env.workspace_root, "namespace_launcher":{
            "path":launcher,"sha256":digest
        }});
        let selected = super::super::server_command(
            &env,
            &root,
            &env.workspace_root,
            Path::new("/bin/true"),
            &spec,
            None,
        )
        .unwrap();
        assert_eq!(selected.get_program(), launcher);
        assert_eq!(
            selected.get_args().collect::<Vec<_>>(),
            namespace.get_args().collect::<Vec<_>>()
        );
        spec["namespace_launcher"]["sha256"] = json!("0".repeat(64));
        assert!(super::super::launch(&mut env, &root, &spec).is_err());
        assert!(!root.join("runtime.json").exists());
        for action in [
            "launch",
            "create",
            "receiver-bootstrap",
            "receiver-start",
            "receiver-activate",
            "receiver-recover",
            "broker-recover",
            "recover",
            "dispatch",
            "collect",
            "status",
            "invalid",
        ] {
            assert!(action_run(action, &mut env, &root, &json!({})).is_err());
        }
        crate::task_env::destroy_environment(&env).unwrap();
    }
}
