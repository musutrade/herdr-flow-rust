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
    let context =
        util::read_state_json_bounded_if_exists(&observer, "observer.json", RECORD_BYTES)?;
    let runtime = util::read_state_json_bounded(root, "runtime.json", RECORD_BYTES)?;
    if runtime["role_models"]["recovery"].is_null() {
        ensure(
            context.is_none(),
            "unconfigured recovery observer cannot be silently adopted",
        )?;
        return Ok(J::Null);
    }
    let context =
        context.ok_or_else(|| FlowError::guard("configured original recovery observer missing"))?;
    ensure(
        context["owner"] == json!(owner)
            && context["coordinator"] == json!(origin)
            && context["observer"] == json!(observer),
        "original recovery observer binding changed",
    )?;
    let from_target = current_target(&context, from, before)?;
    Ok(
        json!({"observer":observer,"origin_coordinator":origin,"phase":"pending","from_target":from_target,
        "to_target":{"generation":generation,"runtime":to["runtime"],"scope":null,"transaction_id":null}}),
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
