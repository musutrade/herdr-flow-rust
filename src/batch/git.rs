//! Isolated, journaled linear-range integration. No developer checkout is
//! written. Only Git objects and a private index are changed.
//!
//! A planned commit is persisted before moving the private ref; recovery
//! compares that ref with both endpoints.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{json, Value};

use crate::error::{ErrorKind, FlowError, Result};
use crate::logging;
use crate::model::{self, J};
use crate::util;

const ALLOWED_COMMANDS: &[&str] = &[
    "apply",
    "cat-file",
    "check-ref-format",
    "checkout",
    "commit-tree",
    "diff-tree",
    "fetch",
    "init",
    "merge-base",
    "read-tree",
    "remote",
    "rev-list",
    "rev-parse",
    "show",
    "status",
    "update-ref",
    "worktree",
    "write-tree",
];

fn validate_args(args: &[String]) -> Result<()> {
    if args.is_empty() {
        return Err(FlowError::progress(
            "batch Git operation requires a command",
        ));
    }
    for argument in args {
        if argument == "-c" || argument.starts_with("--config-env") {
            return Err(FlowError::progress(
                "batch Git does not permit global config overrides",
            ));
        }
    }
    if !ALLOWED_COMMANDS.contains(&args[0].as_str()) {
        logging::log_event(
            "git_command_rejected",
            "batch_git",
            "batch Git command not allowed",
        );
        return Err(FlowError::progress(format!(
            "batch Git command not allowed: {}",
            args[0]
        )));
    }
    Ok(())
}

fn build_environment(
    index: Option<&Path>,
    extra: &BTreeMap<String, String>,
) -> BTreeMap<String, String> {
    let mut settings: BTreeMap<String, String> = std::env::vars()
        .filter(|(key, _)| !key.starts_with("GIT_"))
        .collect();
    let email =
        std::env::var("HERDR_BATCH_EMAIL").unwrap_or_else(|_| "batch@example.invalid".to_string());
    for (key, value) in [
        ("GIT_CONFIG_NOSYSTEM", "1"),
        ("GIT_CONFIG_GLOBAL", "/dev/null"),
        ("GIT_TERMINAL_PROMPT", "0"),
        ("GIT_AUTHOR_NAME", "Herdr batch"),
        ("GIT_COMMITTER_NAME", "Herdr batch"),
    ] {
        settings.insert(key.to_string(), value.to_string());
    }
    settings.insert("GIT_AUTHOR_EMAIL".to_string(), email.clone());
    settings.insert("GIT_COMMITTER_EMAIL".to_string(), email);
    if let Some(index) = index {
        settings.insert("GIT_INDEX_FILE".to_string(), index.display().to_string());
    }
    for (key, value) in extra {
        settings.insert(key.clone(), value.clone());
    }
    settings
}

pub fn run(
    cwd: &Path,
    args: &[String],
    data: Option<&[u8]>,
    index: Option<&Path>,
    env: &BTreeMap<String, String>,
    allow_failure: bool,
) -> Result<util::process::CommandOutput> {
    validate_args(args)?;
    let settings = build_environment(index, env);
    let cwd_path = util::resolve_path(cwd);
    if !cwd_path.is_absolute() {
        return Err(FlowError::progress("batch Git cwd must be absolute"));
    }
    if !cwd_path.exists() {
        return Err(FlowError::progress("batch Git cwd must exist"));
    }
    let mut argv = vec![
        "git".to_string(),
        "-c".to_string(),
        "core.hooksPath=/dev/null".to_string(),
        "-C".to_string(),
        cwd_path.display().to_string(),
    ];
    argv.extend(args.iter().cloned());
    let options = util::process::CommandOptions {
        cwd: None,
        timeout: Duration::from_secs(60),
        env: settings,
        clear_env: true,
        stdin: data.map(ToOwned::to_owned),
    };
    let output = util::process::run_command(&argv, &options).map_err(|error| {
        if error.kind == ErrorKind::Timeout {
            FlowError::progress(
                "batch Git operation timed out; inspect its journal before recovery",
            )
        } else {
            error
        }
    })?;
    if output.status != 0 && !allow_failure {
        return Err(FlowError::progress(format!(
            "batch Git operation failed: {}; private integration state retained",
            util::sanitize_stderr(&output.stderr)
        )));
    }
    Ok(output)
}

