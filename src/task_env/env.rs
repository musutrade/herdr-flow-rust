//! Task environment identity, ownership and lifecycle state.
//!
//! Ownership rests on three durable facts, not on paths alone:
//!
//! * an `instance_id` that names one concrete environment, so a stale handle
//!   can never act on a newer environment that reused the same task id;
//! * the `(device, inode)` identity of each disposable root, so a path that was
//!   swapped for a symlink or another directory is rejected instead of removed;
//! * a per-task exclusive lock held for the lifetime of the handle, so two
//!   handles cannot own the same environment and release each other's slot.

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fs::{File, OpenOptions};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use crate::error::{FlowError, Result};
use crate::util;

use super::boundary::DeliveryTarget;
use super::registry;

/// Environment variable that marks every process the task starts.
///
/// The value is the environment's `instance_id`. It is inherited across
/// `fork`/`exec`, survives `setsid()`, and remains readable at
/// `/proc/<pid>/environ` after the process is orphaned. Cleanup therefore has
/// positive evidence of ownership instead of guessing from process groups.
pub const MARKER_ENV: &str = "HERDR_TASK_ENV_INSTANCE";

/// Lifecycle state of a task environment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EnvironmentState {
    Creating,
    Active,
    Destroying,
    DestroyFailed,
    Destroyed,
}

impl EnvironmentState {
    pub fn is_live(self) -> bool {
        matches!(
            self,
            EnvironmentState::Creating | EnvironmentState::Active | EnvironmentState::DestroyFailed
        )
    }
}

/// Stable identity of a directory, from `stat(2)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RootIdentity {
    pub dev: u64,
    pub ino: u64,
}

impl RootIdentity {
    pub fn of(metadata: &std::fs::Metadata) -> Self {
        Self {
            dev: metadata.dev(),
            ino: metadata.ino(),
        }
    }
}

/// A worktree owned by the environment, plus the repository it belongs to.
///
/// The repository is captured at registration time (from `.git`), so cleanup
/// can remove the administrative record even if the working tree is already
/// gone. `.git` is only ever used to locate the repository, never as a path to
/// delete.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorktreeRegistration {
    pub path: PathBuf,
    pub repo: Option<PathBuf>,
}

/// A registered child process.
///
/// `pgid` must equal `pid`: the environment only registers process-group
/// leaders. Markers discover later children; the registration snapshot retains
/// known descendants after the parent exits. This is not exhaustive discovery
/// of markerless children. `start_time` guards against observed PID reuse.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegisteredProcess {
    pub pid: i32,
    pub pgid: i32,
    pub start_time: Option<u64>,
    /// Descendants visible at registration. Captured while the process is
    /// alive so a later orphan that leaves the process group or session can
    /// still be attributed to this environment.
    #[serde(default)]
    pub descendants: Vec<ProcessRecord>,
    /// Diagnostic snapshot of the supervisor's direct children at registration.
    /// These can belong to other tasks. This list and later differences are
    /// not ownership evidence and do not authorize signalling or retain slots.
    #[serde(default)]
    pub direct_children: Vec<ProcessRecord>,
}

/// A process identity that survives pid reuse only while the start time
/// matches.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessRecord {
    pub pid: i32,
    pub start_time: Option<u64>,
}

/// A live task environment with an explicit disposable boundary.
#[derive(Debug, Serialize, Deserialize)]
pub struct TaskEnvironment {
    pub task_id: String,
    pub instance_id: String,
    pub workspace_root: PathBuf,
    pub temp_root: PathBuf,
    pub workspace_identity: Option<RootIdentity>,
    pub temp_identity: Option<RootIdentity>,
    pub delivery_targets: Vec<DeliveryTarget>,
    pub state: EnvironmentState,
    /// Temporary delivery/cleanup journal; removed with the environment.
    #[serde(default)]
    pub delivery: serde_json::Value,
    pub created_at: f64,
    pub updated_at: f64,
    pub worktree_registrations: Vec<WorktreeRegistration>,
    pub child_processes: Vec<RegisteredProcess>,
    pub open_files: Vec<PathBuf>,
    pub state_file: PathBuf,
    /// Descriptors owned by this handle (not durable).
    #[serde(skip, default = "new_handle_slot")]
    file_handles: Arc<Mutex<BTreeMap<PathBuf, File>>>,
    /// Per-task ownership lock (not durable).
    #[serde(skip, default = "new_lock_slot")]
    ownership_lock: Arc<Mutex<Option<File>>>,
}

fn new_handle_slot() -> Arc<Mutex<BTreeMap<PathBuf, File>>> {
    Arc::new(Mutex::new(BTreeMap::new()))
}

fn new_lock_slot() -> Arc<Mutex<Option<File>>> {
    Arc::new(Mutex::new(None))
}

impl TaskEnvironment {
    pub fn state_file(&self) -> &Path {
        &self.state_file
    }

    /// Atomically replace this handle's durable document.
    ///
    /// Caller must retain task ownership and serialize mutations. This method
    /// does not acquire/check that lock or roll back in-memory changes. An I/O
    /// error after rename may leave the new document visible; inspect/recover
    /// durable state before retrying a higher-level operation.
    pub fn persist(&self) -> Result<()> {
        let value = serde_json::to_value(self)?;
        util::atomic_write_json(&self.state_file, &value)
    }

    fn transition(&mut self, state: EnvironmentState) -> Result<()> {
        self.state = state;
        self.updated_at = util::now();
        self.persist()
    }

    pub fn mark_active(&mut self) -> Result<()> {
        self.transition(EnvironmentState::Active)
    }

    pub fn mark_destroying(&mut self) -> Result<()> {
        self.transition(EnvironmentState::Destroying)
    }

    pub fn mark_destroy_failed(&mut self) -> Result<()> {
        self.transition(EnvironmentState::DestroyFailed)
    }

    /// Release the per-task ownership lock so a retry can proceed.
    /// Repeated calls are harmless. This does not destroy resources or release
    /// registry claims; stop mutating this handle after relinquishing ownership.
    /// Panics if its internal ownership mutex was poisoned.
    pub fn release_ownership(&self) {
        self.ownership_lock
            .lock()
            .expect("ownership slot poisoned")
            .take();
    }

