//! Bootstrap only below an already initialized queue authority. No task claims
//! or user paths are created before that authority reserves the fixed child.
use super::super::{CreateSpec, RootIdentity};
use super::{fs, require};
use crate::{model::J, util, Result};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{
    ffi::OsStr,
    path::{Path, PathBuf},
};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    queue: PathBuf,
    queue_id: String,
    parent: fs::Directory,
    namespace: RootIdentity,
}

pub fn request(queue: &Path, spec: &CreateSpec) -> Result<J> {
    let parent = allocator_parent(queue)?;
    validate_targets(spec, &parent)?;
    let q = crate::queue::load_queue(queue)?;
    require_unreserved_slot(&q, &parent)?;
    encode_request(queue, &q, parent)
}

fn allocator_parent(queue: &Path) -> Result<fs::Directory> {
    let parent = fs::Directory::capture(&crate::queue::artifacts(queue)?)?;
    require(
        super::super::super::registry::control_path()? == parent.path.join("task-environments"),
        "fresh allocation requires explicitly configured queue-owned task-environments control",
    )?;
    Ok(parent)
}

fn validate_targets(spec: &CreateSpec, parent: &fs::Directory) -> Result<()> {
    for path in [&spec.workspace_root, &spec.temp_root] {
        validate_target(path, parent)?;
    }
    Ok(())
}

fn validate_target(path: &Path, parent: &fs::Directory) -> Result<()> {
    let p = path
        .parent()
        .ok_or_else(|| crate::FlowError::guard("root has no parent"))?;
    let target = fs::Directory::capture(p)?;
    require(
        target.mount == parent.mount && target.identity.dev == parent.identity.dev,
        "no managed allocator on target mount",
    )?;
    require(fs::absent(path)?, "allocation target must be absent")
}

fn require_unreserved_slot(q: &J, parent: &fs::Directory) -> Result<()> {
    if q["lifecycle"]["control_bootstrap"].is_null() {
        require(
            fs::absent(&parent.path.join("task-environments"))?,
            "unreserved control slot already exists; refusing adoption",
        )?;
    }
    Ok(())
}

fn encode_request(queue: &Path, q: &J, parent: fs::Directory) -> Result<J> {
    Ok(serde_json::to_value(Request {
        queue: queue.into(),
        queue_id: q["id"].as_str().unwrap_or_default().into(),
        parent,
        namespace: namespace()?,
    })?)
}

fn namespace() -> Result<RootIdentity> {
    Ok(RootIdentity::of(&std::fs::metadata("/proc/self/ns/mnt")?))
}

fn current(value: &J) -> Result<Request> {
    let r: Request = serde_json::from_value(value.clone())?;
    require(
        namespace()? == r.namespace,
        "allocation mount namespace changed",
    )?;
    let q = current_queue(&r)?;
    require(
        q["lifecycle"]["control_bootstrap"]["reservation"] == *value,
        "control has no durable reservation",
    )?;
    r.parent.verify()?;
    Ok(r)
}

fn current_queue(r: &Request) -> Result<J> {
    let q = crate::queue::load_queue(&r.queue)?;
    require(
        q["id"] == r.queue_id && crate::queue::artifacts(&r.queue)? == r.parent.path,
        "control queue binding changed",
    )?;
    Ok(q)
}

pub fn initialize(value: &J) -> Result<J> {
    let r = current(value)?;
    require_unready(&r)?;
    #[cfg(test)]
    super::tests::checkpoint("control-reserved");
    let parent = r.parent.verify()?;
    let (root, registry) = initialize_registry(&parent)?;
    // Keep the binding descriptor alive through the same final sync/proof stage.
    let _binding = initialize_binding(&registry, &r)?;
    initialize_namespaces(&registry)?;
    let proofs = finish_initialization(&r, &parent, &root, &registry)?;
    #[cfg(test)]
    super::tests::checkpoint("control-initialized");
    Ok(serde_json::to_value(proofs)?)
}

fn require_unready(r: &Request) -> Result<()> {
    let q = crate::queue::load_queue(&r.queue)?;
    require(
        q["lifecycle"]["control_bootstrap"]["ready"].is_null(),
        "control bootstrap already completed; do not recreate",
    )
}

