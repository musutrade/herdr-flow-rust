use super::{require, Findings};
use crate::{model, model::J, util::process, FlowError, Result};
use serde_json::json;
use std::{
    fs,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

struct Git<'a> {
    cwd: &'a Path,
    deadline: Instant,
}

impl Git<'_> {
    fn run(&self, args: &[&str]) -> Result<String> {
        let remaining = self
            .deadline
            .checked_duration_since(Instant::now())
            .ok_or_else(|| FlowError::queue("Git observation deadline exceeded"))?;
        let mut options = process::git_options(remaining.min(Duration::from_secs(5)));
        options.cwd = Some(self.cwd.to_path_buf());
        options.stdout_limit = 2 * 1024 * 1024;
        options.stderr_limit = 64 * 1024;
        // git_options removes inherited GIT_*. Set these on the child itself.
        for (k, v) in [
            ("GIT_OPTIONAL_LOCKS", "0"),
            ("GIT_NO_LAZY_FETCH", "1"),
            ("GIT_NO_REPLACE_OBJECTS", "1"),
        ] {
            options.env.insert(k.into(), v.into());
        }
        let argv: Vec<String> = std::iter::once("git")
            .chain(args.iter().copied())
            .map(str::to_owned)
            .collect();
        let out = process::run_command(&argv, &options)?;
        require(
            out.success(),
            &format!("read-only Git {} failed: {}", args[0], out.stderr_text()),
        )?;
        String::from_utf8(out.stdout).map_err(|_| FlowError::queue("non-UTF8 Git observation"))
    }

    fn rev(&self, name: &str) -> Result<String> {
        Ok(self
            .run(&["rev-parse", "--verify", name])?
            .trim()
            .to_owned())
    }

    fn directory(&self, name: &str) -> Result<PathBuf> {
        let value = self.run(&["rev-parse", "--path-format=absolute", name])?;
        Ok(PathBuf::from(value.trim()).canonicalize()?)
    }
}

struct Layout {
    dir: PathBuf,
    common: PathBuf,
    listing: Vec<J>,
    locks: Vec<PathBuf>,
}
struct Content {
    head: String,
    tree: String,
    dirty: String,
    refs: Vec<String>,
}

pub(super) fn observe(p: &Path, f: &mut Findings) -> Result<J> {
    let g = Git {
        cwd: p,
        deadline: Instant::now() + Duration::from_secs(30),
    };
    config(&g)?;
    let layout = layout(&g, f)?;
    let content = content(&g, f)?;
    submodules(&g, f)?;
    recheck(&g, &layout, &content, f)?;
    Ok(
        json!({"head":content.head,"tree":content.tree,"git_dir":layout.dir,"common_dir":layout.common,
        "registered":true,"dirty":!content.dirty.is_empty(),"locks":layout.locks,"durable_refs":content.refs,
        "worktrees":layout.listing,"optional_locks":false,"fsmonitor":false}),
    )
}

fn layout(g: &Git<'_>, f: &mut Findings) -> Result<Layout> {
    require(
        g.directory("--show-toplevel")? == g.cwd,
        "checkout is not exact Git root",
    )?;
    let dir = g.directory("--absolute-git-dir")?;
    let common = g.directory("--git-common-dir")?;
    require(
        dir != common,
        "main checkout is not a disposable linked worktree",
    )?;
    let listing = worktrees(g)?;
    layout_observations(g.cwd, &common, &listing, f)?;
    let locks = observed_locks(&dir, &common, f)?;
    Ok(Layout {
        dir,
        common,
        listing,
        locks,
    })
}

fn layout_observations(p: &Path, common: &Path, listing: &[J], f: &mut Findings) -> Result<()> {
    registration(p, listing, f)?;
    shared(p, common, listing, f)
}

fn observed_locks(dir: &Path, common: &Path, f: &mut Findings) -> Result<Vec<PathBuf>> {
    let observed = locks(dir, common)?;
    if !observed.is_empty() {
        f.block(
            "git_locks",
            format!("{} Git locks observed", observed.len()),
        );
    }
    Ok(observed)
}

fn identity(g: &Git<'_>) -> Result<(String, String)> {
    let head = g.rev("HEAD")?;
    let tree = g.rev("HEAD^{tree}")?;
    require(
        model::is_full_sha(&head) && model::is_full_sha(&tree),
        "invalid Git HEAD/tree",
    )?;
    Ok((head, tree))
}

fn content(g: &Git<'_>, f: &mut Findings) -> Result<Content> {
    let (head, tree) = identity(g)?;
    let dirty = status(g)?;
    if !dirty.is_empty() {
        f.block("dirty_checkout", "tracked or untracked changes present");
    }
    let refs = retention(g, &head)?;
    if refs.is_empty() {
        f.block(
            "missing_durable_ref",
            "HEAD retained by no refs/heads, refs/tags or refs/archive ref",
        );
    }
    Ok(Content {
        head,
        tree,
        dirty,
        refs,
    })
}

fn recheck(g: &Git<'_>, l: &Layout, c: &Content, f: &mut Findings) -> Result<()> {
    let current = reference_snapshot(g, l, &c.head)?;
    let before = json!([c.head, c.refs, l.listing, l.locks]);
    let dirty = status(g)?;
    if current != before || dirty != c.dirty {
        f.unknown(
            "git_changed",
            "Git identity, status, refs or lock observations changed",
        );
    }
    Ok(())
}

fn reference_snapshot(g: &Git<'_>, l: &Layout, head: &str) -> Result<J> {
    Ok(json!([
        g.rev("HEAD")?,
        retention(g, head)?,
        worktrees(g)?,
        locks(&l.dir, &l.common)?
    ]))
}

