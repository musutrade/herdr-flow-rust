use std::process::Command;

fn run(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_herdr-flow"))
        .args(args)
        .output()
        .expect("run herdr-flow")
}

#[test]
fn entrypoint_covers_help_version_and_all_dispatches() {
    assert!(run(&[]).status.success());
    assert!(run(&["--help"]).status.success());
    assert!(run(&["--version"]).status.success());
    for command in [
        "queue",
        "ci",
        "guard",
        "tui",
        "advisor",
        "crap",
        "lint-policy",
    ] {
        let output = run(&[command, "--help"]);
        assert!(
            output.status.success(),
            "{command} --help failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let unknown = run(&["definitely-not-a-command"]);
    assert_eq!(unknown.status.code(), Some(2));
}
