use std::process::Command;

#[test]
fn real_pty_browser_navigation_errors_terminals_and_readonly_boundary() {
    let output = Command::new("python3")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/common/light_cli/tui_pty.py"
        ))
        .arg(env!("CARGO_BIN_EXE_herdr-flow"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
