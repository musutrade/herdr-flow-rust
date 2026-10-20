# Task Environment Lifecycle (Issue #33)

`src/task_env/` implements the first of the six #32 implementation steps: every
task owns one disposable environment, formal deliveries live outside it, and
destruction releases the environment as a whole.

## Ownership model

An environment is identified by `task_id` plus a random `instance_id`, and owns:

| Resource | Location | Destroyed |
| --- | --- | --- |
| workspace root | `workspace_root` | yes, identity-checked |
| temporary root | `temp_root` | yes, identity-checked |
| worktrees | inside `workspace_root` | removed from the recorded repo |
| child processes | processes carrying the environment marker | frozen, then killed |
| files kept open | `open_files` | handles closed, entries unlinked |
| delivery targets | `delivery_targets` | **never** |

Three facts make deletion safe:

1. **Instance identity.** A handle may only act on the environment document
   whose `instance_id` matches. A stale handle cannot release a newer
   environment that reused the task id.
2. **Root identity.** `(device, inode)` is recorded at creation. Cleanup
   re-checks it with `fstatat(AT_SYMLINK_NOFOLLOW)`; a root swapped for a
   symlink or a different directory is refused, and the slot stays occupied.
3. **Process marker.** Every process the environment starts carries
   `HERDR_TASK_ENV_INSTANCE=<instance_id>` in its environment. The marker is
   inherited across `fork`/`exec`, survives `setsid()`, and stays readable at
   `/proc/<pid>/environ` after the process is orphaned. Cleanup kills exactly
   the processes carrying the marker plus their current descendants, so it
   cannot hit an unrelated child or another environment's orphan, and another
   environment's bookkeeping cannot hide one of ours. Processes not started
   through `TaskEnvironment::command` are refused at registration.
4. **Per-task lock.** The live handle holds an exclusive `flock` on
   `locks/<task>.lock`. `restore_environment` and `cleanup_failed_environment`
   refuse to run while another handle owns the task, so descriptors and slots
   are only ever released by their owner. The handle is deliberately not
   `Clone`: a cloned snapshot could omit resources registered after the clone.

Delivery targets are resolved and must not overlap either disposable root in
either direction; this also closes the "protected directory contains the
workspace" case where cleanup used to report a false success.

## Durable state

```
$HERDR_TASK_ENV_ROOT (default ~/.herdr/flow/task_environments)
├── registry.json          # claimed roots, guarded by registry.lock
├── registry.lock
├── envs/<task-id>.json    # environment document
├── plans/<task-id>.json   # pending cleanup plan
└── locks/<task-id>.lock   # per-task ownership lock
```

Separate subdirectories mean a task named `registry` or `cleanup` cannot collide
with internal files. Documents are written with `util::atomic_write_json`.
`HERDR_TASK_ENV_ROOT` exists so tests never touch the real home directory.

## Lifecycle

`create_environment(task_id, workspace_root, delivery_targets, temp_root)`

1. take the per-task lock (one owner per task id);
2. validate the id and resolve all paths;
3. reject delivery targets that overlap a root, and roots that overlap;
4. reject an existing non-empty, non-directory or symlinked root — the
   environment only destroys what it created;
5. write the environment document (`creating`) **before** any index entry or
   directory, so a crash is always observable;
6. claim the roots in the index under `registry.lock` (unique ownership and
   `HERDR_TASK_ENV_LIMIT`, default 32);
7. create the roots (their parents must already exist) and record `(dev, ino)`;
8. mark the document `active`.

Any failure between 5 and 8 removes the document, the claim and the partial
directories, so an interrupted create leaves nothing unmanaged.

`restore_environment(task_id)` re-acquires the per-task lock and returns the
stored document unchanged (`creating` / `destroying` are preserved for
recovery). It fails while a live handle owns the task.

`destroy_environment(&env)`

