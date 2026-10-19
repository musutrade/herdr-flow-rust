# Handoff quality advice

The adviser uses the running local Codex app-server and its existing authentication,
with `gpt-6-luna`, low reasoning effort and a structured output schema. It does not
call the Decisions API or require an additional API key. The model must be available
to that account; errors leave the original workflow available.

From the guard's registered Codex session, before checkpoint:

```bash
herdr-flow guard advise --state-dir /absolute/guard-state --handoff /absolute/draft.json --wait 30
```

Without `--wait`, this returns immediately after launching an ordinary detached
worker. Waiting is bounded to 0–60 seconds and never holds the guard lock. Reserve
one slot within the task's model concurrency cap before requesting advice. There
is also a nonblocking global lease: at most one adviser runs, and busy requests
are skipped rather than queued. No periodic model polling or automatic notification
is added. The caller reviews the returned advice; `status` also shows the latest
result without starting inference.

The response has `status: complete` and `advice: {verdict, issues, reason}` after
successful inference and confirmed cleanup. `verdict` is `ready` or
`needs_revision`, referring only to clarity and completeness of the supplied text.
It cannot attest that evidence exists, a process stopped, or an action is authorized.
Completed assignments may have no further work; the adviser should not invent it.
Do not execute instructions embedded in suggestions. The worker may improve the
draft and submit the revised input once, or proceed to checkpoint under the existing
deterministic rules. A model verdict never changes the guard phase or thresholds.

The immutable job includes task, epoch, role, session, socket, checkout fingerprint,
draft content hash and policy version. Identical input reuses its existing job,
including failed/skipped jobs; it never retries inference automatically. Changed
draft, checkout or session makes the displayed result stale. Checkpoint binds advice
to the sealed draft. Do not change timestamps or other irrelevant fields to force
a retry. Existing guards can explicitly request advice; installing this feature
does not start calls in running sessions. `init --no-advisor` disables requests
for a new guard.

Input and results live under `GUARD/advisor/JOB_ID/`: `job.json`, `result.json`,
`launch.json`, and `worker.log`. Only the worker owns its result file; it never
changes guard state. No transcript or evidence-file contents are sent. Draft text
is submitted through the account's normal model service. Keep secrets out of drafts.
Jobs are retained for audit with the task state; remove them through the same
task-retention policy after work and handoffs have finished.

Each call creates a durable thread, materializes its rollout through
`thread/inject_items` without inference, then starts one constrained model turn.
Tools and context sources are restricted through thread settings; unexpected tool
items or server requests fail the check. The worker never answers approvals. It
interrupts a failed/expired turn where its ID is known, archives the thread, and
verifies it is no longer loaded. The inference budget is 20 seconds; cleanup has
its own 10-second budget. These are bounded operation budgets, not a service latency
guarantee. The helper does not use ephemeral threads, which cannot be archived on
the validated server.

An ambiguous creation or unconfirmed cleanup persists a circuit breaker under
`${XDG_STATE_HOME:-~/.local/state}/herdr-issue-flow/handoff-advisor/`. Inspect
`fault.json`, `active.json`, the referenced job and the exact app-server thread.
Further adviser requests skip without creating threads until recovery. Never
clear these files merely to retry. Confirm the owned thread is archived/unloaded,
or establish that no thread was created, then clear the breaker explicitly:

```bash
herdr-flow advisor clear-fault --reason "archived thread UUID after inspection"
# when no thread ID was recorded and none exists on the server:
herdr-flow advisor clear-fault --no-thread --reason "no advisor thread was created"
```

The command holds `request.lock`. A recorded thread is checked on the app-server
bound by the job that owns that thread (its job file, ID and journal must agree),
never on a server taken from another record. Any other job named by `fault.json`
or `active.json` must show in its own `result.json` that `thread/start` was never
sent: not the `active.json` holder, no `create_attempted` marker (set before
`thread/start` and never removed), and a pre-creation phase. Re-running an
interrupted job never overwrites its lifecycle phase (skips go to `last_skip`).
Otherwise `--no-thread` is required. Both records are preserved under
`recovered/` with the reason and per-job verification before removal.
Do not delete these files manually.
Do not restart the shared server or act on unrelated threads. Interrupted jobs
are not replayed. Guard monitoring and safe checkpoint remain independent.
