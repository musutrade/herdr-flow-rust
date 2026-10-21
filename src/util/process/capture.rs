//! Capture without blocking reads, writes, or pipe-reader joins. The child is
//! kept waitable until cleanup so its PID (and process-group ID) cannot be reused.
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::process::{Child, ChildStderr, ChildStdin, ChildStdout};
use std::thread;
use std::time::{Duration, Instant};

use super::{CommandOptions, CommandOutput};
use crate::{ErrorKind, FlowError, Result};

// Constructed with Some(child); only Drop takes the child. Keeping Option lets
// Drop transfer it to a reaper thread after process-group cleanup.
struct OwnedCommand(Option<Child>);

impl OwnedCommand {
    fn child(&mut self) -> &mut Child {
        // A live OwnedCommand cannot have run Drop's sole take yet.
        self.0.as_mut().expect("owned command child")
    }
}

impl Drop for OwnedCommand {
    fn drop(&mut self) {
        // This is the sole take, and Drop runs once for the initialized owner.
        let mut child = self.0.take().expect("owned command child");
        // process_group(0) reserved this group for this command. waitid below
        // does not reap the leader, even when it exits before its descendants.
        // SAFETY: kill takes only scalar arguments. This exclusively owned
        // child remains unreaped (WNOWAIT), reserving its PID/group ID until
        // group cleanup completes; only then may try_wait/wait reap the leader.
        unsafe { libc::kill(-(child.id() as libc::pid_t), libc::SIGKILL) };
        let _ = child.kill();
        if !matches!(child.try_wait(), Ok(Some(_))) {
            // Reaping must not extend the caller's deadline (e.g. a task stuck
            // in kernel I/O). This thread owns no captured pipe or stdin data.
            thread::spawn(move || {
                let _ = child.wait();
            });
        }
    }
}

pub(super) fn run(
    child: Child,
    options: &CommandOptions,
    program: &str,
    deadline: Instant,
) -> Result<CommandOutput> {
    let mut command = OwnedCommand(Some(child));
    let mut pipes = Pipes::new(command.child(), options)?;
    let status = wait(&mut command, &mut pipes, deadline, program)?;
    Ok(CommandOutput {
        status,
        stdout: pipes.stdout.bytes,
        stderr: pipes.stderr.bytes,
    })
}

fn wait(
    command: &mut OwnedCommand,
    pipes: &mut Pipes<'_>,
    deadline: Instant,
    program: &str,
) -> Result<i32> {
    loop {
        if Instant::now() >= deadline {
            return Err(FlowError::new(
                ErrorKind::Timeout,
                format!("{program} timed out"),
            ));
        }
        pipes.pump()?;
        if let Some(status) = exit_status(command.child())? {
            if pipes.complete() {
                return Ok(status);
            }
        }
        pipes.wait(deadline.saturating_duration_since(Instant::now()))?;
    }
}

fn exit_status(child: &Child) -> io::Result<Option<i32>> {
    let mut info = std::mem::MaybeUninit::<libc::siginfo_t>::zeroed();
    // SAFETY: waitid initializes siginfo; WNOHANG returns si_pid=0 when the
    // child is running. WNOWAIT preserves ownership until group cleanup.
    let result = unsafe {
        libc::waitid(
            libc::P_PID,
            child.id(),
            info.as_mut_ptr(),
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        )
    };
    if result < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: storage was zeroed before waitid and the successful syscall wrote
    // the wait result. The no-event case retains a valid zero si_pid.
    let info = unsafe { info.assume_init() };
    // SAFETY: waitid's child-event layout exposes si_pid; the zeroed no-event
    // representation is checked before reading any child exit status.
    if unsafe { info.si_pid() } == 0 {
        return Ok(None);
    }
    // SAFETY: nonzero si_pid above identifies a waitid child event, for which
    // si_status is the active status field of siginfo_t.
    let status = unsafe { info.si_status() };
    Ok(Some(if info.si_code == libc::CLD_EXITED {
        status
    } else {
        -status
    }))
}

