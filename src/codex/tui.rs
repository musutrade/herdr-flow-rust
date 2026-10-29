//! Run the native Codex TUI through an observed, transparent UNIX byte proxy.
//!
//! The proxy never creates a server and never injects or changes JSON-RPC
//! requests. `tui.json` belongs to one launch nonce and is ready only after an
//! observed successful `thread/start`, `thread/resume` or `thread/fork`
//! response. `proxy_socket` is the filesystem path; `proxy_endpoint` is the
//! full `unix://` URI passed to Codex. Long Linux paths use a live
//! `/proc/<wrapper>/fd/<directory-fd>` alias to that same filesystem socket.

use std::cell::RefCell;
use std::ffi::CString;
use std::fs::{self, File};
use std::io::{ErrorKind as IoErrorKind, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::ExitStatusExt;
use std::path::{Component, Path, PathBuf};
use std::process::{Child, Command, ExitStatus};
use std::rc::Rc;
use std::sync::atomic::{AtomicI32, Ordering};
use std::time::{Duration, Instant};

use serde_json::{json, Map, Value};

use crate::codex::rpc::{protocol_error, MAX_MESSAGE};
use crate::error::{ErrorKind, FlowError, Result};
use crate::model::J;
use crate::util::pyjson::{dumps, sha256_hex, uuid_hex, Separators};
use crate::util::{self, process_start_ticks};

mod wire;

/// Optional task attachment policy. The ordinary `tui` bridge stays transparent.
pub trait BridgePolicy {
    fn forward(&mut self, client: bool, data: &[u8]) -> Result<Vec<u8>>;
    fn observe(&mut self, marker: &J) -> Result<()>;
    fn arguments(&self) -> Vec<String>;
    /// Server catalog bound; native client requests keep the broker byte bound.
    fn message_limit(&self) -> usize {
        NATIVE_SERVER_MESSAGE_LIMIT
    }
    fn take_responses(&mut self) -> Vec<u8> {
        Vec::new()
    }
    fn connected(&mut self) {}
    fn explicit_cwd(&self) -> bool {
        true
    }
}

pub use wire::WireObserver;

// Matches the Codex remote client's bounded server catalog size. Ordinary
// broker and native client requests keep MAX_MESSAGE (16 MiB).
pub(crate) const NATIVE_SERVER_MESSAGE_LIMIT: usize = 128 << 20;

pub const REVIEW_LIMIT: usize = 100;
pub const PENDING_REQUEST_LIMIT: usize = 64;
const MAX_SOCKET_PATH: usize = 107;
const UPSTREAM_CONNECT_TIMEOUT: Duration = Duration::from_secs(2);

/// Native review statuses and their explicit transitions.
pub const REVIEW_TRANSITIONS: &[(&str, &str)] = &[
    ("inProgress", "start"),
    ("approved", "finish"),
    ("denied", "finish"),
    ("timedOut", "finish"),
    ("aborted", "finish"),
];

/// Map a native review status to its transition, if it is supported.
pub fn review_transition(status: &str) -> Option<&'static str> {
    REVIEW_TRANSITIONS
        .iter()
        .find(|(candidate, _)| *candidate == status)
        .map(|(_, action)| *action)
}

/// `uuid.UUID(value)` followed by the canonical lowercase spelling check.
pub fn thread_uuid(value: &str) -> Result<String> {
    let mut hex = value.replace("urn:", "").replace("uuid:", "");
    hex = hex
        .trim_matches(|character| character == '{' || character == '}')
        .to_string();
    hex = hex.replace('-', "");
    if hex.len() != 32 || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(FlowError::usage("badly formed hexadecimal UUID string"));
    }
    let hex = hex.to_lowercase();
    let canonical = format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    );
    if canonical != value.to_lowercase() {
        return Err(FlowError::usage(
            "thread identity must use canonical UUID format",
        ));
    }
    Ok(canonical)
}

fn truncate_chars(value: &str, limit: usize) -> String {
    value.chars().take(limit).collect()
}

fn current_uid() -> u32 {
    // SAFETY: geteuid takes no pointers and has no caller preconditions.
    unsafe { libc::geteuid() }
}

fn absolute_lexical(path: &Path) -> PathBuf {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("/"))
            .join(path)
    };
    let mut normalized = PathBuf::from("/");
    for component in absolute.components() {
        match component {
            Component::RootDir | Component::Prefix(_) | Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            Component::Normal(name) => normalized.push(name),
        }
    }
    normalized
}

fn object_fields(value: Value) -> Map<String, Value> {
    match value {
        Value::Object(map) => map,
        _ => Map::new(),
    }
}

fn runtime_value(value: &Value) -> Option<J> {
    if value.is_null() {
        return None;
    }
    Some(value.clone())
}

fn serialize_json_line(value: &J) -> Vec<u8> {
    let mut text = dumps(value, true, Separators::Default);
    text.push('\n');
    text.into_bytes()
}

/// Python `json.dump(..., allow_nan=False)` equivalent for the values written
/// by this bridge: ASCII escaped, newline terminated and fsynced via the
/// shared atomic writer.
fn write_json_atomic(path: &Path, value: &J, prefix: &str, fsync_directory: bool) -> Result<()> {
    let parent = match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    let data = serialize_json_line(value);
    let mut temporary = tempfile::Builder::new()
        .prefix(prefix)
        .suffix(".json")
        .tempfile_in(parent)
        .map_err(|error| FlowError::io(format!("cannot create temporary file: {error}")))?;
    temporary
        .as_file_mut()
        .set_permissions(fs::Permissions::from_mode(0o600))?;
    temporary
        .as_file_mut()
        .write_all(&data)
        .map_err(|error| FlowError::io(format!("cannot write temporary file: {error}")))?;
    temporary
        .as_file_mut()
        .sync_all()
        .map_err(|error| FlowError::io(format!("cannot fsync temporary file: {error}")))?;
    temporary.persist(path).map_err(|error| {
        FlowError::io(format!(
            "cannot replace {}: {}",
            path.display(),
            error.error
        ))
    })?;
    if fsync_directory {
        util::fsync_dir(parent)?;
    }
    Ok(())
}

fn py_string(value: &J) -> String {
    if value.is_null() {
        return "None".to_string();
    }
    py_scalar_text(value)
}

fn py_scalar_text(value: &J) -> String {
    py_bool_text(value)
        .or_else(|| py_text(value))
        .unwrap_or_else(|| json_text(value))
}

fn py_bool_text(value: &J) -> Option<String> {
    py_bool(value).map(bool_text)
}

fn py_bool(value: &J) -> Option<bool> {
    match value {
        Value::Bool(flag) => Some(*flag),
        _ => None,
    }
}

fn bool_text(flag: bool) -> String {
    if flag {
        "True".to_string()
    } else {
        "False".to_string()
    }
}

fn py_text(value: &J) -> Option<String> {
    py_json_string(value).or_else(|| py_json_number(value))
}

fn py_json_string(value: &J) -> Option<String> {
    match value {
        Value::String(text) => Some(text.clone()),
        _ => None,
    }
}

fn py_json_number(value: &J) -> Option<String> {
    match value {
        Value::Number(number) => Some(number.to_string()),
        _ => None,
    }
}

fn json_text(value: &J) -> String {
    serde_json::to_string(value).unwrap_or_default()
}

fn identity_component(identity: &J, key: &str) -> String {
    match identity.get(key) {
        Some(value) => py_string(value),
        None => String::new(),
    }
}

fn start_ticks_value(pid: u32) -> Value {
    match process_start_ticks(pid) {
        Some(text) => text
            .parse::<i64>()
            .map(|value| json!(value))
            .unwrap_or_else(|_| json!(text)),
        None => Value::Null,
    }
}

/// One launch's control marker. Every update is an atomic private write.
pub struct Marker {
    root: PathBuf,
    value: J,
}

impl Marker {
    pub fn new(root: &Path, nonce: &str, server_socket: &str) -> Self {
        let pid = std::process::id();
        Self {
            root: root.to_path_buf(),
            value: json!({
                "nonce": nonce,
                "pid": null,
                "wrapper_pid": pid,
                "wrapper_start_ticks": start_ticks_value(pid),
                "status": "starting",
                "session": null,
                "server_socket": server_socket,
                "review_events_version": 1,
            }),
        }
    }

    pub fn value(&self) -> &J {
        &self.value
    }

    /// Python `Marker.update(**fields)`: merge fields, stamp `observed_at`,
    /// then atomically replace `tui.json`.
    pub fn update(&mut self, fields: Map<String, Value>) -> Result<()> {
        for (key, value) in fields {
            self.value[key] = value;
        }
        self.value["observed_at"] = json!(util::now());
        write_json_atomic(&self.root.join("tui.json"), &self.value, ".tui-", false)
    }
}

#[derive(Clone)]
struct PreviousBinding {
    thread_id: Option<String>,
    events: Vec<J>,
    overflow_count: J,
}

impl PreviousBinding {
    fn for_thread(&self, thread_id: Option<&str>) -> Option<PreviousBinding> {
        let wanted = thread_id?;
        self.for_some_thread(wanted)
    }

    fn for_some_thread(&self, thread_id: &str) -> Option<PreviousBinding> {
        if self.thread_id.as_deref() == Some(thread_id) {
            Some(self.clone())
        } else {
            None
        }
    }
}

fn check_event_binding(thread_id: Option<&str>, event: &J) -> Result<()> {
    let Some(thread_id) = thread_id else {
        return Err(FlowError::usage(
            "review event differs from its ready thread binding",
        ));
    };
    reject_foreign_event(thread_id, event)
}

fn reject_foreign_event(thread_id: &str, event: &J) -> Result<()> {
    if event.get("thread_id").and_then(Value::as_str) == Some(thread_id) {
        Ok(())
    } else {
        Err(FlowError::usage(
            "review event differs from its ready thread binding",
        ))
    }
}

/// Private durable audit snapshot scoped to this launch and ready identity.
///
/// Snapshots superseded by a launch or main-thread switch remain in
/// `review-history`. The active file retains at most 100 events, with a
/// counter making retention loss visible.
pub struct ReviewEvents {
    root: PathBuf,
    path: PathBuf,
    value: J,
    previous_binding: Option<PreviousBinding>,
}

impl ReviewEvents {
    pub fn new(root: &Path, nonce: &str) -> Result<Self> {
        let root = root.to_path_buf();
        let path = root.join("review-events.json");
        let mut events = Self {
            root,
            path,
            value: json!({}),
            previous_binding: None,
        };
        events.archive(None)?;
        events.value = json!({
            "version": 1,
            "nonce": nonce,
            "thread_id": null,
            "events": [],
            "overflow_count": 0,
        });
        events.write()?;
        Ok(events)
    }

