//! Fallible observations for destructive cleanup, separate from diagnostic APIs.
use std::io;

use super::super::env::ProcessRecord;

pub(crate) fn is_alive(record: &ProcessRecord) -> io::Result<bool> {
    if record.pid <= 1 {
        return Err(io::Error::from_raw_os_error(libc::EINVAL));
    }
    observe(record).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("cannot observe recorded process {}: {error}", record.pid),
        )
    })
}

fn observe(record: &ProcessRecord) -> io::Result<bool> {
    match read_stat(record.pid) {
        Ok(stat) => matches_identity(record, &stat),
        // A hidden/missing proc mount is not proof of exit. Require the kernel
        // to confirm absence; a live pidfd leaves the original error intact.
        Err(error) if missing_proc(&error) => {
            if super::signals::open(record.pid)?.is_none() {
                Ok(false)
            } else {
                Err(error)
            }
        }
        Err(error) => Err(error),
    }
}

// procfs can return ESRCH after opening a stat file whose process exits.
// Neither this error nor ENOENT alone proves absence: callers check pidfd_open.
pub(super) fn missing_proc(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::NotFound || error.raw_os_error() == Some(libc::ESRCH)
}

fn read_stat(pid: i32) -> io::Result<String> {
    #[cfg(test)]
    if let Some(result) = tests::injected_stat(pid) {
        return result;
    }
    std::fs::read_to_string(format!("/proc/{pid}/stat"))
}

fn matches_identity(record: &ProcessRecord, stat: &str) -> io::Result<bool> {
    let (prefix, rest) = stat.rsplit_once(')').ok_or_else(invalid_stat)?;
    let (pid, _) = prefix.split_once(" (").ok_or_else(invalid_stat)?;
    if pid.parse::<i32>().ok() != Some(record.pid) {
        return Err(invalid_stat());
    }
    let mut fields = rest.split_whitespace();
    let state = fields.next().ok_or_else(invalid_stat)?;
    if !matches!(
        state,
        "R" | "S" | "D" | "Z" | "T" | "t" | "X" | "x" | "K" | "W" | "P" | "I"
    ) {
        return Err(invalid_stat());
    }
    let start = fields
        .nth(18)
        .ok_or_else(invalid_stat)?
        .parse::<u64>()
        .map_err(|_| invalid_stat())?;
    Ok(!matches!(state, "Z" | "X" | "x")
        && record.start_time.is_none_or(|expected| start == expected))
}

fn invalid_stat() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "invalid process stat identity")
}

#[cfg(test)]
mod tests {
    // Synthetic stat failures; these do not change host procfs or security policy.
    use super::super::*;
    use std::cell::RefCell;
    use std::io::{self, Read};
    use std::os::fd::AsRawFd;
    use std::process::{Child, Command, Stdio};
    use std::time::{Duration, Instant};

    #[derive(Clone)]
    enum StatFault {
        Read(i32),
        Text(String),
    }

    thread_local! {
        static STAT_FAULT: RefCell<Option<(i32, StatFault)>> = const { RefCell::new(None) };
    }

    pub(super) fn injected_stat(pid: i32) -> Option<io::Result<String>> {
        STAT_FAULT.with(|slot| {
            let slot = slot.borrow();
            let (target, fault) = slot.as_ref()?;
            if pid != *target {
                return None;
            }
            Some(match fault {
                StatFault::Read(errno) => Err(io::Error::from_raw_os_error(*errno)),
                StatFault::Text(text) => Ok(text.clone()),
            })
        })
    }

    struct FaultGuard;

    impl FaultGuard {
        fn set(pid: i32, fault: StatFault) -> Self {
            STAT_FAULT.with(|slot| {
                assert!(slot.borrow().is_none());
                *slot.borrow_mut() = Some((pid, fault));
            });
            Self
        }
    }

    impl Drop for FaultGuard {
        fn drop(&mut self) {
            STAT_FAULT.with(|slot| *slot.borrow_mut() = None);
        }
    }

    struct TestChild(Child);

    impl TestChild {
        fn spawn() -> Self {
            Self(Command::new("sleep").arg("30").spawn().unwrap())
        }

        fn record(&self) -> ProcessRecord {
            let pid = self.0.id() as i32;
            ProcessRecord {
                pid,
                start_time: crate::task_env::env::process_start_time(pid),
            }
        }
    }

    impl Drop for TestChild {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    fn faults() -> Vec<StatFault> {
        vec![
            StatFault::Read(libc::EACCES),
            StatFault::Read(libc::EIO),
            StatFault::Read(libc::ENOENT),
            StatFault::Read(libc::ESRCH),
            StatFault::Text("malformed stat".into()),
        ]
    }

