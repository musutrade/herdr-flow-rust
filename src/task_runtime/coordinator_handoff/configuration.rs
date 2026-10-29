//! Frozen business permissions, independent of temporary handshake overrides.
use super::*;

fn configuration_identity(config: &J) -> Result<()> {
    fields(
        config,
        &[
            "provider",
            "model",
            "effort",
            "approval",
            "sandbox",
            "cwd",
            "upstream_endpoint",
            "role_models",
            "driver",
            "business_session_configuration",
        ],
    )?;
    super::super::role_models::validate_model(&config["model"])?;
    path(config, "cwd")?;
    path(config, "driver")?;
    ensure(
        config["provider"] == config["model"]["providerID"],
        "coordinator provider changed",
    )?;
    ensure(
        config["effort"] == config["model"]["reasoning_effort"],
        "coordinator effort changed",
    )?;
    Ok(())
}

pub(super) fn validate(record: &J, owner: &Owner) -> Result<()> {
    let config = &record["configuration"];
    configuration_identity(config)?;
    permissions(config)?;
    endpoint(&config["upstream_endpoint"])?;
    super::super::role_models::validate(&json!({"role_models":config["role_models"]}))?;
    business_configuration(config)?;
    target_configuration(record, owner)
}

fn target_configuration(record: &J, owner: &Owner) -> Result<()> {
    let config = &record["configuration"];
    for name in ["from", "to"] {
        let scope = &record[name]["scope"]["codex_runtime"];
        if !scope.is_null() {
            ensure(
                scope["cwd"] == config["cwd"]
                    && scope["model"] == config["model"]
                    && scope["instance"] == owner.instance_id,
                "coordinator target changed the frozen configuration",
            )?;
        }
    }
    Ok(())
}

fn permissions(config: &J) -> Result<()> {
    ensure(
        matches!(
            config["approval"].as_str(),
            Some("untrusted" | "on-request")
        ),
        "invalid original coordinator approval policy",
    )?;
    sandbox(&config["sandbox"], &config["cwd"])
}

fn sandbox(policy: &J, cwd: &J) -> Result<()> {
    let object = policy
        .as_object()
        .ok_or_else(|| FlowError::guard("coordinator sandbox must be an object"))?;
    ensure(
        object.keys().all(|k| {
            [
                "type",
                "writableRoots",
                "networkAccess",
                "excludeTmpdirEnvVar",
                "excludeSlashTmp",
            ]
            .contains(&k.as_str())
        }),
        "unknown coordinator sandbox field",
    )?;
    ensure(
        policy["type"] == "workspaceWrite" && policy["networkAccess"] == false,
        "coordinator business sandbox must be the original restricted workspace policy",
    )?;
    roots(policy, cwd)?;
    for key in ["excludeTmpdirEnvVar", "excludeSlashTmp"] {
        if let Some(value) = policy.get(key) {
            ensure(value.is_boolean(), "invalid coordinator sandbox exclusion")?;
        }
    }
    Ok(())
}

fn roots(policy: &J, cwd: &J) -> Result<()> {
    let roots = policy["writableRoots"]
        .as_array()
        .ok_or_else(|| FlowError::guard("coordinator writable roots missing"))?;
    // Upstream may omit cwd because it is implicitly writable. No other
    // checkout or unbounded repeated roots are supported by this first slice.
    ensure(
        roots.len() <= 1 && roots.iter().all(|root| root == cwd),
        "coordinator sandbox escapes the original checkout",
    )
}

fn endpoint(value: &J) -> Result<()> {
    fields(value, &["path", "dev", "ino", "uid"])?;
    path(value, "path")?;
    ensure(
        value["dev"].as_u64().is_some() && value["ino"].as_u64().is_some_and(|n| n > 0),
        "coordinator endpoint inode identity is invalid",
    )?;
    ensure(
        value["uid"]
            .as_u64()
            .is_some_and(|n| u32::try_from(n).is_ok()),
        "coordinator endpoint user identity is invalid",
    )
}

fn business_configuration(config: &J) -> Result<()> {
    let original = &config["business_session_configuration"];
    ensure(
        original.is_object()
            && original["approvalPolicy"] == config["approval"]
            && original["sandbox"] == config["sandbox"]
            && original["cwd"] == config["cwd"]
            && original["model"] == config["model"]["id"]
            && original["modelProvider"] == config["provider"],
        "coordinator original business configuration changed",
    )?;
    if !config["effort"].is_null() {
        ensure(
            original["reasoningEffort"] == config["effort"],
            "coordinator original effort changed",
        )?;
    }
    Ok(())
}
