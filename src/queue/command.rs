//! One-shot command reservations and result capture for the wait queue.

use std::fs::{File, OpenOptions};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde_json::{json, Value};

use crate::error::Result;
use crate::model::{self, J};
use crate::progress;
use crate::util;

use super::{locked, queue_error, save};

fn validate_argv(argv: &[J]) -> Result<Vec<String>> {
    if argv.is_empty() {
        return Err(queue_error(
            "command file must be a nonempty JSON argv array",
        ));
    }
    let mut result = Vec::new();
    for item in argv {
        match item.as_str() {
            Some(value) if !value.is_empty() => result.push(value.to_string()),
            _ => {
                return Err(queue_error(
                    "command file must be a nonempty JSON argv array",
                ))
            }
        }
    }
    Ok(result)
}

fn validate_managed_command(
    t: &J,
    w: &J,
    wait_id: &str,
    cwd: &str,
    session: Option<&str>,
) -> Result<String> {
    ensure_runnable_assignment(t, w, wait_id)?;
    validate_managed_session(t, w, cwd, session)
}

fn runnable_phase(phase: &str) -> bool {
    ["waiting", "next_pending", "executing"].contains(&phase)
}

fn ensure_runnable_assignment(t: &J, w: &J, wait_id: &str) -> Result<()> {
    let binding_matches = progress::binding(w)
        .map(|binding| progress::same(t, &binding))
        .unwrap_or(false);
    match (
        binding_matches,
        model::string(t, "wait").as_deref() == Some(wait_id),
        model::string(t, "control").as_deref() == Some("running"),
        runnable_phase(&model::string(t, "phase").unwrap_or_default()),
    ) {
        (true, true, true, true) => Ok(()),
        _ => Err(queue_error(
            "command is not the current runnable assignment",
        )),
    }
}

fn pinned_session(session: Option<&str>) -> Result<&str> {
    session
        .filter(|value| !value.is_empty())
        .ok_or_else(|| queue_error("managed command requires its pinned producing session"))
}

fn validate_managed_session(t: &J, w: &J, cwd: &str, session: Option<&str>) -> Result<String> {
    let session = pinned_session(session)?;
    validate_managed_producer(t, w, cwd, session)
}

fn validate_managed_producer(t: &J, w: &J, cwd: &str, session: &str) -> Result<String> {
    ensure_producer_matches(w, session)?;
    validate_managed_writer(t, cwd, session)
}

fn ensure_producer_matches(w: &J, session: &str) -> Result<()> {
    if crate::identity::required_producer(w)?["session"] != session {
        return Err(queue_error(
            "managed command requires its pinned producing session",
        ));
    }
    Ok(())
}

fn validate_managed_writer(t: &J, cwd: &str, session: &str) -> Result<String> {
    ensure_writer_cwd(t, cwd)?;
    Ok(session.to_string())
}

fn ensure_writer_workspace(writer: &J, cwd: &str) -> Result<()> {
    if model::string(writer, "workspace").as_deref() != Some(cwd) {
        return Err(queue_error(
            "command cwd differs from the reserved writer workspace",
        ));
    }
    Ok(())
}

fn ensure_writer_cwd(t: &J, cwd: &str) -> Result<()> {
    let Some(writer) = model::get(t, "writer").filter(|value| model::truthy(Some(value))) else {
        return Ok(());
    };
    ensure_writer_workspace(writer, cwd)
}

fn command_state(root: &Path, wait_id: &str, cwd: &str) -> Result<(J, Option<J>, Option<String>)> {
    let s = super::load_queue(root)?;
    command_state_loaded(root, s, wait_id, cwd)
}

fn command_state_loaded(
    root: &Path,
    s: J,
    wait_id: &str,
    cwd: &str,
) -> Result<(J, Option<J>, Option<String>)> {
    let w = command_wait(&s, wait_id)?;
    let env_session = crate::identity::producer(&w)?
        .map(|id| crate::identity::environment(&id))
        .transpose()?;
    command_producer_checked(root, s, w, wait_id, cwd, env_session)
}

