use super::tui_data::{Browser, View};
use serde_json::Value;

pub(super) const HELP: &str = "Commands (type then Enter): projects | tasks | project ID/all | batch TEXT/-\nopen ID | events | bindings | next/n | prev/p | refresh/r | help | quit/q\nopen selects a row on the displayed page. Events show summaries; open an event for full refs.\nBindings show participants; open a binding for its existing observation cache.\nReferences are text only. No link opening, observation, notification, or control actions.\n";

pub(super) fn render(browser: &Browser, data: &Value) -> String {
    let mut text = format!(
        "\n=== Herdr light: READ ONLY | {:?} ===\nProject: {} | Batch: {} | after {} | limit {}\n",
        browser.view,
        browser.project.map_or("all".into(), |id| id.to_string()),
        safe(browser.batch.as_deref().unwrap_or("all")),
        browser.page.after_id,
        browser.page.limit
    );
    let rows = if data.get("task").is_some() {
        text.push_str("Agent REPORT (not observation):\n");
        text.push_str(&pretty(&data["task"]));
        &data["detail"]
    } else {
        data
    };
    if let Some(rows) = rows.as_array() {
        render_rows(&mut text, browser.view, rows);
    } else {
        text.push_str(&pretty(rows));
    }
    text.push_str(if browser.next.is_some() {
        "Next page available (a final empty page is possible).\n"
    } else {
        "End of page set.\n"
    });
    text.push_str("projects | tasks | open ID | events | bindings | next | prev | project ID/all | batch TEXT/- | refresh | help | quit\n");
    text
}
fn render_rows(text: &mut String, view: View, rows: &[Value]) {
    if rows.is_empty() {
        text.push_str("No records match this page/filter.\n");
    }
    for row in rows {
        let label = match view {
            View::Projects => format!("{} | {}", field(row, "label"), field(row, "canonical_root")),
            View::Tasks => format!(
                "project {} | {} | batch {} | reported {}",
                row["project_id"],
                field(row, "title"),
                field(row, "batch_ref"),
                field(row, "reported_state")
            ),
            View::Events => format!(
                "{} | {} | {} refs | {}",
                field(row, "kind"),
                field(row, "reporter"),
                row["refs"].as_array().map_or(0, Vec::len),
                field(row, "summary")
            ),
            View::Bindings => format!(
                "{} | {} | session {}",
                field(&row["details"], "role"),
                field(&row["details"]["herdr_location"], "pane"),
                if row["details"]["session"].is_null() {
                    "missing"
                } else {
                    "recorded"
                }
            ),
            _ => String::new(),
        };
        text.push_str(&format!("{} | {}\n", row["id"], label));
    }
}
fn field(value: &Value, key: &str) -> String {
    let text = value[key].as_str().unwrap_or("-");
    let mut short: String = text.chars().take(160).collect();
    if text.chars().count() > 160 {
        short.push_str("... (open for full detail)");
    }
    safe(&short)
}
pub(super) fn safe(text: &str) -> String {
    text.chars()
        .map(|c| {
            if c.is_control() || matches!(c,'\u{202a}'..='\u{202e}'|'\u{2066}'..='\u{2069}') {
                c.escape_unicode().to_string()
            } else {
                c.to_string()
            }
        })
        .collect()
}
fn pretty(data: &Value) -> String {
    // JSON escapes ASCII controls in stored text; also escape C1/bidi controls.
    let text = serde_json::to_string_pretty(data).unwrap_or_default();
    let safe: String = text
        .chars()
        .map(|c| {
            if (c.is_control() && c != '\n')
                || matches!(c,'\u{202a}'..='\u{202e}'|'\u{2066}'..='\u{2069}')
            {
                c.escape_unicode().to_string()
            } else {
                c.to_string()
            }
        })
        .collect();
    safe + "\n"
}
