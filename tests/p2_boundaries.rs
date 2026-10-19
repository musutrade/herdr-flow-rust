//! Offline regressions for the P2 audit findings.
pub mod common;

use std::ffi::OsString;
use std::fs;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::{symlink, MetadataExt, PermissionsExt};
use std::path::Path;
use std::process::Command;

use herdr_flow::{ci, guard, queue, util};
use serde_json::json;

fn repository(root: &Path) {
    for args in [
        vec!["init", "-q"],
        vec!["config", "user.name", "P2 regression"],
        vec!["config", "user.email", "p2@example.invalid"],
    ] {
        assert!(Command::new("git")
            .args(args)
            .current_dir(root)
            .status()
            .unwrap()
            .success());
    }
    fs::write(root.join("tracked"), "base").unwrap();
    for args in [["add", "tracked"].as_slice(), &["commit", "-qm", "base"]] {
        assert!(Command::new("git")
            .args(args)
            .current_dir(root)
            .status()
            .unwrap()
            .success());
    }
}

#[test]
fn fingerprint_frames_each_untracked_file_and_symlink() {
    let tmp = common::tempdir().unwrap();
    let root = tmp.path();
    repository(root);
    fs::write(root.join("a"), "left").unwrap();
    fs::write(root.join("b"), "right").unwrap();
    let mode = fs::metadata(root.join("b")).unwrap().mode();
    let before = util::fingerprint(root).unwrap();
    fs::write(root.join("a"), format!("leftb\0{mode}right")).unwrap();
    fs::remove_file(root.join("b")).unwrap();
    assert_ne!(before, util::fingerprint(root).unwrap());
    fs::remove_file(root.join("a")).unwrap();
    symlink("left", root.join("a")).unwrap();
    symlink("right", root.join("b")).unwrap();
    let before = util::fingerprint(root).unwrap();
    fs::remove_file(root.join("a")).unwrap();
    fs::remove_file(root.join("b")).unwrap();
    symlink("leftb", root.join("a")).unwrap();
    assert_ne!(before, util::fingerprint(root).unwrap());
}

#[test]
fn fingerprint_binds_raw_non_utf8_names_with_and_without_a_lossy_decoy() {
    let tmp = common::tempdir().unwrap();
    let root = tmp.path();
    repository(root);
    let invalid = root.join(OsString::from_vec(vec![255]));
    fs::write(&invalid, "original").unwrap();
    let before = util::fingerprint(root).unwrap();
    fs::write(&invalid, "changed").unwrap();
    assert_ne!(before, util::fingerprint(root).unwrap());
    fs::write(root.join("\u{fffd}"), "decoy").unwrap();
    let before = util::fingerprint(root).unwrap();
    fs::write(&invalid, "changed again").unwrap();
    assert_ne!(before, util::fingerprint(root).unwrap());
}

#[test]
fn atomic_writes_never_follow_leaf_or_ancestor_symlinks() {
    let tmp = common::tempdir().unwrap();
    let root = tmp.path().join("state");
    common::create_dir(&root).unwrap();
    let victim = tmp.path().join("victim");
    fs::write(&victim, "unchanged").unwrap();
    let target = root.join("queue.json");
    symlink(&victim, &target).unwrap();
    assert!(util::atomic_write_json(&target, &json!({"new": true})).is_err());
    assert_eq!(fs::read_to_string(&victim).unwrap(), "unchanged");
    fs::remove_file(&target).unwrap();
    let missing = tmp.path().join("missing");
    symlink(&missing, &target).unwrap();
    assert!(util::atomic_write_bytes(&target, b"new").is_err());
    assert!(!missing.exists());
    fs::remove_file(&target).unwrap();
    let alias = tmp.path().join("alias");
    symlink(&root, &alias).unwrap();
    assert!(util::atomic_write_bytes(&alias.join("nested/file"), b"new").is_err());
    assert!(!root.join("nested").exists());
    util::atomic_write_bytes(&target, b"first").unwrap();
    util::atomic_write_bytes(&target, b"second").unwrap();
    assert_eq!(fs::read(&target).unwrap(), b"second");
    assert_eq!(fs::metadata(&target).unwrap().mode() & 0o777, 0o600);
}

#[test]
fn state_entries_reject_writable_directories_and_directory_symlinks() {
    let tmp = common::tempdir().unwrap();
    let root = tmp.path().join("state");
    queue::init(&root, None, 30).unwrap();
    assert_eq!(fs::metadata(&root).unwrap().mode() & 0o777, 0o700);
    for mode in [0o700, 0o755] {
        fs::set_permissions(&root, fs::Permissions::from_mode(mode)).unwrap();
        assert!(queue::load_queue(&root).is_ok());
        assert!(queue::locked(&root, false).is_ok());
        assert!(guard::locked(&root).is_ok());
    }
    for mode in [0o770, 0o777, 0o1777] {
        fs::set_permissions(&root, fs::Permissions::from_mode(mode)).unwrap();
        assert!(queue::load_queue(&root).is_err());
        assert!(queue::locked(&root, false).is_err());
        assert!(guard::locked(&root).is_err());
        assert!(queue::save(&root, &mut json!({})).is_err());
        assert!(guard::save(&root, &mut json!({})).is_err());
        assert!(ci::persist(&root, "config", &json!({"state": "success"})).is_err());
    }
    fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
    let alias = tmp.path().join("alias");
    symlink(&root, &alias).unwrap();
    assert!(queue::load_queue(&alias).is_err());
    assert!(queue::locked(&alias, false).is_err());
    assert!(guard::locked(&alias).is_err());
    assert!(ci::persist(&alias, "config", &json!({})).is_err());
}

