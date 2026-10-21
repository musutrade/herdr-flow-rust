//! Bounded, test-only metadata inventory for task-owned and outside resources.
//!
//! This collector never mutates or deletes observed resources. It records only
//! paths/metadata, process identities, queue keys/counts and bounded disk
//! observations. It deliberately does not read file contents, config or auth.
use herdr_flow::queue;
use herdr_flow::task_env::{self, WorktreeRegistration};
use serde_json::{json, Value as J};
use std::ffi::CString;
use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;

const MAX_FILES: usize = 4096;
const MAX_DEPTH: usize = 32;
const MAX_PROCESSES: usize = 256;
const MAX_FDS: usize = 256;
const MAX_QUEUE_KEYS: usize = 128;

pub struct InventorySpec<'a> {
    pub base: &'a Path,
    pub queue: &'a Path,
    pub task_id: &'a str,
    pub instance_id: &'a str,
    pub workspace_root: &'a Path,
    pub temp_root: &'a Path,
    pub runtime_roots: &'a [PathBuf],
    pub worktrees: &'a [WorktreeRegistration],
    pub open_files: &'a [PathBuf],
    pub excluded: &'a [PathBuf],
}

pub fn collect(spec: &InventorySpec<'_>) -> J {
    let mut errors = Vec::new();
    let allowed = allowed_roots(spec);
    let registry = registry_observation(spec, &mut errors);
    let state = state_observation(spec.task_id, &mut errors);
    let cleanup = cleanup_observation(spec.task_id, &mut errors);
    let queue_value = queue_observation(spec.queue, spec.task_id, &mut errors);
    let processes = process_observation(spec, &allowed, &mut errors);
    let worktrees = worktree_observation(spec.worktrees, &mut errors);
    let open_files = spec
        .open_files
        .iter()
        .map(|path| path_observation(path, "open_file", &mut errors))
        .collect::<Vec<_>>();
    let runtime_roots = spec
        .runtime_roots
        .iter()
        .map(|path| path_observation(path, "runtime_root", &mut errors))
        .collect::<Vec<_>>();
    let excluded = spec
        .excluded
        .iter()
        .map(|path| path_observation(path, "excluded", &mut errors))
        .collect::<Vec<_>>();
    let base = base_observation(spec.base, spec.excluded, &mut errors);
    let free_bytes = match free_bytes(spec.base) {
        Ok(bytes) => json!(bytes),
        Err(error) => {
            errors.push(format!("statvfs {}: {error}", spec.base.display()));
            J::Null
        }
    };
    json!({
        "task": spec.task_id,
        "instance": spec.instance_id,
        "workspace_root": spec.workspace_root.display().to_string(),
        "temp_root": spec.temp_root.display().to_string(),
        "queue": spec.queue.display().to_string(),
        "base": spec.base.display().to_string(),
        "registry": registry,
        "state": state,
        "cleanup": cleanup,
        "queue_entries": queue_value,
        "processes": processes,
        "worktrees": worktrees,
        "open_files": open_files,
        "runtime_roots": runtime_roots,
        "excluded": excluded,
        "base_usage": base,
        "free_bytes_filesystem": free_bytes,
        "limits": {
            "max_files": MAX_FILES,
            "max_depth": MAX_DEPTH,
            "max_processes": MAX_PROCESSES,
            "max_fds": MAX_FDS,
            "max_queue_keys": MAX_QUEUE_KEYS
        },
        "errors": errors
    })
}

fn allowed_roots(spec: &InventorySpec<'_>) -> Vec<PathBuf> {
    let mut roots = vec![
        spec.workspace_root.to_path_buf(),
        spec.temp_root.to_path_buf(),
        spec.queue.to_path_buf(),
    ];
    roots.extend(spec.runtime_roots.iter().cloned());
    roots
}

fn path_observation(path: &Path, role: &str, errors: &mut Vec<String>) -> J {
    match fs::symlink_metadata(path) {
        Ok(metadata) => json!({
            "role": role,
            "path": path.display().to_string(),
            "exists": true,
            "kind": kind(&metadata),
            "dev": metadata.dev(),
            "ino": metadata.ino(),
            "len": metadata.len(),
            "blocks": metadata.blocks()
        }),
        Err(error) if error.kind() == io::ErrorKind::NotFound => json!({
            "role": role,
            "path": path.display().to_string(),
            "exists": false
        }),
        Err(error) => {
            errors.push(format!("{role} metadata {}: {error}", path.display()));
            json!({
                "role": role,
                "path": path.display().to_string(),
                "exists": false,
                "error": error.to_string()
            })
        }
    }
}

fn kind(metadata: &fs::Metadata) -> &'static str {
    if metadata.file_type().is_symlink() {
        "symlink"
    } else if metadata.is_file() {
        "file"
    } else if metadata.is_dir() {
        "dir"
    } else {
        "other"
    }
}

