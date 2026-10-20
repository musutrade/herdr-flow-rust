# Context monitoring and safe rotation

Use `herdr-flow guard` for registered local Codex agents in Herdr. The
monitor is a local supervisor process, not another model. Version 2 uses Codex
app-server JSON-RPC over a Unix WebSocket for thread identity, runtime settings
and turn submission. Herdr owns panes and frontend process lifecycle. It does not install hooks
or change global Codex permissions. Claude/other agents and remote Herdr machines
are not supported by this rotation adapter. OpenCode 1.19.3 has a separate
native HTTP session adapter and watcher with no automatic rotation; see
[OpenCode integration](opencode-v2.md).

## What is measured

The supported local Codex JSONL rollout has `session_meta` plus `event_msg` /
`token_count` records. Use `info.last_token_usage.total_tokens` as the most recent
request's context estimate and `info.model_context_window` as its capacity. Never
use cumulative `total_token_usage` as current context. Cached input still occupies
context. This is a sampled estimate, not a live tokenizer: a long ongoing turn can
exceed the threshold before the next record. Missing/unsupported telemetry is
`unknown`, not zero. Compaction does not cancel an already latched rotation request.

## Transcript scan contract

The adapter reads the local Codex JSONL rollout as a stream. It supports
`session_meta` records and `event_msg`/`token_count` plus
`response_item`/`message` user messages; unrelated record types are ignored. A
complete line that is not valid JSON, or a file without usable
`session_meta.id`/`cwd`, is an explicit `GuardError`; it is never treated as
zero usage. A trailing partial line is not consumed: the scan stops before it,
returns `caught_up: false` and `pending_bytes`, and retries it on the next tick.

The persisted cursor is reusable only when the device/inode matches, the file
has not shrunk below the stored offset, and a bounded head prefix
(`head`/`head_len`) is unchanged. A same-inode truncate/rewrite normally changes
that prefix and forces a full rescan. Known limitation: a same-inode rewrite
that preserves the exact head prefix but changes bytes after it is not detected
by this lightweight check; the cursor is a performance hint, not content proof.
`compacted` records clear usage to `unknown` but do not reset the offset or an
already latched rotation request.

`info.last_token_usage.total_tokens` is the current context estimate;
`total_token_usage` is cumulative and is never used for thresholds.

Measured on 2026-10-19 (Linux, local SSD, one process, `tracemalloc`):

| Input | Scan | Elapsed | Peak traced | Caught up |
| --- | --- | --- | --- | --- |
| 16 MiB controlled, ~95k records | first full | 2.17 s | 138 KiB | yes |
| same file, one appended record | incremental | 0.47 ms | 135 KiB | yes |
| same file, 10,000 appended records | incremental backlog | 157 ms | 136 KiB | yes |
| 4 MiB single JSONL record | full | 18.6 ms | 12.4 MiB | yes |
| real 65 MiB Codex rollout | full | 1.04 s | 1.3 MiB | yes |

These samples show no blocking scan or total-file buffering, so the adapter does
not impose a line/byte budget. If a future budget is introduced, it must return
`caught_up: false` with the pending amount, and callers must treat telemetry as
stale and not dispatch on it until the scan catches up.

Defaults: warn at 80,000 tokens OR 60%; request rotation at 120,000 tokens OR 75%.
These are provisional fallback limits, not measured recommendations or model-specific
claims. Calibrate them using the procedure below; do not assume an absolute limit
is appropriate for every context window. The monitor only requests handoff when the agent
is idle/done; it does not interrupt an active turn, dismiss an approval, or kill
commands. At milestones, the worker also reads guard status and ends its turn for
handoff when rotation is due. This is cooperative prevention, not a hard token cap.

`warn` records a status; it does not send a checkpoint request. Coordinators must
inspect their guard after consuming result batches and before starting another
development/review cycle. On warning, prepare the handoff and use the first safe
checkpoint, rather than waiting for the higher automatic rotation threshold or a
user reminder. Record that decision in the task handoff. Pending queue deliveries
must be acknowledged and command/write ownership settled before checkpointing.

## Calibrate from observed sessions

Use actual observations before choosing replacement thresholds. The guard currently
applies configured limits; it does not learn or tune them automatically. Keep the
fallback values until comparable evidence supports a change, and label each task's
limits as provisional or calibrated in its handoff.

Keep a calibration record outside the checkout, with evidence paths and timestamps:

- Actual model, CLI version, reported context window, role/task type, session UUID,
  and the four effective thresholds. Keep different models/windows and materially
  different workloads separate.
- Recent context estimates at milestones, warning/request time, and the last sample
  before and first sample after each compaction. Record whether compaction was
  automatic, manual, or unknown; a `compacted` record alone does not establish its
  cause. A pre-compaction sample is an observation, not the exact compaction limit.