    pub fn value(&self) -> &J {
        &self.value
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Move the current snapshot into `review-history` by launch/thread name.
    fn archive(&self, current: Option<&J>) -> Result<()> {
        // Python `Path.exists()` follows symlinks: a dangling symlink is
        // replaced by the new snapshot instead of being archived.
        if !review_snapshot_present(&self.path) {
            return Ok(());
        }
        self.archive_present(current)
    }

    fn archive_present(&self, current: Option<&J>) -> Result<()> {
        let identity = self.archive_identity(current)?;
        self.archive_identity_file(&identity)
    }

    fn archive_identity(&self, current: Option<&J>) -> Result<J> {
        check_owned_regular_snapshot(&self.path)?;
        archive_identity_from(&self.path, current)
    }

    fn archive_identity_file(&self, identity: &J) -> Result<()> {
        let history = self.root.join("review-history");
        prepare_history_directory(&history)?;
        rename_archive(&self.path, &history, identity)
    }

    /// Bind (or unbind) the active snapshot to a ready main thread. An event is
    /// accepted only while it matches the active ready thread.
    pub fn update(&mut self, thread_id: Option<&str>, event: Option<&J>) -> Result<()> {
        let store = self.rebind_and_record(thread_id, event)?;
        self.finish_update(store)
    }

    fn finish_update(&mut self, store: bool) -> Result<()> {
        if !store {
            return Ok(());
        }
        self.write()
    }

    fn rebind_and_record(&mut self, thread_id: Option<&str>, event: Option<&J>) -> Result<bool> {
        self.rebind(thread_id)?;
        self.append_event(thread_id, event)
    }

    fn rebind(&mut self, thread_id: Option<&str>) -> Result<()> {
        if !self.binding_changed(thread_id) {
            return Ok(());
        }
        self.switch_binding(thread_id)
    }

    fn binding_changed(&self, thread_id: Option<&str>) -> bool {
        self.current_thread_id().as_deref() != thread_id
    }

    fn current_thread_id(&self) -> Option<String> {
        self.value
            .get("thread_id")
            .and_then(Value::as_str)
            .map(ToString::to_string)
    }

    fn switch_binding(&mut self, thread_id: Option<&str>) -> Result<()> {
        self.archive_if_needed()?;
        self.rebind_memory(thread_id);
        Ok(())
    }

    fn archive_if_needed(&self) -> Result<()> {
        if self.value_events_empty() {
            return Ok(());
        }
        self.archive(Some(&self.value))
    }

    fn value_events_empty(&self) -> bool {
        self.value
            .get("events")
            .and_then(Value::as_array)
            .map(Vec::is_empty)
            .unwrap_or(true)
    }

    fn rebind_memory(&mut self, thread_id: Option<&str>) {
        self.remember_unbound(thread_id);
        self.apply_binding(thread_id);
        self.forget_previous_when_bound(thread_id);
    }

    fn remember_unbound(&mut self, thread_id: Option<&str>) {
        if !self.is_unbinding(thread_id) {
            return;
        }
        self.save_previous_binding();
    }

    fn is_unbinding(&self, thread_id: Option<&str>) -> bool {
        thread_id.is_none() && self.current_thread_id().is_some()
    }

    fn save_previous_binding(&mut self) {
        self.previous_binding = Some(PreviousBinding {
            thread_id: self.current_thread_id(),
            events: self.array_field("events"),
            overflow_count: self.field_or_zero("overflow_count"),
        });
    }

    fn array_field(&self, key: &str) -> Vec<J> {
        self.value
            .get(key)
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
    }

    fn field_or_zero(&self, key: &str) -> J {
        self.value.get(key).cloned().unwrap_or_else(|| json!(0))
    }

    fn apply_binding(&mut self, thread_id: Option<&str>) {
        if self.restore_previous_binding(thread_id) {
            return;
        }
        self.reset_binding(thread_id);
    }

    fn restore_previous_binding(&mut self, thread_id: Option<&str>) -> bool {
        let Some(binding) = self.matching_previous_binding(thread_id) else {
            return false;
        };
        // A same-launch main-thread resume must not hide unseen events.
        self.value["thread_id"] = json!(binding.thread_id);
        self.value["events"] = Value::Array(binding.events);
        self.value["overflow_count"] = binding.overflow_count;
        true
    }

    fn matching_previous_binding(&self, thread_id: Option<&str>) -> Option<PreviousBinding> {
        let binding = self.previous_binding.as_ref()?;
        binding.for_thread(thread_id)
    }

    fn reset_binding(&mut self, thread_id: Option<&str>) {
        self.value["thread_id"] = json!(thread_id);
        self.value["events"] = json!([]);
        self.value["overflow_count"] = json!(0);
    }

    fn forget_previous_when_bound(&mut self, thread_id: Option<&str>) {
        if thread_id.is_some() {
            self.previous_binding = None;
        }
    }

    fn append_event(&mut self, thread_id: Option<&str>, event: Option<&J>) -> Result<bool> {
        let Some(event) = event else {
            return Ok(true);
        };
        check_event_binding(thread_id, event)?;
        self.store_event(event)
    }

    fn store_event(&mut self, event: &J) -> Result<bool> {
        if self.event_already_recorded(event) {
            return Ok(false);
        }
        self.push_event(event);
        Ok(true)
    }

    fn event_already_recorded(&self, event: &J) -> bool {
        let Some(event_id) = event.get("event_id") else {
            return false;
        };
        self.array_field("events")
            .iter()
            .any(|item| item.get("event_id") == Some(event_id))
    }

    fn push_event(&mut self, event: &J) {
        let mut events = self.array_field("events");
        events.push(event.clone());
        let overflow = self.trim_events(&mut events);
        self.value["events"] = Value::Array(events);
        self.value["overflow_count"] = json!(overflow);
    }

    fn trim_events(&self, events: &mut Vec<J>) -> i64 {
        let mut overflow = self.overflow_count_value();
        while events.len() > REVIEW_LIMIT {
            events.remove(0);
            overflow += 1;
        }
        overflow
    }

    fn overflow_count_value(&self) -> i64 {
        self.value
            .get("overflow_count")
            .and_then(Value::as_i64)
            .unwrap_or(0)
    }

    fn write(&mut self) -> Result<()> {
        self.value["updated_at"] = json!(util::now());
        write_json_atomic(&self.path, &self.value, ".review-", true)
    }
}

fn review_snapshot_present(path: &Path) -> bool {
    fs::metadata(path).is_ok()
}

fn check_owned_regular_snapshot(path: &Path) -> Result<()> {
    let info = fs::symlink_metadata(path)?;
    ensure_owned_regular_snapshot(&info)
}

fn ensure_owned_regular_snapshot(info: &fs::Metadata) -> Result<()> {
    if is_owned_regular_file(info) {
        Ok(())
    } else {
        Err(FlowError::usage(
            "review snapshot must be an owned regular file",
        ))
    }
}

fn is_owned_regular_file(info: &fs::Metadata) -> bool {
    info.file_type().is_file() && info.uid() == current_uid()
}

fn archive_identity_from(path: &Path, current: Option<&J>) -> Result<J> {
    if let Some(value) = current {
        return Ok(value.clone());
    }
    read_archive_identity(path)
}

fn read_archive_identity(path: &Path) -> Result<J> {
    let data = util::read_file_limited(path, 1024 * 1024)?;
    Ok(parse_archive_identity(&data))
}

fn parse_archive_identity(data: &[u8]) -> J {
    serde_json::from_slice::<J>(data)
        .ok()
        .filter(Value::is_object)
        .unwrap_or_else(|| json!({}))
}

fn prepare_history_directory(history: &Path) -> Result<()> {
    fs::create_dir_all(history)?;
    verify_history_directory(history)
}

fn verify_history_directory(history: &Path) -> Result<()> {
    check_owned_directory(history)?;
    set_private_permissions(history)
}

fn check_owned_directory(path: &Path) -> Result<()> {
    let info = fs::symlink_metadata(path)?;
    ensure_owned_directory(&info, "review history must be an owned real directory")
}

fn ensure_owned_directory(info: &fs::Metadata, message: &str) -> Result<()> {
    if is_owned_directory(info) {
        Ok(())
    } else {
        Err(FlowError::usage(message))
    }
}

fn is_owned_directory(info: &fs::Metadata) -> bool {
    info.is_dir() && info.uid() == current_uid()
}

fn set_private_permissions(path: &Path) -> Result<()> {
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

fn rename_archive(path: &Path, history: &Path, identity: &J) -> Result<()> {
    let name = archive_file_name(identity);
    fs::rename(path, history.join(name))?;
    Ok(())
}

fn archive_file_name(identity: &J) -> String {
    let launch = identity_digest(identity, "nonce");
    let thread = identity_digest(identity, "thread_id");
    format!(
        "{}-{}-{}-{}.json",
        &launch[..16],
        &thread[..16],
        util::now_ns(),
        uuid_hex()
    )
}

fn identity_digest(identity: &J, key: &str) -> String {
    sha256_hex(identity_component(identity, key).as_bytes())
}

/// Bounded pending/completed review state with explicit transitions.
pub struct ReviewTracker {
    /// Insertion-ordered pending events keyed by review identity.
    pub pending: Vec<(String, J)>,
    /// Insertion-ordered completed review identities.
    pub completed: Vec<String>,
}

impl Default for ReviewTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl ReviewTracker {
    pub fn new() -> Self {
        Self {
            pending: Vec::new(),
            completed: Vec::new(),
        }
    }

    pub fn reset(&mut self) {
        self.pending.clear();
        self.completed.clear();
    }

    /// Apply one transition. Returns the overflow warning text when the bound
    /// pending set had to evict its oldest entry.
    pub fn apply(&mut self, event: &J) -> Option<String> {
        let status = event.get("status").and_then(Value::as_str).unwrap_or("");
        match review_transition(status) {
            Some("start") => self.start(event),
            Some("finish") => {
                self.finish(event);
                None
            }
            _ => None,
        }
    }

    fn start(&mut self, event: &J) -> Option<String> {
        let review_id = event.get("review_id").and_then(Value::as_str)?;
        if self.completed.iter().any(|item| item == review_id) {
            return None;
        }
        let mut warning = None;
        let exists = self
            .pending
            .iter()
            .any(|(candidate, _)| candidate == review_id);
        // Keep the tracker bounded even if the server never completes.
        if !exists && self.pending.len() >= REVIEW_LIMIT {
            warning = Some("More than 100 native auto-reviews remain pending.".to_string());
            self.pending.remove(0);
        }
        if let Some(position) = self
            .pending
            .iter()
            .position(|(candidate, _)| candidate == review_id)
        {
            self.pending[position].1 = event.clone();
        } else {
            self.pending.push((review_id.to_string(), event.clone()));
        }
        warning
    }

    fn finish(&mut self, event: &J) {
        let Some(review_id) = event.get("review_id").and_then(Value::as_str) else {
            return;
        };
        self.pending.retain(|(candidate, _)| candidate != review_id);
        if !self.completed.iter().any(|item| item == review_id) {
            self.completed.push(review_id.to_string());
            if self.completed.len() > REVIEW_LIMIT {
                self.completed.remove(0);
            }
        }
    }

    /// The sole pending event, if exactly one review is outstanding.
    pub fn unique_pending(&self) -> Option<&J> {
        if self.pending.len() == 1 {
            self.pending.first().map(|(_, event)| event)
        } else {
            None
        }
    }

    /// Close the reviews of one failed/interrupted turn and return the events
    /// that must be recorded, in pending order.
    pub fn abort_turn(&mut self, turn: &J) -> Vec<J> {
        let turn_identity = turn.get("id").cloned().unwrap_or(Value::Null);
        let status = turn.get("status").and_then(Value::as_str).unwrap_or("");
        let matching: Vec<String> = self
            .pending
            .iter()
            .filter(|(_, pending)| pending.get("turn_id") == Some(&turn_identity))
            .map(|(review_id, _)| review_id.clone())
            .collect();
        let mut recorded = Vec::new();
        for review_id in matching {
            let Some(position) = self
                .pending
                .iter()
                .position(|(candidate, _)| candidate == &review_id)
            else {
                continue;
            };
            let mut event = self.pending[position].1.clone();
            event["status"] = json!("aborted");
            event["source"] = json!("turn/completed");
            event["rationale"] = json!(format!(
                "Turn {} while native auto-review remained pending.",
                status
            ));
            event["completed_at_ms"] = json!((util::now() * 1000.0) as i64);
            self.finish(&event);
            recorded.push(event);
        }
        recorded
    }

    pub fn pending_ids(&self) -> Vec<String> {
        self.pending
            .iter()
            .map(|(review_id, _)| review_id.clone())
            .collect()
    }

    pub fn completed_ids(&self) -> Vec<String> {
        self.completed.clone()
    }

    pub fn pending_contains(&self, review_id: &str) -> bool {
        self.pending
            .iter()
            .any(|(candidate, _)| candidate == review_id)
    }

    pub fn completed_contains(&self, review_id: &str) -> bool {
        self.completed
            .iter()
            .any(|candidate| candidate == review_id)
    }
}

/// Receives marker and review-journal updates from an observation session.
///
/// Returning an error aborts observation exactly as a Python exception from
/// `Marker.update` or `ReviewEvents.update` would.
pub trait ObservationSink {
    fn update(&mut self, fields: Map<String, Value>) -> Result<()>;
    fn review(&mut self, thread_id: Option<&str>, event: Option<&J>) -> Result<()>;
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum RequestKey {
    String(String),
    Integer(i64),
}

fn request_key(value: &J) -> Result<RequestKey> {
    match value {
        Value::String(text) => Ok(RequestKey::String(text.clone())),
        Value::Number(number) => match number.as_i64() {
            Some(value) => Ok(RequestKey::Integer(value)),
            None => Err(protocol_error("invalid JSON-RPC request ID")),
        },
        _ => Err(protocol_error("invalid JSON-RPC request ID")),
    }
}

fn supported_thread_method(message: &J) -> Option<&str> {
    let method = message.get("method").and_then(Value::as_str)?;
    select_thread_method(method)
}

fn select_thread_method(method: &str) -> Option<&str> {
    if matches!(method, "thread/start" | "thread/resume" | "thread/fork") {
        Some(method)
    } else {
        None
    }
}

fn has_request_id(message: &J) -> bool {
    message
        .as_object()
        .map(|object| object.contains_key("id"))
        .unwrap_or(false)
}

fn request_params(message: &J) -> Result<Map<String, Value>> {
    let params_value = message.get("params").cloned().unwrap_or_else(|| json!({}));
    object_params(params_value)
}

fn object_params(value: J) -> Result<Map<String, Value>> {
    match value {
        Value::Object(map) => Ok(map),
        _ => Err(protocol_error("thread request params must be an object")),
    }
}

fn request_allowed(method: &str, params: &Map<String, Value>) -> bool {
    method == "thread/resume" || managed_thread_source(params)
}

fn managed_thread_source(params: &Map<String, Value>) -> bool {
    let ephemeral = params.get("ephemeral").and_then(Value::as_bool) == Some(true);
    !ephemeral && thread_source_ok(params)
}

fn thread_source_ok(params: &Map<String, Value>) -> bool {
    params
        .get("threadSource")
        .is_none_or(|source| source == "user" || source.is_null())
}

fn message_request_key(message: &J) -> Result<RequestKey> {
    let identity = message
        .get("id")
        .ok_or_else(|| protocol_error("invalid JSON-RPC request ID"))?;
    request_key(identity)
}

fn is_settings_update(message: &J) -> bool {
    is_notification_method(message, "thread/settings/updated") && !has_request_id(message)
}

fn is_notification_method(message: &J, wanted: &str) -> bool {
    message.get("method").and_then(Value::as_str) == Some(wanted)
}

fn is_request_response(message: &J) -> bool {
    message.get("method").and_then(Value::as_str).is_none() && has_request_id(message)
}

struct PendingRequest {
    sequence: u64,
    method: String,
    params: J,
}

/// Pair native client requests with responses on the same connection.
pub struct SessionObserver {
    sink: Box<dyn ObservationSink>,
    review_enabled: bool,
    tracker: ReviewTracker,
    pending: Vec<(RequestKey, PendingRequest)>,
    sequence: u64,
    session: Option<String>,
    runtime: Option<J>,
    status: String,
}

impl SessionObserver {
    pub fn new(sink: Box<dyn ObservationSink>) -> Result<Self> {
        Self::with_reviews(sink, true)
    }

    /// Build an observer without automatic review observation (the Python
    /// `review_callback=None` form).
    pub fn with_reviews(sink: Box<dyn ObservationSink>, review_enabled: bool) -> Result<Self> {
        let mut observer = Self {
            sink,
            review_enabled,
            tracker: ReviewTracker::new(),
            pending: Vec::new(),
            sequence: 0,
            session: None,
            runtime: None,
            status: "starting".to_string(),
        };
        if review_enabled {
            observer.sink.review(None, None)?;
        }
        Ok(observer)
    }

    pub fn status(&self) -> &str {
        &self.status
    }

    pub fn session(&self) -> Option<&str> {
        self.session.as_deref()
    }

    pub fn runtime(&self) -> Option<&J> {
        self.runtime.as_ref()
    }

    pub fn tracker(&self) -> &ReviewTracker {
        &self.tracker
    }

    pub fn tracker_mut(&mut self) -> &mut ReviewTracker {
        &mut self.tracker
    }

    /// Pending review identities, mirroring Python `observer.reviews.keys()`.
    pub fn reviews(&self) -> Vec<String> {
        self.tracker.pending_ids()
    }

    /// Completed review identities, mirroring Python
    /// `observer.completed_reviews.keys()`.
    pub fn completed_reviews(&self) -> Vec<String> {
        self.tracker.completed_ids()
    }

    fn emit(&mut self, fields: Map<String, Value>) -> Result<()> {
        let previous = self.ready_session();
        self.apply_fields(&fields);
        self.sink.update(fields)?;
        self.finish_emit(previous)
    }

    fn ready_session(&self) -> Option<String> {
        if self.status == "ready" {
            self.session.clone()
        } else {
            None
        }
    }

    fn apply_fields(&mut self, fields: &Map<String, Value>) {
        self.apply_status(fields);
        self.apply_session(fields);
        self.apply_runtime(fields);
    }

    fn apply_status(&mut self, fields: &Map<String, Value>) {
        if let Some(status) = fields.get("status").and_then(Value::as_str) {
            self.status = status.to_string();
        }
    }

    fn apply_session(&mut self, fields: &Map<String, Value>) {
        if let Some(value) = fields.get("session") {
            self.session = value.as_str().map(ToString::to_string);
        }
    }

    fn apply_runtime(&mut self, fields: &Map<String, Value>) {
        if let Some(value) = fields.get("runtime") {
            self.runtime = runtime_value(value);
        }
    }

    fn finish_emit(&mut self, previous: Option<String>) -> Result<()> {
        let current = self.ready_session();
        if current == previous {
            return Ok(());
        }
        self.reset_reviews(current)
    }

    fn reset_reviews(&mut self, current: Option<String>) -> Result<()> {
        self.tracker.reset();
        if !self.review_enabled {
            return Ok(());
        }
        self.review_update(current.as_deref())
    }

    fn review_update(&mut self, session: Option<&str>) -> Result<()> {
        self.sink.review(session, None)
    }

    /// Observe one native request. Helper/ephemeral thread creation is
    /// deliberately not allowed to supersede the managed durable main thread.
    pub fn request(&mut self, message: &J) -> Result<()> {
        let Some(method) = supported_thread_method(message) else {
            return Ok(());
        };
        if !has_request_id(message) {
            return Ok(());
        }
        self.record_request(method, message)
    }

    fn record_request(&mut self, method: &str, message: &J) -> Result<()> {
        let params = request_params(message)?;
        self.enqueue_request(method, message, params)
    }

    fn enqueue_request(
        &mut self,
        method: &str,
        message: &J,
        params: Map<String, Value>,
    ) -> Result<()> {
        if !request_allowed(method, &params) {
            return Ok(());
        }
        self.push_request(method, message, params)
    }

    fn push_request(
        &mut self,
        method: &str,
        message: &J,
        params: Map<String, Value>,
    ) -> Result<()> {
        let key = message_request_key(message)?;
        self.queue_request(key, method, params)
    }

    fn queue_request(
        &mut self,
        key: RequestKey,
        method: &str,
        params: Map<String, Value>,
    ) -> Result<()> {
        self.check_request_capacity()?;
        self.store_request(key, method, params)
    }

    fn check_request_capacity(&self) -> Result<()> {
        if self.pending.len() >= PENDING_REQUEST_LIMIT {
            Err(protocol_error("too many pending thread requests"))
        } else {
            Ok(())
        }
    }

    fn store_request(
        &mut self,
        key: RequestKey,
        method: &str,
        params: Map<String, Value>,
    ) -> Result<()> {
        self.sequence += 1;
        self.pending.retain(|(existing, _)| *existing != key);
        self.pending.push((
            key,
            PendingRequest {
                sequence: self.sequence,
                method: method.to_string(),
                params: Value::Object(params),
            },
        ));
        self.start_request()
    }

    fn start_request(&mut self) -> Result<()> {
        self.emit(object_fields(json!({
            "status": "starting",
            "session": null,
            "runtime": null,
            "error": null
        })))
    }

    /// Observe one native response or server notification.
    pub fn response(&mut self, message: &J) -> Result<()> {
        self.review_notification(message)?;
        self.dispatch_response(message)
    }

    fn dispatch_response(&mut self, message: &J) -> Result<()> {
        if is_settings_update(message) {
            return self.apply_settings_update(message);
        }
        self.dispatch_thread_response(message)
    }

    fn apply_settings_update(&mut self, message: &J) -> Result<()> {
        let params = message.get("params").cloned().unwrap_or_else(|| json!({}));
        self.settings_update(&params)
    }

    fn dispatch_thread_response(&mut self, message: &J) -> Result<()> {
        if is_request_response(message) {
            return self.resolve_response(message);
        }
        Ok(())
    }

    fn resolve_response(&mut self, message: &J) -> Result<()> {
        let Some(identity) = message.get("id") else {
            return Ok(());
        };
        let key = request_key(identity)?;
        self.resolve_pending(message, key)
    }

    fn resolve_pending(&mut self, message: &J, key: RequestKey) -> Result<()> {
        let Some(request) = self.take_pending(&key) else {
            return Ok(());
        };
        // Responses for a superseded request are ignored.
        if request.sequence != self.sequence {
            return Ok(());
        }
        self.finish_request(message, &request)
    }

    fn take_pending(&mut self, key: &RequestKey) -> Option<PendingRequest> {
        let index = self
            .pending
            .iter()
            .position(|(candidate, _)| candidate == key)?;
        Some(self.pending.remove(index).1)
    }

    fn finish_request(&mut self, message: &J, request: &PendingRequest) -> Result<()> {
        if let Some(error) = message.get("error") {
            return self.emit_thread_error(error);
        }
        self.emit_thread_result(message, request)
    }

    fn emit_thread_error(&mut self, error: &J) -> Result<()> {
        self.emit(object_fields(json!({
            "status": "error",
            "session": null,
            "runtime": null,
            "error": {"reason": "thread request failed", "rpc": error}
        })))
    }

    fn emit_thread_result(&mut self, message: &J, request: &PendingRequest) -> Result<()> {
        let result = message.get("result").cloned().unwrap_or(Value::Null);
        if let Ok((session, runtime)) =
            Self::thread_runtime(&result, &request.method, &request.params)
        {
            return self.emit_thread_ready(session, runtime);
        }
        self.emit_thread_failure_from(&result, request)
    }

    fn emit_thread_ready(&mut self, session: String, runtime: J) -> Result<()> {
        self.emit(object_fields(json!({
            "status": "ready",
            "session": session,
            "runtime": runtime,
            "error": null
        })))
    }

    fn emit_thread_failure_from(&mut self, result: &J, request: &PendingRequest) -> Result<()> {
        let error = Self::thread_runtime(result, &request.method, &request.params)
            .err()
            .unwrap_or_else(|| FlowError::usage("thread response has no thread object"));
        self.emit_thread_failure(&error)
    }

    fn emit_thread_failure(&mut self, error: &FlowError) -> Result<()> {
        self.emit(object_fields(json!({
            "status": "error",
            "session": null,
            "runtime": null,
            "error": error.message
        })))
    }

    fn settings_update(&mut self, params: &J) -> Result<()> {
        if self.status != "ready" {
            return Ok(());
        }
        let Some(params) = params.as_object() else {
            return Ok(());
        };
        if params.get("threadId").and_then(Value::as_str) != self.session.as_deref() {
            return Ok(());
        }
        let Some(settings) = params.get("threadSettings").and_then(Value::as_object) else {
            return Ok(());
        };
        let mut runtime = self
            .runtime
            .as_ref()
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        for (key, value) in settings {
            runtime.insert(key.clone(), value.clone());
        }
        if let Some(effort) = settings.get("effort") {
            runtime.insert("reasoningEffort".to_string(), effort.clone());
        }
        if let Some(policy) = settings.get("sandboxPolicy") {
            runtime.insert("sandbox".to_string(), policy.clone());
        }
        if let Some(cwd) = settings.get("cwd") {
            if let Some(thread) = runtime.get("thread").and_then(Value::as_object) {
                let mut thread = thread.clone();
                thread.insert("cwd".to_string(), cwd.clone());
                runtime.insert("thread".to_string(), Value::Object(thread));
            }
        }
        self.emit(object_fields(json!({"runtime": runtime})))
    }

    fn thread_runtime(result: &J, method: &str, params: &J) -> Result<(String, J)> {
        let Some(result) = result.as_object() else {
            return Err(FlowError::usage("thread response has no thread object"));
        };
        let Some(thread) = result.get("thread").and_then(Value::as_object) else {
            return Err(FlowError::usage("thread response has no thread object"));
        };
        let thread_id = thread
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(|| FlowError::usage("thread identity must be a UUID string"))?;
        let session = thread_uuid(thread_id)?;
        if method == "thread/resume" {
            let requested = params
                .get("threadId")
                .and_then(Value::as_str)
                .ok_or_else(|| FlowError::usage("thread identity must be a UUID string"))?;
            if session != thread_uuid(requested)? {
                return Err(FlowError::usage(
                    "resumed thread differs from requested identity",
                ));
            }
        }
        // Exclude turn history and user content from this small control marker.
        let mut runtime = result.clone();
        runtime.remove("thread");
        let mut filtered = Map::new();
        for key in ["id", "sessionId", "cwd", "environments"] {
            if let Some(value) = thread.get(key) {
                filtered.insert(key.to_string(), value.clone());
            }
        }
        runtime.insert("thread".to_string(), Value::Object(filtered));
        Ok((session, Value::Object(runtime)))
    }

    /// Only inspect unsolicited notifications for the bound main thread. The
    /// action object may contain commands or tool arguments; it is never
    /// copied into the audit record, and no approval request is answered.
    fn review_notification(&mut self, message: &J) -> Result<()> {
        if !self.review_enabled
            || message
                .as_object()
                .map(|object| object.contains_key("id"))
                .unwrap_or(false)
            || self.status != "ready"
        {
            return Ok(());
        }
        let Some(params) = message.get("params").and_then(Value::as_object) else {
            return Ok(());
        };
        if params.get("threadId").and_then(Value::as_str) != self.session.as_deref() {
            return Ok(());
        }
        let Some(method) = message.get("method").and_then(Value::as_str) else {
            return Ok(());
        };
        let params = Value::Object(params.clone());
        match method {
            "item/autoApprovalReview/started" | "item/autoApprovalReview/completed" => {
                self.auto_review(&params)
            }
            "guardianWarning" => self.guardian_warning(&params),
            "turn/completed" => self.turn_finished(&params),
            _ => Ok(()),
        }
    }

    fn validated_review(&mut self, params: &J) -> Result<Option<(J, String, String, String)>> {
        let review = params.get("review").filter(|value| value.is_object());
        let review_id = params
            .get("reviewId")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty() && value.chars().count() <= 256);
        let turn_id = params
            .get("turnId")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty() && value.chars().count() <= 256);
        let (Some(review), Some(review_id), Some(turn_id)) = (review, review_id, turn_id) else {
            self.warning(
                "Native auto-review notification has an unsupported shape; inspect the native transcript.",
                false,
            )?;
            return Ok(None);
        };
        let Some(status) = review.get("status").and_then(Value::as_str) else {
            self.warning(
                "Native auto-review notification has an unsupported status; inspect the native transcript.",
                false,
            )?;
            return Ok(None);
        };
        if review_transition(status).is_none() {
            self.warning(
                "Native auto-review notification has an unsupported status; inspect the native transcript.",
                false,
            )?;
            return Ok(None);
        }
        Ok(Some((
            review.clone(),
            review_id.to_string(),
            turn_id.to_string(),
            status.to_string(),
        )))
    }

    fn review_event(
        &self,
        params: &J,
        review: &J,
        review_id: &str,
        turn_id: &str,
        status: &str,
    ) -> J {
        let mut event = json!({
            "kind": "autoApprovalReview",
            "thread_id": self.session,
            "review_id": review_id,
            "turn_id": turn_id,
            "status": status,
        });
        for (source, target, limit) in [
            ("rationale", "rationale", 1000usize),
            ("riskLevel", "risk_level", 100usize),
            ("userAuthorization", "user_authorization", 100usize),
        ] {
            if let Some(value) = review.get(source).and_then(Value::as_str) {
                event[target] = json!(truncate_chars(value, limit));
            }
        }
        for (source, target) in [
            ("startedAtMs", "started_at_ms"),
            ("completedAtMs", "completed_at_ms"),
        ] {
            if let Some(value) = params.get(source).and_then(Value::as_i64) {
                event[target] = json!(value);
            }
        }
        if let Some(value) = params.get("decisionSource").and_then(Value::as_str) {
            event["decision_source"] = json!(truncate_chars(value, 100));
        }
        event
    }

    fn auto_review(&mut self, params: &J) -> Result<()> {
        let Some((review, review_id, turn_id, status)) = self.validated_review(params)? else {
            return Ok(());
        };
        let event = self.review_event(params, &review, &review_id, &turn_id, &status);
        if let Some(warning) = self.tracker.apply(&event) {
            self.warning(&warning, false)?;
        }
        self.record_review(event)
    }

    fn turn_finished(&mut self, params: &J) -> Result<()> {
        let Some(turn) = params.get("turn").filter(|value| value.is_object()) else {
            return Ok(());
        };
        if !matches!(
            turn.get("status").and_then(Value::as_str),
            Some("interrupted" | "failed")
        ) {
            return Ok(());
        }
        let events = self.tracker.abort_turn(turn);
        for event in events {
            self.record_review(event)?;
        }
        Ok(())
    }

    fn guardian_warning(&mut self, params: &J) -> Result<()> {
        if let Some(message) = params.get("message").and_then(Value::as_str) {
            self.warning(message, true)?;
        }
        Ok(())
    }

    pub fn warning(&mut self, message: &str, associate: bool) -> Result<()> {
        let digest = sha256_hex(message.as_bytes());
        let mut review_id = format!("warning:{digest}");
        let mut turn_id = Value::Null;
        let mut related = None;
        if associate {
            if let Some(pending) = self.tracker.unique_pending().cloned() {
                if let Some(pending_id) = pending.get("review_id").and_then(Value::as_str) {
                    related = Some(pending_id.to_string());
                    turn_id = pending.get("turn_id").cloned().unwrap_or(Value::Null);
                    // The same warning text can occur for multiple distinct
                    // reviews.
                    review_id = format!("{review_id}:{pending_id}");
                }
            }
        }
        let mut event = json!({
            "kind": "guardianWarning",
            "thread_id": self.session,
            "turn_id": turn_id,
            "review_id": review_id,
            "status": "guardianWarning",
            "rationale": truncate_chars(message, 1000),
        });
        if let Some(related) = related {
            event["related_review_id"] = json!(related);
        }
        self.record_review(event)
    }

    pub fn record_review(&mut self, mut event: J) -> Result<()> {
        let identity = json!([
            event.get("thread_id").cloned().unwrap_or(Value::Null),
            event.get("review_id").cloned().unwrap_or(Value::Null),
            event.get("status").cloned().unwrap_or(Value::Null)
        ]);
        let digest = sha256_hex(dumps(&identity, true, Separators::Compact).as_bytes());
        event["event_id"] = json!(digest);
        event["observed_at"] = json!(util::now());
        if self.review_enabled {
            let thread_id = self.session.clone();
            self.sink.review(thread_id.as_deref(), Some(&event))?;
        }
        Ok(())
    }
}

/// Marker plus review journal binding used by the proxy.
#[derive(Clone)]
pub struct RpcObservation {
    pub marker: Rc<RefCell<Marker>>,
    pub review_events: Rc<RefCell<ReviewEvents>>,
}

impl RpcObservation {
    pub fn new(marker: Rc<RefCell<Marker>>, review_events: Rc<RefCell<ReviewEvents>>) -> Self {
        Self {
            marker,
            review_events,
        }
    }