pub fn output(cwd: &Path, args: &[String]) -> Result<String> {
    let output = run(cwd, args, None, None, &BTreeMap::new(), false)?;
    Ok(output.stdout_text().trim().to_string())
}

fn claim_owner(marker: &Path, owner: &J) -> Result<()> {
    claim_open(open_marker(marker), marker, owner)
}

fn open_marker(marker: &Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(marker)
}

fn claim_open(opened: std::io::Result<std::fs::File>, marker: &Path, owner: &J) -> Result<()> {
    opened.map_or_else(
        |error| claim_existing(error, marker, owner),
        |file| claim_write(file, owner),
    )
}

fn claim_existing(error: std::io::Error, marker: &Path, owner: &J) -> Result<()> {
    if error.kind() == std::io::ErrorKind::AlreadyExists {
        claim_marker(marker, owner)
    } else {
        Err(error.into())
    }
}

fn claim_marker(marker: &Path, owner: &J) -> Result<()> {
    let existing = util::read_json(marker)?;
    validate_owner(&existing, owner)
}

fn validate_owner(existing: &J, owner: &J) -> Result<()> {
    if *existing != *owner {
        Err(FlowError::progress(
            "integration workspace belongs to another batch",
        ))
    } else {
        Ok(())
    }
}

fn claim_write(mut file: std::fs::File, owner: &J) -> Result<()> {
    let mut text = serde_json::to_string(owner)?;
    text.push('\n');
    claim_write_bytes(&mut file, text.as_bytes())
}

fn claim_write_bytes(file: &mut std::fs::File, data: &[u8]) -> Result<()> {
    use std::io::Write;
    file.write_all(data)?;
    claim_sync(file)
}

fn claim_sync(file: &mut std::fs::File) -> Result<()> {
    file.sync_all()?;
    Ok(())
}

fn prepare_workspace(root: &Path, marker: &Path, owner: &J) -> Result<()> {
    if marker.exists() {
        return prepare_existing_marker(marker, owner);
    }
    prepare_new_workspace(root, marker, owner)
}

fn prepare_existing_marker(marker: &Path, owner: &J) -> Result<()> {
    let existing = util::read_json(marker)?;
    validate_owner(&existing, owner)
}

fn prepare_new_workspace(root: &Path, marker: &Path, owner: &J) -> Result<()> {
    if root.exists() {
        return prepare_existing_root(root, marker, owner);
    }
    prepare_claim(root, marker, owner)
}

fn prepare_existing_root(root: &Path, marker: &Path, owner: &J) -> Result<()> {
    if workspace_nonempty(root) {
        return Err(FlowError::progress(
            "refuse an unowned nonempty integration workspace",
        ));
    }
    prepare_claim(root, marker, owner)
}

fn workspace_nonempty(root: &Path) -> bool {
    std::fs::read_dir(root)
        .map(|entries| entries.into_iter().next().is_some())
        .unwrap_or(false)
}

fn prepare_claim(root: &Path, marker: &Path, owner: &J) -> Result<()> {
    std::fs::create_dir_all(root)?;
    claim_owner(marker, owner)
}

fn prepare_bare(root: &Path, batch: &J, record: &J) -> Result<PathBuf> {
    let bare = root.join("objects.git");
    if bare.exists() {
        return prepare_bare_ready(&bare, batch, record);
    }
    prepare_bare_init(root, &bare, batch, record)
}

fn prepare_bare_init(root: &Path, bare: &Path, batch: &J, record: &J) -> Result<PathBuf> {
    run(
        root,
        &[
            "init".to_string(),
            "--bare".to_string(),
            bare.display().to_string(),
        ],
        None,
        None,
        &BTreeMap::new(),
        false,
    )?;
    prepare_bare_ready(bare, batch, record)
}

