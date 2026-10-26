//! Producer readiness observes only its own queue result path.
pub mod common;
use herdr_flow::{queue, util};
use serde_json::{json, Value as J};
use std::{path::PathBuf, process::Command};
struct Fixture {
    _temp: tempfile::TempDir,
    root: PathBuf,
    cwd: PathBuf,
    spec: J,
}
impl Fixture {
    fn new() -> Self {
        let temp = common::tempdir().unwrap();
        let root = temp.path().join("queue");
        let cwd = temp.path().join("checkout");
        common::create_dir_all(&cwd).unwrap();
        queue::init(&root, None, 30).unwrap();
        common::create_dir_all(root.join("results")).unwrap();
        let executable = std::fs::canonicalize(env!("CARGO_BIN_EXE_herdr-flow")).unwrap();
        let spec = json!({"role":"producer","cwd":cwd,"executable":executable,
            "sha256":util::pyjson::sha256_hex(&std::fs::read(&executable).unwrap()),
            "identity":{"adapter":"codex","session":"sesfixture"}});
        Self {
            _temp: temp,
            root,
            cwd,
            spec,
        }
    }
    fn run(&self, spec: &J) -> J {
        let path = self.root.join("probe-spec.json");
        util::atomic_write_json(&path, spec).unwrap();
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_herdr-flow"));
        cmd.args(["queue", "readiness", "--state-dir"])
            .arg(&self.root)
            .arg("--spec")
            .arg(path)
            .current_dir(&self.cwd)
            .env("CODEX_THREAD_ID", "sesfixture");
        read_report(&mut cmd)
    }
}
fn read_report(cmd: &mut Command) -> J {
    let output = cmd.output().unwrap();
    assert_eq!(
        output.status.code(),
        Some(1),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: J = serde_json::from_slice(&output.stdout).unwrap();
    let text = report.to_string();
    assert!(!text.contains("fixture-credential-never-report"));
    assert!(!text.contains("never-report-this"));
    assert_eq!(report["ready"], false);
    assert_eq!(report["active_wakeup"], false);
    report
}

fn channel(report: &J) -> &J {
    &report["observed"]["channel"]["status"]
}

#[test]
fn producer_needs_only_its_result_path_and_never_queries_receiver() {
    let f = Fixture::new();
    let mut queue = queue::load_queue(&f.root).unwrap();
    queue["waits"]["w"] = json!({"kind":"signal","closed":false,
        "producer":{"adapter":"codex","session":"sesfixture"}});
    queue::save(&f.root, &mut queue).unwrap();
    let mut spec = f.spec.clone();
    spec["role"] = json!("producer");
    spec["wait"] = json!("w");
    let result = f.run(&spec);
    assert_eq!(channel(&result), "ready");
    assert_eq!(
        result["observed"]["channel"]["control_socket"],
        "not_required"
    );
    assert_eq!(
        std::fs::read_dir(f.root.join("results")).unwrap().count(),
        0
    );
    spec["wait"] = json!("missing");
    assert_eq!(channel(&f.run(&spec)), "unsupported_channel");
}

#[test]
fn producer_reports_missing_and_unwritable_result_paths_without_publishing() {
    use std::os::unix::fs::PermissionsExt;
    let f = Fixture::new();
    let mut queue = queue::load_queue(&f.root).unwrap();
    queue["waits"]["w"] = json!({"kind":"signal","closed":false,
        "producer":{"adapter":"codex","session":"sesfixture"}});
    queue::save(&f.root, &mut queue).unwrap();
    let mut spec = f.spec.clone();
    spec["role"] = json!("producer");
    spec["wait"] = json!("w");
    let results = f.root.join("results");
    let saved = f.root.join("saved-results");
    std::fs::rename(&results, &saved).unwrap();
    let missing = f.run(&spec);
    std::fs::rename(&saved, &results).unwrap();
    assert_eq!(channel(&missing), "missing_configuration");
    std::fs::set_permissions(&results, std::fs::Permissions::from_mode(0o500)).unwrap();
    let denied = f.run(&spec);
    std::fs::set_permissions(&results, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert_eq!(channel(&denied), "policy_denied");
    assert_eq!(std::fs::read_dir(&results).unwrap().count(), 0);
}

#[test]
fn queue_readiness_without_receiver_records_inactive_report() {
    let f = Fixture::new();
    let output = Command::new(env!("CARGO_BIN_EXE_herdr-flow"))
        .args(["queue", "readiness", "--state-dir"])
        .arg(&f.root)
        .env_remove("CODEX_THREAD_ID")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    let report: J = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["active_wakeup"], false);
    assert_eq!(
        util::read_json(&f.root.join("readiness.json")).unwrap(),
        report
    );
}
