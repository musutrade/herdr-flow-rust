//! Upstream account configuration is not disposable task state.
//!
//! Do not turn a missing integration capability into a request to provision
//! credentials. Only Codex is supported, through an existing service.
use super::*;

pub fn status(adapter: &str) -> Result<J> {
    let (baseline, blockers) = match adapter {
        "codex" => (
            "0.164.0",
            vec![
                "requires an existing same-user Unix app-server socket",
                "requires an isolated task Owner and upstream thread/delete support",
            ],
        ),
        _ => {
            return Err(FlowError::guard(
                "unsupported native adapter; only Codex is supported",
            ))
        }
    };
    Ok(json!({
        "adapter":adapter,"inspected_version":baseline,
        "configuration_owner":"upstream","supported":true,
        "connection":"existing-unix-app-server",
        "real_task_acceptance":false,
        "reason":"explicit_endpoint_required",
        "blockers":blockers,
        "account_observed":false,"model_request_sent":false,
        "fixture_is_acceptance":false
    }))
}

/// Read-only capability check; never opens a user's home or contacts a server.
pub fn require_upstream(adapter: &str) -> Result<()> {
    let report = status(adapter)?;
    Err(FlowError::guard(format!(
        "upstream launch requires an explicit supported service endpoint for {} (inspected {}); do not copy credentials or change login/provider/billing; inspect task-runtime configuration-status",
        report["adapter"].as_str().unwrap_or_default(),
        report["inspected_version"].as_str().unwrap_or_default(),
    )))
}

pub(super) fn validate_launch(spec: &J) -> Result<()> {
    let adapter = required(spec, "adapter")?;
    // Validate the adapter even in fixture mode. No unknown mode falls back to
    // an isolated home, and no account configuration is accepted in this field.
    status(&adapter)?;
    match spec.get("configuration") {
        None => require_upstream(&adapter),
        Some(J::String(mode)) if mode == "upstream" => {
            required(spec, "socket")?;
            Ok(())
        },
        Some(J::String(mode)) if mode == "isolated-fixture" => Ok(()),
        _ => Err(FlowError::guard(
            "configuration must be upstream or isolated-fixture; credentials are not launch configuration",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unsupported_modes_never_fall_back_to_a_private_login() {
        {
            let adapter = "codex";
            let report = status(adapter).unwrap();
            assert_eq!(report["supported"], adapter == "codex");
            assert_eq!(report["account_observed"], false);
            assert!(validate_launch(&json!({"adapter":adapter})).is_err());
            assert!(
                validate_launch(&json!({"adapter":adapter,"configuration":"upstream"})).is_err()
            );
            assert!(validate_launch(
                &json!({"adapter":adapter,"configuration":"isolated-fixture"})
            )
            .is_ok());
            for mode in [
                J::Null,
                json!(false),
                json!({"token":"synthetic-secret"}),
                json!("typo"),
            ] {
                let error =
                    validate_launch(&json!({"adapter":adapter,"configuration":mode})).unwrap_err();
                assert!(!error.message.contains("synthetic-secret"));
            }
        }
        assert!(status("other").is_err());
        assert!(validate_launch(&json!({})).is_err());
        assert!(
            validate_launch(&json!({"adapter":"other","configuration":"isolated-fixture"}))
                .is_err()
        );
    }
}