fn prepare_bare_ready(bare: &Path, batch: &J, record: &J) -> Result<PathBuf> {
    let shape = output(
        bare,
        &["rev-parse".to_string(), "--is-bare-repository".to_string()],
    )?;
    prepare_bare_shape(bare, &shape, batch, record)
}

fn prepare_bare_shape(bare: &Path, shape: &str, batch: &J, record: &J) -> Result<PathBuf> {
    if shape != "true" {
        return Err(FlowError::progress("integration object store is not bare"));
    }
    prepare_bare_origin(bare, record, batch)
}

fn prepare_bare_origin(bare: &Path, record: &J, batch: &J) -> Result<PathBuf> {
    let origin = origin_url(record);
    let remote = run(
        bare,
        &[
            "remote".to_string(),
            "get-url".to_string(),
            "origin".to_string(),
        ],
        None,
        None,
        &BTreeMap::new(),
        true,
    )?;
    prepare_bare_remote(bare, &origin, remote, batch)
}

fn origin_url(record: &J) -> String {
    format!(
        "https://{}/{}.git",
        model::string(record, "host").unwrap_or_default(),
        model::string(record, "repo").unwrap_or_default()
    )
}

fn prepare_bare_remote(
    bare: &Path,
    origin: &str,
    remote: util::process::CommandOutput,
    batch: &J,
) -> Result<PathBuf> {
    if remote.status != 0 {
        return prepare_bare_add_origin(bare, origin, batch);
    }
    prepare_bare_existing_origin(bare, origin, remote, batch)
}

fn prepare_bare_existing_origin(
    bare: &Path,
    origin: &str,
    remote: util::process::CommandOutput,
    batch: &J,
) -> Result<PathBuf> {
    if remote.stdout_text().trim() != origin {
        return Err(FlowError::progress("integration origin changed"));
    }
    prepare_bare_base(bare, batch)
}

fn prepare_bare_add_origin(bare: &Path, origin: &str, batch: &J) -> Result<PathBuf> {
    run(
        bare,
        &[
            "remote".to_string(),
            "add".to_string(),
            "origin".to_string(),
            origin.to_string(),
        ],
        None,
        None,
        &BTreeMap::new(),
        false,
    )?;
    prepare_bare_base(bare, batch)
}

fn prepare_bare_base(bare: &Path, batch: &J) -> Result<PathBuf> {
    let base = model::string(batch, "base").unwrap_or_default();
    let check = run(
        bare,
        &[
            "cat-file".to_string(),
            "-e".to_string(),
            format!("{base}^{{commit}}"),
        ],
        None,
        None,
        &BTreeMap::new(),
        true,
    )?;
    prepare_bare_fetch_base(bare, batch, &base, check)
}

fn prepare_bare_fetch_base(
    bare: &Path,
    batch: &J,
    base: &str,
    check: util::process::CommandOutput,
) -> Result<PathBuf> {
    if check.status != 0 {
        return prepare_bare_fetch_origin(bare, batch, base);
    }
    prepare_bare_members(bare, batch)
}

fn prepare_bare_fetch_origin(bare: &Path, batch: &J, base: &str) -> Result<PathBuf> {
    run(
        bare,
        &[
            "fetch".to_string(),
            "--no-tags".to_string(),
            "origin".to_string(),
            base.to_string(),
        ],
        None,
        None,
        &BTreeMap::new(),
        false,
    )?;
    prepare_bare_members(bare, batch)
}

fn prepare_bare_members(bare: &Path, batch: &J) -> Result<PathBuf> {
    let members = bare_members(batch);
    members
        .iter()
        .try_for_each(|member| prepare_bare_member(bare, member))?;
    Ok(bare.to_path_buf())
}

