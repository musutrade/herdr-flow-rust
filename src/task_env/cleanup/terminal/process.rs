//! Fallible per-PID observation for destructive work. Diagnostic marker APIs
//! retain their old best-effort behavior; none of them authorizes this path.
use super::super::{observation, signals};
use super::*;
use std::{
    collections::BTreeMap,
    fs,
    io::{self, Read},
    os::unix::fs::MetadataExt,
    path::PathBuf,
    time::{Duration, Instant},
};

#[derive(Clone)]
struct Seen {
    record: ProcessRecord,
    parent: i32,
    marked: bool,
}
fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}
fn gone(pid: i32) -> io::Result<bool> {
    Ok(signals::open(pid)?.is_none())
}
fn stat(pid: i32) -> io::Result<Option<(ProcessRecord, i32, bool)>> {
    let text = match read_stat(pid) {
        Ok(v) => v,
        Err(e) if observation::missing_proc(&e) && gone(pid)? => return Ok(None),
        Err(e) => return Err(e),
    };
    parse_stat(pid, &text)
}
fn read_stat(pid: i32) -> io::Result<String> {
    #[cfg(test)]
    faults::check(pid, false)?;
    fs::read_to_string(format!("/proc/{pid}/stat"))
}

fn marker(pid: i32, instance: &str) -> io::Result<Option<bool>> {
    #[cfg(test)]
    faults::check(pid, true)?;
    let path = format!("/proc/{pid}/environ");
    let mut bytes = Vec::new();
    let result = fs::File::open(path).and_then(|f| f.take(1024 * 1024 + 1).read_to_end(&mut bytes));
    match result {
        Err(e) if observation::missing_proc(&e) && gone(pid)? => return Ok(None),
        Err(e) => return Err(e),
        Ok(_) => (),
    }
    if bytes.len() > 1024 * 1024 {
        return Err(invalid("process environment exceeds observation budget"));
    }
    let expected = format!("{}={instance}", crate::task_env::MARKER_ENV);
    Ok(Some(
        bytes
            .split(|b| *b == 0)
            .any(|row| row == expected.as_bytes()),
    ))
}
fn scan(plan: &CleanupState) -> Result<BTreeMap<i32, Seen>> {
    let boundary = boundary(plan)?;
    let mut result = BTreeMap::new();
    for (index, entry) in fs::read_dir("/proc")?.enumerate() {
        require(index < 262144, "process scan budget exceeded")?;
        let entry = entry?;
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<i32>() else {
            continue;
        };
        if let Some(seen) = observe_entry(plan, boundary.start_ticks, pid, &entry.path())? {
            result.insert(pid, seen);
        }
    }
    Ok(result)
}
fn add(targets: &mut Vec<ProcessRecord>, record: ProcessRecord) -> Result<bool> {
    require(
        record.pid > 1 && record.pid != std::process::id() as i32 && record.start_time.is_some(),
        "invalid/self cleanup process identity",
    )?;
    if targets.contains(&record) {
        return Ok(false);
    }
    require(targets.len() < 16384, "process ownership budget exceeded")?;
    targets.push(record);
    Ok(true)
}
fn live(record: &ProcessRecord) -> Result<bool> {
    require(
        record.start_time.is_some(),
        "recorded process lacks start identity",
    )?;
    Ok(observation::is_alive(record)?)
}
fn freeze(plan: &mut CleanupState) -> Result<()> {
    boundary(plan)?;
    // Existing registered roots are authority; historical descendant snapshots
    // alone are not. Their survivors must be marked or proved by frozen ancestry.
    freeze_registered(plan)?;
    for _ in 0..32 {
        let changed = freeze_round(plan)?;
        if !changed {
            return validate_retained_descendants(plan);
        }
    }
    Err(FlowError::guard(
        "process family did not stabilize; retry with obligation retained",
    ))
}
fn wait_stopped(records: &[ProcessRecord]) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let mut pending = false;
        for r in records {
            pending |= needs_stop(r)?;
        }
        if !pending {
            return Ok(());
        }
        require(
            Instant::now() < deadline,
            "owned process did not stop; no filesystem cleanup",
        )?;
        std::thread::sleep(Duration::from_millis(10));
    }
}
fn validate_retained_descendants(plan: &CleanupState) -> Result<()> {
    for p in &plan.pending_processes {
        for r in &p.descendants {
            require(
                !live(r)? || terminal(plan)?.targets.contains(r),
                "live historical descendant lacks current signal authority",
            )?;
        }
    }
    Ok(())
}
pub(super) fn quiesce(plan: &mut CleanupState) -> Result<()> {
    freeze(plan)?;
    #[cfg(test)]
    faults::after_freeze();
    boundary(plan)?;
    for r in &terminal(plan)?.targets {
        signals::signal(r, libc::SIGKILL)?;
    }
    await_exits(plan)?;
    ensure_gone(plan)?;
    plan.pending_processes.clear();
    storage::save(plan)
}
pub(super) fn ensure_gone(plan: &CleanupState) -> Result<()> {
    boundary(plan)?;
    for r in &terminal(plan)?.targets {
        require(!live(r)?, "owned process remains")?;
    }
    validate_retained_descendants(plan)?;
    require(
        !scan(plan)?.values().any(|p| p.marked),
        "new marked process observed; retry cleanup",
    )
}

