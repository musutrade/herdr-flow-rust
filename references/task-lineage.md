# Requirement revisions, artifacts and concurrent activities (#34)

`queue task-lineage --state-dir DIR --spec FILE` operates under the same queue
lock, coordinator identity check and atomic save as `task-apply`. It records
relationships and existing authorization; it does not send prompts, manufacture
permission, deliver an artifact or destroy an environment. Use the task environment
from #33 for disposable resources. Independent native producer/session collection,
and four-outcome acceptance are documented in [task-runtime.md](task-runtime.md)
and [independent-acceptance.md](independent-acceptance.md). Final delivery/cleanup
continues in #37.

## Identities and compatibility

- `schema_version`: absent means the legacy V1 task; new legacy registrations
  explicitly write `1`. Requirement parents and linked activities write `2`.
  Unknown versions are rejected by task mutations.
- `requirement_revision`: an explicit, confirmed change to the user's requirement,
  project specification, agreed endpoint or authorization references. Previous
  revisions are retained only while this task environment is active.
- `epoch`: the existing task incarnation from the registered wait. It is separate
  from changing requirements or switching producer sessions.
- `lineage.session_epoch`: positive producer-session generation within an activity.
  A successor dispatch increments it and requires the next wait to pin that value.
- `acceptance_round`: the parent's generation of a submitted candidate. Selecting
  even the same artifact again starts another round and clears prior approval.
- `state_version`: optimistic concurrency for each individual parent/activity
  mutation. ACK and parent changes do not stand in for this version.
- `scope_epoch`: the current stage generation, also bound into linked activities
  and their approvals. Missing means zero for pre-stage records. Entering or
  completing a stage advances it; leaving a stage never restores epoch zero.

Development and acceptance are separate managed child tasks, each with its own
wait, writer, pending results, operations, deadlines and state version. Their
`lineage` binds the parent, requirement revision, role, input artifact, acceptance
round (zero for development) and session epoch. This reuses the existing task
machinery rather than introducing a DAG scheduler.

Old task-add/wait/PR/batch contracts remain available without implicit migration.
A legacy task can explicitly use `kind: "migrate"` with its current state version
and the same requirement fields as `plan`. It must be blocked/cancelled, have no
writer or pending results, and no uncertain/prepared operations. PR/batch-owned
tasks cannot migrate this way. The old record is retained under `legacy` for
runtime reconciliation, but no candidate, evidence or approval is promoted.
Migration never resets an external sending/submitted operation for retry.

The `migrate` action changes queue records only. Retaining an old path, session
identifier, authorization reference or undelivered artifact under `legacy` does
not register it with a task environment or grant permission to delete it. Keep
undelivered artifacts available until their actual identity and delivery status
are reconciled. Associate only resources whose existing task ownership can be
verified through the environment's registration/recovery contracts; do not infer
ownership from a path name, scan a home directory for matching files, or adopt
unknown user resources. If ownership cannot be established, leave that resource
untouched and record the concrete unresolved association. Old evidence and
worker completion remain historical input, never a new acceptance verdict.

## Register the requirement and a real artifact

A parent requires original requirement references, confirmed change references,
project specification references, the agreed endpoint and a confirmation source:

```json
{
  "kind": "plan", "task": "document", "goal": "Deliver the agreed document",
  "basis": {
    "original": ["user request reference"], "changes": [],
    "specifications": ["project style guide"],
    "endpoint": "customer document location",
    "confirmation": "existing confirmed instruction reference"
  },
  "authorization": [
    {"id": "develop", "action": "development", "source": "existing user authorization"}
  ]
}
```

`acceptance` may supply stable completion conditions in the existing criterion
format. When absent, a single required `goal` criterion is generated from `goal`;
the user need not provide a complete test checklist. These conditions and their
sources belong to a requirement revision. `kind: "revise"` requires the full new
requirement fields and current parent `state_version`; it records `previous`,
clears the candidate/delivery target/approval and advances the round. Routine
result application and derived checks cannot rewrite these conditions.

Register an artifact using `kind: "artifact"`, parent `task`, parent
`state_version` and `artifact`:

```json
{
  "kind": "artifact", "task": "document", "state_version": 1,
  "artifact": {
    "adapter": "file-sha256", "id": "ACTUAL_SHA256_OF_FILE",
    "location": "/absolute/task-environment/document", "input": null
  }
}
```

The built-in `file-sha256` adapter hashes a real regular file; the `git` adapter
requires a real commit object identified by its full SHA in the repository at
`location`. Missing objects, wrong hashes, unsupported adapters and Git blob IDs
fail. Object IDs cannot be overwritten. Subsequent versions reference a registered
`input`; registration alone never changes an in-flight acceptance candidate.
Non-Git artifacts do not require a PR, GitHub or a Git workspace.

The library's `ArtifactAdapter` boundary permits customer adapters to define and
verify real objects. Callers implementing an adapter are responsible for object
verification; a JSON declaration claiming verification is not an adapter. The
standard CLI uses the built-in adapters. Library integrations pass the same adapter
to `lineage::action_with_adapter` and `progress::apply_with_adapter`; core state
stores identity and lineage.
Artifacts must remain available and stable through validation and delivery; a
mutable file whose hash changes is refused on reuse.

Select a candidate explicitly with `kind: "candidate"`, parent `task`, current
`state_version`, `candidate` artifact ID and `reason`. Selection revalidates the
actual object, fixes `candidate` and `delivery_target` and starts a new acceptance
round. A development output does **not** call this operation implicitly.

## Stage selection and fresh acceptance

