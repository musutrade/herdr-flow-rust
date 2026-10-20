//! Authenticate the connected local socket, in addition to checking its path.
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::UnixStream;

use crate::error::{FlowError, Result};
use crate::logging;

pub(super) fn verify(stream: &UnixStream) -> Result<()> {
    // SAFETY: geteuid has no preconditions and does not borrow memory.
    let expected = unsafe { libc::geteuid() };
    let result = peer_uid(stream.as_raw_fd()).and_then(|actual| check_uid(actual, expected));
    result.inspect_err(|_| {
        logging::log_event(
            "rpc_peer_rejected",
            "codex_rpc",
            "connected socket peer could not be authenticated",
        );
    })
}

fn check_uid(actual: u32, expected: u32) -> Result<()> {
    if actual != expected {
        return Err(FlowError::rpc("connected app-server peer UID mismatch"));
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn peer_uid(fd: RawFd) -> Result<u32> {
    let mut credentials = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut length = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: credentials and length are initialized writable values of the
    // requested sizes. The borrowed fd remains owned by the live UnixStream;
    // getsockopt neither consumes it nor retains these pointers.
    let result = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut credentials as *mut libc::ucred).cast(),
            &mut length,
        )
    };
    if result != 0 {
        return Err(FlowError::rpc(format!(
            "cannot query app-server peer credentials: {}",
            std::io::Error::last_os_error().kind()
        )));
    }
    if length as usize != std::mem::size_of::<libc::ucred>() {
        return Err(FlowError::rpc("incomplete app-server peer credentials"));
    }
    Ok(credentials.uid)
}

#[cfg(not(target_os = "linux"))]
fn peer_uid(_fd: RawFd) -> Result<u32> {
    Err(FlowError::rpc(
        "app-server peer authentication requires Linux SO_PEERCRED",
    ))
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    #[test]
    fn authenticates_connected_same_uid_socket() {
        let (client, server) = UnixStream::pair().unwrap();
        verify(&client).unwrap();
        verify(&server).unwrap();
    }

    #[test]
    fn rejects_uid_mismatch_and_credential_query_failure() {
        assert!(check_uid(1000, 1001)
            .unwrap_err()
            .message
            .contains("UID mismatch"));
        assert!(peer_uid(-1).unwrap_err().message.contains("cannot query"));
    }
}