fn bare_members(batch: &J) -> Vec<J> {
    let mut members: Vec<J> = model::array_field(batch, "members")
        .cloned()
        .unwrap_or_default();
    members.extend(
        model::array_field(batch, "corrections")
            .cloned()
            .unwrap_or_default(),
    );
    members
}

fn prepare_bare_member(bare: &Path, member: &J) -> Result<()> {
    let head = model::string(member, "head").unwrap_or_default();
    let check = run(
        bare,
        &[
            "cat-file".to_string(),
            "-e".to_string(),
            format!("{head}^{{commit}}"),
        ],
        None,
        None,
        &BTreeMap::new(),
        true,
    )?;
    prepare_bare_member_fetch(bare, member, &head, check)
}

fn prepare_bare_member_fetch(
    bare: &Path,
    member: &J,
    head: &str,
    check: util::process::CommandOutput,
) -> Result<()> {
    if check.status != 0 {
        return prepare_bare_member_run(bare, member, head);
    }
    Ok(())
}

fn prepare_bare_member_run(bare: &Path, member: &J, head: &str) -> Result<()> {
    run(
        bare,
        &[
            "fetch".to_string(),
            "--no-tags".to_string(),
            model::string(member, "checkout").unwrap_or_default(),
            head.to_string(),
        ],
        None,
        None,
        &BTreeMap::new(),
        false,
    )?;
    Ok(())
}

fn prepare(
    batch: &mut J,
    record: &J,
    checkpoint: &mut dyn FnMut(&J) -> Result<()>,
) -> Result<PathBuf> {
    let root = PathBuf::from(model::string(batch, "workspace").unwrap_or_default());
    let marker = root.join("owner.json");
    let owner = json!({
        "batch": batch.get("id").cloned().unwrap_or(Value::Null),
        "repo": record.get("repo").cloned().unwrap_or(Value::Null),
        "base": batch.get("base").cloned().unwrap_or(Value::Null)
    });
    prepare_workspace(&root, &marker, &owner)?;
    let bare = prepare_bare(&root, batch, record)?;
    let reference = "refs/herdr/candidate";
    let current = run(
        &bare,
        &[
            "rev-parse".to_string(),
            "--verify".to_string(),
            reference.to_string(),
        ],
        None,
        None,
        &BTreeMap::new(),
        true,
    )?;
    if current.status != 0 {
        run(
            &bare,
            &[
                "update-ref".to_string(),
                reference.to_string(),
                model::string(batch, "base").unwrap_or_default(),
                "0".repeat(40),
            ],
            None,
            None,
            &BTreeMap::new(),
            false,
        )?;
    }
    batch["integration"]["ready"] = json!(true);
    checkpoint(batch)?;
    Ok(bare)
}

pub fn integrate(
    batch: &mut J,
    record: &J,
    checkpoint: &mut dyn FnMut(&J) -> Result<()>,
) -> Result<()> {
    let bare = prepare(batch, record, checkpoint)?;
    integrate_drive(batch, &bare, checkpoint)
}

fn integrate_drive(
    batch: &mut J,
    bare: &Path,
    checkpoint: &mut dyn FnMut(&J) -> Result<()>,
) -> Result<()> {
    let limit = integration_limit(batch);
    (0..=limit).try_for_each(|_| integrate_step_once(batch, bare, checkpoint).map(|_| ()))?;
    integrate_finalize_if_complete(batch, bare, checkpoint)
}

fn integrate_step_once(
    batch: &mut J,
    bare: &Path,
    checkpoint: &mut dyn FnMut(&J) -> Result<()>,
) -> Result<bool> {
    if integrate_finished(batch) {
        return Ok(false);
    }
    integrate_advance(batch, bare, checkpoint)
}

fn integrate_advance(
    batch: &mut J,
    bare: &Path,
    checkpoint: &mut dyn FnMut(&J) -> Result<()>,
) -> Result<bool> {
    integrate_iteration(batch, bare, checkpoint)?;
    Ok(true)
}

