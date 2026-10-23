//! Explicitly selected, administrator-installed outer bubblewrap launcher.
//! This never installs a launcher or changes a host security profile.
use super::{ensure, required, J};
use crate::{util, Result};
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

/// Resolve an operator-selected outer bwrap. A custom selection must identify
/// a trusted administrator-installed binary; no policy is installed or relaxed.
/// This validates identity, not whether its host profile permits nested bwrap.
pub fn resolve(spec: &J) -> Result<PathBuf> {
    let Some(selection) = spec.get("namespace_launcher") else {
        return Ok(PathBuf::from("/usr/bin/bwrap"));
    };
    let path = PathBuf::from(required(selection, "path")?);
    let digest = required(selection, "sha256")?;
    ensure(path.is_absolute(), "namespace launcher must be absolute")?;
    ensure(
        fs::canonicalize(&path)? == path,
        "namespace launcher must use its canonical path",
    )?;
    trusted_path(&path)?;
    verify_binary(&path, &digest)?;
    Ok(path)
}

fn verify_binary(path: &Path, digest: &str) -> Result<()> {
    ensure(
        digest.len() == 64 && digest.bytes().all(|b| b.is_ascii_hexdigit()),
        "namespace launcher requires a SHA-256 digest",
    )?;
    let metadata = fs::metadata(path)?;
    ensure(
        metadata.is_file() && metadata.mode() & 0o111 != 0 && metadata.len() <= 16 * 1024 * 1024,
        "namespace launcher must be a bounded executable file",
    )?;
    ensure(
        util::pyjson::sha256_hex(&fs::read(path)?) == digest.to_ascii_lowercase(),
        "namespace launcher digest mismatch; no fallback",
    )?;
    Ok(())
}

fn trusted_path(path: &Path) -> Result<()> {
    for entry in path.ancestors() {
        let metadata = fs::symlink_metadata(entry)?;
        ensure(
            metadata.uid() == 0 && metadata.mode() & 0o022 == 0,
            "namespace launcher and parents must be root-owned and not group/world writable",
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn default_is_fixed_and_invalid_selection_never_falls_back() {
        assert_eq!(resolve(&json!({})).unwrap(), Path::new("/usr/bin/bwrap"));
        for selection in [
            J::Null,
            json!({}),
            json!({"path":"relative","sha256":"a".repeat(64)}),
            json!({"path":"/missing","sha256":"bad"}),
            json!({"path":"/missing","sha256":"a".repeat(64)}),
        ] {
            assert!(resolve(&json!({"namespace_launcher":selection})).is_err());
        }
    }

    #[test]
    fn explicit_identity_is_checked_before_launch() {
        // Identity validation only: never execute the selected binary here.
        let path = fs::canonicalize("/bin/true").unwrap();
        let digest = util::pyjson::sha256_hex(&fs::read(&path).unwrap());
        let spec = json!({"namespace_launcher":{"path":path,"sha256":digest}});
        assert_eq!(resolve(&spec).unwrap(), path);
        let mut wrong = spec;
        wrong["namespace_launcher"]["sha256"] = json!("0".repeat(64));
        assert!(resolve(&wrong).is_err());
        let temp = tempfile::tempdir().unwrap();
        let candidate = temp.path().join("bwrap");
        fs::copy(&path, &candidate).unwrap();
        let spec = json!({"namespace_launcher":{"path":candidate,"sha256":digest}});
        assert!(resolve(&spec).is_err());
    }
}
