//! Real file read-volume regressions, isolated from parallel tests.
pub mod common;

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::process::Command;

use herdr_flow::guard;
use serde_json::{json, Value};

fn metadata(id: &str) -> Vec<u8> {
    format!(
        "{}\n",
        json!({"type":"session_meta", "payload":{"id":id,"cwd":"/fixture"}})
    )
    .into_bytes()
}

fn read_bytes() -> u64 {
    fs::read_to_string("/proc/self/io")
        .unwrap()
        .lines()
        .find_map(|line| line.strip_prefix("rchar: "))
        .unwrap()
        .trim()
        .parse()
        .unwrap()
}

fn append(path: &Path, data: &[u8]) {
    OpenOptions::new()
        .append(true)
        .open(path)
        .unwrap()
        .write_all(data)
        .unwrap();
}

#[test]
fn transcript_reads_only_prefix_and_append() {
    if std::env::var_os("HERDR_TRANSCRIPT_IO_CHILD").is_none() {
        let output = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "transcript_reads_only_prefix_and_append",
                "--nocapture",
            ])
            .env("HERDR_TRANSCRIPT_IO_CHILD", "1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    let temp = common::tempdir().unwrap();
    let path = temp.path().join("rollout.jsonl");
    let mut data = metadata("first");
    let row = format!("{}\n", json!({"padding":"x".repeat(4096)}));
    for _ in 0..4096 {
        data.extend_from_slice(row.as_bytes());
    }
    fs::write(&path, data).unwrap();
    let first = guard::transcript(&path).unwrap();
    let before = read_bytes();
    let next = guard::transcript_state(&path, Some(&first["cursor"])).unwrap();
    let unchanged = read_bytes() - before;
    assert_eq!(next["cursor"], first["cursor"]);
    assert!(
        unchanged <= guard::HEAD_BYTES as u64 + 2048,
        "unchanged read {unchanged} bytes"
    );
    let record = format!(
        "{}\n",
        json!({"type":"response_item","payload":{"type":"message","role":"user"}})
    );
    append(&path, record.as_bytes());
    let before = read_bytes();
    let last = guard::transcript_state(&path, Some(&next["cursor"])).unwrap();
    let appended = read_bytes() - before;
    assert_eq!(last["user_messages"], 1);
    assert!(
        appended <= guard::HEAD_BYTES as u64 + record.len() as u64 + 2048,
        "append read {appended} bytes"
    );
}

fn assert_rescan(path: &Path, cursor: &Value, id: &str) {
    let got = guard::transcript_state(path, Some(cursor)).unwrap();
    assert_eq!(got["session"], id);
    assert_eq!(got["user_messages"], 0);
    assert_eq!(got, guard::transcript(path).unwrap());
}

#[test]
fn transcript_invalid_cursors_rescan_current_file() {
    let temp = common::tempdir().unwrap();
    let path = temp.path().join("rollout.jsonl");
    fs::write(&path, metadata("first")).unwrap();
    let first = guard::transcript(&path).unwrap();
    // Same inode, same size, changed prefix.
    fs::write(&path, metadata("other")).unwrap();
    assert_rescan(&path, &first["cursor"], "other");
    // Replacement with an equal prefix and a different inode.
    let current = guard::transcript(&path).unwrap();
    let replacement = temp.path().join("replacement");
    fs::write(&replacement, metadata("other")).unwrap();
    fs::rename(&replacement, &path).unwrap();
    assert_rescan(&path, &current["cursor"], "other");
    // Truncation and malformed or excessive cursor fields must also rescan.
    fs::write(&path, metadata("x")).unwrap();
    assert_rescan(&path, &current["cursor"], "x");
    let current = guard::transcript(&path).unwrap();
    for (key, value) in [
        ("offset", json!(u64::MAX)),
        ("offset", json!(-1)),
        ("head_len", json!(u64::MAX)),
        ("head", json!("invalid")),
    ] {
        let mut cursor = current["cursor"].clone();
        cursor[key] = value;
        assert_rescan(&path, &cursor, "x");
    }
}