fn command_wait(s: &J, wait_id: &str) -> Result<J> {
    let w = model::dig(s, &["waits", wait_id])
        .cloned()
        .ok_or_else(|| queue_error("command requires open signal wait"))?;
    command_wait_checked(s, w)
}

fn command_wait_checked(s: &J, w: J) -> Result<J> {
    ensure_open_signal_wait(s, &w)?;
    Ok(w)
}

fn ensure_open_signal_wait(s: &J, w: &J) -> Result<()> {
    match (
        model::bool_or(s, "stopped", false),
        model::bool_or(w, "closed", false),
        model::string(w, "kind").as_deref() == Some("signal"),
    ) {
        (false, false, true) => Ok(()),
        _ => Err(queue_error("command requires open signal wait")),
    }
}

fn command_producer_checked(
    root: &Path,
    s: J,
    w: J,
    wait_id: &str,
    cwd: &str,
    env_session: Option<String>,
) -> Result<(J, Option<J>, Option<String>)> {
    ensure_command_producer(&w)?;
    command_result_checked(root, s, w, wait_id, cwd, env_session)
}

fn ensure_command_producer(w: &J) -> Result<()> {
    crate::identity::check_environment(w)
}

fn ensure_no_result(root: &Path, wait_id: &str) -> Result<()> {
    if root
        .join("results")
        .join(format!("{wait_id}.json"))
        .exists()
    {
        return Err(queue_error(
            "wait already has a result; do not execute again",
        ));
    }
    Ok(())
}

fn command_result_checked(
    root: &Path,
    s: J,
    w: J,
    wait_id: &str,
    cwd: &str,
    env_session: Option<String>,
) -> Result<(J, Option<J>, Option<String>)> {
    ensure_no_result(root, wait_id)?;
    command_assignment(s, w, wait_id, cwd, env_session)
}

fn assignment_task(s: &J, w: &J) -> Option<J> {
    if model::get(w, "assignment").is_some() {
        return model::string(w, "task").and_then(|task| model::dig(s, &["tasks", &task]).cloned());
    }
    None
}

fn managed_session(
    s: &J,
    t: Option<&J>,
    w: &J,
    wait_id: &str,
    cwd: &str,
    env_session: Option<&str>,
) -> Result<Option<String>> {
    match t {
        Some(t) => {
            progress::lineage::ensure_current(s, t)?;
            validate_managed_command(t, w, wait_id, cwd, env_session).map(Some)
        }
        _ => Ok(None),
    }
}

fn command_assignment(
    s: J,
    w: J,
    wait_id: &str,
    cwd: &str,
    env_session: Option<String>,
) -> Result<(J, Option<J>, Option<String>)> {
    let t = assignment_task(&s, &w);
    let session = managed_session(&s, t.as_ref(), &w, wait_id, cwd, env_session.as_deref())?;
    Ok((s, t, session))
}

fn claim_task(s: &mut J, t: &J, cwd: &str, session: &str, folder: &Path) -> Result<()> {
    let invocation = folder.join("invocation.json").display().to_string();
    let t = claim_accept_pending(s, t, session, &invocation)?;
    claim_after_accept(s, t, cwd, session, &invocation)
}

fn claim_spec(t: &J, invocation: &str) -> J {
    json!({
        "task": model::get(t, "task").cloned().unwrap_or(Value::Null),
        "state_version": model::get(t, "state_version").cloned().unwrap_or(Value::Null),
        "operation": model::dig(t, &["next_action", "operation"]).cloned().unwrap_or(Value::Null),
        "assignment": model::get(t, "assignment").cloned().unwrap_or(Value::Null),
        "evidence": invocation
    })
}