- Request, safe checkpoint, and successor acknowledgement times; context growth
  from request to checkpoint; handoff size; and time until useful work resumes.
  Separate waiting for an active command from time spent preparing the handoff.
- Interrupted or repeated work, lost instructions, missing evidence, and recovery
  effort. Record successful handoffs as well as failures. Do not repeat passed
  tests merely to collect a calibration sample.

Choose a rotation threshold below the observed automatic-compaction region, leaving
room for measured context growth to a safe checkpoint, handoff preparation, and
sampling uncertainty. Place the warning early enough to prepare that checkpoint.
Use a conservative range across comparable sessions, not a single observed maximum
or an invented universal percentage. If automatic compaction has not been observed,
mark that boundary unknown; do not force compaction or delay necessary handoffs to
find it. Missing telemetry cannot justify raising thresholds.

Evaluate both token and ratio limits together: the first reached wins. For a reported
window W, the effective rotation point is min(rotate_tokens, rotate_ratio * W),
with the analogous expression for warning. Avoid leaving an old absolute limit
that unintentionally defeats a calibrated ratio on a larger window.

If compaction or context-related interruption occurs before a safe handoff, increase
the reserve or lower the thresholds. If handoffs repeatedly happen with ample reserve
and their recovery cost is substantial, consider a measured increase. Change one
policy at a time, record the evidence and rationale, and compare subsequent sessions
for compaction, interruption, and handoff cost. Revert if outcomes worsen. A model,
window, or CLI change requires reassessment rather than inheriting a calibrated label.

Apply revised values through the existing team configuration for new registrations;
do not rewrite a running guard's state or cancel a latched request. Record the old
and new values and configuration source. Safe-checkpoint requirements remain in force
regardless of calibration results.

## Register and run

Create one state directory outside the checkout per role/task. "Outside" means
outside both the registered cwd and its git worktree root; `init` rejects either. Check `HERDR_ENV=1`
and the original Herdr skill first. Resolve the running service and its matching
native executable with `codex app-server daemon version`; no automatic service
startup or restart is performed. Use its `socketPath` and `managedCodexPath`.

For a new worker, launch the native TUI through the protocol bridge in an available,
verified idle Herdr shell. Pass the following command as one quoted argument to
`herdr pane run PANE COMMAND`:

```bash
herdr-flow tui --state-dir /absolute/guard-state --socket /absolute/app-server.sock --binary /absolute/matching/codex --cwd /absolute/checkout --nonce UNIQUE_LAUNCH_ID
```

The bridge forwards native WebSocket bytes unchanged, including approvals and
notifications. `tui.json` records a matched interactive thread response, launch nonce,
PID/start time and runtime. It excludes background title threads and transcript
content. Wait for `status: ready`; do not infer readiness from a spawned PID or a
screen. Apply the resolved role model/effort through `thread/settings/update` before
sending the assignment; see [team configuration](team-config.md). After the first
turn materializes its rollout, register the exact marker UUID and matching transcript:

```bash
herdr-flow guard init --state-dir /absolute/guard-state --pane w7:p2 --session UUID --transcript /absolute/rollout-UUID.jsonl --task owner/repo#123 --role dev --epoch 1
herdr-flow guard enable --state-dir /absolute/guard-state
herdr-flow guard status --state-dir /absolute/guard-state
```

`status` is read-only and includes a `diagnosis` object for watcher health,
pending delivery, rotation requests, in-flight mutations and unknown telemetry.
It does not reconcile, replay or grant approval.

For a newly dispatched worker with native Auto-review enabled, materialize the
blank rollout without running a task and use `herdr-flow guard init --auto-review` as described in
[Auto-review setup](auto-review.md). This establishes verified permissions before
the first assignment. The plain commands above preserve current permissions.

`init` requires the protocol bridge in that same state directory, a matching native
process in the pane, and the exact app-server thread. Resolve transcripts under
`$CODEX_HOME/sessions` (default `~/.codex/sessions`), never the newest file globally.
A pane registry prevents two guards owning one pane. `enable` starts or verifies the
watcher before enabling automatic requests and rotation; it fails if readiness is
unconfirmed. `watch --background` is idempotent for a healthy existing watcher and
returns only after the child has acquired its lock and initialized. Health combines
PID/start time, lock ownership and heartbeat. `watch --once` is diagnostic and may
perform enabled transitions.

Keep the pane exclusive to its registered task while enabled. The bridge binds the
managed interactive thread; it is not a screen-focus API. Do not navigate cached
side-agent views or type concurrent commands during automated handoff. There is no
transaction across simultaneous human keystrokes. Newly started/resumed/forked
interactive threads invalidate the prior binding.