fn config(g: &Git<'_>) -> Result<()> {
    // Configuration reads never walk missing objects. Refuse partial/promisor
    // repositories before any object query, including older Git implementations.
    let config = g.run(&["config", "--null", "--list", "--name-only"])?;
    for key in config.split('\0') {
        let key = key.to_ascii_lowercase();
        require(
            !key.contains("partialclone") && !key.ends_with(".promisor"),
            "partial/promisor repository is outside local-only preflight",
        )?;
    }
    Ok(())
}

fn status(g: &Git<'_>) -> Result<String> {
    g.run(&[
        "status",
        "--porcelain=v1",
        "-z",
        "--untracked-files=all",
        "--ignore-submodules=all",
    ])
}

fn submodules(g: &Git<'_>, f: &mut Findings) -> Result<()> {
    let index = g.run(&["ls-files", "--stage", "-z"])?;
    if index.split('\0').any(|line| line.starts_with("160000 ")) {
        f.unknown(
            "submodule_ownership",
            "submodule dirt and shared ownership not inspected",
        );
    }
    Ok(())
}

fn retention(g: &Git<'_>, head: &str) -> Result<Vec<String>> {
    let refs = g.run(&[
        "for-each-ref",
        "--format=%(refname)",
        "--contains",
        head,
        "refs/heads/",
        "refs/tags/",
        "refs/archive/",
    ])?;
    Ok(refs.lines().map(str::to_owned).collect())
}

fn worktrees(g: &Git<'_>) -> Result<Vec<J>> {
    let raw = g.run(&["worktree", "list", "--porcelain", "-z"])?;
    let mut entries = Vec::new();
    let mut entry = json!({});
    for field in raw.split('\0') {
        if field.is_empty() {
            if entry.get("path").is_some() {
                entries.push(entry);
                entry = json!({});
            }
        } else {
            worktree_field(&mut entry, field)?;
        }
    }
    require(
        entry.as_object().is_some_and(|e| e.is_empty()) && !entries.is_empty(),
        "malformed worktree listing",
    )?;
    Ok(entries)
}

fn worktree_field(entry: &mut J, field: &str) -> Result<()> {
    if let Some(p) = field.strip_prefix("worktree ") {
        entry["path"] = json!(p);
    } else if let Some(head) = field.strip_prefix("HEAD ") {
        entry["head"] = json!(head);
    } else if field.starts_with("locked") || field.starts_with("prunable") {
        entry["blocked"] = json!(field);
    } else {
        require(
            field == "bare" || field == "detached" || field.starts_with("branch "),
            "unknown worktree field",
        )?;
    }
    Ok(())
}

fn registration(p: &Path, entries: &[J], f: &mut Findings) -> Result<()> {
    let matches: Vec<_> = entries
        .iter()
        .filter(|e| e["path"].as_str() == p.to_str())
        .collect();
    require(
        matches.len() == 1,
        "missing or ambiguous linked worktree registration",
    )?;
    if !matches[0]["blocked"].is_null() {
        f.block(
            "worktree_locked_or_prunable",
            matches[0]["blocked"].to_string(),
        );
    }
    Ok(())
}

fn shared(p: &Path, common: &Path, entries: &[J], f: &mut Findings) -> Result<()> {
    super::records::overlap(p, common, "common Git directory", f);
    let main = entries[0]["path"]
        .as_str()
        .ok_or_else(|| FlowError::queue("missing main worktree"))?;
    super::records::overlap(p, Path::new(main), "main worktree", f);
    for entry in entries.iter().skip(1) {
        let other = Path::new(
            entry["path"]
                .as_str()
                .ok_or_else(|| FlowError::queue("missing worktree path"))?,
        );
        if other != p {
            super::records::overlap(p, other, "other registered worktree", f);
        }
    }
    Ok(())
}

fn locks(dir: &Path, common: &Path) -> Result<Vec<PathBuf>> {
    let mut result = Vec::new();
    let mut budget = 4096;
    for base in [dir, common] {
        scan_locks(base, false, &mut budget, &mut result)?;
        let refs = base.join("refs");
        if refs.try_exists()? {
            scan_locks(&refs, true, &mut budget, &mut result)?;
        }
    }
    result.sort();
    Ok(result)
}

fn scan_locks(
    dir: &Path,
    recurse: bool,
    budget: &mut usize,
    found: &mut Vec<PathBuf>,
) -> Result<()> {
    require(
        !fs::symlink_metadata(dir)?.file_type().is_symlink(),
        "symlink in Git lock inventory",
    )?;
    for entry in fs::read_dir(dir)? {
        require(*budget > 0, "Git lock inventory limit exceeded")?;
        *budget -= 1;
        lock_entry(&entry?, recurse, budget, found)?;
    }
    Ok(())
}

fn lock_entry(
    entry: &fs::DirEntry,
    recurse: bool,
    budget: &mut usize,
    found: &mut Vec<PathBuf>,
) -> Result<()> {
    let name = entry.file_name();
    if name.to_string_lossy().ends_with(".lock") || name == "locked" {
        found.push(entry.path());
    }
    if recurse {
        scan_child(entry, budget, found)?;
    }
    Ok(())
}

fn scan_child(entry: &fs::DirEntry, budget: &mut usize, found: &mut Vec<PathBuf>) -> Result<()> {
    let kind = entry.file_type()?;
    require(!kind.is_symlink(), "symlink in Git refs inventory")?;
    if kind.is_dir() {
        scan_locks(&entry.path(), true, budget, found)?;
    }
    Ok(())
}
