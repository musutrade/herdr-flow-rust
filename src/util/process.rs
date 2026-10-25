//! Local process boundaries: bounded command execution, background startup
//! readiness, `/proc` identity, Git checkout fingerprinting and shell quoting.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};

use crate::error::{ErrorKind, FlowError, Result};
use crate::model::{self, J};
use crate::util::fs::{read_bytes, resolve_path};
use crate::util::pyjson::to_hex;

mod capture;

#[derive(Clone, Debug)]
pub struct CommandOptions {
    pub cwd: Option<PathBuf>,
    pub timeout: Duration,
    pub env: BTreeMap<String, String>,
    pub clear_env: bool,
    /// Remove inherited GIT_* variables before applying explicit env entries.
    pub remove_git_env: bool,
    /// Optional child-only creation mask; does not alter the supervisor.
    pub creation_mask: Option<u32>,
    /// Kill the direct child if its spawning supervisor dies (Linux).
    pub kill_on_parent_death: bool,
    pub stdin: Option<Vec<u8>>,
    /// Execute this already-open file instead of resolving `argv[0]` again.
    pub executable_file: Option<std::sync::Arc<File>>,
    /// Keep these files open in the child for descriptor-path script arguments.
    pub inherited_files: Vec<std::sync::Arc<File>>,
    /// Maximum captured bytes per stream. Exceeding either limit fails the command.
    pub stdout_limit: usize,
    pub stderr_limit: usize,
}

impl Default for CommandOptions {
    fn default() -> Self {
        Self {
            cwd: None,
            timeout: Duration::from_secs(30),
            env: BTreeMap::new(),
            clear_env: false,
            remove_git_env: false,
            creation_mask: None,
            kill_on_parent_death: false,
            stdin: None,
            executable_file: None,
            inherited_files: Vec::new(),
            stdout_limit: 16 * 1024 * 1024,
            stderr_limit: 16 * 1024 * 1024,
        }
    }
}

#[derive(Clone, Debug)]
pub struct CommandOutput {
    pub status: i32,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

impl CommandOutput {
    pub fn success(&self) -> bool {
        self.status == 0
    }

    pub fn stdout_text(&self) -> String {
        String::from_utf8_lossy(&self.stdout).to_string()
    }

    pub fn stderr_text(&self) -> String {
        String::from_utf8_lossy(&self.stderr).to_string()
    }
}

/// Run a command with captured stdout/stderr and a hard timeout.
///
/// Output is bounded during collection (16 MiB per stream by default). An
/// exceeded budget returns `OutputLimitExceeded`, without partial output, and
/// kills the owned process group using the same cleanup as a timeout.
/// A timeout is reported as `TimeoutExpired`. A non-zero exit is returned to
/// the caller as data; policy about failures belongs to each boundary.
///
/// This Linux implementation owns the spawned child and its new process group.
/// The embedding application must not reap this child independently or configure
/// SIGCHLD auto-reaping: cleanup reserves its PID using waitid(WNOWAIT) until
/// group termination. Descendants that leave this group require a separate
/// task-environment cleanup boundary. This function acquires no application
/// state lock and does not make an invocation idempotent; an error after spawn
/// can follow external side effects, so callers must reconcile before retrying.
/// Empty argv is rejected before spawn. An unrepresentable timeout can panic
/// when adding it to Instant; callers should supply bounded operational limits.
pub fn run_command(argv: &[String], options: &CommandOptions) -> Result<CommandOutput> {
    require_argv(argv)?;
    let deadline = Instant::now() + options.timeout;
    let mut command = build_command(argv, options);
    let child = spawn_child(&mut command, &argv[0])?;
    capture::run(child, options, &argv[0], deadline)
}

fn require_argv(argv: &[String]) -> Result<()> {
    if argv.is_empty() {
        return Err(FlowError::new(
            ErrorKind::Usage,
            "command must not be empty",
        ));
    }
    Ok(())
}

fn build_command(argv: &[String], options: &CommandOptions) -> Command {
    let mut command = executable_command(&argv[0], options);
    command.args(&argv[1..]);
    apply_cwd(&mut command, options.cwd.as_deref());
    apply_env(&mut command, options);
    apply_stdin(&mut command, options.stdin.is_some());
    child_creation_policy(&mut command, options);
    command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    command
}

fn child_creation_policy(command: &mut Command, options: &CommandOptions) {
    if let Some(mask) = options.creation_mask {
        // SAFETY: umask is a scalar syscall in the forked child only.
        unsafe {
            command.pre_exec(move || {
                libc::umask(mask);
                Ok(())
            });
        }
    }
    if options.kill_on_parent_death {
        let parent = std::process::id() as i32;
        // SAFETY: child-only scalar syscalls; capture parent before fork and
        // reject death/reparenting that raced setting PDEATHSIG.
        unsafe {
            command.pre_exec(move || {
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL, 0, 0, 0) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::getppid() != parent {
                    return Err(std::io::Error::from_raw_os_error(libc::ECHILD));
                }
                Ok(())
            });
        }
    }
}