pub(in crate::task_env) fn registered_identity(pid: i32, instance: &str) -> Result<ProcessRecord> {
    let (record, _, dead) =
        stat(pid)?.ok_or_else(|| FlowError::guard("registered process disappeared"))?;
    require(
        !dead && marker(pid, instance)? == Some(true),
        "registered process lacks exact marker",
    )?;
    verify_registered_stat(&record)?;
    Ok(record)
}

fn boundary(plan: &CleanupState) -> Result<&crate::task_env::instance::ProcessBoundary> {
    let boundary = terminal(plan)?
        .creation
        .process_boundary
        .as_ref()
        .ok_or_else(|| FlowError::guard("instance has no process birth boundary"))?;
    boundary.verify()?;
    Ok(boundary)
}
fn known(plan: &CleanupState, record: &ProcessRecord) -> Result<bool> {
    Ok(terminal(plan)?.targets.contains(record)
        || plan.pending_processes.iter().any(|p| {
            (p.pid == record.pid && p.start_time == record.start_time)
                || p.descendants.contains(record)
        }))
}

fn parse_stat(pid: i32, text: &str) -> io::Result<Option<(ProcessRecord, i32, bool)>> {
    let (prefix, rest) = text
        .rsplit_once(')')
        .ok_or_else(|| invalid("malformed proc stat"))?;
    validate_stat_pid(pid, prefix)?;
    let fields = rest.split_whitespace().collect::<Vec<_>>();
    let state = *fields
        .first()
        .ok_or_else(|| invalid("missing proc state"))?;
    if !matches!(
        state,
        "R" | "S" | "D" | "Z" | "T" | "t" | "X" | "x" | "K" | "W" | "P" | "I"
    ) {
        return Err(invalid("invalid proc state"));
    }
    let parent = fields
        .get(1)
        .ok_or_else(|| invalid("missing parent"))?
        .parse()
        .map_err(|_| invalid("invalid parent"))?;
    let start = fields
        .get(19)
        .ok_or_else(|| invalid("missing start time"))?
        .parse()
        .map_err(|_| invalid("invalid start time"))?;
    Ok(Some((
        ProcessRecord {
            pid,
            start_time: Some(start),
        },
        parent,
        matches!(state, "Z" | "X" | "x"),
    )))
}

fn validate_stat_pid(pid: i32, prefix: &str) -> io::Result<()> {
    let id = prefix
        .split_once(" (")
        .ok_or_else(|| invalid("malformed proc pid"))?
        .0
        .parse::<i32>()
        .map_err(|_| invalid("invalid pid"))?;
    if id != pid {
        return Err(invalid("proc pid changed"));
    }
    Ok(())
}

fn observe_entry(plan: &CleanupState, floor: u64, pid: i32, path: &Path) -> Result<Option<Seen>> {
    if !same_uid_process(pid, path)? {
        return Ok(None);
    }
    let Some((record, parent, dead)) = stat(pid).map_err(|error| {
        FlowError::io(format!(
            "cannot observe cleanup stat for pid {pid}: {error}"
        ))
    })?
    else {
        return Ok(None);
    };
    if dead {
        return Ok(None);
    }
    if record.start_time.is_some_and(|start| start < floor) && !known(plan, &record)? {
        return Ok(None);
    }
    let Some(marked) = stable_marker(&record, &plan.instance_id)? else {
        return Ok(None);
    };
    Ok(Some(Seen {
        record,
        parent,
        marked,
    }))
}