fn registry_observation(spec: &InventorySpec<'_>, errors: &mut Vec<String>) -> J {
    let registry = match task_env::load_registry() {
        Ok(registry) => registry,
        Err(error) => {
            errors.push(format!("registry load: {error}"));
            return json!({"error": error.to_string()});
        }
    };
    let entry = registry.get(spec.task_id).map(|entry| {
        json!({
            "task": entry.task_id,
            "instance": entry.instance_id,
            "workspace_root": entry.workspace_root.display().to_string(),
            "temp_root": entry.temp_root.display().to_string(),
            "state_file": entry.state_file.display().to_string(),
            "claimed_roots": entry.claimed_roots().iter().map(|p| p.display().to_string()).collect::<Vec<_>>()
        })
    });
    let slots = match task_env::occupied_slots() {
        Ok(slots) => json!(slots),
        Err(error) => {
            errors.push(format!("occupied_slots: {error}"));
            J::Null
        }
    };
    json!({
        "registered": registry.contains(spec.task_id),
        "entry": entry,
        "occupied_slots": slots
    })
}

fn state_observation(task_id: &str, errors: &mut Vec<String>) -> J {
    match task_env::environment_state_path(task_id) {
        Ok(path) => path_observation(&path, "environment_state", errors),
        Err(error) => {
            errors.push(format!("environment_state_path: {error}"));
            json!({"role":"environment_state","error":error.to_string()})
        }
    }
}

fn cleanup_observation(task_id: &str, errors: &mut Vec<String>) -> J {
    match task_env::cleanup_state_path(task_id) {
        Ok(path) => path_observation(&path, "cleanup_plan", errors),
        Err(error) => {
            errors.push(format!("cleanup_state_path: {error}"));
            json!({"role":"cleanup_plan","error":error.to_string()})
        }
    }
}

fn queue_observation(queue_root: &Path, task_id: &str, errors: &mut Vec<String>) -> J {
    let state = match queue::load_queue(queue_root) {
        Ok(state) => state,
        Err(error) => {
            errors.push(format!("queue load {}: {error}", queue_root.display()));
            return json!({"error": error.to_string()});
        }
    };
    let tasks = state["tasks"].as_object().cloned().unwrap_or_default();
    let mut owned_tasks = Vec::new();
    let mut unrelated_tasks = Vec::new();
    for (key, task) in &tasks {
        let owned = key == task_id || task["lineage"]["requirement"] == task_id;
        if owned {
            owned_tasks.push(key.clone());
        } else {
            unrelated_tasks.push(key.clone());
        }
    }
    let owned_task_set: std::collections::BTreeSet<_> = owned_tasks.iter().cloned().collect();
    let waits = state["waits"].as_object().cloned().unwrap_or_default();
    let owned_waits = waits
        .iter()
        .filter(|(_, wait)| {
            wait["task"] == task_id || owned_task_set.contains(wait["task"].as_str().unwrap_or(""))
        })
        .map(|(key, _)| key.clone())
        .collect::<Vec<_>>();
    let unrelated_waits = waits
        .keys()
        .filter(|key| !owned_waits.contains(*key))
        .cloned()
        .collect::<Vec<_>>();
    let events = state["events"].as_object().cloned().unwrap_or_default();
    let owned_events = events
        .iter()
        .filter(|(_, event)| {
            event["task"] == task_id
                || owned_task_set.contains(event["task"].as_str().unwrap_or(""))
        })
        .map(|(key, _)| key.clone())
        .collect::<Vec<_>>();
    let unrelated_events = events
        .keys()
        .filter(|key| !owned_events.contains(*key))
        .cloned()
        .collect::<Vec<_>>();
    let owned_event_set: std::collections::BTreeSet<_> = owned_events.iter().cloned().collect();
    let deliveries = state["deliveries"].as_object().cloned().unwrap_or_default();
    let mut owned_deliveries = Vec::new();
    let mut unrelated_deliveries = Vec::new();
    for (key, delivery) in &deliveries {
        let owned = delivery["events"].as_array().is_some_and(|events| {
            events.iter().any(|event| {
                event
                    .as_str()
                    .is_some_and(|id| owned_event_set.contains(id))
            })
        });
        if owned {
            owned_deliveries.push(key.clone());
        } else {
            unrelated_deliveries.push(key.clone());
        }
    }
    let truncate = |mut values: Vec<String>| {
        values.sort();
        if values.len() > MAX_QUEUE_KEYS {
            values.truncate(MAX_QUEUE_KEYS);
        }
        values
    };
    json!({
        "owned": {
            "tasks": truncate(owned_tasks),
            "waits": truncate(owned_waits),
            "events": truncate(owned_events),
            "deliveries": truncate(owned_deliveries)
        },
        "unrelated": {
            "tasks": truncate(unrelated_tasks),
            "waits": truncate(unrelated_waits),
            "events": truncate(unrelated_events),
            "deliveries": truncate(unrelated_deliveries)
        }
    })
}

