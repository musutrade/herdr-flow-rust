//! Acknowledged rotation phases, reconciliation and successor ACK.

use super::{
    external_delivery_pending, find_transcript, fingerprint, guard_error, load_guard_state,
    observe_context, read_state, save, transcript_state, validate_fresh_successor,
    validate_path_for_shell, validate_ready, ROTATION_PHASES,
};
use crate::error::Result;
use crate::herdr::GuardAdapter;
use crate::model;
use crate::model::J;
use crate::util;
use crate::util::pyjson::digest;
use crate::util::pyjson::uuid_hex;
use serde_json::json;
use serde_json::Value;
use std::path::Path;
use std::path::PathBuf;

fn request_handoff(root: &Path, s: &mut J, h: &mut dyn GuardAdapter) -> Result<()> {
    request_handoff_ready(s, h).map_or_else(Err, |ready| request_handoff_decide(root, s, h, ready))
}

fn request_handoff_decide(
    root: &Path,
    s: &mut J,
    h: &mut dyn GuardAdapter,
    ready: bool,
) -> Result<()> {
    if !ready {
        return Ok(());
    }
    request_handoff_send(root, s, h)
}

fn request_handoff_ready(s: &J, h: &mut dyn GuardAdapter) -> Result<bool> {
    if !request_handoff_flags(s) {
        return Ok(false);
    }
    h.idle()
}

fn request_handoff_flags(s: &J) -> bool {
    [
        model::bool_or(s, "rotation_due", false),
        model::bool_or(s, "auto_rotate", false),
    ]
    .into_iter()
    .all(|due| due)
}

fn request_handoff_send(root: &Path, s: &mut J, h: &mut dyn GuardAdapter) -> Result<()> {
    request_handoff_begin(root, s)?;
    request_handoff_prompt(root, s, h)
}

fn request_handoff_begin(root: &Path, s: &mut J) -> Result<()> {
    s["phase"] = json!("request_sending");
    save(root, s)
}

fn request_handoff_prompt(root: &Path, s: &mut J, h: &mut dyn GuardAdapter) -> Result<()> {
    let executable = request_handoff_executable()?;
    request_handoff_root(root, s, h, executable)
}

fn request_handoff_executable() -> Result<String> {
    validate_path_for_shell(&util::current_exe()?)
}

fn request_handoff_root(
    root: &Path,
    s: &mut J,
    h: &mut dyn GuardAdapter,
    executable: String,
) -> Result<()> {
    let root_safe = validate_path_for_shell(root)?;
    request_handoff_prompt_inner(root, s, h, &executable, &root_safe)
}

fn request_handoff_prompt_inner(
    root: &Path,
    s: &mut J,
    h: &mut dyn GuardAdapter,
    executable: &str,
    root_safe: &str,
) -> Result<()> {
    h.set_state(s);
    let turn = h.prompt(&format!(
        "Context guard requests a safe handoff for task {}, epoch {}. Read the context-rotation reference installed with this skill. Finish or transfer active commands, release writes, then write a concise handoff and run: {} guard checkpoint --state-dir {} --handoff HANDOFF. Preserve authorization and next action. After checkpoint, end your turn and do not resume work; the guard will start a blank session.",
        model::string(s, "task").unwrap_or_default(),
        model::int(s, "epoch").unwrap_or(0),
        util::shell_quote(executable),
        util::shell_quote(root_safe)
    ))?;
    request_handoff_finish(root, s, turn)
}

fn request_handoff_finish(root: &Path, s: &mut J, turn: String) -> Result<()> {
    s["request_turn"] = json!(turn);
    s["phase"] = json!("requested");
    s["delivery_at"] = json!(util::now());
    save(root, s)
}

fn start_rotation(root: &Path, s: &mut J, h: &mut dyn GuardAdapter) -> Result<()> {
    if !(model::bool_or(s, "auto_rotate", false) && h.idle()?) {
        return Ok(());
    }
    validate_ready(root, s)?;
    h.set_state(s);
    s["before_runtime"] = h.snapshot(None)?;
    validate_ready(root, s)?;
    s["phase"] = json!("tui_exit_sending");
    save(root, s)?;
    h.set_state(s);
    h.exit_tui()?;
    s["phase"] = json!("awaiting_shell");
    s["transition_deadline"] = json!(util::now() + 60.0);
    save(root, s)
}

