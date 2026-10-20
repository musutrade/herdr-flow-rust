//! Incremental transcript parsing and cursor identity.

use super::{guard_error, HEAD_BYTES};
use crate::error::FlowError;
use crate::error::Result;
use crate::model;
use crate::model::J;
use crate::util;
use crate::util::pyjson::sha256_hex;
use serde_json::json;
use serde_json::Map;
use serde_json::Value;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::path::PathBuf;

fn scan_row(row: &J, state: &mut J) -> Result<()> {
    let payload = model::get(row, "payload")
        .cloned()
        .unwrap_or_else(|| json!({}));
    match model::string(row, "type").as_deref() {
        Some("session_meta") => {
            state["meta"] = json!({
                "id": payload.get("id").cloned().unwrap_or(Value::Null),
                "cwd": payload.get("cwd").cloned().unwrap_or(Value::Null)
            });
        }
        Some("compacted") => {
            state["compacted"] = json!(true);
            state["usage"] = Value::Null;
        }
        Some("event_msg")
            if model::string(&payload, "type").as_deref() == Some("token_count")
                && model::get(&payload, "info").is_some() =>
        {
            if let Some(usage) = token_usage(&payload["info"]) {
                state["usage"] = usage;
            }
        }
        Some("response_item")
            if model::string(&payload, "type").as_deref() == Some("message")
                && model::string(&payload, "role").as_deref() == Some("user") =>
        {
            let users = model::u64_field(state, "users").unwrap_or(0) + 1;
            state["users"] = json!(users);
        }
        _ => {}
    }
    Ok(())
}

fn token_usage(info: &J) -> Option<J> {
    let used = model::dig(info, &["last_token_usage", "total_tokens"]).and_then(Value::as_i64)?;
    let window = model::get(info, "model_context_window").and_then(Value::as_i64)?;
    if used < 0 || window <= 0 {
        return None;
    }
    Some(json!({
        "tokens": used,
        "window": window,
        "ratio": used as f64 / window as f64
    }))
}

fn digest_head(bytes: &[u8], length: usize) -> String {
    sha256_hex(&bytes[..length.min(bytes.len())])
}

/// Read the local rollout format, treating partial writes as incomplete.
pub fn transcript(path: &Path) -> Result<J> {
    transcript_state(path, None)
}

#[derive(Clone, Copy)]
struct TranscriptFacts {
    size: usize,
    dev: u64,
    ino: u64,
}

pub fn transcript_state(path: &Path, cursor: Option<&J>) -> Result<J> {
    let mut file = File::open(path)?;
    // Identity, size, prefix and tail all refer to this fd, even if the path is
    // replaced while scanning. Appends beyond this size are read next time.
    let facts = transcript_facts(&file.metadata()?);
    transcript_state_cursor(cursor, &mut file, facts)
}

fn transcript_state_cursor(
    cursor: Option<&J>,
    file: &mut File,
    facts: TranscriptFacts,
) -> Result<J> {
    let prefix = transcript_read_range(file, 0, HEAD_BYTES.min(facts.size))?;
    let has_object = cursor.and_then(Value::as_object).is_some();
    let cursor = transcript_cursor_map(cursor);
    let base_reusable = transcript_reusable_base(has_object, &cursor, facts);
    let (initial_head_len, initial_head_sha) =
        transcript_head_initial(&cursor, &prefix, facts, base_reusable);
    let reusable = transcript_head_reusable(base_reusable, &cursor, &initial_head_sha);
    let (head_len, head_sha) =
        transcript_head_final(reusable, &prefix, facts, initial_head_len, initial_head_sha);
    let state = transcript_initial_state(&cursor, reusable);
    let offset = transcript_initial_offset(&cursor, reusable);
    let data = transcript_read_range(file, offset, facts.size - offset)?;
    transcript_state_parse(data, state, offset, facts, head_len, head_sha)
}

fn transcript_read_range(file: &mut File, offset: usize, length: usize) -> Result<Vec<u8>> {
    file.seek(SeekFrom::Start(offset as u64))?;
    let mut data = Vec::new();
    file.take(length as u64).read_to_end(&mut data)?;
    if data.len() != length {
        return Err(guard_error(
            "transcript changed size while reading; rescan required",
        ));
    }
    Ok(data)
}

fn transcript_facts(metadata: &std::fs::Metadata) -> TranscriptFacts {
    use std::os::unix::fs::MetadataExt;
    TranscriptFacts {
        size: metadata.size() as usize,
        dev: metadata.dev(),
        ino: metadata.ino(),
    }
}

fn transcript_cursor_map(cursor: Option<&J>) -> Map<String, Value> {
    cursor
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default()
}

fn transcript_inode_matches(cursor: &Map<String, Value>, facts: TranscriptFacts) -> bool {
    cursor
        .get("inode")
        .and_then(Value::as_array)
        .map(|inode| {
            inode.len() == 2
                && inode[0].as_u64() == Some(facts.dev)
                && inode[1].as_u64() == Some(facts.ino)
        })
        .unwrap_or(false)
}

fn transcript_reusable_base(
    has_object: bool,
    cursor: &Map<String, Value>,
    facts: TranscriptFacts,
) -> bool {
    let inode_ok = transcript_inode_matches(cursor, facts);
    let offset_ok = cursor
        .get("offset")
        .and_then(Value::as_u64)
        .map(|offset| offset <= facts.size as u64)
        .unwrap_or(false);
    let head_ok = cursor.get("head").and_then(Value::as_str).is_some();
    let head_len_ok = cursor
        .get("head_len")
        .and_then(Value::as_u64)
        .map(|length| length <= HEAD_BYTES.min(facts.size) as u64)
        .unwrap_or(false);
    [has_object, inode_ok, offset_ok, head_ok, head_len_ok]
        .into_iter()
        .all(|ok| ok)
}

