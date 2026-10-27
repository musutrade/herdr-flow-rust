use super::*;

#[test]
#[ignore = "explicit audited bindings from already-closed real acceptance panes"]
fn reclaim_previously_closed_live_presentations() {
    let path = std::env::var("HERDR_RETAINED_PRESENTATIONS").unwrap();
    let bindings = util::read_json(Path::new(&path)).unwrap();
    for binding in bindings.as_array().unwrap() {
        assert!(super::super::herdr::pane(binding).unwrap().is_none());
        reclaim(binding).unwrap();
    }
}

fn fixture() -> (tempfile::TempDir, J, J) {
    let dir = util::fs::test_tempdir();
    let value = json!({"version":3,"selected":0,"workspaces":[{
        "id":"user","public_pane_numbers":{"1":1,"2":2},"next_public_pane_number":3,
        "tabs":[{"root_pane":1,"focused":2,"zoomed":true,
            "layout":{"Split":{"direction":"Vertical","ratio":0.3,"first":{"Pane":1},"second":{"Pane":2}}},
            "panes":{"1":{"cwd":"/user","agent_session":{"agent":"codex","value":"foreign"}},
                "2":{"cwd":"/task","agent_session":{"agent":"codex","value":"owned"}}}}]}]});
    let mut absent = value.clone();
    let binding = json!({"session":"owned","cwd":"/task"});
    snapshot::prune(&mut absent, &binding).unwrap();
    fs::write(
        dir.path().join("session.json"),
        serde_json::to_vec(&absent).unwrap(),
    )
    .unwrap();
    let mut config = json!({"session_store":dir.path().join("session.json")});
    configure(&mut config).unwrap();
    let binding = json!({"session":"owned","cwd":"/task","session_store":config["session_store"]});
    (dir, value, binding)
}

#[test]
fn removes_only_owned_panes_and_layout_leaves_preserving_shared_assets() {
    let (dir, value, binding) = fixture();
    let current = fs::read(dir.path().join("session.json")).unwrap();
    for name in ["session-snapshots", "session-backups"] {
        let folder = dir.path().join(name);
        util::ensure_private_dir(&folder, true).unwrap();
        let user_file = folder.join("notes.txt");
        fs::write(&user_file, b"owned /task user notes, not a snapshot").unwrap();
        let path = folder.join("session-1.json");
        fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
        let metadata = fs::metadata(&path).unwrap();
        let foreign = folder.join("session-2.json");
        fs::write(&foreign, &current).unwrap();
        reclaim(&binding).unwrap();
        assert_eq!(fs::read(&path).unwrap(), current);
        assert_eq!(fs::read(&foreign).unwrap(), current);
        assert_eq!(
            fs::read(&user_file).unwrap(),
            b"owned /task user notes, not a snapshot"
        );
        assert_eq!(
            fs::metadata(&path).unwrap().permissions(),
            metadata.permissions()
        );
        assert_eq!(
            fs::metadata(&path).unwrap().modified().unwrap(),
            metadata.modified().unwrap()
        );
        assert_eq!(fs::read(dir.path().join("session.json")).unwrap(), current);
    }
    reclaim(&binding).unwrap();
    reclaim(&json!({})).unwrap();
}

#[test]
fn unknown_format_foreign_binding_and_root_panes_are_never_pruned() {
    let (_, original, binding) = fixture();
    for (key, value) in [("version", json!(4)), ("workspaces", J::Null)] {
        let mut changed = original.clone();
        changed[key] = value;
        assert!(snapshot::prune(&mut changed, &binding).is_err());
    }
    let mut wrong = binding.clone();
    wrong["cwd"] = json!("/other");
    assert!(snapshot::prune(&mut original.clone(), &wrong).is_err());
    let mut root = original.clone();
    root["workspaces"][0]["tabs"][0]["root_pane"] = json!(2);
    assert!(snapshot::prune(&mut root, &binding).is_err());
    let mut unknown = original.clone();
    unknown["workspaces"][0]["tabs"][0]["layout"] = json!({"Future":2});
    assert!(snapshot::prune(&mut unknown, &binding).is_err());
    let mut foreign = binding.clone();
    foreign["session"] = json!("different");
    let mut copy = original.clone();
    assert!(!snapshot::prune(&mut copy, &foreign).unwrap());
    assert_eq!(copy, original);
}