fn executable_command(program: &str, options: &CommandOptions) -> Command {
    let Some(file) = &options.executable_file else {
        return Command::new(program);
    };
    let fd = file.as_raw_fd();
    let descriptors: Vec<_> = std::iter::once(fd)
        .chain(options.inherited_files.iter().map(|file| file.as_raw_fd()))
        .collect();
    let mut command = Command::new(format!("/proc/self/fd/{fd}"));
    command.arg0(program);
    // SAFETY: the child only performs fcntl on descriptors kept alive by
    // options through spawn. Keeping it across exec also supports shebang
    // interpreters reopening /proc/self/fd/N; the parent retains CLOEXEC.
    unsafe {
        command.pre_exec(move || {
            for descriptor in &descriptors {
                if libc::fcntl(*descriptor, libc::F_SETFD, 0) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
            }
            Ok(())
        });
    }
    command
}

fn apply_cwd(command: &mut Command, cwd: Option<&Path>) {
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }
}

fn apply_env(command: &mut Command, options: &CommandOptions) {
    if options.clear_env {
        command.env_clear();
    }
    if options.remove_git_env {
        remove_git_env(command);
    }
    apply_env_vars(command, &options.env);
}

fn apply_env_vars(command: &mut Command, vars: &BTreeMap<String, String>) {
    for (key, value) in vars {
        command.env(key, value);
    }
}

fn apply_stdin(command: &mut Command, has_stdin: bool) {
    command.stdin(if has_stdin {
        Stdio::piped()
    } else {
        Stdio::null()
    });
}

fn spawn_child(command: &mut Command, program: &str) -> Result<Child> {
    command
        .spawn()
        .map_err(|error| FlowError::new(ErrorKind::Io, format!("cannot start {program}: {error}")))
}

fn remove_git_env(command: &mut Command) {
    for (key, _) in std::env::vars_os() {
        if key.as_bytes().starts_with(b"GIT_") {
            command.env_remove(key);
        }
    }
}

/// A shared environment boundary for Git provenance, fingerprints and batch
/// operations. Explicit batch index/identity entries can be added afterwards.
pub fn git_environment() -> BTreeMap<String, String> {
    [
        ("GIT_CONFIG_NOSYSTEM", "1"),
        ("GIT_CONFIG_GLOBAL", "/dev/null"),
        ("GIT_TERMINAL_PROMPT", "0"),
        ("GIT_CONFIG_COUNT", "2"),
        ("GIT_CONFIG_KEY_0", "core.hooksPath"),
        ("GIT_CONFIG_VALUE_0", "/dev/null"),
        ("GIT_CONFIG_KEY_1", "core.fsmonitor"),
        ("GIT_CONFIG_VALUE_1", "false"),
    ]
    .into_iter()
    .map(|(key, value)| (key.to_string(), value.to_string()))
    .collect()
}

pub fn git_options(timeout: Duration) -> CommandOptions {
    CommandOptions {
        timeout,
        env: git_environment(),
        remove_git_env: true,
        ..Default::default()
    }
}

/// The original `context_guard.command`: non-zero exit raises, stderr is
/// collapsed to one credential-neutral line.
pub fn command(
    argv: &[String],
    cwd: Option<&Path>,
    timeout: Duration,
    transient: bool,
) -> Result<Vec<u8>> {
    if argv.is_empty() {
        return Err(FlowError::guard("command must not be empty"));
    }
    let options = CommandOptions {
        cwd: cwd.map(Path::to_path_buf),
        timeout,
        ..Default::default()
    };
    checked_command(argv, &options, transient)
}

fn checked_command(argv: &[String], options: &CommandOptions, transient: bool) -> Result<Vec<u8>> {
    let output = match run_command(argv, options) {
        Ok(output) => output,
        Err(error) if error.kind == ErrorKind::Timeout => {
            return Err(FlowError::new(
                ErrorKind::Timeout,
                format!("{} timed out", argv[0]),
            ))
        }
        Err(error) => {
            return Err(FlowError::guard(format!(
                "{} is not available: {}",
                argv[0], error
            )))
        }
    };
    if output.status != 0 {
        let mut message = format!(
            "{} failed (exit {})",
            argv.iter().take(2).cloned().collect::<Vec<_>>().join(" "),
            output.status
        );
        let detail = error_detail(&output.stderr, STDERR_LIMIT);
        if !detail.is_empty() {
            message.push_str(": ");
            message.push_str(&detail);
        }
        return Err(FlowError::new(
            if transient {
                ErrorKind::Transient
            } else {
                ErrorKind::Guard
            },
            message,
        ));
    }
    Ok(output.stdout)
}

