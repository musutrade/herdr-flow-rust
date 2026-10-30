use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Project {
    pub id: i64,
    pub canonical_root: String,
    pub label: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NewTask {
    pub project_id: i64,
    pub title: String,
    pub source_ref: Option<String>,
    pub batch_ref: Option<String>,
    pub reported_state: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Task {
    pub id: i64,
    pub project_id: i64,
    pub title: String,
    pub source_ref: Option<String>,
    pub batch_ref: Option<String>,
    pub reported_state: String,
    pub summary: String,
    pub responsible_binding_id: Option<i64>,
    pub revision: i64,
    pub created_at: String,
    pub updated_at: String,
}

/// Opaque, reported session reference; no identity or ownership inference.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Session {
    pub agent: String,
    pub kind: String,
    pub source: String,
    pub value: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Location {
    pub service: String,
    pub workspace: String,
    pub pane: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BindingInput {
    pub role: String,
    pub herdr_location: Location,
    pub session: Option<Session>,
    pub cwd: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Binding {
    pub id: i64,
    pub task_id: i64,
    pub details: BindingInput,
    pub recorded_at: String,
}

/// Reference metadata only. Targets are never opened or interpreted as state.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reference {
    pub target: String,
    pub commit: Option<String>,
    pub result: Option<String>,
    pub recorded_at: Option<String>,
    pub sha256: Option<String>,
    pub temporary: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Change {
    CreateTask {
        task: NewTask,
    },
    Report {
        task_id: i64,
        expected_revision: i64,
        reported_state: String,
    },
    AttachBinding {
        task_id: i64,
        expected_revision: i64,
        binding: BindingInput,
    },
    UpdateBinding {
        task_id: i64,
        expected_revision: i64,
        binding_id: i64,
        binding: BindingInput,
    },
    SetResponsible {
        task_id: i64,
        expected_revision: i64,
        binding_id: Option<i64>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Request {
    pub request_id: String,
    pub reporter: String,
    pub summary: String,
    pub refs: Vec<Reference>,
    pub change: Change,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Receipt {
    pub event_id: i64,
    pub task_id: i64,
    pub revision: i64,
    pub binding_id: Option<i64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Event {
    pub id: i64,
    pub request_id: String,
    pub task_id: i64,
    pub binding_id: Option<i64>,
    pub kind: String,
    pub reporter: String,
    pub recorded_at: String,
    pub summary: String,
    pub refs: Vec<Reference>,
    /// Bounded original report preserves binding/owner change history.
    pub request: Request,
    pub receipt: Receipt,
}

/// Keyset pagination; limits outside 1..=100 are rejected.
#[derive(Clone, Copy, Debug)]
pub struct Page {
    pub after_id: i64,
    pub limit: u32,
}
impl Default for Page {
    fn default() -> Self {
        Self {
            after_id: 0,
            limit: 100,
        }
    }
}
