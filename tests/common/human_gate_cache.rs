use super::*;

#[test]
fn cache_reuses_only_the_exact_authority_and_rejects_atomic_replacement() {
    let temp = util::fs::test_tempdir();
    let root = temp.path().join("queue");
    queue::init_isolated(&root, None, 30).unwrap();
    let mut state = queue::load_queue(&root).unwrap();
    state["tasks"] = json!({"task":{"human_writer":null}});
    util::atomic_write_json(&root.join("queue.json"), &state).unwrap();
    let first = load(&root).unwrap();
    assert!(!first.blocks(Path::new("/not-created")));
    assert!(Arc::ptr_eq(&first, &load(&root).unwrap()));
    let payload = root.join("queue.json/authority.json");
    let metadata = std::fs::metadata(&payload).unwrap();
    state["tasks"]["task"][KEY] = json!(true); // same bytes as null
    util::atomic_write_json(&root.join("queue.json"), &state).unwrap();
    std::fs::File::options()
        .write(true)
        .open(&payload)
        .unwrap()
        .set_times(std::fs::FileTimes::new().set_modified(metadata.modified().unwrap()))
        .unwrap();
    let replacement = std::fs::metadata(&payload).unwrap();
    assert_eq!(replacement.len(), metadata.len());
    assert_eq!(
        replacement.modified().unwrap(),
        metadata.modified().unwrap()
    );
    assert_ne!(replacement.ino(), metadata.ino());
    let changed = load(&root).unwrap();
    assert!(!Arc::ptr_eq(&first, &changed));
    assert!(changed.blocks(Path::new("/not-created")));
    assert!(Arc::ptr_eq(&changed, &load(&root).unwrap()));
    std::fs::write(&payload, "unreadable JSON").unwrap();
    assert!(
        load(&root).is_err(),
        "read failures must not become cached authority"
    );
    state["tasks"] = json!({});
    util::atomic_write_json(&root.join("queue.json"), &state).unwrap();
    assert!(!load(&root).unwrap().blocks(Path::new("/not-created")));
    std::fs::remove_file(&payload).unwrap();
    assert!(load(&root).is_err());
}