pub const STDERR_LIMIT: usize = 2048;

/// Collapse bounded stderr into one printable, credential-neutral line.
pub fn error_detail(data: &[u8], limit: usize) -> String {
    if data.is_empty() {
        return String::new();
    }
    let text = String::from_utf8_lossy(data);
    let text: String = text
        .chars()
        .map(|character| {
            if character == '\n' || character == '\t' || (character as u32) >= 32 {
                character
            } else {
                ' '
            }
        })
        .collect();
    // Redact URL credentials, local user names and long opaque tokens.
    static URL: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    static HOME: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    static TOKEN: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let url = URL.get_or_init(|| regex::Regex::new(r"(https?://)[^@\s/]+@").expect("url regex"));
    let home =
        HOME.get_or_init(|| regex::Regex::new(r"(/home/|/Users/)[^/\s]+").expect("home regex"));
    let token =
        TOKEN.get_or_init(|| regex::Regex::new(r"\b[A-Za-z0-9_-]{32,}\b").expect("token regex"));
    let text = url.replace_all(&text, "$1[REDACTED]@").into_owned();
    let text = home.replace_all(&text, "$1[USER]").into_owned();
    let text = token.replace_all(&text, "[TOKEN]").into_owned();
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() <= limit {
        collapsed
    } else {
        let mut truncated: String = collapsed.chars().take(limit.saturating_sub(3)).collect();
        truncated.push_str("...");
        truncated
    }
}

/// The original `batch_git._sanitize_stderr`: first nonempty line, max 200.
pub fn sanitize_stderr(raw: &[u8]) -> String {
    if raw.is_empty() {
        return "no error output".to_string();
    }
    let text = String::from_utf8_lossy(raw);
    let Some(line) = text.lines().map(str::trim).find(|line| !line.is_empty()) else {
        return "no error output".to_string();
    };
    error_detail(line.as_bytes(), 200)
}

pub fn shell_quote(value: &str) -> String {
    if !value.is_empty()
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(
                    byte,
                    b'@' | b'%' | b'+' | b'=' | b':' | b',' | b'.' | b'/' | b'_' | b'-'
                )
        })
    {
        return value.to_string();
    }
    format!("'{}'", value.replace('\'', "'\\''"))
}

pub fn shell_join(argv: &[String]) -> String {
    argv.iter()
        .map(|argument| shell_quote(argument))
        .collect::<Vec<_>>()
        .join(" ")
}

pub fn check_platform() -> J {
    let system = std::env::consts::OS;
    if cfg!(target_os = "linux") {
        serde_json::json!({"supported": true, "system": "Linux"})
    } else {
        serde_json::json!({
            "supported": false,
            "system": system,
            "reason": format!("{system} is not supported; Linux-only features: /proc filesystem, Unix domain sockets, flock")
        })
    }
}

pub fn process_start_ticks(pid: u32) -> Option<String> {
    let text = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let rest = text.rsplit_once(')')?.1;
    rest.split_whitespace().nth(19).map(ToString::to_string)
}

pub fn process_identity(pid: u32) -> Result<J> {
    let platform = check_platform();
    if !model::bool_or(&platform, "supported", false) {
        return Err(FlowError::guard(model::string_or(
            &platform,
            "reason",
            "platform unsupported",
        )));
    }
    let Some(start_ticks) = process_start_ticks(pid) else {
        return Err(FlowError::guard("cannot establish process identity"));
    };
    Ok(serde_json::json!({"pid": pid, "start_ticks": start_ticks}))
}

pub fn current_uid() -> u32 {
    // SAFETY: geteuid takes no pointers and has no caller preconditions.
    unsafe { libc::geteuid() }
}

pub fn git_output_bounded(cwd: &Path, args: &[&str], timeout: Duration) -> Result<Vec<u8>> {
    let mut argv = vec![
        "git".to_string(),
        "-C".to_string(),
        cwd.display().to_string(),
    ];
    argv.extend(args.iter().map(|argument| argument.to_string()));
    checked_command(&argv, &git_options(timeout), false)
}