    /// Whether this handle still holds the ownership lock.
    pub fn owns_lock(&self) -> bool {
        self.ownership_lock
            .lock()
            .expect("ownership slot poisoned")
            .is_some()
    }

    /// Build a command that starts a task process owned by this environment.
    ///
    /// The child is placed in its own session so it leads its own process
    /// group, and it inherits the environment marker. Only processes started
    /// this way (or descended from them) can be registered and reclaimed.
    /// Paths and the Landlock ruleset are prepared in the parent when building
    /// the command; only restriction syscalls run after fork. Roots must exist
    /// at construction time. Preparation failure is deferred to `spawn` as an
    /// OS error (EOPNOTSUPP when the kernel lacks the required ABI), so callers
    /// must check spawn's Result. The parent is not confined by preparation.
    pub fn command(&self, program: impl AsRef<OsStr>) -> Command {
        self.process_command(program, true)
    }

    /// Trusted namespace bootstrap only: bwrap must write uid_map before its
    /// read-only mount policy can take effect. Never execute an agent directly.
    pub(crate) fn namespace_command(&self, launcher: &Path) -> Command {
        self.process_command(launcher, false)
    }

    fn process_command(&self, program: impl AsRef<OsStr>, confine: bool) -> Command {
        let mut command = Command::new(program);
        command.env(MARKER_ENV, &self.instance_id);
        // Keep temporary files inside the disposable boundary as well.
        command.env("TMPDIR", &self.temp_root);
        let roots = vec![self.workspace_root.clone(), self.temp_root.clone()];
        let prepared = confine.then(|| {
            PreparedConfinement::new(&roots)
                .map_err(|error| error.raw_os_error().unwrap_or(libc::EOPNOTSUPP))
        });
        // SAFETY: paths, rules and errors are prepared before fork. The child
        // borrows the prepared fd and only calls setsid/prctl/restrict_self;
        // errors are constructed from scalar errno values without allocation.
        unsafe {
            use std::os::unix::process::CommandExt as _;
            command.pre_exec(move || {
                if libc::setsid() < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                match &prepared {
                    Some(Ok(policy)) => policy.apply(),
                    Some(Err(errno)) => Err(std::io::Error::from_raw_os_error(*errno)),
                    None => Ok(()),
                }
            });
        }
        command
    }

    /// Whether `pid` carries this environment's marker.
    pub fn owns_process(&self, pid: i32) -> bool {
        process_has_marker(pid, &self.instance_id)
    }

    /// Register a worktree that lives inside the disposable workspace.
    pub fn register_worktree(&mut self, path: &Path) -> Result<()> {
        let resolved = util::resolve_path(path);
        if !self.owns_path(&resolved) {
            return Err(FlowError::progress(format!(
                "worktree {} is outside the disposable boundary",
                resolved.display()
            )));
        }
        if self
            .worktree_registrations
            .iter()
            .any(|item| item.path == resolved)
        {
            return Ok(());
        }
        let registration = WorktreeRegistration {
            repo: discover_repository(&resolved),
            path: resolved,
        };
        self.worktree_registrations.push(registration);
        self.persist()
    }

    /// Take ownership of a child process.
    ///
    /// The process must lead its own process group (spawn with `setsid` or
    /// `setpgid(0, 0)`); otherwise descendants cannot be reclaimed reliably
    /// and registration is refused. It must also carry this environment's
    /// marker, normally inherited from `command`. This records cleanup metadata;
    /// it does not take the caller's Child or assume responsibility for wait().
    /// Keep the ownership lock while registering. /proc reads are observations,
    /// not an atomic identity proof; disappearance can fail registration.
    /// Persistence failure can leave the in-memory record appended.
    pub fn register_process(&mut self, pid: i32) -> Result<()> {
        if pid <= 1 {
            return Err(FlowError::progress(format!(
                "invalid process id {pid}: must be greater than 1"
            )));
        }
        if !self.owns_process(pid) {
            return Err(FlowError::progress(format!(
                "pid {pid} was not started with this environment's marker {}; spawn it with TaskEnvironment::command so it can be reclaimed",
                MARKER_ENV
            )));
        }
        let pgid = process_group(pid).ok_or_else(|| {
            FlowError::progress(format!("cannot read process group of pid {pid}"))
        })?;
        // SAFETY: getpgrp takes no pointers and has no caller preconditions.
        let own_group = unsafe { libc::getpgrp() };
        if pgid != pid {
            return Err(FlowError::progress(format!(
                "pid {pid} is in process group {pgid}, not its own group; spawn it with setsid so its descendants can be reclaimed"
            )));
        }
        if pgid == own_group {
            return Err(FlowError::progress(format!(
                "pid {pid} shares the supervisor's process group"
            )));
        }
        let entry = RegisteredProcess {
            pid,
            pgid,
            start_time: process_start_time(pid),
            descendants: descendants_snapshot(pid),
            direct_children: direct_children_of(std::process::id() as i32),
        };
        if !self.child_processes.contains(&entry) {
            self.child_processes.push(entry);
            self.persist()?;
        }
        Ok(())
    }

    /// Take ownership of an already-open descriptor.
    ///
    /// Transfers one descriptor into this handle's local ownership. Releasing
    /// it closes that descriptor; duplicate descriptors held elsewhere are
    /// unaffected and the underlying file is not thereby deleted.
    /// Caller must hold task ownership and ensure `handle` corresponds to `path`;
    /// the function checks path containment, not descriptor/path identity.
    /// Ownership of the supplied File transfers even on error: rejection or a
    /// duplicate path drops it; persistence failure after insertion retains it
    /// in this handle until release/drop. Duplicate paths do not add another fd.
    /// Restoring metadata does not restore OS descriptors. Panics if the internal
    /// descriptor mutex was poisoned.
    pub fn register_handle(&mut self, path: &Path, handle: File) -> Result<()> {
        let resolved = util::resolve_path(path);
        if !self.owns_path(&resolved) {
            return Err(FlowError::progress(format!(
                "refusing to register {} outside the disposable boundary",
                resolved.display()
            )));
        }
        if self.open_files.contains(&resolved) {
            return Ok(());
        }
        self.file_handles
            .lock()
            .expect("file handle slot poisoned")
            .insert(resolved.clone(), handle);
        self.open_files.push(resolved);
        self.persist()
    }

