//! Read-only observation of an OpenCode dispatch intent and submission journal.
use super::*;
use crate::util::LockProbe;

const STATUS_JSON_LIMIT: usize = 1024 * 1024;
const MAX_ID: usize = 128;
const MAX_PHASE: usize = 64;
const MAX_MODEL_FIELD: usize = 256;
const MAX_PATH: usize = 4096;

enum Record {
    Missing,
    Invalid,
    Value(J),
}

impl Record {
    fn marker(&self) -> &'static str {
        match self {
            Self::Missing => "absent",
            Self::Invalid => "invalid",
            Self::Value(_) => "present",
        }
    }

    fn value(&self) -> Option<&J> {
        match self {
            Self::Value(value) => Some(value),
            _ => None,
        }
    }
}

fn record(root: &Path, name: &str) -> Record {
    match util::read_state_json_bounded_if_exists(root, name, STATUS_JSON_LIMIT) {
        Ok(Some(value)) => Record::Value(value),
        Ok(None) => Record::Missing,
        Err(_) => Record::Invalid,
    }
}

fn field<'a>(value: Option<&'a J>, path: &[&str]) -> Option<&'a J> {
    path.iter()
        .try_fold(value?, |current, key| current.get(*key))
}

fn text<'a>(value: Option<&'a J>, path: &[&str]) -> Option<&'a str> {
    field(value, path)?
        .as_str()
        .filter(|value| !value.is_empty())
}

fn compare(left: Option<&str>, right: Option<&str>) -> &'static str {
    match (left, right) {
        (Some(left), Some(right)) if left == right => "match",
        (Some(_), Some(_)) => "mismatch",
        _ => "unknown",
    }
}

fn valid_model(value: &J) -> bool {
    value.is_object()
        && text(Some(value), &["providerID"]).is_some_and(|value| value.len() <= MAX_MODEL_FIELD)
        && text(Some(value), &["id"]).is_some_and(|value| value.len() <= MAX_MODEL_FIELD)
        && match value.get("variant") {
            None | Some(J::Null) => true,
            Some(variant) => variant
                .as_str()
                .is_some_and(|value| !value.is_empty() && value.len() <= MAX_MODEL_FIELD),
        }
}

fn valid_session(value: &str) -> bool {
    value.starts_with("ses")
        && value.len() <= MAX_ID
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

fn valid_hash(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn valid_message_id(value: &str) -> bool {
    value.starts_with("msg")
        && value.len() <= MAX_ID
        && value.bytes().all(|byte| byte.is_ascii_alphanumeric())
}

fn model_match(left: Option<&J>, right: Option<&J>) -> &'static str {
    match (left, right) {
        (Some(left), Some(right)) if valid_model(left) && valid_model(right) && left == right => {
            "match"
        }
        (Some(_), Some(_)) => "mismatch",
        _ => "unknown",
    }
}

fn valid_path(value: &str) -> bool {
    value.len() <= MAX_PATH && Path::new(value).is_absolute()
}

fn valid_port(value: u64) -> bool {
    (1..=65535).contains(&value)
}

fn same_bounded_text(values: &[Option<&str>], limit: usize) -> &'static str {
    if values.iter().any(|value| value.is_none()) {
        "unknown"
    } else if values
        .iter()
        .filter_map(|value| *value)
        .all(|value| value.len() <= limit)
        && values.iter().all(|value| *value == values[0])
    {
        "match"
    } else {
        "mismatch"
    }
}

fn same_paths(values: &[Option<&str>]) -> &'static str {
    if values.iter().any(|value| value.is_none()) {
        "unknown"
    } else if values.iter().filter_map(|value| *value).all(valid_path)
        && values.iter().all(|value| *value == values[0])
    {
        "match"
    } else {
        "mismatch"
    }
}

fn same_sessions(values: &[Option<&str>]) -> &'static str {
    if values.iter().any(|value| value.is_none()) {
        "unknown"
    } else if values.iter().filter_map(|value| *value).all(valid_session)
        && values.iter().all(|value| *value == values[0])
    {
        "match"
    } else {
        "mismatch"
    }
}

