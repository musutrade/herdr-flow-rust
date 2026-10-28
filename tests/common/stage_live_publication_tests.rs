use super::*;
use std::sync::mpsc;

fn root() -> tempfile::TempDir {
    let root = tempfile::Builder::new()
        .permissions(fs::Permissions::from_mode(0o700))
        .tempdir()
        .unwrap();
    util::protected::namespace(root.path()).unwrap();
    root
}

fn panic_text(result: std::thread::Result<Option<J>>) -> String {
    let error = result.expect_err("persistent invalid presentation must fail closed");
    if let Some(text) = error.downcast_ref::<String>() {
        text.clone()
    } else {
        error.downcast_ref::<&str>().unwrap().to_string()
    }
}

#[test]
fn protected_presentation_publication_gap_defers_until_producer_releases_lock() {
    let directory = root();
    let path = directory.path().to_owned();
    let writer_root = path.clone();
    let expected = json!({"phase":"ready","session":"original-session"});
    let written = expected.clone();
    let (reserved, observe) = mpsc::sync_channel(0);
    let (publish, proceed) = mpsc::sync_channel(0);
    let writer = std::thread::spawn(move || {
        let _lock = util::FileLock::acquire(&writer_root.join("presentation.lock"), false)
            .unwrap()
            .unwrap();
        // Freeze the real first-publication gap: the directory is reserved,
        // but its protected marker and authority payload do not yet exist.
        let document = writer_root.join("presentation.json");
        fs::create_dir(&document).unwrap();
        fs::set_permissions(&document, fs::Permissions::from_mode(0o700)).unwrap();
        reserved.send(()).unwrap();
        proceed.recv_timeout(Duration::from_secs(10)).unwrap();
        // Use the real protected writer to finish marker + atomic payload.
        util::atomic_write_json(&document, &written).unwrap();
    });
    observe.recv_timeout(Duration::from_secs(10)).unwrap();
    assert!(path.join("presentation.json").is_dir());
    assert!(!path.join("presentation.json/.flow-document-v2").exists());
    let racing = std::panic::catch_unwind(|| read(&path, "presentation.json"));
    // Always release/join the producer, including the red-test panic path.
    publish.send(()).unwrap();
    writer.join().unwrap();
    assert_eq!(
        racing.expect("audit read must defer while publication lock is held"),
        None
    );
    assert_eq!(read(&path, "presentation.json"), Some(expected));
    assert!(matches!(
        util::probe_existing_lock(&path.join("presentation.lock")).unwrap(),
        util::LockProbe::Acquired(_)
    ));
}

#[test]
fn observing_unpublished_presentation_never_creates_lock_or_document() {
    let directory = root();
    let before: Vec<_> = fs::read_dir(directory.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert_eq!(read(directory.path(), "presentation.json"), None);
    let after: Vec<_> = fs::read_dir(directory.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert_eq!(before, after);
    assert!(!directory.path().join("presentation.lock").exists());
    assert!(!directory.path().join("presentation.json").exists());
}

#[test]
fn quiescent_unprotected_marker_only_and_malformed_presentations_are_errors() {
    for kind in ["unprotected", "marker-only", "malformed"] {
        let directory = root();
        let path = directory.path();
        let document = path.join("presentation.json");
        let lock = util::FileLock::acquire(&path.join("presentation.lock"), false)
            .unwrap()
            .unwrap();
        match kind {
            "unprotected" => {
                fs::create_dir(&document).unwrap();
                fs::set_permissions(&document, fs::Permissions::from_mode(0o700)).unwrap();
            }
            "marker-only" => util::protected::create(&document).unwrap(),
            "malformed" => {
                util::atomic_write_json(&document, &json!({"phase":"ready"})).unwrap();
                fs::write(document.join("authority.json"), b"{broken").unwrap();
            }
            _ => unreachable!(),
        }
        // Busy publication is deferred even at an incomplete writer boundary.
        assert_eq!(read(path, "presentation.json"), None);
        drop(lock);
        let error = panic_text(std::panic::catch_unwind(|| read(path, "presentation.json")));
        assert!(
            error.contains(&document.display().to_string()),
            "{kind}: {error}"
        );
        assert!(error.contains("op=read"), "{kind}: {error}");
        // Failed reads must release their existing producer lock as well.
        assert!(matches!(
            util::probe_existing_lock(&path.join("presentation.lock")).unwrap(),
            util::LockProbe::Acquired(_)
        ));
    }
}

#[test]
fn published_document_without_producer_lock_is_not_treated_as_unpublished() {
    let directory = root();
    let document = directory.path().join("presentation.json");
    util::atomic_write_json(&document, &json!({"phase":"ready"})).unwrap();
    let error = panic_text(std::panic::catch_unwind(|| {
        read(directory.path(), "presentation.json")
    }));
    assert!(
        error.contains("presentation.lock") && error.contains(&document.display().to_string()),
        "{error}"
    );
    assert!(!directory.path().join("presentation.lock").exists());
}

#[test]
fn presentation_lock_does_not_hide_corruption_in_other_runtime_documents() {
    let directory = root();
    let _writer = util::FileLock::acquire(&directory.path().join("presentation.lock"), false)
        .unwrap()
        .unwrap();
    let document = directory.path().join("runtime.json");
    fs::create_dir(&document).unwrap();
    fs::set_permissions(&document, fs::Permissions::from_mode(0o700)).unwrap();
    let error = panic_text(std::panic::catch_unwind(|| {
        read(directory.path(), "runtime.json")
    }));
    assert!(
        error.contains("op=read") && error.contains(&document.display().to_string()),
        "{error}"
    );
}
