use super::super::{
    observe_cache::Cache,
    observe_state::Snapshot,
    tui_data::{Browser, View},
};
use super::*;
use serde_json::json;

fn browser() -> Browser {
    Browser {
        view: View::Tasks,
        project: None,
        batch: None,
        task: None,
        page: Page {
            after_id: 0,
            limit: 1,
        },
        history: vec![],
        ids: vec![],
        next: None,
    }
}
#[test]
fn tui_query_reuses_records_and_checks_cross_task_scope() {
    let mut f = Fixture::new();
    f.task("one");
    let other = f.temp.path().join("other");
    fs::create_dir(&other).unwrap();
    let project = f.store.register_project(&other, "other").unwrap();
    f.store.apply(&create("two", project.id)).unwrap();
    let options = Options::new(f.root());
    let mut b = browser();
    assert_eq!(
        b.load(&options).unwrap(),
        json!(f.store.tasks(f.project.id, None, b.page).unwrap())
    );
    b.page.after_id = 1;
    assert_eq!(b.load(&options).unwrap()[0]["project_id"], project.id);
    b.project = Some(f.project.id);
    b.task = Some(2);
    b.reset(View::Events);
    assert!(matches!(b.load(&options), Err(Error::NotFound(_))));
    b.task = Some(1);
    b.reset(View::Event(999));
    assert!(matches!(b.load(&options), Err(Error::NotFound(_))));
    f.store
        .apply(&request(
            "bind",
            Change::AttachBinding {
                task_id: 2,
                expected_revision: 1,
                binding: binding(),
            },
        ))
        .unwrap();
    b.reset(View::Binding(1));
    assert!(matches!(b.load(&options), Err(Error::NotFound(_))));
    b.task = None;
    assert!(matches!(b.load(&options), Err(Error::NotFound(_))));
}
#[test]
fn tui_cache_uses_existing_data_and_rejects_changed_binding_context() {
    let mut f = Fixture::new();
    f.task("one");
    f.store
        .apply(&request(
            "bind",
            Change::AttachBinding {
                task_id: 1,
                expected_revision: 1,
                binding: binding(),
            },
        ))
        .unwrap();
    let options = Options::new(f.root());
    let mut b = browser();
    b.task = Some(1);
    b.reset(View::Binding(1));
    let cache = Cache::open(&options, false).unwrap();
    assert_eq!(
        b.load(&options).unwrap()["detail"]["cached_observation"]["reason"],
        "no_cached_observation"
    );
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    let mut snapshot = Snapshot::unknown(f.store.binding(1).unwrap(), now, 60000, "fixture");
    snapshot.observed_state = "idle".into();
    cache.put(&snapshot).unwrap();
    let data = b.load(&options).unwrap();
    assert_eq!(data["detail"]["cached_observation"]["fresh"], true);
    assert_eq!(data["task"]["reported_state"], "open");
    snapshot.binding.details.role = "changed".into();
    cache.put(&snapshot).unwrap();
    assert_eq!(
        b.load(&options).unwrap()["detail"]["cached_observation"]["reason"],
        "binding_changed_since_observation"
    );
    assert_eq!(f.store.events(1, Page::default()).unwrap().len(), 2);
}
