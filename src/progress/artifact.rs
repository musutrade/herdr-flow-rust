//! Customer adapters verify actual objects; the workflow stores their identities.
use std::path::Path;
use std::process::Command;

use serde_json::json;

use super::{ensure, text};
use crate::model::{self, J};
use crate::Result;

/// Implementations must verify the object, not merely accept a producer's claim.
pub trait ArtifactAdapter {
    fn verify(&self, artifact: &J) -> Result<()>;
}

pub struct LocalArtifacts;

impl ArtifactAdapter for LocalArtifacts {
    fn verify(&self, artifact: &J) -> Result<()> {
        match model::string(artifact, "adapter").as_deref() {
            Some("file-sha256") => verify_file(artifact),
            Some("git") => verify_git(artifact),
            _ => Err(super::progress_error("unsupported artifact adapter")),
        }
    }
}

fn object_path(artifact: &J) -> Result<String> {
    let path = text(model::get(artifact, "location"), "artifact location", 1000)?;
    ensure(
        Path::new(&path).is_absolute(),
        "artifact location must be absolute",
    )?;
    Ok(path)
}

fn verify_file(artifact: &J) -> Result<()> {
    let path = object_path(artifact)?;
    let mut file = std::fs::File::open(path)?;
    ensure(
        file.metadata()?.is_file(),
        "artifact must be a regular file",
    )?;
    let mut digest = sha2::Sha256::default();
    hash_file(&mut file, &mut digest)?;
    use sha2::Digest;
    let actual = digest
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    ensure(
        artifact["id"] == json!(actual),
        "artifact digest does not match real file",
    )
}

fn verify_git(artifact: &J) -> Result<()> {
    let path = object_path(artifact)?;
    let id = text(model::get(artifact, "id"), "artifact id", 256)?;
    ensure(model::is_full_sha(&id), "git artifact requires a full SHA")?;
    let output = Command::new("git")
        .args(["--no-replace-objects", "-C", &path, "cat-file", "-t", &id])
        .output()?;
    ensure(output.status.success(), "git artifact does not exist")?;
    ensure(
        output.stdout == b"commit\n",
        "git artifact must identify a commit",
    )
}

fn hash_file(file: &mut std::fs::File, digest: &mut sha2::Sha256) -> Result<()> {
    use sha2::Digest;
    use std::io::Read;
    let mut buffer = [0u8; 8192];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            return Ok(());
        }
        digest.update(&buffer[..count]);
    }
}
