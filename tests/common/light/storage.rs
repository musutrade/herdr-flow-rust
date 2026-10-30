use super::*;
use std::os::unix::fs::{symlink, PermissionsExt};

#[test]
fn private_wal_and_backup_restore_preserve_reports() {
    let mut f = Fixture::new();
    let a = f.task("a");
    let db = f.store.database_path();
    for p in [
        f.root(),
        db.clone(),
        PathBuf::from(format!("{}-wal", db.display())),
        PathBuf::from(format!("{}-shm", db.display())),
    ] {
        let m = fs::metadata(p).unwrap();
        assert_eq!(
            m.permissions().mode() & 0o777,
            if m.is_dir() { 0o700 } else { 0o600 }
        );
    }
    let before = fs::metadata(f.temp.path()).unwrap().permissions().mode();
    let mut r = report("report", a.task_id, 1);
    r.refs.push(Reference {
        target: "/outside/result.json".into(),
        commit: None,
        result: None,
        recorded_at: None,
        sha256: Some("a".repeat(64)),
        temporary: false,
    });
    f.store.apply(&r).unwrap();
    let root = f.temp.path().join("backup");
    f.store.backup(&root, &[]).unwrap();
    let mut restored = Store::open(Options::new(&root)).unwrap();
    assert_eq!(
        restored.task(a.task_id).unwrap(),
        f.store.task(a.task_id).unwrap()
    );
    assert_eq!(
        restored.events(a.task_id, Page::default()).unwrap(),
        f.store.events(a.task_id, Page::default()).unwrap()
    );
    assert_eq!(restored.apply(&r).unwrap(), f.store.apply(&r).unwrap());
    assert!(f.store.backup(&root, &[]).is_err());
    assert!(f.store.backup(&f.root().join("bad"), &[]).is_err());
    assert_eq!(
        before,
        fs::metadata(f.temp.path()).unwrap().permissions().mode()
    );
    assert!(f.store.sqlite_version().unwrap().starts_with("3."));
}

#[test]
fn unsupported_and_nonempty_unversioned_databases_are_rejected() {
    let f = Fixture::new();
    f.store.conn.pragma_update(None, "user_version", 2).unwrap();
    assert!(matches!(
        Store::open(Options::new(f.root())),
        Err(Error::UnsupportedSchema(2))
    ));
    assert!(matches!(f.store.task(1), Err(Error::UnsupportedSchema(2))));
    f.store.conn.pragma_update(None, "user_version", 0).unwrap();
    assert!(matches!(
        Store::open(Options::new(f.root())),
        Err(Error::Invalid(_))
    ));
}

#[test]
fn forbidden_paths_aliases_and_replacement_are_rejected() {
    let t = tempfile::tempdir().unwrap();
    let root = t.path().join("data");
    for forbidden in [&root, t.path(), &root.join("nested")] {
        let mut options = Options::new(&root);
        options.forbidden_directories.push(forbidden.to_path_buf());
        assert!(Store::open(options).is_err());
    }
    assert!(!root.exists());
    let forbidden = t.path().join("reclaim");
    fs::create_dir(&forbidden).unwrap();
    let alias = t.path().join("alias");
    symlink(&forbidden, &alias).unwrap();
    let mut options = Options::new(alias.join("new"));
    options.forbidden_directories.push(forbidden);
    assert!(Store::open(options).is_err());
    let mut good = Options::new(&root);
    good.forbidden_directories.push(t.path().join("database"));
    let store = Store::open(good).unwrap();
    fs::rename(&root, t.path().join("moved")).unwrap();
    symlink(t.path().join("moved"), &root).unwrap();
    assert!(store.projects(Page::default()).is_err());
    let dangling = t.path().join("dangling");
    symlink(t.path().join("absent"), &dangling).unwrap();
    assert!(Store::open(Options::new(dangling.join("child"))).is_err());
}

#[test]
fn nonprivate_directory_and_symlink_database_are_rejected_without_chmod() {
    let t = tempfile::tempdir().unwrap();
    let root = t.path().join("public");
    fs::create_dir(&root).unwrap();
    fs::set_permissions(&root, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(Store::open(Options::new(&root)).is_err());
    assert_eq!(
        fs::metadata(&root).unwrap().permissions().mode() & 0o777,
        0o755
    );
    fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
    let elsewhere = t.path().join("other");
    fs::write(&elsewhere, b"untouched").unwrap();
    symlink(&elsewhere, root.join("records.sqlite3")).unwrap();
    assert!(Store::open(Options::new(&root)).is_err());
    assert_eq!(fs::read(elsewhere).unwrap(), b"untouched");
}

#[test]
fn empty_old_environment_and_readonly_queries_work() {
    let mut f = Fixture::new();
    let t = f.task("a");
    let mut options = Options::new(f.root());
    options.read_only = true;
    let read = Store::open(options).unwrap();
    assert_eq!(read.task(t.task_id).unwrap().revision, 1);
    assert_eq!(
        read.conn
            .pragma_query_value(None, "foreign_keys", |r| r.get::<_, i64>(0))
            .unwrap(),
        1
    );
    assert_eq!(
        read.conn
            .pragma_query_value(None, "journal_mode", |r| r.get::<_, String>(0))
            .unwrap(),
        "wal"
    );
    assert!(!f.temp.path().join("queue.json").exists());
}

#[test]
fn backup_during_reports_is_a_consistent_snapshot() {
    let mut f = Fixture::new();
    f.task("create");
    let root = f.root();
    let writer = std::thread::spawn(move || {
        let mut store = Store::open(Options::new(root)).unwrap();
        for revision in 1..=20 {
            store
                .apply(&report(&format!("r{revision}"), 1, revision))
                .unwrap();
        }
    });
    let backup = f.temp.path().join("snapshot");
    f.store.backup(&backup, &[]).unwrap();
    writer.join().unwrap();
    let snapshot = Store::open(Options::new(backup)).unwrap();
    let revision = snapshot.task(1).unwrap().revision;
    assert_eq!(
        snapshot.events(1, Page::default()).unwrap().len() as i64,
        revision
    );
    assert!((1..=21).contains(&revision));
    assert_eq!(f.store.task(1).unwrap().revision, 21);
}

#[test]
fn sidecar_and_hardlink_aliases_cannot_be_opened() {
    let f = Fixture::new();
    let db = f.store.database_path();
    let root = f.root();
    drop(f.store);
    let protected = f.temp.path().join("protected");
    fs::write(&protected, b"original").unwrap();
    let sidecar = PathBuf::from(format!("{}-wal", db.display()));
    symlink(&protected, &sidecar).unwrap();
    assert!(Store::open(Options::new(&root)).is_err());
    fs::remove_file(sidecar).unwrap();
    fs::hard_link(&db, f.temp.path().join("alias.sqlite")).unwrap();
    assert!(Store::open(Options::new(root)).is_err());
    assert_eq!(fs::read(protected).unwrap(), b"original");
}

#[test]
fn linked_sqlite_version_and_build_options_are_observable() {
    let f = Fixture::new();
    let version = f.store.sqlite_version().unwrap();
    let source: String = f
        .store
        .conn
        .query_row("SELECT sqlite_source_id()", [], |r| r.get(0))
        .unwrap();
    let mut q = f.store.conn.prepare("PRAGMA compile_options").unwrap();
    let options = q
        .query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    println!("SQLite version={version}; source_id={source}; compile_options={options:?}");
    assert_eq!(version, rusqlite::version());
    assert!(options.iter().any(|s| s == "THREADSAFE=1"));
}
