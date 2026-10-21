//! Exclusive broker lifetime and task-bound stale socket recovery.
use super::*;
use std::fs;
use std::os::unix::fs::{FileTypeExt, MetadataExt};

pub(super) struct Owner {
    pub listener: UnixListener,
    root: PathBuf,
    instance: String,
    socket: (u64, u64),
    lock: util::FileLock,
}

impl Owner {
    pub fn acquire(root: &Path) -> Result<Self> {
        let instance = task_instance(root)?;
        let lock = util::FileLock::acquire(&root.join("broker.lock"), false)?
            .ok_or_else(|| FlowError::guard("broker owner is still active"))?;
        recover(root, &instance)?;
        let listener = UnixListener::bind(root.join("broker.sock"))?;
        let socket = socket_identity(&root.join("broker.sock"))?;
        let owner = Self {
            listener,
            root: root.to_path_buf(),
            instance,
            socket,
            lock,
        };
        owner.listener.set_nonblocking(true)?;
        owner.record_identity()?;
        Ok(owner)
    }

    fn record_identity(&self) -> Result<()> {
        let mut identity = util::process::process_identity(std::process::id())?;
        identity["instance"] = json!(self.instance);
        identity["dev"] = json!(self.socket.0);
        identity["ino"] = json!(self.socket.1);
        util::atomic_write_json(&self.root.join("broker-owner.json"), &identity)?;
        util::atomic_write_json(
            &self.root.join("broker-status.json"),
            &json!({"phase":"running", "pid":std::process::id(), "instance":self.instance, "start_ticks":identity["start_ticks"]}),
        )?;
        Ok(())
    }

    pub fn verify(&self) -> Result<()> {
        self.verify_lock()?;
        let runtime = util::read_state_json(&self.root, "runtime.json")?;
        ensure(
            runtime["instance"] == self.instance,
            "broker task instance changed",
        )?;
        ensure(
            socket_identity(&self.root.join("broker.sock"))? == self.socket,
            "broker socket ownership changed",
        )
    }
    fn verify_lock(&self) -> Result<()> {
        let held = fs::metadata(format!("/proc/self/fd/{}", self.lock.as_raw_fd()))?;
        let named = fs::symlink_metadata(self.root.join("broker.lock"))?;
        ensure(
            named.file_type().is_file() && (held.dev(), held.ino()) == (named.dev(), named.ino()),
            "broker ownership lock changed",
        )
    }
}

impl Drop for Owner {
    fn drop(&mut self) {
        let path = self.root.join("broker.sock");
        // Keep the exclusive lock through cleanup; never remove a replacement.
        let _ = remove_owned_socket(&path, self.socket);
    }
}

fn task_instance(root: &Path) -> Result<String> {
    let runtime = util::read_state_json(root, "runtime.json")?;
    let instance = required(&runtime, "instance")?;
    ensure(
        std::env::var(crate::task_env::MARKER_ENV).ok().as_deref() == Some(instance.as_str()),
        "broker requires its task environment identity",
    )?;
    Ok(instance)
}

fn socket_identity(path: &Path) -> Result<(u64, u64)> {
    let metadata = fs::symlink_metadata(path)?;
    ensure(
        metadata.file_type().is_socket() && metadata.uid() == util::process::current_uid(),
        "broker socket must be an owned real socket",
    )?;
    Ok((metadata.dev(), metadata.ino()))
}

fn recover(root: &Path, instance: &str) -> Result<()> {
    let path = root.join("broker.sock");
    match fs::symlink_metadata(&path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
        Ok(_) => {}
    }
    let socket = socket_identity(&path)?;
    validate_previous_owner(root, instance, socket)?;
    require_no_listener(&path)?;
    remove_owned_socket(&path, socket)
}
fn remove_owned_socket(path: &Path, socket: (u64, u64)) -> Result<()> {
    ensure(
        socket_identity(path)? == socket,
        "broker socket ownership changed before removal",
    )?;
    fs::remove_file(path)?;
    Ok(())
}

fn validate_previous_owner(root: &Path, instance: &str, socket: (u64, u64)) -> Result<()> {
    let owner = util::read_state_json(root, "broker-owner.json")?;
    ensure(
        owner["instance"] == instance && owner["dev"] == socket.0 && owner["ino"] == socket.1,
        "stale broker socket identity does not match task owner",
    )?;
    require_dead(&owner)?;
    Ok(())
}

fn require_no_listener(path: &Path) -> Result<()> {
    // A listener from an older version without our lock must not be displaced.
    match util::unix_socket::connect(path) {
        Ok(_) => return Err(FlowError::guard("broker socket still has a live listener")),
        Err(error) if error.kind() == std::io::ErrorKind::ConnectionRefused => {}
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

fn require_dead(owner: &J) -> Result<()> {
    let (pid, start) = process_record(owner)?;
    match fs::read_to_string(format!("/proc/{pid}/stat")) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
        Ok(stat) => {
            let observed = stat
                .rsplit_once(')')
                .and_then(|(_, rest)| rest.split_whitespace().nth(19))
                .ok_or_else(|| FlowError::guard("cannot verify broker process identity"))?;
            ensure(observed != start, "previous broker process is still alive")
        }
    }
}

fn process_record(owner: &J) -> Result<(u64, String)> {
    let pid = owner["pid"]
        .as_u64()
        .filter(|pid| *pid > 0 && *pid <= i32::MAX as u64)
        .ok_or_else(|| FlowError::guard("invalid broker owner pid"))?;
    let start = required(owner, "start_ticks")?;
    ensure(
        start.parse::<u64>().is_ok(),
        "invalid broker owner start_ticks",
    )?;
    Ok((pid, start))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn process_identity_distinguishes_live_reused_dead_and_invalid_records() {
        let current = util::process::process_identity(std::process::id()).unwrap();
        assert!(require_dead(&current)
            .unwrap_err()
            .message
            .contains("still alive"));
        let mut reused = current.clone();
        reused["start_ticks"] = json!("0");
        require_dead(&reused).unwrap();
        for invalid in [
            json!({}),
            json!({"pid":0,"start_ticks":"1"}),
            json!({"pid":1,"start_ticks":"bad"}),
        ] {
            assert!(require_dead(&invalid).is_err());
        }
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let identity = util::process::process_identity(child.id()).unwrap();
        child.kill().unwrap();
        child.wait().unwrap();
        require_dead(&identity).unwrap();
    }
}
