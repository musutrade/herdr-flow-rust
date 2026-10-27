//! Operator-selected models, persisted with the coordinator runtime.
//! Selection never imports credentials or falls back to another role's model.
use super::*;

pub(super) fn validate_model(model: &J) -> Result<()> {
    let fields = model
        .as_object()
        .ok_or_else(|| FlowError::guard("configured model required"))?;
    ensure(
        fields
            .keys()
            .all(|key| ["providerID", "id", "reasoning_effort"].contains(&key.as_str())),
        "model selection only accepts providerID, id and reasoning_effort",
    )?;
    required(model, "id")?;
    required(model, "providerID")?;
    if let Some(effort) = model.get("reasoning_effort") {
        ensure(
            matches!(
                effort.as_str(),
                Some("none" | "minimal" | "low" | "medium" | "high" | "xhigh" | "max" | "ultra")
            ),
            "invalid explicit reasoning_effort; no default substitution",
        )?;
    }
    Ok(())
}

pub(super) fn validate(spec: &J) -> Result<()> {
    let Some(models) = spec.get("role_models") else {
        // Standalone native runtimes do not dispatch other roles.
        return Ok(());
    };
    let fields = models
        .as_object()
        .ok_or_else(|| FlowError::guard("role_models must be an object"))?;
    ensure(
        fields.contains_key("developer")
            && fields.contains_key("reviewer")
            && fields
                .keys()
                .all(|k| ["developer", "reviewer", "recovery"].contains(&k.as_str())),
        "role_models requires developer/reviewer and accepts an explicit recovery model",
    )?;
    for role in ["developer", "reviewer"] {
        validate_model(&models[role])?;
    }
    if let Some(model) = models.get("recovery") {
        validate_model(model)?;
    }
    Ok(())
}

pub(super) fn select(runtime: &J, role: &J) -> Result<J> {
    let key = match role.as_str() {
        Some("development") => "developer",
        Some("acceptance") => "reviewer",
        _ => {
            return Err(FlowError::guard(
                "unsupported activity role for model selection",
            ))
        }
    };
    ensure(
        runtime.get("role_models").is_some(),
        "explicit developer/reviewer role_models required; no coordinator-model fallback",
    )?;
    validate(runtime)?;
    Ok(runtime["role_models"][key].clone())
}

pub(super) fn thread_config(model: &J) -> J {
    let mut config = json!({"project_doc_max_bytes":0,"memories":{"use_memories":false,"generate_memories":false}});
    if let Some(effort) = model.get("reasoning_effort") {
        config["model_reasoning_effort"] = effort.clone();
    }
    config
}

pub(super) fn verify_effort(model: &J, result: &J) -> Result<()> {
    if let Some(effort) = model.get("reasoning_effort") {
        ensure(
            result["reasoningEffort"] == *effort,
            "Codex selected another reasoning effort",
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_effort_is_preserved_and_verified() {
        let model = json!({"providerID":"existing","id":"selected","reasoning_effort":"high"});
        validate_model(&model).unwrap();
        assert_eq!(thread_config(&model)["model_reasoning_effort"], "high");
        verify_effort(&model, &json!({"reasoningEffort":"high"})).unwrap();
        for result in [json!({}), json!({"reasoningEffort":"medium"})] {
            assert!(verify_effort(&model, &result).is_err());
        }
        for effort in [J::Null, json!("unknown"), json!(true)] {
            let mut invalid = model.clone();
            invalid["reasoning_effort"] = effort;
            assert!(validate_model(&invalid).is_err());
        }
        let absent = json!({"providerID":"existing","id":"selected"});
        assert!(thread_config(&absent)
            .get("model_reasoning_effort")
            .is_none());
        verify_effort(&absent, &json!({})).unwrap();
    }

    #[test]
    fn role_selection_is_explicit_and_never_falls_back() {
        let config = json!({"model":{"providerID":"existing","id":"coordinator"},
            "role_models":{"developer":{"providerID":"existing","id":"dev"},
                "reviewer":{"providerID":"existing","id":"review"}}});
        validate(&config).unwrap();
        for (role, expected) in [("development", "dev"), ("acceptance", "review")] {
            assert_eq!(select(&config, &json!(role)).unwrap()["id"], expected);
        }
        assert!(select(&config, &json!("unknown")).is_err());
        let absent = json!({"model":config["model"]});
        validate(&absent).unwrap();
        assert!(select(&absent, &json!("development")).is_err());
        for value in [
            J::Null,
            json!({}),
            json!({"developer":config["model"]}),
            json!({"developer":config["model"],"reviewer":null}),
            json!({"developer":config["model"],"reviewer":config["model"],"extra":true}),
        ] {
            assert!(validate(&json!({"role_models":value})).is_err());
        }
        for model in [
            J::Null,
            json!({}),
            json!({"id":"dev","providerID":"existing","token":"forbidden"}),
            json!({"id":"","providerID":"existing"}),
            json!({"id":"dev","providerID":""}),
        ] {
            assert!(validate_model(&model).is_err());
        }
    }
}
