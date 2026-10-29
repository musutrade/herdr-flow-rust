use super::*;
const USAGE: &str = "Usage: herdr-flow task-runtime <prepare|review-snapshot|launch|create|receiver-bootstrap|receiver-start|receiver-activate|receiver-recover|broker-recover|recover|dispatch|collect|status|recovery-start|recovery-status|publication-status|human-status|human-request|human-acquire|human-return|tui-open|tui-status|tui-close> --task TASK --state-dir DIR [--spec FILE]\n       herdr-flow task-runtime publication-watch --state-dir DIR\n       herdr-flow task-runtime configuration-status --adapter <codex>\nLogin/provider/billing belong to upstream. Native TUI resumes the same task in Herdr; closing it does not cancel the task. Human writer transfer requires an explicit request, successful acquisition and recovery verification on return. Codex upstream mode connects to an existing Unix app-server; isolated-fixture is only for credential-free protocol tests.";

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
    if let Some(result) = owned_action(action, &root, task, &spec) {
        return result;
    }
    let mut env = crate::task_env::restore_environment(task)?;
    action_run(action, &mut env, &root, &spec)
}

fn owned_action(action: &str, root: &Path, task: &str, spec: &J) -> Option<Result<J>> {
    match action {
        "human-request" | "human-acquire" | "human-return" => {
            Some(human::action(root, task, action, spec))
        }
        _ => read_only_status(action, root, task),
    }
}
fn read_only_status(action: &str, root: &Path, task: &str) -> Option<Result<J>> {
    match action {
        "human-status" => Some(human::status(root, task)),
        "recovery-status" => Some(bound_status(root, task, recovery::status)),
        "publication-status" => Some(bound_status(root, task, publication::status)),
        _ => None,
    }
}

fn bound_status(root: &Path, task: &str, inspect: fn(&Path) -> Result<J>) -> Result<J> {
    crate::lifecycle::runtime_permit(root)
        .and_then(|permit| {
            permit.ok_or_else(|| FlowError::guard("runtime status requires an isolated Owner"))
        })
        .and_then(|permit| {
            ensure(
                permit.owner().task_id == task,
                "runtime status task changed",
            )
            .and_then(|()| inspect(root))
        })
}

fn special(args: &[String]) -> Option<Result<J>> {
    if args
        .first()
        .is_some_and(|arg| arg == "configuration-status")
    {
        return Some(configuration_status(args));
    }
    watch_special(args)
}

fn watch_special(args: &[String]) -> Option<Result<J>> {
    match args {
        [action, option, path] if option == "--state-dir" => {
            background_action(action, Path::new(path))
        }
        _ => None,
    }
}

fn background_action(action: &str, root: &Path) -> Option<Result<J>> {
    match action {
        "publication-watch" => Some(publication::watch(root).map(|_| json!({"stopped":true}))),
        "recovery-watch" => Some(recovery::watch(root).map(|_| json!({"stopped":true}))),
        "tui-serve" => Some(tui::serve(root).map(|code| json!({"exit_code":code}))),
        "receiver-watch" => {
            Some(native::receiver::active::watch(root).map(|_| json!({"stopped":true})))
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
        "recovery-start" => recovery::start(env, root),
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
    fn publication_status_is_read_only_and_requires_the_coordinator_owner() {
        let root = crate::util::fs::test_tempdir();
        let report = read_only_status("publication-status", root.path(), "ours")
            .unwrap()
            .unwrap_err();
        assert!(report.to_string().contains("isolated Owner"));
        assert!(read_only_status("other", root.path(), "ours").is_none());
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
        for action in [
            "human-status",
            "human-request",
            "human-acquire",
            "human-return",
        ] {
            let error = run(&[
                action.into(),
                "--task".into(),
                "ours".into(),
                "--state-dir".into(),
                root.path().to_string_lossy().into_owned(),
            ])
            .unwrap_err();
            assert!(error.message.contains("isolated Owner"));
            assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
        }
        let error = run(&[
            "publication-status".into(),
            "--task".into(),
            "ours".into(),
            "--state-dir".into(),
            root.path().to_string_lossy().into_owned(),
        ])
        .unwrap_err();
        assert!(error.to_string().contains("isolated Owner"));
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
    }

    #[test]
    fn publication_watcher_refuses_an_untrusted_process_marker() {
        let root = crate::util::fs::test_tempdir();
        let owner = crate::lifecycle::Owner {
            queue_root: "/unused".into(),
            queue_id: "q".into(),
            task_id: "ours".into(),
            epoch: 1,
            instance_id: "untrusted-publication-cli-test".into(),
        };
        util::atomic_write_json(
            &root.path().join("publisher.json"),
            &json!({"root":root.path(),"owner":owner}),
        )
        .unwrap();
        let args = [
            "publication-watch".into(),
            "--state-dir".into(),
            root.path().to_string_lossy().into_owned(),
        ];
        let error = special(&args).unwrap().unwrap_err();
        assert!(error.to_string().contains("watcher identity changed"));
        assert!(!root.path().join("watch.json").exists());
        assert!(special(&["publication-watch".into(), "--unknown".into(), "x".into()]).is_none());
    }

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
