//! Fallible proc directory scans. Individual PID visibility remains best effort.
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io;

use super::env::{parent_of, process_has_marker, process_start_time, ProcessRecord};

fn process_ids() -> io::Result<Vec<i32>> {
    #[cfg(test)]
    if let Some(result) = tests::injected_scan() {
        return result;
    }
    let entries = std::fs::read_dir("/proc").map_err(scan_error)?;
    collect_ids(entries.map(|entry| entry.map(|entry| entry.file_name())))
}

fn collect_ids(entries: impl IntoIterator<Item = io::Result<OsString>>) -> io::Result<Vec<i32>> {
    let mut ids = Vec::new();
    for entry in entries {
        let name = entry.map_err(scan_error)?;
        if let Some(pid) = name.to_str().and_then(|name| name.parse::<i32>().ok()) {
            ids.push(pid);
        }
    }
    Ok(ids)
}

fn scan_error(error: io::Error) -> io::Error {
    io::Error::new(
        error.kind(),
        format!("cannot scan /proc processes: {error}"),
    )
}

pub(crate) fn marked_processes(instance_id: &str) -> io::Result<Vec<ProcessRecord>> {
    let mut records = Vec::new();
    for pid in process_ids()? {
        if process_has_marker(pid, instance_id) {
            records.push(ProcessRecord {
                pid,
                start_time: process_start_time(pid),
            });
        }
    }
    records.sort_by_key(|record| record.pid);
    Ok(records)
}

pub(super) fn descendants_snapshot(pid: i32) -> io::Result<Vec<ProcessRecord>> {
    let mut parents = BTreeMap::new();
    for child in process_ids()? {
        if let Some(parent) = parent_of(child) {
            parents.insert(child, parent);
        }
    }
    Ok(descendants(pid, &parents))
}