fn await_shell(root: &Path, s: &mut J, h: &mut dyn GuardAdapter) -> Result<()> {
    if !model::bool_or(s, "auto_rotate", false) {
        return Ok(());
    }
    if util::now() > model::number(s, "transition_deadline").unwrap_or(0.0) {
        return Err(guard_error(
            "TUI exit overdue; inspect without replaying /quit",
        ));
    }
    h.set_state(s);
    if h.shell_ready()? {
        validate_ready(root, s)?;
        s["phase"] = json!("tui_launch_sending");
        s["bridge_nonce"] = json!(uuid_hex());
        save(root, s)?;
        h.set_state(s);
        h.launch_tui(root, &model::string(s, "bridge_nonce").unwrap_or_default())?;
        s["phase"] = json!("awaiting_tui");
        s["transition_deadline"] = json!(util::now() + 120.0);
        save(root, s)?;
    }
    Ok(())
}

fn await_tui(root: &Path, s: &mut J, h: &mut dyn GuardAdapter) -> Result<()> {
    if !model::bool_or(s, "auto_rotate", false) {
        return Ok(());
    }
    if util::now() > model::number(s, "transition_deadline").unwrap_or(0.0) {
        return Err(guard_error(
            "successor TUI protocol readiness overdue; inspect bridge marker",
        ));
    }
    h.set_state(s);
    let Some(session) = h.ready_tui()? else {
        return Ok(());
    };
    if model::string(s, "session").as_deref() == Some(session.as_str()) {
        return Err(guard_error("fresh TUI reused predecessor session"));
    }
    s["successor"] = json!(session);
    s["phase"] = json!("runtime_restore_sending");
    save(root, s)?;
    h.set_state(s);
    h.restore_runtime(s.get("before_runtime").unwrap_or(&Value::Null), &session)?;
    let identity = h.verify_tui(&session)?;
    s["phase"] = json!("successor_ready");
    s["tui_process"] = identity;
    save(root, s)
}

fn resume_successor(root: &Path, s: &mut J, h: &mut dyn GuardAdapter) -> Result<()> {
    resume_successor_ready(s, h)
        .map_or_else(Err, |ready| resume_successor_decide(root, s, h, ready))
}

fn resume_successor_decide(
    root: &Path,
    s: &mut J,
    h: &mut dyn GuardAdapter,
    ready: bool,
) -> Result<()> {
    if !ready {
        return Ok(());
    }
    resume_successor_run(root, s, h)
}

fn resume_successor_ready(s: &J, h: &mut dyn GuardAdapter) -> Result<bool> {
    if !model::bool_or(s, "auto_rotate", false) {
        return Ok(false);
    }
    h.idle()
}

fn resume_successor_run(root: &Path, s: &mut J, h: &mut dyn GuardAdapter) -> Result<()> {
    h.set_state(s);
    resume_successor_verify(root, s, h)
}

fn resume_successor_verify(root: &Path, s: &mut J, h: &mut dyn GuardAdapter) -> Result<()> {
    resume_successor_runtime(s, h)?;
    resume_successor_validate(root, s, h)
}

fn resume_successor_runtime(s: &J, h: &mut dyn GuardAdapter) -> Result<()> {
    crate::herdr::same_runtime(
        s.get("before_runtime").unwrap_or(&Value::Null),
        &h.snapshot(None)?,
    )
}

fn resume_successor_validate(root: &Path, s: &mut J, h: &mut dyn GuardAdapter) -> Result<()> {
    validate_fresh_successor(s)?;
    resume_successor_ready_state(root, s, h)
}

fn resume_successor_ready_state(root: &Path, s: &mut J, h: &mut dyn GuardAdapter) -> Result<()> {
    validate_ready(root, s)?;
    resume_successor_begin(root, s, h)
}

fn resume_successor_begin(root: &Path, s: &mut J, h: &mut dyn GuardAdapter) -> Result<()> {
    s["phase"] = json!("resume_sending");
    save(root, s)?;
    resume_successor_prompt(root, s, h)
}