#[test]
fn nested_layout_removes_reopened_task_panes_and_keeps_other_splits() {
    let (_, mut value, binding) = fixture();
    let tab = &mut value["workspaces"][0]["tabs"][0];
    tab["layout"] = json!({"Split":{"direction":"Horizontal","ratio":0.7,
        "first":tab["layout"],"second":{"Pane":3}}});
    tab["panes"]["3"] = json!({"cwd":"/user-two"});
    assert!(snapshot::prune(&mut value, &binding).unwrap());
    let tab = &value["workspaces"][0]["tabs"][0];
    assert_eq!(tab["layout"]["Split"]["ratio"], 0.7);
    assert_eq!(tab["layout"]["Split"]["first"], json!({"Pane":1}));
    assert_eq!(tab["panes"]["3"], json!({"cwd":"/user-two"}));
}

#[test]
fn captured_pane_identity_survives_agent_exit_but_never_matches_a_replacement() {
    let (dir, mut value, binding) = fixture();
    fs::write(
        dir.path().join("session.json"),
        serde_json::to_vec(&value).unwrap(),
    )
    .unwrap();
    let captured = capture(&binding).unwrap();
    assert_eq!(capture(&captured).unwrap(), captured);
    assert_eq!(
        captured["persisted_panes"],
        json!([{"workspace":"user","id":"2","number":2}])
    );
    value["workspaces"][0]["tabs"][0]["panes"]["2"]
        .as_object_mut()
        .unwrap()
        .remove("agent_session");
    assert!(!snapshot::prune(&mut value.clone(), &binding).unwrap());
    let mut replacement = value.clone();
    replacement["workspaces"][0]["tabs"][0]["panes"]["2"]["agent_session"] =
        json!({"value":"foreign","agent":"codex"});
    assert!(snapshot::prune(&mut replacement, &captured).is_err());
    assert!(snapshot::prune(&mut value, &captured).unwrap());
    assert!(value["workspaces"][0]["tabs"][0]["panes"]["2"].is_null());
}

#[test]
fn saved_shell_is_reclaimed_without_session_text_or_deleting_another_shell() {
    let (dir, mut value, mut binding) = fixture();
    binding["persisted_panes"] = json!(snapshot::identify(&value, &binding).unwrap());
    let panes = &mut value["workspaces"][0]["tabs"][0]["panes"];
    for id in ["1", "2"] {
        panes[id].as_object_mut().unwrap().remove("agent_session");
        panes[id]["cwd"] = binding["cwd"].clone();
    }
    let folder = dir.path().join("session-snapshots");
    util::ensure_private_dir(&folder, true).unwrap();
    let path = folder.join("session-1.json");
    fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
    reclaim(&binding).unwrap();
    let saved = util::read_json(&path).unwrap();
    assert!(saved["workspaces"][0]["tabs"][0]["panes"]["2"].is_null());
    assert_eq!(
        saved["workspaces"][0]["tabs"][0]["panes"]["1"],
        value["workspaces"][0]["tabs"][0]["panes"]["1"]
    );
}

fn tab_fixture() -> (tempfile::TempDir, J, J) {
    let (dir, value, mut binding) = fixture();
    let mut value = value.clone();
    let workspace = &mut value["workspaces"][0];
    let mut owned = workspace["tabs"][0].clone();
    owned["root_pane"] = json!(2);
    owned["layout"] = json!({"Pane":2});
    owned["panes"].as_object_mut().unwrap().remove("1");
    let mut user = workspace["tabs"][0].clone();
    snapshot::prune(&mut value, &binding).unwrap();
    user["panes"].as_object_mut().unwrap().remove("2");
    user["layout"] = json!({"Pane":1});
    user["focused"] = json!(1);
    let workspace = &mut value["workspaces"][0];
    workspace["tabs"] = json!([owned, user]);
    workspace["public_pane_numbers"] = json!({"1":1,"2":2});
    workspace["public_tab_numbers"] = json!([42, 3]);
    workspace["active_tab"] = json!(1);
    binding["workspace"] = json!("user");
    binding["tab_number"] = json!(42);
    (dir, value, binding)
}

