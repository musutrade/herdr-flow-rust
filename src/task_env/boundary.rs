//! Resource boundary protection and path validation.

use std::path::{Path, PathBuf};
use serde::{Deserialize, Serialize};

use crate::error::{FlowError, Result};
use crate::util;
use super::env::TaskEnvironment;

/// A formal delivery target outside the disposable environment.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeliveryTarget {
    /// Target path (must be outside disposable boundaries).
    pub path: PathBuf,
    
    /// Target type (e.g., "pr", "branch", "file").
    pub target_type: String,
    
    /// Optional description.
    pub description: Option<String>,
}

impl DeliveryTarget {
    /// Create a new delivery target.
    pub fn new(path: PathBuf, target_type: String) -> Self {
        Self {
            path,
            target_type,
            description: None,
        }
    }
    
    /// Create a delivery target with description.
    pub fn with_description(path: PathBuf, target_type: String, description: String) -> Self {
        Self {
            path,
            target_type,
            description: Some(description),
        }
    }
}

/// A resource boundary defining disposable and protected areas.
#[derive(Debug)]
pub struct ResourceBoundary {
    /// Disposable workspace roots.
    pub disposable: Vec<PathBuf>,
    
    /// Protected delivery targets.
    pub protected: Vec<PathBuf>,
}

impl ResourceBoundary {
    /// Create a boundary from an environment.
    pub fn from_environment(env: &TaskEnvironment) -> Self {
        let disposable = vec![
            env.workspace_root.clone(),
            env.temp_root.clone(),
        ];
        
        let protected = env.delivery_targets
            .iter()
            .map(|t| t.path.clone())
            .collect();
        
        Self {
            disposable,
            protected,
        }
    }
    
    /// Check if a path is within any disposable boundary.
    pub fn is_disposable(&self, path: &Path) -> bool {
        let resolved = util::resolve_path(path);
        self.disposable.iter().any(|root| {
            util::is_relative_to(&resolved, root)
        })
    }
    
    /// Check if a path is a protected target.
    pub fn is_protected(&self, path: &Path) -> bool {
        let resolved = util::resolve_path(path);
        self.protected.iter().any(|target| {
            util::resolve_path(target) == resolved || 
            util::is_relative_to(&resolved, target)
        })
    }
    
    /// Validate that a path is within the allowed boundaries.
    pub fn validate(&self, path: &Path) -> Result<()> {
        if self.is_protected(path) {
            return Ok(());
        }
        
        if self.is_disposable(path) {
            return Ok(());
        }
        
        Err(FlowError::progress(&format!(
            "path {:?} is outside managed boundaries",
            path
        )))
    }
}

/// Check if a path is within the environment's disposable boundary.
pub fn is_within_disposable(env: &TaskEnvironment, path: &Path) -> bool {
    let boundary = ResourceBoundary::from_environment(env);
    boundary.is_disposable(path)
}

/// Check if a path is a delivery target.
pub fn is_delivery(env: &TaskEnvironment, path: &Path) -> bool {
    let boundary = ResourceBoundary::from_environment(env);
    boundary.is_protected(path)
}

/// Validate that a path is within allowed boundaries for the environment.
pub fn validate_path(env: &TaskEnvironment, path: &Path) -> Result<()> {
    let boundary = ResourceBoundary::from_environment(env);
    boundary.validate(path)
}

/// Detect if two paths overlap (one contains the other).
#[allow(dead_code)]
pub fn paths_overlap(a: &Path, b: &Path) -> bool {
    let a_resolved = util::resolve_path(a);
    let b_resolved = util::resolve_path(b);
    
    util::is_relative_to(&a_resolved, &b_resolved) ||
    util::is_relative_to(&b_resolved, &a_resolved)
}

/// Validate that workspace roots don't overlap with protected paths.
#[allow(dead_code)]
pub fn validate_no_overlap(
    workspace_root: &Path,
    temp_root: &Path,
    protected_paths: &[PathBuf],
) -> Result<()> {
    // Check workspace against protected
    for protected in protected_paths {
        if paths_overlap(workspace_root, protected) {
            return Err(FlowError::progress(&format!(
                "workspace {:?} overlaps with protected path {:?}",
                workspace_root, protected
            )));
        }
    }
    
    // Check temp against protected
    for protected in protected_paths {
        if paths_overlap(temp_root, protected) {
            return Err(FlowError::progress(&format!(
                "temp root {:?} overlaps with protected path {:?}",
                temp_root, protected
            )));
        }
    }
    
    // Check workspace against temp
    if paths_overlap(workspace_root, temp_root) {
        return Err(FlowError::progress(&format!(
            "workspace {:?} overlaps with temp root {:?}",
            workspace_root, temp_root
        )));
    }
    
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    
    #[test]
    fn test_paths_overlap() {
        assert!(paths_overlap(
            Path::new("/foo/bar"),
            Path::new("/foo")
        ));
        
        assert!(paths_overlap(
            Path::new("/foo"),
            Path::new("/foo/bar")
        ));
        
        assert!(!paths_overlap(
            Path::new("/foo/bar"),
            Path::new("/baz/qux")
        ));
    }
    
    #[test]
    fn test_boundary_validation() {
        let boundary = ResourceBoundary {
            disposable: vec![
                PathBuf::from("/tmp/task-123"),
                PathBuf::from("/work/task-123"),
            ],
            protected: vec![
                PathBuf::from("/repo/main"),
            ],
        };
        
        assert!(boundary.is_disposable(Path::new("/tmp/task-123/file.txt")));
        assert!(boundary.is_disposable(Path::new("/work/task-123/subdir/file.txt")));
        assert!(boundary.is_protected(Path::new("/repo/main/file.txt")));
        assert!(!boundary.is_disposable(Path::new("/other/path")));
        assert!(!boundary.is_protected(Path::new("/other/path")));
    }
}
