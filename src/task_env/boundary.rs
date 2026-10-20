//! Delivery targets and the disposable/protected resource boundary.
//!
//! Every comparison resolves both sides with `util::resolve_path`, so a
//! symlink alias cannot be used to smuggle a delivery target inside the
//! disposable boundary (or to hide a disposable path behind an alias).

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{FlowError, Result};
use crate::util;

use super::env::TaskEnvironment;

/// A formal delivery location that must survive environment destruction.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeliveryTarget {
    /// Resolved absolute target path.
    pub path: PathBuf,
    /// Kind of artefact, for diagnostics (for example `branch` or `file`).
    pub target_type: String,
    pub description: Option<String>,
}

impl DeliveryTarget {
    pub fn new(path: PathBuf, target_type: String) -> Self {
        Self {
            path,
            target_type,
            description: None,
        }
    }

    pub fn described(path: PathBuf, target_type: String, description: String) -> Self {
        Self {
            path,
            target_type,
            description: Some(description),
        }
    }
}

/// Normalise and validate delivery targets against both disposable roots.
pub fn validate_delivery_targets(
    targets: Vec<DeliveryTarget>,
    workspace_root: &Path,
    temp_root: &Path,
) -> Result<Vec<DeliveryTarget>> {
    let mut resolved = Vec::with_capacity(targets.len());
    for target in targets {
        let path = util::resolve_path(&target.path);
        for root in [workspace_root, temp_root] {
            if util::is_relative_to(&path, root) {
                return Err(FlowError::progress(format!(
                    "delivery target {} is inside disposable root {}",
                    path.display(),
                    root.display()
                )));
            }
        }
        resolved.push(DeliveryTarget {
            path,
            target_type: target.target_type,
            description: target.description,
        });
    }
    Ok(resolved)
}

/// Disposable and protected path sets for one environment.
#[derive(Debug, Clone)]
pub struct ResourceBoundary {
    pub disposable: Vec<PathBuf>,
    pub protected: Vec<PathBuf>,
}

impl ResourceBoundary {
    pub fn from_environment(env: &TaskEnvironment) -> Self {
        Self {
            disposable: vec![
                util::resolve_path(&env.workspace_root),
                util::resolve_path(&env.temp_root),
            ],
            protected: env
                .delivery_targets
                .iter()
                .map(|target| util::resolve_path(&target.path))
                .collect(),
        }
    }

    /// True when `path` lands inside a disposable root.
    pub fn is_disposable(&self, path: &Path) -> bool {
        let resolved = util::resolve_path(path);
        self.disposable
            .iter()
            .any(|root| util::is_relative_to(&resolved, root))
    }

    /// True when `path` is, or lives under, a protected target.
    pub fn is_protected(&self, path: &Path) -> bool {
        let resolved = util::resolve_path(path);
        self.protected
            .iter()
            .any(|target| util::is_relative_to(&resolved, target))
    }

    /// Reject paths that are neither disposable nor protected.
    pub fn validate(&self, path: &Path) -> Result<()> {
        if self.is_disposable(path) || self.is_protected(path) {
            return Ok(());
        }
        Err(FlowError::progress(format!(
            "path {} is outside every managed boundary",
            path.display()
        )))
    }
}

/// True when `path` lies inside the environment's disposable roots.
pub fn is_within_disposable(env: &TaskEnvironment, path: &Path) -> bool {
    ResourceBoundary::from_environment(env).is_disposable(path)
}

/// True when `path` is a formal delivery target.
pub fn is_delivery(env: &TaskEnvironment, path: &Path) -> bool {
    ResourceBoundary::from_environment(env).is_protected(path)
}

/// Validate that `path` is inside a managed boundary.
pub fn validate_path(env: &TaskEnvironment, path: &Path) -> Result<()> {
    ResourceBoundary::from_environment(env).validate(path)
}

/// True when either resolved path contains the other.
pub fn paths_overlap(left: &Path, right: &Path) -> bool {
    let left = util::resolve_path(left);
    let right = util::resolve_path(right);
    util::is_relative_to(&left, &right) || util::is_relative_to(&right, &left)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::fs::test_tempdir;
    use std::os::unix::fs::symlink;

    #[test]
    fn overlap_detects_containment_in_both_directions() {
        assert!(paths_overlap(Path::new("/foo/bar"), Path::new("/foo")));
        assert!(paths_overlap(Path::new("/foo"), Path::new("/foo/bar")));
        assert!(!paths_overlap(Path::new("/foo/bar"), Path::new("/baz/qux")));
    }

    #[test]
    fn symlink_alias_is_not_a_disposable_or_protected_escape() {
        let directory = test_tempdir();
        let real = directory.path().join("real");
        std::fs::create_dir(&real).unwrap();
        let alias = directory.path().join("alias");
        symlink(&real, &alias).unwrap();

        let boundary = ResourceBoundary {
            disposable: vec![real.clone()],
            protected: vec![directory.path().join("protected")],
        };
        assert!(boundary.is_disposable(&alias.join("child")));
        assert!(!boundary.is_protected(&alias.join("child")));
    }

    #[test]
    fn validate_rejects_unmanaged_paths() {
        let boundary = ResourceBoundary {
            disposable: vec![PathBuf::from("/tmp/task")],
            protected: vec![PathBuf::from("/repo/main")],
        };
        assert!(boundary.validate(Path::new("/tmp/task/file")).is_ok());
        assert!(boundary.validate(Path::new("/repo/main/file")).is_ok());
        assert!(boundary.validate(Path::new("/elsewhere")).is_err());
    }
}