1. verify the document's `instance_id` matches the handle;
2. write the cleanup plan and mark the document `destroying`;
3. close the environment's descriptors;
4. terminate processes: scan `/proc` for the marker, add their current
   descendants, freeze everything with `SIGSTOP`, then `SIGKILL`. Freezing
   first means a `SIGTERM` handler cannot spawn a late child, and the marker
   catches detached or orphaned descendants. A registered pid whose identity
   no longer matches is never signalled; an unmarked live pid keeps the slot
   instead of being guessed at;
5. remove worktrees through the repository recorded at registration and verify
   with `git worktree list`; a leftover registration keeps the item pending;
6. unlink registered files relative to an `O_NOFOLLOW` walk (a leaf swapped for
   a symlink loses only the link), then remove roots through a pinned
   `O_NOFOLLOW` directory descriptor whose `(dev, ino)` still matches. The tree
   is emptied with `unlinkat` relative to that descriptor. The final entry is
   renamed to an unpredictable quarantine name, its identity is verified there,
   and only then unlinked. A path swapped at the last moment is moved aside
   untouched, and any rename or replacement keeps the plan (and slot) alive
   instead of reporting a false success; directory modes are widened with
   `fchmod` only;
7. only when everything is gone: remove the plan, the document (if it is still
   this instance) and the index entry.

A failure keeps the plan, the document (`destroy_failed`) and the slot.
`cleanup_failed_environment(task_id)` resumes it, up to `MAX_ATTEMPTS`; items
whose resources are already gone are treated as complete, so a crash mid-retry
can finish. `interrupted_tasks()` / `recover_interrupted(task_id)` handle any
non-active document (`creating`, `destroying`, `destroy_failed`) or a leftover
plan. A root without a recorded identity is only reclaimed when it is empty and
no other live task claims it, so recovering an interrupted intent can never
delete another environment's data. The claim is taken under the registry lock,
making "unclaimed" and "now owned by the recovery" one atomic step.

## Resource slots

`occupied_slots()` counts environment documents plus pending cleanup plans;
`limit()` reads `HERDR_TASK_ENV_LIMIT`. A failed cleanup therefore keeps
consuming capacity. Registry mutation holds an exclusive `flock`, so concurrent
registrations cannot lose updates, and root claims prevent two tasks from
sharing an empty workspace.

## Adapter use (implementation 3 continues this)

```rust
let env = create_environment("task-1", &ws, targets, &tmp)?;
validate_path(&env, &path)?;                       // before writing
env.register_handle(&path, file)?;                 // transfer the descriptor
env.register_process(child.id() as i32)?;          // child must lead its own pgroup
env.register_worktree(&worktree)?;                 // repo captured from .git
```

Registered processes must be spawned in their own session/process group
(`setsid`, as `util::process::spawn_with_ready` already does); registration of a
process sharing the supervisor's group is refused. Destruction is forceful: the
group is frozen before any signal, so graceful shutdown handlers do not run.
That trade-off is deliberate, because a handler that spawns a detached child
would otherwise outlive the environment.

Wiring Git, command execution, Codex/OpenCode and the "delivered" transition of
`progress.rs` into this API is implementation step 3/5 — this step provides the
primitives and does not treat a bare destroy as business success.

## Tests

`tests/task_env_tests.rs` (37 tests) covers the round trip, delivery survival,
both overlap directions, symlink alias rejection, pre-existing assets, shared
empty root rejection, protected permissions with symlink and hard link, swapped
root and swapped file symlinks, forged `gitdir`, real `git worktree` cleanup,
descriptor ownership, stale-handle isolation, early-exit and SIGTERM-spawned
descendants, process-group registration, failed-destroy slot retention and
retry, retry exhaustion, interrupted create, interrupted-state recovery,
concurrent registrations, task-id traversal, internal-name shadowing, boundary
validation, intent recovery that must not touch another task's root, recovery
from a `destroying` state, retry after the root is already gone, and idempotent
destroy, descendants spawned after registration, unrelated supervisor children,
cross-environment orphan isolation, mismatched process identity, multi-root
claim merging, renamed-root slot retention, and plan-only recovery.

```bash
cargo test --test task_env_tests
cargo fmt --all -- --check
cargo clippy --all-targets --all-features --locked -- -D warnings
```
