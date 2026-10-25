# Queue-bound task creation and isolated allocation

This is the first slice of [Issue #130](https://github.com/musutrade/herdr-flow-rust/issues/130).
A task is the lifecycle unit: its eventual final success **or** final failure
must trigger unified teardown. An intermediate command, test or CI failure is
not final failure. This slice establishes creation ownership; it does not yet
connect terminal outcomes, isolate all old writers or enable automatic teardown.
The opt-in [lifecycle barrier and finalizer](task-lifecycle.md) use a separate
fresh authority mode. The allocation protocol below applies only to fresh
format-3 isolated instances; legacy PR1 behavior is unchanged. These creation
tests are not deployment promises.

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

Isolated creation takes the queue lock, then nonblocking prospective lifecycle
EX, the existing nonblocking environment lock and the registry lock. Library callers of
`queue::task_action` retain its queue-lock requirement; the high-level entry owns
that transaction. No model or Herdr process starts during allocation.

### Fresh isolated allocation protocol

The trust root is an **already successfully initialized protected queue authority**.
This protocol does not repair an interrupted first `queue init-isolated`. Explicitly
configure `HERDR_TASK_ENV_ROOT` as `QUEUE/queue.json/task-environments`; an external
first control directory is unsupported. The fixed `queue.json/task-environments`
shape is reserved for managed control; a legacy directory using that shape is
also refused. No existing registry or previous authority
format is adopted or migrated, and configuration is never silently redirected.

1. Before any task directory is created, check that each requested target is absent
   and its existing trusted parent can receive an atomic move from this allocator.
   Compare device and mount IDs as well as the mount namespace. The first fixed
   `task-environments` child must be absent, including when an unknown empty
   directory is already there.
2. Save `lifecycle.control_bootstrap.reservation` in the existing queue authority:
   queue identity, the authority parent inode/mount and its fixed child name. Only
   then initialize that child, registry and env/plan/lock namespaces. This durable
   parent reservation owns the entire internal bootstrap subtree before its first
   `mkdirat`; it does not rely on a marker written afterwards. Sync the created
   directories and parents, then checkpoint their exact identities as `ready` in
   the queue. A missing/replaced ready control is an error, never a reinitialization.
3. Generate one instance and immutable `environment.creation_seed` in memory,
   including Owner, initial environment image, managed envs parent inode/mount,
   fixed per-task document slot and target-parent proofs. Save this seed in the
   queue as `creating` **before** creating its control document or task lock.
   Initialize only this reserved document slot. Sync its exact initial image and
   envelope; checkpoint `environment.bootstrap` with the envelope inode/mount.
   Partial initial bytes must be an exact prefix of that image. I/O errors are
   errors. Once bootstrap is known, the seed cannot recreate a missing or replaced
   document, including after the instance becomes active.
4. For workspace, temporary root and checkout separately, persist a fixed-slot
   transfer intent inside the task envelope. `mkdirat` the empty private slot,
   sync slot and envelope, then persist the slot inode. Use descriptor-anchored
   `renameat2(RENAME_NOREPLACE)` to its absent target. Sync **both** source and
   target parents, recheck parent/target identities, then persist the published
   root identity and clear the transfer record. A successful syscall alone is
   not the publication checkpoint.
5. Save `git_pending`, add the detached worktree without checkout, then record
   its exact Git identities. Inspect a successful add with a lost response instead
   of repeating it. Populate only missing files, validate the initial checkout,
   and save active environment and queue checkpoints.

An interrupted transfer resumes only when the expected inode is still in its slot
and the target is absent, or the slot is absent and that inode is at the target.
Both present, replacement, symlink, mount change or unexplained disappearance
refuse recovery. Terminal failure uses the same allocation intent: discard an
unpublished owned empty slot, or finish the known handoff's two-parent sync and
identity check before freezing cleanup. It never materializes business roots just
to delete them. Allocation scratch and the per-task control envelope disappear
through the existing finalizer, with the task's other temporary registrations.
The shared control skeleton remains queue infrastructure, not a task evidence archive.

Each **individual transfer** requires one mount. This does not require every
project or queue on one disk: each queue can use a managed allocator on its own
mount. This first implementation rejects a request when its targets cannot use
that allocator, before business resources are created. Even equal devices do not
prove equal mounts. `EXDEV`, `EOPNOTSUPP` or `ENOSYS` never trigger copy/delete or
direct-target-mkdir fallback; the original intent remains recoverable for cleanup.
Existing cross-mount cleanup protections remain in force.

The reserved private namespace grants ownership of its internal allocation
subtree. It does not claim to distinguish arbitrary same-UID replacement of an
as-yet-unfingerprinted empty slot from the original empty slot: those histories
have no distinguishable persistent identity. Unknown **user targets** never gain
ownership from a path, random slot name, marker or xattr. Known control and slot
identities must match exactly. This is a supported-writer protocol, not a general
sandbox against a same-UID actor modifying the authority itself.

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

Resume cannot change the source, roots or commit. In isolated mode it requires
the exact durable queue seed; no instance is created before that checkpoint.
It can complete its bootstrap, reconcile the same successful Git add, or finish
the active queue checkpoint after a lost reply. Repeated create is refused.
Already-active verification permits subsequent task commits while requiring the
same directory/Git objects. Replacement roots, symlinks, foreign Git links,
changed queue mappings or durable instance documents fail closed.

Legacy PR1 instances still have a mkdir-before-inode uncertainty window and
remain outside this fresh protocol. They are never migrated or retroactively
claimed. Partial Git administration that cannot be matched is also retained as
an explicit recovery error, not proof of release or an invitation for manual
approval to delete it.

The isolated allocation fixtures kill a real supervisor at control bootstrap,
initial payload writes, slot creation, inode persistence and transfer/sync
boundaries. They resume the same instance or commit final failure, then assert
root/Git/control/registry/plan/gate cleanup **before** fixture destruction.
Replacement and separate-mount refusals preserve foreign objects. These establish
process-crash behavior; filesystem cache loss, power failure, actual Agent/Herdr
integration and the full acceptance campaign remain separate unverified layers.

## Boundaries and retention

No Herdr workspace, terminal, Agent or native session is created by this entry.
Herdr is optional: only actual capabilities with verified ownership and release
may later be connected. This uses the existing task runtime for managed execution;
it does not build another runtime or depend on upstream Herdr changes.

New queue-bound instances cannot use legacy destroy/recover as an accidental
finalizer. `task-env status` reports `queue_instance_resume_only`; no final
outcome is invented. Legacy confirmed-delivery behavior is unchanged. These
legacy local refusals alone are **not** an old-binary writer barrier. Fresh
isolated mode uses the protected authority described in [task lifecycle](task-lifecycle.md);
production deployment and complete real-task acceptance remain separate.
No existing production workspace is adopted or deleted.

Creation journals and queue associations are task-temporary recovery data, not
formal deliverables or a permanent evidence archive. Unified finalization must
remove them when obligations are fulfilled. Only explicitly agreed delivery
targets may survive the task. See [task progress](task-progress.md),
[task runtime](task-runtime.md) and [delivery](delivery.md).

Tests use isolated Git repositories, fixture queues and a non-model child
process. They prove those local boundaries and fixture final-outcome teardown, not real
Agent/Herdr integration, real terminal shutdown or the 20-task acceptance campaign.