pub fn checkout_root(cwd: &Path) -> PathBuf {
    let result = git_output_bounded(
        cwd,
        &["rev-parse", "--show-toplevel"],
        Duration::from_secs(30),
    );
    match result {
        Ok(output) => resolve_path(Path::new(String::from_utf8_lossy(&output).trim())),
        Err(_) => resolve_path(cwd),
    }
}

pub fn inside_checkout(path: &Path, cwd: &Path) -> bool {
    let path = resolve_path(path);
    path.starts_with(resolve_path(cwd)) || path.starts_with(checkout_root(cwd))
}

/// Bind branch, HEAD, index/worktree diff and untracked bytes; never stage.
pub fn fingerprint(cwd: &Path) -> Result<String> {
    let repo_root = repository_root(cwd)?;
    ensure_no_submodules(&repo_root)?;
    let mut hasher = Sha256::new();
    hasher.update(b"herdr-checkout-fingerprint-v2\0");
    hash_tracked_state(&repo_root, &mut hasher)?;
    hash_untracked(&repo_root, &mut hasher)?;
    Ok(to_hex(&hasher.finalize()))
}

fn repository_root(cwd: &Path) -> Result<PathBuf> {
    let output = git_output_bounded(
        cwd,
        &["rev-parse", "--show-toplevel"],
        Duration::from_secs(30),
    )?;
    let text = String::from_utf8_lossy(&output);
    Ok(PathBuf::from(text.trim()))
}

fn git_in(repo_root: &Path, args: &[&str]) -> Result<Vec<u8>> {
    git_output_bounded(repo_root, args, Duration::from_secs(120))
}

fn ensure_no_submodules(repo_root: &Path) -> Result<()> {
    let staged = git_in(repo_root, &["ls-files", "--stage", "-z"])?;
    reject_if_submodule(&staged)
}

fn reject_if_submodule(staged: &[u8]) -> Result<()> {
    if staged
        .split(|byte| *byte == 0)
        .any(|entry| entry.starts_with(b"160000 "))
    {
        return Err(FlowError::guard(
            "submodule checkout requires an explicit recursive binding adapter",
        ));
    }
    Ok(())
}

fn hash_tracked_state(repo_root: &Path, hasher: &mut Sha256) -> Result<()> {
    let head = git_in(repo_root, &["rev-parse", "HEAD"])?;
    let branch = branch_binding(repo_root)?;
    let worktree_diff = git_in(
        repo_root,
        &["diff", "--no-ext-diff", "--no-textconv", "--binary", "HEAD"],
    )?;
    let index_diff = git_in(
        repo_root,
        &[
            "diff",
            "--no-ext-diff",
            "--no-textconv",
            "--cached",
            "--binary",
        ],
    )?;
    for data in [
        head.as_slice(),
        branch.as_slice(),
        worktree_diff.as_slice(),
        index_diff.as_slice(),
    ] {
        hash_length_prefixed(hasher, data);
    }
    Ok(())
}

fn branch_binding(repo_root: &Path) -> Result<Vec<u8>> {
    let abbrev = git_in(repo_root, &["rev-parse", "--abbrev-ref", "HEAD"])?;
    symbolic_branch(repo_root, &abbrev)
}

fn symbolic_branch(repo_root: &Path, abbrev: &[u8]) -> Result<Vec<u8>> {
    if String::from_utf8_lossy(abbrev).trim() == "HEAD" {
        return Ok(b"DETACHED".to_vec());
    }
    git_in(repo_root, &["symbolic-ref", "-q", "HEAD"])
}

fn hash_length_prefixed(hasher: &mut Sha256, data: &[u8]) {
    hasher.update((data.len() as u64).to_be_bytes());
    hasher.update(data);
}

fn hash_untracked(repo_root: &Path, hasher: &mut Sha256) -> Result<()> {
    let untracked = git_in(
        repo_root,
        &["ls-files", "--others", "--exclude-standard", "-z"],
    )?;
    let mut names = untracked_names(&untracked);
    names.sort();
    for name in names {
        hash_untracked_entry(repo_root, name, hasher)?;
    }
    Ok(())
}

fn untracked_names(untracked: &[u8]) -> Vec<&[u8]> {
    untracked
        .split(|byte| *byte == 0)
        .filter(|name| !name.is_empty())
        .collect()
}

fn hash_untracked_entry(repo_root: &Path, name: &[u8], hasher: &mut Sha256) -> Result<()> {
    hash_length_prefixed(hasher, name);
    let path = repo_root.join(std::ffi::OsStr::from_bytes(name));
    let metadata = fs::symlink_metadata(&path)
        .map_err(|error| FlowError::guard(format!("cannot inspect untracked path: {error}")))?;
    if metadata.file_type().is_symlink() {
        return hash_symlink(&path, hasher);
    }
    if metadata.is_file() {
        return hash_regular_file(&path, &metadata, hasher);
    }
    Err(FlowError::guard(
        "unsupported untracked path; cannot bind checkout",
    ))
}