    pub fn build(&self) -> Result<SessionObserver> {
        SessionObserver::new(Box::new(self.clone()))
    }
}

impl ObservationSink for RpcObservation {
    fn update(&mut self, fields: Map<String, Value>) -> Result<()> {
        self.marker.borrow_mut().update(fields)
    }

    fn review(&mut self, thread_id: Option<&str>, event: Option<&J>) -> Result<()> {
        self.review_events.borrow_mut().update(thread_id, event)
    }
}

// ---------------------------------------------------------------------------
// Signals
// ---------------------------------------------------------------------------

static STOP_SIGNAL: AtomicI32 = AtomicI32::new(0);
// The handler owns no process identity. The bridge's exclusive Child owner
// forwards the request after checking whether it has already reaped the child.
extern "C" fn forward_signal(signum: libc::c_int) {
    STOP_SIGNAL.store(signum, Ordering::SeqCst);
}

type PreviousSignals = Vec<(libc::c_int, libc::sigaction)>;

fn set_signal_action(signum: libc::c_int, handler: libc::sighandler_t) -> Result<libc::sigaction> {
    // SAFETY: Linux sigaction is a C record whose fields admit zero. We then
    // initialize the handler, flags and signal mask before installation.
    let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
    action.sa_sigaction = handler;
    action.sa_flags = libc::SA_RESTART;
    // SAFETY: sa_mask points to live writable sigset_t storage in action.
    unsafe {
        libc::sigemptyset(&mut action.sa_mask);
    }
    // SAFETY: all-zero is valid storage for this Linux C record; sigaction
    // fills the previous disposition on success before we use it.
    let mut previous: libc::sigaction = unsafe { std::mem::zeroed() };
    // SAFETY: both records live through the call. Callers pass either our
    // extern-C one-int signal handler or a previously obtained disposition.
    let result = unsafe { libc::sigaction(signum, &action, &mut previous) };
    if result != 0 {
        return Err(FlowError::io(format!(
            "cannot install signal handler: {}",
            std::io::Error::last_os_error()
        )));
    }
    Ok(previous)
}

fn restore_signals(previous: &PreviousSignals) {
    for (signum, action) in previous {
        // SAFETY: action is the saved disposition from a successful sigaction;
        // it stays live during restoration, and null omits the output record.
        unsafe {
            libc::sigaction(*signum, action, std::ptr::null_mut());
        }
    }
}

fn install_signals() -> Result<PreviousSignals> {
    STOP_SIGNAL.store(0, Ordering::SeqCst);
    let mut previous = Vec::new();
    for signum in [libc::SIGTERM, libc::SIGHUP] {
        match set_signal_action(signum, forward_signal as *const () as libc::sighandler_t) {
            Ok(action) => previous.push((signum, action)),
            Err(error) => {
                restore_signals(&previous);
                return Err(error);
            }
        }
    }
    match set_signal_action(libc::SIGINT, libc::SIG_IGN) {
        Ok(action) => previous.push((libc::SIGINT, action)),
        Err(error) => {
            restore_signals(&previous);
            return Err(error);
        }
    }
    Ok(previous)
}

// ---------------------------------------------------------------------------
// Proxy bridge
// ---------------------------------------------------------------------------

struct Endpoint {
    socket: UnixStream,
    buffer: Vec<u8>,
    wire: Option<WireObserver>,
    eof: bool,
}

struct PollBatch {
    fds: Vec<libc::pollfd>,
    targets: Vec<usize>,
    listener_index: Option<usize>,
}

impl PollBatch {
    fn new() -> Self {
        Self {
            fds: Vec::new(),
            targets: Vec::new(),
            listener_index: None,
        }
    }

