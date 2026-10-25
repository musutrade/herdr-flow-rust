# Queue-bound task creation (PR1)

This is the first slice of [Issue #130](https://github.com/musutrade/herdr-flow-rust/issues/130).
A task is the lifecycle unit: its eventual final success **or** final failure
must trigger unified teardown. An intermediate command, test or CI failure is
not final failure. This slice establishes creation ownership; it does not yet
connect terminal outcomes, isolate all old writers or enable automatic teardown.
The opt-in [PR2 lifecycle barrier](task-lifecycle.md) is a separate fresh authority
mode; PR3/PR4 remain required before production teardown. These creation tests
are not deployment promises.

## One authority and existing machinery

`queue task-environment --state-dir QUEUE --spec FILE` uses the existing
coordinator identity check and queue lock. The task must already exist at the
exact `queue_id`, `task`, positive `epoch` and `state_version`, with no writer,
pending result, operation or lane/publication ownership. Its source commit is
fixed. Creating a new environment never adopts an existing path or old environment.

```json
{
  "action": "create",
  "queue_id": "the-existing-queue-id",
  "task": "task-130",
  "epoch": 1,
  "state_version": 1,
  "environment": {
    "workspace_root": "/private/task-130/work",
    "temp_root": "/private/task-130/temp",
    "delivery_targets": [],
    "repository": "/private/source-repository",
    "commit": "0123456789012345678901234567890123456789"
  }
}
```

Paths are explicit absolute paths. Both disposable roots must be absent, with
existing trusted parents, distinct from each other, the source repository and
control directories. The command creates one detached Git worktree at
`workspace_root/checkout`; it does not change source HEAD or create a branch.
Git's worktree administration is an explicitly recorded source-repository
mutation. Git child commands use the existing bounded executor, isolated Git
configuration and a child-only private creation mask; the supervisor's mask and
global configuration are unchanged. Partial/promisor repositories and configured
checkout filters are rejected before creation; task creation does not authorize
executing repository smudge/filter programs.

The environment document retains one `instance_id` plus
`creation.binding={queue_id,task_id,epoch}`. The queue task's `environment` maps
the same tuple to the exact state file and checkout. Existing root inode/device
identities, registry claims and per-task ownership lock are reused. Creation also
records source/common-directory identities, checkout inode, `.git` file inode,
Git administrative directory inode, and reciprocal Git links/registration.
A path, label or matching commit alone is not ownership.

Mutating creation Git commands carry the same task/epoch/queue/instance markers.
Recovery refuses to replay while an observed marked execution remains alive.
The bounded command executor additionally kills the direct Git child on supervisor
death, checking the parent identity after installing the death signal. This does
not prove complete helper-tree exclusion; existing process discovery has its
own observation limits and PR2 supplies the wider writer barrier.

Existing task runtime and native-session journals carry this owner tuple, while
processes retain the existing instance marker and PID/start-time registration.
Native activity ownership must resolve to the same task or its explicit lineage
requirement. Legacy environments remain legacy, with no inferred queue binding.
The existing `task-env create` entry does not silently upgrade them.

## Checkpoints and recovery

The lock order is queue → nonblocking task-environment lock → registry. Library
callers of `queue::task_action` retain the existing queue-lock requirement.
Creation uses existing atomic document saves, not a new database or scheduler:

1. Persist the immutable environment intent and reserve its roots/slot.
2. Save the queue mapping as `creating` before any Git work.
3. Create roots and checkout, persisting each object identity separately.
4. Save `git_pending`, add the detached worktree without checkout, then record
   its exact Git identities. A successful add with a lost response is inspected,
   not blindly repeated or replaced.
5. Populate only missing files, validate the clean initial checkout, then save
   the environment as `active` and checkpoint the queue as `active`.

An external-command failure or uncertain save retains the intent. Use the
current task version and exact observed instance to resume:

```json
{
  "action": "resume",
  "queue_id": "the-existing-queue-id",
  "task": "task-130",
  "epoch": 1,
  "state_version": 2,
  "instance_id": "the-recorded-instance-id"
}
```

Resume cannot change the source, roots or commit. It can complete an intent
whose first queue save failed, reconcile the same successful Git add, or finish
the active queue checkpoint after a lost reply. Repeated create is refused.
Already-active verification permits subsequent task commits while requiring the
same directory/Git objects. Replacement roots, symlinks, foreign Git links,
changed queue mappings or durable instance documents fail closed.

There remains a precise uncertainty window between creating a directory and
persisting its inode. If a directory exists without a recorded identity, PR1
retains it and reports an unresolved creation; it neither adopts nor deletes it.
Likewise, partial Git administration that cannot be matched is retained. These
are explicit recovery failures, not proof of release and not requests for a user
to approve deletion. Broader cross-module recovery/fault combinations remain PR4.
Private-directory checks and exact identities do not constitute global exclusion
of arbitrary same-UID writers; the all-writer barrier is PR2.

## Boundaries and retention

No Herdr workspace, terminal, Agent or native session is created by this entry.
Herdr is optional: only actual capabilities with verified ownership and release
may later be connected. This uses the existing task runtime for managed execution;
it does not build another runtime or depend on upstream Herdr changes.

New queue-bound instances cannot use legacy destroy/recover as an accidental
finalizer. `task-env status` reports `queue_instance_resume_only`; no final
outcome is invented. Legacy confirmed-delivery behavior is unchanged. These
local refusals are **not** an old-binary writer barrier: do not deploy this slice
or create production instances before the subsequent reviewed lifecycle slices.
No existing production workspace is adopted or deleted.

Creation journals and queue associations are task-temporary recovery data, not
formal deliverables or a permanent evidence archive. Unified finalization must
remove them when obligations are fulfilled. Only explicitly agreed delivery
targets may survive the task. See [task progress](task-progress.md),
[task runtime](task-runtime.md) and [delivery](delivery.md).

Tests use isolated Git repositories, fixture queues and a non-model child
process. They prove those local boundaries, not real Agent/Herdr integration,
terminal shutdown, final-outcome teardown or the 20-task acceptance campaign.