After all existing linked executions have finished (complete or known failed) and been applied, their writers
released and pending/uncertain operations reconciled, select a stage explicitly:

```json
{
  "kind": "stage", "task": "document", "state_version": 3,
  "id": "draft", "scope": "Deliver the independently reviewed draft",
  "criteria": ["draft-content"]
}
```

Every selected ID must already exist in the agreed requirement revision. Selection
retains its full criterion, required flag, original basis and authority. It clears
the current candidate/approval and advances the scope epoch and acceptance round.
Another active stage or delivery cannot be overwritten. Register new development
or acceptance activities normally; they inherit this scope. Native packets carry
both the scoped `requirement` and complete `overall_requirement`.

Finish a confirmed delivery with [stage-complete](delivery.md#stage-delivery-within-the-same-requirement-2026-10-27),
which reopens the same parent for another stage or full-scope acceptance. Stage
success cannot finalize the parent. Old bindings remain stale after completion,
including old development results. A confirmed requirement revision explicitly
supersedes an active stage and advances the scope epoch; it does not release an
outstanding writer or grant a replacement execution.

## Register independent activities

```json
{
  "kind": "activity", "task": "document", "state_version": 3,
  "activity": "document-review-1", "role": "acceptance",
  "input": "ACTUAL_REGISTERED_ARTIFACT_ID", "session_epoch": 1,
  "result_wait": {
    "id": "document-review-1", "epoch": 1, "assignment": "review-1",
    "owner": "reviewer", "kind": "signal", "timeout_seconds": 1800,
    "reason": "Independent verification", "condition": "Result published"
  }
}
```

Use `role: "development"` for development. Acceptance must use the selected
candidate; development may continue from a registered input while acceptance is
running. The transaction registers both the wait and child task, so a failed
reservation leaves neither behind. Goal, stable conditions and permissions are
inherited from the fixed requirement revision. Duplicate task/wait IDs fail rather
than dispatching again. Task, wait and artifact capacities are bounded.

For an already-started writing activity include the existing absolute `workspace`,
`execution_evidence` and pinned `producer_session` in `result_wait`, as required
by the existing task registration contract. Workspace reservations include nested
paths and resolved symlink aliases across sibling activities and other tasks.
Separate workspaces can proceed concurrently. A stale/paused/cancelled activity
keeps its reservation until inspected release; supersession never kills a writer
or frees its resource silently. Producer-specific session conventions remain the
existing conventions until #35.

Register subsequent dispatches through `task-apply` / `task-begin` /
`task-accept`. The successor wait must copy `lineage` and `artifact_adapter`, use
the new assignment and increment `lineage.session_epoch`. The normal receiver
readiness checks and prepared → sending → accepted protocol still apply. Recovery
with the same assignment/session uses a fresh wait ID with the same binding.

An acceptance activity can use `kind: "checks"` with its **child** task/version:

```json
{
  "kind": "checks", "task": "document-review-1", "state_version": 1,
  "checks": [
    {"id": "layout", "criterion": "goal", "description": "Verify document layout",
     "source": "project style guide section 2"}
  ]
}
```

Each check references a stable completion criterion and an explicit basis source.
Updating this list only changes `derived_checks`. Deleting all derived checks does
not satisfy, remove or weaken the required goal evidence in `task-apply`.

## Atomic results, stale events and recovery

A development `task-apply` may include `output` in the artifact format, with
`input` exactly equal to that activity's development input. The actual object is
verified and its activity/input relation is committed in the **same transaction**
as the result application and successor intent. Verification failure rolls the
entire transaction back. A new output is recorded in the parent's artifacts and
activity record; it never changes the review candidate or grants approval.

Current linked acceptance uses the [independent verdict protocol](independent-acceptance.md).
Only applying a bound immutable `passed` report records approval for that exact
requirement revision, candidate and round, after revalidating the object. Ordinary
execution completion and coordinator-supplied evidence cannot grant approval.
Failed, unable and ambiguous reports save their corresponding continuation instead.
Completing a child is not delivery or completion of the parent task environment.

Collection archives/withdraws late results; already collected results are checked
again at application. Begin, takeover, command execution and recovery also reject
obsolete bindings. ACK remains receipt only. `event-current` rejects obsolete
results and timeout alarms, and obsolete activities produce no new progress-stall
alarms. Their resources and ambiguous sends remain inspectable and are never
implicitly replayed. Restart preserves all these bindings in `queue.json`.

Pausing a parent suspends application, dispatch and alarms but retains valid
arriving results for later application after resuming. Diagnostics distinguish
`coordinating`, `parent_inactive` and `obsolete_activity`. A pause is not a new
requirement revision.

Changing a requirement invalidates both development and acceptance children from
the earlier revision. Replacing a candidate/round invalidates only the earlier
acceptance round; independent development can continue. Parent/activity state
versions are independent, so producing B does not invalidate a still-current
review of A. Review A can approve only A; selecting B clears that approval.

All relationship records, migration records and evidence are runtime task data,
not permanent completion archives. They belong to the disposable environment and
are removed with the owned task environment. The lineage tests cover queue
persistence, association, ACK, stale-result rejection, resource exclusion and
non-replayed operation recovery. Native-role verification is described in the
runtime and independent-acceptance contracts; customer delivery remains #37.

Native producer identities, fresh task-owned sessions, development output collection
and disposal are specified in [task-runtime.md](task-runtime.md). Linked native
waits use `producer: {adapter, session}`; legacy `producer_session` is Codex-only.

Linked acceptance now requires an immutable independent verdict; see
[independent acceptance](independent-acceptance.md) for application and recovery.
