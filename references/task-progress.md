# V1 result application and takeover

For versioned requirements, non-Git artifacts and concurrent development/acceptance
activities, see [the #34 lineage contract](task-lineage.md). The legacy commands
below remain compatible; they do not implicitly migrate tasks.

This is the first V1 implementation slice, Issues #15/#13, in the order specified
by #9. It adds progress records to the existing `queue.json`; it does not enable
PR publication, automatic merge, trusted App checks or batching. Those remain
separate V1/V2 acceptance stages. Offline tests do not attest live notification.

## Receiver and producer readiness

Register the exact current Codex native TUI bridge and guard using
[context rotation](context-rotation.md), enable the queue, and start both existing
watchers. From the receiving session run:

```bash
herdr-flow queue readiness --state-dir /absolute/queue
```

This writes `readiness.json` and exits 1 when active waking is unavailable. It
checks the current `CODEX_THREAD_ID`, pinned registration, app-server identity,
both watcher process/start-time/lock/heartbeat records, delivery state and notify
setting. `active_wakeup` describes current transport readiness; the separate
`last_confirmed_receipt` identifies an actual ACK from this session, if one exists.
Neither field attests that results have been applied. `producer_channels` lists
the registered result paths, owner, identity and return mechanism.

`context_telemetry` and `safe_rotation` are independent of notification readiness.
Observation-only guards can deliver messages; automatic rotation additionally
requires known telemetry and enabled `auto_rotate`. OpenCode automatic rotation
is explicitly unsupported. Other producers can use a pinned signal or command
wrapper, without claiming the Codex rotation capability.

For OpenCode 1.19.3, bind the exact native session/model/directory with
`opencode register`, then use `opencode dispatch` / `collect` against a current
managed signal wait. Collection verifies live identity, message correlation,
task/epoch/assignment/candidate and workspace HEAD before publishing a report
and signal. The queue receiver can be Codex or OpenCode; OpenCode receivers use
`OPENCODE_SESSION_ID` for ACK/readiness/task commands. `producer_session` remains
Codex-specific and must be omitted for OpenCode. Generic `producer_identity`
metadata alone is not enforced by the queue: filesystem trust and the native
collector's live checks remain separate. Automatic rotation is unsupported;
2.0 integration remains deferred. See
[OpenCode integration](opencode-v2.md).

An embedded `--no-daemon` Codex frontend has no native bridge marker. Do not copy
an old marker, borrow an old guard, fabricate `tui.json`, or restart an active
writer to make readiness pass. Finish local work and persist a handoff before an
inspected, quiescent reattach through the documented bridge. No automatic waking
or live rotation can be claimed until that receiving session passes its checks.

## Register the result before starting work

Existing queues and unlinked waits keep their existing behavior. To manage task
progress, add `assignment`, full `candidate` SHA and `producer_session` to the
normal result wait. Guard waits remain independent health dependencies.

```json
{
  "id": "step-one", "task": "owner/repo#123", "epoch": 1,
  "assignment": "step-one-a1", "candidate": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
  "producer_session": "11111111-1111-1111-1111-111111111111",
  "owner": "dev", "kind": "signal", "timeout_seconds": 1800,
  "reason": "Wait for the first accepted step", "condition": "Result and evidence published"
}
```

Then use `task-add --spec /absolute/task.json` before result collection:

```json
{
  "task": "owner/repo#123", "wait": "step-one", "goal": "Complete the two accepted steps",
  "apply_timeout_seconds": 600,
  "workspace": "/absolute/checkout", "execution_evidence": "/absolute/initial-assignment.json",
  "acceptance": [
    {"id": "first", "description": "First step checked", "required": true},
    {"id": "second", "description": "Second step checked", "required": true}
  ],
  "authorization": [
    {"id": "develop", "action": "development", "source": "Existing user instruction and scope"}
  ]
}
```

For a writing assignment, include the absolute workspace and existing execution
evidence. Workspace paths are resolved before comparing ownership. The queue
refuses a second reserved writer at the same path. This is local coordination,
not a filesystem security boundary; independent queues must still serialize
writers through their task contracts. A read-only initial dependency may omit
workspace. An authorization record describes existing permission; it grants none.

The collector associates the immutable event, assignment, epoch, candidate and
application deadline with the task **before delivery**. `ack` records receipt and
releases the delivery marker. It leaves the result in `tasks[TASK].pending`.
Closing that task's current wait or stopping its queue is refused until the task
is complete or explicitly cancelled.

## Apply a result and persist its next action

Read current `queue.json`, ACK the delivery, evaluate the evidence, then run
`task-apply --spec /absolute/application.json` from the registered coordinator.
All mutations require the current `state_version`, under the same queue lock.

```json
{
  "task": "owner/repo#123", "state_version": 2, "event": "ACTUAL_EVENT_ID",
  "release_evidence": "/absolute/verified-first-writer-release.json",
  "acceptance": {"first": "/absolute/first-result.json"},
  "next_action": {
    "kind": "dispatch", "operation": "step-two-op1", "assignment": "step-two-a1",
    "session": "22222222-2222-2222-2222-222222222222", "owner": "dev",
    "workspace": "/absolute/checkout", "writes": true,
    "authorization": "develop", "action": "development",
    "instruction": "Perform the second accepted step and publish its evidence",
    "wait": "step-two", "accept_timeout_seconds": 600
  }
}
```

Register `step-two` first with the same task/epoch/candidate, new assignment and
exact receiving producer session. Dispatch intent creation and dispatch beginning
both require current receiver notification readiness. Application and the concrete
successor intent are saved in one atomic queue update. Other allowed successors:

- `{"kind":"wait","wait":"new-registered-dependency"}` requires a matching open,
  unconsumed wait, whose owner, condition and deadline are retained.
- `{"kind":"blocked","owner":"coordinator","reason":"specific cause","condition":"specific recovery condition"}`
  records a genuine blocker without manufacturing another assignment.
- `{"kind":"complete"}` requires evidence for every original required acceptance
  criterion, resolved blocking findings, and release of any writing assignment.

If there are multiple pending results, list all other event IDs in `archive_events`
to explicitly supersede them. Application cannot change the original acceptance
inventory. Review `findings` require `id`, explicit boolean `blocking`, `evidence`
and `clears_when`; `resolve_findings` maps finding IDs to inspected resolution or
retraction evidence. Nonblocking observations do not block completion.

## Dispatch, takeover, and ambiguity

`task-begin --spec` accepts `task`, current `state_version` and `operation`. It
durably changes the operation from `prepared` to `sending` **before** the caller
uses an existing authorized transport. A second begin is refused. The optional
`task-sent --spec` records a returned `response` such as a transport turn ID; it
does not prove takeover or completion and does not reset the takeover deadline.

The exact receiving `CODEX_THREAD_ID` calls `task-accept --spec` with task, current
version, operation, assignment and takeover `evidence`. An accept from another
session/assignment is rejected. A result that arrives before the accept is retained
but cannot advance the task. Once accepted, it becomes available for application.

For a managed long command, the existing `run` wrapper performs takeover itself
after exclusively reserving `commands/WAIT/invocation.json`, before starting the
child. Invoke it from the pinned producer session after `task-begin`. It verifies
the assignment and reserved cwd, records accepted/executing state, then captures
the real command result. Reusing the wait cannot execute the command twice.

If a send, wrapper, or reply is interrupted, inspect the existing invocation,
session and operation. `sending`/`submitted` are never reset or replayed
automatically. The intended recipient may still provide the exact takeover.
Otherwise record a blocker or explicitly cancel, inspect the existing writer,
and release it with evidence before any replacement contract. This does not
promise exactly-once external execution.

## Deadlines and recovery

`herdr-flow queue watch` calls progress checks during its existing `tick()`, even
without new producer events. No extra daemon, cron, periodic model prompt or
GitHub poll is created. It emits one `progress_stall` event per task/epoch/
assignment/state-version/reason for an unapplied result or unaccepted successor.
Result deadlines start at persisted collection time; takeover deadlines start
at persisted intent preparation. ACK, restart, transport response and repeated
ticks do not extend them. An unresolved send remains ambiguous, not retryable.

Before acting on an alarm, use `event-current --event EVENT_ID`. It exits 1 for
an obsolete alarm or paused/cancelled task. Normal commands and registered waits
remain silent until their existing wait deadline, which reports timeout without
killing, restarting or reclassifying work as a code defect.

`task-control --spec` uses task/version, `control` (`paused`, `running`, or
`cancelled`) and an explicit reason. Pause/cancel suppress progress continuation;
cancel does not kill a process or release its writer. `task-release --spec`
requires inspected release evidence for a blocked/cancelled task. To recover a
blocked task, register a fresh matching dependency and use `task-resume --spec`
with task/version, `wait` and recovery `evidence`. It preserves prior acceptance.

A stopped watcher cannot send its own failure alarm. `readiness` explicitly
reports that gap; restore the existing receiver/watcher identities, then the next
tick checks the original overdue deadlines. Never claim an ACK from another
session or a status-board-only queue as current automatic protection.

## Read-only diagnosis

`herdr-flow queue status` prints a `diagnosis` line and a concrete next step for
each managed task, plus guidance for the current notification state. `--json`
adds the same data under `diagnostics` (`delivery` and `tasks`) without saving
it. `readiness` likewise adds `next_steps` alongside `gaps`. These are read-only
views; they do not apply results, dispatch work, approve anything or grant a
retry.

The task classifications separate states that can look equally idle in a pane:

- `executing`: an owner/session is working; check its registered wait and
  deadline. Do not kill, reassign or duplicate the writer without inspected
  evidence.
- `result_pending_application` / `result_unapplied_overdue`: a result arrived
  but is not applied. ACK is receipt only; apply the current event with
  `task-apply`, and never resend the original assignment.
- `successor_prepared`: the next operation is recorded but not sent. Use the
  existing `task-begin` path, then require exact `task-accept`.
- `successor_uncertain` / `successor_takeover_overdue`: dispatch or takeover may
  have happened. Inspect the existing operation and exact recipient session;
  accept only a matching takeover, otherwise record a blocker. Never replay.
- `waiting`: a registered wait is open; observe the event or its deadline. No
  model polling is needed.
- `blocked`: an explicit condition is recorded; recover only through a fresh
  matching wait and `task-resume` with inspected evidence.
- `paused`, `cancelled`, `completed`, `no_open_next_action` and
  `result_pending_without_event` are reported separately rather than treated as
  normal progress.

`herdr-flow guard status` includes the same kind of `diagnosis` object for
watcher health, pending external delivery, rotation requests, in-flight
mutations and unknown telemetry. Watcher health also records a `next_step` for a
stopped process, reused PID, expired heartbeat or lost lock. Diagnostics never
suggest killing an unrelated process, bypassing a state-directory mutex,
auto-approving a prompt or reusing a wait ID to reset recovery limits.

Agent `idle`/`done` is not a task state. A pane can be idle while a result is
pending application, a successor is unaccepted, or a valid external wait is
open. Use the persisted classification and the listed evidence instead of the
terminal label. Cross-check these messages against the retained V1/#13 recovery
evidence when exercising a real interruption; a status line is not a new
authorization.

## Retention and evidence

Queue state is bounded at 100 managed tasks, 1,000 waits, 10,000 events and 10,000
deliveries; each task permits 128 operations and keeps the latest 256 transition
index entries, disclosing discarded index entries via `older_transitions`.
Unresolved results, operations, original acceptance, authorization and writer
ownership are kept separately. Do not automatically discard unresolved evidence
at a capacity limit. Complete/cancel and reconcile the queue, then archive it
outside the checkout and start a new bounded task contract.

Record the exact live receiver, result event, ACK, application and takeover times
from the queue/delivery/operation records. Retain a compact evidence index and
the declared completion conditions. Full transcripts, private paths and command
logs stay outside GitHub. The Rust test suite exercises state-level apply/stall
transitions and timestamps, but not process crash/restart, real-clock watcher
ticks or live notification; the actual subprocess wrapper is only partially
covered. Live active-arrival and rotation acceptance must be recorded separately.