    fn add_listener(&mut self, listener: Option<&UnixListener>) {
        let Some(listener) = listener else {
            return;
        };
        self.listener_index = Some(self.fds.len());
        self.targets.push(usize::MAX);
        self.fds.push(libc::pollfd {
            fd: listener.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        });
    }

    fn add_endpoint_poll(&mut self, index: usize, endpoint: &Option<Endpoint>) {
        let Some(endpoint) = endpoint.as_ref() else {
            return;
        };
        let events = endpoint_events(endpoint);
        if events == 0 {
            return;
        }
        self.targets.push(index);
        self.fds.push(libc::pollfd {
            fd: endpoint.socket.as_raw_fd(),
            events,
            revents: 0,
        });
    }
}

fn endpoint_events(endpoint: &Endpoint) -> i16 {
    read_events(endpoint) | write_events(endpoint)
}

fn read_events(endpoint: &Endpoint) -> i16 {
    if endpoint.eof {
        0
    } else {
        libc::POLLIN
    }
}

fn write_events(endpoint: &Endpoint) -> i16 {
    if endpoint.buffer.is_empty() {
        0
    } else {
        libc::POLLOUT
    }
}

fn read_event(revents: i16) -> bool {
    revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0
}

fn write_event(revents: i16) -> bool {
    revents & libc::POLLOUT != 0
}

fn eventful(read: bool, write: bool) -> bool {
    read || write
}

fn protocol_message(error: &FlowError) -> Option<String> {
    if error.kind == ErrorKind::RpcProtocol {
        Some(error.message.clone())
    } else {
        None
    }
}

fn poll_failure() -> Result<Option<PollBatch>> {
    let error = std::io::Error::last_os_error();
    if error.kind() == IoErrorKind::Interrupted {
        return Ok(None);
    }
    Err(FlowError::io(format!("proxy poll failed: {error}")))
}

fn interpret_poll(batch: PollBatch, result: libc::c_int) -> Option<PollBatch> {
    if result == 0 {
        return None;
    }
    Some(batch)
}

fn serve_continue(service: &mut ProxyBridge, outcome: &Result<()>) -> bool {
    outcome.is_ok() && service.serve_should_continue()
}

fn executable_access(candidate: &Path) -> bool {
    let metadata = match fs::metadata(candidate) {
        Ok(metadata) => metadata,
        Err(_) => return false,
    };
    if !metadata.is_file() {
        return false;
    }
    let Ok(path) = CString::new(candidate.as_os_str().as_bytes()) else {
        return false;
    };
    // SAFETY: path is NUL-terminated and lives through access; X_OK is a valid
    // scalar mode. This is an access probe, not a guarantee about a later exec.
    unsafe { libc::access(path.as_ptr(), libc::X_OK) == 0 }
}

fn executable_path(command: &str) -> Option<String> {
    if command.is_empty() {
        return None;
    }
    if command.contains('/') {
        let path = Path::new(command);
        return executable_access(path).then(|| command.to_string());
    }
    let path_var =
        std::env::var_os("PATH").unwrap_or_else(|| std::ffi::OsString::from("/bin:/usr/bin"));
    for directory in std::env::split_paths(&path_var) {
        let candidate = directory.join(command);
        if executable_access(&candidate) {
            return Some(candidate.to_string_lossy().to_string());
        }
    }
    None
}

#[cfg(test)]
mod executable_tests {
    use super::*;

    #[test]
    fn executable_lookup_checks_absolute_paths_path_search_and_access() {
        assert_eq!(executable_path(""), None);
        assert_eq!(executable_path("/nonexistent/herdr-native"), None);
        assert_eq!(executable_path("/bin/sh"), Some("/bin/sh".into()));
        assert!(executable_path("sh").is_some());
        assert_eq!(
            executable_path("nonexistent-herdr-native-acceptance-20261020"),
            None
        );
        assert!(!executable_access(Path::new("/bin")));
        let file = tempfile::NamedTempFile::new().unwrap();
        assert!(!executable_access(file.path()));
    }
}

/// Nonblocking UNIX connect with the original two second upstream deadline.
fn connect_unix_stream(path: &Path, timeout: Duration) -> Result<UnixStream> {
    crate::codex::rpc::validate_socket(&path.display().to_string())?;
    let fd = open_upstream_socket()?;
    let result = connect_socket(fd, path, timeout);
    finish_connect(fd, result)
}

fn open_upstream_socket() -> Result<RawFd> {
    // SAFETY: socket takes scalar protocol/flag arguments. A successful result
    // is owned by connect_unix_stream until finish_connect transfers/closes it.
    let fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0) };
    if fd < 0 {
        return Err(socket_error("cannot create upstream socket"));
    }
    Ok(fd)
}

fn socket_error(context: &str) -> FlowError {
    FlowError::io(format!("{context}: {}", std::io::Error::last_os_error()))
}

fn finish_connect(fd: RawFd, result: Result<()>) -> Result<UnixStream> {
    if let Err(error) = result {
        close_socket(fd);
        return Err(error);
    }
    // SAFETY: open_upstream_socket created this owned fd and all error paths
    // above close it. Transfer its sole ownership to the stream exactly once.
    let stream = unsafe { UnixStream::from_raw_fd(fd) };
    crate::codex::peer::verify(&stream)?;
    Ok(stream)
}

fn close_socket(fd: RawFd) {
    // SAFETY: the connection error path owns this raw fd and has not converted
    // it into UnixStream. It is closed once and never used afterward.
    unsafe {
        libc::close(fd);
    }
}

fn connect_socket(fd: RawFd, path: &Path, timeout: Duration) -> Result<()> {
    configure_nonblocking(fd)?;
    connect_address(fd, path, timeout)
}