fn process_observation(
    spec: &InventorySpec<'_>,
    allowed: &[PathBuf],
    errors: &mut Vec<String>,
) -> J {
    let records = task_env::marked_processes(spec.instance_id);
    let truncated = records.len() > MAX_PROCESSES;
    let records = records
        .into_iter()
        .take(MAX_PROCESSES)
        .map(|record| {
            let pid = record.pid;
            let before = proc_start(pid);
            let mut before_start = None;
            let mut record_json = json!({
                "pid": pid,
                "pgid": record.pgid,
                "expected_start": record.start_time,
                "start_before": before.as_ref().ok().cloned().flatten(),
                "status": "unknown",
                "fds": [],
                "fd_errors": [],
                "cwd": J::Null,
                "complete": false
            });
            match before {
                Err(error) => {
                    errors.push(format!("process {pid} start before: {error}"));
                    record_json["status"] = json!("error");
                    record_json["error"] = json!(error);
                    return record_json;
                }
                Ok(None) => {
                    record_json["status"] = json!("gone");
                    return record_json;
                }
                Ok(Some(start)) => {
                    if record.start_time.is_some() && Some(start) != record.start_time {
                        record_json["status"] = json!("identity_changed");
                        return record_json;
                    }
                    before_start = Some(start);
                    record_json["status"] = json!("sampled");
                }
            }
            let fd_dir = format!("/proc/{pid}/fd");
            match fs::read_dir(&fd_dir) {
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    record_json["status"] = json!("gone");
                    return record_json;
                }
                Err(error) => {
                    errors.push(format!("process {pid} fd dir: {error}"));
                    record_json["status"] = json!("error");
                    record_json["error"] = json!(error.to_string());
                }
                Ok(entries) => {
                    let mut fds = Vec::new();
                    let mut fd_errors = Vec::new();
                    for entry in entries.take(MAX_FDS) {
                        let entry = match entry {
                            Ok(entry) => entry,
                            Err(error) => {
                                fd_errors.push(format!("fd entry: {error}"));
                                continue;
                            }
                        };
                        let fd = entry.file_name().to_string_lossy().to_string();
                        match fs::read_link(format!("/proc/{pid}/fd/{fd}")) {
                            Ok(target) => {
                                let class = if target.is_absolute() {
                                    path_class(&target, allowed, spec.excluded)
                                } else {
                                    "kernel"
                                };
                                fds.push(json!({
                                    "fd": fd,
                                    "target": target.display().to_string(),
                                    "class": class
                                }));
                            }
                            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                            Err(error) => {
                                fd_errors.push(format!("fd {fd}: {error}"));
                            }
                        }
                    }
                    record_json["fds"] = json!(fds);
                    record_json["fd_errors"] = json!(fd_errors);
                }
            }
            match fs::read_link(format!("/proc/{pid}/cwd")) {
                Ok(cwd) => {
                    record_json["cwd"] = json!({
                        "target": cwd.display().to_string(),
                        "class": path_class(&cwd, allowed, spec.excluded)
                    });
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => {
                    errors.push(format!("process {pid} cwd: {error}"));
                    record_json["cwd"] = json!({"error": error.to_string()});
                }
            }
            let after = proc_start(pid);
            match after {
                Err(error) => {
                    errors.push(format!("process {pid} start after: {error}"));
                    record_json["status"] = json!("error_after");
                    record_json["error"] = json!(error);
                }
                Ok(None) => {
                    record_json["status"] = json!("gone_after_sampling");
                }
                Ok(Some(after_start)) => {
                    let complete = before_start.is_some() && before_start == Some(after_start);
                    record_json["start_after"] = json!(after_start);
                    record_json["complete"] = json!(complete);
                    if !complete {
                        record_json["status"] = json!("identity_changed");
                    }
                }
            }
            record_json
        })
        .collect::<Vec<_>>();
    json!({
        "instance": spec.instance_id,
        "count": records.len(),
        "truncated": truncated,
        "records": records
    })
}

fn proc_start(pid: i32) -> Result<Option<u64>, String> {
    match fs::metadata(format!("/proc/{pid}")) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.to_string()),
        Ok(_) => task_env::process_start_time(pid)
            .map(Some)
            .ok_or_else(|| "process start time is unreadable".to_string()),
    }
}

fn path_class(path: &Path, allowed: &[PathBuf], excluded: &[PathBuf]) -> &'static str {
    if excluded
        .iter()
        .any(|excluded| path == excluded || path.starts_with(excluded))
    {
        "excluded"
    } else if allowed.iter().any(|root| path.starts_with(root)) {
        "task_owned"
    } else {
        "outside_unknown"
    }
}

