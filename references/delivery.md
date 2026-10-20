# Correct delivery and disposable completion

A version 2 requirement is finished only after the current accepted artifact has
reached the agreed external endpoint **and** its task environment has been
released. An independent pass, an execution exit, an ACK, a Git push, and successful
cleanup are separate facts. Legacy `complete` still completes an assignment; it
cannot complete a version 2 requirement's environment.

## Ownership and customer executors

Create the environment with its formal target outside the disposable roots. Use
the environment ID as the requirement task ID. Put native producer/coordinator
sessions, guard/CI monitors, logs, source copies, build output and private caches
inside that environment from the start; use the task-owned runtime launcher so
process markers and default storage follow the environment. Shared native sessions
cannot provide task-specific transcript deletion and are refused. The lifecycle
supervisor runs outside the task process group and logs to the task-owned root;
it must survive terminating the coordinator and its descendants.

`delivery::Executor` is the customer boundary. `preflight` verifies project
applicability and existing policy, `execute` performs the authorized action, and
`observe` independently checks the actual object and endpoint. Only observe may
return `Delivered(receipt)`; do not implement it by echoing an input JSON status or
trusting an exit code. An unavailable query is `Unknown`, not proof of absence.
The core guarantees intent/binding/recovery, not facts that an arbitrary adapter
merely asserts. Custom executors must reconcile the same saved operation ID and
prove that any `Absent` result excludes an in-flight operation before allowing a
retry. No automatic approval, rerun or escalation is introduced.

Two concrete executors are included:

- `file`: copy a file pinned by SHA-256 to a predeclared external file, using
  exclusive creation and file/directory sync. Observe hashes the actual regular
  file and rejects symbolic links. A partial/wrong target fails; it is not
  overwritten automatically.
- `git`: push the accepted full commit SHA to an explicit `refs/heads/...` in a
  predeclared local Git repository. The required `expected` full SHA supplies an
  exact lease for the old target ref. Observe reads the actual target ref. Moving
  source HEAD cannot replace the accepted commit. An unexpected target commit is
  unknown and requires inspection. Push and hook processes carry the task and
  operation markers; an in-flight process prevents an absence result. The Git
  child also exits if its supervisor dies before/during execution. These commands
  use real Git, not a simulated
  publication result. Git hooks/configuration remain customer policy; existing hooks are not
  disabled. The writer inherits them but confines writes to the two task roots
  and the declared formal repository, with TMPDIR in the task root. A hook that
  needs other writable locations fails closed; it cannot leak task files there.

For existing PR/batch customers, keep the current publication intent, CI inventory,
trusted Gate verifier and merge transport. They must finish their existing checks;
the delivery layer does not create a second push/merge intent. Use
`delivery::publication::verify` in the customer executor's observe method after
that path reports landing. Pass the live queue snapshot so dependency and lane
provenance remains available during reconciliation. It checks the direct accepted head or the reconciled
batch-member mapping, requires the existing PR/batch/lane completion prerequisites,
and reuses `pr::reconcile` GETs to verify the exact landing remains on the target.
Its returned receipt can confirm delivery. No GitHub requirement is added to the
file path. The bridge test uses injected GitHub GET responses; it is not a live
GitHub publication test.

## Durable protocol

The library functions take an exclusively owned `TaskEnvironment`. The CLI uses
`restore_environment` to acquire that ownership:

```text
herdr-flow delivery prepare --task TASK --queue /absolute/queue --spec /owned/spec.json
herdr-flow delivery execute --task TASK
herdr-flow delivery reconcile --task TASK
herdr-flow delivery retry --task TASK
herdr-flow delivery finish --task TASK
```

The CLI selects the concrete local file/Git executor. Custom customer executors use
the library interface. A file specification is:

```json
{"adapter":"file","target":"/formal/result.txt","authorization":"deliver","state_version":8}
```

The current requirement must already contain authorization `deliver` with action
`delivery` and its original source. The parent must have the current independent
passed verdict and delivery continuation. The target must equal a target registered
at environment creation. A Git specification also has `ref` and `expected`.

Preparation verifies the real source and saves the approval, exact object, target,
endpoint, authorization reference and unique intent in the environment document.
It then freezes the requirement under the normal queue lock. If interrupted between
those writes, repeating prepare with the identical request completes the freeze
without executing anything. Revision/candidate changes and ordinary control resume
cannot reopen a frozen delivery. Existing PR/batch and writer prerequisites apply.

Execute persists `unknown` **before** submitting once. Both successful and failed
transport returns are followed by observation. It returns the durable outcome to
the calling coordinator: `confirmed`, `failed`, `unknown` or `absent`. A crash or
unknown result must use reconcile, which only queries existing execution. Execute
cannot be called again. Explicit retry requires `absent` plus another fresh
absence query; it retains the same intent. A file mismatch, changed remote ref,
missing authority or stale approval cannot enter successful cleanup.

Once confirmed, finish records `cleaning`; neither it nor reconcile can execute
publication. A later restart resumes only cleanup. If cleanup fails, delivery stays
confirmed/cleaning, the slot remains occupied and the coordinator must resolve the
specific resource failure. It must not create a substitute environment or report
that the already successful delivery failed.

## Shared records and destruction

Finish first marks the environment destroying and stops marked task processes,
including native servers, brokers, monitors and their descendants. This ends the
dedicated sessions and closes their file descriptors. It then removes the parent,
linked activities, waits, events and result sidecars from its queue. Mixed
notification sidecars retain only unrelated events; emptied deliveries disappear.
A shared metadata guard's matching external-delivery pointer is cleared under its
own lock. A receiver owned by this task is detached. No whole-queue backup is made.
The small removal-ID manifest is saved before changing any shared files so a crash
between sidecar changes and the queue commit is recoverable.

Shared batch/lane records that still reference this task are rejected before
publication (and checked again before cleanup). They cannot silently retain a
completed task's evidence. Use a queue/lane wholly owned by the environment for
those paths, or reconcile their resource ownership before preparing delivery.
Do not declare a shared native conversation disposable by deleting only its guard
metadata. Shared services are not stopped or recursively deleted.

After shared removal is durably recorded, the existing descriptor-pinned
whole-environment destructor releases worktree registrations, files and both
roots. A partially removed environment retries the saved destruction plan; it does
not need the already removed queue. Completion is returned only after destruction
succeeds. The environment document, cleanup plan, registry entry and per-task lock
file are removed. Task-lock opening and lock-file removal share the registry lock
to prevent an unlinked-lock ownership race. No completed-task summary, report,
evidence index, archive, retained receipt or extra delivery copy is written.
Permanent shared infrastructure locks and an empty registry are not task records.
A plan-only interruption can be resumed with `delivery finish` only if the minimal
plan attests confirmed delivery. Repeating finish after all state is gone reports
`absent` / `not_attested`: absence alone cannot prove a historical delivery.

Tests exercise real local files and Git repositories, a live task-owned subprocess,
private session/cache files, shared queue/guard cleanup, stale identities, existing
PR/batch prerequisites, uncertainty and restart boundaries. Native agent/model
end-to-end completion is part of #38; these protocol fixtures do not claim an
autonomous agent coordinator or a new real model session.