fn configure_nonblocking(fd: RawFd) -> Result<()> {
    let flags = socket_flags(fd)?;
    set_nonblocking(fd, flags)
}

fn socket_flags(fd: RawFd) -> Result<libc::c_int> {
    // SAFETY: the connect path owns fd for this call; F_GETFL takes no pointer
    // and neither closes the descriptor nor transfers ownership.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 {
        return Err(socket_error("cannot configure upstream socket"));
    }
    Ok(flags)
}

fn set_nonblocking(fd: RawFd, flags: libc::c_int) -> Result<()> {
    // SAFETY: fd is still owned by the connect path; F_SETFL receives the
    // previously queried flags plus O_NONBLOCK and no pointer argument.
    let result = unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) };
    if result < 0 {
        return Err(socket_error("cannot configure upstream socket"));
    }
    Ok(())
}

fn connect_address(fd: RawFd, path: &Path, timeout: Duration) -> Result<()> {
    let (address, length) = unix_address(path)?;
    start_connect(fd, &address, length, timeout)
}

fn unix_address(path: &Path) -> Result<(libc::sockaddr_un, libc::socklen_t)> {
    let bytes = path.as_os_str().as_bytes();
    let (name, abstract_socket) = socket_name(bytes);
    let address = fill_address(name, abstract_socket)?;
    let length = address_length(name, abstract_socket);
    Ok((address, length))
}

fn socket_name(bytes: &[u8]) -> (&[u8], bool) {
    match bytes.first() {
        Some(&0) => (&bytes[1..], true),
        _ => (bytes, false),
    }
}

fn fill_address(name: &[u8], abstract_socket: bool) -> Result<libc::sockaddr_un> {
    // SAFETY: this Linux C record contains only integer/byte fields, so zero is
    // valid. Family and a capacity-checked name are initialized before connect.
    let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    address.sun_family = libc::AF_UNIX as libc::sa_family_t;
    let capacity = address.sun_path.len();
    check_socket_name_length(name, capacity)?;
    copy_socket_name(&mut address, name, abstract_socket);
    Ok(address)
}

fn check_socket_name_length(name: &[u8], capacity: usize) -> Result<()> {
    if name.len() >= capacity {
        return Err(FlowError::io(
            "app-server socket path exceeds the UNIX socket limit".to_string(),
        ));
    }
    Ok(())
}

fn copy_socket_name(address: &mut libc::sockaddr_un, name: &[u8], abstract_socket: bool) {
    let offset = usize::from(abstract_socket);
    for (index, byte) in name.iter().enumerate() {
        address.sun_path[index + offset] = *byte as libc::c_char;
    }
}

fn address_length(name: &[u8], abstract_socket: bool) -> libc::socklen_t {
    let offset = sockaddr_path_offset();
    if abstract_socket {
        (offset + 1 + name.len()) as libc::socklen_t
    } else {
        (offset + name.len() + 1) as libc::socklen_t
    }
}

fn sockaddr_path_offset() -> usize {
    // SAFETY: sockaddr_un admits zero; it is used solely to compute addresses
    // of fields within this live record, without dereferencing raw pointers.
    let address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    (&address.sun_path as *const _ as usize) - (&address as *const _ as usize)
}

fn start_connect(
    fd: RawFd,
    address: &libc::sockaddr_un,
    length: libc::socklen_t,
    timeout: Duration,
) -> Result<()> {
    // SAFETY: unix_address supplies a live sockaddr_un and a length within its
    // storage. fd remains owned by the connect path while connect borrows it.
    let result = unsafe { libc::connect(fd, address as *const _ as *const libc::sockaddr, length) };
    if result == 0 {
        return Ok(());
    }
    handle_connect_error(fd, timeout)
}

fn handle_connect_error(fd: RawFd, timeout: Duration) -> Result<()> {
    let error = std::io::Error::last_os_error();
    if connect_in_progress(&error) {
        return wait_for_connect(fd, timeout);
    }
    Err(FlowError::io(format!("upstream connect failed: {error}")))
}

fn connect_in_progress(error: &std::io::Error) -> bool {
    matches!(
        error.raw_os_error(),
        Some(libc::EINPROGRESS) | Some(libc::EALREADY)
    )
}

fn wait_for_connect(fd: RawFd, timeout: Duration) -> Result<()> {
    let result = poll_writable(fd, timeout)?;
    require_writable(fd, result)
}

fn poll_writable(fd: RawFd, timeout: Duration) -> Result<libc::c_int> {
    retry_interrupted_poll(timeout, &mut |remaining| poll_once(fd, remaining))
        .map_err(|error| FlowError::io(format!("upstream connect poll failed: {error}")))
}

fn poll_once(fd: RawFd, timeout: Duration) -> std::io::Result<libc::c_int> {
    let mut poll_fd = libc::pollfd {
        fd,
        events: libc::POLLOUT,
        revents: 0,
    };
    // SAFETY: poll_fd is one live writable element; fd stays owned by the
    // connect path and poll only borrows it.
    let result = unsafe { libc::poll(&mut poll_fd, 1, poll_millis(timeout)) };
    if result >= 0 {
        Ok(result)
    } else {
        Err(std::io::Error::last_os_error())
    }
}

fn retry_interrupted_poll(
    timeout: Duration,
    poll: &mut impl FnMut(Duration) -> std::io::Result<libc::c_int>,
) -> std::io::Result<libc::c_int> {
    let started = Instant::now();
    let mut remaining = timeout;
    loop {
        match poll(remaining) {
            Err(error) if error.kind() == IoErrorKind::Interrupted => {
                remaining = timeout.saturating_sub(started.elapsed());
                if remaining.is_zero() {
                    return Ok(0);
                }
            }
            result => return result,
        }
    }
}

fn poll_millis(timeout: Duration) -> libc::c_int {
    timeout.as_millis().min(i32::MAX as u128) as libc::c_int
}

fn require_writable(fd: RawFd, result: libc::c_int) -> Result<()> {
    if result == 0 {
        return Err(FlowError::io("upstream connect timed out".to_string()));
    }
    verify_socket_connected(fd)
}

fn verify_socket_connected(fd: RawFd) -> Result<()> {
    let socket_error = read_socket_error(fd)?;
    require_connected(socket_error)
}

fn read_socket_error(fd: RawFd) -> Result<libc::c_int> {
    let mut code: libc::c_int = 0;
    let mut length = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
    // SAFETY: code and length are writable, correctly sized outputs that live
    // through getsockopt. SO_ERROR writes a c_int; fd remains borrowed/open.
    let result = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_ERROR,
            &mut code as *mut _ as *mut libc::c_void,
            &mut length,
        )
    };
    if result < 0 {
        return Err(socket_error("cannot inspect upstream connection"));
    }
    Ok(code)
}

fn require_connected(socket_error: libc::c_int) -> Result<()> {
    if socket_error != 0 {
        return Err(FlowError::io(format!(
            "upstream connect failed: {}",
            std::io::Error::from_raw_os_error(socket_error)
        )));
    }
    Ok(())
}

/// Transparent, observed byte proxy between the native TUI and app-server.
pub struct ProxyBridge {
    policy: Option<Box<dyn BridgePolicy>>,
    root: PathBuf,
    server_socket: PathBuf,
    binary: String,
    cwd: String,
    nonce: String,
    thread: Option<String>,
    child: Option<Child>,
    listener: Option<UnixListener>,
    lock_file: Option<File>,
    directory: Option<File>,
    proxy_path: PathBuf,
    proxy_endpoint: Option<String>,
    proxy_identity: Option<(u64, u64)>,
    marker: Option<Rc<RefCell<Marker>>>,
    review_events: Option<Rc<RefCell<ReviewEvents>>>,
    observation: Option<RpcObservation>,
    endpoints: [Option<Endpoint>; 2],
}

impl ProxyBridge {
    pub fn new(
        root: impl AsRef<Path>,
        server_socket: impl AsRef<Path>,
        binary: &str,
        cwd: &str,
        nonce: &str,
        thread: Option<&str>,
    ) -> Self {
        let root = absolute_lexical(root.as_ref());
        let proxy_path = root.join("tui.sock");
        Self {
            policy: None,
            root,
            server_socket: server_socket.as_ref().to_path_buf(),
            binary: binary.to_string(),
            cwd: cwd.to_string(),
            nonce: nonce.to_string(),
            thread: thread.map(ToString::to_string),
            child: None,
            listener: None,
            lock_file: None,
            directory: None,
            proxy_path,
            proxy_endpoint: None,
            proxy_identity: None,
            marker: None,
            review_events: None,
            observation: None,
            endpoints: [None, None],
        }
    }

    pub fn proxy_socket(&self) -> &Path {
        &self.proxy_path
    }

    pub fn with_policy(mut self, policy: Box<dyn BridgePolicy>) -> Self {
        self.policy = Some(policy);
        self
    }

    pub fn proxy_endpoint(&self) -> Option<&str> {
        self.proxy_endpoint.as_deref()
    }

    pub fn marker_value(&self) -> Option<J> {
        self.marker
            .as_ref()
            .map(|marker| marker.borrow().value().clone())
    }

    pub fn review_events_value(&self) -> Option<J> {
        self.review_events
            .as_ref()
            .map(|events| events.borrow().value().clone())
    }

    pub fn child_pid(&self) -> Option<u32> {
        self.child.as_ref().map(Child::id)
    }

    pub fn is_connected(&self) -> bool {
        self.endpoints.iter().all(Option::is_some)
    }

    #[cfg(test)]
    pub(crate) fn test_transfer(&mut self, index: usize, read: bool, write: bool) -> Result<()> {
        self.transfer(index, read, write)
    }

    #[cfg(test)]
    pub(crate) fn test_iteration(&mut self) -> Result<()> {
        self.serve_iteration()
    }

    fn validate_inputs(&mut self) -> Result<()> {
        if self.nonce.is_empty() || self.nonce.chars().count() > 128 {
            return Err(FlowError::usage("nonce must contain 1 to 128 characters"));
        }
        if let Some(thread) = self.thread.clone() {
            if thread.is_empty() {
                self.thread = None;
            } else {
                self.thread = Some(thread_uuid(&thread)?);
            }
        }
        Ok(())
    }

    fn acquire_directory(&mut self) -> Result<()> {
        self.ensure_state_directory()?;
        self.open_directory_lock()
    }

    fn ensure_state_directory(&self) -> Result<()> {
        create_state_directory(&self.root)?;
        prepare_state_directory(&self.root)
    }

    fn open_directory_lock(&mut self) -> Result<()> {
        let directory_fd = util::open_dir_fd(&self.root)?;
        self.install_directory_lock(directory_fd)
    }

    fn install_directory_lock(&mut self, directory_fd: RawFd) -> Result<()> {
        // SAFETY: open_directory_lock obtained this newly owned descriptor from
        // open_dir_fd. No other File owns it; this is its sole ownership transfer.
        let directory = unsafe { File::from_raw_fd(directory_fd) };
        let lock_file = open_lock_file(&directory)?;
        self.attach_directory(directory, lock_file)
    }

    fn attach_directory(&mut self, directory: File, lock_file: File) -> Result<()> {
        lock_exclusive(&lock_file)?;
        self.directory = Some(directory);
        self.lock_file = Some(lock_file);
        Ok(())
    }

    fn setup_observation(&mut self) -> Result<()> {
        let server_socket = self.server_socket.to_string_lossy().to_string();
        let marker = Rc::new(RefCell::new(Marker::new(
            &self.root,
            &self.nonce,
            &server_socket,
        )));
        self.marker = Some(marker.clone());
        marker.borrow_mut().update(Map::new())?;
        let review_events = Rc::new(RefCell::new(ReviewEvents::new(&self.root, &self.nonce)?));
        self.review_events = Some(review_events.clone());
        self.observation = Some(RpcObservation::new(marker, review_events));
        Ok(())
    }

    fn bind_proxy_socket(&mut self) -> Result<()> {
        remove_stale_proxy_socket(&self.proxy_path)?;
        self.create_proxy_listener()
    }

    fn create_proxy_listener(&mut self) -> Result<()> {
        let real_path = self.proxy_path.to_string_lossy().to_string();
        let bind_path = self.socket_bind_path(&real_path)?;
        self.open_proxy_listener(bind_path, real_path)
    }

    fn socket_bind_path(&self, real_path: &str) -> Result<String> {
        if real_path.len() > MAX_SOCKET_PATH {
            return self.live_directory_alias();
        }
        Ok(real_path.to_string())
    }

    fn live_directory_alias(&self) -> Result<String> {
        let directory_fd = self
            .directory
            .as_ref()
            .map(AsRawFd::as_raw_fd)
            .ok_or_else(|| FlowError::io("state directory is unavailable"))?;
        Ok(format!(
            "/proc/{}/fd/{}/tui.sock",
            std::process::id(),
            directory_fd
        ))
    }

    fn open_proxy_listener(&mut self, bind_path: String, real_path: String) -> Result<()> {
        let endpoint = format!("unix://{bind_path}");
        self.proxy_endpoint = Some(endpoint.clone());
        let listener = bind_listener(&bind_path)?;
        self.finish_proxy_listener(listener, endpoint, real_path)
    }

    fn finish_proxy_listener(
        &mut self,
        listener: UnixListener,
        endpoint: String,
        real_path: String,
    ) -> Result<()> {
        let info = fs::symlink_metadata(&self.proxy_path)?;
        self.proxy_identity = Some((info.dev(), info.ino()));
        self.activate_proxy_listener(listener, endpoint, real_path)
    }

