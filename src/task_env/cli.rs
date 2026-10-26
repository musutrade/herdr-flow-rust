//! Explicit preparatory environment entry; recovery keeps the library's semantics.
use std::collections::BTreeMap;
use std::io::Read;
use std::path::PathBuf;

use serde::Deserialize;
use serde_json::{json, Value};

use crate::{FlowError, Result};

const USAGE: &str = "Usage: herdr-flow task-env create --task TASK --spec FILE\n       herdr-flow task-env status --task TASK\n       herdr-flow task-env recover --task TASK\nPreparatory entry only: no Herdr worktree/session or generic active/no-delivery finish.\nstatus is a read-only snapshot, never cleanup authorization.\nrecover calls existing interrupted recovery once, from an external supervisor:\nit may stop owned processes and delete disposable roots, including Active WITH a cleanup plan.\nActive WITHOUT a plan is a no-op. Missing-task recovery may create empty control directories.";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Spec {
    workspace_root: PathBuf,
    temp_root: PathBuf,
    delivery_targets: Vec<Target>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Target {
    path: PathBuf,
    target_type: String,
    description: Option<String>,
}

fn options(args: &[String]) -> Result<BTreeMap<&str, &str>> {
    let mut result = BTreeMap::new();
    let (pairs, remainder) = args.as_chunks::<2>();
    for pair in pairs {
        if !["--task", "--spec"].contains(&pair[0].as_str())
            || pair[1].is_empty()
            || result.insert(pair[0].as_str(), pair[1].as_str()).is_some()
        {
            return Err(FlowError::usage(USAGE));
        }
    }
    if !remainder.is_empty() {
        return Err(FlowError::usage(USAGE));
    }
    Ok(result)
}

fn read_spec(path: &str) -> Result<Spec> {
    const LIMIT: u64 = 1024 * 1024;
    let mut bytes = Vec::new();
    std::fs::File::open(path)?
        .take(LIMIT + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > LIMIT {
        return Err(FlowError::usage("environment spec exceeds 1 MiB"));
    }
    let spec: Spec = serde_json::from_slice(&bytes)?;
    validate_spec(&spec)?;
    Ok(spec)
}

fn validate_spec(spec: &Spec) -> Result<()> {
    if spec
        .delivery_targets
        .iter()
        .any(|t| t.target_type.trim().is_empty())
    {
        return Err(FlowError::usage("delivery target_type must be nonempty"));
    }
    if !spec.workspace_root.is_absolute()
        || !spec.temp_root.is_absolute()
        || spec.delivery_targets.iter().any(|t| !t.path.is_absolute())
    {
        return Err(FlowError::usage(
            "roots and delivery targets must be absolute",
        ));
    }
    Ok(())
}

fn create(task: &str, path: &str) -> Result<Value> {
    let spec = read_spec(path)?;
    let targets = spec
        .delivery_targets
        .into_iter()
        .map(|t| super::DeliveryTarget {
            path: t.path,
            target_type: t.target_type,
            description: t.description,
        })
        .collect();
    let env = super::create_environment(task, &spec.workspace_root, targets, &spec.temp_root)?;
    Ok(
        json!({"task_id":env.task_id,"instance_id":env.instance_id,"state":env.state,
        "workspace_root":env.workspace_root,"temp_root":env.temp_root,
        "delivery_targets":env.delivery_targets}),
    )
}

fn run(args: &[String]) -> Result<Value> {
    let action = args.first().ok_or_else(|| FlowError::usage(USAGE))?;
    let opts = options(&args[1..])?;
    let task = opts.get("--task").ok_or_else(|| FlowError::usage(USAGE))?;
    super::validate_task_id(task)?;
    match (action.as_str(), opts.get("--spec")) {
        ("create", Some(path)) => create(task, path),
        ("status", None) => super::status(task),
        ("recover", None) => Ok(json!({"task_id":task,
            "recovered":super::recover_interrupted(task)?})),
        _ => Err(FlowError::usage(USAGE)),
    }
}

pub fn cli(args: &[String]) -> i32 {
    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!("{USAGE}");
        return 0;
    }
    match run(args) {
        Ok(value) => {
            println!("{value}");
            i32::from(value["state"] == "unknown")
        }
        Err(error) => {
            eprintln!("{error}");
            2
        }
    }
}
