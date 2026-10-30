use super::*;
use std::{fs, path::PathBuf};
use tempfile::TempDir;

pub(super) mod failures;
mod records;
mod storage;

struct Fixture {
    temp: TempDir,
    store: Store,
    project: Project,
}
impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let mut store = Store::open(Options::new(temp.path().join("data"))).unwrap();
        let project = store.register_project(temp.path(), "project").unwrap();
        Self {
            temp,
            store,
            project,
        }
    }
    fn root(&self) -> PathBuf {
        self.temp.path().join("data")
    }
    fn task(&mut self, key: &str) -> Receipt {
        self.store.apply(&create(key, self.project.id)).unwrap()
    }
}
fn create(key: &str, project_id: i64) -> Request {
    request(
        key,
        Change::CreateTask {
            task: NewTask {
                project_id,
                title: "work".into(),
                source_ref: Some("https://example.test/issues/1".into()),
                batch_ref: None,
                reported_state: "open".into(),
            },
        },
    )
}
fn request(key: &str, change: Change) -> Request {
    Request {
        request_id: key.into(),
        reporter: "developer".into(),
        summary: "report".into(),
        refs: vec![],
        change,
    }
}
fn report(key: &str, id: i64, revision: i64) -> Request {
    request(
        key,
        Change::Report {
            task_id: id,
            expected_revision: revision,
            reported_state: "done".into(),
        },
    )
}
fn binding() -> BindingInput {
    BindingInput {
        role: "developer".into(),
        herdr_location: Location {
            service: "local".into(),
            workspace: "wR".into(),
            pane: "p1".into(),
        },
        session: None,
        cwd: "/project/worktree".into(),
    }
}
mod observe;