`disable` pauses new actions without undoing a submitted mutation. `release` removes
the registration at a settled phase and ends its watcher. A `needs_attention` guard
keeps its watcher alive but sends nothing; inspected `reconcile` and `enable` ensure
a watcher exists. Detached watchers do not survive reboot or restart themselves
after a process crash. Restart with the same `watch --background` command. Ambiguous
submissions are never replayed. Inspect `state.json`, `tui.json`, and `watch.log`.

### Existing sessions and version 1 state

Version 1 guards cannot run through version 2 implicitly. Disable automation, resolve
pending deliveries and reach a quiescent `monitoring` or `parked` phase. Stop only the
identified old watcher, verifying its PID/start time. Preserve its state and history.
For a native TUI not already bridged, verify its idle thread through app-server,
exit the frontend using `/quit`, confirm the same pane has returned to its original
shell using `herdr pane process-info`, then launch the bridge above with
`--thread EXISTING_UUID`. This reattaches the existing conversation without creating
a handoff or changing server/global configuration. Never relaunch an active writer.

Once the bridge confirms that exact UUID, run:

```bash
herdr-flow guard migrate --state-dir /absolute/guard-state
herdr-flow guard enable --state-dir /absolute/guard-state
```

`migrate` archives the previous state, requires the old watcher lock to be free, and
binds the protocol/process identity. Unsettled or unrelated guards require their
own inspected recovery; installing the skill does not migrate running tasks.

## Worker checkpoint

At a safe milestone, finish or safely stop local commands, transfer independent
observers to a named owner, release the checkout writer, and write a concise JSON
handoff using [the handoff fields](handoff.md). Add:

```json
{
  "quiescent": true,
  "running_commands": [],
  "writer": null,
  "processes": []
}
```

A still-running independent observer must instead have `independent: true`, an
`owner`, and an `identity` (service/run details). This is an explicit worker
attestation, not proof from `idle` or an exhaustive OS process scan. If any command
or other checkout writer is unaccounted for, do not checkpoint. Coordinate a shared
checkout freeze before sealing; changes invalidate the seal. Preserve authorization,
completed evidence, unresolved blockers, exact next action and original untracked
files. Limit the handoff to 16 KB; put logs/history in referenced files.

Before sealing, run the optional [handoff quality adviser](handoff-advice.md) from
this registered session, with one model slot available:

```bash
herdr-flow guard advise --state-dir /absolute/guard-state --handoff /absolute/handoff-input.json --wait 30
```

Consider current advice and revise the draft if useful. A failed, busy, stale or
timed-out adviser leaves the original checkpoint protocol available. Do not wait
indefinitely or treat `ready` as proof of a safe checkout. New guards allow this
explicit draft check by default; `init --no-advisor` disables it. Neither the
watcher nor checkpoint launches model calls implicitly.

```bash
herdr-flow guard checkpoint --state-dir /absolute/guard-state --handoff /absolute/handoff-input.json
```

The command takes the calling session from `CODEX_THREAD_ID`, validates the task and
epoch, seals the handoff, and binds branch/HEAD/index/worktree/untracked bytes. After
success, end the turn and remain stopped. Do not continue task work. The guard waits
for app-server idle, rejects new input or changed checkout, and snapshots the complete
runtime. It persists each mutation intent before acting:

1. Exit the old native frontend with `/quit`; await its original empty shell.
2. Launch a fresh native TUI through the bridge; await its matched thread UUID.
3. Restore and verify model, reasoning, collaboration mode, multi-agent policy,
   approval policy/reviewer, permission profile/sandbox, service tier and disabled
   plugins. Cwd/provider/workspace-root drift requires inspection.
4. Revalidate the handoff and checkout, then submit one `turn/start` containing the
   handoff location and ACK instructions.

The old rollout remains available. No `/new`, `/status`, model picker, screen label
or English UI recognition participates in control. Runtime updates are thread-local;
the global Codex configuration remains unchanged. Approval requests pass to the
native client and are never automatically answered by the adapter.

A blank thread cannot yet be `thread/resume`d on the validated server. Until its
first turn creates a durable rollout, only the matching bridge's native start/settings
responses may provide the runtime snapshot. Other RPC errors fail closed. The guard
never sends an extra bootstrap model turn just to create a transcript.

## Successor acknowledgement

Before any task mutation, the new session reads the handoff and applicable repository
instructions, then runs:

```bash
herdr-flow guard ack --state-dir /absolute/guard-state
```

