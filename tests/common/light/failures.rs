use super::*;
use std::{
    cell::Cell,
    process::{Child, Command},
    time::{Duration, Instant},
};

thread_local! { static PAUSE: Cell<bool> = const { Cell::new(false) }; }

pub(in crate::light) fn after_mutation() {
    if PAUSE.get() {
        let path = std::env::var_os("LIGHT_TEST_ROOT").unwrap();
        fs::write(PathBuf::from(path).join("ready-kill"), b"ready").unwrap();
        loop {
            std::thread::park();
        }
    }
}

struct Worker(Child);
impl Drop for Worker {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
fn worker(root: &std::path::Path, mode: &str) -> Worker {
    Worker(
        Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "light::tests::failures::process_worker",
                "--nocapture",
            ])
            .env("LIGHT_TEST_ROOT", root)
            .env("LIGHT_TEST_MODE", mode)
            .spawn()
            .unwrap(),
    )
}
fn wait(path: &std::path::Path) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while !path.exists() {
        assert!(
            Instant::now() < deadline,
            "worker timed out: {}",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn process_worker() {
    let Some(root) = std::env::var_os("LIGHT_TEST_ROOT") else {
        return;
    };
    let root = PathBuf::from(root);
    let mode = std::env::var("LIGHT_TEST_MODE").unwrap();
    let mut options = Options::new(root.join("data"));
    options.busy_timeout = Duration::from_secs(5);
    let mut store = Store::open(options).unwrap();
    if mode == "kill" {
        PAUSE.set(true);
        let _ = store.apply(&report("interrupted", 1, 1));
        panic!("must be killed before event insert");
    }
    fs::write(root.join(format!("ready-{mode}")), b"ready").unwrap();
    wait(&root.join("go"));
    let result = match store.apply(&report(&mode, 1, 1)) {
        Ok(_) => "success",
        Err(Error::RevisionConflict {
            expected: 1,
            actual: 2,
        }) => "revision_conflict",
        other => panic!("unexpected race result: {other:?}"),
    };
    fs::write(root.join(format!("result-{mode}")), result).unwrap();
}

#[test]
fn concurrent_processes_commit_once_and_report_revision_conflict() {
    let mut f = Fixture::new();
    f.task("create");
    let mut a = worker(f.temp.path(), "a");
    let mut b = worker(f.temp.path(), "b");
    wait(&f.temp.path().join("ready-a"));
    wait(&f.temp.path().join("ready-b"));
    fs::write(f.temp.path().join("go"), b"go").unwrap();
    assert!(a.0.wait().unwrap().success());
    assert!(b.0.wait().unwrap().success());
    let mut results = [
        fs::read_to_string(f.temp.path().join("result-a")).unwrap(),
        fs::read_to_string(f.temp.path().join("result-b")).unwrap(),
    ];
    results.sort();
    assert_eq!(results, ["revision_conflict", "success"]);
    assert_eq!(f.store.task(1).unwrap().revision, 2);
    assert_eq!(f.store.events(1, Page::default()).unwrap().len(), 2);
}

#[test]
fn killed_process_rolls_back_mutation_without_event_and_retry_succeeds() {
    let mut f = Fixture::new();
    f.task("create");
    let mut child = worker(f.temp.path(), "kill");
    wait(&f.temp.path().join("ready-kill"));
    child.0.kill().unwrap();
    child.0.wait().unwrap();
    assert_eq!(f.store.task(1).unwrap().revision, 1);
    assert_eq!(f.store.task(1).unwrap().reported_state, "open");
    assert_eq!(f.store.events(1, Page::default()).unwrap().len(), 1);
    f.store.apply(&report("interrupted", 1, 1)).unwrap();
    assert_eq!(f.store.events(1, Page::default()).unwrap().len(), 2);
}

#[test]
fn busy_and_readonly_errors_do_not_partially_write() {
    let mut f = Fixture::new();
    f.task("create");
    let mut another = Store::open(Options::new(f.root())).unwrap();
    let tx = f
        .store
        .conn
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .unwrap();
    let start = Instant::now();
    assert!(
        matches!(another.apply(&report("busy",1,1)),Err(Error::Sqlite(rusqlite::Error::SqliteFailure(e,_))) if e.code == rusqlite::ErrorCode::DatabaseBusy)
    );
    assert!(start.elapsed() < Duration::from_secs(3));
    drop(tx);
    let mut options = Options::new(f.root());
    options.read_only = true;
    let mut readonly = Store::open(options).unwrap();
    assert!(
        matches!(readonly.apply(&report("readonly",1,1)),Err(Error::Sqlite(rusqlite::Error::SqliteFailure(e,_))) if e.code == rusqlite::ErrorCode::ReadOnly)
    );
    assert_eq!(f.store.events(1, Page::default()).unwrap().len(), 1);
}

#[test]
fn disk_full_and_event_failure_rollback_summary_and_revision() {
    let mut f = Fixture::new();
    f.task("create");
    // Force SQLite's real SQLITE_FULL path without filling the host filesystem.
    let pages: i64 = f
        .store
        .conn
        .pragma_query_value(None, "page_count", |r| r.get(0))
        .unwrap();
    f.store
        .conn
        .pragma_update(None, "max_page_count", pages)
        .unwrap();
    let mut r = report("full", 1, 1);
    r.summary = "x".repeat(8192);
    assert!(
        matches!(f.store.apply(&r),Err(Error::Sqlite(rusqlite::Error::SqliteFailure(e,_))) if e.code == rusqlite::ErrorCode::DiskFull)
    );
    assert_eq!(f.store.task(1).unwrap().revision, 1);
    f.store
        .conn
        .pragma_update(None, "max_page_count", pages + 100)
        .unwrap();
    f.store.conn.execute_batch("CREATE TEMP TRIGGER reject_event BEFORE INSERT ON events BEGIN SELECT RAISE(ABORT,'injected event failure'); END;").unwrap();
    assert!(f.store.apply(&report("event", 1, 1)).is_err());
    assert_eq!(f.store.task(1).unwrap().reported_state, "open");
    assert_eq!(f.store.events(1, Page::default()).unwrap().len(), 1);
    f.store
        .conn
        .execute_batch("DROP TRIGGER reject_event")
        .unwrap();
    f.store.apply(&r).unwrap();
}