fn same_models(values: &[Option<&J>]) -> &'static str {
    if values.iter().any(|value| value.is_none()) {
        "unknown"
    } else if values.iter().filter_map(|value| *value).all(valid_model)
        && values.iter().all(|value| *value == values[0])
    {
        "match"
    } else {
        "mismatch"
    }
}

fn object_match(left: Option<&J>, right: Option<&J>) -> &'static str {
    match (left, right) {
        (Some(left), Some(right))
            if left.as_object().is_some_and(|object| !object.is_empty())
                && right.as_object().is_some_and(|object| !object.is_empty())
                && left == right =>
        {
            "match"
        }
        (Some(_), Some(_)) => "mismatch",
        _ => "unknown",
    }
}

fn packet_digest(packet: Option<&J>) -> Option<String> {
    let packet = packet?;
    if !packet.is_object() {
        return None;
    }
    serde_json::to_string(packet)
        .ok()
        .map(|text| util::pyjson::sha256_hex(text.as_bytes()))
}

fn selected(value: Option<&str>, limit: usize) -> J {
    match value {
        Some(value) if value.len() <= limit => json!(value),
        Some(_) => json!("invalid"),
        None => J::Null,
    }
}

fn selected_session(value: Option<&str>) -> J {
    match value {
        Some(value) if valid_session(value) => json!(value),
        Some(_) => json!("invalid"),
        None => J::Null,
    }
}

fn selected_hash(value: Option<&str>) -> J {
    match value {
        Some(value) if valid_hash(value) => json!(value),
        Some(_) => json!("invalid"),
        None => J::Null,
    }
}

fn selected_message(value: Option<&str>) -> J {
    match value {
        Some(value) if valid_message_id(value) => json!(value),
        Some(_) => json!("invalid"),
        None => J::Null,
    }
}

fn selected_phase(value: Option<&str>) -> J {
    match value {
        Some(value) if value.len() <= MAX_PHASE => json!(value),
        Some(_) => json!("invalid"),
        None => json!("invalid"),
    }
}

fn session_identity(
    state: Option<&J>,
    creation: Option<&J>,
    dispatch: Option<&J>,
    submission: Option<&J>,
) -> &'static str {
    let values = [
        text(state, &["session"]),
        text(creation, &["session"]),
        text(dispatch, &["session"]),
        text(submission, &["session"]),
    ];
    if values.iter().any(|value| value.is_none()) {
        "unknown"
    } else if values.iter().all(|value| *value == values[0]) {
        "match"
    } else {
        "mismatch"
    }
}

fn packet_identity(
    state: Option<&J>,
    creation: Option<&J>,
    dispatch: Option<&J>,
    submission: Option<&J>,
) -> &'static str {
    let legacy = text(creation, &["packet_sha256"]);
    let observed = text(dispatch, &["packet_sha256"]);
    let expected = packet_digest(field(creation, &["packet"]));
    let queue = same_bounded_text(
        &[
            text(creation, &["queue_id"]),
            text(submission, &["context", "queue_id"]),
        ],
        MAX_ID,
    );
    let wait = same_bounded_text(
        &[
            text(creation, &["wait"]),
            text(submission, &["context", "wait"]),
        ],
        MAX_ID,
    );
    let queue_dir = same_paths(&[
        text(creation, &["queue_dir"]),
        text(submission, &["context", "queue_dir"]),
    ]);
    let binding = object_match(
        field(creation, &["packet", "binding"]),
        field(submission, &["context", "binding"]),
    );
    let session = same_sessions(&[
        text(dispatch, &["session"]),
        text(state, &["session"]),
        text(creation, &["session"]),
        text(submission, &["session"]),
    ]);
    let hash = match (legacy, observed, expected.as_deref()) {
        (Some(legacy), Some(observed), Some(expected))
            if valid_hash(legacy)
                && valid_hash(observed)
                && valid_hash(expected)
                && legacy == observed
                && observed == expected =>
        {
            "match"
        }
        (Some(_), Some(_), Some(_)) => "mismatch",
        _ => "unknown",
    };
    let checks = [queue, wait, queue_dir, binding, session, hash];
    if checks.contains(&"mismatch") {
        "mismatch"
    } else if checks.iter().all(|value| *value == "match") {
        "match"
    } else {
        "unknown"
    }
}