    /// Open `path` without following a final symlink and take ownership.
    pub fn register_file(&mut self, path: &Path) -> Result<()> {
        let resolved = util::resolve_path(path);
        if !self.owns_path(&resolved) {
            return Err(FlowError::progress(format!(
                "refusing to register {} outside the disposable boundary",
                resolved.display()
            )));
        }
        if self.open_files.contains(&resolved) {
            return Ok(());
        }
        let handle = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&resolved)
            .map_err(|error| {
                FlowError::progress(format!(
                    "cannot open {} for registration: {error}",
                    resolved.display()
                ))
            })?;
        self.register_handle(&resolved, handle)
    }

    pub fn unregister_worktree(&mut self, path: &Path) -> Result<()> {
        let resolved = util::resolve_path(path);
        self.worktree_registrations
            .retain(|item| item.path != resolved);
        self.persist()
    }

    pub fn unregister_process(&mut self, pid: i32) -> Result<()> {
        self.child_processes.retain(|entry| entry.pid != pid);
        self.persist()
    }

    /// Remove one path's cleanup registration and drop only its locally owned fd.
    /// Metadata restored without a live fd cannot close another path's handle.
    /// The file itself is not unlinked. Repeated removal is harmless; persistence
    /// failure does not undo the in-memory removal or reopen the descriptor.
    /// Caller must retain task ownership; a poisoned descriptor mutex panics.
    pub fn unregister_file(&mut self, path: &Path) -> Result<()> {
        let resolved = util::resolve_path(path);
        self.open_files.retain(|item| item != &resolved);
        self.file_handles
            .lock()
            .expect("file handle slot poisoned")
            .remove(&resolved);
        self.persist()
    }

    /// Drop every descriptor this handle still owns.
    pub fn release_file_handles(&self) {
        self.file_handles
            .lock()
            .expect("file handle slot poisoned")
            .clear();
    }

    pub fn open_handle_count(&self) -> usize {
        self.file_handles
            .lock()
            .expect("file handle slot poisoned")
            .len()
    }

    /// True when `path` lexically lies inside the disposable boundary.
    pub fn owns_path(&self, path: &Path) -> bool {
        let resolved = util::resolve_path(path);
        util::is_relative_to(&resolved, &self.workspace_root)
            || util::is_relative_to(&resolved, &self.temp_root)
    }

    /// Durable roots with their recorded identities.
    pub fn roots(&self) -> [(PathBuf, Option<RootIdentity>); 2] {
        [
            (self.workspace_root.clone(), self.workspace_identity),
            (self.temp_root.clone(), self.temp_identity),
        ]
    }

    fn attach_lock(&self, lock: File) {
        *self.ownership_lock.lock().expect("ownership slot poisoned") = Some(lock);
    }
}

/// Process group of `pid`, if visible.
pub fn process_group(pid: i32) -> Option<i32> {
    // SAFETY: getpgid takes only a scalar and performs no memory access.
    let pgid = unsafe { libc::getpgid(pid) };
    if pgid > 0 {
        Some(pgid)
    } else {
        None
    }
}

/// Kernel start time of a process.
pub fn process_start_time(pid: i32) -> Option<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let rest = stat.rsplit_once(')')?.1;
    rest.split_whitespace().nth(19)?.parse().ok()
}

/// Locate the main repository of a linked worktree from its `.git` file.
///
/// Only used to find the repository for `git worktree` commands. The parsed
/// path is verified to sit at `<repo>/.git/worktrees/<name>`.
pub fn discover_repository(worktree: &Path) -> Option<PathBuf> {
    let admin = worktree_admin(worktree)?;
    let worktrees = admin.parent()?;
    require_name(worktrees, "worktrees")?;
    let dot_git = worktrees.parent()?;
    require_name(dot_git, ".git")?;
    dot_git.parent().map(Path::to_path_buf)
}

fn worktree_admin(worktree: &Path) -> Option<PathBuf> {
    let gitfile = worktree.join(".git");
    let gitdir = read_gitdir(worktree, &gitfile)?;
    gitdir.canonicalize().ok()
}

fn read_gitdir(worktree: &Path, gitfile: &Path) -> Option<PathBuf> {
    let metadata = std::fs::symlink_metadata(gitfile).ok()?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return None;
    }
    let text = std::fs::read_to_string(gitfile).ok()?;
    let gitdir = Path::new(text.strip_prefix("gitdir:")?.trim());
    Some(if gitdir.is_absolute() {
        gitdir.to_path_buf()
    } else {
        worktree.join(gitdir)
    })
}

fn require_name(path: &Path, expected: &str) -> Option<()> {
    if path.file_name()? == expected {
        Some(())
    } else {
        None
    }
}

fn process_state(pid: i32) -> Option<char> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let rest = stat.rsplit_once(')')?.1;
    rest.split_whitespace().next()?.chars().next()
}

/// Best-effort diagnostic: whether a recorded identity is observed live.
/// Observation failure also returns false. Destructive cleanup uses its own
/// fallible observation and must not use this boolean as proof of exit.
pub fn process_record_is_alive(record: &ProcessRecord) -> bool {
    if record.pid <= 1 {
        return false;
    }
    if matches!(process_state(record.pid), Some('Z') | None) {
        return false;
    }
    match (process_start_time(record.pid), record.start_time) {
        (Some(current), Some(expected)) => current == expected,
        (Some(_), None) => true,
        (None, _) => false,
    }
}

/// Current direct children of `pid`.
pub fn direct_children_of(pid: i32) -> Vec<ProcessRecord> {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    let mut children = Vec::new();
    for entry in entries.flatten() {
        let Some(name) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        let Ok(child) = name.parse::<i32>() else {
            continue;
        };
        if parent_of(child) == Some(pid) {
            children.push(ProcessRecord {
                pid: child,
                start_time: process_start_time(child),
            });
        }
    }
    children.sort_by_key(|record| record.pid);
    children
}