fn hash_symlink(path: &Path, hasher: &mut Sha256) -> Result<()> {
    use std::os::unix::ffi::OsStrExt;
    hasher.update(b"link:");
    let target = fs::read_link(path)?;
    hash_length_prefixed(hasher, target.as_os_str().as_bytes());
    Ok(())
}

fn hash_regular_file(path: &Path, metadata: &fs::Metadata, hasher: &mut Sha256) -> Result<()> {
    hasher.update(b"file:");
    hasher.update(metadata.mode().to_be_bytes());
    let mut file = File::open(path)?;
    let mut content = Sha256::new();
    let mut buffer = vec![0u8; 1024 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        content.update(&buffer[..read]);
    }
    hasher.update(content.finalize());
    Ok(())
}

pub fn terminate_child(child: &mut Child) {
    if child.try_wait().ok().flatten().is_none() {
        let _ = child.kill();
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        match child.try_wait() {
            Ok(Some(_)) => return,
            Ok(None) => thread::sleep(Duration::from_millis(20)),
            Err(_) => return,
        }
    }
    let _ = child.kill();
    let _ = child.wait();
}

fn set_cloexec(fd: RawFd, enabled: bool) -> std::io::Result<()> {
    // SAFETY: fd is borrowed; F_GETFD takes no pointer and transfers no owner.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let updated = if enabled {
        flags | libc::FD_CLOEXEC
    } else {
        flags & !libc::FD_CLOEXEC
    };
    // SAFETY: F_SETFD takes only scalar flags. The caller keeps fd open; no
    // allocation or locks occur, including when called after fork.
    if unsafe { libc::fcntl(fd, libc::F_SETFD, updated) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

pub struct BackgroundChild {
    pub child: Child,
    pub pid: u32,
    pub ready: Option<J>,
}

impl BackgroundChild {
    pub fn reap(mut self) {
        thread::spawn(move || {
            let _ = self.child.wait();
        });
    }
}

/// Spawn a detached helper that writes one readiness JSON object to fd 3.
///
/// Linux: creates a new session, appends stdout/stderr to `log_path`, and
/// passes `--ready-fd 3` to the helper. The caller must supply a compatible
/// helper and an operator-controlled log path; this is not a shell interface.
/// Empty argv returns `Usage` before creating logs or pipes. Other errors may
/// leave diagnostic log files. A readiness error terminates and waits for the
/// direct child; it does not attest to cleanup of arbitrary descendants.
/// On success the caller owns the child and must arrange reaping (for example
/// with [`BackgroundChild::reap`]); readiness JSON is a protocol response, not
/// independent proof that the helper is healthy. This operation is not idempotent.
pub fn spawn_with_ready(
    argv: &[String],
    log_path: &Path,
    cwd: Option<&Path>,
    timeout: Duration,
) -> Result<BackgroundChild> {
    require_argv(argv)?;
    ensure_log_parent(log_path)?;
    let (log, log_err) = open_log_pair(log_path)?;
    let (read_fd, write_fd) = readiness_pipe()?;
    let mut command = ready_command(argv, log, log_err, cwd, write_fd.as_raw_fd());
    let mut child = command
        .spawn()
        .map_err(|error| FlowError::io(format!("cannot start background helper: {error}")))?;
    drop(write_fd);
    let ready = read_ready(read_fd.as_raw_fd(), timeout);
    drop(read_fd);
    let ready = ready.inspect_err(|_| {
        terminate_child(&mut child);
    })?;
    let pid = child.id();
    Ok(BackgroundChild {
        child,
        pid,
        ready: Some(ready),
    })
}

fn ensure_log_parent(log_path: &Path) -> Result<()> {
    if let Some(parent) = log_path.parent() {
        crate::util::fs::ensure_private_dir(parent, true)?;
    }
    Ok(())
}

fn open_log_pair(log_path: &Path) -> Result<(fs::File, fs::File)> {
    let log = open_log_file(log_path)?;
    let clone = log.try_clone()?;
    Ok((log, clone))
}

fn open_log_file(log_path: &Path) -> Result<fs::File> {
    OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(log_path)
        .map_err(|error| FlowError::io(format!("cannot open log: {error}")))
}

fn readiness_pipe() -> Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0 as RawFd; 2];
    // SAFETY: fds provides two writable c_int slots for the entire call. Only a
    // successful pipe2 call transfers two distinct descriptors to us. CLOEXEC
    // is atomic with creation, so concurrent exec cannot inherit a pipe end.
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return Err(FlowError::io(format!(
            "cannot create readiness pipe: {}",
            std::io::Error::last_os_error()
        )));
    }
    // SAFETY: successful pipe2 returned two fresh owned descriptors. Each is
    // transferred exactly once; OwnedFd closes both on every later error path.
    Ok(unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) })
}

