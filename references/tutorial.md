# Minimal startup, result, and recovery tutorial

This tutorial builds one coordinator receiver, one queue and one unique task
with two accepted steps. It exercises the current V1 path: a result actively
arrives, the receiver ACKs it, the result is applied, and the next step is
taken over. It does not create a PR, merge, or claim trusted App checks. The
same shape is used by the retained V1 live acceptance; see
[V1 acceptance](v1-acceptance.md) for that evidence.

## 0. Prerequisites and boundaries

- Rust 1.80+ and Cargo, POSIX `flock`, and a Herdr-managed session.
- A running local Codex app-server and the matching native TUI bridge, as
  described in [context rotation](context-rotation.md#register-and-run).
- The exact pane, Codex session UUID, transcript path and task ID. Do not use
  the newest transcript or an old pane as a substitute.
- State, result files, command logs and evidence live outside the checkout.
- Existing authorization is recorded in the task handoff; this tutorial grants
  no push, merge, approval or infrastructure permission.

Placeholders below are intentionally obvious. Replace every `REPLACE_...` value
with the value checked for the current task. Commands marked as receiver
commands must run from the exact registered coordinator session
(`CODEX_THREAD_ID`).

## 1. Resolve configuration once

Read the user defaults and optional repository override as described in
[team configuration](team-config.md). The standalone binary does not load the
configuration automatically. Resolve the coordinator thresholds and pass them
explicitly. The fallback values below are provisional, not measured claims.

```bash
export GUARD_DIR=/absolute/state/coordinator-guard
export QUEUE_DIR=/absolute/state/queue
export PANE=REPLACE_PANE
export SESSION=REPLACE_CODEX_SESSION_UUID
export TRANSCRIPT=/absolute/transrc/REPLACE_ROLLOUT.jsonl
export TASK=owner/repo#REPLACE_ISSUE
```

## 2. Register the coordinator receiver

```bash
herdr-flow guard init \
  --state-dir "$GUARD_DIR" \
  --pane "$PANE" \
  --session "$SESSION" \
  --transcript "$TRANSCRIPT" \
  --task "$TASK" \
  --role coordinator \
  --warn-tokens 80000 --rotate-tokens 120000 \
  --warn-ratio 0.60 --rotate-ratio 0.75
```

Then start the watcher. Use observation mode when notifications only need a
receiver. Use automatic rotation only when that capability is actually wanted
and authorized; `enable` also verifies/starts the existing watcher.

```bash
# Observation-only receiver (notifications, no automatic rotation)
herdr-flow guard watch --state-dir "$GUARD_DIR" --background

# Or, when automatic rotation is required and authorized
herdr-flow guard enable --state-dir "$GUARD_DIR"
```

Inspect once and record the result:

```bash
herdr-flow guard status --state-dir "$GUARD_DIR"
```

Pass conditions:

- `phase` is `monitoring`; `watcher.healthy` is true.
- `diagnosis.classification` is `monitoring`; `telemetry_unknown` is reported
  separately and cannot support automatic rotation.
- No `external_delivery`, pending `rotation_due` or handoff phase is active.
- Observation mode has `auto_rotate=false`. Only `auto_rotate=true` plus a
  healthy watcher and known telemetry supports automatic rotation.

`herdr-flow guard status` is JSON already; it has no `--json` flag. Do not use
`/status`, a model menu or a terminal label as identity evidence. Missing
enable, an unbridged/embedded session, a stale pane or an old guard must be
reported as a gap, not as "protected".

## 3. Start the queue and verify readiness

```bash
herdr-flow queue init \
  --state-dir "$QUEUE_DIR" \
  --guard-state-dir "$GUARD_DIR" \
  --poll-seconds 30

herdr-flow queue enable --state-dir "$QUEUE_DIR"
herdr-flow queue watch --state-dir "$QUEUE_DIR" --background
herdr-flow queue readiness --state-dir "$QUEUE_DIR"
```

Run `readiness` from the exact receiver session. It writes `readiness.json` and
exits 1 when active waking is unavailable. Check:

- `active_wakeup=true`; `notify=true`; the queue is not stopped.
- The receiver session, queue watcher and guard watcher are the intended ones.
- `producer_channels` contains the exact result waits and owners.
- `next_steps` is empty when ready.

`readiness` proves the notification transport, not that a result has been
applied. A queue with notifications disabled is only a status board. Use
`herdr-flow queue status --state-dir "$QUEUE_DIR" --json` for the
current task/delivery diagnosis; it is read-only.

## 4. Register one result wait and task before dispatch

`step1-wait.json`:

```json
{
  "id": "step1",
  "task": "owner/repo#123",
  "epoch": 1,
  "assignment": "step1-a1",
  "candidate": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
  "producer_session": "00000000-0000-0000-0000-000000000000",
  "owner": "producer",
  "kind": "signal",
  "timeout_seconds": 1800,
  "reason": "Wait for the first accepted step",
  "condition": "Result and evidence published"
}
```

The repeated `a` SHA, zero UUID and paths above are placeholders; replace them
with actual values. `candidate` must be the full 40-character SHA. Keep
`producer_session` for a worker so a different session cannot signal. Remove it
only when the producer is a local command/file path with no session identity.

`task.json`:

```json
{
  "task": "owner/repo#123",
  "wait": "step1",
  "goal": "Complete two accepted steps",
  "apply_timeout_seconds": 600,
  "acceptance": [
    {"id": "first", "description": "First step checked", "required": true},
    {"id": "second", "description": "Second step checked", "required": true}
  ],
  "authorization": [
    {"id": "develop", "action": "development", "source": "Existing user instruction and scope"}
  ]
}
```

For a writing assignment, also include `workspace` and an existing
`execution_evidence` file. The queue records the initial assignment before work
starts.

```bash
herdr-flow queue add --state-dir "$QUEUE_DIR" --spec /absolute/step1-wait.json
herdr-flow queue task-add --state-dir "$QUEUE_DIR" --spec /absolute/task.json
```

Now dispatch step one within the existing authorization. For a managed command,
use a JSON argv file and run the wrapper from the pinned producer session:

```bash
herdr-flow queue run \
  --state-dir "$QUEUE_DIR" --wait step1 \
  --command-file /absolute/step1.argv.json \
  --cwd /absolute/worktree
```

For a worker, send the assignment through the normal transport and give the
worker the exact signal command. Do not treat a transport response or terminal
`idle` as completion.

## 5. Receive, ACK, apply, and continue

The queue delivers the result to the exact receiver. The notification includes
a delivery ID. ACK only confirms receipt:

```bash
herdr-flow queue ack --state-dir "$QUEUE_DIR" --delivery REPLACE_DELIVERY_ID
```

Status now shows `result_pending_application` (or an overdue variant), with the
event and apply deadline. Inspect the evidence and the current `state_version`,
then apply:

First register the `step2` wait with the same task/epoch, assignment `step2-a1`
and the exact producer session. Then use the current `state_version` from
`status --json` in `step1-apply.json`; the number below is illustrative.

```json
{
  "task": "owner/repo#123",
  "state_version": 2,
  "event": "REPLACE_ACTUAL_EVENT_ID",
  "release_evidence": "/absolute/state/queue/commands/step1/result.json",
  "acceptance": {"first": "/absolute/state/queue/commands/step1/result.json"},
  "next_action": {
    "kind": "dispatch",
    "operation": "step2-op1",
    "assignment": "step2-a1",
    "session": "11111111-1111-1111-1111-111111111111",
    "owner": "producer",
    "workspace": "/absolute/worktree",
    "writes": true,
    "authorization": "develop",
    "action": "development",
    "instruction": "Perform the second accepted step and publish its evidence",
    "wait": "step2",
    "accept_timeout_seconds": 600
  }
}
```

Run the application from the receiver session:

```bash
herdr-flow queue task-apply --state-dir "$QUEUE_DIR" --spec /absolute/step1-apply.json
```

The result and concrete successor intent are persisted together. If the
successor is a managed command, persist the send and let the wrapper take over.
`step2-begin.json` is:

```json
{"task": "owner/repo#123", "state_version": 3, "operation": "step2-op1"}
```

Again, use the current version from status rather than the illustrative `3`.

```bash
herdr-flow queue task-begin \
  --state-dir "$QUEUE_DIR" --spec /absolute/step2-begin.json

herdr-flow queue run \
  --state-dir "$QUEUE_DIR" --wait step2 \
  --command-file /absolute/step2.argv.json \
  --cwd /absolute/worktree
```

For a worker, `task-begin` records `sending` before the transport submission;
the exact producer session calls `task-accept` with the operation and assignment.
A returned transport ID alone is not takeover.

After step two's result is ACKed, apply `next_action` with `kind: complete` and
evidence for every required acceptance item. Release the writer with inspected
evidence. `task-apply` refuses completion while required acceptance is missing,
blocking findings are unresolved, or a writer is still owned.

## 6. Recovery and troubleshooting

Use `herdr-flow queue status` / `--json`, `readiness`, `herdr-flow guard status`,
and `event-current` before acting. The diagnosis separates states that can all
look idle in a terminal:

| Situation | What the diagnosis shows | Safe next step |
| --- | --- | --- |
| Command/worker still working | `executing` | Check the registered wait/deadline; do not kill or reassign without evidence. |
| Result received, not applied | `result_pending_application` / `result_unapplied_overdue` | ACK only confirmed receipt; apply with the current event and version. Do not resend. |
| Successor recorded, not sent | `successor_prepared` | Use the existing `task-begin` path, then exact takeover. |
| Send/takeover uncertain | `successor_uncertain` / `successor_takeover_overdue` | Inspect the existing operation and exact session. Accept a matching takeover or record a blocker; never replay. |
| Open registered dependency | `waiting` | Wait for the event/deadline; no model polling. |
| Explicit blocker | `blocked` | Register a fresh matching wait and use `task-resume` with inspected evidence. |
| Paused or cancelled | `paused` / `cancelled` | Preserve state and writer; continue only on a new user instruction. |
| Missing next action | `no_open_next_action` | Inspect queue state and register a concrete wait before dispatch. |
| Delivery uncertain or ACK overdue | notification diagnosis | Inspect the exact receiver/delivery; do not resend. Use `resolve` only after establishing `not-delivered`. |
| Watcher stopped, PID reused, expired heartbeat or lost lock | watcher `next_step` | Inspect the process/lock; restart only the verified watcher. Do not bypass the mutex or kill another task. |
| Receiver busy/blocked by approval | delivery diagnosis | Inspect the active turn/approval; do not auto-approve or kill it. |
| Old or late event | archived event | Do not apply it to a new candidate or assignment. |

ACK-after-crash and apply-after-crash are deliberately recoverable: the result
and its deadline remain in the queue. A missing ACK does not prove the result
was not delivered; an ACK does not prove it was applied.

## 7. Other producers and current boundaries

- **OpenCode:** use a registered `signal` wait and verify its exact session and
  model independently. OpenCode can return results, but automatic rotation is
  not supported. The 1.19.3 adapter adds only pane/shell/process checks; it does
  not add RPC or automatic handoff. See [OpenCode integration](opencode-v2.md).
  Do not apply Codex RPC or reset commands to it.
- **CI:** use `herdr-flow ci` with a pinned config and a `ci` wait; the observer is
  read-only and never merges or reruns CI. See [CI monitoring](ci-monitor.md).
- **Long command:** the existing `run` wrapper reserves the invocation before
  starting the child and records the real exit status; see
  [task progress](task-progress.md#dispatch-takeover-and-ambiguity).

Current V1 evidence covers exact receiver arrival/ACK/application/takeover,
same-PR normal/repair observation, and an external OpenCode review signal. It
does not cover trusted App checks, local gate integration, V2 batching, or
OpenCode automatic rotation. Those remain separate acceptance work; do not
describe them as already verified.

## 8. Evidence hygiene

Keep only a compact index: task/assignment/epoch, candidate SHA, event and
delivery IDs, ACK/application/takeover times, result and command paths, and the
current diagnosis. Keep raw transcripts, private paths and full logs outside
GitHub. A status board or a written result file without an actual delivery path
is not proof of automatic continuation.
