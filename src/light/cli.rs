//! Record-only command entry point. Local queries never contact Herdr.
use super::{cli_args, cli_output, cli_run};

pub const USAGE: &str = r#"Usage: herdr-flow light [storage options] <command> [options]

Records only; these commands do not control agents or legacy tasks.
  init
  project register --root DIR --label TEXT
  project list [--after-id ID] [--limit 1..100]
  task add --input FILE|-
  task report --input FILE|-
  task list --project ID [--batch TEXT] [--after-id ID] [--limit 1..100]
  task show --project ID --task ID [--after-id EVENT_ID] [--limit 1..100]
  binding attach --input FILE|-
  binding show --project ID --task ID [--after-id BINDING_ID] [--limit 1..100]
  backup --output-root DIR

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
No Herdr connection is required. See docs/light-cli.md for input examples.
"#;

pub fn cli(args: &[String]) -> i32 {
    if args.is_empty() || args.iter().any(|a| a == "--help" || a == "-h") {
        print!("{USAGE}");
        return 0;
    }
    match cli_args::parse(args).and_then(cli_run::execute) {
        Ok(value) => cli_output::success(value),
        Err(error) => cli_output::failure(error),
    }
}