fn worktree_observation(worktrees: &[WorktreeRegistration], errors: &mut Vec<String>) -> Vec<J> {
    let mut seen = std::collections::BTreeSet::new();
    worktrees
        .iter()
        .map(|worktree| {
            let path = path_observation(&worktree.path, "worktree", errors);
            let repo = worktree
                .repo
                .as_ref()
                .map(|repo| path_observation(repo, "worktree_repo", errors));
            let listing = worktree.repo.as_ref().and_then(|repo| {
                if seen.insert(repo.clone()) {
                    match Command::new("git")
                        .arg("-C")
                        .arg(repo)
                        .args(["worktree", "list", "--porcelain"])
                        .output()
                    {
                        Ok(output) if output.status.success() => Some(json!({
                            "repo": repo.display().to_string(),
                            "entries": String::from_utf8_lossy(&output.stdout)
                                .lines()
                                .filter(|line| line.starts_with("worktree "))
                                .take(64)
                                .collect::<Vec<_>>()
                        })),
                        Ok(output) => {
                            errors.push(format!(
                                "git worktree list {}: {}",
                                repo.display(),
                                String::from_utf8_lossy(&output.stderr).trim()
                            ));
                            None
                        }
                        Err(error) => {
                            errors.push(format!("git worktree list {}: {error}", repo.display()));
                            None
                        }
                    }
                } else {
                    None
                }
            });
            json!({
                "path": path,
                "repo": repo,
                "git_worktree_list": listing
            })
        })
        .collect()
}

fn base_observation(base: &Path, excluded: &[PathBuf], errors: &mut Vec<String>) -> J {
    #[derive(Default)]
    struct Usage {
        entries: usize,
        files: usize,
        dirs: usize,
        symlinks: usize,
        bytes: u64,
        blocks: u64,
        excluded_files: usize,
        excluded_bytes: u64,
        excluded_blocks: u64,
        truncated: bool,
    }
    fn walk(
        path: &Path,
        depth: usize,
        excluded: &[PathBuf],
        usage: &mut Usage,
        errors: &mut Vec<String>,
    ) {
        if usage.entries >= MAX_FILES || depth > MAX_DEPTH {
            usage.truncated = true;
            return;
        }
        let metadata = match fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return,
            Err(error) => {
                errors.push(format!("base metadata {}: {error}", path.display()));
                return;
            }
        };
        usage.entries += 1;
        let is_excluded = excluded.contains(&path.to_path_buf());
        if metadata.is_dir() && !metadata.file_type().is_symlink() {
            usage.dirs += 1;
            if is_excluded {
                return;
            }
            match fs::read_dir(path) {
                Ok(entries) => {
                    for entry in entries {
                        match entry {
                            Ok(entry) => {
                                if usage.entries >= MAX_FILES {
                                    usage.truncated = true;
                                    break;
                                }
                                walk(&entry.path(), depth + 1, excluded, usage, errors);
                            }
                            Err(error) => {
                                errors.push(format!("base entry {}: {error}", path.display()));
                            }
                        }
                    }
                }
                Err(error) => errors.push(format!("base read_dir {}: {error}", path.display())),
            }
        } else {
            if metadata.file_type().is_symlink() {
                usage.symlinks += 1;
            } else {
                usage.files += 1;
            }
            usage.bytes += metadata.len();
            usage.blocks += metadata.blocks();
            if is_excluded {
                usage.excluded_files += 1;
                usage.excluded_bytes += metadata.len();
                usage.excluded_blocks += metadata.blocks();
            }
        }
    }
    let mut usage = Usage::default();
    walk(base, 0, excluded, &mut usage, errors);
    json!({
        "path": base.display().to_string(),
        "entries": usage.entries,
        "files": usage.files,
        "dirs": usage.dirs,
        "symlinks": usage.symlinks,
        "bytes": usage.bytes,
        "allocated_blocks_512": usage.blocks,
        "excluded_files": usage.excluded_files,
        "excluded_bytes": usage.excluded_bytes,
        "excluded_allocated_blocks_512": usage.excluded_blocks,
        "truncated": usage.truncated
    })
}

fn free_bytes(path: &Path) -> Result<u64, String> {
    let path = CString::new(path.as_os_str().as_bytes()).map_err(|error| error.to_string())?;
    let mut stat = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    // SAFETY: path is NUL-terminated and stat points to writable storage.
    let result = unsafe { libc::statvfs(path.as_ptr(), stat.as_mut_ptr()) };
    if result != 0 {
        return Err(io::Error::last_os_error().to_string());
    }
    // SAFETY: statvfs initialized stat on success.
    let stat = unsafe { stat.assume_init() };
    Ok(stat.f_bavail as u64 * stat.f_frsize as u64)
}
