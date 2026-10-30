//! Records and opt-in observation. Local record/cache queries never contact Herdr.
use super::{cli_args, cli_output, cli_run};

pub const USAGE: &str = r#"Usage: herdr-flow light [storage options] <command> [options]

Records and optional observations; these commands do not control agents or legacy tasks.
  init
  tui [--project ID] [--batch TEXT] [--limit 1..100] (interactive terminals only)
  project register --root DIR --label TEXT
  project list [--after-id ID] [--limit 1..100]
  task add --input FILE|-
  task report --input FILE|-
  task list --project ID [--batch TEXT] [--after-id ID] [--limit 1..100]
  task show --project ID --task ID [--after-id EVENT_ID] [--limit 1..100]
  binding attach --input FILE|-
  binding show --project ID --task ID [--after-id BINDING_ID] [--limit 1..100]
  backup --output-root DIR
  observe --once|--watch|--show --project ID --task ID --binding ID
    --socket ABSOLUTE_PATH --service REGISTERED_SERVICE (not required for --show)
    [--interval-ms 100..60000] [--timeout-ms 50..5000] [--stale-ms 100..3600000]
    [--samples N] (watch only; omit for foreground until stopped)
    [--bark-key-file PRIVATE_FILE] [--bark-endpoint https://HOST/push]
Observation uses the current public Herdr JSON API; versions are diagnostic only.
--show reads cached observations/notification attempts offline. No task reports are changed.

Storage options (may appear anywhere):
  --data-root DIR       default: ${XDG_STATE_HOME:-$HOME/.local/state}/herdr-light
  --forbid DIR          explicit forbidden directory; repeatable, checked both ways
  --busy-timeout-ms N   0..5000, default 250; Busy is reported, never retried automatically

Writes require a JSON Request via --input (maximum 65536 bytes).
The Request contains request_id, reporter, summary, refs and change.
change.kind must match the command: create_task / report / attach_binding.
Updates require task_id and expected_revision; task creation uses project_id.
Unknown JSON fields and extra/duplicate flags are rejected.
Run init explicitly before other commands. Queries do not initialize missing stores.
Success: stdout JSON {"ok":true,"data":...}; errors: stderr JSON, empty stdout.
Exit codes: 0 success/help, 2 input/usage, 3 missing task/project scope,
            4 request/revision conflict, 5 busy/locked, 6 storage/schema failure.
Pages expose items and next_after_id; a full final page may require an empty next page.
Unknown project list filters and missing-task binding pages return []; task show
requires an existing task. Existing tasks must match the requested project. Re-registering a project returns its original id and label.
Record commands require no Herdr connection. See docs/light-cli.md for input examples.
"#;

pub fn cli(args: &[String]) -> i32 {
    if args.is_empty() || args.iter().any(|a| a == "--help" || a == "-h") {
        print!("{USAGE}");
        return 0;
    }
    let inv = match cli_args::parse(args) {
        Ok(inv) => inv,
        Err(error) => return cli_output::failure(error),
    };
    if inv.command == cli_args::Command::Tui {
        return match super::tui::run(inv) {
            Ok(()) => 0,
            Err(error) => cli_output::failure(error),
        };
    }
    match cli_run::execute(inv) {
        Ok(value) => cli_output::success(value),
        Err(error) => cli_output::failure(error),
    }
}
