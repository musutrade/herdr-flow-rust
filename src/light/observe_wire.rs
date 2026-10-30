//! Current public JSON API only. No version negotiation, dispatch or recovery.
use serde_json::{json, Value};
use std::{
    io::{self, Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd},
        unix::net::UnixStream,
    },
    path::Path,
    time::{Duration, Instant},
};

pub(super) const LIMIT: usize = 65536;
pub(super) struct Wire {
    stream: UnixStream,
    pending: Vec<u8>,
    timeout: Duration,
    endpoint: String,
}
fn invalid() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "invalid or oversized observer response",
    )
}
impl Wire {
    pub fn connect(path: &Path, timeout: Duration) -> io::Result<Self> {
        use std::os::unix::fs::MetadataExt;
        let before = std::fs::metadata(path)?;
        let stream = connect(path)?;
        let after = std::fs::metadata(path)?;
        if (before.dev(), before.ino()) != (after.dev(), after.ino()) {
            return Err(io::ErrorKind::Interrupted.into());
        }
        let endpoint = format!("{}:{}", after.dev(), after.ino());
        stream.set_read_timeout(Some(timeout))?;
        stream.set_write_timeout(Some(timeout))?;
        Ok(Self {
            stream,
            pending: Vec::new(),
            timeout,
            endpoint,
        })
    }
    pub fn request(&mut self, method: &str, params: Value) -> io::Result<Value> {
        let message =
            json!({"id":"light-observe","method":method,"params":params}).to_string() + "\n";
        self.stream.write_all(message.as_bytes())?;
        self.next(self.timeout)?.ok_or_else(invalid)
    }
    pub fn next(&mut self, timeout: Duration) -> io::Result<Option<Value>> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(index) = self.pending.iter().position(|b| *b == b'\n') {
                return self.line(index).map(Some);
            }
            if self.pending.len() >= LIMIT {
                return Err(invalid());
            }
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                return Ok(None);
            };
            self.stream.set_read_timeout(Some(remaining))?;
            if !self.read_chunk()? {
                return Ok(None);
            }
        }
    }
    fn line(&mut self, index: usize) -> io::Result<Value> {
        let bytes: Vec<_> = self.pending.drain(..=index).collect();
        serde_json::from_slice(&bytes).map_err(|_| invalid())
    }
    fn read_chunk(&mut self) -> io::Result<bool> {
        let mut bytes = [0u8; 4096];
        let length = bytes.len().min(LIMIT - self.pending.len());
        match self.stream.read(&mut bytes[..length]) {
            Ok(0) => Err(io::ErrorKind::UnexpectedEof.into()),
            Ok(n) => {
                self.pending.extend_from_slice(&bytes[..n]);
                Ok(true)
            }
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
                ) =>
            {
                Ok(false)
            }
            Err(e) => Err(e),
        }
    }
    pub fn instance(&self) -> io::Result<String> {
        let credentials = peer(&self.stream)?;
        if credentials.uid != unsafe { libc::geteuid() } {
            return Err(io::ErrorKind::PermissionDenied.into());
        }
        let start = process_start(credentials.pid as u32)?;
        let boot = std::fs::read_to_string("/proc/sys/kernel/random/boot_id")?;
        Ok(format!(
            "{}:{}:{}:{}",
            boot.trim(),
            credentials.pid,
            start,
            self.endpoint
        ))
    }
}
fn peer(stream: &UnixStream) -> io::Result<libc::ucred> {
    let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of_val(&cred) as libc::socklen_t;
    let result = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut cred as *mut libc::ucred).cast(),
            &mut len,
        )
    };
    if result < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(cred)
}
fn address(path: &Path) -> io::Result<libc::sockaddr_un> {
    use std::os::unix::ffi::OsStrExt;
    let bytes = path.as_os_str().as_bytes();
    let mut addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    if !path.is_absolute() || bytes.contains(&0) || bytes.len() >= addr.sun_path.len() {
        return Err(invalid());
    }
    addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
    for (dest, source) in addr.sun_path.iter_mut().zip(bytes) {
        *dest = *source as libc::c_char;
    }
    Ok(addr)
}
fn connect(path: &Path) -> io::Result<UnixStream> {
    let addr = address(path)?;
    let raw = unsafe {
        libc::socket(
            libc::AF_UNIX,
            libc::SOCK_STREAM | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
            0,
        )
    };
    if raw < 0 {
        return Err(io::Error::last_os_error());
    }
    let fd = unsafe { OwnedFd::from_raw_fd(raw) };
    let result = unsafe {
        libc::connect(
            fd.as_raw_fd(),
            (&addr as *const libc::sockaddr_un).cast(),
            std::mem::size_of_val(&addr) as libc::socklen_t,
        )
    };
    if result < 0 {
        return Err(io::Error::last_os_error());
    }
    let stream = UnixStream::from(fd);
    stream.set_nonblocking(false)?;
    Ok(stream)
}
pub(super) fn process_start(pid: u32) -> io::Result<String> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat"))?;
    let tail = stat.rsplit_once(')').ok_or_else(invalid)?.1;
    if matches!(tail.split_whitespace().next(), Some("Z" | "X")) {
        return Err(io::ErrorKind::NotFound.into());
    }
    let start = tail.split_whitespace().nth(19).ok_or_else(invalid)?;
    Ok(start.into())
}
