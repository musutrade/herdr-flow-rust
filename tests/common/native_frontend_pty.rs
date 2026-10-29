//! Real PTY/job-control proof for the exact frontend command; no native service.
use super::*;
use std::{
    ffi::CString,
    fs::{File, OpenOptions},
    io::Write,
    os::{
        fd::FromRawFd,
        unix::{
            ffi::OsStrExt,
            fs::{OpenOptionsExt, PermissionsExt},
            process::CommandExt,
        },
    },
    process::{Child, Command, Stdio},
    time::Instant,
};

struct Pty {
    master: Option<File>,
    child: Child,
    owned: Vec<(u32, String)>,
}
impl Pty {
    fn start(cwd: &Path) -> Self {
        let (mut master, mut slave) = (-1, -1);
        // SAFETY: live writable descriptor slots; null name/termios/window use defaults.
        assert_eq!(
            unsafe {
                libc::openpty(
                    &mut master,
                    &mut slave,
                    std::ptr::null_mut(),
                    std::ptr::null(),
                    std::ptr::null(),
                )
            },
            0
        );
        for fd in [master, slave] {
            // SAFETY: the fresh descriptors are live; prevent the child from
            // retaining the PTY master and defeating a real close/hangup.
            assert_eq!(
                unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) },
                0
            );
        }
        // SAFETY: openpty returned two fresh owned descriptors, transferred once.
        let master = unsafe { File::from_raw_fd(master) };
        // SAFETY: slave is the other newly owned descriptor from openpty.
        let slave = unsafe { File::from_raw_fd(slave) };
        let mut command = Command::new("/bin/bash");
        command
            .args(["--noprofile", "--norc", "-i"])
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("TERM", "xterm")
            .env("HISTFILE", "/dev/null")
            .env("HISTSIZE", "0")
            .current_dir(cwd)
            .stdin(Stdio::from(slave.try_clone().unwrap()))
            .stdout(Stdio::from(slave.try_clone().unwrap()))
            .stderr(Stdio::from(slave));
        // SAFETY: child-only scalar syscalls after stdio installation; no allocation or locks.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() < 0 || libc::ioctl(0, libc::TIOCSCTTY, 0) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = command.spawn().unwrap();
        Self {
            master: Some(master),
            child,
            owned: vec![],
        }
    }
    fn close(&mut self) -> std::process::ExitStatus {
        self.master.take();
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return status;
            }
            if Instant::now() >= deadline {
                self.kill_owned();
                self.child.kill().unwrap();
                return self.child.wait().unwrap();
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    fn kill_owned(&self) {
        for (pid, ticks) in &self.owned {
            if util::process_start_ticks(*pid).as_ref() == Some(ticks) {
                // SAFETY: exact fixture-owned PID/start identity was checked immediately above.
                unsafe {
                    libc::kill(*pid as i32, libc::SIGKILL);
                }
            }
        }
    }
}
impl Drop for Pty {
    fn drop(&mut self) {
        self.close();
        self.kill_owned();
    }
}

