//! Local watcher readiness and liveness, independent of task phase.

use std::os::fd::RawFd;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

use serde_json::{json, Value};

use crate::error::{FlowError, Result};
use crate::model::{self, J};
use crate::util;
use crate::util::process::{spawn_with_ready, terminate_child, BackgroundChild};

pub fn watch_error(message: impl Into<String>) -> FlowError {
    FlowError::watch(message)
}

fn unhealthy(reason: &str, next_step: &str) -> J {
    json!({"healthy": false, "reason": reason, "next_step": next_step})
}

fn value_string(value: Option<&J>) -> String {
    match value {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Number(number)) => number.to_string(),
        Some(Value::Null) | None => String::new(),
        Some(other) => other.to_string(),
    }
}

fn start_ticks(pid: u32) -> Option<String> {
    util::process_start_ticks(pid)
}

/// Inspect watcher identity, heartbeat and lock ownership.
pub fn health(root: &Path) -> J {
    let root = root.to_path_buf();
    let state = util::read_json(&root.join("watch.json"));
    let Ok(state) = state else {
        return unhealthy(
            "watcher identity or heartbeat is unavailable",
            "inspect the state directory and process identity; do not infer delivery or completion",
        );
    };
    check_watch_status(&root, state)
}

fn check_watch_status(root: &Path, state: J) -> J {
    if model::string(&state, "status").as_deref() != Some("running") {
        return unhealthy(
            "watcher stopped",
            "restart only the verified existing watcher and confirm its lock/heartbeat; do not create a second watcher",
        );
    }
    check_process_identity(root, state)
}

fn check_process_identity(root: &Path, state: J) -> J {
    let pid = model::u64_field(&state, "pid").unwrap_or(0) as u32;
    let recorded_ticks = value_string(model::get(&state, "start_ticks"));
    let expected_ticks = start_ticks(pid);
    if expected_ticks.as_deref() != Some(recorded_ticks.as_str()) {
        return unhealthy(
            "watcher process identity changed",
            "inspect the state directory and process identity; do not kill another task or reuse its lock",
        );
    }
    check_heartbeat(root, state)
}

fn check_heartbeat(root: &Path, state: J) -> J {
    let heartbeat_at = model::number(&state, "heartbeat_at").unwrap_or(0.0);
    let max_age = model::number(&state, "max_age").unwrap_or(0.0);
    if util::now() - heartbeat_at > max_age {
        return unhealthy(
            "watcher heartbeat expired",
            "inspect watch.log and the process; restart only this watcher after confirming ownership",
        );
    }
    check_lock(root, state)
}

fn check_lock(root: &Path, state: J) -> J {
    let lock_path = root.join("watch.lock");
    if !lock_path.exists() {
        return unhealthy(
            "watcher lock is not held",
            "inspect the lock and process ownership; do not switch state directories to bypass the mutex",
        );
    }
    assess_lock(&lock_path, state)
}

fn assess_lock(lock_path: &Path, state: J) -> J {
    let acquired = util::FileLock::acquire(lock_path, false);
    let Ok(acquired) = acquired else {
        return unhealthy(
            "watcher identity or heartbeat is unavailable",
            "inspect the state directory and process identity; do not infer delivery or completion",
        );
    };
    watch_lock_state(acquired, state)
}

fn watch_lock_state(acquired: Option<util::FileLock>, state: J) -> J {
    let None = acquired else {
        return unhealthy(
            "watcher lock is not held",
            "inspect the lock and process ownership; do not switch state directories to bypass the mutex",
        );
    };
    healthy_state(state)
}

fn healthy_state(mut state: J) -> J {
    if let Some(map) = state.as_object_mut() {
        map.insert("healthy".to_string(), json!(true));
    }
    state
}

pub fn acquire_lock(path: &Path, attempts: u32, delay: Duration) -> Result<util::FileLock> {
    try_acquire_lock(path, attempts, delay, 0)
}

fn try_acquire_lock(
    path: &Path,
    attempts: u32,
    delay: Duration,
    attempt: u32,
) -> Result<util::FileLock> {
    let exhausted = attempt >= attempts;
    let Some(()) = (!exhausted).then_some(()) else {
        return Err(watch_error("another watcher already owns this directory"));
    };
    let lock = util::FileLock::acquire(path, false)?;
    acquired_lock_or_retry(lock, path, attempts, delay, attempt)
}