fn submission_identity(
    state: Option<&J>,
    creation: Option<&J>,
    submission: Option<&J>,
) -> &'static str {
    let session = same_sessions(&[
        text(state, &["session"]),
        text(creation, &["session"]),
        text(submission, &["session"]),
    ]);
    let model = model_match(field(state, &["model"]), field(submission, &["model"]));
    let cwd = same_paths(&[text(state, &["cwd"]), text(submission, &["cwd"])]);
    let adapter = same_bounded_text(
        &[
            text(submission, &["context", "producer_identity", "adapter"]),
            Some("opencode"),
        ],
        32,
    );
    let producer_session = same_sessions(&[
        text(submission, &["context", "producer_identity", "session"]),
        text(state, &["session"]),
    ]);
    let producer_model = model_match(
        field(submission, &["context", "producer_identity", "model"]),
        field(state, &["model"]),
    );
    let producer_cwd = same_paths(&[
        text(submission, &["context", "producer_identity", "cwd"]),
        text(state, &["cwd"]),
    ]);
    let checks = [
        session,
        model,
        cwd,
        adapter,
        producer_session,
        producer_model,
        producer_cwd,
    ];
    if checks.contains(&"mismatch") {
        "mismatch"
    } else if checks.iter().all(|value| *value == "match") {
        "match"
    } else {
        "unknown"
    }
}

fn runtime_identity(runtime: &J, creation: Option<&J>, state: Option<&J>) -> J {
    let runtime_cwd = text(Some(runtime), &["cwd"]);
    let creation_cwd = text(creation, &["runtime", "cwd"]);
    let state_cwd = text(state, &["cwd"]);
    let runtime_model = field(Some(runtime), &["model"]);
    let creation_model = field(creation, &["runtime", "model"]);
    let state_model = field(state, &["model"]);
    let runtime_adapter = text(Some(runtime), &["adapter"]);
    let creation_adapter = text(creation, &["runtime", "adapter"]);
    let state_adapter = text(state, &["adapter"]);
    let runtime_port = runtime["port"].as_u64().filter(|port| valid_port(*port));
    let creation_port = field(creation, &["runtime", "port"])
        .and_then(J::as_u64)
        .filter(valid_port);
    let port = match (runtime_port, creation_port) {
        (Some(runtime), Some(creation)) if runtime == creation => "match",
        (Some(_), Some(_)) => "mismatch",
        _ => "unknown",
    };
    let endpoint = match runtime_port {
        Some(port) => {
            let expected = format!("http://127.0.0.1:{port}");
            compare(text(state, &["endpoint"]), Some(expected.as_str()))
        }
        None => "mismatch",
    };
    json!({
        "instance":same_bounded_text(
            &[
                text(Some(runtime), &["instance"]),
                text(creation, &["instance"]),
                text(creation, &["runtime", "instance"]),
            ],
            MAX_ID,
        ),
        "cwd":same_paths(&[runtime_cwd, creation_cwd, state_cwd]),
        "model":same_models(&[runtime_model, creation_model, state_model]),
        "adapter":same_bounded_text(
            &[runtime_adapter, creation_adapter, state_adapter],
            32,
        ),
        "port":port,
        "endpoint":endpoint,
    })
}

fn marker_pair(dispatch: &Record, submission: &Record) -> &'static str {
    match (dispatch.marker(), submission.marker()) {
        ("absent", "absent") => "markers_absent",
        ("present", "absent") => "dispatch_marker_only",
        ("present", "present") => "dispatch_and_submission_present",
        ("absent", "present") => "submission_marker_only",
        _ => "invalid_or_unknown",
    }
}

fn is_native_terminal(phase: &str) -> bool {
    matches!(phase, "complete" | "failed" | "decision")
}

fn operator_action(
    guard: &str,
    markers: &str,
    exact: bool,
    collection_signalled: bool,
    collect_available: bool,
) -> &'static str {
    if guard == "busy" {
        "wait_for_owner"
    } else if guard != "acquired" {
        "inspect_guard_lock"
    } else if markers == "markers_absent" {
        "inspect_dispatch_prerequisites"
    } else if exact && collection_signalled {
        "already_published"
    } else if collect_available {
        "collect"
    } else if markers == "dispatch_marker_only" {
        "preserve_and_block"
    } else {
        "preserve_and_inspect"
    }
}

