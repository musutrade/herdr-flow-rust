//! Lexical checks on recorded paths, without filesystem resolution or ownership.
use super::super::{CleanupState, DeliveryTarget};
use super::require;
use crate::Result;
use std::collections::BTreeSet;
use std::path::{Component, Path, PathBuf};

pub(super) fn normalize(path: &Path) -> PathBuf {
    let mut result = PathBuf::new();
    for part in path.components() {
        match part {
            Component::ParentDir => {
                result.pop();
            }
            Component::CurDir => (),
            other => result.push(other.as_os_str()),
        }
    }
    result
}

fn overlap(left: &Path, right: &Path) -> bool {
    let left = normalize(left);
    let right = normalize(right);
    left.starts_with(&right) || right.starts_with(&left)
}

pub(super) fn roots(control: &Path, roots: &[PathBuf]) -> Result<()> {
    for (n, root) in roots.iter().enumerate() {
        require(
            root.is_absolute() && !overlap(root, control),
            "root overlaps control tree or is relative",
        )?;
        require(
            !roots[..n].iter().any(|p| overlap(p, root)),
            "recorded roots overlap",
        )?;
    }
    Ok(())
}

pub(super) fn targets(roots: &[PathBuf], targets: &[DeliveryTarget]) -> Result<()> {
    for target in targets {
        require(
            target.path.is_absolute() && !target.target_type.trim().is_empty(),
            "invalid recorded delivery target",
        )?;
        require(
            !roots.iter().any(|p| overlap(p, &target.path)),
            "delivery target overlaps disposable roots",
        )?;
    }
    Ok(())
}

pub(super) fn subset(actual: &[PathBuf], expected: &[PathBuf]) -> Result<()> {
    let set: BTreeSet<_> = expected.iter().map(|p| normalize(p)).collect();
    require(
        actual.iter().all(|p| set.contains(&normalize(p))),
        "plan/registry roots disagree with recorded ownership",
    )
}

pub(super) fn plan(control: &Path, plan: &CleanupState) -> Result<Vec<PathBuf>> {
    let paths: Vec<_> = plan.roots.iter().map(|r| r.path.clone()).collect();
    roots(control, &paths)?;
    subset(&plan.pending_root_removals, &paths)?;
    if let Some(q) = &plan.quarantine {
        subset(std::slice::from_ref(&q.root), &paths)?;
    }
    Ok(paths)
}