    fn activate_proxy_listener(
        &mut self,
        listener: UnixListener,
        endpoint: String,
        real_path: String,
    ) -> Result<()> {
        prepare_proxy_listener(&self.proxy_path, &listener)?;
        self.record_listener(listener, endpoint, real_path)
    }

    fn record_listener(
        &mut self,
        listener: UnixListener,
        endpoint: String,
        real_path: String,
    ) -> Result<()> {
        listener.set_nonblocking(true)?;
        self.listener = Some(listener);
        self.marker_update(object_fields(json!({
            "proxy_socket": real_path,
            "proxy_endpoint": endpoint
        })))
    }

    pub fn prepare(&mut self) -> Result<()> {
        self.validate_inputs()?;
        self.acquire_directory()?;
        self.setup_observation()?;
        self.bind_proxy_socket()
    }

    pub fn start(&mut self) -> Result<()> {
        let Some(binary) = executable_path(&self.binary) else {
            return Err(FlowError::usage(
                "native Codex binary does not exist or is not executable",
            ));
        };
        let Some(endpoint) = self.proxy_endpoint.clone() else {
            return Err(FlowError::io("proxy endpoint is unavailable"));
        };
        let mut argv = vec![binary.clone()];
        if let Some(thread) = &self.thread {
            argv.push("resume".to_string());
            argv.push(thread.clone());
        }
        argv.push("--remote".to_string());
        argv.push(endpoint);
        if self
            .policy
            .as_ref()
            .is_none_or(|policy| policy.explicit_cwd())
        {
            argv.push("--cd".to_string());
            argv.push(self.cwd.clone());
        }
        if let Some(policy) = &self.policy {
            argv.extend(policy.arguments());
        }
        // Native terminal I/O and controlling PTY are inherited unchanged.
        let child = Command::new(&argv[0])
            .args(&argv[1..])
            .spawn()
            .map_err(|error| FlowError::io(format!("cannot launch native TUI: {error}")))?;
        let pid = child.id();
        self.child = Some(child);
        let real_binary = util::resolve_path(Path::new(&binary));
        self.marker_update(object_fields(json!({
            "pid": pid,
            "pid_start_ticks": start_ticks_value(pid),
            "binary": real_binary.to_string_lossy()
        })))
    }

    fn child_running(&mut self) -> bool {
        let Some(child) = self.child.as_mut() else {
            return false;
        };
        match child.try_wait() {
            Ok(Some(_)) => false,
            Ok(None) => true,
            Err(_) => false,
        }
    }

    fn marker_update(&mut self, fields: Map<String, Value>) -> Result<()> {
        let Some(marker) = self.marker.as_ref() else {
            return Ok(());
        };
        marker.borrow_mut().update(fields)
    }

    fn marker_error(&mut self, message: &str, include_runtime: bool) -> Result<()> {
        let mut fields = Map::new();
        fields.insert("status".to_string(), json!("error"));
        fields.insert("session".to_string(), Value::Null);
        if include_runtime {
            fields.insert("runtime".to_string(), Value::Null);
        }
        fields.insert("error".to_string(), json!(message));
        self.marker_update(fields)
    }

    /// Accept one native connection and open the paired upstream connection.
    pub fn accept(&mut self) -> Result<()> {
        let Some(downstream) = self.accept_downstream()? else {
            return Ok(());
        };
        self.install_endpoints(downstream)
    }

    fn accept_downstream(&mut self) -> Result<Option<UnixStream>> {
        let Some(listener) = self.listener.as_ref() else {
            return Err(FlowError::io("proxy listener is unavailable"));
        };
        accept_result(listener.accept())
    }

    fn install_endpoints(&mut self, downstream: UnixStream) -> Result<()> {
        if self.endpoints.iter().any(Option::is_some) {
            return Ok(());
        }
        self.connect_endpoints(downstream)
    }

    fn connect_endpoints(&mut self, downstream: UnixStream) -> Result<()> {
        downstream.set_nonblocking(true)?;
        self.connect_upstream(downstream)
    }

    fn connect_upstream(&mut self, downstream: UnixStream) -> Result<()> {
        let upstream = connect_unix_stream(&self.server_socket, UPSTREAM_CONNECT_TIMEOUT)?;
        self.attach_endpoints(downstream, upstream)
    }

    fn attach_endpoints(&mut self, downstream: UnixStream, upstream: UnixStream) -> Result<()> {
        if let Some(policy) = &mut self.policy {
            policy.connected();
        }
        let observer = self.build_wire_observer()?;
        self.store_endpoints(downstream, upstream, observer);
        Ok(())
    }

    fn build_wire_observer(&self) -> Result<Rc<RefCell<SessionObserver>>> {
        let observation = self
            .observation
            .as_ref()
            .ok_or_else(|| FlowError::io("proxy observation is unavailable"))?;
        build_observer(observation)
    }

    fn store_endpoints(
        &mut self,
        downstream: UnixStream,
        upstream: UnixStream,
        observer: Rc<RefCell<SessionObserver>>,
    ) {
        let limit = self.direction_limit(1);
        self.endpoints[0] = Some(Endpoint {
            socket: downstream,
            buffer: Vec::new(),
            wire: Some(WireObserver::with_limit(
                true,
                observer.clone(),
                MAX_MESSAGE,
            )),
            eof: false,
        });
        self.endpoints[1] = Some(Endpoint {
            socket: upstream,
            buffer: Vec::new(),
            wire: Some(WireObserver::with_limit(false, observer, limit)),
            eof: false,
        });
    }

    fn pair_mut(&mut self, index: usize) -> Option<(&mut Endpoint, &mut Endpoint)> {
        let (left, right) = self.endpoints.split_at_mut(1);
        if index == 0 {
            Some((left[0].as_mut()?, right[0].as_mut()?))
        } else {
            Some((right[0].as_mut()?, left[0].as_mut()?))
        }
    }

    fn read_transfer(&mut self, index: usize) -> Result<()> {
        let Some(endpoint) = self.endpoints[index].as_mut() else {
            return Ok(());
        };
        let mut buffer = [0u8; 65_536];
        match endpoint.socket.read(&mut buffer) {
            Ok(0) => self.read_eof(index),
            Ok(count) => {
                let data = buffer[..count].to_vec();
                self.read_data(index, &data)
            }
            Err(error)
                if matches!(
                    error.kind(),
                    IoErrorKind::WouldBlock | IoErrorKind::Interrupted
                ) =>
            {
                Ok(())
            }
            Err(error) => Err(FlowError::io(format!("proxy read failed: {error}"))),
        }
    }

    fn read_eof(&mut self, index: usize) -> Result<()> {
        {
            let Some((endpoint, peer)) = self.pair_mut(index) else {
                return Ok(());
            };
            endpoint.eof = true;
            endpoint.wire = None;
            peer.wire = None;
        }
        self.marker_error("native WebSocket connection ended", false)?;
        let shutdown = {
            let Some((_, peer)) = self.pair_mut(index) else {
                return Ok(());
            };
            if peer.buffer.is_empty() {
                Some(peer.socket.shutdown(std::net::Shutdown::Write))
            } else {
                None
            }
        };
        if let Some(result) = shutdown {
            result
                .map_err(|error| FlowError::io(format!("cannot shut down proxy peer: {error}")))?;
        }
        let disconnect = {
            let Some((endpoint, peer)) = self.pair_mut(index) else {
                return Ok(());
            };
            peer.eof && endpoint.buffer.is_empty() && peer.buffer.is_empty()
        };
        if disconnect {
            self.disconnect(index, None)?;
        }
        Ok(())
    }

    fn read_data(&mut self, index: usize, data: &[u8]) -> Result<()> {
        let filtered;
        let mut responses = Vec::new();
        let data = if let Some(policy) = &mut self.policy {
            filtered = policy.forward(index == 0, data)?;
            responses = policy.take_responses();
            filtered.as_slice()
        } else {
            data
        };
        let response_limit = self.direction_limit(1);
        if let Some(endpoint) = self.endpoints[0].as_mut() {
            endpoint.buffer.extend(responses);
            check_forward_buffer(&endpoint.buffer, response_limit)?;
        }
        self.buffer_incoming(index, data)?;
        self.feed_wire(index, data)
    }

    fn direction_limit(&self, index: usize) -> usize {
        if index == 0 {
            return MAX_MESSAGE;
        }
        self.policy
            .as_ref()
            .map_or(NATIVE_SERVER_MESSAGE_LIMIT, |policy| policy.message_limit())
    }

    fn buffer_incoming(&mut self, index: usize, data: &[u8]) -> Result<()> {
        let limit = self.direction_limit(index);
        let Some((_, peer)) = self.pair_mut(index) else {
            return Ok(());
        };
        peer.buffer.extend_from_slice(data);
        check_forward_buffer(&peer.buffer, limit)
    }

    fn feed_wire(&mut self, index: usize, data: &[u8]) -> Result<()> {
        if let Err(error) = self.feed_endpoint(index, data) {
            return self.handle_feed_error(index, error);
        }
        Ok(())
    }

    fn feed_endpoint(&mut self, index: usize, data: &[u8]) -> Result<()> {
        let Some((endpoint, _)) = self.pair_mut(index) else {
            return Ok(());
        };
        let Some(wire) = endpoint.wire.as_mut() else {
            return Ok(());
        };
        wire.feed(data)
    }

    fn handle_feed_error(&mut self, index: usize, error: FlowError) -> Result<()> {
        if !recoverable_feed_error(&error) {
            return Err(error);
        }
        self.clear_wire_pair(index);
        let message = error.message.clone();
        self.marker_error(&message, true)
    }

    fn clear_wire_pair(&mut self, index: usize) {
        let Some((endpoint, peer)) = self.pair_mut(index) else {
            return;
        };
        endpoint.wire = None;
        peer.wire = None;
    }

    fn write_transfer(&mut self, index: usize) -> Result<()> {
        let Some((endpoint, peer)) = self.pair_mut(index) else {
            return Ok(());
        };
        if endpoint.buffer.is_empty() {
            return Ok(());
        }
        write_buffer(endpoint, peer)
    }

    fn transfer(&mut self, index: usize, read: bool, write: bool) -> Result<()> {
        if self.endpoints[index].is_none() || self.endpoints[1 - index].is_none() {
            return Ok(());
        }
        if read {
            self.read_transfer(index)?;
        }
        if write {
            self.write_transfer(index)?;
        }
        Ok(())
    }

    fn disconnect(&mut self, index: usize, error: Option<&str>) -> Result<()> {
        let _ = self.endpoints[0].take();
        let _ = self.endpoints[1].take();
        let _ = index;
        if self.child_running() {
            self.marker_error(error.unwrap_or("native WebSocket connection closed"), true)?;
        }
        Ok(())
    }

    fn serve(&mut self) -> Result<()> {
        let mut outcome = Ok(());
        while serve_continue(self, &outcome) {
            outcome = self.serve_iteration();
            if outcome.is_ok() {
                if let (Some(policy), Some(marker)) = (&mut self.policy, &self.marker) {
                    outcome = policy.observe(marker.borrow().value());
                }
            }
        }
        outcome
    }

    fn serve_should_continue(&mut self) -> bool {
        self.child_running() && STOP_SIGNAL.load(Ordering::SeqCst) == 0
    }

    fn serve_iteration(&mut self) -> Result<()> {
        self.flush_policy()?;
        let Some(batch) = self.poll_events()? else {
            return Ok(());
        };
        self.dispatch_poll_events(batch)
    }

    fn flush_policy(&mut self) -> Result<()> {
        if self.policy.is_some() && self.is_connected() {
            // A task's short-lived lifecycle permit can defer exact input.
            // Retry it even when neither peer sends another byte.
            self.read_data(0, &[])?;
            self.read_data(1, &[])?;
        }
        Ok(())
    }

    fn poll_events(&mut self) -> Result<Option<PollBatch>> {
        let mut batch = self.build_poll_batch();
        // SAFETY: the vector is live, writable and not resized during poll;
        // supplied count equals its length and the bridge retains the fd owners.
        let result =
            unsafe { libc::poll(batch.fds.as_mut_ptr(), batch.fds.len() as libc::nfds_t, 100) };
        if result < 0 {
            return poll_failure();
        }
        Ok(interpret_poll(batch, result))
    }

    fn build_poll_batch(&self) -> PollBatch {
        let mut batch = PollBatch::new();
        batch.add_listener(self.listener.as_ref());
        self.add_endpoint_polls(&mut batch);
        batch
    }

    fn add_endpoint_polls(&self, batch: &mut PollBatch) {
        for (index, endpoint) in self.endpoints.iter().enumerate() {
            batch.add_endpoint_poll(index, endpoint);
        }
    }

    fn dispatch_poll_events(&mut self, batch: PollBatch) -> Result<()> {
        self.maybe_accept(&batch)?;
        self.dispatch_targets(&batch, 0)
    }

    fn maybe_accept(&mut self, batch: &PollBatch) -> Result<()> {
        let Some(index) = batch.listener_index else {
            return Ok(());
        };
        self.accept_if_ready(index, batch)
    }