fn resume_successor_prompt(root: &Path, s: &mut J, h: &mut dyn GuardAdapter) -> Result<()> {
    let executable = resume_successor_executable()?;
    resume_successor_root(root, s, h, executable)
}

fn resume_successor_executable() -> Result<String> {
    validate_path_for_shell(&util::current_exe()?)
}

fn resume_successor_root(
    root: &Path,
    s: &mut J,
    h: &mut dyn GuardAdapter,
    executable: String,
) -> Result<()> {
    let root_safe = validate_path_for_shell(root)?;
    resume_successor_prompt_inner(root, s, h, &executable, &root_safe)
}

fn resume_successor_prompt_inner(
    root: &Path,
    s: &mut J,
    h: &mut dyn GuardAdapter,
    executable: &str,
    root_safe: &str,
) -> Result<()> {
    h.set_state(s);
    let turn = h.prompt(&format!(
        "You are the successor {} for task {}. Read {root_safe}/handoff.json and {root_safe}/state.json; do not load the old conversation. Read only applicable instructions and handoff references. Verify checkout identity and preserve prior authorization. Before any task mutation, run: {} guard ack --state-dir {}. On a state-lock-busy error retry after one second without task writes. On other ACK failures stop and report. After ACK continue next_action within existing scope. Do not repeat completed validation solely because this is a fresh session.",
        model::string(s, "role").unwrap_or_default(),
        model::string(s, "task").unwrap_or_default(),
        util::shell_quote(executable),
        util::shell_quote(root_safe)
    ))?;
    resume_successor_finish(root, s, turn)
}

fn resume_successor_finish(root: &Path, s: &mut J, turn: String) -> Result<()> {
    s["resume_turn"] = json!(turn);
    s["phase"] = json!("awaiting_ack");
    s["delivery_at"] = json!(util::now());
    save(root, s)
}

fn handle_phase(root: &Path, s: &mut J, h: &mut dyn GuardAdapter) -> Result<()> {
    match model::string(s, "phase").as_deref() {
        Some("monitoring") => request_handoff(root, s, h),
        Some("ready") => start_rotation(root, s, h),
        Some("awaiting_shell") => await_shell(root, s, h),
        Some("awaiting_tui") => await_tui(root, s, h),
        Some("successor_ready") => resume_successor(root, s, h),
        _ => Ok(()),
    }
}

pub fn once(root: &Path, h: &mut dyn GuardAdapter) -> Result<J> {
    let _permit = crate::lifecycle::runtime_permit(root)?;
    let root = util::state_dir(root, false)?;
    let s = load_guard_state(&root)?;
    once_state(&root, h, s)
}

fn once_state(root: &Path, h: &mut dyn GuardAdapter, s: J) -> Result<J> {
    if once_terminal(&s) {
        return Ok(s);
    }
    let phase = model::string(&s, "phase").unwrap_or_default();
    once_review_pending(root, h, s, &phase)
}

fn once_terminal(s: &J) -> bool {
    matches!(
        model::string(s, "phase").as_deref(),
        Some("needs_attention" | "released")
    )
}

fn once_review_pending(root: &Path, h: &mut dyn GuardAdapter, s: J, phase: &str) -> Result<J> {
    if phase == "review_config_sending" {
        return Err(guard_error(
            "review configuration is ambiguous; inspect then review-reconcile without replay",
        ));
    }
    once_parked(root, h, s, phase)
}

fn once_parked(root: &Path, h: &mut dyn GuardAdapter, mut s: J, phase: &str) -> Result<J> {
    model::remove(&mut s, "pause_reason");
    if phase == "parked" {
        return Ok(s);
    }
    once_observe(root, h, s, phase)
}

fn once_observe(root: &Path, h: &mut dyn GuardAdapter, s: J, phase: &str) -> Result<J> {
    h.set_state(&s);
    if once_should_observe(phase) {
        return once_observe_active(root, h, s);
    }
    once_delivery(root, h, s)
}

