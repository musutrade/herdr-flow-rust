//! A new on-disk fence is useless to an older live executable. Admit only a
//! positively observed cohort using the invoking frozen executable, under EX.
use super::*;
use crate::task_env::ProcessRecord;
use std::fs::File;
use std::os::unix::fs::MetadataExt;

#[derive(PartialEq, Eq)]
struct Executable(u64, u64);

impl Executable {
    fn open(path: &Path) -> Result<Self> {
        let metadata = File::open(path)?.metadata()?;
        ensure(metadata.is_file(), "handoff executable is not a file")?;
        Ok(Self(metadata.dev(), metadata.ino()))
    }

    fn process(&self, record: &ProcessRecord) -> Result<bool> {
        ensure(
            record.start_time.is_some(),
            "handoff process start identity missing",
        )?;
        if !crate::task_env::observe_process_alive(record)? {
            return Ok(false);
        }
        let actual = Self::open(Path::new(&format!("/proc/{}/exe", record.pid)))?;
        ensure(actual == *self,
            "handoff requires the actual running frozen executable; older or unknown process cannot honor the fence")?;
        ensure(
            crate::task_env::observe_process_alive(record)?,
            "handoff process identity changed during executable observation",
        )?;
        Ok(true)
    }
}

pub(super) fn admit(env: &TaskEnvironment, state: &J, owner: &Owner) -> Result<()> {
    let binary = Executable::open(Path::new("/proc/self/exe"))?;
    let mut allowed = Vec::new();
    for (root, runtime) in
        super::super::inventory::runtimes(&state["tasks"][&owner.task_id], owner)?
    {
        controls(env, &root, &runtime, &binary, &mut allowed)?;
    }
    watch(
        &crate::queue::artifacts(&owner.queue_root)?,
        &binary,
        &mut allowed,
    )?;
    // Reuse the existing fallible registered/descendant/marker/checkout scan.
    // No unknown task process is exempted merely because a journal omitted it.
    crate::task_env::terminal::human_quiescence(env, &env.workspace_root.join("checkout"), &allowed)
}

fn controls(
    env: &TaskEnvironment,
    root: &Path,
    runtime: &J,
    binary: &Executable,
    allowed: &mut Vec<ProcessRecord>,
) -> Result<()> {
    driver(binary, runtime)?;
    add(binary, process(runtime, "start_time")?, allowed)?;
    frontends(env, root, runtime, binary, allowed)?;
    let owner: Owner = serde_json::from_value(runtime["lifecycle_owner"].clone())?;
    watches(root, &owner, binary, allowed)
}

fn driver(binary: &Executable, runtime: &J) -> Result<()> {
    let path = match runtime.get("driver_binary") {
        Some(_) => super::super::path(runtime, "driver_binary")?,
        None => std::env::current_exe()?,
    };
    ensure(
        Executable::open(&path)? == *binary,
        "handoff driver restart path is not the invoking frozen executable",
    )
}

fn process(value: &J, ticks: &str) -> Result<ProcessRecord> {
    let pid = value["pid"]
        .as_u64()
        .and_then(|p| i32::try_from(p).ok())
        .filter(|p| *p > 1)
        .ok_or_else(|| FlowError::guard("handoff process PID missing"))?;
    let start_time = value[ticks]
        .as_u64()
        .or_else(|| value[ticks].as_str().and_then(|s| s.parse().ok()))
        .ok_or_else(|| FlowError::guard("handoff process start identity missing"))?;
    Ok(ProcessRecord {
        pid,
        start_time: Some(start_time),
    })
}

fn add(binary: &Executable, record: ProcessRecord, allowed: &mut Vec<ProcessRecord>) -> Result<()> {
    binary.process(&record)?;
    allowed.push(record);
    Ok(())
}

fn frontends(
    env: &TaskEnvironment,
    root: &Path,
    runtime: &J,
    binary: &Executable,
    allowed: &mut Vec<ProcessRecord>,
) -> Result<()> {
    // The validated snapshot orders native child then Flow wrapper. The native
    // child is exempt only behind this exact verified wrapper and its binding.
    let (presentation, processes) = tui::retirement_snapshot(env, root)?;
    if presentation.is_none() {
        return Ok(());
    }
    frontend_identity(runtime, &processes, binary)?;
    allowed.extend(processes);
    Ok(())
}

fn frontend_identity(runtime: &J, processes: &[ProcessRecord], binary: &Executable) -> Result<()> {
    ensure(
        processes.len() == 2,
        "handoff frontend process proof incomplete",
    )?;
    let frontend = Executable::open(&super::super::path(&runtime["native_tui"], "binary")?)?;
    let child_alive = frontend.process(&processes[0])?;
    let wrapper_alive = binary.process(&processes[1])?;
    ensure(
        !child_alive || wrapper_alive,
        "native frontend has no verified Flow wrapper",
    )?;
    Ok(())
}