fn same_uid_process(pid: i32, path: &Path) -> Result<bool> {
    let metadata = match fs::metadata(path) {
        Ok(m) => m,
        Err(e) if observation::missing_proc(&e) && gone(pid)? => return Ok(false),
        Err(e) => {
            return Err(FlowError::io(format!(
                "cannot observe cleanup uid for pid {pid}: {e}"
            )))
        }
    };
    // Positive foreign UID evidence only. Same-UID permission/read errors
    // are not interpreted as a negative marker match.
    if metadata.uid() != util::process::current_uid() {
        return Ok(false);
    }
    Ok(true)
}

/// Read-only writer check. Reuses the terminal path's fallible PID/marker
/// observations, but never creates a cleanup plan or sends a signal.
pub(crate) fn human_quiescence(
    env: &crate::task_env::TaskEnvironment,
    checkout: &Path,
    allowed: &[ProcessRecord],
) -> Result<()> {
    let boundary = env
        .creation
        .as_ref()
        .and_then(|c| c.process_boundary.as_ref())
        .ok_or_else(|| FlowError::guard("human handoff has no process birth boundary"))?;
    boundary.verify()?;
    human_registered(env, allowed)?;
    human_scan(env, checkout, allowed, boundary.start_ticks)
}

fn human_registered(
    env: &crate::task_env::TaskEnvironment,
    allowed: &[ProcessRecord],
) -> Result<()> {
    for process in &env.child_processes {
        let record = ProcessRecord {
            pid: process.pid,
            start_time: process.start_time,
        };
        human_registered_process(&record, allowed)?;
        for child in &process.descendants {
            human_registered_process(child, allowed)?;
        }
    }
    Ok(())
}

fn human_registered_process(record: &ProcessRecord, allowed: &[ProcessRecord]) -> Result<()> {
    require(
        !live(record)? || allowed.contains(record),
        "registered task writer or descendant is still alive",
    )
}

fn human_scan(
    env: &crate::task_env::TaskEnvironment,
    checkout: &Path,
    allowed: &[ProcessRecord],
    floor: u64,
) -> Result<()> {
    for (index, entry) in fs::read_dir("/proc")?.enumerate() {
        require(index < 262144, "human process scan budget exceeded")?;
        let entry = entry?;
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<i32>() else {
            continue;
        };
        human_process(env, checkout, allowed, floor, pid, &entry.path())?;
    }
    Ok(())
}

fn human_process(
    env: &crate::task_env::TaskEnvironment,
    checkout: &Path,
    allowed: &[ProcessRecord],
    floor: u64,
    pid: i32,
    path: &Path,
) -> Result<()> {
    if pid == std::process::id() as i32 || !same_uid_process(pid, path)? {
        return Ok(());
    }
    let Some((record, _, dead)) = stat(pid)? else {
        return Ok(());
    };
    // Like terminal cleanup, the birth boundary excludes pre-existing host
    // processes. This is managed writer admission, not a same-user OS lock.
    if dead || allowed.contains(&record) || record.start_time.is_some_and(|t| t < floor) {
        return Ok(());
    }
    human_candidate(env, checkout, &record, path)
}

fn human_candidate(
    env: &crate::task_env::TaskEnvironment,
    checkout: &Path,
    record: &ProcessRecord,
    path: &Path,
) -> Result<()> {
    let Some(marked) = stable_marker(record, &env.instance_id)? else {
        return Ok(());
    };
    require(
        !marked,
        "task-marked execution is still alive; no human writer transfer",
    )?;
    human_cwd(record.pid, checkout, path)
}

fn human_cwd(pid: i32, checkout: &Path, path: &Path) -> Result<()> {
    let Some(cwd) = writer_cwd(pid, path)? else {
        return Ok(());
    };
    require(
        !crate::task_env::paths_overlap(&cwd, checkout),
        "a process still uses the checkout; inspect its writer before handoff",
    )?;
    Ok(())
}

