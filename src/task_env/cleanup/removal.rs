//! Removal of one already-owned ordinary directory. The caller holds the
//! environment and owner locks and persists this state in its enclosing plan.
//! This helper grants no ownership, kills no processes and never unmounts.

use super::*;
use std::ffi::OsString;
use std::path::Component;

/// Durable, exact authority for retiring one review clone. Keep the original
/// identity and quarantine receipt even after completion, so retries can reject
/// an unrelated replacement. Never construct this from a newly observed inode
/// during recovery.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct DirectoryRemoval {
    pub path: PathBuf,
    pub identity: RootIdentity,
    pub quarantine: Option<Quarantine>,
    pub complete: bool,
}

impl DirectoryRemoval {
    /// Prepare a removal using an identity already recorded by the owner.
    pub(crate) fn new(path: PathBuf, identity: RootIdentity) -> Result<Self> {
        let state = Self {
            path,
            identity,
            quarantine: None,
            complete: false,
        };
        let (parent, name) = state.open_parent()?;
        open_verified_root(&parent, &name, identity, &state.path)?;
        reject_mounts(&state.path)?;
        Ok(state)
    }

    /// Persist each proposed checkpoint through `persist` before its operation.
    /// The callback must durably save the entire enclosing plan. A failed save
    /// leaves this state unchanged and the corresponding operation unstarted.
    /// Callers must retain both locks until this method returns.
    pub(crate) fn drive(&mut self, mut persist: impl FnMut(&Self) -> Result<()>) -> Result<()> {
        let (parent, name) = self.open_parent()?;
        if self.complete {
            return self.verify_absent(&parent, &name);
        }
        let recovered_intent = self.quarantine.is_some();
        let quarantine = self.prepare_quarantine(&parent, &name, &mut persist)?;
        if self.quarantine_is_gone(&parent, &name, &quarantine, recovered_intent, &mut persist)? {
            return self.finish(&mut persist);
        }
        let dir = self.clear_quarantine(&parent, &name, &quarantine)?;
        self.unlink_quarantine(&parent, &name, &quarantine, &dir, &mut persist)?;
        self.finish(&mut persist)
    }

    fn prepare_quarantine(
        &mut self,
        parent: &File,
        name: &OsStr,
        persist: &mut impl FnMut(&Self) -> Result<()>,
    ) -> Result<Quarantine> {
        if let Some(quarantine) = &self.quarantine {
            return Ok(quarantine.clone());
        }
        open_verified_root(parent, name, self.identity, &self.path)?;
        reject_mounts(&self.path)?;
        let mut next = self.clone();
        let quarantine = Quarantine {
            root: self.path.clone(),
            name: format!(".herdr-rm-{}", util::uuid_hex()),
            unlink_committed: false,
        };
        next.quarantine = Some(quarantine.clone());
        self.checkpoint(next, persist)?;
        Ok(quarantine)
    }

    fn quarantine_is_gone(
        &self,
        parent: &File,
        name: &OsStr,
        quarantine: &Quarantine,
        recovered_intent: bool,
        persist: &mut impl FnMut(&Self) -> Result<()>,
    ) -> Result<bool> {
        match kind_at(parent, OsStr::new(&quarantine.name))? {
            None if quarantine.unlink_committed => {
                self.verify_absent(parent, name)?;
                Ok(true)
            }
            None => {
                self.rename_pending(parent, name, quarantine, recovered_intent, persist)?;
                Ok(false)
            }
            Some(EntryKind::Directory) => Ok(false),
            Some(_) => Err(replaced_quarantine_error(&self.path)),
        }
    }

    fn rename_pending(
        &self,
        parent: &File,
        name: &OsStr,
        quarantine: &Quarantine,
        recovered_intent: bool,
        persist: &mut impl FnMut(&Self) -> Result<()>,
    ) -> Result<()> {
        // A durable rename intent may precede its filesystem operation. Only
        // the original identity at the original entry permits a restart.
        ensure_entry_identity(parent, name, self.identity, &self.path)?;
        reject_mounts(&self.path)?;
        if recovered_intent {
            persist(self)?;
        }
        rename_into_quarantine(parent, name, quarantine, &self.path)?;
        parent.sync_all()?;
        Ok(())
    }

