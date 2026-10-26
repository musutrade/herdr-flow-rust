//! Fixed acceptance snapshots within an already-owned disposable root.
//! No external worktree administration or new environment/cleanup authority.
use super::{RootIdentity, TaskEnvironment};
use crate::{lifecycle::Owner, model::J, util, FlowError, Result};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    os::unix::fs::DirBuilderExt,
    path::{Path, PathBuf},
    time::Duration,
};

const OPERATION: &str = "HERDR_REVIEW_SNAPSHOT";

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ReviewWorkspace {
    pub owner: Owner,
    pub binding: J,
    pub wait: String,
    pub runtime: PathBuf,
    pub path: PathBuf,
    pub source: PathBuf,
    pub phase: Phase,
    pub identity: Option<RootIdentity>,
    pub git_identity: Option<RootIdentity>,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Reserved,
    Directory,
    Cloning,
    Cloned,
    CheckingOut,
    Ready,
}

fn require(ok: bool, message: &str) -> Result<()> {
    if ok {
        Ok(())
    } else {
        Err(FlowError::guard(message))
    }
}

fn save(env: &mut TaskEnvironment, record: &ReviewWorkspace) -> Result<()> {
    let key = assignment(record)?;
    env.review_workspaces.insert(key, record.clone());
    env.persist()
}

fn assignment(record: &ReviewWorkspace) -> Result<String> {
    crate::model::string(&record.binding, "assignment")
        .ok_or_else(|| FlowError::guard("review assignment missing"))
}

pub(crate) fn validate(env: &TaskEnvironment, record: &ReviewWorkspace) -> Result<()> {
    require(
        super::isolation::owner(env).as_ref() == Some(&record.owner),
        "review Owner changed",
    )?;
    let key = assignment(record)?;
    require(
        record.path == env.workspace_root.join("reviews").join(&key)
            && record.runtime == env.temp_root.join("rt").join(&key),
        "review resource path changed",
    )?;
    require(
        util::resolve_path(&record.path) == record.path,
        "review path is not canonical",
    )?;
    super::validate_ancestors(&record.path)
}

/// An existing record is resumed; an unknown directory is never adopted.
pub(crate) fn materialize(
    env: &mut TaskEnvironment,
    mut record: ReviewWorkspace,
) -> Result<ReviewWorkspace> {
    let _permit = super::isolation::mutation_permit(env)?;
    validate(env, &record)?;
    quiescent(&record)?;
    materialize_directory(env, &mut record)?;
    materialize_git(env, &mut record)?;
    verify(&record)?;
    finish(env, &mut record)?;
    Ok(record)
}

fn materialize_directory(env: &mut TaskEnvironment, record: &mut ReviewWorkspace) -> Result<()> {
    if record.phase == Phase::Reserved {
        directory(env, record)?;
    }
    verify_directory(record)
}

fn materialize_git(env: &mut TaskEnvironment, record: &mut ReviewWorkspace) -> Result<()> {
    if record.phase == Phase::Directory {
        clone_once(env, record)?;
    }
    if record.phase == Phase::Cloning {
        finish_clone(env, record)?;
    }
    if record.phase == Phase::Cloned {
        checkout_once(env, record)?;
    }
    Ok(())
}

fn finish(env: &mut TaskEnvironment, record: &mut ReviewWorkspace) -> Result<()> {
    if record.phase == Phase::CheckingOut {
        record.phase = Phase::Ready;
        save(env, record)?;
    }
    require(record.phase == Phase::Ready, "review snapshot is not ready")
}

fn directory(env: &mut TaskEnvironment, record: &mut ReviewWorkspace) -> Result<()> {
    util::ensure_private_dir(record.path.parent().unwrap(), true)?;
    fs::DirBuilder::new().mode(0o700).create(&record.path)?;
    record.identity = Some(identity(&record.path)?);
    record.phase = Phase::Directory;
    save(env, record)
}

fn identity(path: &Path) -> Result<RootIdentity> {
    let metadata = fs::symlink_metadata(path)?;
    require(
        metadata.is_dir() && !metadata.file_type().is_symlink(),
        "review directory type changed",
    )?;
    Ok(RootIdentity::of(&metadata))
}

fn verify_directory(record: &ReviewWorkspace) -> Result<()> {
    require(
        Some(identity(&record.path)?) == record.identity,
        "review directory identity changed",
    )
}

fn clone_once(env: &mut TaskEnvironment, record: &mut ReviewWorkspace) -> Result<()> {
    record.phase = Phase::Cloning;
    save(env, record)?;
    run(
        record,
        &record.source,
        &[
            "clone",
            "--no-local",
            "--no-checkout",
            "--",
            text(&record.source)?,
            text(&record.path)?,
        ],
    )?;
    finish_clone(env, record)
}