    fn accept_if_ready(&mut self, index: usize, batch: &PollBatch) -> Result<()> {
        if batch.fds[index].revents == 0 {
            return Ok(());
        }
        self.accept()
    }

    fn dispatch_targets(&mut self, batch: &PollBatch, position: usize) -> Result<()> {
        let Some(target) = batch.targets.get(position) else {
            return Ok(());
        };
        self.dispatch_target(batch, position, *target)?;
        self.dispatch_targets(batch, position + 1)
    }

    fn dispatch_target(&mut self, batch: &PollBatch, position: usize, target: usize) -> Result<()> {
        if target == usize::MAX {
            return Ok(());
        }
        self.dispatch_endpoint_event(target, batch.fds[position].revents)
    }

    fn dispatch_endpoint_event(&mut self, target: usize, revents: i16) -> Result<()> {
        if !self.target_ready(target, revents) {
            return Ok(());
        }
        self.transfer_events(target, revents)
    }

    fn target_ready(&self, target: usize, revents: i16) -> bool {
        revents != 0 && self.endpoints[target].is_some()
    }

    fn transfer_events(&mut self, target: usize, revents: i16) -> Result<()> {
        let read = read_event(revents);
        let write = write_event(revents);
        if !eventful(read, write) {
            return Ok(());
        }
        self.transfer_target(target, read, write)
    }

    fn transfer_target(&mut self, target: usize, read: bool, write: bool) -> Result<()> {
        if let Err(error) = self.transfer(target, read, write) {
            if self.policy.is_some() && error.kind == ErrorKind::Guard {
                return Err(error);
            }
            let message = protocol_message(&error);
            return self.disconnect(target, message.as_deref());
        }
        Ok(())
    }

    pub fn stop_child(&mut self) -> Result<()> {
        let Some(child) = self.child.as_mut() else {
            return Ok(());
        };
        stop_child_process(child)?;
        Ok(())
    }

    fn wait_child(&mut self) -> Result<i32> {
        if self.should_stop_child() {
            return self.stop_and_wait();
        }
        self.collect_child_status()
    }

    fn should_stop_child(&mut self) -> bool {
        STOP_SIGNAL.load(Ordering::SeqCst) != 0 && self.child_running()
    }

    fn stop_and_wait(&mut self) -> Result<i32> {
        self.graceful_stop()?;
        self.collect_child_status()
    }

    fn graceful_stop(&mut self) -> Result<()> {
        let signal = STOP_SIGNAL.load(Ordering::SeqCst);
        let child = self.child_or_error()?;
        signal_owned_child(child, signal)?;
        stop_until_deadline(child)
    }

    fn child_or_error(&mut self) -> Result<&mut Child> {
        self.child
            .as_mut()
            .ok_or_else(|| FlowError::io("native TUI process is unavailable"))
    }

    fn collect_child_status(&mut self) -> Result<i32> {
        let status = self.await_child()?;
        self.finish_status(status)
    }

    fn await_child(&mut self) -> Result<ExitStatus> {
        let child = self.child_or_error()?;
        wait_child_status(child)
    }

    fn finish_status(&mut self, status: ExitStatus) -> Result<i32> {
        let code = child_exit_code(status);
        self.record_exit(code)
    }

    fn record_exit(&mut self, code: i32) -> Result<i32> {
        self.marker_update(object_fields(json!({
            "status": "exited",
            "exit_code": code
        })))?;
        Ok(reported_exit_code(code))
    }

    pub fn cleanup(&mut self) -> Result<()> {
        self.endpoints[0] = None;
        self.endpoints[1] = None;
        self.listener = None;
        if let Some((device, inode)) = self.proxy_identity.take() {
            match fs::symlink_metadata(&self.proxy_path) {
                Ok(info) => {
                    if (info.dev(), info.ino()) == (device, inode) {
                        fs::remove_file(&self.proxy_path)?;
                    }
                }
                Err(error) if error.kind() == IoErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        self.lock_file = None;
        self.directory = None;
        Ok(())
    }

    fn report_run_error(&mut self, error: &FlowError) {
        if self.marker.is_some() {
            if let Err(update_error) = self.marker_error(&error.message, false) {
                eprintln!("codex-tui bridge: {update_error}");
            }
        }
        eprintln!("codex-tui bridge: {error}");
    }

    fn run_inner(&mut self, previous: &mut PreviousSignals) -> Result<i32> {
        self.prepare()?;
        self.start()?;
        *previous = install_signals()?;
        self.serve()?;
        self.wait_child()
    }

    /// Run to completion and return the child exit status (128 + signal for
    /// signal-terminated children, as the original entry point did).
    ///
    /// Owns process-wide SIGTERM/SIGHUP/SIGINT dispositions until restoration;
    /// run only one bridge and do not concurrently change these dispositions.
    /// This bridge must exclusively reap its Child: no external waiters,
    /// SIGCHLD=SIG_IGN or SA_NOCLDWAIT. The signal handler only records a request;
    /// the owner forwards it at the next loop boundary (poll is bounded at
    /// 100ms, the upstream connect wait has one 2s budget across EINTR retries),
    /// then allows 5s before kill. Filesystem work and scheduling can delay that
    /// boundary; this is not a hard overall deadline.
    pub fn run(&mut self) -> i32 {
        STOP_SIGNAL.store(0, Ordering::SeqCst);
        let mut previous: PreviousSignals = Vec::new();
        let outcome = self.run_inner(&mut previous);
        let mut code = match outcome {
            Ok(code) => code,
            Err(error) => {
                self.report_run_error(&error);
                1
            }
        };
        if let Err(error) = self.stop_child() {
            eprintln!("codex-tui bridge: {error}");
            code = 1;
        }
        if let Err(error) = self.cleanup() {
            eprintln!("codex-tui bridge: {error}");
            code = 1;
        }
        restore_signals(&previous);
        code
    }
}

fn remove_stale_proxy_socket(path: &Path) -> Result<()> {
    let info = optional_socket_metadata(path)?;
    remove_optional_socket(path, info)
}

fn optional_socket_metadata(path: &Path) -> std::io::Result<Option<fs::Metadata>> {
    fs::symlink_metadata(path)
        .map(Some)
        .or_else(ignore_not_found)
}

fn ignore_not_found(error: std::io::Error) -> std::io::Result<Option<fs::Metadata>> {
    if error.kind() == IoErrorKind::NotFound {
        Ok(None)
    } else {
        Err(error)
    }
}

fn remove_optional_socket(path: &Path, info: Option<fs::Metadata>) -> Result<()> {
    let Some(info) = info else {
        return Ok(());
    };
    remove_owned_socket(path, &info)
}

fn remove_owned_socket(path: &Path, info: &fs::Metadata) -> Result<()> {
    if owned_proxy_socket(info) {
        return fs::remove_file(path).map_err(Into::into);
    }
    Err(FlowError::usage(
        "refusing to replace unowned or non-socket proxy path",
    ))
}

fn owned_proxy_socket(info: &fs::Metadata) -> bool {
    info.file_type().is_socket() && info.uid() == current_uid()
}

fn bind_listener(bind_path: &str) -> Result<UnixListener> {
    UnixListener::bind(bind_path)
        .map_err(|error| FlowError::io(format!("cannot bind proxy socket: {error}")))
}

fn prepare_proxy_listener(path: &Path, listener: &UnixListener) -> Result<()> {
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    listen_on_socket(listener)
}

fn listen_on_socket(listener: &UnixListener) -> Result<()> {
    // Python listens with a small backlog (4).
    // SAFETY: listener owns the bound socket throughout listen; the syscall
    // takes scalar arguments and does not transfer descriptor ownership.
    let result = unsafe { libc::listen(listener.as_raw_fd(), 4) };
    if result != 0 {
        return Err(FlowError::io(format!(
            "cannot listen on proxy socket: {}",
            std::io::Error::last_os_error()
        )));
    }
    Ok(())
}

fn accept_result(
    result: std::io::Result<(UnixStream, std::os::unix::net::SocketAddr)>,
) -> Result<Option<UnixStream>> {
    result
        .map(|(stream, _)| Some(stream))
        .or_else(tolerate_accept_error)
}

fn tolerate_accept_error(error: std::io::Error) -> Result<Option<UnixStream>> {
    if accept_would_block(&error) {
        Ok(None)
    } else {
        Err(FlowError::io(format!(
            "cannot accept native connection: {error}"
        )))
    }
}

fn accept_would_block(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        IoErrorKind::WouldBlock | IoErrorKind::Interrupted
    )
}

fn build_observer(observation: &RpcObservation) -> Result<Rc<RefCell<SessionObserver>>> {
    let observer = observation.build()?;
    Ok(Rc::new(RefCell::new(observer)))
}

fn check_forward_buffer(buffer: &[u8], limit: usize) -> Result<()> {
    if buffer.len() > limit * 2 {
        return Err(protocol_error("proxy forwarding buffer exceeds limit"));
    }
    Ok(())
}

fn recoverable_feed_error(error: &FlowError) -> bool {
    matches!(
        error.kind,
        ErrorKind::RpcProtocol | ErrorKind::Usage | ErrorKind::Json
    )
}

fn write_buffer(endpoint: &mut Endpoint, peer: &mut Endpoint) -> Result<()> {
    let written = write_some(&mut endpoint.socket, &endpoint.buffer)?;
    let Some(written) = written else {
        return Ok(());
    };
    drain_written(&mut endpoint.buffer, written);
    finish_write(endpoint, peer)
}

fn write_some(socket: &mut UnixStream, buffer: &[u8]) -> Result<Option<usize>> {
    socket.write(buffer).map(Some).or_else(tolerate_write_error)
}

fn tolerate_write_error(error: std::io::Error) -> Result<Option<usize>> {
    if would_block_or_interrupted(&error) {
        Ok(None)
    } else {
        Err(FlowError::io(format!("proxy write failed: {error}")))
    }
}

fn would_block_or_interrupted(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        IoErrorKind::WouldBlock | IoErrorKind::Interrupted
    )
}

fn drain_written(buffer: &mut Vec<u8>, written: usize) {
    if written > 0 {
        buffer.drain(..written);
    }
}

fn finish_write(endpoint: &mut Endpoint, peer: &mut Endpoint) -> Result<()> {
    if should_shutdown(endpoint, peer) {
        return shutdown_endpoint(endpoint);
    }
    Ok(())
}

fn should_shutdown(endpoint: &Endpoint, peer: &Endpoint) -> bool {
    endpoint.buffer.is_empty() && peer.eof
}

fn shutdown_endpoint(endpoint: &mut Endpoint) -> Result<()> {
    endpoint
        .socket
        .shutdown(std::net::Shutdown::Write)
        .map_err(|error| FlowError::io(format!("cannot shut down proxy endpoint: {error}")))
}

/// The caller exclusively owns waiting/reaping this Child. A non-reaped exit
/// keeps its PID reserved, so the check-to-signal interval cannot target reuse.
fn signal_owned_child(child: &mut Child, signal: i32) -> Result<()> {
    if child.try_wait()?.is_some() {
        return Ok(());
    }
    // SAFETY: scalar signal and positive PID of our unreaped Child; no memory
    // arguments. Requires no external waiter, SIGCHLD=SIG_IGN or SA_NOCLDWAIT.
    let result = unsafe { libc::kill(child.id() as libc::pid_t, signal) };
    if result != 0 {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::ESRCH) {
            return Err(error.into());
        }
    }
    Ok(())
}

fn stop_child_process(child: &mut Child) -> Result<()> {
    let running = child_running_now(child)?;
    stop_running_child(child, running)
}

fn stop_running_child(child: &mut Child, running: bool) -> Result<()> {
    if running {
        return terminate_child(child);
    }
    Ok(())
}

fn child_running_now(child: &mut Child) -> Result<bool> {
    Ok(child.try_wait()?.is_none())
}

fn terminate_child(child: &mut Child) -> Result<()> {
    send_sigterm(child);
    let deadline = Instant::now() + Duration::from_secs(5);
    let exited = wait_for_exit(child, deadline)?;
    finish_termination(child, exited);
    Ok(())
}

fn send_sigterm(child: &Child) {
    let pid = child.id();
    // SAFETY: terminate_child is reached only after the exclusive Child owner
    // observed try_wait(None). An exited unreaped child reserves its PID until
    // our next wait; run() forbids external waiters and SIGCHLD auto-reaping.
    // kill receives only scalar arguments and borrows no Rust memory.
    unsafe {
        libc::kill(pid as libc::pid_t, libc::SIGTERM);
    }
}

fn wait_for_exit(child: &mut Child, deadline: Instant) -> Result<bool> {
    if deadline_reached(deadline) {
        return Ok(false);
    }
    wait_next_check(child, deadline)
}

fn wait_next_check(child: &mut Child, deadline: Instant) -> Result<bool> {
    let exited = child_has_exited(child)?;
    finish_check(child, deadline, exited)
}

fn finish_check(child: &mut Child, deadline: Instant, exited: bool) -> Result<bool> {
    if exited {
        return Ok(true);
    }
    std::thread::sleep(Duration::from_millis(20));
    wait_for_exit(child, deadline)
}

fn deadline_reached(deadline: Instant) -> bool {
    Instant::now() >= deadline
}

fn child_has_exited(child: &mut Child) -> Result<bool> {
    Ok(child.try_wait()?.is_some())
}

fn finish_termination(child: &mut Child, exited: bool) {
    if exited {
        return;
    }
    let _ = child.kill();
    let _ = child.wait();
}