fn integrate_finished(batch: &J) -> bool {
    [
        integrate_conflict_phase(batch),
        integrate_offset_done(batch),
    ]
    .into_iter()
    .any(|finished| finished)
}

fn integrate_complete(batch: &J) -> bool {
    integrate_offset_done(batch) && !integrate_conflict_phase(batch)
}

fn integrate_conflict_phase(batch: &J) -> bool {
    model::string(batch, "phase").as_deref() == Some("integration_conflict")
}

fn integrate_offset_done(batch: &J) -> bool {
    integration_offset(batch) >= integration_steps(batch).len()
}

fn integrate_finalize_if_complete(
    batch: &mut J,
    bare: &Path,
    checkpoint: &mut dyn FnMut(&J) -> Result<()>,
) -> Result<()> {
    if integrate_complete(batch) {
        return integrate_finish(batch, bare, checkpoint);
    }
    Ok(())
}

fn integrate_finish(
    batch: &mut J,
    bare: &Path,
    checkpoint: &mut dyn FnMut(&J) -> Result<()>,
) -> Result<()> {
    let integrated = output(
        bare,
        &["rev-parse".to_string(), "refs/herdr/candidate".to_string()],
    )?;
    batch["phase"] = json!("integrated");
    batch["integrated"] = json!(integrated);
    checkpoint(batch)
}

fn integration_limit(batch: &J) -> usize {
    integration_steps(batch)
        .len()
        .saturating_mul(2)
        .saturating_add(1)
}

fn integration_offset(batch: &J) -> usize {
    model::dig(batch, &["integration", "offset"])
        .and_then(Value::as_u64)
        .unwrap_or(0) as usize
}

fn integration_index(batch: &J) -> PathBuf {
    PathBuf::from(model::string(batch, "workspace").unwrap_or_default()).join("integration.index")
}

fn integration_parent(batch: &J) -> String {
    let mapping = model::dig(batch, &["integration", "mapping"])
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if mapping.is_empty() {
        return model::string(batch, "base").unwrap_or_default();
    }
    mapping
        .last()
        .and_then(|entry| model::string(entry, "integrated"))
        .unwrap_or_default()
}

fn integration_steps(batch: &J) -> Vec<(String, String)> {
    integration_sources(batch)
        .iter()
        .flat_map(member_steps)
        .collect()
}

fn integration_sources(batch: &J) -> Vec<J> {
    let mut sources: Vec<J> = model::array_field(batch, "members")
        .cloned()
        .unwrap_or_default();
    sources.extend(
        model::array_field(batch, "corrections")
            .cloned()
            .unwrap_or_default(),
    );
    sources
}

fn member_steps(member: &J) -> Vec<(String, String)> {
    let task = model::string(member, "task").unwrap_or_default();
    model::array_field(member, "own_commits")
        .map(|commits| {
            commits
                .iter()
                .filter_map(Value::as_str)
                .map(|source| (task.clone(), source.to_string()))
                .collect()
        })
        .unwrap_or_default()
}

fn integrate_iteration(
    batch: &mut J,
    bare: &Path,
    checkpoint: &mut dyn FnMut(&J) -> Result<()>,
) -> Result<()> {
    let steps = integration_steps(batch);
    let offset = integration_offset(batch);
    let (task, source) = steps[offset].clone();
    let parent = integration_parent(batch);
    let actual = output(
        bare,
        &["rev-parse".to_string(), "refs/herdr/candidate".to_string()],
    )?;
    integrate_choose(
        batch, bare, checkpoint, offset, &task, &source, &parent, actual,
    )
}

fn integrate_choose(
    batch: &mut J,
    bare: &Path,
    checkpoint: &mut dyn FnMut(&J) -> Result<()>,
    offset: usize,
    task: &str,
    source: &str,
    parent: &str,
    actual: String,
) -> Result<()> {
    let journal = model::dig(batch, &["integration", "current"])
        .cloned()
        .filter(|value| !value.is_null());
    if let Some(journal) = journal {
        return integrate_recover(
            batch, bare, checkpoint, offset, source, parent, &actual, journal,
        );
    }
    integrate_apply_step(
        batch, bare, checkpoint, offset, task, source, parent, &actual,
    )
}

