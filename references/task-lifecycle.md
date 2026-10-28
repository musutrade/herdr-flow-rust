# Isolated task outcomes, write barriers and finalization

These are the lifecycle slices of [Issue #130](https://github.com/musutrade/herdr-flow-rust/issues/130).
It extends [task-instance creation](task-instance.md) with explicit owner outcomes
and short mutation permits, then automatic owned-resource finalization. PR3
reuses the existing watch and cleanup machinery; PR4 exercises cross-module
fault combinations. This does not deploy a new runtime or migrate production resources.

## Fresh authority only

`queue init-isolated --state-dir NEW_QUEUE` requires an absent or empty private
queue directory. For fresh format 3, explicitly set `HERDR_TASK_ENV_ROOT` to
`QUEUE/queue.json/task-environments`. The queue first durably reserves this fixed
control child, then records its identities before any task allocation. See the
[allocation protocol](task-instance.md#fresh-isolated-allocation-protocol) for
mount restrictions and bootstrap recovery. Neither legacy queues, PR1 instances,
live handles nor existing control registries are adopted or upgraded. Earlier
isolated formats, including PR3/PR4 format 2, are not migrated. Old fixed entry points reject this
format. The queue has a checked monotonic `authority_version`: every save must
match the current version, preventing an old snapshot from resurrecting a removed
owner without retaining a permanent per-task tombstone.

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

Confirmed intermediate stages can reclaim settled child execution resources
before this final outcome. The original parent and referenced runtime evidence
remain active; see [stage resource reclamation](stage-resource-reclamation.md)
for eligibility, pending continuation, retirement fences and terminal takeover.

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
writer, process, worktree, receipt or registration is cleared by recording alone; it does not
claim teardown has happened at that checkpoint. The high-level terminal entry
then releases its recording locks and automatically attempts finalization. Other
owners retain independent business permits.

## Automatic finalization (PR3)

The CLI `task-finalize` and explicit owner cancellation use the high-level
`queue::finalization::task_action` entry: record the final decision, release the
queue transaction, then drive one bounded cleanup attempt. The caller-locked
`queue::task_action` library entry is a recording layer for transactions and
crash-recovery fixtures; it is not the complete finalization API. Existing watch
iterations also drive pending obligations, including on stopped queues. Busy
resources or observation errors preserve the obligation for system retry; they
do not change the business outcome or request manual deletion approval.

The driver holds the exact owner's EX gate, uses short queue/environment locks,
and freezes typed association and resource identities in the existing temporary
cleanup plan. It stops proved task processes before touching the worktree or
roots. Both final success and final failure remove the entire dedicated roots,
including dirty files, untracked files, build outputs and runtime journals.
No project-type, extension or directory-content deletion classifier is used.
Formal delivery targets outside those roots remain intact.

Git removal verifies source, checkout, Git pointer and administrative identities.
Native mutation children carry the instance/task/epoch/queue markers and direct
parent-death protection. A durable `GitRemoving` checkpoint permits recovery of
partly removed components: every surviving identity must still match. Incomplete
admin directories are quarantined at the recorded inode before their contents
are cleared. There is no repository-wide prune. Wrong identities, unknown
partial roots or mounts retain the obligation; a missing path alone does not
prove completed cleanup.

If real Git add completed while its reply was lost, the original creation may
still be `GitPending` without a saved Git identity. After process quiescence,
cleanup can reconcile that exact add read-only: both roots and checkout must have
their original recorded inodes, the repository/common directory and reciprocal
Git links must match, HEAD must equal the creation commit, and the exact admin
directory must not occur in the saved pre-add snapshot. Cleanup freezes the
observed identity as `git_removal_identity` in the temporary terminal plan before
`GitRemoving`. It does not alter the environment's original creation document,
materialize the checkout, or repeat add. Recovery uses that frozen proof through
Git removal and final absence verification; it never resamples a replacement
path. Prepared intents and unknown mkdir objects do not gain this authority.

Process observation is fallible. Fresh creation records boot ID, PID/time
namespace identity and an integer boot-clock birth floor before managed spawn;
recovery never resamples it. Only positively older, unregistered processes can
be excluded before reading their environment. Equal/newer processes and known
registered, descendant or frozen targets remain strictly checked. Boot/namespace
changes, unreadable relevant identity or uncertain ancestry block destructive
steps for retry. Historical descendant snapshots alone never grant signal
authority. Direct-child parent-death protection is not a universal process-tree
sandbox or proof of arbitrary marker-clearing helper ownership.

Owned results/waits/events are removed by full queue/task/epoch/assignment/owner
binding. Mixed in-flight deliveries retain their ID, receiver session, phase and
uncertainty while owned event payloads are removed; surviving owners can ACK the
same delivery. They are never resent or falsely ACKed by cleanup. An all-owned
delivery is removed together with its exact shared-receiver link. A task-owned
receiver additionally requires a protected runtime Owner and containment inside
the registered roots; a label is insufficient. Shared receivers and unrelated
lane state remain intact. Fresh isolated admission rejects cross-owner V1
claim/root succession; cleanup cannot erase a surviving claim authority chain.
Explicit same-owner V2 activities retain their individual epochs and are removed
with the owner. Settled Git receipt references are matched by typed identity.

Two closing owners can depend on the same task-owned receiver. Its owner may
finish cleanup before the other owner resumes. Under the queue lock, receiver
retirement reads each pending owner's immutable cleanup plan once, without
taking that other owner's EX gate. The same fixed map is used for pruning and
validating the queue transaction. That transaction records an exact
`receiver_dependency` and `retired_by` Owner in the dependent owner's existing
cleanup obligation, including when its plan was saved before the dependency
checkpoint. The plan itself is never rewritten. A verified retirement skips
the old receiver path entirely; missing paths or matching session labels alone
do not grant this permission. Without that proof, cleanup verifies the frozen
Owner and scope before and after locking the existing guard, without creating
its directory. A replacement receiver is not touched. These dependency records
disappear with the owner; ordinary saves still cannot change terminal rows.

After resources and associations are gone, the driver releases the environment
and registry claim, checkpoints the cleanup-plan inode proof in the queue,
removes that temporary plan and task gates, and atomically removes the owner and
its activities last. The queue checkpoint supports interrupted plan erasure.
Late writes, signals, ACKs and spawn cannot recreate deleted authority. All
cleanup records, logs inside roots and recovery registrations are temporary;
this creates no permanent evidence archive.

## Recovery and acceptance boundaries

Pending cleanup is not a permanent archive. Only the agreed formal delivery
survives unified cleanup; authority, gates, journals, temporary logs and recovery
records are removed with their task obligations.
Generic legacy destroy/recover/unregister paths cannot release isolated claims.
No new isolated task should be deployed for production use before the subsequent
reviewed recovery and genuine end-to-end acceptance are complete. Fresh format-3
creation uses a durable parent-owned allocation envelope and fixed slots before
atomic transfer. The old mkdir-before-inode window remains unsupported for legacy
instances; none are adopted or migrated. Process-interruption fixtures for the
fresh protocol do not prove power-failure recovery or production readiness.

Tests use private fixture queues, Git worktrees and non-model subprocesses. They
exercise real locks, directory sentinels and an idle fixture broker refusing a
late RPC without contacting a native server. They do not replace real Agent,
Herdr, production teardown or the 20-task end-to-end acceptance. Herdr remains
optional and no shared resource ownership is inferred.

The creation/removal reply-loss fixture executes real Git in a supervisor child,
withholds the completed mutation's reply, then kills the supervisor. Assertions
before fixture destruction check same-instance recovery, direct-child exit,
unchanged original creation, and removal of roots, Git registration, queue owner
and temporary recovery records. Invalid pre-add identity, HEAD, reciprocal links
and replaced inodes are separate refusal cases. These are isolated crash fixtures,
not real Agent or Herdr lifecycle acceptance.

A test-only checkpoint also stops a real driver after durable process freezing:
the supervisor is killed while the fixture child is still stopped, then another
driver terminates it and removes the roots. A separate combination uses the
existing Local file executor and protocol acceptance fixtures: an actual file
delivery loses its reply, observation later confirms it, and terminal recovery
preserves the customer file with exactly one execution. The acceptance protocol
in that test is synthetic; the endpoint I/O and cleanup are real fixture actions.

Additional combinations run two separate driver processes against one obligation,
recover an actually filtered mixed-delivery sidecar after a busy guard prevents
the queue commit, and reuse a fully cleaned task name and paths for a new epoch
and instance. The latter verifies that stale results, snapshots and terminal
requests cannot change the new instance. Receiver-retirement tests distinguish
real owner cleanup from deliberately constructed checkpoint and corrupt-record
states; they do not treat missing state as retirement proof.