fn once_should_observe(phase: &str) -> bool {
    matches!(phase, "monitoring" | "requested" | "ready")
}

fn once_observe_active(root: &Path, h: &mut dyn GuardAdapter, mut s: J) -> Result<J> {
    observe_context(root, &mut s, h)?;
    once_delivery(root, h, s)
}

fn once_delivery(root: &Path, h: &mut dyn GuardAdapter, s: J) -> Result<J> {
    if external_delivery_pending(&s) {
        return once_pause(root, s);
    }
    once_handle(root, h, s)
}

fn once_pause(root: &Path, mut s: J) -> Result<J> {
    s["pause_reason"] = json!("wait_queue_delivery_pending");
    save(root, &mut s)?;
    Ok(s)
}

fn once_handle(root: &Path, h: &mut dyn GuardAdapter, mut s: J) -> Result<J> {
    h.set_state(&s);
    handle_phase(root, &mut s, h)?;
    once_after_phase(h, s)
}

fn once_after_phase(h: &mut dyn GuardAdapter, s: J) -> Result<J> {
    h.set_state(&s);
    if once_ambiguous(&s) {
        return Err(guard_error(
            "delivery is ambiguous; inspect and reconcile, never blindly replay",
        ));
    }
    once_overdue(&s)
}

fn once_ambiguous(s: &J) -> bool {
    matches!(
        model::string(s, "phase").as_deref(),
        Some(
            "request_sending"
                | "tui_exit_sending"
                | "tui_launch_sending"
                | "runtime_restore_sending"
                | "resume_sending"
        )
    )
}

fn once_overdue(s: &J) -> Result<J> {
    if once_handoff_overdue(s) {
        return Err(guard_error(
            "handoff or acknowledgement overdue; inspect without resending",
        ));
    }
    Ok(s.clone())
}

fn once_handoff_overdue(s: &J) -> bool {
    [
        matches!(
            model::string(s, "phase").as_deref(),
            Some("requested" | "awaiting_ack")
        ),
        util::now() - model::number(s, "delivery_at").unwrap_or(0.0) > 600.0,
    ]
    .into_iter()
    .all(|overdue| overdue)
}

pub fn reconcile(root: &Path, session: &str, h: &mut dyn GuardAdapter, shell: bool) -> Result<J> {
    let _permit = crate::lifecycle::runtime_permit(root)?;
    let root = util::state_dir(root, false)?;
    let s = read_state(&root)?;
    reconcile_state(root, session, h, shell, s)
}

fn reconcile_state(
    root: PathBuf,
    session: &str,
    h: &mut dyn GuardAdapter,
    shell: bool,
    s: J,
) -> Result<J> {
    reconcile_phase(&s)?;
    h.set_state(&s);
    let prior = model::string(&s, "phase_before_error").unwrap_or_default();
    reconcile_prior(root, session, h, shell, s, &prior)
}

fn reconcile_phase(s: &J) -> Result<()> {
    if model::string(s, "phase").as_deref() == Some("needs_attention") {
        return Ok(());
    }
    Err(guard_error(
        "reconcile only applies after an inspected error",
    ))
}

fn reconcile_prior(
    root: PathBuf,
    session: &str,
    h: &mut dyn GuardAdapter,
    shell: bool,
    s: J,
    prior: &str,
) -> Result<J> {
    if prior == "review_config_sending" {
        return Err(guard_error(
            "use review-reconcile for an ambiguous approval configuration",
        ));
    }
    reconcile_branch(root, session, h, shell, s, prior)
}

fn reconcile_branch(
    root: PathBuf,
    session: &str,
    h: &mut dyn GuardAdapter,
    shell: bool,
    s: J,
    prior: &str,
) -> Result<J> {
    if shell {
        return reconcile_shell(root, h, s, prior);
    }
    reconcile_thread(root, session, h, s, prior)
}

fn reconcile_history_path(root: &Path) -> PathBuf {
    root.join("history")
        .join(format!("recovery-{}.json", util::now_ns()))
}

fn reconcile_record(root: &Path, s: &J) -> Result<()> {
    util::atomic_write_json(&reconcile_history_path(root), s)
}