fn integrate_recover(
    batch: &mut J,
    bare: &Path,
    checkpoint: &mut dyn FnMut(&J) -> Result<()>,
    offset: usize,
    source: &str,
    parent: &str,
    actual: &str,
    journal: J,
) -> Result<()> {
    if journal_mismatch(&journal, source, parent) {
        return Err(FlowError::progress(
            "integration journal no longer matches the frozen plan",
        ));
    }
    integrate_recover_ref(batch, bare, checkpoint, offset, actual, parent, journal)
}

fn journal_mismatch(journal: &J, source: &str, parent: &str) -> bool {
    model::string(journal, "source") != Some(source.to_string())
        || model::string(journal, "parent") != Some(parent.to_string())
}

fn integrate_recover_ref(
    batch: &mut J,
    bare: &Path,
    checkpoint: &mut dyn FnMut(&J) -> Result<()>,
    offset: usize,
    actual: &str,
    parent: &str,
    journal: J,
) -> Result<()> {
    let integrated = model::string(&journal, "integrated").unwrap_or_default();
    if !integration_ref_matches(actual, parent, &integrated) {
        return Err(FlowError::progress(
            "integration ref changed outside the journal",
        ));
    }
    integrate_recover_update(
        batch,
        bare,
        checkpoint,
        offset,
        actual,
        parent,
        journal,
        &integrated,
    )
}

fn integration_ref_matches(actual: &str, parent: &str, integrated: &str) -> bool {
    actual == parent || actual == integrated
}

fn integrate_recover_update(
    batch: &mut J,
    bare: &Path,
    checkpoint: &mut dyn FnMut(&J) -> Result<()>,
    offset: usize,
    actual: &str,
    parent: &str,
    journal: J,
    integrated: &str,
) -> Result<()> {
    if actual == parent {
        return integrate_recover_move(
            batch, bare, checkpoint, offset, parent, journal, integrated,
        );
    }
    integrate_recover_record(batch, checkpoint, offset, journal)
}

fn integrate_recover_move(
    batch: &mut J,
    bare: &Path,
    checkpoint: &mut dyn FnMut(&J) -> Result<()>,
    offset: usize,
    parent: &str,
    journal: J,
    integrated: &str,
) -> Result<()> {
    run(
        bare,
        &[
            "update-ref".to_string(),
            "refs/herdr/candidate".to_string(),
            integrated.to_string(),
            parent.to_string(),
        ],
        None,
        None,
        &BTreeMap::new(),
        false,
    )?;
    integrate_recover_record(batch, checkpoint, offset, journal)
}

fn integrate_recover_record(
    batch: &mut J,
    checkpoint: &mut dyn FnMut(&J) -> Result<()>,
    offset: usize,
    journal: J,
) -> Result<()> {
    push_integration_mapping(batch, journal);
    batch["integration"]["offset"] = json!(offset + 1);
    batch["integration"]["current"] = Value::Null;
    checkpoint(batch)
}

fn push_integration_mapping(batch: &mut J, entry: J) {
    if let Some(mapping) = model::get_mut(batch, "integration")
        .and_then(|integration| integration.get_mut("mapping"))
        .and_then(Value::as_array_mut)
    {
        mapping.push(entry);
    }
}

fn integrate_apply_step(
    batch: &mut J,
    bare: &Path,
    checkpoint: &mut dyn FnMut(&J) -> Result<()>,
    offset: usize,
    task: &str,
    source: &str,
    parent: &str,
    actual: &str,
) -> Result<()> {
    if actual != parent {
        return Err(FlowError::progress(
            "unaccounted integration ref; inspect without replay",
        ));
    }
    integrate_read_tree(batch, bare, checkpoint, offset, task, source, parent)
}

