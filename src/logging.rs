//! Structured logging with the same fields as the original implementation.

use serde::Serialize;
use serde_json::Value;

use crate::util::time::now;

#[derive(Serialize)]
struct Entry<'a> {
    timestamp: f64,
    event_type: &'a str,
    component: &'a str,
    message: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    task_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    guard_epoch: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    assignment_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    delivery_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    metadata: Option<&'a Value>,
}

#[derive(Default)]
pub struct Fields<'a> {
    pub task_id: Option<&'a str>,
    pub guard_epoch: Option<u64>,
    pub assignment_id: Option<&'a str>,
    pub delivery_id: Option<&'a str>,
    pub metadata: Option<&'a Value>,
}

pub fn log_event(event_type: &str, component: &str, message: &str) {
    log_event_fields(event_type, component, message, Fields::default());
}

pub fn log_event_fields(event_type: &str, component: &str, message: &str, fields: Fields<'_>) {
    let entry = Entry {
        timestamp: now(),
        event_type,
        component,
        message,
        task_id: fields.task_id,
        guard_epoch: fields.guard_epoch,
        assignment_id: fields.assignment_id,
        delivery_id: fields.delivery_id,
        metadata: fields.metadata,
    };
    if let Ok(line) = serde_json::to_string(&entry) {
        use std::io::Write;
        let mut stderr = std::io::stderr().lock();
        let _ = writeln!(stderr, "{line}");
        let _ = stderr.flush();
    }
}

#[macro_export]
macro_rules! log_event {
    ($event:expr, $component:expr, $message:expr) => {
        $crate::logging::log_event($event, $component, $message)
    };
    ($event:expr, $component:expr, $message:expr, $($key:ident = $value:expr),+ $(,)?) => {{
        let metadata = ::serde_json::json!({ $(stringify!($key): $value),+ });
        $crate::logging::log_event_fields(
            $event,
            $component,
            $message,
            $crate::logging::Fields {
                metadata: Some(&metadata),
                ..Default::default()
            },
        )
    }};
}