fn acquired_lock_or_retry(
    lock: Option<util::FileLock>,
    path: &Path,
    attempts: u32,
    delay: Duration,
    attempt: u32,
) -> Result<util::FileLock> {
    let Some(lock) = lock else {
        return retry_acquire_lock(path, attempts, delay, attempt);
    };
    Ok(lock)
}

fn retry_acquire_lock(
    path: &Path,
    attempts: u32,
    delay: Duration,
    attempt: u32,
) -> Result<util::FileLock> {
    if attempt + 1 < attempts {
        thread::sleep(delay);
        return try_acquire_lock(path, attempts, delay, attempt + 1);
    }
    Err(watch_error("another watcher already owns this directory"))
}

/// Run a generic watcher loop with a lock, heartbeat and readiness handshake.
pub fn run<F, V>(
    root: &Path,
    tick: F,
    validate: V,
    interval: u64,
    ready_fd: Option<RawFd>,
    once: bool,
) -> Result<()>
where
    F: FnMut() -> Result<bool>,
    V: Fn() -> Result<()>,
{
    let mut ready_fd = ready_fd;
    run_inner(root, tick, validate, interval, &mut ready_fd, once).inspect_err(|error| {
        // Startup failures must not leave a background launcher waiting for
        // readiness until its timeout.
        if let Some(fd) = ready_fd.take() {
            util::write_ready_fd(
                fd,
                &json!({
                    "ready": false,
                    "pid": std::process::id(),
                    "error": error.to_string()
                }),
            );
        }
    })
}

fn run_inner<F, V>(
    root: &Path,
    tick: F,
    validate: V,
    interval: u64,
    ready_fd: &mut Option<RawFd>,
    once: bool,
) -> Result<()>
where
    F: FnMut() -> Result<bool>,
    V: Fn() -> Result<()>,
{
    validate()
        .and_then(|()| prepare_watch_root(root))
        .and_then(|root| watch_session(root, tick, interval, ready_fd, once))
}

fn prepare_watch_root(root: &Path) -> Result<PathBuf> {
    let root = util::resolve_path(root);
    std::fs::create_dir_all(&root)?;
    Ok(root)
}

fn watch_session<F>(
    root: PathBuf,
    tick: F,
    interval: u64,
    ready_fd: &mut Option<RawFd>,
    once: bool,
) -> Result<()>
where
    F: FnMut() -> Result<bool>,
{
    let lock = acquire_lock(&root.join("watch.lock"), 10, Duration::from_millis(50))?;
    launch_watch_session(root, lock, tick, interval, ready_fd, once)
}

fn launch_watch_session<F>(
    root: PathBuf,
    lock: util::FileLock,
    mut tick: F,
    interval: u64,
    ready_fd: &mut Option<RawFd>,
    once: bool,
) -> Result<()>
where
    F: FnMut() -> Result<bool>,
{
    let mut state = initial_watch_state(interval);
    util::atomic_write_json(&root.join("watch.json"), &state)?;
    notify_ready(ready_fd);
    let outcome = tick_until_done(&root, &mut state, &mut tick, interval, once);
    notify_loop_error(ready_fd, &outcome);
    finish_watch_session(&root, &mut state, lock);
    outcome
}

fn initial_watch_state(interval: u64) -> J {
    json!({
        "pid": std::process::id(),
        "start_ticks": start_ticks(std::process::id()).unwrap_or_default(),
        "started_at": util::now(),
        "heartbeat_at": util::now(),
        "max_age": std::cmp::max(180, interval as i64 * 3 + 30),
        "status": "running"
    })
}

fn notify_ready(ready_fd: &mut Option<RawFd>) {
    if let Some(fd) = ready_fd.take() {
        util::write_ready_fd(fd, &json!({"ready": true, "pid": std::process::id()}));
    }
}

fn notify_loop_error(ready_fd: &mut Option<RawFd>, outcome: &Result<()>) {
    let Err(error) = outcome else {
        return;
    };
    if let Some(fd) = ready_fd.take() {
        util::write_ready_fd(
            fd,
            &json!({
                "ready": false,
                "pid": std::process::id(),
                "error": error.to_string()
            }),
        );
    }
}

fn tick_until_done<F>(
    root: &Path,
    state: &mut J,
    tick: &mut F,
    interval: u64,
    once: bool,
) -> Result<()>
where
    F: FnMut() -> Result<bool>,
{
    let mut run = || -> Result<()> {
        loop {
            let stop = tick()?;
            state["heartbeat_at"] = json!(util::now());
            util::atomic_write_json(&root.join("watch.json"), state)?;
            if once || stop {
                return Ok(());
            }
            thread::sleep(Duration::from_secs(interval));
        }
    };
    run()
}