fn integrate_read_tree(
    batch: &mut J,
    bare: &Path,
    checkpoint: &mut dyn FnMut(&J) -> Result<()>,
    offset: usize,
    task: &str,
    source: &str,
    parent: &str,
) -> Result<()> {
    let index = integration_index(batch);
    run(
        bare,
        &["read-tree".to_string(), parent.to_string()],
        None,
        Some(&index),
        &BTreeMap::new(),
        false,
    )?;
    integrate_diff(
        batch, bare, checkpoint, offset, task, source, parent, &index,
    )
}

fn integrate_diff(
    batch: &mut J,
    bare: &Path,
    checkpoint: &mut dyn FnMut(&J) -> Result<()>,
    offset: usize,
    task: &str,
    source: &str,
    parent: &str,
    index: &Path,
) -> Result<()> {
    let patch = run(
        bare,
        &[
            "diff-tree".to_string(),
            "--root".to_string(),
            "--no-commit-id".to_string(),
            "-p".to_string(),
            "--binary".to_string(),
            "--full-index".to_string(),
            "--no-ext-diff".to_string(),
            "--no-textconv".to_string(),
            source.to_string(),
        ],
        None,
        None,
        &BTreeMap::new(),
        false,
    )?
    .stdout;
    integrate_apply(
        batch, bare, checkpoint, offset, task, source, parent, index, patch,
    )
}

fn integrate_apply(
    batch: &mut J,
    bare: &Path,
    checkpoint: &mut dyn FnMut(&J) -> Result<()>,
    offset: usize,
    task: &str,
    source: &str,
    parent: &str,
    index: &Path,
    patch: Vec<u8>,
) -> Result<()> {
    let applied = run(
        bare,
        &[
            "apply".to_string(),
            "--cached".to_string(),
            "--3way".to_string(),
            "--allow-empty".to_string(),
            "-".to_string(),
        ],
        Some(&patch),
        Some(index),
        &BTreeMap::new(),
        true,
    )?;
    integrate_applied(
        batch, bare, checkpoint, offset, task, source, parent, index, applied,
    )
}

fn integrate_applied(
    batch: &mut J,
    bare: &Path,
    checkpoint: &mut dyn FnMut(&J) -> Result<()>,
    offset: usize,
    task: &str,
    source: &str,
    parent: &str,
    index: &Path,
    applied: util::process::CommandOutput,
) -> Result<()> {
    if applied.status != 0 {
        return integrate_conflict(batch, checkpoint, offset, task, source, parent);
    }
    integrate_tree(batch, bare, checkpoint, offset, task, source, parent, index)
}

fn integrate_conflict(
    batch: &mut J,
    checkpoint: &mut dyn FnMut(&J) -> Result<()>,
    offset: usize,
    task: &str,
    source: &str,
    parent: &str,
) -> Result<()> {
    batch["integration"]["conflict"] = json!({
        "task": task,
        "source": source,
        "parent": parent,
        "offset": offset
    });
    batch["phase"] = json!("integration_conflict");
    checkpoint(batch)
}

fn integrate_tree(
    batch: &mut J,
    bare: &Path,
    checkpoint: &mut dyn FnMut(&J) -> Result<()>,
    offset: usize,
    task: &str,
    source: &str,
    parent: &str,
    index: &Path,
) -> Result<()> {
    let tree = output_index(bare, index)?;
    integrate_date(batch, bare, checkpoint, offset, task, source, parent, tree)
}

fn integrate_date(
    batch: &mut J,
    bare: &Path,
    checkpoint: &mut dyn FnMut(&J) -> Result<()>,
    offset: usize,
    task: &str,
    source: &str,
    parent: &str,
    tree: String,
) -> Result<()> {
    let date = output(
        bare,
        &[
            "show".to_string(),
            "-s".to_string(),
            "--format=%ct".to_string(),
            source.to_string(),
        ],
    )?;
    integrate_create(
        batch, bare, checkpoint, offset, task, source, parent, tree, &date,
    )
}