    fn clear_quarantine(
        &self,
        parent: &File,
        name: &OsStr,
        quarantine: &Quarantine,
    ) -> Result<File> {
        complete_committed(parent, name, &self.path)?;
        let dir = open_verified_root(
            parent,
            OsStr::new(&quarantine.name),
            self.identity,
            &self.path,
        )?;
        reject_mounts(&self.path)?;
        reject_mounts(&self.quarantine_path()?)?;
        clear_directory(&dir, 0)?;
        Ok(dir)
    }

    fn unlink_quarantine(
        &mut self,
        parent: &File,
        name: &OsStr,
        quarantine: &Quarantine,
        dir: &File,
        persist: &mut impl FnMut(&Self) -> Result<()>,
    ) -> Result<()> {
        let mut next = self.clone();
        next.quarantine
            .as_mut()
            .expect("validated quarantine")
            .unlink_committed = true;
        // Reaffirm even a recovered unlink intent before repeating its action.
        self.checkpoint(next, persist)?;
        let quarantine_name = OsStr::new(&quarantine.name);
        ensure_entry_identity(parent, quarantine_name, self.identity, &self.path)?;
        unlink_verified_directory(parent, quarantine_name, dir, &self.path)?;
        parent.sync_all()?;
        self.verify_absent(parent, name)
    }

    fn checkpoint(
        &mut self,
        next: Self,
        persist: &mut impl FnMut(&Self) -> Result<()>,
    ) -> Result<()> {
        persist(&next)?;
        *self = next;
        Ok(())
    }

    fn finish(&mut self, persist: &mut impl FnMut(&Self) -> Result<()>) -> Result<()> {
        let mut next = self.clone();
        next.complete = true;
        self.checkpoint(next, persist)
    }

    fn open_parent(&self) -> Result<(File, OsString)> {
        validate_removal_path(&self.path)?;
        registry::validate_outside_control(&self.path)?;
        let name = self
            .path
            .file_name()
            .ok_or_else(|| FlowError::guard("refusing removal of filesystem root"))?;
        self.validate_quarantine(name)?;
        Ok((open_absolute_parent(&self.path)?, name.to_os_string()))
    }

    fn validate_quarantine(&self, root_name: &OsStr) -> Result<()> {
        match &self.quarantine {
            Some(quarantine) => self.validate_quarantine_receipt(quarantine, root_name),
            None if self.complete => Err(FlowError::guard(
                "completed removal lacks quarantine receipt",
            )),
            None => Ok(()),
        }
    }

    fn validate_quarantine_receipt(
        &self,
        quarantine: &Quarantine,
        root_name: &OsStr,
    ) -> Result<()> {
        if quarantine.root != self.path
            || OsStr::new(&quarantine.name) == root_name
            || (self.complete && !quarantine.unlink_committed)
        {
            return Err(FlowError::guard("invalid directory removal quarantine"));
        }
        validate_quarantine_name(&quarantine.name)
    }

    fn quarantine_path(&self) -> Result<PathBuf> {
        let quarantine = self
            .quarantine
            .as_ref()
            .ok_or_else(|| FlowError::guard("removal lacks quarantine receipt"))?;
        Ok(self.path.with_file_name(&quarantine.name))
    }

    fn verify_absent(&self, parent: &File, name: &OsStr) -> Result<()> {
        complete_committed(parent, name, &self.path)?;
        let quarantine = self
            .quarantine
            .as_ref()
            .ok_or_else(|| FlowError::guard("removal lacks quarantine receipt"))?;
        if kind_at(parent, OsStr::new(&quarantine.name))?.is_some() {
            return Err(replaced_quarantine_error(&self.path));
        }
        Ok(())
    }
}

fn validate_removal_path(path: &Path) -> Result<()> {
    if !path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, Component::RootDir | Component::Normal(_)))
        || path.as_os_str().as_bytes()[1..]
            .split(|byte| *byte == b'/')
            .any(|part| part.is_empty() || part == b"." || part == b"..")
    {
        return Err(FlowError::guard(
            "removal path must be an exact absolute path",
        ));
    }
    Ok(())
}

