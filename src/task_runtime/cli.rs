use super::*;
const USAGE: &str = "Usage: herdr-flow task-runtime <prepare|launch|create|receiver-bootstrap|recover|dispatch|collect|status> --task TASK --state-dir DIR [--spec FILE]\nUses an existing owned task environment; never attaches to a shared server.";

fn run(args: &[String]) -> Result<J> {
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
fn parse_options(args: &[String]) -> Result<std::collections::BTreeMap<&str, &str>> {
    let mut options = std::collections::BTreeMap::new();
    let mut pairs = args.chunks_exact(2);
    for pair in &mut pairs {
        ensure(
            ["--task", "--state-dir", "--spec"].contains(&pair[0].as_str()),
            USAGE,
        )?;
        ensure(
            options.insert(pair[0].as_str(), pair[1].as_str()).is_none(),
            "duplicate runtime option",
        )?;
    }
    ensure(pairs.remainder().is_empty(), USAGE)?;
    Ok(options)
}

fn action_run(action: &str, env: &mut TaskEnvironment, root: &Path, spec: &J) -> Result<J> {
    match action {
        "prepare" => Ok(json!({"root":prepare(env,&required(spec,"assignment")?)?})),
        "launch" => {
            launch(env, root, spec).map(|child| json!({"pid":child.id(),"phase":"started"}))
        }
        "create" => native::create(env, root, spec),
        "receiver-bootstrap" => native::receiver::bootstrap(env, root, spec),
        "recover" => native::recover(env, root),
        "dispatch" => native::dispatch(env, root),
        "collect" => native::collect(env, root),
        "status" => status::status(env, root),
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
