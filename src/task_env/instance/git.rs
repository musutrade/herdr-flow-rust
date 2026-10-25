//! Detached Git worktree creation with durable pre-Git intent and exact identity.
use super::{
    intent as get_intent, require, roots, CreateSpec, CreationIntent, CreationPhase, GitIdentity,
    InstanceBinding, TaskEnvironment,
};
use crate::{util, FlowError, Result};
use std::{
    fs,
    path::{Path, PathBuf},
    time::Duration,
};

fn options() -> util::process::CommandOptions {
    let mut options = util::process::git_options(Duration::from_secs(60));
    options.env.insert("GIT_OPTIONAL_LOCKS".into(), "0".into());
    options.env.insert("GIT_NO_LAZY_FETCH".into(), "1".into());
    options
        .env
        .insert("GIT_NO_REPLACE_OBJECTS".into(), "1".into());
    options.creation_mask = Some(0o077);
    options.stdout_limit = 1024 * 1024;
    options.stderr_limit = 64 * 1024;
    options
}

fn run(repo: &Path, args: &[&str]) -> Result<String> {
    execute(repo, args, &options())
}

fn owned_run(env: &TaskEnvironment, repo: &Path, args: &[&str]) -> Result<String> {
    let mut options = options();
    let binding = &get_intent(env)?.binding;
    for (key, value) in [
        (super::super::MARKER_ENV, env.instance_id.clone()),
        ("HERDR_TASK_ID", binding.task_id.clone()),
        ("HERDR_TASK_EPOCH", binding.epoch.to_string()),
        ("HERDR_TASK_QUEUE", binding.queue_id.clone()),
    ] {
        options.env.insert(key.into(), value);
    }
    options.kill_on_parent_death = true;
    execute(repo, args, &options)
}

fn execute(repo: &Path, args: &[&str], options: &util::process::CommandOptions) -> Result<String> {
    let mut argv = vec![
        "git".into(),
        "-C".into(),
        repo.to_string_lossy().into_owned(),
    ];
    argv.extend(args.iter().map(|a| a.to_string()));
    let result = util::process::run_command(&argv, options)?;
    require(
        result.success(),
        &format!("instance Git command failed: {}", result.stderr_text()),
    )?;
    Ok(result.stdout_text().trim().to_string())
}

pub(super) fn intent(binding: &InstanceBinding, spec: &CreateSpec) -> Result<CreationIntent> {
    let repository = util::resolve_path(&spec.repository);
    let repository_identity = roots::identity(&repository, true)?;
    let common_dir = PathBuf::from(run(
        &repository,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )?);
    let common_identity = roots::identity(&common_dir, true)?;
    reject_filters(&repository)?;
    let commit = run(
        &repository,
        &[
            "rev-parse",
            "--verify",
            &format!("{}^{{commit}}", spec.commit),
        ],
    )?;
    require(
        commit == spec.commit,
        "commit is not the exact requested commit object",
    )?;
    validate_new_roots(spec, &common_dir)?;
    Ok(CreationIntent {
        binding: binding.clone(),
        lifecycle_queue: None,
        phase: CreationPhase::Prepared,
        repository,
        repository_identity,
        common_dir,
        common_identity,
        commit,
        checkout: util::resolve_path(&spec.workspace_root).join("checkout"),
        checkout_identity: None,
        git_identity: None,
        git_admin_before: None,
    })
}

fn validate_new_roots(spec: &CreateSpec, common_dir: &Path) -> Result<()> {
    for root in [&spec.workspace_root, &spec.temp_root] {
        require(
            !util::overlap(common_dir, root),
            "instance root overlaps Git control directory",
        )?;
        // A new instance never takes over even an empty pre-existing root.
        require(
            fs::symlink_metadata(root).is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound),
            "instance root must be absent; no adoption",
        )?;
    }
    Ok(())
}

fn source(i: &CreationIntent) -> Result<()> {
    roots::verify(&i.repository, i.repository_identity, true)?;
    roots::verify(&i.common_dir, i.common_identity, true)?;
    let current = run(
        &i.repository,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )?;
    require(
        Path::new(&current) == i.common_dir,
        "source Git repository changed",
    )
}

