//! Cross-task publication/writer checks on the caller's locked queue snapshot.
//! No ownership is released here, and no external operation is retried.

use std::path::{Path, PathBuf};

use super::{ensure, object, paths_overlap, progress_error, text};
use crate::model::{self, J};
use crate::Result;

fn present<'a>(record: &'a J, key: &str) -> Option<&'a J> {
    record.get(key).filter(|value| !value.is_null())
}

fn canonical_directory(path: &Path) -> Result<PathBuf> {
    ensure(path.is_absolute(), "ownership path must be absolute")?;
    let resolved = path
        .canonicalize()
        .map_err(|_| progress_error("ownership path cannot be verified"))?;
    ensure(resolved.is_dir(), "ownership path must be a directory")?;
    Ok(resolved)
}

fn owned_directory(record: &J, key: &str) -> Result<PathBuf> {
    object(record, "ownership record")?;
    let path = text(record.get(key), "ownership path", 1000)?;
    canonical_directory(Path::new(&path))
}

fn foreign_tasks(s: &J, owner: &str, mut check: impl FnMut(&J) -> Result<()>) -> Result<()> {
    let Some(tasks) = s.get("tasks") else {
        return Ok(());
    };
    for (name, task) in object(tasks, "queue tasks")? {
        // Current-task lifecycle/identity checks belong to the caller. Use the
        // queue key for this exemption, never a foreign record's claimed name.
        if name == owner {
            continue;
        }
        ensure(
            model::string(task, "task").as_deref() == Some(name),
            "task ownership identity does not match queue key",
        )?;
        check(task)?;
    }
    Ok(())
}

fn foreign_writer(task: &J, checkout: &Path) -> Result<()> {
    let Some(writer) = present(task, "writer") else {
        return Ok(());
    };
    let owned = owned_directory(writer, "workspace")?;
    ensure(
        !paths_overlap(checkout, &owned),
        "publication checkout has another task's writer; inspect and release it first",
    )
}

fn intent(record: &J) -> Result<[String; 4]> {
    object(record, "PR ownership record")?;
    Ok([
        text(record.get("host"), "PR host", 1000)?.to_ascii_lowercase(),
        text(record.get("repo"), "PR repo", 1000)?.to_ascii_lowercase(),
        text(record.get("base_ref"), "PR base_ref", 1000)?,
        text(record.get("head_ref"), "PR head_ref", 1000)?,
    ])
}

fn foreign_intent(task: &J, requested: &[String; 4]) -> Result<()> {
    let Some(pr) = present(task, "pr") else {
        return Ok(());
    };
    ensure(
        intent(pr)? != *requested,
        "publication intent is owned by another task; retain and reconcile its record",
    )
}

pub(crate) fn publication(s: &J, task: &J, record: &J) -> Result<()> {
    let owner = text(task.get("task"), "publication task", 1000)?;
    let checkout = owned_directory(record, "checkout")?;
    let requested = intent(record)?;
    foreign_tasks(s, &owner, |other| {
        foreign_writer(other, &checkout)?;
        foreign_intent(other, &requested)
    })
}

fn sending_checkout(task: &J, workspace: &Path) -> Result<()> {
    let Some(pr) = present(task, "pr") else {
        return Ok(());
    };
    object(pr, "PR ownership record")?;
    let publication = pr
        .get("publication")
        .ok_or_else(|| progress_error("PR publication ownership is unavailable"))?;
    let phase = text(publication.get("phase"), "publication phase", 1000)?;
    ensure(
        matches!(phase.as_str(), "prepared" | "sending" | "observed"),
        "PR publication ownership phase is unknown",
    )?;
    if phase == "sending" {
        let checkout = owned_directory(pr, "checkout")?;
        let workspace = canonical_directory(workspace)?;
        ensure(
            !paths_overlap(&workspace, &checkout),
            "workspace has another task's sending publication; reconcile, never replay",
        )?;
    }
    Ok(())
}

pub(crate) fn writer_reservation(s: &J, owner: &str, workspace: &Path) -> Result<()> {
    foreign_tasks(s, owner, |other| sending_checkout(other, workspace))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn same_task_update_keeps_its_intent_and_writer_ownership() {
        let checkout = tempfile::tempdir().unwrap();
        let record = json!({"checkout":checkout.path(), "host":"github.com", "repo":"o/r",
            "base_ref":"main", "head_ref":"feature", "number":7,
            "publication":{"kind":"update", "phase":"prepared"}});
        let task = json!({"task":"owner", "pr":record,
            "writer":{"workspace":checkout.path()}});
        let s = json!({"tasks":{"owner":task}});
        // The original own-task idle/repair checks remain in their callers.
        publication(&s, &task, &record).unwrap();
        writer_reservation(&s, "owner", checkout.path()).unwrap();
    }
}