fn claim_accept_next(s: &mut J, t: &J, session: &str, invocation: &str) -> Result<()> {
    let wait = &s["waits"][t["wait"].as_str().unwrap_or_default()];
    let identity = crate::identity::required_producer(wait)?;
    if identity["session"] != session {
        return Err(queue_error("takeover producer changed"));
    }
    progress::accept_identity(s, &claim_spec(t, invocation), &identity, util::now()).map(|_| ())
}

fn reload_claimed_task(s: &J, t: &J) -> Result<J> {
    let task_name = model::string(t, "task").unwrap_or_default();
    model::dig(s, &["tasks", &task_name])
        .cloned()
        .ok_or_else(|| queue_error("command task disappeared"))
}

fn claim_accept_pending(s: &mut J, t: &J, session: &str, invocation: &str) -> Result<J> {
    let t = t.clone();
    if model::string(&t, "phase").as_deref() == Some("next_pending") {
        return claim_accept_checked(s, &t, session, invocation);
    }
    Ok(t)
}

fn claim_accept_checked(s: &mut J, t: &J, session: &str, invocation: &str) -> Result<J> {
    claim_accept_next(s, t, session, invocation)?;
    reload_claimed_task(s, t)
}

fn writer_missing(t: &J) -> bool {
    model::get(t, "writer").map(Value::is_null).unwrap_or(true)
}

fn claim_reserve_writer(s: &mut J, t: &mut J, cwd: &str, session: &str) -> Result<()> {
    let assignment = model::string(t, "assignment").unwrap_or_default();
    let task_name = model::string(t, "task").unwrap_or_default();
    t["writer"] = progress::reserve_writer(s, cwd, &assignment, session, &task_name)?;
    t["writer"]["producer"] =
        crate::identity::required_producer(&s["waits"][t["wait"].as_str().unwrap_or_default()])?;
    Ok(())
}

fn claim_writer(s: &mut J, t: J, cwd: &str, session: &str) -> Result<J> {
    if writer_missing(&t) {
        return claim_writer_reserved(s, t, cwd, session);
    }
    Ok(t)
}

fn claim_writer_reserved(s: &mut J, mut t: J, cwd: &str, session: &str) -> Result<J> {
    claim_reserve_writer(s, &mut t, cwd, session)?;
    Ok(t)
}

fn claim_after_accept(s: &mut J, t: J, cwd: &str, session: &str, invocation: &str) -> Result<()> {
    let t = claim_writer(s, t, cwd, session)?;
    claim_store(s, t, invocation)
}

fn claim_store(s: &mut J, mut t: J, invocation: &str) -> Result<()> {
    let writer = model::get_mut(&mut t, "writer")
        .and_then(Value::as_object_mut)
        .ok_or_else(|| queue_error("managed command writer must be a JSON object"))?;
    writer.insert("evidence".to_string(), json!(invocation));
    t["phase"] = json!("executing");
    let version = model::int(&t, "state_version").unwrap_or(0) + 1;
    t["state_version"] = json!(version);
    progress::audit_fields(
        &mut t,
        util::now(),
        "command_claimed",
        &[("evidence", json!(invocation))],
    );
    let task_name = model::string(&t, "task").unwrap_or_default();
    s["tasks"][&task_name] = t;
    Ok(())
}

fn reserve(
    root: &Path,
    wait_id: &str,
    argv: &[String],
    cwd: &str,
    s: &mut J,
    t: Option<&J>,
    session: Option<&str>,
) -> Result<(PathBuf, J)> {
    let folder = root.join("commands").join(wait_id);
    util::ensure_private_dir(&folder, true)?;
    let record = json!({
        "argv": argv,
        "cwd": cwd,
        "started_at": util::now(),
        "wrapper_pid": std::process::id()
    });
    // Exclusive reservation survives a wrapper crash; never rerun blindly.
    util::write_new_file(&folder.join("invocation.json"), &record)
        .map_err(|_| queue_error("wait already has a reserved invocation; do not execute again"))?;
    if let Some(t) = t {
        let session = session
            .ok_or_else(|| queue_error("managed command requires its pinned producing session"))?;
        claim_task(s, t, cwd, session, &folder)?;
        save(root, s)?;
    }
    Ok((folder, record))
}

