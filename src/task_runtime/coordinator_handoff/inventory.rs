//! A bounded inventory of the existing task-owned runtime namespace.
use super::*;

pub(super) fn runtimes(task: &J, owner: &Owner) -> Result<Vec<(PathBuf, J)>> {
    let parent = path(&task["environment"]["creation_seed"], "temp_root")?.join("rt");
    let mut result = Vec::new();
    for (index, entry) in std::fs::read_dir(parent)?.enumerate() {
        ensure(index < 128, "handoff runtime inventory exceeds bound")?;
        let root = entry?.path();
        let runtime = runtime(&root, owner)?;
        result.push((root, runtime));
    }
    Ok(result)
}

fn runtime(root: &Path, owner: &Owner) -> Result<J> {
    super::anchor::protected(root, owner)?;
    let runtime = util::read_state_json_bounded(root, "runtime.json", RECORD_BYTES)?;
    ensure(
        runtime["root"] == json!(root)
            && runtime["lifecycle_owner"] == json!(owner)
            && runtime["instance"] == owner.instance_id,
        "handoff runtime inventory identity changed",
    )?;
    Ok(runtime)
}