fn transcript_head_initial(
    cursor: &Map<String, Value>,
    data: &[u8],
    facts: TranscriptFacts,
    base_reusable: bool,
) -> (usize, String) {
    let head_len = if base_reusable {
        cursor.get("head_len").and_then(Value::as_u64).unwrap_or(0) as usize
    } else {
        HEAD_BYTES.min(facts.size)
    };
    let head_sha = digest_head(data, head_len);
    (head_len, head_sha)
}

fn transcript_head_reusable(
    base_reusable: bool,
    cursor: &Map<String, Value>,
    head_sha: &str,
) -> bool {
    if !base_reusable {
        return false;
    }
    cursor.get("head").and_then(Value::as_str) == Some(head_sha)
}

fn transcript_head_final(
    reusable: bool,
    data: &[u8],
    facts: TranscriptFacts,
    initial_head_len: usize,
    initial_head_sha: String,
) -> (usize, String) {
    if reusable {
        return (initial_head_len, initial_head_sha);
    }
    let head_len = HEAD_BYTES.min(facts.size);
    let head_sha = digest_head(data, head_len);
    (head_len, head_sha)
}

fn transcript_initial_state(cursor: &Map<String, Value>, reusable: bool) -> J {
    if reusable {
        json!({
            "meta": cursor.get("meta").cloned().unwrap_or(Value::Null),
            "usage": cursor.get("usage").cloned().unwrap_or(Value::Null),
            "users": cursor.get("user_messages").cloned().unwrap_or_else(|| json!(0)),
            "compacted": cursor.get("compacted").cloned().unwrap_or(json!(false))
        })
    } else {
        json!({"meta": null, "usage": null, "users": 0, "compacted": false})
    }
}

fn transcript_initial_offset(cursor: &Map<String, Value>, reusable: bool) -> usize {
    if reusable {
        cursor.get("offset").and_then(Value::as_u64).unwrap_or(0) as usize
    } else {
        0
    }
}

fn transcript_state_parse(
    data: Vec<u8>,
    mut state: J,
    offset: usize,
    facts: TranscriptFacts,
    head_len: usize,
    head_sha: String,
) -> Result<J> {
    let offset = offset + transcript_consume(&data, 0, &mut state)?;
    transcript_state_output(&state, offset, facts, head_len, head_sha)
}

fn transcript_consume(data: &[u8], start: usize, state: &mut J) -> Result<usize> {
    let mut offset = start;
    let Some(tail) = data.get(start..) else {
        return Err(guard_error(
            "transcript cursor exceeds file size; rescan required",
        ));
    };
    tail.split_inclusive(|byte| *byte == b'\n')
        .take_while(|line| line.ends_with(b"\n"))
        .try_for_each(|line| {
            transcript_scan_line(line, state)?;
            offset += line.len();
            Ok::<(), FlowError>(())
        })?;
    Ok(offset)
}

fn transcript_scan_line(line: &[u8], state: &mut J) -> Result<()> {
    let row: J = serde_json::from_slice(line)
        .map_err(|_| guard_error("malformed complete transcript record"))?;
    scan_row(&row, state)
}

fn transcript_state_output(
    state: &J,
    offset: usize,
    facts: TranscriptFacts,
    head_len: usize,
    head_sha: String,
) -> Result<J> {
    let (id, cwd) = transcript_identity(state)?;
    Ok(transcript_output(
        state, id, cwd, offset, facts, head_len, head_sha,
    ))
}

fn transcript_identity(state: &J) -> Result<(String, String)> {
    let meta = model::get(state, "meta").cloned().unwrap_or(Value::Null);
    let id = model::string(&meta, "id").unwrap_or_default();
    let cwd = model::string(&meta, "cwd").unwrap_or_default();
    if [id.as_str(), cwd.as_str()]
        .into_iter()
        .any(|value| value.is_empty())
    {
        return Err(guard_error(
            "unsupported transcript: session metadata missing",
        ));
    }
    Ok((id, cwd))
}

fn transcript_output(
    state: &J,
    id: String,
    cwd: String,
    offset: usize,
    facts: TranscriptFacts,
    head_len: usize,
    head_sha: String,
) -> J {
    let cwd = util::resolve_path(Path::new(&cwd)).display().to_string();
    let users = model::u64_field(state, "users").unwrap_or(0);
    let usage = state.get("usage").cloned().unwrap_or(Value::Null);
    let compacted = model::bool_or(state, "compacted", false);
    json!({
        "session": id.clone(),
        "cwd": cwd.clone(),
        "usage": usage.clone(),
        "user_messages": users,
        "compacted": compacted,
        "caught_up": offset == facts.size,
        "pending_bytes": facts.size.saturating_sub(offset),
        "cursor": {
            "inode": [facts.dev, facts.ino],
            "offset": offset,
            "head": head_sha,
            "head_len": head_len,
            "meta": {"id": id, "cwd": cwd},
            "usage": usage,
            "user_messages": users,
            "compacted": compacted
        }
    })
}

pub fn find_transcript(session: &str, sessions_dir: &Path) -> Result<PathBuf> {
    let matches = util::files_with_suffix(sessions_dir, &format!("{session}.jsonl"));
    if matches.len() != 1 {
        return Err(guard_error(
            "new session transcript is not uniquely available yet",
        ));
    }
    Ok(util::resolve_path(&matches[0]))
}