    #[test]
    fn unknown_observation_never_filters_or_signals_a_target() {
        let mut child = TestChild::spawn();
        let record = child.record();
        for fault in faults() {
            let _guard = FaultGuard::set(record.pid, fault);
            let mut targets = vec![record.clone()];
            assert!(retain_live_records(&mut targets).is_err());
            assert_eq!(targets, vec![record.clone()]);
            assert!(live_survivors(&targets).is_err());
            assert!(wait_for_targets(&targets).is_err());
            assert!(terminate_records(&mut targets).is_err());
            assert!(signals::signal(&record, libc::SIGKILL).is_err());
            assert!(child.0.try_wait().unwrap().is_none());
        }
    }

    #[test]
    fn marked_target_observation_errors_block_termination() {
        let instance = format!("observation-test-{}", std::process::id());
        let mut child = TestChild(
            Command::new("sh")
                // Stay in this PID after announcing readiness. A subsequent
                // exec would introduce another /proc environment transition.
                .args(["-c", "printf 'ready\\n'; IFS= read -r stop"])
                .env(crate::task_env::env::MARKER_ENV, &instance)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()
                .unwrap(),
        );
        // spawn() returning does not establish that the executable has reached
        // user code or that its /proc environment is ready for observation.
        let ready = await_ready(&mut child.0);
        let diagnostic = child_diagnostic(&mut child.0, &instance);
        assert!(ready.is_ok(), "readiness: {ready:?}; {diagnostic}");
        assert!(child.0.try_wait().unwrap().is_none(), "{diagnostic}");
        let record = child.record();
        assert!(record.start_time.is_some(), "{diagnostic}");
        assert!(
            crate::task_env::env::process_has_marker(record.pid, &instance),
            "{diagnostic}"
        );
        assert!(ensure_no_marked(&instance).is_err());
        for fault in faults() {
            let _guard = FaultGuard::set(record.pid, fault);
            assert!(ensure_no_marked(&instance).is_err());
            assert!(terminate_marked(&instance).is_err());
            assert!(child.0.try_wait().unwrap().is_none());
        }
        terminate_marked(&instance).unwrap();
        child.0.wait().unwrap();
        ensure_no_marked(&instance).unwrap();
    }

    fn await_ready(child: &mut Child) -> io::Result<()> {
        let stdout = child.stdout.as_mut().unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        for expected in b"ready\n" {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(io::ErrorKind::TimedOut.into());
            }
            let mut fd = libc::pollfd {
                fd: stdout.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: one valid pollfd, and a bounded nonnegative timeout.
            let result =
                unsafe { libc::poll(&mut fd, 1, remaining.as_millis().max(1) as libc::c_int) };
            if result < 0 {
                return Err(io::Error::last_os_error());
            }
            if result == 0 {
                return Err(io::ErrorKind::TimedOut.into());
            }
            let mut byte = [0];
            stdout.read_exact(&mut byte)?;
            if byte[0] != *expected {
                return Err(io::ErrorKind::InvalidData.into());
            }
        }
        Ok(())
    }

    fn child_diagnostic(child: &mut Child, instance: &str) -> String {
        let pid = child.id() as i32;
        let needle = format!("{}={instance}", crate::task_env::env::MARKER_ENV);
        let environment = match std::fs::read(format!("/proc/{pid}/environ")) {
            Ok(bytes) => format!(
                "bytes={}, marker={}",
                bytes.len(),
                bytes
                    .split(|b| *b == 0)
                    .any(|entry| entry == needle.as_bytes())
            ),
            Err(error) => format!("kind={:?}, errno={:?}", error.kind(), error.raw_os_error()),
        };
        format!(
            "pid={pid}, start={:?}, exit={:?}, environ=({environment})",
            crate::task_env::env::process_start_time(pid),
            child.try_wait()
        )
    }

    fn stat(record: &ProcessRecord, state: &str, start: &str) -> String {
        format!(
            "{} (name with ) brackets) {state} {} {start}",
            record.pid,
            vec!["0"; 18].join(" ")
        )
    }