fn descendants(pid: i32, parents: &BTreeMap<i32, i32>) -> Vec<ProcessRecord> {
    let mut found = Vec::new();
    let mut frontier = vec![pid];
    while let Some(current) = frontier.pop() {
        for (child, parent) in parents {
            if *parent == current
                && !found
                    .iter()
                    .any(|record: &ProcessRecord| record.pid == *child)
            {
                found.push(ProcessRecord {
                    pid: *child,
                    start_time: process_start_time(*child),
                });
                frontier.push(*child);
            }
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::delivery::{self, Executor, Observation};
    use crate::task_env::{self, DeliveryTarget};
    use serde_json::{json, Value};
    use std::cell::{Cell, RefCell};

    #[derive(Clone, Copy)]
    enum ScanFault {
        Open(i32),
        Entry(i32),
        Empty,
    }

    thread_local! {
        static SCAN_FAULT: RefCell<Option<(usize, ScanFault)>> = const { RefCell::new(None) };
    }

    // Synthetic errors scoped to the calling test thread. No host mount,
    // permissions, security policy or production environment hook is changed.
    pub(super) fn injected_scan() -> Option<io::Result<Vec<i32>>> {
        next_fault().map(|fault| match fault {
            ScanFault::Open(errno) => Err(scan_error(io::Error::from_raw_os_error(errno))),
            ScanFault::Entry(errno) => collect_ids([
                Ok(OsString::from(std::process::id().to_string())),
                Err(io::Error::from_raw_os_error(errno)),
            ]),
            ScanFault::Empty => collect_ids([]),
        })
    }

    fn next_fault() -> Option<ScanFault> {
        SCAN_FAULT.with(|slot| {
            let mut slot = slot.borrow_mut();
            let (skip, fault) = slot.as_mut()?;
            if *skip > 0 {
                *skip -= 1;
                return None;
            }
            Some(*fault)
        })
    }

    struct FaultGuard;

    impl FaultGuard {
        fn set(fault: ScanFault) -> Self {
            Self::after(fault, 0)
        }

        fn after(fault: ScanFault, skip: usize) -> Self {
            SCAN_FAULT.with(|slot| {
                assert!(slot.borrow().is_none());
                *slot.borrow_mut() = Some((skip, fault));
            });
            Self
        }
    }

    impl Drop for FaultGuard {
        fn drop(&mut self) {
            SCAN_FAULT.with(|slot| *slot.borrow_mut() = None);
        }
    }

    fn faults() -> [ScanFault; 4] {
        [
            ScanFault::Open(libc::EACCES),
            ScanFault::Open(libc::ENOENT),
            ScanFault::Open(libc::EIO),
            ScanFault::Entry(libc::EIO),
        ]
    }

    #[test]
    fn scan_errors_are_not_empty_or_partial_success() {
        for fault in faults() {
            let _guard = FaultGuard::set(fault);
            assert!(marked_processes("not-present").is_err());
            assert!(descendants_snapshot(std::process::id() as i32).is_err());
        }
        let _guard = FaultGuard::set(ScanFault::Empty);
        assert!(marked_processes("not-present").unwrap().is_empty());
        assert!(descendants_snapshot(std::process::id() as i32)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn enumeration_filters_names_and_traverses_descendants() {
        use std::os::unix::ffi::OsStringExt;
        let ids = collect_ids([
            Ok(OsString::from("12")),
            Ok(OsString::from("self")),
            Ok(OsString::from_vec(vec![0xff])),
            Ok(OsString::from("34")),
        ])
        .unwrap();
        assert_eq!(ids, vec![12, 34]);
        let parents = BTreeMap::from([(20, 10), (30, 20), (40, 1)]);
        let found = descendants(10, &parents);
        assert_eq!(
            found.iter().map(|record| record.pid).collect::<Vec<_>>(),
            vec![20, 30]
        );
    }

    fn cleanup_failure(fault: ScanFault) {
        let temp = crate::util::fs::test_tempdir();
        let _registry = task_env::test_env_root(&temp.path().join("registry"));
        let mut env = task_env::create_environment(
            "scan-fault",
            &temp.path().join("work"),
            vec![],
            &temp.path().join("tmp"),
        )
        .unwrap();
        let file = env.workspace_root.join("payload");
        std::fs::write(&file, "owned payload").unwrap();
        env.register_file(&file).unwrap();
        // No registered process can supply an independent blocker: the scan
        // failure itself must preserve these resources and the occupied slot.
        assert!(env.child_processes.is_empty());
        {
            let _guard = FaultGuard::set(fault);
            assert!(task_env::quiesce_environment(&mut env)
                .unwrap_err()
                .to_string()
                .contains("cannot scan /proc"));
            assert!(task_env::destroy_environment(&env).is_err());
            assert_retained(&env, &file);
            assert!(task_env::cleanup_failed_environment(&env.task_id).is_err());
            assert_retained(&env, &file);
        }
        task_env::cleanup_failed_environment(&env.task_id).unwrap();
        assert!(!env.workspace_root.exists());
        assert!(!env.temp_root.exists());
        assert!(!env.state_file.exists());
        assert!(!task_env::cleanup_state_path(&env.task_id).unwrap().exists());
        assert_eq!(task_env::occupied_slots().unwrap(), 0);
    }

    fn assert_retained(env: &task_env::TaskEnvironment, file: &std::path::Path) {
        let path = task_env::cleanup_state_path(&env.task_id).unwrap();
        let plan: task_env::CleanupState =
            serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        assert_eq!(plan.pending_files, vec![file.to_path_buf()]);
        assert_eq!(plan.pending_root_removals.len(), 2);
        assert!(plan.last_error.unwrap().contains("cannot scan /proc"));
        assert_eq!(std::fs::read_to_string(file).unwrap(), "owned payload");
        assert!(env.workspace_root.is_dir());
        assert!(env.temp_root.is_dir());
        assert!(env.state_file.exists());
        assert_eq!(task_env::occupied_slots().unwrap(), 2);
    }

    #[test]
    fn discovery_failure_preserves_cleanup_and_saved_plan_retry() {
        for fault in faults() {
            cleanup_failure(fault);
        }
    }

    #[test]
    fn descendant_scan_failure_keeps_discovered_child_running() {
        let temp = crate::util::fs::test_tempdir();
        let _registry = task_env::test_env_root(&temp.path().join("registry"));
        let mut env = task_env::create_environment(
            "descendant-scan",
            &temp.path().join("work"),
            vec![],
            &temp.path().join("tmp"),
        )
        .unwrap();
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .env(task_env::MARKER_ENV, &env.instance_id)
            .spawn()
            .unwrap();
        let outcome;
        let still_running;
        {
            // Initial marker scan succeeds; discovery of that target's
            // descendants fails before the first signal is allowed.
            let _guard = FaultGuard::after(ScanFault::Entry(libc::EIO), 1);
            outcome = task_env::quiesce_environment(&mut env);
            still_running = child.try_wait().unwrap().is_none();
        }
        let _ = child.kill();
        child.wait().unwrap();
        assert!(outcome
            .unwrap_err()
            .to_string()
            .contains("cannot scan /proc"));
        assert!(still_running);
        task_env::destroy_environment(&env).unwrap();
    }

    struct ObserveLocal {
        submissions: Cell<usize>,
    }

    impl Executor for ObserveLocal {
        fn preflight(&self, _: &Value) -> crate::Result<()> {
            Ok(())
        }
        fn execute(&self, _: &Value) -> crate::Result<()> {
            self.submissions.set(self.submissions.get() + 1);
            Ok(())
        }
        fn observe(&self, intent: &Value) -> crate::Result<Observation> {
            delivery::local::Local.observe(intent)
        }
    }

    #[test]
    fn delivery_scan_failure_stays_unknown_and_never_retries() {
        let temp = crate::util::fs::test_tempdir();
        let _registry = task_env::test_env_root(&temp.path().join("registry"));
        let target = temp.path().join("customer");
        let mut env = task_env::create_environment(
            "scan-delivery",
            &temp.path().join("work"),
            vec![DeliveryTarget::new(target.clone(), "formal".into())],
            &temp.path().join("tmp"),
        )
        .unwrap();
        let executor = ObserveLocal {
            submissions: Cell::new(0),
        };
        // Synthetic attempted-delivery journal, not evidence of a submission.
        // The missing endpoint would otherwise let Local report Absent.
        let intent = json!({"phase":"absent","instance":env.instance_id,"id":"scan-operation","request":{"adapter":"file","target":target}});
        for fault in faults() {
            env.delivery = intent.clone();
            env.persist().unwrap();
            let _guard = FaultGuard::set(fault);
            assert!(delivery::retry(&mut env, &executor)
                .unwrap_err()
                .to_string()
                .contains("cannot scan /proc"));
            assert_eq!(env.delivery, intent);
            let result = delivery::reconcile(&mut env, &executor).unwrap();
            assert_eq!(result["phase"], "unknown");
            assert!(result["problem"]
                .as_str()
                .unwrap()
                .contains("cannot scan /proc"));
            assert!(delivery::retry(&mut env, &executor).is_err());
            assert_eq!(executor.submissions.get(), 0);
            assert!(!target.exists());
        }
        let observed = delivery::reconcile(&mut env, &executor).unwrap();
        assert_eq!(observed["phase"], "absent");
        assert_eq!(executor.submissions.get(), 0);
        task_env::destroy_environment(&env).unwrap();
    }
}