fn watches(
    root: &Path,
    owner: &Owner,
    binary: &Executable,
    allowed: &mut Vec<ProcessRecord>,
) -> Result<()> {
    watch(root, binary, allowed)?;
    receiver_watch(root, owner, binary, allowed)?;
    observer_watch(root, owner, binary, allowed)
}

fn receiver_watch(
    root: &Path,
    owner: &Owner,
    binary: &Executable,
    allowed: &mut Vec<ProcessRecord>,
) -> Result<()> {
    if let Some(receiver) =
        util::read_state_json_bounded_if_exists(root, "receiver.json", RECORD_BYTES)?
    {
        let guard = super::super::path(&receiver, "guard_dir")?;
        super::super::anchor::protected(&guard, owner)?;
        watch(&guard, binary, allowed)?;
    }
    Ok(())
}

fn observer_watch(
    root: &Path,
    owner: &Owner,
    binary: &Executable,
    allowed: &mut Vec<ProcessRecord>,
) -> Result<()> {
    let observer = root.join("recovery-observer");
    match std::fs::symlink_metadata(&observer) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
        Ok(_) => {
            super::super::anchor::protected(&observer, owner)?;
            watch(&observer, binary, allowed)
        }
    }
}

fn watch(root: &Path, binary: &Executable, allowed: &mut Vec<ProcessRecord>) -> Result<()> {
    if let Some(watch) = util::read_state_json_bounded_if_exists(root, "watch.json", RECORD_BYTES)?
    {
        add(binary, process(&watch, "start_ticks")?, allowed)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct OldProcess(std::process::Child);

    impl OldProcess {
        fn start() -> Self {
            Self(
                std::process::Command::new("/usr/bin/sleep")
                    .arg("60")
                    .spawn()
                    .unwrap(),
            )
        }
        fn record(&self) -> ProcessRecord {
            let pid = self.0.id() as i32;
            ProcessRecord {
                pid,
                start_time: crate::task_env::process_start_time(pid),
            }
        }
    }
    impl Drop for OldProcess {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    fn current() -> ProcessRecord {
        let pid = std::process::id() as i32;
        ProcessRecord {
            pid,
            start_time: crate::task_env::process_start_time(pid),
        }
    }

    #[test]
    fn frontend_exemption_requires_both_exact_images_and_a_live_new_wrapper() {
        let binary = Executable::open(Path::new("/proc/self/exe")).unwrap();
        let mut child = OldProcess::start();
        let native = child.record();
        let wrapper = current();
        let runtime = json!({"native_tui":{"binary":"/usr/bin/sleep"}});
        frontend_identity(&runtime, &[native.clone(), wrapper.clone()], &binary).unwrap();
        assert!(frontend_identity(&runtime, &[native.clone(), native.clone()], &binary).is_err());
        let wrong_binary = json!({"native_tui":{"binary":std::env::current_exe().unwrap()}});
        assert!(
            frontend_identity(&wrong_binary, &[native.clone(), wrapper.clone()], &binary).is_err()
        );
        assert!(frontend_identity(&runtime, &[], &binary).is_err());
        child.0.kill().unwrap();
        child.0.wait().unwrap();
        assert!(!binary.process(&native).unwrap());
        assert!(
            frontend_identity(&wrong_binary, &[wrapper, native], &binary)
                .unwrap_err()
                .message
                .contains("no verified Flow wrapper")
        );
    }

    #[test]
    fn watcher_claims_cannot_replace_actual_executable_or_start_identity() {
        let temp = util::fs::test_tempdir();
        let root = temp.path().join("guard");
        util::protected::runtime_namespace(&root).unwrap();
        let binary = Executable::open(Path::new("/proc/self/exe")).unwrap();
        let mut allowed = Vec::new();
        let record = current();
        let mut value =
            json!({"pid":record.pid,"start_ticks":record.start_time.unwrap().to_string()});
        util::atomic_write_json(&root.join("watch.json"), &value).unwrap();
        watch(&root, &binary, &mut allowed).unwrap();
        assert_eq!(allowed, vec![record]);
        let old = OldProcess::start();
        value["pid"] = json!(old.record().pid);
        value["start_ticks"] = json!(old.record().start_time);
        util::atomic_write_json(&root.join("watch.json"), &value).unwrap();
        assert!(watch(&root, &binary, &mut allowed).is_err());
        assert_eq!(allowed.len(), 1);
        value["start_ticks"] = J::Null;
        util::atomic_write_json(&root.join("watch.json"), &value).unwrap();
        assert!(watch(&root, &binary, &mut allowed).is_err());
    }
}