Checkpoint and ACK commands must run inside the identified Codex session;
`--session` cannot override a different `CODEX_THREAD_ID`. The new UUID must match; the checkout and sealed handoff must still match. On success
the old registration is archived, the guard epoch increments, the new transcript is monitored,
and the successor continues `next_action` using existing authorization. Report the
new guard epoch to the coordinator. This is the session-generation counter in the
guard state and sealed rotation handoff, distinct from the task epoch in
`current.json`, CI configuration and queue waits. Rotation alone does not change
the task epoch or invalidate its waits. Repeated or old-session ACKs are rejected. Do not load
the previous conversation or repeat tests solely because of rotation.

A lock-busy error means a monitor poll is still in progress; retry the checkpoint or
ACK after that poll, without doing task writes in between. A request or ACK taking
more than ten minutes enters `needs_attention`; inspect rather than resend.

## Recovery and limits

Every external mutation has a persisted `*_sending` state before it is attempted.
A crash or timeout can leave delivery ambiguous. Automatic progress stops and records
`phase_before_error`; neither turn submission, settings update nor frontend launch
is automatically replayed. Inspect the exact thread, process, marker and transcript.
Restore the sealed runtime through the API if necessary. After inspecting the live
UUID, use:

```bash
herdr-flow guard reconcile --state-dir /absolute/guard-state --session INSPECTED_UUID
```

This archives the error and disables automation. A confirmed original thread returns
to monitoring; a confirmed successor with matching runtime/seals returns to
`successor_ready`. If a handoff turn might already exist, recovery waits for its ACK
without resending. For an inspected empty original shell after frontend exit/start
failure, `reconcile --shell` returns to the launch checkpoint. It is unavailable
after work submission. Review recovered state, then `enable` to continue.

Never alter a stored hash to bypass changed input. Resolve new user input or checkout
changes and create a fresh checkpoint from the correct session. Pending approvals
remain with the native client. No exactly-once execution guarantee is claimed across
transport failure; durable intent plus ACK prevents automatic duplicate submission.

The adapter was validated with local app-server/native TUI 0.162.1 on Linux.
Settings updates are experimental and require capability negotiation; native and
server versions must match at registration. `/proc` process identity and Unix
sockets are required, and an app-server socket owned by another user or
world-writable is rejected. Schema/identity failures stop automation. The local
rollout format remains version-dependent. Git submodules require a recursive
checkout adapter and are rejected. See the
[official app-server protocol](https://learn.chatgpt.com/docs/app-server).

## Wait-queue coordination

The wait queue shares this guard lock and sets `external_delivery` while a notification
is unacknowledged. Usage, level and rotation-due observations continue; rotation
actions and checkpoint pause until that delivery is ACKed or explicitly reconciled.
After ten minutes the queue attempts one Herdr notification without replaying the
model prompt. Preserve the queue path in the handoff. A waiting queue
follows the acknowledged successor UUID; it does not prompt during handoff phases.

## Startup readiness and delivery faults

`awaiting_shell` has a 60-second exit deadline; `awaiting_tui` has a 120-second
protocol-readiness deadline. Incomplete startup is observed without replaying a
launch. Blank successors acquire a durable transcript on their first handoff turn;
ACK verifies it. Runtime drift or expired deadlines require inspection. A fault
records its original phase and attempts a Herdr notification. Worker guard waits
also check watcher process identity, lock and heartbeat, independently of phase.

Local transport faults (a Herdr CLI nonzero exit, a timeout, or an app-server
connection drop without a server error response) during phases with no journaled
in-flight action are counted in `transient_errors` and retried on the next poll;
the third consecutive fault escalates to `needs_attention`. A successful poll resets
the counter. Identity changes, server-rejected RPCs and any fault during a
`*_sending` phase escalate immediately and are never retried.

## Completed assignments

Workers must checkpoint after publishing and signalling a completed result, with
`assignment: {"id": "review-pr34-e43", "status": "completed", "evidence": "/absolute/report.json"}`.
The existing session/quiescence checks still apply. Completion is an explicit worker
attestation with an existing evidence file, not an inference from idle/done or an
independent approval of the result. The guard enters `parked` and sends no rotation
or resume prompts. A guard wait recognizes this as healthy.

When the coordinator assigns new work, send its new assignment ID and scope to the
parked worker. The worker reads that contract and checkpoints a fresh handoff with
`assignment: {"id": "review-pr37-e46", "status": "pending"}` before doing task work.
The checkpoint captures the current checkout, which may have changed while parked.
The usual sealed-input rotation and successor ACK then apply. Previously completed
IDs cannot be resumed. Do not send a second task prompt to the successor.

Old handoffs without `assignment` retain their existing behavior; parking requires
explicit completion. Coordinators awaiting queue/CI events must remain monitoring.
Installing this change does not retroactively park existing worker states; workers
must record completion through checkpoint. Restart existing watcher processes to
rebuild and restart the updated binary before using this protocol.