fn wait_pid(path: &Path) -> u32 {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if let Ok(text) = std::fs::read_to_string(path) {
            if let Ok(pid) = text.trim().parse() {
                return pid;
            }
        }
        assert!(
            Instant::now() < deadline,
            "missing fixture PID at {}",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}
fn identity(pid: u32) -> J {
    let text = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
    let fields: Vec<_> = text[text.rfind(')').unwrap() + 2..]
        .split_whitespace()
        .collect();
    json!({"pid":pid,"ppid":fields[1].parse::<u32>().unwrap(),"pgrp":fields[2],"sid":fields[3],"start":fields[19],"cwd":std::fs::read_link(format!("/proc/{pid}/cwd")).unwrap()})
}
fn stopped(pid: u32, ticks: &str) -> bool {
    let Ok(text) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
        return true;
    };
    let fields: Vec<_> = text[text.rfind(')').unwrap() + 2..]
        .split_whitespace()
        .collect();
    fields[19] != ticks || fields[0] == "Z"
}
fn wait_stopped(pid: u32, ticks: &str) {
    let deadline = Instant::now() + Duration::from_secs(3);
    while !stopped(pid, ticks) {
        assert!(Instant::now() < deadline, "fixture PID {pid} did not stop");
        std::thread::sleep(Duration::from_millis(5));
    }
}
fn release(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        match OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(path)
        {
            Ok(mut file) => {
                file.write_all(b"done\n").unwrap();
                return;
            }
            Err(error) => {
                assert!(Instant::now() < deadline, "fixture FIFO: {error}");
                std::thread::sleep(Duration::from_millis(5));
            }
        }
    }
}
fn exercise(hangup: bool) -> J {
    let temp = util::fs::test_tempdir();
    let checkout = temp.path().join("checkout");
    util::ensure_private_dir(&checkout, true).unwrap();
    let directory = temp.path().join("attachment");
    util::ensure_private_dir(&directory, true).unwrap();
    let fifo = directory.join("release");
    let name = CString::new(fifo.as_os_str().as_bytes()).unwrap();
    // SAFETY: name is a valid NUL-terminated owned path, mode is a scalar.
    assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
    let driver = temp.path().join("frontend driver");
    std::fs::write(&driver,"#!/bin/sh\nd=$4\nprintf '%s\\n' \"$$\" > \"$d/wrapper.pid\"\ntrap 'kill \"$native\" 2>/dev/null; wait \"$native\" 2>/dev/null; exit 0' HUP TERM INT\n/bin/sh -c 'read token < \"$1\"' native \"$d/release\" &\nnative=$!\nprintf '%s\\n' \"$native\" > \"$d/native.pid\"\nwait \"$native\"\n").unwrap();
    std::fs::set_permissions(&driver, std::fs::Permissions::from_mode(0o700)).unwrap();
    let instance = util::uuid_hex();
    let runtime = json!({"driver_binary":driver,"instance":instance});
    let record = json!({"directory":directory,"instance":instance});
    let command = super::super::frontend_command(&runtime, &record).unwrap();
    let mut pty = Pty::start(&checkout);
    let shell = identity(pty.child.id());
    writeln!(pty.master.as_mut().unwrap(), "{command}").unwrap();
    let wrapper = identity(wait_pid(&directory.join("wrapper.pid")));
    let native = identity(wait_pid(&directory.join("native.pid")));
    for item in [&wrapper, &native] {
        pty.owned.push((
            item["pid"].as_u64().unwrap() as u32,
            item["start"].as_str().unwrap().into(),
        ));
    }
    let processes = json!({"shell_pid":shell["pid"],"foreground_processes":[{"pid":wrapper["pid"]},{"pid":native["pid"]}]});
    // PID files can become visible just before the native child execs. Wait
    // for that exact fixture identity to acquire its marker, not for a delay.
    let deadline = Instant::now() + Duration::from_secs(3);
    while !crate::task_env::process_has_marker(native["pid"].as_u64().unwrap() as i32, &instance) {
        assert!(
            Instant::now() < deadline,
            "native exec did not acquire marker"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    safe_foreground(&record, &processes).unwrap();
    assert!(safe_foreground(
        &record,
        &json!({"shell_pid":shell["pid"],"foreground_processes":[{"pid":std::process::id()}]})
    )
    .is_err());
    let status = if hangup {
        pty.close()
    } else {
        release(&fifo);
        wait_stopped(
            wrapper["pid"].as_u64().unwrap() as u32,
            wrapper["start"].as_str().unwrap(),
        );
        pty.close()
    };
    wait_stopped(
        native["pid"].as_u64().unwrap() as u32,
        native["start"].as_str().unwrap(),
    );
    json!({"hangup":hangup,"shell":shell,"wrapper":wrapper,"native":native,"exit":status.to_string(),"native_stopped":true})
}

#[test]
fn dedicated_frontend_exec_preserves_shell_pid_and_reaps_native_on_exit_and_pty_close() {
    for hangup in [false, true] {
        let facts = exercise(hangup);
        eprintln!("frontend PTY evidence: {facts}");
        assert_eq!(
            facts["wrapper"]["pid"], facts["shell"]["pid"],
            "waiting checkout shell must be replaced: {facts}"
        );
        assert_eq!(facts["wrapper"]["start"], facts["shell"]["start"]);
        assert_eq!(facts["wrapper"]["ppid"], json!(std::process::id()));
        assert_eq!(facts["native"]["ppid"], facts["wrapper"]["pid"]);
        assert_eq!(facts["exit"], "exit status: 0", "{facts}");
    }
}