const LANDLOCK_CREATE_RULESET_VERSION: u32 = 1;
const LANDLOCK_RULE_PATH_BENEATH: u32 = 1;
const LANDLOCK_ACCESS_FS_WRITE_FILE: u64 = 1 << 1;
const LANDLOCK_ACCESS_FS_REMOVE_DIR: u64 = 1 << 4;
const LANDLOCK_ACCESS_FS_REMOVE_FILE: u64 = 1 << 5;
const LANDLOCK_ACCESS_FS_MAKE_CHAR: u64 = 1 << 6;
const LANDLOCK_ACCESS_FS_MAKE_DIR: u64 = 1 << 7;
const LANDLOCK_ACCESS_FS_MAKE_REG: u64 = 1 << 8;
const LANDLOCK_ACCESS_FS_MAKE_SOCK: u64 = 1 << 9;
const LANDLOCK_ACCESS_FS_MAKE_FIFO: u64 = 1 << 10;
const LANDLOCK_ACCESS_FS_MAKE_BLOCK: u64 = 1 << 11;
const LANDLOCK_ACCESS_FS_MAKE_SYM: u64 = 1 << 12;
const LANDLOCK_ACCESS_FS_REFER: u64 = 1 << 13;
const LANDLOCK_ACCESS_FS_TRUNCATE: u64 = 1 << 14;

#[repr(C)]
struct LandlockRulesetAttr {
    handled_access_fs: u64,
}

#[repr(C, packed)]
struct LandlockPathBeneathAttr {
    allowed_access: u64,
    parent_fd: i32,
}

/// Kernel-enforced protection of the environment's state and directory entries.
///
/// The child may write, create, rename and remove files *inside* its disposable
/// roots, but every other location is read-only for it: it cannot rewrite the
/// environment document (which would let it retarget cleanup at external
/// assets), cannot truncate state files, and cannot rename or remove the roots
/// themselves. This makes both the cleanup metadata and the parent directory
/// entry something the task cannot rewrite.
pub(crate) struct PreparedConfinement {
    ruleset: File,
}

impl PreparedConfinement {
    /// Prepare all paths, rules and fallible allocations in the parent.
    pub(crate) fn new(roots: &[PathBuf]) -> std::io::Result<Self> {
        let handled = handled_access(required_landlock_abi()?);
        let fd = create_landlock_ruleset(handled)?;
        // SAFETY: successful creation returns one fresh CLOEXEC ruleset fd;
        // transfer ownership once so every preparation error closes it.
        let ruleset = unsafe { File::from_raw_fd(fd) };
        add_root_rules(ruleset.as_raw_fd(), handled, roots)?;
        Ok(Self { ruleset })
    }

    /// Apply only in the fork child: no allocation, locks, or path lookup.
    pub(crate) fn apply(&self) -> std::io::Result<()> {
        set_no_new_privs()?;
        restrict_self(self.ruleset.as_raw_fd())
    }
}

fn add_root_rules(ruleset: i32, handled: u64, roots: &[PathBuf]) -> std::io::Result<()> {
    roots
        .iter()
        .try_for_each(|root| add_root_rule(ruleset, handled, root))?;
    add_null_rule(ruleset)
}

// /dev/null is a sink, not retained task storage. Native runtimes use it for
// detached subprocess I/O. No other device or global path becomes writable.
fn add_null_rule(ruleset: i32) -> std::io::Result<()> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_PATH | libc::O_CLOEXEC)
        .open("/dev/null")?;
    let rule = LandlockPathBeneathAttr {
        allowed_access: LANDLOCK_ACCESS_FS_WRITE_FILE,
        parent_fd: file.as_raw_fd(),
    };
    add_rule_syscall(ruleset, &rule)
}

fn set_no_new_privs() -> std::io::Result<()> {
    // SAFETY: prctl takes scalar arguments only.
    if unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

fn required_landlock_abi() -> std::io::Result<i64> {
    // SAFETY: a null ruleset with VERSION returns the supported ABI.
    let abi = unsafe {
        libc::syscall(
            libc::SYS_landlock_create_ruleset,
            std::ptr::null::<LandlockRulesetAttr>(),
            0usize,
            LANDLOCK_CREATE_RULESET_VERSION,
        )
    };
    // TRUNCATE is a separate right introduced in ABI 3 and is required to keep
    // the control documents unmodifiable; an older kernel must fail closed.
    if abi < 3 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            format!("Landlock ABI 3 (TRUNCATE) required, kernel reports {abi}"),
        ));
    }
    Ok(abi)
}

fn handled_access(_abi: i64) -> u64 {
    LANDLOCK_ACCESS_FS_WRITE_FILE
        | LANDLOCK_ACCESS_FS_REMOVE_FILE
        | LANDLOCK_ACCESS_FS_REMOVE_DIR
        | LANDLOCK_ACCESS_FS_MAKE_CHAR
        | LANDLOCK_ACCESS_FS_MAKE_DIR
        | LANDLOCK_ACCESS_FS_MAKE_REG
        | LANDLOCK_ACCESS_FS_MAKE_SOCK
        | LANDLOCK_ACCESS_FS_MAKE_FIFO
        | LANDLOCK_ACCESS_FS_MAKE_BLOCK
        | LANDLOCK_ACCESS_FS_MAKE_SYM
        | LANDLOCK_ACCESS_FS_REFER
        | LANDLOCK_ACCESS_FS_TRUNCATE
}

fn create_landlock_ruleset(handled: u64) -> std::io::Result<i32> {
    let attr = LandlockRulesetAttr {
        handled_access_fs: handled,
    };
    // SAFETY: attr is a plain C struct; the syscall only reads it.
    let ruleset = unsafe {
        libc::syscall(
            libc::SYS_landlock_create_ruleset,
            &attr as *const LandlockRulesetAttr,
            std::mem::size_of::<LandlockRulesetAttr>(),
            0u32,
        )
    };
    if ruleset < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(ruleset as i32)
}

fn add_root_rule(ruleset: i32, handled: u64, root: &Path) -> std::io::Result<()> {
    let directory = open_rule_root(root)?;
    let rule = LandlockPathBeneathAttr {
        allowed_access: handled,
        parent_fd: directory.as_raw_fd(),
    };
    add_rule_syscall(ruleset, &rule)
}

