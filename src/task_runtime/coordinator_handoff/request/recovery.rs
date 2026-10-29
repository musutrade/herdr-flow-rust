//! Preserve the existing observer, including across cancelled generations.
use super::*;

pub(super) fn reference(
    root: &Path,
    owner: &Owner,
    from: &J,
    to: &J,
    generation: usize,
    before: &J,
) -> Result<J> {
    let origin = origin(root, before)?;
    let observer = origin.join("recovery-observer");
    let runtime = util::read_state_json_bounded(root, "runtime.json", RECORD_BYTES)?;
    if runtime["role_models"]["recovery"].is_null() {
        absent_observer(&observer)?;
        return Ok(J::Null);
    }
    let context =
        util::read_state_json_bounded_if_exists(&observer, "observer.json", RECORD_BYTES)?;
    let context =
        context.ok_or_else(|| FlowError::guard("configured original recovery observer missing"))?;
    context_identity(&context, &observer, &origin, owner)?;
    let from_target = current_target(&context, from, before)?;
    Ok(
        json!({"observer":observer,"origin_coordinator":origin,"phase":"pending","from_target":from_target,
        "to_target":{"generation":generation,"runtime":to["runtime"],"scope":null,"transaction_id":null}}),
    )
}

fn absent_observer(observer: &Path) -> Result<()> {
    ensure(
        !observer.try_exists()?,
        "unconfigured recovery observer cannot be silently adopted",
    )
}

fn context_identity(context: &J, observer: &Path, origin: &Path, owner: &Owner) -> Result<()> {
    ensure(
        context["owner"] == json!(owner)
            && context["coordinator"] == json!(origin)
            && context["observer"] == json!(observer),
        "original recovery observer binding changed",
    )
}

fn origin(root: &Path, before: &J) -> Result<PathBuf> {
    let Some(first) = before["records"].as_array().and_then(|r| r.first()) else {
        return Ok(root.to_path_buf());
    };
    if first["recovery"].is_null() {
        return Ok(root.to_path_buf());
    }
    path(&first["recovery"], "origin_coordinator")
}

fn current_target(context: &J, from: &J, before: &J) -> Result<J> {
    let previous = before["records"]
        .as_array()
        .and_then(|r| r.iter().rev().find(|r| r["phase"] == "complete"));
    let expected = if let Some(previous) = previous {
        previous["recovery"]["to_target"].clone()
    } else {
        json!({"generation":0,"runtime":from["runtime"],"scope":from["scope"],"transaction_id":null})
    };
    ensure(
        expected["runtime"] == from["runtime"] && expected["scope"] == from["scope"],
        "original recovery target does not match current receiver",
    )?;
    if context["receiver_target"].is_null() {
        ensure(
            expected["generation"] == 0 && context["coordinator"] == from["runtime"],
            "recovery successor target is missing",
        )?;
    } else {
        ensure(
            context["receiver_target"] == expected,
            "original recovery target changed",
        )?;
    }
    Ok(expected)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancellation_retains_the_last_completed_recovery_target_and_exact_transaction() {
        let from = json!({"runtime":"/task/rt/successor-2","scope":{"session":"successor-2"}});
        let target = json!({"generation":2,"runtime":from["runtime"],"scope":from["scope"],
            "transaction_id":"binding-2"});
        let before = json!({"records":[
            {"phase":"cancelled","recovery":{"origin_coordinator":"/task/rt/original"}},
            {"phase":"complete","recovery":{"to_target":target}},
            {"phase":"cancelled","recovery":{"to_target":{"generation":3,"transaction_id":null}}}
        ]});
        let context = json!({"coordinator":"/task/rt/original","receiver_target":target});
        assert_eq!(
            origin(Path::new("/task/rt/successor-2"), &before).unwrap(),
            PathBuf::from("/task/rt/original")
        );
        assert_eq!(current_target(&context, &from, &before).unwrap(), target);
        let mut missing = context.clone();
        missing.as_object_mut().unwrap().remove("receiver_target");
        assert!(current_target(&missing, &from, &before).is_err());
        for (field, value) in [
            ("generation", json!(3)),
            ("transaction_id", json!("foreign")),
        ] {
            let mut changed = context.clone();
            changed["receiver_target"][field] = value;
            assert!(current_target(&changed, &from, &before).is_err());
        }
        let mut changed = from;
        changed["scope"]["session"] = json!("foreign");
        assert!(current_target(&context, &changed, &before).is_err());
    }
}
