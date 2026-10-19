//! JSON document helpers.
//!
//! Durable documents stay dynamically shaped, as they were in the Python
//! implementation (tasks carry optional lane/PR/batch records). These helpers
//! provide checked access without panicking on malformed state.

use serde_json::{Map, Value};

use crate::error::{ErrorKind, FlowError, Result};

pub type J = Value;
pub type JMap = Map<String, Value>;

pub fn object<'a>(value: &'a J, name: &str) -> Result<&'a JMap> {
    value
        .as_object()
        .ok_or_else(|| FlowError::progress(format!("{name} must be an object")))
}

pub fn object_mut<'a>(value: &'a mut J, name: &str) -> Result<&'a mut JMap> {
    value
        .as_object_mut()
        .ok_or_else(|| FlowError::progress(format!("{name} must be an object")))
}

pub fn as_object(value: &J) -> Option<&JMap> {
    value.as_object()
}

pub fn as_object_mut(value: &mut J) -> Option<&mut JMap> {
    value.as_object_mut()
}

pub fn as_array(value: &J) -> Option<&Vec<J>> {
    value.as_array()
}

pub fn get<'a>(value: &'a J, key: &str) -> Option<&'a J> {
    value.as_object().and_then(|map| map.get(key))
}

pub fn get_mut<'a>(value: &'a mut J, key: &str) -> Option<&'a mut J> {
    value.as_object_mut().and_then(|map| map.get_mut(key))
}

/// Nested lookup: `dig(value, &["head", "sha"])`.
pub fn dig<'a>(value: &'a J, path: &[&str]) -> Option<&'a J> {
    let mut cursor = value;
    for key in path {
        cursor = get(cursor, key)?;
    }
    Some(cursor)
}

pub fn dig_mut<'a>(value: &'a mut J, path: &[&str]) -> Option<&'a mut J> {
    dig_mut_from(value, path)
}

fn dig_mut_from<'a>(value: &'a mut J, path: &[&str]) -> Option<&'a mut J> {
    if path.is_empty() {
        return Some(value);
    }
    let key = path[0];
    get_mut(value, key).and_then(|next| dig_mut_from(next, &path[1..]))
}

pub fn dig_str(value: &J, path: &[&str]) -> Option<String> {
    dig(value, path)
        .and_then(Value::as_str)
        .map(ToString::to_string)
}

pub fn dig_int(value: &J, path: &[&str]) -> Option<i64> {
    dig(value, path).and_then(Value::as_i64)
}

pub fn dig_f64(value: &J, path: &[&str]) -> Option<f64> {
    dig(value, path).and_then(Value::as_f64)
}

pub fn set(value: &mut J, key: &str, field: J) {
    if let Some(map) = value.as_object_mut() {
        map.insert(key.to_string(), field);
    }
}

pub fn remove(value: &mut J, key: &str) -> Option<J> {
    value.as_object_mut().and_then(|map| map.remove(key))
}

pub fn string(value: &J, key: &str) -> Option<String> {
    get(value, key)
        .and_then(Value::as_str)
        .map(ToString::to_string)
}

pub fn string_or(value: &J, key: &str, default: &str) -> String {
    string(value, key).unwrap_or_else(|| default.to_string())
}

pub fn bool_field(value: &J, key: &str) -> Option<bool> {
    get(value, key).and_then(Value::as_bool)
}

pub fn bool_or(value: &J, key: &str, default: bool) -> bool {
    bool_field(value, key).unwrap_or(default)
}

pub fn int(value: &J, key: &str) -> Option<i64> {
    get(value, key).and_then(Value::as_i64)
}

pub fn u64_field(value: &J, key: &str) -> Option<u64> {
    get(value, key).and_then(Value::as_u64)
}

pub fn number(value: &J, key: &str) -> Option<f64> {
    get(value, key).and_then(Value::as_f64)
}

pub fn array_field<'a>(value: &'a J, key: &str) -> Option<&'a Vec<J>> {
    get(value, key).and_then(Value::as_array)
}

pub fn object_field<'a>(value: &'a J, key: &str) -> Option<&'a JMap> {
    get(value, key).and_then(Value::as_object)
}

/// Python `progress.text`: nonempty text up to `limit` characters.
pub fn text(value: Option<&J>, name: &str, limit: usize) -> Result<String> {
    match value.and_then(Value::as_str) {
        Some(text) if !text.trim().is_empty() && text.chars().count() <= limit => {
            Ok(text.to_string())
        }
        _ => Err(FlowError::progress(format!(
            "{name} must be nonempty text (at most {limit} characters)"
        ))),
    }
}

pub fn text_default(value: Option<&J>, name: &str) -> Result<String> {
    text(value, name, 1000)
}

/// Python `progress.duration`: finite seconds in 1..=604800.
pub fn duration(value: Option<&J>, default: Option<f64>) -> Result<f64> {
    match value {
        Some(Value::Number(number)) => {
            let value = number.as_f64().unwrap_or(f64::NAN);
            if value.is_finite() && (1.0..=604_800.0).contains(&value) {
                Ok(value)
            } else {
                Err(FlowError::progress(
                    "progress timeout must be 1 second to 7 days",
                ))
            }
        }
        None => default
            .ok_or_else(|| FlowError::progress("progress timeout must be 1 second to 7 days")),
        _ => Err(FlowError::progress(
            "progress timeout must be 1 second to 7 days",
        )),
    }
}

pub fn is_full_sha(value: &str) -> bool {
    is_full_sha_lower(value)
}

pub fn is_full_sha_lower(value: &str) -> bool {
    value.len() == 40
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

pub fn is_sha256_hex(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

pub fn is_canonical_uuid(value: &str) -> bool {
    static UUID: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = UUID.get_or_init(|| {
        regex::Regex::new(
            r"^[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}$",
        )
        .expect("static UUID regex")
    });
    re.is_match(value)
}

pub fn is_canonical_uuid_lower(value: &str) -> bool {
    is_canonical_uuid(value) && value == value.to_lowercase()
}

pub fn error_from(kind: ErrorKind, message: impl Into<String>) -> FlowError {
    FlowError::new(kind, message)
}

/// Python truthiness for JSON values (used by compatibility checks).
pub fn truthy(value: Option<&J>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::Bool(value)) => *value,
        Some(Value::Number(number)) => number.as_f64().unwrap_or(0.0) != 0.0,
        Some(Value::String(text)) => !text.is_empty(),
        Some(Value::Array(items)) => !items.is_empty(),
        Some(Value::Object(map)) => !map.is_empty(),
    }
}

/// Recursively sort object keys; serde_json's default map is already a BTreeMap.
pub fn sorted_value(value: &J) -> J {
    if let Value::Array(items) = value {
        return Value::Array(items.iter().map(sorted_value).collect());
    }
    sorted_object_value(value)
}

fn sorted_object_value(value: &J) -> J {
    if let Value::Object(map) = value {
        return sorted_object(map);
    }
    value.clone()
}

fn sorted_object(map: &JMap) -> J {
    let mut result = JMap::new();
    for (key, item) in map {
        result.insert(key.clone(), sorted_value(item));
    }
    Value::Object(result)
}

/// Deep-copy followed by a closure, mirroring the transactional `deepcopy`
/// pattern of the original state machines.
pub fn map_copy<F>(value: &J, change: F) -> J
where
    F: FnOnce(&mut J),
{
    let mut copy = value.clone();
    change(&mut copy);
    copy
}
