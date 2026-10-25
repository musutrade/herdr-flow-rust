# Isolated lifecycle outcomes and write barrier (PR2)

This is the second ordered slice of [Issue #130](https://github.com/musutrade/herdr-flow-rust/issues/130).
It extends [task-instance creation](task-instance.md) with explicit owner outcomes
and short mutation permits. It does **not** deploy a new runtime or execute
terminal cleanup. Unified stop and whole-root deletion are PR3; cross-module
crash combinations are PR4. Existing production resources are not migrated.

## Fresh authority only

`queue init-isolated --state-dir NEW_QUEUE` requires an absent or empty private
queue directory. Use a fresh `HERDR_TASK_ENV_ROOT` for its environment control
state. Neither legacy queues, PR1 instances, live handles nor existing control
registries are adopted or upgraded.

The logical `queue.json` and environment `registry.json` are directories whose
private `authority.json` contains the new document. Queue results, deliveries,
watch state and gates reside in the queue authority namespace. Environment,
plan and lock indexes reside in the registry authority namespace. Each JSON
sidecar in a protected namespace is also a document directory. Reads reject
ordinary JSON files introduced there by an old writer, including an old queue
initialized below the new directory. Old file reads, atomic file replacement
and file removal cannot consume, overwrite or release the new authority.

Runtime and receiver namespaces retain an explicit `lifecycle-owner.json` and
protect `runtime.json`, `state.json`, creation and submission journals. The
broker never treats a legacy sidecar as an isolated authority. No PATH binary
or existing watcher is replaced by initialization. This protects supported old
entry points; it is not protection against arbitrary same-UID filesystem
modification or a claim of global process exclusion.

## Owner and activity identity

The lifecycle Owner is exactly
`{queue_root, queue_id, task_id, epoch, instance_id}`. The queue root is canonical
and absolute. A V1 task can own its instance. A V2 activity uses only its explicit
`lineage.requirement` owner; the activity retains its own task, epoch, assignment
and session epoch. Completing a child activity does not finalize the owner.

Environment creation targets the owner. A V2 owner must already select an
existing Git artifact whose adapter is `git` and whose full SHA equals both the
selected candidate and the creation commit; the existing artifact verifier
checks it. Null candidates and non-Git artifacts are unsupported here. There is
no HEAD fallback or inference from a child activity.

Before binding an instance, isolated tasks permit only explicit registration,
lineage plan/artifact/candidate preparation and environment creation. They do
not dispatch, execute or ingest results. Creating uses prospective owner EX
with an empty instance only to reserve that task gate. Ordinary permit checks
always require the exact nonempty instance and an active queue association.

## Short permits and lock order

Ordinary writes, RPC submissions and observer writes take lifecycle SH. Final
outcomes take EX. Both are nonblocking and task-scoped; unrelated owners can
continue concurrently. A permit covers one bounded action, not a server,
watcher, native session or complete model turn. Idle brokers hold no SH permit.
The broker takes SH and reads current authority atomically without reacquiring
its parent caller's queue lock. It writes only its own bounded journals.

Queue mutation retains the existing queue lock, and environment documents keep
their own exclusive handle lock to prevent whole-document lost updates.
Queue-first entry points acquire lifecycle permits nonblockingly: contention
unwinds and releases the queue lock before an explicit retry. They never wait
for EX while holding queue. Environment-to-queue entries also fail nonblockingly.
There is no blocking cycle or lifetime model-turn lock.

Creation holds EX across the persisted intent, creating queue checkpoint,
materialization and active checkpoint. Resume uses the same immutable intent.
Ordinary writes recheck the tuple and closing state. Saving a stale queue cannot
remove/change a bound mapping or overwrite an already recorded terminal task.
Deleting authority does not authorize its recreation by a stale handle.

An isolated `TaskEnvironment::command` cannot authorize delayed spawn: its raw
command refuses execution. Supported task-runtime launch keeps a permit through
command construction, spawn, process registration and journal persistence.
An original native creation/submission context stores the complete Owner;
collectors return that stored `lifecycle_owner`, rather than filling it from a
later queue snapshot. Stale signals and imported results fail before mutation.
Legacy `queue run`, including background spawn, is refused for isolated queues.

The separate long-running CI watcher has no task-exclusive lifecycle
registration in this slice. It explicitly refuses isolated runtime namespaces
and isolated child execution; it is not silently relabelled as a managed
observer. Existing bounded queue/PR/batch CI observation remains available under
queue permits, and independent legacy CI observers retain their contract.

## Explicit final outcome

The existing coordinator invokes:

```text
herdr-flow queue task-finalize --state-dir QUEUE --spec FILE
```

```json
{
  "task": "owner-task",
  "state_version": 3,
  "queue_id": "the-queue-id",
  "epoch": 1,
  "instance_id": "the-exact-instance",
  "outcome": "failure",
  "reason": "Explicit owner decision that this task has finally failed"
}
```

One atomic queue save records `final_outcome`, `final_reason`, `finalized_at`,
`phase=finalizing`, and `cleanup_obligation={owner,status:pending,recorded_at}`.
Failure requires an explicit reason. Success additionally requires the existing
confirmed delivery for this exact environment/task/instance. Task acceptance,
child completion, a test/CI failure, pause, and process exit are not owner final
success or failure. Explicit cancellation of an isolated owner records final
failure; cancellation of an activity does not close its parent.

A retry with the same owner, outcome and reason confirms the existing decision;
a conflicting decision fails. Failure does not require an empty writer,
pending list or operation list: those registrations are obligations to stop and
reclaim later, not permission to block the final decision indefinitely. Terminal
restore reuses EX and the environment handle lock, checks the durable owner, and
allows incomplete Creating state without trying to adopt or materialize unknown
resources. It never repairs a registry as a side effect of the decision.

Once the final outcome is recorded, ordinary writes, spawn, submission, result
signal/import, task mutation and observer attachment refuse that owner. No
writer, process, worktree, receipt or registration is cleared; PR2 does not
claim teardown has happened. An already active mixed notification delivery may
remain frozen with its uncertainty preserved until PR3 handles related entries.
It is not falsely ACKed, dropped or resent. Other owners' business permits are
independent, although that shared notification slot can remain occupied.

## Recovery and acceptance boundaries

Pending cleanup is not a permanent archive. Only the agreed formal delivery
survives the eventual unified cleanup; authority, gates, journals, temporary
logs and recovery records are temporary obligations to remove in PR3/PR4.
Generic legacy destroy/recover/unregister paths cannot release isolated claims.
No new isolated task should be deployed for production use before the subsequent
reviewed cleanup and recovery slices are complete.

Tests use private fixture queues, Git worktrees and non-model subprocesses. They
exercise real locks, directory sentinels and an idle fixture broker refusing a
late RPC without contacting a native server. They do not replace real Agent,
Herdr, production teardown or the 20-task end-to-end acceptance. Herdr remains
optional and no shared resource ownership is inferred.