fn stop_until_deadline(child: &mut Child) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(5);
    let exited = wait_for_exit(child, deadline)?;
    finish_graceful_stop(child, exited)
}

fn finish_graceful_stop(child: &mut Child, exited: bool) -> Result<()> {
    if exited {
        return Ok(());
    }
    kill_if_still_running(child)
}

fn kill_if_still_running(child: &mut Child) -> Result<()> {
    let running = child_running_now(child)?;
    kill_running_child(child, running);
    Ok(())
}

fn kill_running_child(child: &mut Child, running: bool) {
    if running {
        let _ = child.kill();
    }
}

fn wait_child_status(child: &mut Child) -> Result<ExitStatus> {
    child
        .wait()
        .map_err(|error| FlowError::io(format!("cannot wait for native TUI: {error}")))
}

fn child_exit_code(status: ExitStatus) -> i32 {
    if let Some(code) = status.code() {
        return code;
    }
    -status.signal().unwrap_or(1)
}

fn reported_exit_code(code: i32) -> i32 {
    if code >= 0 {
        code
    } else {
        128 - code
    }
}

fn create_state_directory(root: &Path) -> Result<()> {
    if let Err(error) = fs::create_dir_all(root) {
        return tolerate_existing_directory(root, error);
    }
    Ok(())
}

fn tolerate_existing_directory(root: &Path, error: std::io::Error) -> Result<()> {
    if root.exists() {
        Ok(())
    } else {
        Err(FlowError::io(format!(
            "cannot create state directory: {error}"
        )))
    }
}

fn prepare_state_directory(root: &Path) -> Result<()> {
    check_state_directory(root)?;
    set_private_permissions(root)
}

fn check_state_directory(root: &Path) -> Result<()> {
    let info = fs::symlink_metadata(root)
        .map_err(|error| FlowError::io(format!("cannot inspect state directory: {error}")))?;
    ensure_owned_real_directory(&info)
}

fn ensure_owned_real_directory(info: &fs::Metadata) -> Result<()> {
    if is_owned_real_directory(info) {
        Ok(())
    } else {
        Err(FlowError::usage(
            "state directory must be an owned real directory",
        ))
    }
}

fn is_owned_real_directory(info: &fs::Metadata) -> bool {
    !info.file_type().is_symlink() && is_owned_directory(info)
}

fn open_lock_file(directory: &File) -> Result<File> {
    let lock_name = CString::new("tui.lock").expect("static lock name");
    // SAFETY: directory stays open and lock_name stays NUL-terminated for the
    // call. O_CREAT is paired with a mode_t argument for this variadic syscall.
    let lock_fd = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            lock_name.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0o600 as libc::mode_t,
        )
    };
    if lock_fd < 0 {
        return Err(FlowError::io(format!(
            "cannot open TUI bridge lock: {}",
            std::io::Error::last_os_error()
        )));
    }
    // SAFETY: successful openat returned a new owned fd, with no existing File
    // owner. Transfer it exactly once; File closes it on every later path.
    Ok(unsafe { File::from_raw_fd(lock_fd) })
}

fn lock_exclusive(lock_file: &File) -> Result<()> {
    // SAFETY: lock_file remains borrowed/open; flock takes scalar flags and
    // does not consume the descriptor.
    let result = unsafe { libc::flock(lock_file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if result == 0 {
        return Ok(());
    }
    lock_error(std::io::Error::last_os_error())
}

fn lock_error(error: std::io::Error) -> Result<()> {
    if error.raw_os_error() == Some(libc::EWOULDBLOCK) {
        return Err(FlowError::new(
            ErrorKind::Other,
            "another TUI bridge owns this state directory",
        ));
    }
    Err(FlowError::io(format!(
        "cannot lock TUI bridge state: {error}"
    )))
}

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
struct Options {
    state_dir: Option<String>,
    socket: Option<String>,
    binary: Option<String>,
    cwd: Option<String>,
    nonce: Option<String>,
    thread: Option<String>,
}

const USAGE: &str = "Usage: herdr-flow tui --state-dir DIR --socket SOCKET --binary BINARY --cwd CWD --nonce NONCE [--thread THREAD]";

fn following_argument(args: &[String], index: usize) -> Result<String> {
    args.get(index + 1)
        .cloned()
        .ok_or_else(|| FlowError::usage("expected one argument"))
}

fn argument_value(args: &[String], index: usize, inline: Option<&str>) -> Result<String> {
    if let Some(value) = inline {
        return Ok(value.to_string());
    }
    following_argument(args, index)
}

#[derive(Clone, Copy)]
enum OptionSlot {
    StateDir,
    Socket,
    Binary,
    Cwd,
    Nonce,
    Thread,
}

impl OptionSlot {
    fn index(self) -> usize {
        self as usize
    }
}

const FLAG_SLOTS: [(&str, OptionSlot); 6] = [
    ("--state-dir", OptionSlot::StateDir),
    ("--socket", OptionSlot::Socket),
    ("--binary", OptionSlot::Binary),
    ("--cwd", OptionSlot::Cwd),
    ("--nonce", OptionSlot::Nonce),
    ("--thread", OptionSlot::Thread),
];

const REQUIRED_SLOTS: [(OptionSlot, &str); 5] = [
    (OptionSlot::StateDir, "--state-dir"),
    (OptionSlot::Socket, "--socket"),
    (OptionSlot::Binary, "--binary"),
    (OptionSlot::Cwd, "--cwd"),
    (OptionSlot::Nonce, "--nonce"),
];

fn flag_slot(name: &str) -> Option<OptionSlot> {
    FLAG_SLOTS
        .iter()
        .find(|(flag, _)| *flag == name)
        .map(|(_, slot)| *slot)
}

#[derive(Default)]
struct ArgCursor {
    skip_next: bool,
}

impl ArgCursor {
    fn parse(
        &mut self,
        args: &[String],
        index: usize,
        values: &mut [Option<String>; 6],
    ) -> Result<()> {
        if self.skip_next {
            self.skip_next = false;
            return Ok(());
        }
        self.skip_next = takes_following_value(args, index);
        parse_argument(args, index, values)
    }
}

fn takes_following_value(args: &[String], index: usize) -> bool {
    let (name, inline) = split_argument(&args[index]);
    inline.is_none() && flag_slot(name).is_some()
}

fn parse_args(args: &[String]) -> Result<Options> {
    let mut values: [Option<String>; 6] = [None, None, None, None, None, None];
    let mut cursor = ArgCursor::default();
    (0..args.len()).try_for_each(|index| cursor.parse(args, index, &mut values))?;
    require_options(values)
}

fn parse_argument(args: &[String], index: usize, values: &mut [Option<String>; 6]) -> Result<()> {
    let (name, inline) = split_argument(&args[index]);
    if let Some(slot) = flag_slot(name) {
        return set_flag_value(args, index, inline, values, slot);
    }
    reject_unknown_argument(name)
}

fn split_argument(argument: &str) -> (&str, Option<&str>) {
    match argument.split_once('=') {
        Some((name, value)) if name.starts_with("--") => (name, Some(value)),
        _ => (argument, None),
    }
}

fn set_flag_value(
    args: &[String],
    index: usize,
    inline: Option<&str>,
    values: &mut [Option<String>; 6],
    slot: OptionSlot,
) -> Result<()> {
    values[slot.index()] = Some(argument_value(args, index, inline)?);
    Ok(())
}

fn reject_unknown_argument(name: &str) -> Result<()> {
    if matches!(name, "-h" | "--help") {
        return Err(FlowError::new(ErrorKind::Other, "help requested"));
    }
    Err(FlowError::usage(format!("unrecognized arguments: {name}")))
}

fn require_options(values: [Option<String>; 6]) -> Result<Options> {
    REQUIRED_SLOTS
        .iter()
        .try_for_each(|(slot, name)| require_present(&values, *slot, name))?;
    Ok(build_options(values))
}

fn require_present(values: &[Option<String>; 6], slot: OptionSlot, name: &str) -> Result<()> {
    if values[slot.index()].is_none() {
        return Err(FlowError::usage(format!(
            "the following arguments are required: {name}"
        )));
    }
    Ok(())
}

fn build_options(values: [Option<String>; 6]) -> Options {
    let [state_dir, socket, binary, cwd, nonce, thread] = values;
    Options {
        state_dir,
        socket,
        binary,
        cwd,
        nonce,
        thread,
    }
}

fn cli_error(message: &str) -> i32 {
    util::write_stderr(&format!("{USAGE}\nherdr-flow tui: error: {message}\n"));
    2
}

/// Parse the bridge command line and run it, returning the process exit code.
pub fn run_bridge(args: &[String]) -> Result<i32> {
    let options = parse_args(args)?;
    let mut bridge = ProxyBridge::new(
        options.state_dir.as_deref().unwrap_or_default(),
        options.socket.as_deref().unwrap_or_default(),
        options.binary.as_deref().unwrap_or_default(),
        options.cwd.as_deref().unwrap_or_default(),
        options.nonce.as_deref().unwrap_or_default(),
        options.thread.as_deref(),
    );
    Ok(bridge.run())
}

pub fn cli(args: &[String]) -> i32 {
    let options = match parse_args(args) {
        Ok(options) => options,
        Err(error) => {
            if error.message == "help requested" {
                println!("{USAGE}");
                return 0;
            }
            return cli_error(&error.message);
        }
    };
    let mut bridge = ProxyBridge::new(
        options.state_dir.as_deref().unwrap_or_default(),
        options.socket.as_deref().unwrap_or_default(),
        options.binary.as_deref().unwrap_or_default(),
        options.cwd.as_deref().unwrap_or_default(),
        options.nonce.as_deref().unwrap_or_default(),
        options.thread.as_deref(),
    );
    bridge.run()
}

#[cfg(test)]
mod signal_handoff_tests {
    use super::*;

    #[test]
    fn handler_defers_delivery_to_child_owner() {
        let mut child = Command::new("sleep").arg("30").spawn().unwrap();
        forward_signal(libc::SIGTERM);
        std::thread::sleep(Duration::from_millis(50));
        let still_running = child.try_wait().unwrap().is_none();
        let pending = STOP_SIGNAL.swap(0, Ordering::SeqCst);
        let _ = child.kill();
        child.wait().unwrap();
        assert_eq!(pending, libc::SIGTERM);
        assert!(still_running, "handler signalled the numeric PID directly");
        // The owner still forwards the original signal, then treats a reaped
        // Child as complete without sending anything to its former PID.
        let root = tempfile::tempdir().unwrap();
        let mut bridge = ProxyBridge::new(
            root.path(),
            root.path().join("unused.sock"),
            "unused",
            "/",
            "signal-test",
            None,
        );
        bridge.child = Some(Command::new("sleep").arg("30").spawn().unwrap());
        forward_signal(libc::SIGHUP);
        let outcome = bridge.wait_child();
        STOP_SIGNAL.store(0, Ordering::SeqCst);
        let live = bridge.child.as_mut().unwrap();
        if outcome.is_err() {
            let _ = live.kill();
        }
        let status = live.wait().unwrap();
        assert_eq!(outcome.unwrap(), 128 + libc::SIGHUP);
        assert_eq!(status.signal(), Some(libc::SIGHUP));
        signal_owned_child(live, libc::SIGTERM).unwrap();
    }
}

#[cfg(test)]
mod connect_deadline_tests {
    use super::*;

    #[test]
    fn interruption_cannot_restart_an_exhausted_budget() {
        let mut calls = 0;
        let result = retry_interrupted_poll(Duration::from_millis(10), &mut |budget| {
            calls += 1;
            if calls == 1 {
                // Model a poll interrupted after consuming its entire budget.
                // No timing threshold assertion: sleep cannot end early.
                std::thread::sleep(budget);
                Err(std::io::Error::from_raw_os_error(libc::EINTR))
            } else {
                Ok(1)
            }
        })
        .unwrap();
        assert_eq!(
            (result, calls),
            (0, 1),
            "expired wait restarted after EINTR"
        );
    }
}

#[cfg(test)]
mod connect_poll_tests {
    use super::*;

    #[test]
    fn interruption_storm_uses_constant_stack_and_preserves_ready_result() {
        let timeout = Duration::from_secs(10);
        let mut calls = 0;
        let result = retry_interrupted_poll(timeout, &mut |remaining| {
            assert!(remaining <= timeout);
            calls += 1;
            if calls <= 10_000 {
                Err(std::io::Error::from_raw_os_error(libc::EINTR))
            } else {
                Ok(1)
            }
        })
        .unwrap();
        assert_eq!((result, calls), (1, 10_001));
    }

    #[test]
    fn zero_budget_probes_ready_fd_and_non_interrupt_errors_survive() {
        let (stream, _peer) = UnixStream::pair().unwrap();
        assert_eq!(
            poll_writable(stream.as_raw_fd(), Duration::ZERO).unwrap(),
            1
        );
        let error = retry_interrupted_poll(Duration::ZERO, &mut |_| {
            Err(std::io::Error::from_raw_os_error(libc::EINVAL))
        })
        .unwrap_err();
        assert_eq!(error.raw_os_error(), Some(libc::EINVAL));
        assert_eq!(
            retry_interrupted_poll(Duration::ZERO, &mut |_| Ok(0)).unwrap(),
            0
        );
    }
}