fn open_rule_root(root: &Path) -> std::io::Result<File> {
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_PATH | libc::O_DIRECTORY | libc::O_CLOEXEC)
        .open(root)
}

fn add_rule_syscall(ruleset: i32, rule: &LandlockPathBeneathAttr) -> std::io::Result<()> {
    // SAFETY: ruleset is live and rule outlives the call.
    let result = unsafe {
        libc::syscall(
            libc::SYS_landlock_add_rule,
            ruleset,
            LANDLOCK_RULE_PATH_BENEATH,
            rule as *const LandlockPathBeneathAttr,
            0u32,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

fn restrict_self(ruleset: i32) -> std::io::Result<()> {
    // SAFETY: the PreparedConfinement File remains borrowed and live; the
    // syscall consumes no ownership. CLOEXEC closes the child copy on exec.
    let result = unsafe { libc::syscall(libc::SYS_landlock_restrict_self, ruleset, 0u32) };
    let error = std::io::Error::last_os_error();
    if result == 0 {
        Ok(())
    } else {
        Err(error)
    }
}

/// Whether a process carries the marker for `instance_id`.
pub fn process_has_marker(pid: i32, instance_id: &str) -> bool {
    let Ok(bytes) = std::fs::read(format!("/proc/{pid}/environ")) else {
        return false;
    };
    let needle = format!("{MARKER_ENV}={instance_id}");
    bytes
        .split(|byte| *byte == 0)
        .any(|entry| entry == needle.as_bytes())
}

/// Best-effort diagnostic list of visible processes carrying the marker.
/// Scan failures return an empty list; cleanup and delivery use fallible scans.
pub fn marked_processes(instance_id: &str) -> Vec<ProcessRecord> {
    super::discovery::marked_processes(instance_id).unwrap_or_default()
}

/// Best-effort registration snapshot of currently visible transitive children.
/// Cleanup uses a fallible scan instead of treating scan failure as no children.
pub fn descendants_snapshot(pid: i32) -> Vec<ProcessRecord> {
    super::discovery::descendants_snapshot(pid).unwrap_or_default()
}

pub(super) fn parent_of(pid: i32) -> Option<i32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // After comm: state ppid pgrp ...
    stat.rsplit_once(')')?
        .1
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()
}

/// Best-effort diagnostic for a registered live (non-zombie) process.
/// Observation failure also returns false; this is not proof of exit.
pub fn process_is_alive(entry: &RegisteredProcess) -> bool {
    if entry.pid <= 1 {
        return false;
    }
    if matches!(process_state(entry.pid), Some('Z') | None) {
        return false;
    }
    match (process_start_time(entry.pid), entry.start_time) {
        (Some(current), Some(expected)) => current == expected,
        (Some(_), None) => true,
        (None, _) => false,
    }
}

/// Create a new task environment.
///
/// Protocol (each step leaves a recoverable state):
/// 1. take the per-task lock (exclusive ownership);
/// 2. validate ids, paths, targets and root claims;
/// 3. write the environment document with `creating` **before** any directory
///    or index entry exists, so a crash here is always visible;
/// 4. claim the roots in the index (unique ownership, resource limit);
/// 5. materialise the roots and record their identities;
/// 6. mark the document `active`.
///
/// Roots must be distinct/non-overlapping, vacant and outside the control tree;
/// delivery targets must not overlap them. Existing ancestors must pass the
/// ownership/mode checks. The returned handle retains the task lock; another
/// create is not an idempotent lookup. Failures may leave recoverable metadata
/// or roots despite best-effort rollback: use `recover_interrupted`, not blind
/// deletion or repeated creation. Process confinement/cleanup depends on Linux
/// /proc and Landlock support; successful creation alone does not test spawning.
pub fn create(
    task_id: &str,
    workspace_root: &Path,
    delivery_targets: Vec<DeliveryTarget>,
    temp_root: &Path,
) -> Result<TaskEnvironment> {
    registry::validate_task_id(task_id)?;
    let lock = acquire_task_lock(task_id)?;
    let (workspace_root, temp_root, delivery_targets) =
        prepare_roots(workspace_root, delivery_targets, temp_root)?;
    let state_file = ensure_state_absent(task_id)?;

    let mut env = build_environment(
        task_id,
        workspace_root.clone(),
        temp_root.clone(),
        delivery_targets,
        state_file.clone(),
    );
    env.attach_lock(lock);

    if let Err(error) = env.persist() {
        env.release_ownership();
        return Err(error);
    }
    if let Err(error) = reserve_environment(&env, &state_file) {
        let _ = std::fs::remove_file(&state_file);
        env.release_ownership();
        return Err(error);
    }
    if let Err(error) = materialize_and_fingerprint(&mut env) {
        rollback_creation(&env);
        return Err(error);
    }
    if let Err(error) = env.mark_active() {
        env.release_ownership();
        return Err(error);
    }
    Ok(env)
}

fn acquire_task_lock(task_id: &str) -> Result<std::fs::File> {
    registry::try_lock_task(task_id)?.ok_or_else(|| {
        FlowError::progress(format!(
            "environment for task {task_id} is already owned by another live handle"
        ))
    })
}

fn prepare_roots(
    workspace_root: &Path,
    delivery_targets: Vec<DeliveryTarget>,
    temp_root: &Path,
) -> Result<(PathBuf, PathBuf, Vec<DeliveryTarget>)> {
    let workspace_root = util::resolve_path(workspace_root);
    let temp_root = util::resolve_path(temp_root);
    let delivery_targets =
        super::boundary::validate_delivery_targets(delivery_targets, &workspace_root, &temp_root)?;
    ensure_targets_disjoint(&delivery_targets, &workspace_root, &temp_root)?;
    validate_root_layout(&workspace_root, &temp_root)?;
    Ok((workspace_root, temp_root, delivery_targets))
}

fn ensure_targets_disjoint(
    targets: &[DeliveryTarget],
    workspace_root: &Path,
    temp_root: &Path,
) -> Result<()> {
    for target in targets {
        for root in [workspace_root, temp_root] {
            if util::overlap(&target.path, root) {
                return Err(FlowError::progress(format!(
                    "delivery target {} overlaps with disposable root {}",
                    target.path.display(),
                    root.display()
                )));
            }
        }
    }
    Ok(())
}

fn validate_root_layout(workspace_root: &Path, temp_root: &Path) -> Result<()> {
    ensure_distinct(workspace_root, temp_root)?;
    registry::validate_outside_control(workspace_root)?;
    registry::validate_outside_control(temp_root)?;
    require_vacant(workspace_root)?;
    require_vacant(temp_root)?;
    validate_ancestors(workspace_root)?;
    validate_ancestors(temp_root)?;
    Ok(())
}

fn ensure_distinct(workspace_root: &Path, temp_root: &Path) -> Result<()> {
    if util::overlap(workspace_root, temp_root) {
        return Err(FlowError::progress(format!(
            "workspace {} and temp root {} overlap",
            workspace_root.display(),
            temp_root.display()
        )));
    }
    Ok(())
}

fn ensure_state_absent(task_id: &str) -> Result<PathBuf> {
    let state_file = registry::environment_state_path(task_id)?;
    if state_file.exists() || registry::cleanup_state_path(task_id)?.exists() {
        return Err(FlowError::progress(format!(
            "environment for task {task_id} already exists; destroy or recover it first"
        )));
    }
    Ok(state_file)
}

fn build_environment(
    task_id: &str,
    workspace_root: PathBuf,
    temp_root: PathBuf,
    delivery_targets: Vec<DeliveryTarget>,
    state_file: PathBuf,
) -> TaskEnvironment {
    let now = util::now();
    TaskEnvironment {
        task_id: task_id.to_string(),
        instance_id: util::uuid_hex(),
        workspace_root,
        temp_root,
        workspace_identity: None,
        temp_identity: None,
        delivery_targets,
        state: EnvironmentState::Creating,
        delivery: serde_json::Value::Null,
        created_at: now,
        updated_at: now,
        worktree_registrations: Vec::new(),
        child_processes: Vec::new(),
        open_files: Vec::new(),
        state_file,
        file_handles: new_handle_slot(),
        ownership_lock: new_lock_slot(),
    }
}

fn reserve_environment(env: &TaskEnvironment, state_file: &Path) -> Result<()> {
    registry::register_environment(registry::RegistryEntry {
        task_id: env.task_id.clone(),
        instance_id: env.instance_id.clone(),
        workspace_root: env.workspace_root.clone(),
        temp_root: env.temp_root.clone(),
        roots: vec![env.workspace_root.clone(), env.temp_root.clone()],
        state_file: state_file.to_path_buf(),
        registered_at: util::now(),
    })
}

fn materialize_and_fingerprint(env: &mut TaskEnvironment) -> Result<()> {
    materialize(&env.workspace_root, &env.temp_root)?;
    let (workspace_identity, temp_identity) = fingerprint(&env.workspace_root, &env.temp_root)?;
    env.workspace_identity = Some(workspace_identity);
    env.temp_identity = Some(temp_identity);
    Ok(())
}

fn rollback_creation(env: &TaskEnvironment) {
    let _ = remove_created(&env.workspace_root);
    let _ = remove_created(&env.temp_root);
    let _ = std::fs::remove_file(&env.state_file);
    let _ = registry::unregister_environment(&env.task_id, &env.instance_id);
    env.release_ownership();
}

/// Load a live environment document and take ownership of it.
///
/// Fails while another live handle owns the task, which prevents a restored
/// handle from releasing descriptors or slots it does not own.
/// Reads metadata without restarting processes, reopening registered files or
/// completing interrupted cleanup. The returned handle owns a fresh lock and
/// an empty in-memory descriptor list. Active/creating/destroying states may all
/// be loaded; choose recovery based on the persisted state. Malformed/missing
/// metadata fails without reconstructing it, and each call needs exclusive
/// ownership rather than sharing an existing handle.
pub fn restore(task_id: &str) -> Result<TaskEnvironment> {
    registry::validate_task_id(task_id)?;
    let lock = acquire_task_lock(task_id)?;
    let state_file = registry::environment_state_path(task_id)?;
    ensure_state_exists(task_id, &state_file)?;
    let mut env = read_environment(&state_file)?;
    registry::validate_outside_control(&env.workspace_root)?;
    registry::validate_outside_control(&env.temp_root)?;
    env.state_file = state_file;
    env.file_handles = new_handle_slot();
    env.ownership_lock = new_lock_slot();
    env.attach_lock(lock);
    Ok(env)
}

fn ensure_state_exists(task_id: &str, state_file: &Path) -> Result<()> {
    if state_file.exists() {
        return Ok(());
    }
    Err(FlowError::progress(format!(
        "environment for task {task_id} does not exist"
    )))
}

fn read_environment(state_file: &Path) -> Result<TaskEnvironment> {
    let text = std::fs::read_to_string(state_file)?;
    serde_json::from_str(&text).map_err(|error| {
        FlowError::progress(format!("cannot parse {}: {error}", state_file.display()))
    })
}

/// Recover an environment left behind by an interrupted create or destroy.
///
/// Returns true when cleanup was performed, false when neither an interrupted
/// document nor a cleanup plan needs work. A live owner blocks recovery. This
/// can kill owned processes and remove disposable roots; it does not resume task
/// execution. Failed cleanup retains evidence/claims for retry, subject to the
/// cleanup retry limit. Repeated successful recovery eventually returns false.
pub fn recover_interrupted(task_id: &str) -> Result<bool> {
    registry::validate_task_id(task_id)?;
    let state_file = registry::environment_state_path(task_id)?;
    if !state_file.exists() {
        return recover_plan_only(task_id);
    }
    let env = restore(task_id)?;
    if !interrupted(&env.state) && !plan_exists(task_id) {
        return Ok(false);
    }
    super::cleanup::destroy(&env)?;
    Ok(true)
}

fn recover_plan_only(task_id: &str) -> Result<bool> {
    if !plan_exists(task_id) {
        return Ok(false);
    }
    super::cleanup::cleanup_failed_environment(task_id)?;
    Ok(true)
}

fn interrupted(state: &EnvironmentState) -> bool {
    *state != EnvironmentState::Active
}

fn plan_exists(task_id: &str) -> bool {
    registry::cleanup_state_path(task_id)
        .map(|path| path.exists())
        .unwrap_or(false)
}

/// Refuse to take over an existing directory that already holds data.
fn require_vacant(path: &Path) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
        Ok(metadata) => {
            if metadata.file_type().is_symlink() {
                return Err(FlowError::progress(format!(
                    "refusing to claim symlink {}",
                    path.display()
                )));
            }
            if !metadata.is_dir() {
                return Err(FlowError::progress(format!(
                    "refusing to claim non-directory {}",
                    path.display()
                )));
            }
            if std::fs::read_dir(path)?.next().is_some() {
                return Err(FlowError::progress(format!(
                    "refusing to claim non-empty directory {}",
                    path.display()
                )));
            }
            Ok(())
        }
    }
}

