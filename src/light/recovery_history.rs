//! Read a single explicitly authorized thread, never scan conversations/logs.
use super::{
    recovery::{Authorization, Turn},
    Error, Result,
};
use rusqlite::{Connection, OpenFlags, OptionalExtension};
use serde_json::Value;
use std::time::Duration;

pub fn read_latest(authorization: &Authorization) -> Result<Option<Turn>> {
    let conn = connect(authorization)?;
    let Some(mut turn) = latest(&conn, authorization)? else {
        return Ok(None);
    };
    turn.items_terminal = terminal_items(&conn, &turn)?;
    Ok(Some(turn))
}
fn connect(authorization: &Authorization) -> Result<Connection> {
    let conn = Connection::open_with_flags(
        &authorization.history,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )?;
    conn.busy_timeout(Duration::from_millis(250))?;
    conn.execute_batch(
        "PRAGMA query_only=ON; PRAGMA trusted_schema=OFF; PRAGMA temp_store=MEMORY; BEGIN;",
    )?;
    Ok(conn)
}
fn latest(conn: &Connection, authorization: &Authorization) -> Result<Option<Turn>> {
    let row = conn.query_row("SELECT turn_id,rollout_ordinal,status,error_json,completed_at FROM thread_turns WHERE thread_id=?1 ORDER BY rollout_ordinal DESC LIMIT 1",[&authorization.thread_id],|r| {
        Ok((r.get::<_,String>(0)?,r.get::<_,i64>(1)?,r.get::<_,String>(2)?,r.get::<_,Option<String>>(3)?,r.get::<_,Option<i64>>(4)?))
    }).optional()?;
    let Some((turn_id, ordinal, status, error, completed_at)) = row else {
        return Ok(None);
    };
    let error_code = error
        .map(|s| serde_json::from_str::<Value>(&s))
        .transpose()?
        .and_then(|v| classify_error(v.get("codexErrorInfo")?));
    Ok(Some(Turn {
        thread_id: authorization.thread_id.clone(),
        turn_id,
        ordinal,
        status,
        error_code,
        completed_at,
        items_terminal: false,
    }))
}
fn terminal_items(conn: &Connection, turn: &Turn) -> Result<bool> {
    let mut stmt=conn.prepare("SELECT item_json FROM thread_items WHERE thread_id=?1 AND turn_id=?2 ORDER BY rollout_ordinal LIMIT 1001")?;
    let rows = stmt.query_map(rusqlite::params![turn.thread_id, turn.turn_id], |r| {
        r.get::<_, String>(0)
    })?;
    let mut terminal = true;
    let mut count = 0;
    for row in rows {
        count += 1;
        let s = row?;
        terminal &= parse_item(&s)?;
    }
    Ok(terminal && count <= 1000)
}
fn parse_item(s: &str) -> Result<bool> {
    if s.len() > 1024 * 1024 {
        return Err(Error::Invalid("history item exceeds bound".into()));
    }
    Ok(item_terminal(&serde_json::from_str::<Value>(s)?))
}

fn classify_error(v: &Value) -> Option<String> {
    if v.as_str() == Some("serverOverloaded") {
        return Some("serverOverloaded".into());
    }
    // Structured transport terminal failures only. In particular, do not turn
    // a 401/403/429 wrapper into a retryable network error.
    let object = v.as_object()?;
    if object.len() != 1 {
        return None;
    }
    let (kind, details) = object.iter().next()?;
    if ![
        "responseStreamDisconnected",
        "responseStreamConnectionFailed",
        "responseTooManyFailedAttempts",
    ]
    .contains(&kind.as_str())
    {
        return None;
    }
    let code = details.get("httpStatusCode")?;
    if code.is_null() || code.as_u64().is_some_and(|c| (500..=599).contains(&c)) {
        Some("responseStreamDisconnected".into())
    } else {
        None
    }
}
fn item_terminal(v: &Value) -> bool {
    match v.get("type").and_then(Value::as_str) {
        Some("userMessage" | "agentMessage" | "reasoning" | "plan") => true,
        Some(
            "commandExecution"
            | "fileChange"
            | "mcpToolCall"
            | "dynamicToolCall"
            | "webSearch"
            | "collabAgentToolCall",
        ) => {
            matches!(
                v.get("status").and_then(Value::as_str),
                Some("completed" | "failed")
            )
        }
        _ => false,
    }
}