fn writer_cwd(pid: i32, path: &Path) -> Result<Option<PathBuf>> {
    match fs::read_link(path.join("cwd")) {
        Ok(cwd) => Ok(Some(cwd)),
        Err(error) if observation::missing_proc(&error) && gone(pid)? => Ok(None),
        Err(error) => Err(FlowError::io(format!(
            "cannot observe writer cwd for pid {pid}: {error}"
        ))),
    }
}

#[cfg(test)]
mod human_tests {
    use super::*;

    #[test]
    fn missing_cwd_for_a_live_process_cannot_prove_no_writer() {
        let directory = util::fs::test_tempdir();
        let pid = std::process::id() as i32;
        let error = human_cwd(pid, directory.path(), directory.path()).unwrap_err();
        assert!(error.message.contains(&format!("writer cwd for pid {pid}")));
    }
}

fn stable_marker(record: &ProcessRecord, instance: &str) -> Result<Option<bool>> {
    // Namespace/credential transitions can briefly deny environ reads. Retry
    // only that error, with an unchanged PID/start identity on every attempt.
    for attempt in 0..5 {
        match marker_observation(record, instance) {
            Err(error) if error.kind() == io::ErrorKind::PermissionDenied && attempt < 4 => {
                std::thread::sleep(Duration::from_millis(10));
            }
            result => {
                return result.map_err(|error| {
                    FlowError::io(format!(
                        "cannot observe cleanup marker for pid {} start {:?}: {error}",
                        record.pid, record.start_time
                    ))
                })
            }
        }
    }
    unreachable!("the final attempt always returns")
}

fn marker_observation(record: &ProcessRecord, instance: &str) -> io::Result<Option<bool>> {
    let observed = marker(record.pid, instance);
    let Some((after, _, dead)) = stat(record.pid)? else {
        return Ok(None);
    };
    if *record != after {
        return Err(invalid(
            "process identity changed during observation; retry",
        ));
    }
    if dead {
        return Ok(None);
    }
    observed
}

fn freeze_registered(plan: &mut CleanupState) -> Result<()> {
    for p in plan.pending_processes.clone() {
        let r = ProcessRecord {
            pid: p.pid,
            start_time: p.start_time,
        };
        if live(&r)? {
            add(&mut plan.terminal.as_mut().unwrap().targets, r)?;
        }
    }
    storage::save(plan)?;
    Ok(())
}

fn discover_targets(
    plan: &mut CleanupState,
    prior: &[ProcessRecord],
    seen: &BTreeMap<i32, Seen>,
) -> Result<bool> {
    let mut changed = false;
    for p in seen.values().filter(|p| p.marked) {
        changed |= add(
            &mut plan.terminal.as_mut().unwrap().targets,
            p.record.clone(),
        )?;
    }
    // Only a currently stopped, start-matched owned parent grants ancestry.
    for p in seen.values() {
        if prior.iter().any(|r| {
            seen.get(&r.pid).is_some_and(|parent| parent.record == *r) && p.parent == r.pid
        }) {
            changed |= add(
                &mut plan.terminal.as_mut().unwrap().targets,
                p.record.clone(),
            )?;
        }
    }
    Ok(changed)
}

fn needs_stop(r: &ProcessRecord) -> Result<bool> {
    if !live(r)? {
        return Ok(false);
    }
    let text = fs::read_to_string(format!("/proc/{}/stat", r.pid))?;
    let state = text
        .rsplit_once(')')
        .and_then(|(_, v)| v.split_whitespace().next())
        .ok_or_else(|| invalid("invalid stopped stat"))?;
    Ok(!matches!(state, "T" | "t" | "Z" | "X" | "x"))
}

fn await_exits(plan: &CleanupState) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let alive = terminal(plan)?
            .targets
            .iter()
            .map(live)
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .any(|b| b);
        if !alive {
            break;
        }
        require(
            Instant::now() < deadline,
            "owned process exit pending; cleanup will retry",
        )?;
        std::thread::sleep(Duration::from_millis(10));
    }
    Ok(())
}

fn verify_registered_stat(record: &ProcessRecord) -> Result<()> {
    let (after, _, dead) =
        stat(record.pid)?.ok_or_else(|| FlowError::guard("registered process disappeared"))?;
    require(
        !dead && after == *record,
        "registered process identity changed",
    )?;
    Ok(())
}