fn ready_command(
    argv: &[String],
    log: fs::File,
    log_err: fs::File,
    cwd: Option<&Path>,
    write_fd: RawFd,
) -> Command {
    let mut command = Command::new(&argv[0]);
    command
        .args(&argv[1..])
        .arg("--ready-fd")
        .arg("3")
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log_err));
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }
    // SAFETY: after fork the closure uses only async-signal-safe dup2, fcntl
    // (via set_cloexec), setsid and errno access; it does not allocate or lock.
    // write_fd stays open through spawn; fd 3 is the child's readiness writer.
    unsafe {
        command.pre_exec(move || {
            if libc::dup2(write_fd, 3) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            set_cloexec(3, false)?;
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    command
}

fn read_ready(fd: RawFd, timeout: Duration) -> Result<J> {
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(FlowError::watch(
                "helper startup timed out; inspect its log",
            ));
        }
        let millis = remaining.as_millis().min(i32::MAX as u128) as i32;
        let mut poll_fd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: poll_fd is one live writable pollfd; fd is borrowed from the
        // caller and remains open until read_ready returns.
        let result = unsafe { libc::poll(&mut poll_fd, 1, millis) };
        if result < 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(FlowError::io(format!("readiness poll failed: {error}")));
        }
        if result == 0 {
            return Err(FlowError::watch(
                "helper startup timed out; inspect its log",
            ));
        }
        let mut buffer = [0u8; 4096];
        // SAFETY: buffer is writable for exactly buffer.len() bytes and remains
        // live during read. The caller retains ownership of fd.
        let read = unsafe { libc::read(fd, buffer.as_mut_ptr().cast(), buffer.len()) };
        if read <= 0 {
            return Err(FlowError::watch(
                "helper exited before readiness; inspect its log",
            ));
        }
        let data = &buffer[..read as usize];
        return serde_json::from_slice(data)
            .map_err(|_| FlowError::watch("helper exited before readiness; inspect its log"));
    }
}

/// Spawn a detached process without a readiness handshake (advisor workers,
/// queue command wrappers).
///
/// Unix-only: creates a new session and appends both output streams to the
/// operator-controlled log path. Empty argv is rejected before filesystem
/// changes. Success transfers child ownership to the caller, which must handle
/// termination and reaping; dropping `Child` does neither. Spawn success does
/// not prove application readiness. Repeating a successful call starts another
/// process, and errors may leave diagnostic log files.
pub fn spawn_detached(argv: &[String], log_path: &Path, cwd: Option<&Path>) -> Result<Child> {
    require_argv(argv)?;
    if let Some(parent) = log_path.parent() {
        crate::util::fs::ensure_private_dir(parent, true)?;
    }
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(log_path)
        .map_err(|error| FlowError::io(format!("cannot open log: {error}")))?;
    let log_err = log.try_clone()?;
    let mut command = Command::new(&argv[0]);
    command
        .args(&argv[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log_err));
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }
    // SAFETY: this post-fork closure calls only async-signal-safe setsid and
    // reads errno on failure; it captures no allocation or synchronization.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    command
        .spawn()
        .map_err(|error| FlowError::io(format!("cannot start background process: {error}")))
}

/// Spawn the native TUI with inherited terminal I/O.
///
/// Empty argv returns `Usage`. The process inherits the caller's environment,
/// terminal and process-group context; this helper does not isolate it or wait
/// for readiness. The caller owns the returned child and is responsible for
/// termination/reaping. Repeating the call starts another process.
pub fn spawn_inherit(argv: &[String], cwd: Option<&Path>) -> Result<Child> {
    require_argv(argv)?;
    let mut command = Command::new(&argv[0]);
    command.args(&argv[1..]);
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }
    command
        .spawn()
        .map_err(|error| FlowError::io(format!("cannot launch native TUI: {error}")))
}

pub fn current_exe() -> Result<PathBuf> {
    std::env::current_exe()
        .map_err(|error| FlowError::io(format!("cannot locate current executable: {error}")))
}

