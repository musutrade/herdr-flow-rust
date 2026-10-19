//! Python-compatible canonical JSON, hashing and random identifiers.
//!
//! A few durable identities in the original implementation are SHA-256 hashes
//! of `json.dumps(..., sort_keys=True)` output. Hash inputs are therefore
//! serialized in Python's format (sorted keys, either compact or default
//! separators, optional ASCII escaping) instead of serde_json's compact form.

use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use crate::error::{FlowError, Result};
use crate::util::time::now_ns;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Separators {
    /// `json.dumps` default: `", "` and `": "`.
    Default,
    /// `separators=(",", ":")`.
    Compact,
}

/// Serialize a JSON value the way `json.dumps` does.
pub fn dumps(value: &Value, ensure_ascii: bool, separators: Separators) -> String {
    let mut output = String::new();
    write_value(value, ensure_ascii, separators, &mut output);
    output
}

fn write_value(value: &Value, ensure_ascii: bool, separators: Separators, out: &mut String) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(true) => out.push_str("true"),
        Value::Bool(false) => out.push_str("false"),
        Value::Number(number) => out.push_str(&format_number(number)),
        Value::String(text) => write_string(text, ensure_ascii, out),
        Value::Array(items) => write_array(items, ensure_ascii, separators, out),
        Value::Object(map) => write_object(map, ensure_ascii, separators, out),
    }
}

fn write_array(items: &[Value], ensure_ascii: bool, separators: Separators, out: &mut String) {
    out.push('[');
    for (index, item) in items.iter().enumerate() {
        write_array_item(item, index, ensure_ascii, separators, out);
    }
    out.push(']');
}

fn write_array_item(
    item: &Value,
    index: usize,
    ensure_ascii: bool,
    separators: Separators,
    out: &mut String,
) {
    if index > 0 {
        push_separator(separators, out);
    }
    write_value(item, ensure_ascii, separators, out);
}

fn write_object(
    map: &Map<String, Value>,
    ensure_ascii: bool,
    separators: Separators,
    out: &mut String,
) {
    out.push('{');
    let mut keys: Vec<&String> = map.keys().collect();
    keys.sort();
    for (index, key) in keys.into_iter().enumerate() {
        write_object_entry(map, key, index, ensure_ascii, separators, out);
    }
    out.push('}');
}

fn write_object_entry(
    map: &Map<String, Value>,
    key: &str,
    index: usize,
    ensure_ascii: bool,
    separators: Separators,
    out: &mut String,
) {
    if index > 0 {
        push_separator(separators, out);
    }
    write_string(key, ensure_ascii, out);
    push_colon(separators, out);
    write_value(&map[key], ensure_ascii, separators, out);
}

fn push_separator(separators: Separators, out: &mut String) {
    out.push_str(match separators {
        Separators::Default => ", ",
        Separators::Compact => ",",
    });
}

fn push_colon(separators: Separators, out: &mut String) {
    out.push_str(match separators {
        Separators::Default => ": ",
        Separators::Compact => ":",
    });
}

fn format_number(number: &serde_json::Number) -> String {
    if let Some(value) = number.as_i64() {
        return value.to_string();
    }
    if let Some(value) = number.as_u64() {
        return value.to_string();
    }
    let value = number.as_f64().unwrap_or(f64::NAN);
    if value.is_nan() {
        return "NaN".to_string();
    }
    if value.is_infinite() {
        return if value.is_sign_negative() {
            "-Infinity".to_string()
        } else {
            "Infinity".to_string()
        };
    }
    let mut text = format!("{value}");
    if !text.contains('.') && !text.contains('e') && !text.contains('E') {
        text.push_str(".0");
    }
    text
}

fn write_string(text: &str, ensure_ascii: bool, out: &mut String) {
    out.push('"');
    for character in text.chars() {
        write_string_character(character, ensure_ascii, out);
    }
    out.push('"');
}

fn write_string_character(character: char, ensure_ascii: bool, out: &mut String) {
    if let Some(escaped) = simple_escape(character) {
        out.push_str(escaped);
        return;
    }
    if is_control(character) {
        out.push_str(&format!("\\u{:04x}", character as u32));
        return;
    }
    if ensure_ascii && !character.is_ascii() {
        write_non_ascii(character, out);
        return;
    }
    out.push(character);
}

fn simple_escape(character: char) -> Option<&'static str> {
    match character {
        '"' => Some("\\\""),
        '\\' => Some("\\\\"),
        '\n' => Some("\\n"),
        '\r' => Some("\\r"),
        '\t' => Some("\\t"),
        '\u{08}' => Some("\\b"),
        '\u{0c}' => Some("\\f"),
        _ => None,
    }
}

fn is_control(character: char) -> bool {
    (character as u32) < 0x20
}

fn write_non_ascii(character: char, out: &mut String) {
    let code = character as u32;
    if code <= 0xffff {
        out.push_str(&format!("\\u{code:04x}"));
    } else {
        write_surrogate_pair(code, out);
    }
}

fn write_surrogate_pair(code: u32, out: &mut String) {
    let offset = code - 0x1_0000;
    let high = 0xd800 + (offset >> 10);
    let low = 0xdc00 + (offset & 0x3ff);
    out.push_str(&format!("\\u{high:04x}\\u{low:04x}"));
}

/// `json.dumps(value, sort_keys=True)` with default separators and ASCII output.
pub fn python_sorted_default(value: &Value) -> String {
    dumps(value, true, Separators::Default)
}

/// `json.dumps(value, sort_keys=True, separators=(',', ':'))`.
pub fn python_sorted_compact(value: &Value) -> String {
    dumps(value, true, Separators::Compact)
}