/// Create only the roots themselves. Parents must already exist, so an
/// environment never becomes the owner of directories it did not record.
fn materialize(workspace_root: &Path, temp_root: &Path) -> Result<()> {
    for root in [workspace_root, temp_root] {
        if root.exists() {
            continue;
        }
        let parent_ok = root.parent().map(Path::is_dir).unwrap_or(false);
        if !parent_ok {
            return Err(FlowError::progress(format!(
                "parent of {} must already exist",
                root.display()
            )));
        }
        {
            use std::os::unix::fs::DirBuilderExt;
            std::fs::DirBuilder::new().mode(0o700).create(root)
        }
        .map_err(|error| {
            FlowError::progress(format!("cannot create {}: {error}", root.display()))
        })?;
    }
    Ok(())
}

fn remove_created(path: &Path) -> std::io::Result<()> {
    std::fs::remove_dir(path).or_else(ignore_not_found)
}

fn ignore_not_found(error: std::io::Error) -> std::io::Result<()> {
    if error.kind() == std::io::ErrorKind::NotFound {
        Ok(())
    } else {
        Err(error)
    }
}

fn fingerprint(workspace_root: &Path, temp_root: &Path) -> Result<(RootIdentity, RootIdentity)> {
    let workspace = std::fs::symlink_metadata(workspace_root)?;
    let temp = std::fs::symlink_metadata(temp_root)?;
    if workspace.file_type().is_symlink() || !workspace.is_dir() {
        return Err(FlowError::progress(format!(
            "workspace {} must be a real directory",
            workspace_root.display()
        )));
    }
    if temp.file_type().is_symlink() || !temp.is_dir() {
        return Err(FlowError::progress(format!(
            "temp root {} must be a real directory",
            temp_root.display()
        )));
    }
    Ok((RootIdentity::of(&workspace), RootIdentity::of(&temp)))
}

