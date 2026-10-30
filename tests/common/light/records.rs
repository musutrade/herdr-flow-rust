use super::*;
use std::os::unix::fs::symlink;

#[test]
fn projects_aliases_batches_and_pagination_are_independent() {
    let mut f = Fixture::new();
    let alias = f.temp.path().join("alias");
    symlink(f.temp.path(), &alias).unwrap();
    assert_eq!(
        f.store
            .register_project(&alias.join("./data/.."), "different label")
            .unwrap(),
        f.project
    );
    let second = f.temp.path().join("second");
    fs::create_dir(&second).unwrap();
    let p2 = f.store.register_project(&second, "second").unwrap();
    let mut batch_a = create("a", f.project.id);
    if let Change::CreateTask { task } = &mut batch_a.change {
        task.batch_ref = Some("batch-a".into());
    }
    let a = f.store.apply(&batch_a).unwrap();
    let mut batch_b = create("b", f.project.id);
    if let Change::CreateTask { task } = &mut batch_b.change {
        task.batch_ref = Some("batch-b".into());
    }
    let b = f.store.apply(&batch_b).unwrap();
    let unbatched = f.task("no-batch");
    assert_eq!(f.store.task(unbatched.task_id).unwrap().batch_ref, None);
    let mut req = create("batch", f.project.id);
    if let Change::CreateTask { task } = &mut req.change {
        task.batch_ref = Some("batch".into());
    }
    let c = f.store.apply(&req).unwrap();
    let other = f.store.apply(&create("other", p2.id)).unwrap();
    assert_ne!(a.task_id, b.task_id);
    let tasks = f
        .store
        .tasks(
            f.project.id,
            None,
            Page {
                after_id: a.task_id,
                limit: 1,
            },
        )
        .unwrap();
    assert_eq!(tasks[0].id, b.task_id);
    assert_eq!(
        f.store
            .tasks(f.project.id, Some("batch"), Page::default())
            .unwrap()[0]
            .id,
        c.task_id
    );
    assert_eq!(
        f.store.tasks(p2.id, None, Page::default()).unwrap()[0].id,
        other.task_id
    );
    assert_eq!(f.store.projects(Page::default()).unwrap().len(), 2);
    assert!(matches!(
        f.store.apply(&create("unknown", 999)),
        Err(Error::NotFound("project"))
    ));
    assert!(f
        .store
        .register_project(&f.store.database_path(), "file")
        .is_err());
    assert!(f
        .store
        .projects(Page {
            after_id: 0,
            limit: 101
        })
        .is_err());
}

#[test]
fn retries_preserve_original_receipt_and_conflicts_are_distinct() {
    let mut f = Fixture::new();
    let t = f.task("create");
    let mut r = report("update", t.task_id, 1);
    r.refs.push(Reference {
        target: "https://example.test/ci/1".into(),
        commit: Some("abc".into()),
        result: Some("success".into()),
        recorded_at: Some("2026-10-30".into()),
        sha256: None,
        temporary: false,
    });
    let receipt = f.store.apply(&r).unwrap();
    assert_eq!(f.store.apply(&r).unwrap(), receipt);
    r.summary = "different".into();
    assert!(matches!(f.store.apply(&r), Err(Error::RequestConflict)));
    assert!(matches!(
        f.store.apply(&report("stale", t.task_id, 1)),
        Err(Error::RevisionConflict {
            expected: 1,
            actual: 2
        })
    ));
    assert!(matches!(
        f.store.apply(&report("missing", 999, 1)),
        Err(Error::NotFound("task"))
    ));
    let events = f.store.events(t.task_id, Page::default()).unwrap();
    assert_eq!(events.len(), 2);
    assert_eq!(events[1].refs[0].commit.as_deref(), Some("abc"));
    assert_eq!(f.store.task(t.task_id).unwrap().reported_state, "done");
    assert!(f.store.task(999).is_err());
}

#[test]
fn binding_history_and_responsibility_are_task_scoped() {
    let mut f = Fixture::new();
    let a = f.task("a");
    let b = f.task("b");
    let first = f
        .store
        .apply(&request(
            "attach",
            Change::AttachBinding {
                task_id: a.task_id,
                expected_revision: 1,
                binding: binding(),
            },
        ))
        .unwrap();
    let second = f
        .store
        .apply(&request(
            "attach-again",
            Change::AttachBinding {
                task_id: a.task_id,
                expected_revision: 2,
                binding: binding(),
            },
        ))
        .unwrap();
    assert_ne!(first.binding_id, second.binding_id);
    assert!(f
        .store
        .apply(&request(
            "cross",
            Change::SetResponsible {
                task_id: b.task_id,
                expected_revision: 1,
                binding_id: first.binding_id
            }
        ))
        .is_err());
    assert_eq!(f.store.task(b.task_id).unwrap().revision, 1);
    f.store
        .apply(&request(
            "owner",
            Change::SetResponsible {
                task_id: a.task_id,
                expected_revision: 3,
                binding_id: first.binding_id,
            },
        ))
        .unwrap();
    let mut details = binding();
    details.role = "auditor".into();
    details.session = Some(Session {
        agent: "hermes".into(),
        kind: "id".into(),
        source: "herdr:hermes".into(),
        value: "opaque".into(),
    });
    f.store
        .apply(&request(
            "edit",
            Change::UpdateBinding {
                task_id: a.task_id,
                expected_revision: 4,
                binding_id: first.binding_id.unwrap(),
                binding: details.clone(),
            },
        ))
        .unwrap();
    assert!(f.store.apply(&report("old", a.task_id, 4)).is_err());
    assert_eq!(
        f.store.bindings(a.task_id, Page::default()).unwrap()[0].details,
        details
    );
    let history = f.store.events(a.task_id, Page::default()).unwrap();
    assert_eq!(history.len(), 5);
    assert!(
        matches!(&history[1].request.change, Change::AttachBinding { binding, .. } if binding.session.is_none())
    );
    f.store
        .apply(&request(
            "clear",
            Change::SetResponsible {
                task_id: a.task_id,
                expected_revision: 5,
                binding_id: None,
            },
        ))
        .unwrap();
    assert_eq!(
        f.store.task(a.task_id).unwrap().responsible_binding_id,
        None
    );
}