#[test]
fn queue_init_refuses_existing_writable_or_non_directory_roots() {
    let tmp = common::tempdir().unwrap();
    let root = tmp.path().join("state");
    common::create_dir(&root).unwrap();
    fs::set_permissions(&root, fs::Permissions::from_mode(0o777)).unwrap();
    assert!(queue::init(&root, None, 30).is_err());
    assert!(!root.join("queue.json").exists());
    let file = tmp.path().join("file");
    fs::write(&file, "unchanged").unwrap();
    assert!(util::ensure_private_dir(&file, false).is_err());
    // / is root-owned; checking it must never chmod it or require sudo.
    if unsafe { libc::geteuid() } != 0 {
        assert!(util::ensure_private_dir(Path::new("/"), false).is_err());
    }
}

#[test]
fn locks_refuse_symlink_and_non_regular_targets() {
    let tmp = common::tempdir().unwrap();
    let victim = tmp.path().join("victim");
    fs::write(&victim, "unchanged").unwrap();
    let lock = tmp.path().join("queue.lock");
    symlink(&victim, &lock).unwrap();
    assert!(util::FileLock::acquire(&lock, false).is_err());
    assert_eq!(fs::read_to_string(victim).unwrap(), "unchanged");
    fs::remove_file(&lock).unwrap();
    common::create_dir(&lock).unwrap();
    assert!(util::FileLock::acquire(&lock, false).is_err());
}

#[test]
fn canonical_json_matches_python_exponents_rounding_and_del() {
    let value = json!({"tiny": 1e-7, "large": 1e20, "del": "\u{7f}", "zero": -0.0});
    assert_eq!(
        util::python_sorted_compact(&value),
        r#"{"del":"\u007f","large":1e+20,"tiny":1e-07,"zero":-0.0}"#
    );
    assert_eq!(
        util::python_sorted_default(&value),
        r#"{"del": "\u007f", "large": 1e+20, "tiny": 1e-07, "zero": -0.0}"#
    );
    assert_eq!(
        util::python_sorted_compact_utf8(&json!("\u{7f}")),
        "\"\u{7f}\""
    );
    // CPython json.dumps + hashlib.sha256, independent of this serializer.
    assert_eq!(
        util::sha256_json_sorted(&value),
        "4e18a9eafe436e39d64a82ddaefeefac57abafec04b4ba92ff9f8cd1bc23363b"
    );
    assert_eq!(
        util::sha256_json_compact(&value),
        "a6fd568d10d0426cda494072e155e78aa8d839c0cded58c4f86272f8c6283e5d"
    );
}

#[test]
fn python_float_corpus_matches_in_both_separator_modes_and_roundtrips() {
    for line in include_str!("fixtures/python_floats.tsv")
        .lines()
        .filter(|line| !line.starts_with('#'))
    {
        let (bits, expected) = line.split_once('\t').unwrap();
        let bits = u64::from_str_radix(bits, 16).unwrap();
        let value = json!(f64::from_bits(bits));
        assert_eq!(
            util::python_sorted_compact(&value),
            expected,
            "bits={bits:x}"
        );
        assert_eq!(
            util::python_sorted_default(&value),
            expected,
            "bits={bits:x}"
        );
        let parsed: serde_json::Value = serde_json::from_str(expected).unwrap();
        assert_eq!(parsed.as_f64().unwrap().to_bits(), bits, "{expected}");
    }
}

#[test]
fn state_reads_and_watch_launches_validate_the_same_boundary() {
    let tmp = common::tempdir().unwrap();
    let root = tmp.path().join("state");
    queue::init(&root, None, 30).unwrap();
    let config = tmp.path().join("ci.json");
    util::atomic_write_json(
        &config,
        &json!({"version": 1, "task": "o/r#1", "epoch": 1,
        "repo": "o/r", "sha": "a".repeat(40),
        "workflows": [{"file": "ci.yml", "event": "push", "jobs": ["gate"]}]}),
    )
    .unwrap();
    fs::set_permissions(&root, fs::Permissions::from_mode(0o777)).unwrap();
    assert!(ci::run(&config, &root, true, None).is_err());
    assert!(ci::background(&config, &root).is_err());
    assert!(herdr_flow::watch_service::background(
        &root,
        &[],
        || Ok(()),
        std::time::Duration::from_secs(1)
    )
    .is_err());
    fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
    assert!(util::read_state_json(&root, "../ci.json").is_err());
    fs::remove_file(root.join("queue.json")).unwrap();
    symlink(&config, root.join("queue.json")).unwrap();
    assert!(queue::load_queue(&root).is_err());
    assert!(!root.join("watch.json").exists());
}