/// Largest acceptable ancestor mode: owner-writable, not group/world writable
/// (a root-owned sticky directory such as `/tmp` is acceptable).
fn ancestor_is_trusted(metadata: &std::fs::Metadata) -> bool {
    // SAFETY: geteuid takes no pointers and has no caller preconditions.
    let uid = unsafe { libc::geteuid() };
    let owner_ok = metadata.uid() == uid || metadata.uid() == 0;
    let root_sticky = metadata.uid() == 0 && metadata.mode() & libc::S_ISVTX != 0;
    owner_ok && (metadata.mode() & 0o022 == 0 || root_sticky)
}

/// Check existing ancestors of `path` for symlinks and untrusted ownership/mode.
/// The leaf and missing ancestors are not checked. This is a point-in-time
/// pathname check, not a descriptor pin or protection against later same-UID
/// mutation; callers must preserve the trusted-directory deployment boundary.
pub fn validate_ancestors(path: &Path) -> Result<()> {
    let mut cursor = path.to_path_buf();
    while let Some(parent) = cursor.parent() {
        if !parent.as_os_str().is_empty() && parent.exists() {
            let metadata = std::fs::symlink_metadata(parent)?;
            if metadata.file_type().is_symlink() {
                return Err(FlowError::guard(format!(
                    "ancestor {} is a symlink",
                    parent.display()
                )));
            }
            if !ancestor_is_trusted(&metadata) {
                return Err(FlowError::guard(format!(
                    "ancestor {} is group/world writable or owned by another user",
                    parent.display()
                )));
            }
        }
        cursor = parent.to_path_buf();
    }
    Ok(())
}

/// Open a directory without following symlinks.
pub(crate) fn open_dir_nofollow(path: &Path) -> Result<File> {
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
        .map_err(|error| FlowError::progress(format!("cannot open {}: {error}", path.display())))
}

/// Open `relative` beneath an already-open directory, component by component,
/// never following a symlink.
pub(crate) fn open_beneath(root: &File, relative: &Path) -> Result<File> {
    let mut current = root.try_clone()?;
    for component in relative.components() {
        let name = component_name(component, relative)?;
        current = open_component(&current, name, relative)?;
    }
    Ok(current)
}

fn component_name<'a>(
    component: std::path::Component<'a>,
    relative: &Path,
) -> Result<&'a std::ffi::OsStr> {
    match component {
        std::path::Component::Normal(name) => Ok(name),
        _ => Err(FlowError::progress(format!(
            "invalid relative path {}",
            relative.display()
        ))),
    }
}

fn open_component(parent: &File, name: &std::ffi::OsStr, relative: &Path) -> Result<File> {
    let name = c_name(name)?;
    let fd = open_component_raw(parent, &name);
    file_from_fd(fd, relative)
}

fn c_name(name: &std::ffi::OsStr) -> Result<std::ffi::CString> {
    use std::os::unix::ffi::OsStrExt as _;
    std::ffi::CString::new(name.as_bytes())
        .map_err(|_| FlowError::progress("path component contains NUL"))
}

fn open_component_raw(parent: &File, name: &std::ffi::CString) -> i32 {
    // SAFETY: parent is a live directory fd and name is NUL-terminated.
    unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    }
}

fn file_from_fd(fd: i32, relative: &Path) -> Result<File> {
    if fd < 0 {
        return Err(FlowError::progress(format!(
            "cannot open {} beneath root: {}",
            relative.display(),
            std::io::Error::last_os_error()
        )));
    }
    // SAFETY: successful openat returns a fresh descriptor we now own.
    Ok(unsafe { <File as std::os::fd::FromRawFd>::from_raw_fd(fd) })
}

