//! Operator-selected models, persisted with the coordinator runtime.
//! Selection never imports credentials or falls back to another role's model.
use super::*;

pub(super) fn validate_model(model: &J) -> Result<()> {
    let fields = model
        .as_object()
        .ok_or_else(|| FlowError::guard("configured model required"))?;
    ensure(
        fields.len() == 2
            && fields
                .keys()
                .all(|key| ["providerID", "id"].contains(&key.as_str())),
        "model selection only accepts providerID and id",
    )?;
    required(model, "id")?;
    required(model, "providerID")?;
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
        fields.len() == 2 && fields.contains_key("developer") && fields.contains_key("reviewer"),
        "role_models requires exactly developer and reviewer",
    )?;
    for role in ["developer", "reviewer"] {
        validate_model(&models[role])?;
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

#[cfg(test)]
mod tests {
    use super::*;

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