fn reconcile_finish(root: &Path, mut s: J) -> Result<J> {
    model::remove(&mut s, "error");
    model::remove(&mut s, "phase_before_error");
    save(root, &mut s)?;
    Ok(s)
}

fn reconcile_shell(root: PathBuf, h: &mut dyn GuardAdapter, s: J, prior: &str) -> Result<J> {
    reconcile_shell_prior(prior)?;
    h.set_state(&s);
    reconcile_shell_ready(root, h, s)
}

fn reconcile_shell_prior(prior: &str) -> Result<()> {
    if matches!(
        prior,
        "tui_exit_sending" | "awaiting_shell" | "tui_launch_sending" | "awaiting_tui"
    ) {
        return Ok(());
    }
    Err(guard_error(
        "shell recovery cannot discard an in-flight work delivery",
    ))
}

fn reconcile_shell_ready(root: PathBuf, h: &mut dyn GuardAdapter, s: J) -> Result<J> {
    let ready = h.shell_ready()?;
    reconcile_shell_confirmed(root, h, s, ready)
}

fn reconcile_shell_confirmed(
    root: PathBuf,
    h: &mut dyn GuardAdapter,
    s: J,
    ready: bool,
) -> Result<J> {
    if !ready {
        return Err(guard_error(
            "shell recovery requires a confirmed empty foreground shell",
        ));
    }
    reconcile_shell_validate(root, h, s)
}

fn reconcile_shell_validate(root: PathBuf, _h: &mut dyn GuardAdapter, mut s: J) -> Result<J> {
    validate_ready(&root, &mut s)?;
    reconcile_shell_write(&root, s)
}

fn reconcile_shell_write(root: &Path, s: J) -> Result<J> {
    reconcile_record(root, &s)?;
    reconcile_shell_finish(root, s)
}

fn reconcile_shell_finish(root: &Path, mut s: J) -> Result<J> {
    s["phase"] = json!("awaiting_shell");
    s["auto_rotate"] = json!(false);
    s["transition_deadline"] = json!(util::now() + 120.0);
    model::remove(&mut s, "successor");
    model::remove(&mut s, "bridge_nonce");
    reconcile_finish(root, s)
}

fn reconcile_thread(
    root: PathBuf,
    session: &str,
    h: &mut dyn GuardAdapter,
    s: J,
    prior: &str,
) -> Result<J> {
    let actual = reconcile_actual(&s, h)?;
    reconcile_thread_session(root, session, h, s, prior, actual)
}

fn reconcile_actual(s: &J, h: &mut dyn GuardAdapter) -> Result<String> {
    if reconcile_bridge_pending(s) {
        return reconcile_ready_tui(h);
    }
    h.session()
}

fn reconcile_bridge_pending(s: &J) -> bool {
    model::get(s, "bridge_nonce")
        .map(|value| model::truthy(Some(value)))
        .unwrap_or(false)
}

fn reconcile_ready_tui(h: &mut dyn GuardAdapter) -> Result<String> {
    Ok(h.ready_tui()?.unwrap_or_default())
}

fn reconcile_thread_session(
    root: PathBuf,
    session: &str,
    h: &mut dyn GuardAdapter,
    s: J,
    prior: &str,
    actual: String,
) -> Result<J> {
    reconcile_thread_matches(session, &actual)?;
    reconcile_thread_record(root, h, s, prior, actual)
}

fn reconcile_thread_matches(session: &str, actual: &str) -> Result<()> {
    if actual == session {
        return Ok(());
    }
    Err(guard_error(
        "--session must match the inspected live thread",
    ))
}

fn reconcile_thread_record(
    root: PathBuf,
    h: &mut dyn GuardAdapter,
    mut s: J,
    prior: &str,
    actual: String,
) -> Result<J> {
    reconcile_record(&root, &s)?;
    reconcile_thread_apply(&root, &mut s, h, prior, actual)
}

fn reconcile_thread_apply(
    root: &Path,
    s: &mut J,
    h: &mut dyn GuardAdapter,
    prior: &str,
    actual: String,
) -> Result<J> {
    if model::string(s, "session").as_deref() == Some(actual.as_str()) {
        return reconcile_same_session(root, s.clone());
    }
    reconcile_successor(root, s.clone(), h, prior, actual)
}