pub(super) fn verify_recorded(i: &CreationIntent) -> Result<()> {
    source(i)?;
    if let Some(expected) = &i.git_identity {
        require(
            &inspect(i)? == expected,
            "Git administrative identity changed",
        )?;
    }
    Ok(())
}

fn inspect(i: &CreationIntent) -> Result<GitIdentity> {
    let git_file = i.checkout.join(".git");
    let git_file_identity = roots::identity(&git_file, false)?;
    let admin_dir = admin_pointer(&git_file, &i.common_dir)?;
    let admin_identity = roots::identity(&admin_dir, true)?;
    reciprocal_links(i, &admin_dir)?;
    require(listed(i)?, "worktree absent from exact Git registry")?;
    Ok(GitIdentity {
        git_file_identity,
        admin_dir,
        admin_identity,
    })
}

fn admin_pointer(git_file: &Path, common_dir: &Path) -> Result<PathBuf> {
    let pointer = bounded_text(git_file)?;
    let admin_dir = PathBuf::from(
        pointer
            .trim()
            .strip_prefix("gitdir: ")
            .ok_or_else(|| FlowError::progress("invalid worktree Git pointer"))?,
    );
    require(
        admin_dir.is_absolute()
            && admin_dir.parent() == Some(common_dir.join("worktrees").as_path()),
        "foreign Git administrative directory",
    )?;
    Ok(admin_dir)
}

fn reciprocal_links(i: &CreationIntent, admin_dir: &Path) -> Result<()> {
    let backlink = bounded_text(&admin_dir.join("gitdir"))?;
    require(
        Path::new(backlink.trim()) == i.checkout.join(".git"),
        "Git worktree backlink changed",
    )?;
    let common = bounded_text(&admin_dir.join("commondir"))?;
    require(
        util::resolve_path(&admin_dir.join(common.trim())) == i.common_dir,
        "worktree common repository changed",
    )
}

fn bounded_text(path: &Path) -> Result<String> {
    use std::io::Read;
    require(
        fs::symlink_metadata(path)?.is_file(),
        "Git identity file is not regular",
    )?;
    let mut data = Vec::new();
    fs::File::open(path)?.take(16385).read_to_end(&mut data)?;
    require(data.len() <= 16384, "Git identity file exceeds budget")?;
    String::from_utf8(data).map_err(|_| FlowError::progress("Git identity file is not UTF-8"))
}

fn listed(i: &CreationIntent) -> Result<bool> {
    let listing = run(&i.repository, &["worktree", "list", "--porcelain", "-z"])?;
    let wanted = format!("worktree {}", i.checkout.display());
    Ok(listing.split('\0').filter(|v| *v == wanted).count() == 1)
}

pub(super) fn verify_ready(i: &CreationIntent) -> Result<()> {
    source(i)?;
    let expected = i
        .git_identity
        .as_ref()
        .ok_or_else(|| FlowError::progress("missing worktree Git identity"))?;
    require(
        &inspect(i)? == expected,
        "Git administrative identity changed",
    )
}

pub(super) fn materialize(env: &mut TaskEnvironment) -> Result<()> {
    source(get_intent(env)?)?;
    if get_intent(env)?.phase == CreationPhase::Prepared {
        prepare_git(env)?;
    }
    create_git(env)?;
    populate(env)
}

fn empty_checkout(i: &CreationIntent) -> Result<()> {
    require(
        fs::read_dir(&i.checkout)?.next().is_none(),
        "new checkout is not empty",
    )?;
    require(
        !listed(i)?,
        "checkout already registered; refusing adoption",
    )
}

fn prepare_git(env: &mut TaskEnvironment) -> Result<()> {
    let i = get_intent(env)?;
    empty_checkout(i)?;
    let before = admin_snapshot(i)?;
    env.creation.as_mut().unwrap().git_admin_before = Some(before);
    env.creation.as_mut().unwrap().phase = CreationPhase::GitPending;
    env.persist()
}