fn stop_targets(prior: &[ProcessRecord]) -> Result<()> {
    for r in prior {
        signals::signal(r, libc::SIGSTOP)?;
    }
    wait_stopped(prior)?;
    Ok(())
}

fn freeze_round(plan: &mut CleanupState) -> Result<bool> {
    let prior = terminal(plan)?.targets.clone();
    stop_targets(&prior)?;
    let seen = scan(plan)?;
    let changed = discover_targets(plan, &prior, &seen)?;
    storage::save(plan)?;
    Ok(changed)
}
#[cfg(test)]
pub(super) mod faults {
    use std::{
        cell::{Cell, RefCell},
        io,
    };
    thread_local! {static FAULT:RefCell<Option<(i32,bool,i32)>>=const{RefCell::new(None)};}
    thread_local! {static REMAINING:Cell<usize>=const{Cell::new(usize::MAX)};}
    thread_local! {static STOP_READY:RefCell<Option<std::path::PathBuf>>=const{RefCell::new(None)};}
    pub fn pause_after_freeze(path: std::path::PathBuf) {
        STOP_READY.with(|slot| *slot.borrow_mut() = Some(path));
    }
    pub(super) fn after_freeze() {
        STOP_READY.with(|slot| {
            if let Some(path) = slot.borrow().as_ref() {
                std::fs::write(path, b"stopped\n").unwrap();
                loop {
                    // SAFETY: only the private crash-test supervisor enables
                    // this checkpoint. Its parent kills and reaps it boundedly.
                    unsafe { libc::pause() };
                }
            }
        });
    }
    pub struct Guard;
    pub fn inject(pid: i32, environ: bool, errno: i32) -> Guard {
        FAULT.with(|f| {
            assert!(f.borrow().is_none());
            *f.borrow_mut() = Some((pid, environ, errno));
        });
        Guard
    }
    pub fn inject_times(pid: i32, environ: bool, errno: i32, count: usize) -> Guard {
        let guard = inject(pid, environ, errno);
        REMAINING.with(|remaining| remaining.set(count));
        guard
    }
    pub fn remaining() -> usize {
        REMAINING.with(Cell::get)
    }
    pub(super) fn check(pid: i32, environ: bool) -> io::Result<()> {
        FAULT.with(|f| match *f.borrow() {
            Some((p, e, errno)) if p == pid && e == environ => {
                REMAINING.with(|remaining| match remaining.get() {
                    0 => Ok(()),
                    count => {
                        if count != usize::MAX {
                            remaining.set(count - 1);
                        }
                        Err(io::Error::from_raw_os_error(errno))
                    }
                })
            }
            _ => Ok(()),
        })
    }
    impl Drop for Guard {
        fn drop(&mut self) {
            FAULT.with(|f| *f.borrow_mut() = None);
            REMAINING.with(|remaining| remaining.set(usize::MAX));
        }
    }
}

#[cfg(test)]
mod marker_tests {
    use super::*;

    struct Child(std::process::Child);
    impl Drop for Child {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    #[test]
    fn marker_retry_is_bounded_and_never_accepts_a_reused_pid() {
        let mut child = Child(
            std::process::Command::new("sleep")
                .arg("30")
                .spawn()
                .unwrap(),
        );
        let record = stat(child.0.id() as i32).unwrap().unwrap().0;
        {
            let _fault = faults::inject_times(record.pid, true, libc::EACCES, 6);
            assert!(stable_marker(&record, "unowned-test-marker").is_err());
            assert_eq!(faults::remaining(), 1, "at most five reads per observation");
            assert!(child.0.try_wait().unwrap().is_none());
        }
        let mut stale = record.clone();
        stale.start_time = Some(record.start_time.unwrap() + 1);
        let error = marker_observation(&stale, "unowned-test-marker").unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(child.0.try_wait().unwrap().is_none());
        let _fault = faults::inject(record.pid, false, libc::ESRCH);
        assert_eq!(
            stat(record.pid).unwrap_err().raw_os_error(),
            Some(libc::ESRCH)
        );
        child.0.kill().unwrap();
        child.0.wait().unwrap();
        assert!(stat(record.pid).unwrap().is_none());
        assert_eq!(stable_marker(&record, "unowned-test-marker").unwrap(), None);
    }
}
