//! Adapter-qualified identities. Legacy producer_session means Codex only.
use crate::{
    error::FlowError,
    model::{self, J},
    Result,
};
use serde_json::json;

pub fn validate(value: &J) -> Result<J> {
    let adapter = model::string(value, "adapter").unwrap_or_default();
    let session = model::string(value, "session").unwrap_or_default();
    if !matches!(adapter.as_str(), "codex" | "opencode") {
        return Err(FlowError::guard("unsupported session adapter"));
    }
    if session.is_empty()
        || session.len() > 128
        || !session
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        return Err(FlowError::guard("invalid producer session"));
    }
    Ok(json!({"adapter":adapter,"session":session}))
}

pub fn producer(wait: &J) -> Result<Option<J>> {
    let legacy = model::string(wait, "producer_session");
    if let Some(value) = wait.get("producer").filter(|v| !v.is_null()) {
        let identity = validate(value)?;
        if let Some(session) = legacy {
            if identity != json!({"adapter":"codex","session":session}) {
                return Err(FlowError::guard("conflicting producer identities"));
            }
        }
        return Ok(Some(identity));
    }
    legacy
        .map(|session| validate(&json!({"adapter":"codex","session":session})))
        .transpose()
}

pub fn required_producer(wait: &J) -> Result<J> {
    producer(wait)?.ok_or_else(|| FlowError::guard("result wait must pin a producer identity"))
}

pub fn environment(identity: &J) -> Result<String> {
    let identity = validate(identity)?;
    let key = if identity["adapter"] == "opencode" {
        "OPENCODE_SESSION_ID"
    } else {
        "CODEX_THREAD_ID"
    };
    let actual = std::env::var(key).unwrap_or_default();
    if identity["session"] != actual {
        return Err(FlowError::guard("producer session mismatch"));
    }
    Ok(actual)
}

pub fn check_environment(wait: &J) -> Result<()> {
    producer(wait)?
        .map(|id| environment(&id))
        .transpose()
        .map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn explicit_identity_is_validated_without_codex_impersonation() {
        assert!(validate(&json!({"adapter":"other","session":"a"})).is_err());
        assert!(validate(&json!({"adapter":"opencode","session":"../a"})).is_err());
        assert!(producer(
            &json!({"producer":{"adapter":"opencode","session":"a"},"producer_session":"a"})
        )
        .is_err());
        let codex = std::env::var("CODEX_THREAD_ID").unwrap_or_default();
        if !codex.is_empty() {
            assert_eq!(
                environment(&json!({"adapter":"codex","session":codex})).unwrap(),
                codex
            );
        }
        assert!(environment(
            &json!({"adapter":"opencode","session":"definitely-not-this-session"})
        )
        .is_err());
        assert!(check_environment(&json!({})).is_ok());
        assert!(required_producer(&json!({})).is_err());
        assert_eq!(
            producer(&json!({"producer_session":"a"})).unwrap().unwrap()["adapter"],
            "codex"
        );
        assert_eq!(
            producer(&json!({"producer":{"adapter":"codex","session":"a"},"producer_session":"a"}))
                .unwrap()
                .unwrap()["session"],
            "a"
        );
    }
}
