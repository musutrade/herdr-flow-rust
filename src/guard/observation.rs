//! Verified transcript observations and context thresholds.

use super::{guard_error, save, transcript_state};
use crate::error::Result;
use crate::herdr::GuardAdapter;
use crate::model;
use crate::model::J;
use serde_json::json;
use serde_json::Value;
use std::path::Path;

pub(super) fn observe_context(root: &Path, s: &mut J, h: &mut dyn GuardAdapter) -> Result<()> {
    let session = model::string(s, "session").unwrap_or_default();
    h.set_state(s);
    observe_verified(root, s, h, &session)
}

fn observe_verified(root: &Path, s: &mut J, h: &mut dyn GuardAdapter, session: &str) -> Result<()> {
    h.verify_tui(session)?;
    observe_transcript(root, s)
}

fn observe_transcript(root: &Path, s: &mut J) -> Result<()> {
    let transcript_path =
        model::string(s, "transcript").ok_or_else(|| guard_error("transcript is unavailable"))?;
    observe_read(root, s, Path::new(&transcript_path))
}

fn observe_read(root: &Path, s: &mut J, transcript_path: &Path) -> Result<()> {
    let cursor = model::get(s, "scan").cloned();
    let data = transcript_state(transcript_path, cursor.as_ref())?;
    observe_data(root, s, data)
}

fn observe_data(root: &Path, s: &mut J, data: J) -> Result<()> {
    s["scan"] = data.get("cursor").cloned().unwrap_or(Value::Null);
    if observe_identity_mismatch(s, &data) {
        return Err(guard_error("transcript identity changed"));
    }
    observe_usage(root, s, &data)
}

fn observe_identity_mismatch(s: &J, data: &J) -> bool {
    [
        model::string(data, "session") != model::string(s, "session"),
        model::string(data, "cwd") != model::string(s, "cwd"),
    ]
    .into_iter()
    .any(|mismatch| mismatch)
}

fn observe_usage(root: &Path, s: &mut J, data: &J) -> Result<()> {
    let usage = data.get("usage").cloned().unwrap_or(Value::Null);
    s["usage"] = usage.clone();
    let level = observe_level(s, &usage);
    observe_apply(root, s, level)
}

fn observe_level(s: &J, usage: &J) -> &'static str {
    if usage.is_null() {
        return "unknown";
    }
    observe_level_known(s, usage)
}

fn observe_level_known(s: &J, usage: &J) -> &'static str {
    let tokens = model::number(usage, "tokens").unwrap_or(0.0);
    let ratio = model::number(usage, "ratio").unwrap_or(0.0);
    let rotate = observe_threshold(s, tokens, ratio, "rotate_tokens", "rotate_ratio");
    let warn = observe_threshold(s, tokens, ratio, "warn_tokens", "warn_ratio");
    observe_level_select(rotate, warn)
}

fn observe_threshold(s: &J, tokens: f64, ratio: f64, tokens_key: &str, ratio_key: &str) -> bool {
    let token_limit = model::number(s, tokens_key).unwrap_or(f64::INFINITY);
    let ratio_limit = model::number(s, ratio_key).unwrap_or(f64::INFINITY);
    [tokens >= token_limit, ratio >= ratio_limit]
        .into_iter()
        .any(|hit| hit)
}

fn observe_level_select(rotate: bool, warn: bool) -> &'static str {
    let index = [rotate, warn].into_iter().position(|hit| hit).unwrap_or(2);
    ["rotate", "warn", "normal"][index]
}

fn observe_apply(root: &Path, s: &mut J, level: &str) -> Result<()> {
    s["level"] = json!(level);
    if level == "rotate" {
        s["rotation_due"] = json!(true);
    }
    save(root, s)
}
