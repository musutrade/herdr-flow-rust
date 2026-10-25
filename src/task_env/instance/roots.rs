//! Fingerprint each created directory before moving to the next boundary.
use super::{intent, require, RootIdentity, TaskEnvironment};
use crate::{FlowError, Result};
use std::{fs, os::unix::fs::DirBuilderExt, path::Path};

pub(super) fn identity(path: &Path, directory: bool) -> Result<RootIdentity> {
    super::super::validate_ancestors(path)?;
    let meta = fs::symlink_metadata(path)?;
    require(
        !meta.file_type().is_symlink(),
        "instance object is a symlink",
    )?;
    require(
        if directory {
            meta.is_dir()
        } else {
            meta.is_file()
        },
        "instance object type changed",
    )?;
    Ok(RootIdentity::of(&meta))
}

pub(super) fn verify(path: &Path, expected: RootIdentity, directory: bool) -> Result<()> {
    require(
        identity(path, directory)? == expected,
        "instance object identity changed",
    )
}

fn recorded(path: &Path, expected: Option<RootIdentity>) -> Result<()> {
    if let Some(expected) = expected {
        verify(path, expected, true)
    } else {
        match fs::symlink_metadata(path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            _ => Err(FlowError::progress(
                "existing instance directory has no recorded identity; refusing adoption",
            )),
        }
    }
}

pub(super) fn verify_recorded(env: &TaskEnvironment) -> Result<()> {
    if intent(env)?.allocation.is_some() {
        return super::allocation::verify_recorded(env);
    }
    for (path, expected) in env.roots() {
        recorded(&path, expected)?;
    }
    let i = intent(env)?;
    require(
        i.checkout == env.workspace_root.join("checkout"),
        "instance checkout boundary changed",
    )?;
    recorded(&i.checkout, i.checkout_identity)
}

fn create(path: &Path) -> Result<RootIdentity> {
    super::super::validate_ancestors(path)?;
    fs::DirBuilder::new().mode(0o700).create(path)?;
    identity(path, true)
}

pub(super) fn materialize(env: &mut TaskEnvironment) -> Result<()> {
    if intent(env)?.allocation.is_some() {
        return super::allocation::materialize(env);
    }
    verify_recorded(env)?;
    workspace(env)?;
    temporary(env)?;
    checkout(env)?;
    verify_recorded(env)
}

fn workspace(env: &mut TaskEnvironment) -> Result<()> {
    if env.workspace_identity.is_none() {
        env.workspace_identity = Some(create(&env.workspace_root)?);
        env.persist()?;
    }
    Ok(())
}

fn temporary(env: &mut TaskEnvironment) -> Result<()> {
    if env.temp_identity.is_none() {
        env.temp_identity = Some(create(&env.temp_root)?);
        env.persist()?;
    }
    Ok(())
}

fn checkout(env: &mut TaskEnvironment) -> Result<()> {
    if intent(env)?.checkout_identity.is_none() {
        let id = create(&intent(env)?.checkout)?;
        env.creation.as_mut().unwrap().checkout_identity = Some(id);
        env.persist()?;
    }
    Ok(())
}