fn validate_quarantine_name(name: &str) -> Result<()> {
    let mut parts = Path::new(name).components();
    if !name.starts_with(".herdr-rm-")
        || !matches!(parts.next(), Some(Component::Normal(_)))
        || parts.next().is_some()
        || Path::new(name).file_name() != Some(OsStr::new(name))
    {
        return Err(FlowError::guard("invalid directory removal quarantine"));
    }
    Ok(())
}

fn open_absolute_parent(path: &Path) -> Result<File> {
    // open_dir_nofollow guards its final component. Walk the complete parent
    // from / through the existing descriptor walk to reject ancestor symlinks.
    let root = open_dir_nofollow(Path::new("/"))?;
    let relative = path
        .parent()
        .expect("absolute leaf parent")
        .strip_prefix("/")
        .map_err(|_| FlowError::guard("invalid removal parent"))?;
    open_beneath(&root, relative)
}

/// Match mountinfo paths by components, decoding the kernel's octal escaping.
/// Inspection is read-only; the caller must arrange removal of any mount.
fn reject_mounts(path: &Path) -> Result<()> {
    let mounts = std::fs::read_to_string("/proc/self/mountinfo")?;
    for row in mounts.lines() {
        let encoded = row
            .split_whitespace()
            .nth(4)
            .ok_or_else(|| FlowError::guard("malformed mount observation"))?;
        let mount = encoded
            .replace("\\040", " ")
            .replace("\\011", "\t")
            .replace("\\012", "\n")
            .replace("\\134", "\\");
        if Path::new(&mount).starts_with(path) {
            return Err(FlowError::guard("mounted subtree has no removal ownership"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::symlink;

    fn fixture() -> (tempfile::TempDir, DirectoryRemoval) {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("review");
        fs::create_dir_all(path.join("nested")).unwrap();
        fs::write(path.join("nested/result"), "owned").unwrap();
        let identity = RootIdentity::of(&fs::metadata(&path).unwrap());
        let removal = DirectoryRemoval::new(path, identity).unwrap();
        (temp, removal)
    }

    fn roundtrip(state: &DirectoryRemoval) -> DirectoryRemoval {
        serde_json::from_str(&serde_json::to_string(state).unwrap()).unwrap()
    }

    fn crash() -> FlowError {
        FlowError::progress("injected persistence interruption")
    }

    fn rename_intent(state: &mut DirectoryRemoval) {
        let mut durable = None;
        assert!(state
            .drive(|next| {
                durable = Some(roundtrip(next));
                Err(crash())
            })
            .is_err());
        *state = durable.unwrap();
    }

    #[test]
    fn completes_and_repeats_without_touching_siblings_or_symlink_targets() {
        let (temp, mut state) = fixture();
        let outside = temp.path().join("outside");
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("keep"), "external").unwrap();
        symlink(&outside, state.path.join("external-link")).unwrap();
        let original = state.identity;
        let mut durable = None;
        state
            .drive(|next| {
                durable = Some(roundtrip(next));
                Ok(())
            })
            .unwrap();
        assert!(state.complete);
        assert_eq!(state.identity, original);
        assert!(!state.path.exists());
        assert_eq!(
            fs::read_to_string(outside.join("keep")).unwrap(),
            "external"
        );
        let mut recovered = durable.unwrap();
        recovered
            .drive(|_| panic!("completion should need no new checkpoint"))
            .unwrap();
        assert!(temp.path().is_dir());
    }

    #[test]
    fn every_persisted_prefix_recovers_with_the_original_identity() {
        // Include a crash immediately after saving the completion receipt.
        for stop_at in 1..=3 {
            let (_temp, mut state) = fixture();
            let original = state.identity;
            let mut durable = None;
            let mut checkpoint = 0;
            assert!(
                state
                    .drive(|next| {
                        checkpoint += 1;
                        durable = Some(roundtrip(next));
                        if checkpoint == stop_at {
                            Err(crash())
                        } else {
                            Ok(())
                        }
                    })
                    .is_err(),
                "checkpoint {stop_at} must be reached"
            );
            let mut recovered = durable.unwrap();
            recovered.drive(|_| Ok(())).unwrap();
            assert!(recovered.complete);
            assert_eq!(recovered.identity, original);
            assert!(!recovered.path.exists());
        }
    }

    #[test]
    fn rename_and_unlink_outcomes_recover_from_their_prior_intents() {
        for after_unlink in [false, true] {
            let (_temp, mut state) = fixture();
            rename_intent(&mut state);
            let quarantine = state.quarantine_path().unwrap();
            fs::rename(&state.path, &quarantine).unwrap();
            if after_unlink {
                // Obtain the durable unlink checkpoint, then reproduce the
                // filesystem outcome of unlink before the next checkpoint.
                let mut durable = None;
                assert!(state
                    .drive(|next| {
                        durable = Some(roundtrip(next));
                        Err(crash())
                    })
                    .is_err());
                state = durable.unwrap();
                assert!(state.quarantine.as_ref().unwrap().unlink_committed);
                fs::remove_dir(&quarantine).unwrap();
            }
            state.drive(|_| Ok(())).unwrap();
            assert!(state.complete);
            assert!(!quarantine.exists());
        }
    }

    #[test]
    fn failed_persistence_prevents_rename_and_final_unlink() {
        let (_temp, mut state) = fixture();
        assert!(state.drive(|_| Err(crash())).is_err());
        assert!(state.quarantine.is_none());
        assert!(state.path.join("nested/result").is_file());
        rename_intent(&mut state);
        let quarantine = state.quarantine_path().unwrap();
        fs::rename(&state.path, &quarantine).unwrap();
        assert!(state.drive(|_| Err(crash())).is_err());
        assert!(quarantine.is_dir());
        assert!(!state.quarantine.as_ref().unwrap().unlink_committed);
        state.drive(|_| Ok(())).unwrap();
    }

    #[test]
    fn replacements_and_unexplained_missing_entries_are_rejected() {
        for with_intent in [false, true] {
            let (temp, mut state) = fixture();
            if with_intent {
                rename_intent(&mut state);
            }
            let moved = temp.path().join("moved-original");
            fs::rename(&state.path, &moved).unwrap();
            assert!(state.drive(|_| Ok(())).is_err());
            fs::create_dir(&state.path).unwrap();
            fs::write(state.path.join("foreign"), "keep").unwrap();
            assert!(state.drive(|_| Ok(())).is_err());
            assert_eq!(
                fs::read_to_string(state.path.join("foreign")).unwrap(),
                "keep"
            );
            assert!(moved.join("nested/result").is_file());
        }
        let (temp, mut state) = fixture();
        rename_intent(&mut state);
        let quarantine = state.quarantine_path().unwrap();
        fs::rename(&state.path, temp.path().join("original")).unwrap();
        fs::create_dir(&quarantine).unwrap();
        fs::write(quarantine.join("foreign"), "keep").unwrap();
        assert!(state.drive(|_| Ok(())).is_err());
        assert!(quarantine.join("foreign").is_file());
    }

    #[test]
    fn completed_receipt_rejects_reappeared_entries() {
        let (_temp, mut state) = fixture();
        state.drive(|_| Ok(())).unwrap();
        fs::create_dir(&state.path).unwrap();
        fs::write(state.path.join("foreign"), "keep").unwrap();
        assert!(state.drive(|_| Ok(())).is_err());
        assert!(state.path.join("foreign").is_file());
    }

    #[test]
    fn symlink_ancestors_and_invalid_quarantine_names_are_rejected() {
        let (temp, mut state) = fixture();
        let alias = temp.path().join("alias");
        symlink(temp.path(), &alias).unwrap();
        assert!(DirectoryRemoval::new(alias.join("review"), state.identity).is_err());
        state.quarantine = Some(Quarantine {
            root: state.path.clone(),
            name: ".herdr-rm-escape/../../outside".into(),
            unlink_committed: true,
        });
        assert!(state
            .drive(|_| panic!("invalid state must not be persisted"))
            .is_err());
        assert!(state.path.join("nested/result").is_file());
    }

    #[test]
    fn mountpoint_is_rejected_without_attempting_removal() {
        // /proc is a mount in the supported Linux environment. This check is
        // read-only and requires no mount namespace or elevated privileges.
        assert!(reject_mounts(Path::new("/proc")).is_err());
    }
}
