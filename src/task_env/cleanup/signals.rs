//! Identity-checked signalling through a kernel process handle.
use std::fs::File;
use std::os::fd::{AsRawFd, FromRawFd};

use crate::task_env::env::{process_record_is_alive, ProcessRecord};

pub(super) fn signal(record: &ProcessRecord, signal: i32) -> std::io::Result<()> {
    if record.pid <= 1 || record.start_time.is_none() {
        return Err(std::io::Error::from_raw_os_error(libc::EINVAL));
    }
    let Some(handle) = open(record.pid)? else {
        return Ok(());
    };
    // Check after opening: a PID replaced before pidfd_open must not acquire
    // authority from an old record. Replacement after this check cannot retarget
    // the handle. /proc timestamps have kernel clock-tick precision, not a
    // cryptographic identity; discovery still needs valid ownership evidence.
    if !process_record_is_alive(record) {
        return Ok(());
    }
    send(&handle, signal)
}

fn open(pid: i32) -> std::io::Result<Option<File>> {
    // SAFETY: pidfd_open takes a positive process ID and scalar flags; success
    // returns a fresh CLOEXEC descriptor, owned exactly once below.
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0u32) };
    if fd < 0 {
        return absent_or_error(std::io::Error::last_os_error()).map(|()| None);
    }
    // SAFETY: successful pidfd_open returned a fresh descriptor owned here.
    Ok(Some(unsafe { File::from_raw_fd(fd as i32) }))
}

fn send(handle: &File, signal: i32) -> std::io::Result<()> {
    // SAFETY: live borrowed pidfd; null siginfo requests kernel-generated
    // signal metadata, and flags are zero. No user memory is read or written.
    let result = unsafe {
        libc::syscall(
            libc::SYS_pidfd_send_signal,
            handle.as_raw_fd(),
            signal,
            std::ptr::null::<libc::siginfo_t>(),
            0u32,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        absent_or_error(std::io::Error::last_os_error())
    }
}

fn absent_or_error(error: std::io::Error) -> std::io::Result<()> {
    if error.raw_os_error() == Some(libc::ESRCH) {
        Ok(())
    } else {
        Err(error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task_env::env::process_start_time;

    #[test]
    fn handle_survives_exit_and_invalid_signal_propagates() {
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let pid = child.id() as i32;
        let record = ProcessRecord {
            pid,
            start_time: process_start_time(pid),
        };
        let handle = open(pid).unwrap().unwrap();
        let probe = signal(&record, 0);
        let invalid = send(&handle, -1);
        let _ = child.kill();
        child.wait().unwrap();
        probe.unwrap();
        assert_eq!(invalid.unwrap_err().raw_os_error(), Some(libc::EINVAL));
        // The retained handle refers to the exited task, not a future task
        // with the same numerical PID. ESRCH is an idempotent completion.
        send(&handle, libc::SIGKILL).unwrap();
        assert!(open(i32::MAX).unwrap().is_none());
    }

    #[test]
    fn missing_identity_and_process_group_ids_fail_closed() {
        for record in [
            ProcessRecord {
                pid: -1,
                start_time: Some(1),
            },
            ProcessRecord {
                pid: 0,
                start_time: Some(1),
            },
            ProcessRecord {
                pid: 1,
                start_time: Some(1),
            },
            ProcessRecord {
                pid: std::process::id() as i32,
                start_time: None,
            },
        ] {
            assert_eq!(
                signal(&record, 0).unwrap_err().raw_os_error(),
                Some(libc::EINVAL)
            );
        }
    }
}