fn finish_watch_session(root: &Path, state: &mut J, lock: util::FileLock) {
    state["status"] = json!("stopped");
    state["heartbeat_at"] = json!(util::now());
    let _ = util::atomic_write_json(&root.join("watch.json"), state);
    drop(lock);
}

/// Confirm readiness, then transfer child ownership to a reaper.
pub fn background<V>(
    root: &Path,
    argv_without_exe: &[String],
    validate: V,
    timeout: Duration,
) -> Result<J>
where
    V: FnOnce() -> Result<()>,
{
    validate()
        .map(|()| util::resolve_path(root))
        .and_then(|root| start_watch_background(root, argv_without_exe, timeout))
}

fn start_watch_background(
    root: PathBuf,
    argv_without_exe: &[String],
    timeout: Duration,
) -> Result<J> {
    let current = health(&root);
    if model::bool_or(&current, "healthy", false) {
        return Ok(already_running_watch(current, &root));
    }
    spawn_watch_background(root, argv_without_exe, timeout)
}

fn already_running_watch(mut value: J, root: &Path) -> J {
    value["already_running"] = json!(true);
    value["state_dir"] = json!(root.display().to_string());
    value
}

fn spawn_watch_background(
    root: PathBuf,
    argv_without_exe: &[String],
    timeout: Duration,
) -> Result<J> {
    watch_background_argv(argv_without_exe)
        .and_then(|argv| spawn_with_ready(&argv, &root.join("watch.log"), None, timeout))
        .and_then(|mut child| {
            confirm_watch_background(&root, &mut child).map(|state| (child, state))
        })
        .map(|(child, state)| ready_watch_background(&root, child, state))
}

fn watch_background_argv(argv_without_exe: &[String]) -> Result<Vec<String>> {
    let exe = util::current_exe()?;
    let mut argv = vec![exe.display().to_string()];
    argv.extend(argv_without_exe.iter().cloned());
    Ok(argv)
}

fn confirm_watch_background(root: &Path, child: &mut BackgroundChild) -> Result<J> {
    confirm_watch_ready(child).and_then(|()| confirm_watch_health(root, child))
}

fn confirm_watch_ready(child: &mut BackgroundChild) -> Result<()> {
    let ready = child.ready.clone().unwrap_or(Value::Null);
    if !model::bool_or(&ready, "ready", false) {
        return reject_watch_background(child, &ready);
    }
    let ready_pid = model::int(&ready, "pid").unwrap_or_default();
    let matching = acceptable_ready_pid(ready_pid, child.pid);
    let Some(()) = matching.then_some(()) else {
        return reject_watch_background(child, &ready);
    };
    Ok(())
}

fn acceptable_ready_pid(ready_pid: i64, pid: u32) -> bool {
    ready_pid == 0 || ready_pid == pid as i64
}

fn confirm_watch_health(root: &Path, child: &mut BackgroundChild) -> Result<J> {
    let state = health(root);
    verify_watch_health(&state, child).map(|()| state)
}

fn verify_watch_health(state: &J, child: &mut BackgroundChild) -> Result<()> {
    if !model::bool_or(state, "healthy", false) {
        return reject_unhealthy_watch(child);
    }
    verify_watch_pid(state, child)
}

fn verify_watch_pid(state: &J, child: &mut BackgroundChild) -> Result<()> {
    let state_pid = model::u64_field(state, "pid").unwrap_or(0) as u32;
    if state_pid == child.pid {
        return Ok(());
    }
    reject_unhealthy_watch(child)
}

fn reject_watch_background<T>(child: &mut BackgroundChild, ready: &J) -> Result<T> {
    let message = model::string(ready, "error")
        .unwrap_or_else(|| "watcher exited before readiness; inspect watch.log".to_string());
    terminate_child(&mut child.child);
    Err(watch_error(message))
}

fn reject_unhealthy_watch<T>(child: &mut BackgroundChild) -> Result<T> {
    terminate_child(&mut child.child);
    Err(watch_error(
        "watcher lost readiness immediately after startup",
    ))
}

fn ready_watch_background(root: &Path, child: BackgroundChild, mut state: J) -> J {
    let pid = child.pid;
    child.reap();
    state["ready"] = json!(true);
    state["state_dir"] = json!(root.display().to_string());
    let _ = pid;
    state
}