fn reconcile_same_session(root: &Path, mut s: J) -> Result<J> {
    s["phase"] = json!("monitoring");
    s["auto_rotate"] = json!(false);
    s["rotation_due"] = json!(true);
    reconcile_finish(root, s)
}

fn reconcile_successor(
    root: &Path,
    mut s: J,
    h: &mut dyn GuardAdapter,
    prior: &str,
    actual: String,
) -> Result<J> {
    validate_ready(root, &mut s)?;
    reconcile_successor_state(root, s, h, prior, actual)
}

fn reconcile_successor_state(
    root: &Path,
    s: J,
    h: &mut dyn GuardAdapter,
    prior: &str,
    actual: String,
) -> Result<J> {
    let mut successor_state = s.clone();
    successor_state["successor"] = json!(actual.clone());
    h.set_state(&successor_state);
    reconcile_successor_runtime(&s, h, &actual)?;
    reconcile_successor_fields(root, s, h, prior, actual)
}

fn reconcile_successor_runtime(s: &J, h: &mut dyn GuardAdapter, actual: &str) -> Result<()> {
    crate::herdr::same_runtime(
        s.get("before_runtime").unwrap_or(&Value::Null),
        &h.snapshot(Some(actual))?,
    )
}

fn reconcile_successor_fields(
    root: &Path,
    mut s: J,
    h: &mut dyn GuardAdapter,
    prior: &str,
    actual: String,
) -> Result<J> {
    let identity = h.verify_tui(&actual)?;
    s["successor"] = json!(actual);
    s["tui_process"] = identity;
    s["auto_rotate"] = json!(false);
    reconcile_successor_phase(root, s, prior)
}

fn reconcile_successor_phase(root: &Path, s: J, prior: &str) -> Result<J> {
    if matches!(prior, "resume_sending" | "awaiting_ack") {
        return reconcile_awaiting_ack(root, s);
    }
    reconcile_successor_rotation(root, s, prior)
}

fn reconcile_awaiting_ack(root: &Path, mut s: J) -> Result<J> {
    s["phase"] = json!("awaiting_ack");
    s["delivery_at"] = json!(util::now());
    reconcile_finish(root, s)
}

fn reconcile_successor_rotation(root: &Path, s: J, prior: &str) -> Result<J> {
    if ROTATION_PHASES.contains(&prior) {
        return reconcile_successor_ready(root, s);
    }
    Err(guard_error(
        "unregistered successor; inspect lifecycle before recovery",
    ))
}

fn reconcile_successor_ready(root: &Path, mut s: J) -> Result<J> {
    s["phase"] = json!("successor_ready");
    reconcile_finish(root, s)
}

pub fn ack(root: &Path, session: &str) -> Result<J> {
    let _permit = crate::lifecycle::runtime_permit(root)?;
    let root = util::state_dir(root, false)?;
    let s = read_state(&root)?;
    ack_version(root, session, s)
}

fn ack_version(root: PathBuf, session: &str, s: J) -> Result<J> {
    if model::int(&s, "version") != Some(2) {
        return Err(guard_error("legacy guard requires explicit migrate"));
    }
    ack_phase(root, session, s)
}

fn ack_phase(root: PathBuf, session: &str, s: J) -> Result<J> {
    if !ack_phase_ok(&s, session) {
        return Err(guard_error("unexpected successor acknowledgement"));
    }
    ack_integrity(&root, session, s)
}

fn ack_phase_ok(s: &J, session: &str) -> bool {
    [
        model::string(s, "phase").as_deref() == Some("awaiting_ack"),
        model::string(s, "successor").as_deref() == Some(session),
    ]
    .into_iter()
    .all(|ok| ok)
}

fn ack_integrity(root: &Path, session: &str, s: J) -> Result<J> {
    let checkout = ack_checkout(&s)?;
    ack_checkout_compare(root, session, s, checkout)
}

fn ack_checkout(s: &J) -> Result<String> {
    fingerprint(&model::string(s, "cwd").unwrap_or_default())
}

