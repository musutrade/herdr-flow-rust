//! Two concrete customer paths; neither trusts process exit as delivery proof.
use super::*;
use std::{fs::OpenOptions, io::Write};

pub struct Local;
impl Executor for Local {
    fn preflight(&self, intent: &J) -> Result<()> {
        let spec = &intent["request"];
        match field(spec, "adapter")?.as_str() {
            "file" => ensure(
                intent["artifact"]["adapter"] == "file-sha256",
                "file delivery requires a digest",
            ),
            "git" => {
                ensure(
                    intent["artifact"]["adapter"] == "git",
                    "git delivery requires a fixed commit",
                )?;
                let reference = reference(spec)?;
                crate::pr::git(
                    Path::new(&field(spec, "target")?),
                    &["check-ref-format", &reference],
                )?;
                crate::pr::sha(model::get(spec, "expected"))?;
                Ok(())
            }
            _ => Err(FlowError::progress("unsupported customer executor")),
        }
    }
    fn execute(&self, intent: &J) -> Result<()> {
        match field(&intent["request"], "adapter")?.as_str() {
            "file" => copy_file(intent),
            "git" => push(intent),
            _ => Err(FlowError::progress("unsupported customer executor")),
        }
    }
    fn observe(&self, intent: &J) -> Result<Observation> {
        if operation_running(intent)? {
            return Ok(Observation::Unknown(
                "delivery subprocess is still running; do not replay".into(),
            ));
        }
        match field(&intent["request"], "adapter")?.as_str() {
            "file" => inspect_file(intent),
            "git" => inspect_git(intent),
            _ => Err(FlowError::progress("unsupported customer executor")),
        }
    }
}
fn reference(spec: &J) -> Result<String> {
    let value = field(spec, "ref")?;
    ensure(
        value.starts_with("refs/heads/"),
        "Git delivery requires an explicit branch ref",
    )?;
    Ok(value)
}
fn copy_file(intent: &J) -> Result<()> {
    let mut source = std::fs::File::open(field(&intent["artifact"], "location")?)?;
    let mut target = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(field(&intent["request"], "target")?)?;
    copy_and_sync(&mut source, &mut target)?;
    sync_parent(intent)
}
fn copy_and_sync(source: &mut std::fs::File, target: &mut std::fs::File) -> Result<()> {
    std::io::copy(source, target)?;
    target.flush()?;
    target.sync_all()?;
    Ok(())
}
fn sync_parent(intent: &J) -> Result<()> {
    std::fs::File::open(
        Path::new(&field(&intent["request"], "target")?)
            .parent()
            .unwrap(),
    )?
    .sync_all()?;
    Ok(())
}
fn inspect_file(intent: &J) -> Result<Observation> {
    let target = field(&intent["request"], "target")?;
    if !Path::new(&target).try_exists()? {
        return Ok(Observation::Absent);
    }
    ensure(
        std::fs::symlink_metadata(&target)?.file_type().is_file(),
        "formal file must be a regular file, not a symlink",
    )?;
    let artifact = json!({"adapter":"file-sha256","id":intent["artifact"]["id"],"location":target});
    match crate::progress::artifact::LocalArtifacts.verify(&artifact) {
        Ok(()) => Ok(Observation::Delivered(
            json!({"target":target,"digest":artifact["id"],"endpoint":intent["endpoint"]}),
        )),
        Err(error) if error.kind == crate::ErrorKind::Progress => {
            Ok(Observation::Failed(error.to_string()))
        }
        Err(error) => Err(error),
    }
}
fn push(intent: &J) -> Result<()> {
    let spec = &intent["request"];
    let target = field(spec, "target")?;
    let reference = reference(spec)?;
    let id = field(&intent["artifact"], "id")?;
    let expected = field(spec, "expected")?;
    let lease = format!("--force-with-lease={reference}:{expected}");
    let mapping = format!("{id}:{reference}");
    let mut command = git_writer(intent)?;
    let output = command
        .args(["push", &lease, "--", &target, &mapping])
        .output()?;
    ensure(
        output.status.success(),
        &format!(
            "Git delivery failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ),
    )?;
    Ok(())
}
const OPERATION: &str = "HERDR_DELIVERY_OPERATION";
fn git_writer(intent: &J) -> Result<std::process::Command> {
    use std::os::unix::process::CommandExt;
    let mut command = std::process::Command::new("git");
    command
        .arg("--no-replace-objects")
        .arg("-C")
        .arg(field(&intent["artifact"], "location")?)
        .env(crate::task_env::MARKER_ENV, field(intent, "instance")?)
        .env(OPERATION, field(intent, "id")?)
        .env("TMPDIR", field(intent, "temp_root")?)
        .process_group(0);
    let roots = writer_roots(intent)?;
    let policy = crate::task_env::PreparedConfinement::new(&roots)?;
    let parent = std::process::id() as i32;
    // SAFETY: paths and ruleset are prepared before fork. The callback only
    // performs process-control/confinement syscalls and uses scalar OS errors.
    // A supervisor dying before exec must not leave an unmarked writer.
    unsafe {
        command.pre_exec(move || protect_writer(parent, &policy));
    }
    Ok(command)
}
fn protect_writer(
    parent: i32,
    policy: &crate::task_env::PreparedConfinement,
) -> std::io::Result<()> {
    arm_death_signal()
        .and_then(|()| same_parent(parent))
        .and_then(|()| policy.apply())
}
fn arm_death_signal() -> std::io::Result<()> {
    // SAFETY: prctl takes only integer arguments for PR_SET_PDEATHSIG.
    if unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}
fn same_parent(parent: i32) -> std::io::Result<()> {
    // SAFETY: getppid has no caller preconditions.
    if unsafe { libc::getppid() } != parent {
        return Err(std::io::Error::from_raw_os_error(libc::ECHILD));
    }
    Ok(())
}
fn writer_roots(intent: &J) -> Result<Vec<std::path::PathBuf>> {
    Ok(vec![
        field(intent, "workspace_root")?.into(),
        field(intent, "temp_root")?.into(),
        field(&intent["request"], "target")?.into(),
    ])
}
fn operation_running(intent: &J) -> Result<bool> {
    let instance = field(intent, "instance")?;
    let marker = format!("{OPERATION}={}", field(intent, "id")?);
    for process in crate::task_env::marked_processes(&instance) {
        if operation_matches(process.pid, marker.as_bytes())? {
            return Ok(true);
        }
    }
    Ok(false)
}
fn operation_matches(pid: i32, marker: &[u8]) -> Result<bool> {
    match std::fs::read(format!("/proc/{pid}/environ")) {
        Ok(bytes) => Ok(bytes.split(|b| *b == 0).any(|entry| entry == marker)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}
fn inspect_git(intent: &J) -> Result<Observation> {
    let spec = &intent["request"];
    let target = field(spec, "target")?;
    let reference = reference(spec)?;
    let actual = crate::pr::git(Path::new(&target), &["rev-parse", "--verify", &reference])?;
    if json!(actual.trim()) == intent["artifact"]["id"] {
        return Ok(Observation::Delivered(
            json!({"repository":target,"ref":reference,
            "commit":intent["artifact"]["id"],"endpoint":intent["endpoint"]}),
        ));
    }
    if json!(actual.trim()) == spec["expected"] {
        return Ok(Observation::Absent);
    }
    Ok(Observation::Unknown(
        "target ref changed to another commit; inspect existing execution".into(),
    ))
}
