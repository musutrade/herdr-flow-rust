//! Local Unix sockets, including task roots longer than sockaddr_un permits.
use std::fs::OpenOptions;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::{
    ffi::OsStrExt,
    fs::OpenOptionsExt,
    net::{UnixListener, UnixStream},
};
use std::path::Path;

pub(crate) fn bind(path: &Path) -> io::Result<UnixListener> {
    with_socket_path(path, |path| UnixListener::bind(path))
}

pub(crate) fn connect(path: &Path) -> io::Result<UnixStream> {
    with_socket_path(path, connect_at)
}

// The descriptor is held through bind/connect. The socket remains at its
// original task path; this creates neither a symlink nor an external endpoint.
fn with_socket_path<T>(path: &Path, action: impl FnOnce(&Path) -> io::Result<T>) -> io::Result<T> {
    match address(path) {
        Ok(_) => return action(path),
        Err(error) if path.as_os_str().as_bytes().contains(&0) => return Err(error),
        Err(_) => (),
    }
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "socket name missing"))?;
    let directory = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(parent)?;
    let alias =
        std::path::PathBuf::from(format!("/proc/self/fd/{}", directory.as_raw_fd())).join(name);
    action(&alias)
}

fn connect_at(path: &Path) -> io::Result<UnixStream> {
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
    fn long_socket_paths_bind_connect_and_preserve_existing_entries() {
        use std::io::{Read, Write};
        let dir = crate::util::fs::test_tempdir();
        let root = dir.path().join("nested-".repeat(24));
        std::fs::create_dir(&root).unwrap();
        let path = root.join("broker.sock");
        let listener = bind(&path).unwrap();
        assert!(path.exists());
        assert!(bind(&path).is_err());
        let mut client = connect(&path).unwrap();
        client.write_all(b"owned").unwrap();
        let (mut server, _) = listener.accept().unwrap();
        let mut bytes = [0; 5];
        server.read_exact(&mut bytes).unwrap();
        assert_eq!(&bytes, b"owned");
        drop(listener);
        std::fs::remove_file(&path).unwrap();
        std::fs::write(&path, "keep").unwrap();
        assert!(bind(&path).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "keep");
        assert!(bind(&root.join("x".repeat(200))).is_err());
        assert!(bind(Path::new("bad\0path")).is_err());
        assert!(connect(Path::new("bad\0path")).is_err());
    }

    #[test]
    fn saturated_listener_fails_promptly_and_stale_socket_is_refused() {
        const CHILD: &str = "HERDR_SOCKET_BACKLOG_TEST_CHILD";
        if std::env::var_os(CHILD).is_none() {
            // A concurrent fork can temporarily inherit a CLOEXEC listener
            // until exec, keeping it alive after this thread drops its copy.
            // Run this close/refusal assertion without sibling spawning tests.
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "util::unix_socket::tests::saturated_listener_fails_promptly_and_stale_socket_is_refused", "--nocapture"])
                .env(CHILD, "1")
                .output()
                .unwrap();
            assert!(output.status.success(), "{output:?}");
            return;
        }
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