/// Entry kind relative to an open directory, without following a symlink.
pub(crate) fn kind_at(dir: &File, name: &std::ffi::OsStr) -> Result<Option<EntryKind>> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt as _;

    let name_c = CString::new(name.as_bytes())
        .map_err(|_| FlowError::progress("path component contains NUL"))?;
    // SAFETY: zeroed stat is a valid initial value for fstatat to fill.
    let mut stat: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: dir is live, name is NUL-terminated, stat is writable.
    let result = unsafe {
        libc::fstatat(
            dir.as_raw_fd(),
            name_c.as_ptr(),
            &mut stat,
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if result != 0 {
        let error = std::io::Error::last_os_error();
        if error.kind() == std::io::ErrorKind::NotFound {
            return Ok(None);
        }
        return Err(FlowError::progress(format!(
            "cannot stat {name:?}: {error}"
        )));
    }
    let mode = stat.st_mode & libc::S_IFMT;
    let kind = if mode == libc::S_IFLNK {
        EntryKind::Symlink
    } else if mode == libc::S_IFDIR {
        EntryKind::Directory
    } else if mode == libc::S_IFREG {
        EntryKind::File
    } else {
        EntryKind::Other
    };
    Ok(Some(kind))
}

/// Kind of a directory entry, from `lstat` semantics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EntryKind {
    File,
    Directory,
    Symlink,
    Other,
}

/// Unlink an entry relative to an open directory. Removes the link itself; a
/// symlink target is never touched.
pub(crate) fn unlink_at(dir: &File, name: &std::ffi::OsStr) -> Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt as _;

    let name = CString::new(name.as_bytes())
        .map_err(|_| FlowError::progress("path component contains NUL"))?;
    // SAFETY: dir is live and name is NUL-terminated; flags are zero.
    let result = unsafe { libc::unlinkat(dir.as_raw_fd(), name.as_ptr(), 0) };
    if result == 0 {
        return Ok(());
    }
    let error = std::io::Error::last_os_error();
    if error.kind() == std::io::ErrorKind::NotFound {
        return Ok(());
    }
    Err(FlowError::progress(format!(
        "cannot unlink {name:?}: {error}"
    )))
}

/// Open a child directory relative to `parent`, without following a symlink.
pub(crate) fn open_child_dir(parent: &File, name: &std::ffi::OsStr) -> Result<Option<File>> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt as _;

    let name = CString::new(name.as_bytes())
        .map_err(|_| FlowError::progress("path component contains NUL"))?;
    // SAFETY: parent is a live directory fd, name is NUL-terminated.
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        let error = std::io::Error::last_os_error();
        if matches!(
            error.raw_os_error(),
            Some(libc::ENOENT) | Some(libc::ENOTDIR) | Some(libc::ELOOP)
        ) {
            return Ok(None);
        }
        return Err(FlowError::progress(format!(
            "cannot open {:?}: {error}",
            name
        )));
    }
    // SAFETY: successful openat returns a fresh descriptor we now own.
    Ok(Some(unsafe {
        <File as std::os::fd::FromRawFd>::from_raw_fd(fd)
    }))
}

/// Names of the entries in an open directory.
pub(crate) fn directory_entries(dir: &File) -> Result<Vec<std::ffi::OsString>> {
    let alias = format!("/proc/self/fd/{}", dir.as_raw_fd());
    let mut names = Vec::new();
    for entry in std::fs::read_dir(alias)?.flatten() {
        names.push(entry.file_name());
    }
    Ok(names)
}

/// Make an open directory writable by its owner.
pub(crate) fn make_dir_writable(dir: &File) -> Result<()> {
    let metadata = dir.metadata()?;
    let mode = metadata.mode() & 0o7777;
    if mode & 0o700 == 0o700 {
        return Ok(());
    }
    // SAFETY: dir is a live descriptor; fchmod takes scalars.
    if unsafe { libc::fchmod(dir.as_raw_fd(), mode | 0o700) } != 0 {
        return Err(FlowError::progress(format!(
            "cannot chmod open directory: {}",
            std::io::Error::last_os_error()
        )));
    }
    Ok(())
}

/// Remove every entry inside an open directory, then leave the directory
/// itself in place. All operations are relative to pinned descriptors, so a
/// path swapped after verification cannot redirect the deletion.
pub(crate) fn clear_directory(dir: &File, depth: usize) -> Result<()> {
    if depth > 128 {
        return Err(FlowError::progress("directory nesting too deep"));
    }
    make_dir_writable(dir)?;
    for name in directory_entries(dir)? {
        clear_entry(dir, &name, depth)?;
    }
    Ok(())
}

fn clear_entry(dir: &File, name: &std::ffi::OsStr, depth: usize) -> Result<()> {
    match kind_at(dir, name)? {
        None => Ok(()),
        Some(EntryKind::Directory) => clear_subdirectory(dir, name, depth),
        Some(_) => unlink_at(dir, name),
    }
}

fn clear_subdirectory(dir: &File, name: &std::ffi::OsStr, depth: usize) -> Result<()> {
    let Some(child) = open_child_dir(dir, name)? else {
        return Ok(());
    };
    clear_directory(&child, depth + 1)?;
    unlink_at_dir(dir, name)
}

/// `unlinkat(..., AT_REMOVEDIR)` relative to an open directory.
pub(crate) fn unlink_at_dir(dir: &File, name: &std::ffi::OsStr) -> Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt as _;

    let name = CString::new(name.as_bytes())
        .map_err(|_| FlowError::progress("path component contains NUL"))?;
    // SAFETY: dir is live, name is NUL-terminated, AT_REMOVEDIR is scalar.
    let result = unsafe { libc::unlinkat(dir.as_raw_fd(), name.as_ptr(), libc::AT_REMOVEDIR) };
    if result == 0 {
        return Ok(());
    }
    let error = std::io::Error::last_os_error();
    if matches!(
        error.raw_os_error(),
        Some(libc::ENOENT) | Some(libc::ENOTEMPTY)
    ) {
        return Ok(());
    }
    Err(FlowError::progress(format!(
        "cannot remove directory {name:?}: {error}"
    )))
}
