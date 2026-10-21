//! Connect to a local Unix socket without blocking on a full listen backlog.
use std::io;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::{ffi::OsStrExt, net::UnixStream};
use std::path::Path;

pub(crate) fn connect(path: &Path) -> io::Result<UnixStream> {
    let address = address(path)?;
    // SAFETY: socket takes scalar constants and returns a new owned descriptor.
    let fd = unsafe {
        libc::socket(
            libc::AF_UNIX,
            libc::SOCK_STREAM | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC,
            0,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: successful socket transferred one fresh descriptor, with no other owner.
    let stream = unsafe { UnixStream::from_raw_fd(fd) };
    // SAFETY: address is initialized, terminated and live for its supplied size;
    // the borrowed descriptor remains owned by stream during connect.
    let result = unsafe {
        libc::connect(
            stream.as_raw_fd(),
            (&address as *const libc::sockaddr_un).cast(),
            std::mem::size_of_val(&address) as libc::socklen_t,
        )
    };
    if result < 0 {
        return Err(io::Error::last_os_error());
    }
    stream.set_nonblocking(false)?;
    Ok(stream)
}
fn address(path: &Path) -> io::Result<libc::sockaddr_un> {
    // SAFETY: sockaddr_un consists of scalar integers and a fixed byte array;
    // zero is valid for every field and provides the pathname terminator.
    let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    let bytes = path.as_os_str().as_bytes();
    if bytes.len() >= address.sun_path.len() || bytes.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid broker socket path",
        ));
    }
    address.sun_family = libc::AF_UNIX as libc::sa_family_t;
    for (out, byte) in address.sun_path.iter_mut().zip(bytes) {
        *out = *byte as libc::c_char;
    }
    Ok(address)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;
    #[test]
    fn saturated_listener_fails_promptly_and_stale_socket_is_refused() {
        let dir = crate::util::fs::test_tempdir();
        let path = dir.path().join("broker.sock");
        let listener = UnixListener::bind(&path).unwrap();
        // SAFETY: listener owns a live listening socket; listen only updates its backlog.
        assert_eq!(unsafe { libc::listen(listener.as_raw_fd(), 1) }, 0);
        let start = std::time::Instant::now();
        let mut pending = Vec::new();
        let mut refused = false;
        for _ in 0..4 {
            match connect(&path) {
                Ok(stream) => pending.push(stream),
                Err(error) => {
                    assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
                    refused = true;
                    break;
                }
            }
        }
        assert!(refused);
        assert!(start.elapsed() < std::time::Duration::from_secs(1));
        drop(listener);
        assert_eq!(
            connect(&path).unwrap_err().kind(),
            io::ErrorKind::ConnectionRefused
        );
        assert!(connect(Path::new(&"x".repeat(200))).is_err());
        assert!(connect(Path::new("bad\0path")).is_err());
    }
}