/// Observe one runtime's journals. The caller owns the environment; this
/// routine never sends, collects, retries, creates sessions or deletes data.
pub(crate) fn observe(root: &Path, runtime: &J) -> J {
    let adapter = runtime["adapter"].as_str().unwrap_or_default();
    if adapter != "opencode" {
        let adapter = if adapter.is_empty() {
            J::Null
        } else {
            selected(Some(adapter), 32)
        };
        return json!({
            "schema":1,
            "scope":"runtime_journal_snapshot",
            "adapter":adapter,
            "supported":false,
            "markers":"not_sampled",
            "guard":"not_sampled",
            "consistency":"not_sampled",
            "identity":"not_sampled",
            "submission_evidence":"not_sampled",
            "collect_available":false,
            "safe_to_retry":false,
            "operator_action":"unsupported_adapter"
        });
    }

    let probe = crate::guard::probe(root);
    let (guard, consistency, guard_lock) = match probe {
        Ok(LockProbe::Acquired(lock)) => ("acquired", "sampled_locked_guard", Some(lock)),
        Ok(LockProbe::Busy) => ("busy", "sampled_guard_busy", None),
        Ok(LockProbe::Missing) => ("missing", "guard_missing", None),
        Err(_) => ("unknown", "guard_unknown", None),
    };

    let creation = record(root, "creation.json");
    let state = record(root, "state.json");
    let dispatch = record(root, "dispatch.json");
    let submission = record(root, "submission.json");
    drop(guard_lock);

    let creation_value = creation.value();
    let state_value = state.value();
    let dispatch_value = dispatch.value();
    let submission_value = submission.value();
    let markers = marker_pair(&dispatch, &submission);
    let session_identity = session_identity(
        state_value,
        creation_value,
        dispatch_value,
        submission_value,
    );
    let packet_identity = packet_identity(
        state_value,
        creation_value,
        dispatch_value,
        submission_value,
    );
    let submission_identity = submission_identity(state_value, creation_value, submission_value);
    let runtime_binding = runtime_identity(runtime, creation_value, state_value);
    let identity = json!({
        "runtime_instance":runtime_binding["instance"],
        "runtime_cwd":runtime_binding["cwd"],
        "runtime_model":runtime_binding["model"],
        "runtime_adapter":runtime_binding["adapter"],
        "runtime_port":runtime_binding["port"],
        "runtime_endpoint":runtime_binding["endpoint"],
        "session":session_identity,
        "packet":packet_identity,
        "submission":submission_identity,
        "state_adapter":compare(
            text(state_value, &["adapter"]),
            Some("opencode")
        )
    });
    let identity_match = [
        "runtime_instance",
        "runtime_cwd",
        "runtime_model",
        "runtime_adapter",
        "runtime_port",
        "runtime_endpoint",
        "session",
        "packet",
        "submission",
        "state_adapter",
    ]
    .iter()
    .all(|key| identity[*key] == "match");
    let creation_phase = text(creation_value, &["phase"]).unwrap_or("invalid");
    let submission_phase = text(submission_value, &["phase"]).unwrap_or("invalid");
    let submission_phase_known = matches!(
        submission_phase,
        "sending"
            | "submitted"
            | "running"
            | "needs_attention"
            | "complete"
            | "failed"
            | "decision"
            | "signalled"
    );
    let dispatch_phase_known = text(dispatch_value, &["phase"]) == Some("sending");
    let message_id = text(submission_value, &["message_id"]);
    let message_id_known = message_id.is_some_and(valid_message_id);
    let native_terminal = is_native_terminal(submission_phase);
    let collection_signalled = submission_phase == "signalled";
    let exact = identity_match
        && creation_phase == "bound"
        && dispatch_phase_known
        && submission_phase_known
        && message_id_known;
    let collect_available =
        guard == "acquired" && submission.marker() == "present" && exact && !collection_signalled;
    let submission_evidence = match submission.marker() {
        "absent" => "marker_absent",
        "present" => "marker_present",
        _ => "invalid_or_unknown",
    };
    json!({
        "schema":1,
        "scope":"runtime_journal_snapshot",
        "adapter":"opencode",
        "supported":true,
        "runtime_instance":selected(text(Some(runtime), &["instance"]), MAX_ID),
        "creation_phase":selected_phase(Some(creation_phase)),
        "session":selected_session(text(state_value, &["session"])),
        "markers":markers,
        "guard":guard,
        "consistency":consistency,
        "dispatch":{
            "marker":dispatch.marker(),
            "phase":if dispatch_phase_known { json!("sending") } else { json!("invalid") },
            "session":selected_session(text(dispatch_value, &["session"])),
            "packet_sha256":selected_hash(text(dispatch_value, &["packet_sha256"]))
        },
        "submission":{
            "marker":submission.marker(),
            "phase":selected_phase(if submission_phase_known { Some(submission_phase) } else { None }),
            "message_id":selected_message(message_id)
        },
        "submission_evidence":submission_evidence,
        "native_terminal":native_terminal,
        "collection_signalled":collection_signalled,
        "identity":identity,
        "collect_available":collect_available,
        "safe_to_retry":false,
        "operator_action":operator_action(guard, markers, exact, collection_signalled, collect_available)
    })
}