fn integrate_create(
    batch: &mut J,
    bare: &Path,
    checkpoint: &mut dyn FnMut(&J) -> Result<()>,
    offset: usize,
    task: &str,
    source: &str,
    parent: &str,
    tree: String,
    date: &str,
) -> Result<()> {
    let message = format!(
        "Integrate {task}\n\nHerdr-Batch: {}\nHerdr-Source: {source}\nHerdr-Offset: {offset}\n",
        model::string(batch, "id").unwrap_or_default()
    );
    let mut env = BTreeMap::new();
    env.insert("GIT_AUTHOR_DATE".to_string(), format!("@{date} +0000"));
    env.insert("GIT_COMMITTER_DATE".to_string(), format!("@{date} +0000"));
    let commit = run(
        bare,
        &[
            "commit-tree".to_string(),
            tree.clone(),
            "-p".to_string(),
            parent.to_string(),
        ],
        Some(message.as_bytes()),
        None,
        &env,
        false,
    )?
    .stdout_text()
    .trim()
    .to_string();
    integrate_record(batch, checkpoint, task, source, parent, tree, commit)
}

fn integrate_record(
    batch: &mut J,
    checkpoint: &mut dyn FnMut(&J) -> Result<()>,
    task: &str,
    source: &str,
    parent: &str,
    tree: String,
    commit: String,
) -> Result<()> {
    batch["integration"]["current"] = json!({
        "task": task,
        "source": source,
        "parent": parent,
        "integrated": commit,
        "tree": tree
    });
    checkpoint(batch)
}

fn output_index(bare: &Path, index: &Path) -> Result<String> {
    let output = run(
        bare,
        &["write-tree".to_string()],
        None,
        Some(index),
        &BTreeMap::new(),
        false,
    )?;
    Ok(output.stdout_text().trim().to_string())
}

pub fn import_repair(batch: &J, checkout: &Path, head: &str) -> Result<(PathBuf, String)> {
    let bare =
        PathBuf::from(model::string(batch, "workspace").unwrap_or_default()).join("objects.git");
    run(
        &bare,
        &[
            "fetch".to_string(),
            "--no-tags".to_string(),
            checkout.display().to_string(),
            head.to_string(),
        ],
        None,
        None,
        &BTreeMap::new(),
        false,
    )?;
    import_repair_parent(bare, head)
}

fn import_repair_parent(bare: PathBuf, head: &str) -> Result<(PathBuf, String)> {
    let parent = output(
        &bare,
        &["rev-parse".to_string(), "refs/herdr/candidate".to_string()],
    )?;
    import_repair_ancestor(bare, parent, head)
}

fn import_repair_ancestor(bare: PathBuf, parent: String, head: &str) -> Result<(PathBuf, String)> {
    crate::pr::git(&bare, &["merge-base", "--is-ancestor", &parent, head])?;
    import_repair_linear(bare, parent, head)
}

fn import_repair_linear(bare: PathBuf, parent: String, head: &str) -> Result<(PathBuf, String)> {
    if parent == head {
        return Err(repair_range_error());
    }
    import_repair_merges(bare, parent, head)
}

fn import_repair_merges(bare: PathBuf, parent: String, head: &str) -> Result<(PathBuf, String)> {
    let merges = repair_merges(&bare, &parent, head)?;
    import_repair_range(bare, parent, merges)
}

fn repair_merges(bare: &Path, parent: &str, head: &str) -> Result<String> {
    crate::pr::git(
        bare,
        &["rev-list", "--merges", &format!("{parent}..{head}")],
    )
}

fn import_repair_range(bare: PathBuf, parent: String, merges: String) -> Result<(PathBuf, String)> {
    if merges.trim().is_empty() {
        Ok((bare, parent))
    } else {
        Err(repair_range_error())
    }
}

fn repair_range_error() -> FlowError {
    FlowError::progress("batch repair must append a nonempty linear range to the pinned candidate")
}
