use herdr_flow::{guard, model::J, queue, util};
use serde_json::json;

pub fn rejects_without_writing(mut state: J, spec: &J, action: &str, diagnostic: &str) {
    let temp = super::common::tempdir().unwrap();
    let root = temp.path().join("queue");
    let guard_root = temp.path().join("guard");
    let session = "11111111-1111-1111-1111-111111111111";
    let registration =
        json!({"session":session,"phase":"monitoring","task":"fixture","role":"coordinator"});
    util::atomic_write_json(&guard_root.join("state.json"), &registration).unwrap();
    state["guard_dir"] = json!(guard_root);
    state["receiver"] = guard::scope(&registration);
    queue::save(&root, &mut state).unwrap();
    let spec_path = temp.path().join("spec.json");
    util::atomic_write_json(&spec_path, spec).unwrap();
    let binary = std::env::var_os("HERDR_TEST_RELEASE_BIN")
        .unwrap_or_else(|| env!("CARGO_BIN_EXE_herdr-flow").into());
    let output = std::process::Command::new(binary)
        .env("CODEX_THREAD_ID", session)
        .args(["queue", action, "--state-dir"])
        .arg(&root)
        .arg("--spec")
        .arg(&spec_path)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains(diagnostic),
        "{output:?}"
    );
    assert_eq!(queue::load_queue(&root).unwrap(), state);
}