pub fn status(env: &TaskEnvironment, root: &Path) -> Result<J> {
    let runtime = super::verify_bounded(env, root, STATUS_JSON_LIMIT)?;
    Ok(observe(root, &runtime))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::{symlink, PermissionsExt};

    fn write_json(root: &Path, name: &str, value: J) {
        util::atomic_write_json(&root.join(name), &value).unwrap();
    }

    fn valid_root() -> (tempfile::TempDir, PathBuf, J) {
        let temp = crate::util::fs::test_tempdir();
        let root = temp.path().join("runtime");
        fs::create_dir(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        let lock = util::FileLock::acquire(&root.join("guard.lock"), false)
            .unwrap()
            .unwrap();
        drop(lock);
        let binding = json!({"candidate":"a","lineage":{"role":"development"}});
        let packet = json!({"binding":binding.clone()});
        let packet_sha256 =
            util::pyjson::sha256_hex(serde_json::to_string(&packet).unwrap().as_bytes());
        let model = json!({"providerID":"p","id":"m"});
        let cwd = root.to_string_lossy().to_string();
        let queue_dir = root.join("queue").to_string_lossy().to_string();
        let producer =
            json!({"adapter":"opencode","session":"ses1","model":model.clone(),"cwd":cwd.clone()});
        let runtime = json!({
            "adapter":"opencode","instance":"instance-1","cwd":cwd.clone(),
            "model":model.clone(),"port":4096
        });
        write_json(
            &root,
            "creation.json",
            json!({
                "phase":"bound","instance":"instance-1","session":"ses1",
                "runtime":runtime.clone(), "queue_dir":queue_dir.clone(), "queue_id":"queue1", "wait":"wait1",
                "packet":packet, "packet_sha256":packet_sha256.clone()
            }),
        );
        write_json(
            &root,
            "state.json",
            json!({
                "adapter":"opencode","session":"ses1","model":model.clone(),
                "cwd":cwd.clone(),"endpoint":"http://127.0.0.1:4096"
            }),
        );
        write_json(
            &root,
            "dispatch.json",
            json!({"phase":"sending","session":"ses1","packet_sha256":packet_sha256}),
        );
        write_json(
            &root,
            "submission.json",
            json!({
                "message_id":"msg1","session":"ses1","model":model,"cwd":cwd,
                "phase":"submitted",
                "context":{
                    "queue_dir":queue_dir,"queue_id":"queue1","wait":"wait1","binding":binding,
                    "producer_identity":producer
                }
            }),
        );
        (temp, root, runtime)
    }

    fn observe(root: &Path, runtime: &J) -> J {
        super::observe(root, runtime)
    }

    #[test]
    fn status_treats_absent_markers_as_absent_and_never_creates_the_lock() {
        let temp = crate::util::fs::test_tempdir();
        let root = temp.path().join("runtime");
        fs::create_dir(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        let status = observe(
            &root,
            &json!({"adapter":"opencode","instance":"instance-1"}),
        );
        assert_eq!(status["markers"], "markers_absent");
        assert_eq!(status["guard"], "missing");
        assert_eq!(status["operator_action"], "inspect_guard_lock");
        assert_eq!(status["safe_to_retry"], false);
        assert_eq!(status["collect_available"], false);
        assert!(!root.join("guard.lock").exists());
        assert!(!root.join("dispatch.json").exists());
        assert!(!status.to_string().contains("not_attempted"));
    }

    #[test]
    fn valid_snapshot_is_sampled_and_marks_collection_available() {
        let (_temp, root, runtime) = valid_root();
        let before: Vec<_> = [
            "creation.json",
            "state.json",
            "dispatch.json",
            "submission.json",
        ]
        .iter()
        .map(|name| fs::read(root.join(name)).unwrap())
        .collect();
        let status = observe(&root, &runtime);
        assert_eq!(status["markers"], "dispatch_and_submission_present");
        assert_eq!(status["guard"], "acquired");
        assert_eq!(status["consistency"], "sampled_locked_guard");
        assert_eq!(status["submission_evidence"], "marker_present");
        assert_eq!(status["identity"]["runtime_instance"], "match");
        assert_eq!(status["identity"]["runtime_cwd"], "match");
        assert_eq!(status["identity"]["runtime_model"], "match");
        assert_eq!(status["identity"]["runtime_adapter"], "match");
        assert_eq!(status["identity"]["runtime_port"], "match");
        assert_eq!(status["identity"]["runtime_endpoint"], "match");
        assert_eq!(status["identity"]["session"], "match");
        assert_eq!(status["identity"]["packet"], "match");
        assert_eq!(status["identity"]["submission"], "match");
        assert_eq!(status["collect_available"], true);
        assert_eq!(status["safe_to_retry"], false);
        for (name, bytes) in [
            "creation.json",
            "state.json",
            "dispatch.json",
            "submission.json",
        ]
        .iter()
        .zip(before)
        {
            assert_eq!(fs::read(root.join(name)).unwrap(), bytes);
        }
    }

    #[test]
    fn native_terminal_phases_remain_collectable_until_published() {
        for phase in ["complete", "failed", "decision"] {
            let (_temp, root, runtime) = valid_root();
            let mut submission = util::read_json(&root.join("submission.json")).unwrap();
            submission["phase"] = json!(phase);
            write_json(&root, "submission.json", submission);
            let status = observe(&root, &runtime);
            assert_eq!(status["submission"]["phase"], phase);
            assert_eq!(status["native_terminal"], true);
            assert_eq!(status["collection_signalled"], false);
            assert_eq!(status["collect_available"], true);
            assert_eq!(status["operator_action"], "collect");
            assert_eq!(status["safe_to_retry"], false);
        }
    }

    #[test]
    fn signalled_submission_is_already_published_but_not_recollected() {
        let (_temp, root, runtime) = valid_root();
        let mut submission = util::read_json(&root.join("submission.json")).unwrap();
        submission["phase"] = json!("signalled");
        write_json(&root, "submission.json", submission);
        let status = observe(&root, &runtime);
        assert_eq!(status["native_terminal"], false);
        assert_eq!(status["collection_signalled"], true);
        assert_eq!(status["collect_available"], false);
        assert_eq!(status["operator_action"], "already_published");
    }

    #[test]
    fn invalid_signalled_submission_is_not_trusted_as_published() {
        let (_temp, root, runtime) = valid_root();
        let mut submission = util::read_json(&root.join("submission.json")).unwrap();
        submission["phase"] = json!("signalled");
        write_json(&root, "submission.json", submission);
        write_json(
            &root,
            "dispatch.json",
            json!({"phase":"sending","session":"ses1","packet_sha256":"wrong"}),
        );
        let status = observe(&root, &runtime);
        assert_eq!(status["collection_signalled"], true);
        assert_eq!(status["collect_available"], false);
        assert_eq!(status["operator_action"], "preserve_and_inspect");
    }

    #[test]
    fn absent_markers_require_prerequisite_inspection_even_with_a_free_guard() {
        let temp = crate::util::fs::test_tempdir();
        let root = temp.path().join("runtime");
        fs::create_dir(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        let lock = util::FileLock::acquire(&root.join("guard.lock"), false)
            .unwrap()
            .unwrap();
        drop(lock);
        let status = observe(
            &root,
            &json!({"adapter":"opencode","instance":"instance-1"}),
        );
        assert_eq!(status["guard"], "acquired");
        assert_eq!(status["markers"], "markers_absent");
        assert_eq!(status["operator_action"], "inspect_dispatch_prerequisites");
        assert_eq!(status["safe_to_retry"], false);
    }

    #[test]
    fn selected_output_fields_are_bounded_and_fixed_invalid_markers() {
        let (_temp, root, runtime) = valid_root();
        let long = "x".repeat(MAX_ID + 1);
        let mut state = util::read_json(&root.join("state.json")).unwrap();
        state["session"] = json!(long.clone());
        write_json(&root, "state.json", state);
        let mut dispatch = util::read_json(&root.join("dispatch.json")).unwrap();
        dispatch["phase"] = json!("x".repeat(MAX_PHASE + 1));
        dispatch["packet_sha256"] = json!("x".repeat(MAX_ID + 1));
        write_json(&root, "dispatch.json", dispatch);
        let status = observe(&root, &runtime);
        assert_eq!(status["session"], "invalid");
        assert_eq!(status["dispatch"]["phase"], "invalid");
        assert_eq!(status["dispatch"]["packet_sha256"], "invalid");
        assert_eq!(status["collect_available"], false);
        assert!(!status.to_string().contains(long.as_str()));
    }

    #[test]
    fn dispatch_only_marks_preservation_not_retry_or_attempted() {
        let (_temp, root, runtime) = valid_root();
        fs::remove_file(root.join("submission.json")).unwrap();
        let status = observe(&root, &runtime);
        assert_eq!(status["markers"], "dispatch_marker_only");
        assert_eq!(status["submission_evidence"], "marker_absent");
        assert_eq!(status["collect_available"], false);
        assert_eq!(status["safe_to_retry"], false);
        assert_eq!(status["operator_action"], "preserve_and_block");
        assert!(!status.to_string().contains("not_attempted"));
        assert!(!status.to_string().contains("unsent"));
    }

    #[test]
    fn busy_guard_is_sampled_and_does_not_claim_collection() {
        let (_temp, root, runtime) = valid_root();
        let held = util::FileLock::acquire(&root.join("guard.lock"), false)
            .unwrap()
            .unwrap();
        let status = observe(&root, &runtime);
        assert_eq!(status["guard"], "busy");
        assert_eq!(status["consistency"], "sampled_guard_busy");
        assert_eq!(status["collect_available"], false);
        assert_eq!(status["operator_action"], "wait_for_owner");
        drop(held);
    }

    #[test]
    fn inconsistent_identity_fails_closed() {
        let (_temp, root, runtime) = valid_root();
        write_json(
            &root,
            "dispatch.json",
            json!({"phase":"sending","session":"ses-1","packet_sha256":"other"}),
        );
        let status = observe(&root, &runtime);
        assert_eq!(status["identity"]["packet"], "mismatch");
        assert_eq!(status["collect_available"], false);
        assert_eq!(status["operator_action"], "preserve_and_inspect");
        assert_eq!(status["safe_to_retry"], false);
    }

    #[test]
    fn malformed_journal_is_invalid_without_raw_error_text() {
        let (_temp, root, runtime) = valid_root();
        let path = root.join("submission.json");
        fs::write(&path, b"{").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        let status = observe(&root, &runtime);
        assert_eq!(status["submission"]["marker"], "invalid");
        assert_eq!(status["markers"], "invalid_or_unknown");
        assert_eq!(status["collect_available"], false);
        assert!(!status.to_string().contains("cannot parse"));
        assert!(!status.to_string().contains("invalid_json"));
        assert!(!status.to_string().contains("error"));
    }

    #[test]
    fn symlink_guard_is_unknown_and_not_created() {
        let (_temp, root, runtime) = valid_root();
        let guard = root.join("guard.lock");
        fs::remove_file(&guard).unwrap();
        let target = root.join("real-lock");
        fs::write(&target, b"").unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o600)).unwrap();
        symlink(&target, &guard).unwrap();
        let status = observe(&root, &runtime);
        assert_eq!(status["guard"], "unknown");
        assert_eq!(status["consistency"], "guard_unknown");
        assert_eq!(status["collect_available"], false);
        assert!(!status.to_string().contains("cannot probe"));
    }

    #[test]
    fn null_empty_or_mismatched_identity_shapes_fail_closed() {
        let (_temp, root, runtime) = valid_root();
        let mut state = util::read_json(&root.join("state.json")).unwrap();
        state["model"] = J::Null;
        write_json(&root, "state.json", state);
        let status = observe(&root, &runtime);
        assert_eq!(status["identity"]["runtime_model"], "mismatch");
        assert_eq!(status["identity"]["submission"], "mismatch");
        assert_eq!(status["collect_available"], false);

        let (_temp, root, runtime) = valid_root();
        let mut state = util::read_json(&root.join("state.json")).unwrap();
        state["model"] = json!({"providerID":"","id":""});
        state["cwd"] = json!("relative");
        state["endpoint"] = json!("http://127.0.0.1:9999");
        write_json(&root, "state.json", state);
        let status = observe(&root, &runtime);
        assert_eq!(status["identity"]["runtime_model"], "mismatch");
        assert_eq!(status["identity"]["runtime_cwd"], "mismatch");
        assert_eq!(status["identity"]["runtime_endpoint"], "mismatch");
        assert_eq!(status["collect_available"], false);

        let (_temp, root, runtime) = valid_root();
        let mut creation = util::read_json(&root.join("creation.json")).unwrap();
        creation["packet"]["binding"] = J::Null;
        write_json(&root, "creation.json", creation);
        let status = observe(&root, &runtime);
        assert_eq!(status["identity"]["packet"], "mismatch");
        assert_eq!(status["collect_available"], false);
    }

    #[test]
    fn packet_hash_queue_and_runtime_bindings_are_recomputed() {
        let (_temp, root, runtime) = valid_root();
        let mut submission = util::read_json(&root.join("submission.json")).unwrap();
        submission["context"]["queue_dir"] = json!("/wrong/queue");
        write_json(&root, "submission.json", submission);
        let status = observe(&root, &runtime);
        assert_eq!(status["identity"]["packet"], "mismatch");
        assert_eq!(status["collect_available"], false);

        let (_temp, root, runtime) = valid_root();
        let mut creation = util::read_json(&root.join("creation.json")).unwrap();
        creation["runtime"]["port"] = json!(5000);
        write_json(&root, "creation.json", creation);
        let status = observe(&root, &runtime);
        assert_eq!(status["identity"]["runtime_port"], "mismatch");
        assert_eq!(status["collect_available"], false);

        let (_temp, root, runtime) = valid_root();
        let mut creation = util::read_json(&root.join("creation.json")).unwrap();
        creation["packet"]["binding"]["mutated"] = json!(true);
        write_json(&root, "creation.json", creation);
        let status = observe(&root, &runtime);
        assert_eq!(status["identity"]["packet"], "mismatch");
        assert_eq!(status["collect_available"], false);
    }

    #[test]
    fn unsupported_adapter_does_not_invent_marker_facts() {
        let temp = crate::util::fs::test_tempdir();
        let status = observe(temp.path(), &json!({"adapter":"codex","instance":"i"}));
        assert_eq!(status["supported"], false);
        assert_eq!(status["markers"], "not_sampled");
        assert_eq!(status["submission_evidence"], "not_sampled");
        assert_eq!(status["collect_available"], false);
        assert_eq!(status["safe_to_retry"], false);
        assert_eq!(status["operator_action"], "unsupported_adapter");
    }

    #[test]
    fn status_requires_an_owned_active_environment() {
        let temp = crate::util::fs::test_tempdir();
        let _env_root = crate::task_env::test_env_root(&temp.path().join("registry"));
        let root = temp.path().join("work");
        let tmp = temp.path().join("tmp");
        let mut env =
            crate::task_env::create_environment("dispatch-status-owner", &root, vec![], &tmp)
                .unwrap();
        env.release_ownership();
        let error = status(&env, &root).unwrap_err();
        assert_eq!(error.kind, crate::error::ErrorKind::Guard);
    }
}