/// Open a real directory without following a leaf symlink, with CLOEXEC.
///
/// Success transfers one owned raw descriptor to the caller. Convert it exactly
/// once into an owning File/OwnedFd or close it once; borrowing the number does
/// not transfer ownership. The descriptor is not inherited across exec.
/// This checks directory type and the leaf symlink only; callers enforcing a
/// trusted state path must separately validate ownership and ancestor policy.
/// Errors return no owned descriptor and do not create filesystem entries.
pub fn open_dir_fd(path: &Path) -> Result<RawFd> {
    use std::os::unix::ffi::OsStrExt;
    let c_path = std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|_| FlowError::io("path contains NUL"))?;
    // SAFETY: c_path is NUL-terminated and lives through open. O_CREAT is absent,
    // so no variadic mode argument is needed. Success returns a newly owned fd.
    let fd = unsafe {
        libc::open(
            c_path.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(FlowError::io(format!(
            "cannot open state directory: {}",
            std::io::Error::last_os_error()
        )));
    }
    Ok(fd)
}

pub fn path_metadata_mode(path: &Path) -> Option<u32> {
    fs::symlink_metadata(path)
        .ok()
        .map(|metadata| metadata.mode())
}

pub fn is_regular_uid(path: &Path) -> bool {
    fs::symlink_metadata(path)
        .map(|metadata| metadata.file_type().is_file() && metadata.uid() == current_uid())
        .unwrap_or(false)
}

pub fn is_socket_uid(path: &Path) -> bool {
    fs::symlink_metadata(path)
        .map(|metadata| metadata.file_type().is_socket() && metadata.uid() == current_uid())
        .unwrap_or(false)
}

pub fn read_dir_names(path: &Path) -> Vec<String> {
    fs::read_dir(path)
        .map(|entries| {
            entries
                .flatten()
                .filter_map(|entry| entry.file_name().into_string().ok())
                .collect()
        })
        .unwrap_or_default()
}

pub fn write_stdout(value: &str) {
    let mut stdout = std::io::stdout();
    let _ = stdout.write_all(value.as_bytes());
    let _ = stdout.write_all(b"\n");
    let _ = stdout.flush();
}

pub fn write_stderr(value: &str) {
    let mut stderr = std::io::stderr();
    let _ = stderr.write_all(value.as_bytes());
    let _ = stderr.flush();
}

pub fn read_json_stdin() -> Result<J> {
    let mut text = String::new();
    std::io::stdin().read_to_string(&mut text)?;
    serde_json::from_str(&text).map_err(Into::into)
}

pub fn ensure_existing_file(path: &Path) -> Result<()> {
    let metadata = fs::metadata(path)
        .map_err(|error| FlowError::io(format!("evidence file unavailable: {error}")))?;
    if !metadata.is_file() {
        return Err(FlowError::io("evidence path must be a regular file"));
    }
    Ok(())
}

pub fn read_file_identity(path: &Path) -> Result<(u64, u64)> {
    let metadata = fs::metadata(path)?;
    Ok((metadata.dev(), metadata.ino()))
}

pub fn file_is_world_writable(path: &Path) -> bool {
    fs::metadata(path)
        .map(|metadata| metadata.mode() & 0o002 != 0)
        .unwrap_or(false)
}

pub fn read_bytes_limited(path: &Path, limit: usize) -> Result<Vec<u8>> {
    read_bytes(path).map(|mut data| {
        if data.len() > limit {
            data.truncate(limit);
        }
        data
    })
}