    #[test]
    fn observation_distinguishes_identity_exit_and_invalid_data() {
        let child = TestChild::spawn();
        let record = child.record();
        let start = record.start_time.unwrap().to_string();
        for (state, timestamp, expected) in [
            ("S", start.as_str(), Some(true)),
            ("R", "0", Some(false)),
            ("Z", start.as_str(), Some(false)),
            ("X", start.as_str(), Some(false)),
            ("?", start.as_str(), None),
            ("Sleeping", start.as_str(), None),
            ("S", "invalid", None),
            ("S", "", None),
        ] {
            let _guard =
                FaultGuard::set(record.pid, StatFault::Text(stat(&record, state, timestamp)));
            assert_eq!(observation::is_alive(&record).ok(), expected);
        }
        let missing_identity = ProcessRecord {
            start_time: None,
            ..record.clone()
        };
        assert!(observation::is_alive(&missing_identity).unwrap());
        {
            let _guard = FaultGuard::set(record.pid, StatFault::Text("999 (other) S 0".into()));
            assert!(observation::is_alive(&record).is_err());
        }
        drop(child);
        assert!(!observation::is_alive(&record).unwrap());
        for pid in [-1, 0, 1] {
            assert!(observation::is_alive(&ProcessRecord {
                pid,
                start_time: None
            })
            .is_err());
        }
    }

    fn fixture_worktree(repo: &Path, worktree: &Path) {
        std::fs::create_dir(repo).unwrap();
        for args in [
            vec!["init", "-q"],
            vec![
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.com",
                "commit",
                "--allow-empty",
                "-qm",
                "base",
            ],
        ] {
            assert!(Command::new("git")
                .arg("-C")
                .arg(repo)
                .args(args)
                .status()
                .unwrap()
                .success());
        }
        assert!(Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["worktree", "add", "--detach", "-q"])
            .arg(worktree)
            .status()
            .unwrap()
            .success());
    }

    fn assert_retained(env: &TaskEnvironment, file: &Path, worktree: &Path) {
        let plan = load_plan(&env.task_id).unwrap();
        assert_eq!(plan.pending_processes, env.child_processes);
        assert_eq!(plan.pending_files, vec![file.to_path_buf()]);
        assert_eq!(plan.pending_worktrees, env.worktree_registrations);
        assert_eq!(plan.pending_root_removals.len(), 2);
        assert!(plan
            .last_error
            .unwrap()
            .contains("cannot observe recorded process"));
        assert_eq!(std::fs::read_to_string(file).unwrap(), "payload");
        assert!(worktree.join(".git").is_file());
        assert!(env.workspace_root.is_dir());
        assert!(env.temp_root.is_dir());
        assert!(env.state_file.exists());
        assert_eq!(registry::occupied_slots().unwrap(), 2);
    }

    fn observation_failure_preserves_cleanup(descendant: bool, fault: StatFault) {
        let temp = crate::util::fs::test_tempdir();
        let _registry = crate::task_env::test_env_root(&temp.path().join("registry"));
        let mut env = crate::task_env::create_environment(
            "observation-error",
            &temp.path().join("work"),
            vec![],
            &temp.path().join("tmp"),
        )
        .unwrap();
        let repo = temp.path().join("repo");
        let worktree = env.workspace_root.join("checkout");
        fixture_worktree(&repo, &worktree);
        env.register_worktree(&worktree).unwrap();
        let file = worktree.join("payload");
        std::fs::write(&file, "payload").unwrap();
        env.register_file(&file).unwrap();
        let child = TestChild::spawn();
        let record = child.record();
        // Explicit synthetic registration: no marker or signalling authority is
        // granted. Only this test-owned child supplies the known live identity.
        env.child_processes.push(RegisteredProcess {
            pid: if descendant { i32::MAX } else { record.pid },
            pgid: i32::MAX,
            start_time: record.start_time,
            descendants: if descendant {
                vec![record.clone()]
            } else {
                vec![]
            },
            direct_children: vec![],
        });
        env.persist().unwrap();
        {
            let _guard = FaultGuard::set(record.pid, fault);
            assert!(quiesce_environment(&mut env)
                .unwrap_err()
                .to_string()
                .contains("cannot observe recorded process"));
            assert!(destroy(&env).is_err());
            assert_retained(&env, &file, &worktree);
            assert!(cleanup_failed_environment(&env.task_id).is_err());
            assert_retained(&env, &file, &worktree);
        }
        env.release_ownership();
        drop(child);
        cleanup_failed_environment(&env.task_id).unwrap();
        assert!(!env.workspace_root.exists());
        assert!(!env.temp_root.exists());
        assert!(!registry::cleanup_state_path(&env.task_id).unwrap().exists());
        assert_eq!(registry::occupied_slots().unwrap(), 0);
        assert!(repo.is_dir());
    }

    #[test]
    fn parent_observation_failure_preserves_cleanup_and_retry() {
        for fault in faults() {
            observation_failure_preserves_cleanup(false, fault);
        }
    }

    #[test]
    fn descendant_observation_failure_preserves_cleanup_and_retry() {
        for fault in faults() {
            observation_failure_preserves_cleanup(true, fault);
        }
    }
}