fn ack_checkout_compare(root: &Path, session: &str, s: J, checkout: String) -> Result<J> {
    if checkout != model::string(&s, "checkout").unwrap_or_default() {
        return Err(guard_error(
            "checkout or handoff changed before acknowledgement",
        ));
    }
    ack_handoff(root, session, s)
}

fn ack_handoff(root: &Path, session: &str, s: J) -> Result<J> {
    let actual = ack_handoff_digest(root)?;
    ack_handoff_compare(root, session, s, actual)
}

fn ack_handoff_digest(root: &Path) -> Result<String> {
    Ok(digest(&util::read_bytes(&root.join("handoff.json"))?))
}

fn ack_handoff_compare(root: &Path, session: &str, s: J, actual: String) -> Result<J> {
    if actual != model::string(&s, "handoff_sha").unwrap_or_default() {
        return Err(guard_error(
            "checkout or handoff changed before acknowledgement",
        ));
    }
    ack_transcript(root, session, s)
}

fn ack_transcript(root: &Path, session: &str, s: J) -> Result<J> {
    let sessions_dir = model::string(&s, "sessions_dir")
        .ok_or_else(|| guard_error("sessions directory is unavailable"))?;
    ack_find(root, session, s, &sessions_dir)
}

fn ack_find(root: &Path, session: &str, s: J, sessions_dir: &str) -> Result<J> {
    let successor_transcript = find_transcript(session, Path::new(sessions_dir))?;
    ack_read(root, session, s, &successor_transcript)
}

fn ack_read(root: &Path, session: &str, s: J, successor_transcript: &Path) -> Result<J> {
    let meta = transcript_state(successor_transcript, None)?;
    ack_meta(root, session, s, successor_transcript, &meta)
}

fn ack_meta(root: &Path, session: &str, s: J, successor_transcript: &Path, meta: &J) -> Result<J> {
    if ack_meta_mismatch(&s, session, meta) {
        return Err(guard_error("successor identity mismatch"));
    }
    ack_archive(root, session, s, successor_transcript)
}

fn ack_meta_mismatch(s: &J, session: &str, meta: &J) -> bool {
    [
        model::string(meta, "session").as_deref() == Some(session),
        model::string(meta, "cwd") == model::string(s, "cwd"),
    ]
    .into_iter()
    .any(|ok| !ok)
}

fn ack_archive(root: &Path, session: &str, s: J, successor_transcript: &Path) -> Result<J> {
    let archive = ack_archive_path(root, &s);
    ack_write_archive(&archive, &s)?;
    ack_update(root, session, s, successor_transcript)
}

fn ack_archive_path(root: &Path, s: &J) -> PathBuf {
    root.join("history").join(format!(
        "epoch-{}-{}.json",
        model::int(s, "epoch").unwrap_or(0),
        model::string(s, "session").unwrap_or_default()
    ))
}

fn ack_write_archive(archive: &Path, s: &J) -> Result<()> {
    util::atomic_write_json(archive, s)
}

fn ack_update(root: &Path, session: &str, mut s: J, successor_transcript: &Path) -> Result<J> {
    ack_set_fields(&mut s, session, successor_transcript);
    ack_remove_fields(&mut s);
    ack_save(root, s)
}

fn ack_set_fields(s: &mut J, session: &str, successor_transcript: &Path) {
    s["session"] = json!(session);
    s["transcript"] = json!(successor_transcript.display().to_string());
    s["epoch"] = json!(model::int(s, "epoch").unwrap_or(0) + 1);
    s["phase"] = json!("monitoring");
    s["rotation_due"] = json!(false);
    s["level"] = json!("unknown");
    s["usage"] = Value::Null;
}

fn ack_remove_fields(s: &mut J) {
    [
        "successor",
        "successor_transcript",
        "checkout",
        "checkpoint_users",
        "checkpoint_at",
        "handoff_sha",
        "scan",
        "ready_scan",
    ]
    .into_iter()
    .for_each(|key| {
        model::remove(s, key);
    });
}

fn ack_save(root: &Path, mut s: J) -> Result<J> {
    save(root, &mut s)?;
    Ok(s)
}