fn create_git(env: &mut TaskEnvironment) -> Result<()> {
    let i = get_intent(env)?;
    if i.phase != CreationPhase::GitPending {
        return verify_recorded(i);
    }
    roots::verify_recorded(env)?;
    if !i.checkout.join(".git").try_exists()? {
        add_git(env)?;
    }
    roots::verify_recorded(env)?;
    record_git(env)
}

fn add_git(env: &TaskEnvironment) -> Result<()> {
    let i = get_intent(env)?;
    require(
        i.git_admin_before.as_ref() == Some(&admin_snapshot(i)?),
        "Git administration changed after intent; unresolved partial add, no replay",
    )?;
    empty_checkout(i)?;
    let path = i
        .checkout
        .to_str()
        .ok_or_else(|| FlowError::progress("non-UTF8 checkout"))?;
    owned_run(
        env,
        &i.repository,
        &[
            "worktree",
            "add",
            "--detach",
            "--no-checkout",
            path,
            &i.commit,
        ],
    )?;
    Ok(())
}

fn initial_head(i: &CreationIntent) -> Result<()> {
    require(
        run(&i.checkout, &["rev-parse", "HEAD"])? == i.commit,
        "creation HEAD changed",
    )
}

fn record_git(env: &mut TaskEnvironment) -> Result<()> {
    let i = get_intent(env)?;
    let id = inspect(i)?;
    initial_head(i)?;
    env.creation.as_mut().unwrap().git_identity = Some(id);
    env.creation.as_mut().unwrap().phase = CreationPhase::GitRegistered;
    env.persist()
}

fn populate(env: &mut TaskEnvironment) -> Result<()> {
    let i = get_intent(env)?;
    verify_ready(i)?;
    reject_filters(&i.checkout)?;
    initial_head(i)?;
    populate_files(env)?;
    roots::verify_recorded(env)?;
    verify_populated(i)
}

fn populate_files(env: &TaskEnvironment) -> Result<()> {
    let i = get_intent(env)?;
    // Fill missing files only, without force or deleting obstacles.
    owned_run(env, &i.checkout, &["read-tree", &i.commit])?;
    owned_run(env, &i.checkout, &["checkout-index", "--all", "--quiet"])?;
    Ok(())
}

fn verify_populated(i: &CreationIntent) -> Result<()> {
    verify_ready(i)?;
    require(
        run(
            &i.checkout,
            &["status", "--porcelain", "--untracked-files=all"],
        )?
        .is_empty(),
        "created worktree is not clean",
    )
}

fn admin_snapshot(
    i: &CreationIntent,
) -> Result<std::collections::BTreeMap<String, super::RootIdentity>> {
    let path = i.common_dir.join("worktrees");
    let entries = match fs::read_dir(&path) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Default::default()),
        Err(e) => return Err(e.into()),
    };
    collect_admin(&path, entries)
}

fn collect_admin(
    path: &Path,
    entries: fs::ReadDir,
) -> Result<std::collections::BTreeMap<String, super::RootIdentity>> {
    roots::identity(path, true)?;
    let mut result = std::collections::BTreeMap::new();
    for entry in entries {
        require(
            result.len() < 1024,
            "Git worktree administration exceeds observation budget",
        )?;
        let entry = entry?;
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| FlowError::progress("non-UTF8 Git worktree administration"))?;
        result.insert(name, roots::identity(&entry.path(), true)?);
    }
    Ok(result)
}

// Checkout filters are external execution, not part of this creation capability.
fn reject_filters(repo: &Path) -> Result<()> {
    let config = run(repo, &["config", "--includes", "--null", "--list"])?;
    for row in config.split('\0') {
        let name = row
            .split('\n')
            .next()
            .unwrap_or_default()
            .to_ascii_lowercase();
        require(
            !name.contains("partialclone") && !name.ends_with(".promisor"),
            "partial/promisor repositories are unsupported for task instance creation",
        )?;
        require(
            !name.starts_with("filter."),
            "source checkout filters are unsupported for task instance creation",
        )?;
    }
    Ok(())
}
