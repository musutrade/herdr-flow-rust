//! Multi-call command entry point.

use std::process::ExitCode;

fn usage() {
    println!(
        "herdr-flow {}\n\nUsage: herdr-flow <command> [options]\n\nCommands:\n  queue    durable waits, tasks, deliveries and metrics\n  ci       read-only GitHub Actions / Check observer\n  guard    Codex context guard and session rotation\n  tui      native Codex TUI observation bridge\n  advisor  optional handoff text advice\n  opencode native OpenCode sessions and monitoring diagnostics\n  task-env preparatory owned environment create/status/recover\n  task-runtime task-owned native sessions and collection\n  delivery confirmed customer delivery and final destruction\n  lint-policy  reject Clippy suppressions\n  crap     Rust complexity/CRAP gate",
        env!("CARGO_PKG_VERSION")
    );
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(command) = args.first() else {
        usage();
        return ExitCode::SUCCESS;
    };
    let code = dispatch(command, &args[1..]);
    ExitCode::from(code.clamp(0, 255) as u8)
}

fn dispatch(command: &str, rest: &[String]) -> i32 {
    match command {
        "queue" | "wait-queue" => herdr_flow::queue::cli::cli(rest),
        "ci" | "ci-watch" => herdr_flow::ci::watch::cli(rest),
        "guard" | "context-guard" => herdr_flow::guard::cli(rest),
        "tui" | "codex-tui" => herdr_flow::codex::tui::cli(rest),
        "advisor" | "handoff-advisor" => herdr_flow::codex::advisor::cli(rest),
        "opencode" => herdr_flow::opencode::cli(rest),
        "crap" | "check-crap" => herdr_flow::crap::cli(rest),
        "lint-policy" => herdr_flow::lint_policy::cli(rest),
        other => fallback(other, rest),
    }
}

fn fallback(command: &str, rest: &[String]) -> i32 {
    match command {
        "delivery" | "task-runtime-broker" | "task-runtime" | "task-env" => {
            environment_command(command, rest)
        }
        "-h" | "--help" | "help" => {
            usage();
            0
        }
        "--version" | "-V" => {
            println!("herdr-flow {}", env!("CARGO_PKG_VERSION"));
            0
        }
        other => {
            eprintln!("herdr-flow: unknown command: {other}");
            usage();
            2
        }
    }
}

fn environment_command(command: &str, rest: &[String]) -> i32 {
    match command {
        "delivery" => herdr_flow::delivery::cli::cli(rest),
        "task-runtime-broker" => broker(rest),
        "task-runtime" => herdr_flow::task_runtime::cli::cli(rest),
        _ => herdr_flow::task_env::cli::cli(rest),
    }
}

fn broker(args: &[String]) -> i32 {
    if args.len() != 2 || args[0] != "--state-dir" {
        return 2;
    }
    match herdr_flow::task_runtime::broker::serve(std::path::Path::new(&args[1])) {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("{error}");
            2
        }
    }
}

#[cfg(test)]
mod broker_tests {
    use super::*;
    #[test]
    fn internal_broker_requires_its_owned_runtime() {
        assert_eq!(broker(&[]), 2);
        assert_eq!(broker(&["--wrong".into(), "x".into()]), 2);
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            broker(&["--state-dir".into(), dir.path().display().to_string()]),
            2
        );
    }
}

#[cfg(test)]
mod entry_tests {
    #[test]
    fn preparatory_entry_and_existing_help_routes() {
        for command in ["delivery", "task-runtime", "task-env"] {
            assert_eq!(super::fallback(command, &["--help".into()]), 0);
        }
        for command in ["help", "--help", "--version"] {
            assert_eq!(super::fallback(command, &[]), 0);
        }
        assert_eq!(super::fallback("unknown", &[]), 2);
        assert_eq!(super::fallback("task-runtime-broker", &[]), 2);
    }
}