fn execute(folder: &Path, argv: &[String], cwd: &str, record: J) -> Result<(String, String)> {
    match open_logs(folder) {
        Ok((stdout, stderr)) => execute_spawn(folder, argv, cwd, record, stdout, stderr),
        Err(error) => record_start_failure(folder, record, error.to_string()),
    }
}

fn open_log(path: &Path) -> std::io::Result<File> {
    OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(path)
}

fn open_logs(folder: &Path) -> std::io::Result<(File, File)> {
    // Attempt both opens as before, retaining stdout's error precedence.
    let stdout = open_log(&folder.join("stdout.log"));
    let stderr = open_log(&folder.join("stderr.log"));
    Ok((stdout?, stderr?))
}

fn record_start_failure(folder: &Path, mut record: J, error: String) -> Result<(String, String)> {
    record["error"] = json!("LogError");
    record["error_detail"] = json!(error);
    record["finished_at"] = json!(util::now());
    util::atomic_write_json(&folder.join("result.json"), &record)?;
    Ok((
        "decision".to_string(),
        "Command could not start; inspect retained invocation and result".to_string(),
    ))
}

fn execute_spawn(
    folder: &Path,
    argv: &[String],
    cwd: &str,
    record: J,
    stdout: File,
    stderr: File,
) -> Result<(String, String)> {
    let mut command = Command::new(&argv[0]);
    command
        .args(&argv[1..])
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr));
    match command.spawn() {
        Ok(mut child) => execute_child(folder, record, &mut child),
        Err(error) => record_spawn_failure(folder, record, format!("{:?}", error.kind())),
    }
}

fn record_spawn_failure(folder: &Path, mut record: J, kind: String) -> Result<(String, String)> {
    record["error"] = json!(kind);
    record["finished_at"] = json!(util::now());
    util::atomic_write_json(&folder.join("result.json"), &record)?;
    Ok((
        "decision".to_string(),
        "Command could not start; inspect retained invocation and result".to_string(),
    ))
}

fn execute_child(
    folder: &Path,
    record: J,
    child: &mut std::process::Child,
) -> Result<(String, String)> {
    let mut record = record;
    record["child_pid"] = json!(child.id());
    let _ = util::atomic_write_json(&folder.join("process.json"), &record);
    match child.wait() {
        Ok(status) => record_wait_success(folder, record, status),
        Err(error) => record_wait_failure(folder, record, error.to_string()),
    }
}

fn exit_code(status: std::process::ExitStatus) -> i32 {
    status.code().unwrap_or_else(|| {
        use std::os::unix::process::ExitStatusExt;
        status.signal().map(|signal| -signal).unwrap_or(1)
    })
}

fn record_wait_success(
    folder: &Path,
    mut record: J,
    status: std::process::ExitStatus,
) -> Result<(String, String)> {
    let code = exit_code(status);
    record["exit_code"] = json!(code);
    record["finished_at"] = json!(util::now());
    util::atomic_write_json(&folder.join("result.json"), &record)?;
    let outcome = if code == 0 { "complete" } else { "failed" };
    Ok((
        outcome.to_string(),
        format!("Command exited with status {code}; this is execution status, not review approval"),
    ))
}

fn record_wait_failure(folder: &Path, mut record: J, detail: String) -> Result<(String, String)> {
    record["error"] = json!("WaitError");
    record["error_detail"] = json!(detail);
    record["finished_at"] = json!(util::now());
    util::atomic_write_json(&folder.join("result.json"), &record)?;
    Ok((
        "decision".to_string(),
        "Command could not be awaited; inspect retained invocation and result".to_string(),
    ))
}

