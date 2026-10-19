# CI observer

Requires Rust 1.80+ and Cargo, POSIX `flock`, authenticated `gh`, and read access to the
configured Actions/Checks. No third-party services. It observes explicit
inventories; full policy, trusted execution and publication remain separate.

Create a config outside the repository. Derive workflow filenames, events, and
exact required job names from current repository policy. List both push and PR
entries when both matter. Do not copy this example as a complete policy:

```json
{
  "version": 1,
  "task": "owner/repo#123",
  "epoch": 1,
  "host": "github.com",
  "repo": "owner/repo",
  "sha": "0123456789abcdef0123456789abcdef01234567",
  "pr": 123,
  "interval": 60,
  "deadline_seconds": 21600,
  "workflows": [
    {"file": "ci.yml", "event": "pull_request", "jobs": ["Required Quality Aggregate"]},
    {"file": "ci.yml", "event": "push", "jobs": ["Required Quality Aggregate"]}
  ]
}
```

Run once (exit 0 success, 1 failure, 2 error/superseded/timeout, 3 pending):

```bash
herdr-flow ci --config /absolute/config.json --state-dir /absolute/ci-state --once
```

For an inventory of required Actions jobs, set `"decision": "required_jobs"`
on each applicable workflow entry. This requires a nonempty list of unique exact
`jobs` names. Every configured workflow/event must still supply those jobs for
the pinned SHA and current attempt. All matching jobs must complete successfully;
advisory jobs remain in the observation but their failure or continued execution
does not delay this decision. Workflow-level failure alone does not override
successful required jobs in this explicit mode.

Required job `failure` produces a failure event. Missing jobs after workflow
completion, skipped/neutral jobs, cancellations, timeouts and other non-success
conclusions produce `needs_attention` (exit 2), delivered as a decision rather than
a code-repair failure. Inspect the retained job conclusions and `decision_reason`
before choosing recovery. This mode may finish while advisory jobs keep running;
it does not cancel them. The default `"decision": "workflow"` preserves the
existing whole-workflow behavior described below.

`needs_attention` takes precedence over `failure`, both among required jobs in
one workflow and across configured workflow/event entries. A failed required job
combined with a cancelled/skipped job, or a missing job in a completed run, needs
inspection before code repair can be dispatched. Failed jobs remain in the
observation. A wholly missing workflow run stays pending until the observer's
original deadline and then reports `timeout`; it never passes or authorizes repair.

Choose this inventory from current repository policy. Success covers this Actions
inventory only; it does not attest external App checks, approvals, current base
applicability or merge eligibility. This setting neither changes repository rules
nor grants permission to publish or merge.

For external Checks, omit `workflows` and provide exact `pr`, `base_ref`,
`target_base` and `checks: [{name, app_id, external_id}]`. Pin the expected App
and obtain the run's external ID from the trusted service. See the complete
[consumer flow](batching.md#execute-the-gate-and-publish). Mixed inventories
require both Actions and Checks. Missing reports stay pending; neutral/skipped,
wrong identities and changed inputs need inspection. The consumer refuses a
commit with more than 1000 Check suites because GitHub's run endpoint truncates
its suite window. It does not infer completeness from a truncated green result.
This is read-only observation, not trusted execution or a change to merge rules.

When a user service manager is unavailable, detach directly:

```bash
herdr-flow ci --config /absolute/config.json --state-dir /absolute/ci-state --background
```

This waits (up to 15 seconds) until the detached child holds the state lock and
has verified or written `identity.json`, then prints `{"pid", "state_dir", "ready": true}`.
A lock conflict, identity mismatch or invalid configuration exits nonzero and the
child is reaped; no PID is reported. First CI results still arrive via `status.json`.
Startup logs go to `process.log`. It survives terminal closure but has
no automatic crash restart. Resume with the same command and state directory;
the lock rejects concurrent observers. For manual termination verify the PID's
command and state directory before sending SIGTERM; never kill a stale PID blindly.

Run as a supervised user process on Linux (use a unique unit for each epoch):

```bash
systemd-run --user --unit=ci-watch-123-e1 --property=Restart=on-failure --property=RestartSec=30 --property=RestartPreventExitStatus='1 2' "$HOME/.cargo/bin/herdr-flow ci" --config /absolute/config.json --state-dir /absolute/ci-state
systemctl --user status ci-watch-123-e1
journalctl --user -u ci-watch-123-e1 -n 30
systemctl --user stop ci-watch-123-e1
```

Replace the binary path for other installations. User services must have `gh`
in PATH and access to its authentication; do not place tokens in the config.
A service survives agent exit; surviving logout depends on host user-service
configuration. Do not enable system-wide or lingering services without need.

`identity.json` pins the entire config and the original deadline. `watch.lock`
prevents duplicate writers. State files are replaced atomically with 0600
permissions; a symlinked state parent is rejected. `status.json` is replaced
atomically; `events/*.json` contains immutable content-addressed observations.
Identical observations are not rewritten as new events. Read current status first; historical event timestamps
are observations, not workflow completion timestamps. A restart uses the same
identity and deadline, and re-queries GitHub. A new SHA/config needs a new state
directory. Archive/delete finished task directories according to user retention
needs; the watcher never removes evidence.

Early job failures are reported while remaining workflows continue. All listed
workflows must exist and succeed; explicitly required jobs must succeed (skipped
and neutral are not accepted for these jobs). Other skipped/neutral jobs are
left to the workflow's aggregate/policy. Missing runs/jobs never pass. A completed
run with missing required jobs (or no jobs) reports failure immediately; inspect
`missing_jobs` for a stale job-name inventory. Active runs may still acquire jobs.
The observer exits once all configured runs and their listed jobs have completed,
including this failure case, rather than waiting for the overall deadline. The newest
run ID for each workflow/event and its current attempt are observed. Jobs are
paginated for that attempt, then run identity is rechecked. Network/auth/parse
errors become `api_error` with bounded backoff, never a green result. The deadline
persists across restarts. Each HTTP subprocess has a 45-second timeout; a poll
can span multiple requests and deadline enforcement happens between polls.

After successful completion the watcher exits; later reruns require starting it
again. Re-query before any merge. It does not download artifacts or logs, verify
artifact contents, or authorize publication.

## Event delivery boundary

The observer provides a durable event outbox. For automatic coordinator notification,
connect its state directory to a CI wait in [the wait queue](waiting.md), pinning
`identity.json`'s configuration hash and matching task/epoch. The queue batches
semantic outcomes, binds the current coordinator session, and requires ACK. The
observer alone does not wake agents. Neither component claims exactly-once terminal
delivery or grants merge authority.

API references: [workflow runs](https://docs.github.com/en/rest/actions/workflow-runs),
[attempt jobs](https://docs.github.com/en/rest/actions/workflow-jobs), and
[Check runs and suite-window limits](https://docs.github.com/en/rest/checks/runs#list-check-runs-for-a-git-reference).
