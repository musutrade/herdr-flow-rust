# Task Environment Lifecycle (Issue #33)

`src/task_env/` implements the first of the six #32 implementation steps: every
task owns one disposable environment, formal deliveries live outside it, and
destruction releases the environment as a whole.

## Ownership model

An environment is identified by `task_id` and owns:

| Resource | Location | Destroyed |
| --- | --- | --- |
| workspace root | `workspace_root` | yes |
| temporary root | `temp_root` | yes |
| worktrees | inside `workspace_root` | yes |
| child processes | registered pids | terminated (with descendants) |
| files kept open | `open_files` | handles closed, files removed |
| delivery targets | `delivery_targets` | **never** |

Delivery targets are resolved with `util::resolve_path` and rejected when they
resolve inside either disposable root, so a symlink alias cannot smuggle a
delivery into the disposable region (or vice versa).

## Durable state

```
$HERDR_TASK_ENV_ROOT (default ~/.herdr/flow/task_environments)
├── registry.json          # live environments, guarded by registry.lock
├── registry.lock
├── <task-id>.json         # environment document
└── cleanup/<task-id>.json # cleanup plan, only while destruction is pending
```

All documents are written with `util::atomic_write_json` (private mode, fsync,
rename). `HERDR_TASK_ENV_ROOT` overrides the root; tests use it to stay out of
the real home directory.

## Lifecycle

`create_environment(task_id, workspace_root, delivery_targets, temp_root)`

1. validate `task_id` (`[A-Za-z0-9._-]`, no traversal) and resolve all paths;
2. reject delivery targets inside a disposable root, and overlapping roots;
3. reject an existing non-empty, non-directory or symlinked root — the
   environment only ever destroys what it created;
4. reserve a resource slot in `registry.json` and write the environment
   document **before** touching the filesystem;
5. materialise the roots; on failure remove what this attempt created and
   release the slot.

`restore_environment(task_id)` returns the stored document as-is. It never
allocates a second environment, including after a crash mid-create or
mid-destroy (`creating` / `destroying` are preserved).

`destroy_environment(&env)`

1. write the cleanup plan, mark the document `destroying`;
2. release file handles, terminate processes (descendants collected first,
   SIGTERM then SIGKILL, guarded by kernel start time against pid reuse);
3. remove worktrees that live inside `workspace_root` (`.git` contents are
   never trusted as a delete target);
4. remove registered files and then the disposable roots, using
   `symlink_metadata` walks that never chmod through a symlink;
5. only when everything is gone: drop the plan, the document and the slot.

A failure keeps the plan and the slot, marks the document `destroy_failed`, and
`cleanup_failed_environment(task_id)` resumes it. Retries are bounded by
`MAX_ATTEMPTS` (5), after which the environment is reported as stuck instead of
silently allocating a new one.

## Resource slots

`registry::occupied_slots()` counts live environments plus pending cleanup
plans. `registry::limit()` reads `HERDR_TASK_ENV_LIMIT` (default 32). A failed
cleanup therefore keeps consuming capacity; callers cannot escape a broken
environment by creating a fresh root. Registry mutation holds an exclusive
`flock`, so concurrent registrations cannot lose updates.

## Adapter use (implementation 3 continues this)

```rust
let env = create_environment("task-1", &ws, targets, &tmp)?;
// before writing anything:
validate_path(&env, &path)?;
// register everything the adapters create:
env.register_worktree(&worktree)?;
env.register_process_with_start(child.id() as i32, None)?;
env.register_file(&handle_path)?;
```

`is_disposable` / `is_delivery` classification and `ResourceBoundary` are the
contract adapters use to reject writes outside managed paths. Wiring Git,
command execution, Codex/OpenCode, and the "delivered" transition of
`progress.rs` into this API is implementation step 3/5 — this step only
provides the primitives and does not treat a bare destroy as business success.

## Tests

`tests/task_env_tests.rs` (18 tests) covers: round-trip create/destroy, restore
without duplication, delivery survival, symlink alias rejection, pre-existing
assets, delivery inside a root, protected permissions, forged `gitdir`,
descriptor release, descendant termination, invalid pids, failed-destroy slot
retention and retry, retry exhaustion, interrupted create, concurrent
registrations, task-id traversal, boundary validation, and idempotent destroy.

```bash
cargo test --test task_env_tests
cargo fmt --all -- --check
cargo clippy --all-targets --all-features --locked -- -D warnings
```