/// Write one JSON readiness message to an inherited fd and close it.
///
/// The caller transfers a dedicated writable descriptor, with no remaining
/// File/OwnedFd owner or concurrent user. Existing callers take it from an
/// Option exactly once. This raw API does not validate descriptor provenance.
/// It is best-effort: write failure stops transmission and is not returned;
/// callers must not treat return as acknowledged readiness. The receiver may
/// reject partial/oversized data. Writes can block if the receiver is not
/// draining, so use this only for the bounded startup handshake. Do not pass
/// an arbitrary application descriptor or invoke it again on the same number.
pub fn write_ready_fd(fd: RawFd, value: &J) {
    let mut data = match serde_json::to_vec(value) {
        Ok(data) => data,
        Err(_) => return,
    };
    data.push(b'\n');
    let mut written = 0;
    while written < data.len() {
        // SAFETY: the remaining slice is readable for the supplied length.
        // fd is the inherited writer consumed by this function, not a File fd.
        let result =
            unsafe { libc::write(fd, data[written..].as_ptr().cast(), data.len() - written) };
        if result <= 0 {
            break;
        }
        written += result as usize;
    }
    // SAFETY: this consumes the caller-supplied readiness descriptor once, after
    // the final write; no Rust owner subsequently closes or uses it here.
    unsafe {
        libc::close(fd);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn pipe() -> (RawFd, RawFd) {
        let mut fds = [0 as RawFd; 2];
        // SAFETY: this test supplies two writable descriptor slots and returns
        // their ownership only after pipe succeeds.
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        (fds[0], fds[1])
    }

    fn close(fd: RawFd) {
        // SAFETY: test callers transfer one owned raw pipe end to this helper
        // and do not use it afterward.
        unsafe {
            libc::close(fd);
        }
    }

    #[test]
    fn ready_fd_round_trips_and_times_out() {
        let (read_fd, write_fd) = pipe();
        write_ready_fd(write_fd, &serde_json::json!({"ready": true}));
        let value = read_ready(read_fd, Duration::from_secs(1)).unwrap();
        assert_eq!(value["ready"], true);
        close(read_fd);

        let (read_fd, write_fd) = pipe();
        let error = read_ready(read_fd, Duration::from_millis(10)).unwrap_err();
        assert_eq!(error.kind, crate::error::ErrorKind::Watch);
        close(read_fd);
        close(write_fd);
    }

    #[test]
    fn cloexec_flag_can_be_set_and_cleared() {
        let (read_fd, write_fd) = pipe();
        set_cloexec(read_fd, true).unwrap();
        // SAFETY: the test keeps read_fd open; F_GETFD takes no pointer or owner.
        let flags = unsafe { libc::fcntl(read_fd, libc::F_GETFD) };
        assert!(flags & libc::FD_CLOEXEC != 0);
        set_cloexec(read_fd, false).unwrap();
        // SAFETY: read_fd remains owned and open until the closes below.
        let flags = unsafe { libc::fcntl(read_fd, libc::F_GETFD) };
        assert!(flags & libc::FD_CLOEXEC == 0);
        close(read_fd);
        close(write_fd);
    }

    #[test]
    fn readiness_pipe_starts_cloexec_and_invalid_flags_fail() {
        let (reader, writer) = readiness_pipe().unwrap();
        for fd in [reader.as_raw_fd(), writer.as_raw_fd()] {
            // SAFETY: both owners remain live; F_GETFD only reads scalar flags.
            let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
            assert!(flags >= 0 && flags & libc::FD_CLOEXEC != 0);
        }
        assert_eq!(
            set_cloexec(-1, false).unwrap_err().raw_os_error(),
            Some(libc::EBADF)
        );
    }

    #[test]
    fn existing_file_requires_a_regular_file() {
        let directory = crate::util::fs::test_tempdir();
        let file = directory.path().join("evidence");
        fs::write(&file, b"x").unwrap();
        ensure_existing_file(&file).unwrap();
        assert!(ensure_existing_file(directory.path()).is_err());
        assert!(ensure_existing_file(&directory.path().join("missing")).is_err());
    }

    #[test]
    fn terminate_child_stops_a_running_process() {
        let mut child = Command::new("sh").arg("-c").arg("sleep 5").spawn().unwrap();
        terminate_child(&mut child);
        assert!(child.try_wait().unwrap().is_some());
    }

    #[test]
    fn spawn_with_ready_accepts_a_helper_frame() {
        let directory = crate::util::fs::test_tempdir();
        let log = directory.path().join("helper.log");
        let argv = vec![
            "sh".to_string(),
            "-c".to_string(),
            "printf '{\"ready\":true}' >&3".to_string(),
        ];
        let mut child = spawn_with_ready(&argv, &log, None, Duration::from_secs(2)).unwrap();
        assert_eq!(child.ready.as_ref().unwrap()["ready"], true);
        terminate_child(&mut child.child);
    }

    #[test]
    fn fingerprint_tracks_head_worktree_and_untracked_files() {
        let directory = crate::util::fs::test_tempdir();
        let root = directory.path();
        let git = |arguments: &[&str]| {
            let status = Command::new("git")
                .current_dir(root)
                .args(arguments)
                .status()
                .unwrap();
            assert!(status.success(), "git {arguments:?} failed");
        };
        git(&["init", "-q"]);
        git(&["config", "user.email", "test@example.com"]);
        git(&["config", "user.name", "Test"]);
        fs::write(root.join("tracked"), b"one").unwrap();
        git(&["add", "tracked"]);
        git(&["commit", "-q", "-m", "initial"]);
        let first = fingerprint(root).unwrap();
        fs::write(root.join("tracked"), b"two").unwrap();
        let second = fingerprint(root).unwrap();
        assert_ne!(first, second);
        fs::write(root.join("untracked"), b"three").unwrap();
        let third = fingerprint(root).unwrap();
        assert_ne!(second, third);
        git(&["checkout", "-q", "--detach"]);
        let detached = fingerprint(root).unwrap();
        assert_ne!(third, detached);
    }
}