fn finish_clone(env: &mut TaskEnvironment, record: &mut ReviewWorkspace) -> Result<()> {
    verify_directory(record)?;
    let git_id = clone_identity(record)?;
    verify_candidate_object(record)?;
    record.git_identity = Some(git_id);
    record.phase = Phase::Cloned;
    save(env, record)
}

fn clone_identity(record: &ReviewWorkspace) -> Result<RootIdentity> {
    let git_id = identity(&record.path.join(".git"))?;
    if let Some(expected) = record.git_identity {
        require(git_id == expected, "review Git identity changed")?;
    }
    verify_common(record)?;
    Ok(git_id)
}

fn verify_common(record: &ReviewWorkspace) -> Result<()> {
    let common = run(
        record,
        &record.path,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )?;
    require(
        Path::new(&common) == record.path.join(".git"),
        "review must have its own Git repository",
    )
}

fn verify_candidate_object(record: &ReviewWorkspace) -> Result<()> {
    let object = format!("{}^{{commit}}", candidate(record)?);
    require(
        run(record, &record.path, &["rev-parse", "--verify", &object])? == candidate(record)?,
        "review commit missing",
    )
}

fn checkout_once(env: &mut TaskEnvironment, record: &mut ReviewWorkspace) -> Result<()> {
    record.phase = Phase::CheckingOut;
    save(env, record)?;
    run(
        record,
        &record.path,
        &["checkout", "--detach", candidate(record)?],
    )?;
    Ok(())
}

pub(crate) fn verify(record: &ReviewWorkspace) -> Result<()> {
    verify_directory(record)?;
    verify_git_identity(record)?;
    verify_common(record)?;
    verify_head(record)?;
    verify_contents(record)
}

fn verify_git_identity(record: &ReviewWorkspace) -> Result<()> {
    require(
        Some(identity(&record.path.join(".git"))?) == record.git_identity,
        "review Git identity changed",
    )
}

fn verify_head(record: &ReviewWorkspace) -> Result<()> {
    require(
        run(record, &record.path, &["rev-parse", "HEAD"])? == candidate(record)?,
        "review snapshot HEAD changed",
    )
}

fn verify_contents(record: &ReviewWorkspace) -> Result<()> {
    require(
        run(record, &record.path, &["diff", "--exit-code", "HEAD", "--"])?.is_empty(),
        "review snapshot tracked contents changed",
    )
}

fn candidate(record: &ReviewWorkspace) -> Result<&str> {
    record.binding["candidate"]
        .as_str()
        .filter(|s| crate::model::is_full_sha(s))
        .ok_or_else(|| FlowError::guard("review requires a full commit SHA"))
}

fn text(path: &Path) -> Result<&str> {
    path.to_str()
        .ok_or_else(|| FlowError::guard("review path must be UTF-8"))
}

fn run(record: &ReviewWorkspace, cwd: &Path, args: &[&str]) -> Result<String> {
    let mut options = util::process::git_options(Duration::from_secs(60));
    options.kill_on_parent_death = true;
    options.creation_mask = Some(0o077);
    options
        .env
        .insert(super::MARKER_ENV.into(), record.owner.instance_id.clone());
    options.env.insert(OPERATION.into(), assignment(record)?);
    options
        .env
        .insert("HERDR_TASK_ID".into(), record.owner.task_id.clone());
    options
        .env
        .insert("HERDR_TASK_EPOCH".into(), record.owner.epoch.to_string());
    options
        .env
        .insert("HERDR_TASK_QUEUE".into(), record.owner.queue_id.clone());
    let mut argv = vec![
        "git".into(),
        "--no-replace-objects".into(),
        "-C".into(),
        text(cwd)?.into(),
    ];
    argv.extend(args.iter().map(|s| (*s).to_owned()));
    let result = util::process::run_command(&argv, &options)?;
    require(
        result.success(),
        "review snapshot Git operation failed; inspect without replay",
    )?;
    Ok(result.stdout_text().trim().to_owned())
}

fn quiescent(record: &ReviewWorkspace) -> Result<()> {
    let marker = format!("{OPERATION}={}", assignment(record)?);
    for process in super::discovery::marked_processes(&record.owner.instance_id)? {
        let bytes = fs::read(format!("/proc/{}/environ", process.pid))?;
        require(
            !bytes
                .split(|b| *b == 0)
                .any(|entry| entry == marker.as_bytes()),
            "review snapshot operation still running",
        )?;
    }
    Ok(())
}