#[test]
fn owned_tab_is_removed_with_maps_and_active_user_tab_preserved() {
    let (dir, value, binding) = tab_fixture();
    let user = value["workspaces"][0]["tabs"][1].clone();
    let mut expected = value.clone();
    assert!(snapshot::prune(&mut expected, &binding).unwrap());
    let workspace = &expected["workspaces"][0];
    assert_eq!(workspace["tabs"], json!([user]));
    assert_eq!(workspace["public_tab_numbers"], json!([3]));
    assert_eq!(workspace["public_pane_numbers"], json!({"1":1}));
    assert_eq!(workspace["active_tab"], 0);
    fs::write(
        dir.path().join("session.json"),
        serde_json::to_vec(&expected).unwrap(),
    )
    .unwrap();
    let folder = dir.path().join("session-snapshots");
    util::ensure_private_dir(&folder, true).unwrap();
    let path = folder.join("session-1.json");
    fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
    reclaim(&binding).unwrap();
    assert_eq!(util::read_json(&path).unwrap(), expected);

    let mut active_owned = value.clone();
    active_owned["workspaces"][0]["active_tab"] = json!(0);
    snapshot::prune(&mut active_owned, &binding).unwrap();
    assert_eq!(active_owned["workspaces"][0]["active_tab"], 0);
    let mut foreign = binding.clone();
    foreign["tab_number"] = json!(3);
    assert!(snapshot::prune(&mut value.clone(), &foreign).is_err());
    foreign["tab_number"] = binding["tab_number"].clone();
    foreign["workspace"] = json!("different");
    assert!(snapshot::prune(&mut value.clone(), &foreign).is_err());
}

#[test]
fn user_pane_added_to_owned_tab_survives_owned_root_cleanup() {
    let (_, mut value, binding) = tab_fixture();
    let original = value["workspaces"][0]["tabs"][1].clone();
    let user_pane = original["panes"]["1"].clone();
    let tab = &mut value["workspaces"][0]["tabs"][0];
    tab["panes"]["3"] = user_pane.clone();
    tab["layout"] = json!({"Split":{"first":{"Pane":2},"second":{"Pane":3}}});
    value["workspaces"][0]["public_pane_numbers"]["3"] = json!(9);
    snapshot::prune(&mut value, &binding).unwrap();
    let workspace = &value["workspaces"][0];
    assert_eq!(workspace["tabs"][1], original);
    assert_eq!(workspace["public_tab_numbers"], json!([42, 3]));
    assert_eq!(workspace["active_tab"], 1);
    assert_eq!(workspace["tabs"][0]["root_pane"], 3);
    assert_eq!(workspace["tabs"][0]["focused"], 3);
    assert_eq!(workspace["tabs"][0]["panes"], json!({"3":user_pane}));
}

#[test]
fn replaced_deleted_and_symlinked_copies_are_preserved_or_refused() {
    use std::os::unix::fs::symlink;
    let (dir, value, _) = fixture();
    let path = dir.path().join("session-1.json");
    fs::write(&path, b"original").unwrap();
    let metadata = fs::metadata(&path).unwrap();
    fs::remove_file(&path).unwrap();
    assert!(replace_snapshot(&path, &metadata, b"original", b"new").is_err());
    assert!(!path.exists());
    fs::write(&path, b"replacement").unwrap();
    assert!(replace_snapshot(&path, &metadata, b"original", b"new").is_err());
    assert_eq!(fs::read(&path).unwrap(), b"replacement");
    let link = dir.path().join("session-link.json");
    symlink(&path, &link).unwrap();
    assert!(read_snapshot(&link).is_err());
    let mut config = json!({"session_store":link});
    assert!(configure(&mut config).is_err());
    fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
    assert!(is_snapshot(&path));
    assert!(!is_snapshot(&dir.path().join("unrelated.json")));
}

#[test]
fn close_must_be_persisted_and_store_identity_cannot_change() {
    let (dir, value, mut binding) = fixture();
    binding["session_store"]["ino"] = json!(0);
    assert!(reclaim(&binding).is_err());
    binding["session_store"] = store_identity(&dir.path().join("session.json")).unwrap();
    fs::write(
        dir.path().join("session.json"),
        serde_json::to_vec(&value).unwrap(),
    )
    .unwrap();
    assert!(reclaim(&binding)
        .unwrap_err()
        .message
        .contains("close is not persisted"));
    assert_eq!(
        util::read_json(&dir.path().join("session.json")).unwrap(),
        value
    );
}