#[test]
fn bounded_inputs_fail_before_writes() {
    let mut f = Fixture::new();
    let a = f.task("a");
    let mut r = report("too-long", a.task_id, 1);
    r.summary = "x".repeat(8193);
    assert!(f.store.apply(&r).is_err());
    r.summary.clear();
    r.request_id.clear();
    assert!(f.store.apply(&r).is_err());
    r.request_id = "nul".into();
    r.reporter = "\0".into();
    assert!(f.store.apply(&r).is_err());
    assert_eq!(f.store.events(a.task_id, Page::default()).unwrap().len(), 1);
}

#[test]
fn event_failure_also_rolls_back_binding_and_owner_changes() {
    let mut f = Fixture::new();
    f.task("a");
    let b = f
        .store
        .apply(&request(
            "attach",
            Change::AttachBinding {
                task_id: 1,
                expected_revision: 1,
                binding: binding(),
            },
        ))
        .unwrap();
    f.store.conn.execute_batch("CREATE TEMP TRIGGER reject_event BEFORE INSERT ON events BEGIN SELECT RAISE(ABORT,'injected'); END;").unwrap();
    assert!(f
        .store
        .apply(&request(
            "owner",
            Change::SetResponsible {
                task_id: 1,
                expected_revision: 2,
                binding_id: b.binding_id
            }
        ))
        .is_err());
    assert!(f
        .store
        .apply(&request(
            "binding",
            Change::AttachBinding {
                task_id: 1,
                expected_revision: 2,
                binding: binding()
            }
        ))
        .is_err());
    assert!(f
        .store
        .apply(&request(
            "edit",
            Change::UpdateBinding {
                task_id: 1,
                expected_revision: 2,
                binding_id: b.binding_id.unwrap(),
                binding: BindingInput {
                    role: "auditor".into(),
                    ..binding()
                }
            }
        ))
        .is_err());
    assert_eq!(f.store.bindings(1, Page::default()).unwrap().len(), 1);
    assert_eq!(
        f.store.bindings(1, Page::default()).unwrap()[0]
            .details
            .role,
        "developer"
    );
    assert_eq!(f.store.task(1).unwrap().responsible_binding_id, None);
    assert_eq!(f.store.task(1).unwrap().revision, 2);
}

#[test]
fn sql_constraints_reject_foreign_owners_and_wrong_types() {
    let mut f = Fixture::new();
    f.task("a");
    f.task("b");
    let b = f
        .store
        .apply(&request(
            "attach",
            Change::AttachBinding {
                task_id: 1,
                expected_revision: 1,
                binding: binding(),
            },
        ))
        .unwrap();
    assert!(f
        .store
        .conn
        .execute(
            "UPDATE tasks SET responsible_binding_id=?1 WHERE id=2",
            [b.binding_id]
        )
        .is_err());
    assert!(f
        .store
        .conn
        .execute("UPDATE tasks SET revision='text' WHERE id=1", [])
        .is_err());
    assert!(f
        .store
        .conn
        .execute("UPDATE tasks SET project_id=999 WHERE id=1", [])
        .is_err());
    assert!(f
        .store
        .conn
        .execute("UPDATE tasks SET summary=?1 WHERE id=1", ["x".repeat(8193)])
        .is_err());
}

#[test]
fn invalid_reference_and_binding_payloads_do_not_change_history() {
    let mut f = Fixture::new();
    f.task("a");
    let base = Reference {
        target: "https://example.test/run".into(),
        commit: None,
        result: None,
        recorded_at: None,
        sha256: None,
        temporary: true,
    };
    let mut r = report("invalid", 1, 1);
    r.refs = vec![base.clone(); 33];
    assert!(f.store.apply(&r).is_err());
    r.refs = vec![Reference {
        target: "relative/path".into(),
        ..base.clone()
    }];
    assert!(f.store.apply(&r).is_err());
    r.refs = vec![Reference {
        commit: Some("x".repeat(129)),
        ..base
    }];
    assert!(f.store.apply(&r).is_err());
    let mut b = binding();
    b.cwd = "relative".into();
    assert!(f
        .store
        .apply(&request(
            "binding",
            Change::AttachBinding {
                task_id: 1,
                expected_revision: 1,
                binding: b
            }
        ))
        .is_err());
    assert_eq!(f.store.events(1, Page::default()).unwrap().len(), 1);
}

#[test]
fn public_errors_distinguish_failure_categories() {
    let errors = [
        Error::Invalid("input".into()),
        Error::NotFound("task"),
        Error::RequestConflict,
        Error::RevisionConflict {
            expected: 1,
            actual: 2,
        },
        Error::UnsupportedSchema(2),
        Error::Io(std::io::Error::other("io")),
        Error::Sqlite(rusqlite::Error::InvalidQuery),
        Error::Json(serde_json::from_str::<Request>("{").unwrap_err()),
    ];
    for e in errors {
        assert!(!e.to_string().is_empty());
    }
}