fn signal_result(root: &Path, wait_id: &str, outcome: &str, message: &str, folder: &Path) {
    match locked(root, true) {
        Ok(_lock) => {
            let result_path = folder.join("result.json").display().to_string();
            if let Err(error) = super::signal(root, wait_id, outcome, message, Some(&result_path)) {
                util::write_stderr(&format!(
                    "wait_queue: command result retained at {} but not signalled: {}\n",
                    result_path, error.message
                ));
            }
        }
        Err(error) => {
            util::write_stderr(&format!(
                "wait_queue: command result retained at {} but not signalled: {}\n",
                folder.join("result.json").display(),
                error.message
            ));
        }
    }
}

pub fn run_command(root: &Path, wait_id: &str, argv_spec: &J, cwd: &str) -> Result<i32> {
    super::isolation::require_legacy_execution(root)?;
    run_legacy_command(root, wait_id, argv_spec, cwd)
}

fn run_legacy_command(root: &Path, wait_id: &str, argv_spec: &J, cwd: &str) -> Result<i32> {
    let argv = command_argv(argv_spec)?;
    run_argv(root, wait_id, &argv, cwd)
}

fn command_argv(argv_spec: &J) -> Result<Vec<String>> {
    let Some(items) = argv_spec.as_array() else {
        return Err(queue_error(
            "command file must be a nonempty JSON argv array",
        ));
    };
    validate_argv(items)
}

fn run_argv(root: &Path, wait_id: &str, argv: &[String], cwd: &str) -> Result<i32> {
    let cwd = util::resolve_path(Path::new(cwd)).display().to_string();
    let (folder, record) = reserve_command(root, wait_id, argv, &cwd)?;
    finish_command(root, wait_id, argv, &cwd, &folder, record)
}

fn reserve_command(root: &Path, wait_id: &str, argv: &[String], cwd: &str) -> Result<(PathBuf, J)> {
    let _lock = locked(root, false)?;
    reserve_current_command(root, wait_id, argv, cwd)
}

fn reserve_current_command(
    root: &Path,
    wait_id: &str,
    argv: &[String],
    cwd: &str,
) -> Result<(PathBuf, J)> {
    let (mut s, t, session) = command_state(root, wait_id, cwd)?;
    reserve(
        root,
        wait_id,
        argv,
        cwd,
        &mut s,
        t.as_ref(),
        session.as_deref(),
    )
}

fn finish_command(
    root: &Path,
    wait_id: &str,
    argv: &[String],
    cwd: &str,
    folder: &Path,
    record: J,
) -> Result<i32> {
    let (outcome, message) = execute(folder, argv, cwd, record)?;
    signal_result(root, wait_id, &outcome, &message, folder);
    Ok(command_exit_code(&outcome))
}

fn command_exit_code(outcome: &str) -> i32 {
    if outcome == "complete" {
        0
    } else {
        1
    }
}

/// CLI-facing wrapper: read a JSON argv file then execute once.
pub fn run_from_file(root: &Path, wait_id: &str, command_file: &Path, cwd: &str) -> Result<i32> {
    let argv_spec = util::read_json(command_file)?;
    run_command(root, wait_id, &argv_spec, cwd)
}

#[cfg(test)]
mod identity_tests {
    use super::*;
    #[test]
    fn command_claim_checks_takeover_identity_and_records_writer_adapter() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = json!({"tasks":{},"waits":{"w":{"producer":{"adapter":"codex","session":"sesworker"}}}});
        let mut t = json!({"task":"d","wait":"w","assignment":"a"});
        assert!(claim_accept_next(&mut s, &t, "other", "evidence").is_err());
        claim_reserve_writer(&mut s, &mut t, dir.path().to_str().unwrap(), "sesworker").unwrap();
        assert_eq!(t["writer"]["producer"]["adapter"], "codex");
    }
    #[test]
    fn managed_commands_require_the_adapter_qualified_session() {
        let wait = json!({"producer":{"adapter":"codex","session":"sesworker"}});
        assert!(ensure_producer_matches(&wait, "sesworker").is_ok());
        assert!(ensure_producer_matches(&wait, "other").is_err());
        assert!(ensure_producer_matches(&json!({}), "sesworker").is_err());
    }
}
