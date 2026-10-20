//! Multi-call command entry point.

use std::process::ExitCode;

fn usage() {
    println!(
        "herdr-flow {}\n\nUsage: herdr-flow <command> [options]\n\nCommands:\n  queue    durable waits, tasks, deliveries and metrics\n  ci       read-only GitHub Actions / Check observer\n  guard    Codex context guard and session rotation\n  tui      native Codex TUI observation bridge\n  advisor  optional handoff text advice\n  opencode native OpenCode sessions and monitoring diagnostics\n  lint-policy  reject Clippy suppressions\n  crap     Rust complexity/CRAP gate",
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
        other => fallback(other),
    }
}

fn fallback(command: &str) -> i32 {
    match command {
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