/// `json.dumps(value, sort_keys=True, separators=(',', ':'), ensure_ascii=False)`.
pub fn python_sorted_compact_utf8(value: &Value) -> String {
    dumps(value, false, Separators::Compact)
}

/// `json.dumps(value, sort_keys=True, ensure_ascii=True, separators=(',', ':'))`.
pub fn canonical(value: &Value) -> Vec<u8> {
    python_sorted_compact(value).into_bytes()
}

pub fn sha256_bytes(data: &[u8]) -> [u8; 32] {
    let mut digest = Sha256::new();
    digest.update(data);
    digest.finalize().into()
}

pub fn sha256_hex(data: &[u8]) -> String {
    to_hex(&sha256_bytes(data))
}

pub fn sha256_json_sorted(value: &Value) -> String {
    sha256_hex(python_sorted_default(value).as_bytes())
}

pub fn sha256_json_compact(value: &Value) -> String {
    sha256_hex(python_sorted_compact(value).as_bytes())
}

pub fn digest(data: &[u8]) -> String {
    sha256_hex(data)
}

pub fn digest_short(data: &[u8], chars: usize) -> String {
    let full = sha256_hex(data);
    full[..chars.min(full.len())].to_string()
}

pub fn to_hex(data: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(data.len() * 2);
    for byte in data {
        output.push(DIGITS[(byte >> 4) as usize] as char);
        output.push(DIGITS[(byte & 0x0f) as usize] as char);
    }
    output
}

/// Random bytes from the Linux kernel CSPRNG.
pub fn random_bytes(count: usize) -> Result<Vec<u8>> {
    use std::io::Read;
    let mut file = std::fs::File::open("/dev/urandom")
        .map_err(|error| FlowError::io(format!("cannot read /dev/urandom: {error}")))?;
    let mut buffer = vec![0u8; count];
    file.read_exact(&mut buffer)
        .map_err(|error| FlowError::io(format!("cannot read /dev/urandom: {error}")))?;
    Ok(buffer)
}

/// `uuid.uuid4().hex` (32 lowercase hex characters, no dashes).
pub fn uuid_hex() -> String {
    let mut bytes = random_bytes(16).unwrap_or_else(|_| {
        // A collision-resistant fallback if /dev/urandom is unavailable. The
        // resulting value is still unpredictable enough for local state names.
        let nanos = now_ns();
        let pid = std::process::id() as u128;
        let value = nanos.wrapping_mul(0x9e3779b97f4a7c15).wrapping_add(pid);
        value.to_le_bytes()[..16].to_vec()
    });
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    to_hex(&bytes)
}

/// `uuid.uuid4()` in canonical lowercase form.
pub fn uuid_v4() -> String {
    let hex = uuid_hex();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

/// Stable key ordering for a JSON object, cloned into a plain map.
pub fn with_sorted_keys(map: &Map<String, Value>) -> Value {
    Value::Object(map.clone())
}

/// `urllib.parse.quote(value, safe='')` for path segments.
pub fn quote_component(value: &str) -> String {
    let mut output = String::new();
    for byte in value.bytes() {
        let safe = byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b'-' | b'~');
        if safe {
            output.push(byte as char);
        } else {
            output.push_str(&format!("%{byte:02X}"));
        }
    }
    output
}

/// `urllib.parse.urlencode` for the two value pairs used by this project.
pub fn urlencode_query(pairs: &[(&str, &str)]) -> String {
    pairs
        .iter()
        .map(|(key, value)| {
            format!(
                "{}={}",
                quote_component(key),
                quote_component(value).replace("%20", "+")
            )
        })
        .collect::<Vec<_>>()
        .join("&")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn matches_python_spacing_and_ascii() {
        let value = json!({"b": 1, "a": "x"});
        assert_eq!(python_sorted_default(&value), r#"{"a": "x", "b": 1}"#);
        assert_eq!(python_sorted_compact(&value), r#"{"a":"x","b":1}"#);
        assert_eq!(
            python_sorted_compact(&json!({"a": "中"})),
            r#"{"a":"\u4e2d"}"#
        );
        assert_eq!(
            python_sorted_compact_utf8(&json!({"a": "中"})),
            r#"{"a":"中"}"#
        );
    }

    #[test]
    fn escapes_strings_and_nested_containers() {
        let value = json!({
            "text": "\"\\\n\r\t\u{08}\u{0c}\u{01}中😀",
            "array": [1, "x", true, null],
            "object": {"b": 2, "a": 1}
        });
        assert_eq!(
            python_sorted_compact(&value),
            r#"{"array":[1,"x",true,null],"object":{"a":1,"b":2},"text":"\"\\\n\r\t\b\f\u0001\u4e2d\ud83d\ude00"}"#
        );
        assert_eq!(
            python_sorted_default(&value),
            r#"{"array": [1, "x", true, null], "object": {"a": 1, "b": 2}, "text": "\"\\\n\r\t\b\f\u0001\u4e2d\ud83d\ude00"}"#
        );
        assert_eq!(
            python_sorted_compact_utf8(&value),
            "{\"array\":[1,\"x\",true,null],\"object\":{\"a\":1,\"b\":2},\"text\":\"\\\"\\\\\\n\\r\\t\\b\\f\\u0001中😀\"}"
        );
    }

    #[test]
    fn formats_floats_like_python() {
        assert_eq!(
            format_number(serde_json::json!(1.0).as_number().unwrap()),
            "1.0"
        );
        assert_eq!(
            format_number(serde_json::json!(0.5).as_number().unwrap()),
            "0.5"
        );
    }

    #[test]
    fn uuids_are_canonical() {
        let value = uuid_v4();
        assert!(crate::model::is_canonical_uuid_lower(&value));
        assert_eq!(uuid_hex().len(), 32);
    }
}