fn nonblocking(pipe: &impl AsRawFd) -> io::Result<()> {
    let fd = pipe.as_raw_fd();
    // SAFETY: pipe is borrowed and stays live; F_GETFL has no pointer argument.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the same borrowed descriptor remains open; F_SETFL receives only
    // the flags returned above plus O_NONBLOCK and does not transfer ownership.
    if unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

struct Captured<R> {
    pipe: Option<R>,
    bytes: Vec<u8>,
    limit: usize,
    name: &'static str,
}

impl<R: Read + AsRawFd> Captured<R> {
    fn new(pipe: Option<R>, limit: usize, name: &'static str) -> Result<Self> {
        let pipe = pipe.ok_or_else(|| FlowError::io("missing capture pipe"))?;
        nonblocking(&pipe)?;
        Ok(Self {
            pipe: Some(pipe),
            bytes: Vec::new(),
            limit,
            name,
        })
    }

    fn pump(&mut self) -> Result<()> {
        let Some(pipe) = self.pipe.as_mut() else {
            return Ok(());
        };
        // Bound work per turn: a continuously writing process cannot postpone
        // checking the deadline, and neither output stream can starve stdin.
        let mut buffer = [0; 64 * 1024];
        match pipe.read(&mut buffer) {
            Ok(0) => self.pipe = None,
            Ok(n) => {
                if n > self.limit.saturating_sub(self.bytes.len()) {
                    return Err(FlowError::new(
                        ErrorKind::OutputLimit,
                        format!("{} exceeded {} byte capture limit", self.name, self.limit),
                    ));
                }
                self.bytes.extend_from_slice(&buffer[..n]);
            }
            Err(error) if retry_io(&error) => {}
            Err(error) => return Err(error.into()),
        }
        Ok(())
    }
}

fn retry_io(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
    )
}

struct Input<'a> {
    pipe: Option<ChildStdin>,
    remaining: &'a [u8],
}

impl<'a> Input<'a> {
    fn new(pipe: Option<ChildStdin>, data: &'a [u8]) -> io::Result<Self> {
        if let Some(pipe) = &pipe {
            nonblocking(pipe)?;
        }
        Ok(Self {
            pipe,
            remaining: data,
        })
    }

    fn pump(&mut self) -> io::Result<()> {
        if self.remaining.is_empty() {
            self.pipe = None;
        }
        let Some(pipe) = self.pipe.as_mut() else {
            return Ok(());
        };
        let end = self.remaining.len().min(64 * 1024);
        match pipe.write(&self.remaining[..end]) {
            Ok(n) => self.remaining = &self.remaining[n..],
            Err(error) if error.kind() == io::ErrorKind::BrokenPipe => self.pipe = None,
            Err(error) if retry_io(&error) => {}
            Err(error) => return Err(error),
        }
        Ok(())
    }
}

struct Pipes<'a> {
    stdout: Captured<ChildStdout>,
    stderr: Captured<ChildStderr>,
    stdin: Input<'a>,
}

impl<'a> Pipes<'a> {
    fn new(child: &mut Child, options: &'a CommandOptions) -> Result<Self> {
        Ok(Self {
            stdout: Captured::new(child.stdout.take(), options.stdout_limit, "stdout")?,
            stderr: Captured::new(child.stderr.take(), options.stderr_limit, "stderr")?,
            stdin: Input::new(
                child.stdin.take(),
                options.stdin.as_deref().unwrap_or_default(),
            )?,
        })
    }

    fn pump(&mut self) -> Result<()> {
        self.stdout.pump()?;
        self.stderr.pump()?;
        self.stdin.pump().map_err(Into::into)
    }

    fn complete(&self) -> bool {
        self.stdout.pipe.is_none() && self.stderr.pipe.is_none() && self.stdin.pipe.is_none()
    }

    fn wait(&self, remaining: Duration) -> io::Result<()> {
        let mut fds = [
            poll_fd(self.stdout.pipe.as_ref(), libc::POLLIN),
            poll_fd(self.stderr.pipe.as_ref(), libc::POLLIN),
            poll_fd(self.stdin.pipe.as_ref(), libc::POLLOUT),
        ];
        // Wake immediately on I/O, and periodically to observe child exit even
        // after all pipes close. Never pace large output by a fixed sleep.
        let millis = remaining.as_millis().min(5) as i32;
        // SAFETY: fds is a live writable array of exactly the supplied length;
        // each pipe owner stays borrowed, and poll ignores absent fd=-1 entries.
        let result = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, millis) };
        if result < 0 {
            let error = io::Error::last_os_error();
            if !retry_io(&error) {
                return Err(error);
            }
        }
        Ok(())
    }
}

fn poll_fd(pipe: Option<&impl AsRawFd>, events: libc::c_short) -> libc::pollfd {
    libc::pollfd {
        fd: pipe.map(AsRawFd::as_raw_fd).unwrap_or(-1),
        events,
        revents: 0,
    }
}