fn initialize_registry(parent: &std::fs::File) -> Result<(std::fs::File, std::fs::File)> {
    let root = fs::reserved_dir(parent, OsStr::new("task-environments"))?;
    fs::entries(&root, &["registry.json"])?;
    let registry = fs::reserved_dir(&root, OsStr::new("registry.json"))?;
    fs::entries(
        &registry,
        &[
            ".flow-document-v2",
            ".flow-namespace-v2",
            "authority.json",
            "queue-binding.json",
            "envs",
            "plans",
            "locks",
        ],
    )?;
    fs::marker(&registry, true, true)?;
    fs::initial_file(&registry, "authority.json", br#"{"entries":{}}"#)?;
    Ok((root, registry))
}

fn initialize_binding(registry: &std::fs::File, r: &Request) -> Result<std::fs::File> {
    let binding = fs::reserved_dir(registry, OsStr::new("queue-binding.json"))?;
    fs::entries(&binding, &[".flow-document-v2", "authority.json"])?;
    fs::marker(&binding, true, false)?;
    fs::initial_file(
        &binding,
        "authority.json",
        &serde_json::to_vec(&json!({"queue_root":r.queue,"queue_id":r.queue_id}))?,
    )?;
    Ok(binding)
}

fn initialize_namespaces(registry: &std::fs::File) -> Result<()> {
    for name in ["envs", "plans", "locks"] {
        let dir = fs::reserved_dir(registry, OsStr::new(name))?;
        fs::entries(
            &dir,
            if name == "locks" {
                &[]
            } else {
                &[".flow-namespace-v2"]
            },
        )?;
        fs::marker(&dir, false, name != "locks")?;
    }
    Ok(())
}

fn finish_initialization(
    r: &Request,
    parent: &std::fs::File,
    root: &std::fs::File,
    registry: &std::fs::File,
) -> Result<Vec<fs::Directory>> {
    fs::sync_pair(registry, root)?;
    fs::sync_pair(root, parent)?;
    r.parent.verify()?;
    capture_proofs(r)
}

const CONTROL_PATHS: [&str; 6] = [
    "task-environments",
    "task-environments/registry.json",
    "task-environments/registry.json/queue-binding.json",
    "task-environments/registry.json/envs",
    "task-environments/registry.json/plans",
    "task-environments/registry.json/locks",
];

fn capture_proofs(r: &Request) -> Result<Vec<fs::Directory>> {
    let mut proofs = Vec::new();
    for relative in CONTROL_PATHS {
        proofs.push(fs::Directory::capture(&r.parent.path.join(relative))?);
    }
    Ok(proofs)
}

pub fn verify(request: &J, ready: &J) -> Result<()> {
    let r = current(request)?;
    let proofs: Vec<fs::Directory> = serde_json::from_value(ready.clone())?;
    verify_proofs(&r, &proofs)?;
    let root = r.parent.path.join("task-environments");
    verify_control_payload(&root)?;
    verify_namespaces(&root)?;
    verify_binding(&r, &root)
}

fn verify_proofs(r: &Request, proofs: &[fs::Directory]) -> Result<()> {
    require(
        proofs.len() == CONTROL_PATHS.len(),
        "control bootstrap proof incomplete",
    )?;
    for (proof, path) in proofs.iter().zip(CONTROL_PATHS) {
        require(
            proof.path == r.parent.path.join(path) && proof.mount == r.parent.mount,
            "control proof path or mount changed",
        )?;
        proof.verify()?;
    }
    Ok(())
}

fn verify_control_payload(root: &Path) -> Result<()> {
    require(
        super::super::super::registry::control_path()?.as_path() == root,
        "configured control changed",
    )?;
    require(
        util::protected::is_document(&root.join("registry.json"))?
            && util::protected::is_namespace(&root.join("registry.json"))?,
        "control marker missing",
    )?;
    require(
        util::read_json(&root.join("registry.json"))?["entries"].is_object(),
        "control registry payload missing or malformed",
    )
}

fn verify_namespaces(root: &Path) -> Result<()> {
    for name in ["envs", "plans"] {
        require(
            util::protected::is_namespace(&root.join("registry.json").join(name))?,
            "control namespace missing",
        )?;
    }
    Ok(())
}

fn verify_binding(r: &Request, root: &Path) -> Result<()> {
    require(
        util::read_state_json(&root.join("registry.json"), "queue-binding.json")?
            == json!({"queue_root":r.queue,"queue_id":r.queue_id}),
        "control binding changed",
    )
}

pub fn verify_current(queue: &Path) -> Result<()> {
    let q = crate::queue::load_queue(queue)?;
    let c = &q["lifecycle"]["control_bootstrap"];
    verify(&c["reservation"], &c["ready"])
}
