# Correct delivery and disposable completion

## Stage delivery within the same requirement (2026-10-27)

An explicit `task-lineage` action with `kind: "stage"` selects existing requirement
criterion IDs and records an ID, scope description and monotonically increasing
scope epoch. It cannot add authorization or change the selected criteria. See
[stage selection](task-lineage.md#stage-selection-and-fresh-acceptance). Development
and independent acceptance inherit the selected criteria. Native input also
includes the complete original requirement as `overall_requirement`.

Preparation pins that stage alongside its actual artifact, independent approval
and existing authorization. Execution and observation use the existing delivery
protocol below; an ACK or a successful push exit alone still cannot confirm it.
Once the adapter has observed a nonempty receipt, continue with:

```text
herdr-flow delivery stage-complete --task TASK --queue /absolute/queue --spec /owned/stage-complete.json
```

The spec is `{"delivery":"EXACT_CONFIRMED_INTENT_ID"}`. The isolated coordinator
can use `herdr_queue` action `stage-complete` with the same spec. It holds the
original lifecycle permit and environment ownership, verifies the frozen parent
and exact stage approval, and requires linked activities to have applied terminal
executions (`complete` or known `failed`), released writers, no pending results and
no uncertain operations. A settled failed execution does not pin future stages;
its failure is not acceptance evidence. Unknown decision events remain blocked.

A queue checkpoint containing the full confirmed intent is saved before clearing
the environment journal. Only then is the parent reopened, its scope epoch and
acceptance round advanced, and its stage, candidate, approval and continuation
cleared. After interruption, the same delivery ID resumes only these state writes;
it never invokes an executor. Repeating the last completed ID returns its receipt
without changing a newer stage. Only the latest stage delivery checkpoint is kept.

The task, instance, workspace and authority remain the same. A new stage, or the
full requirement after leaving a stage, must select a candidate and obtain fresh
independent acceptance. Earlier stage results cannot regain currency when the
stage field becomes empty. Explicit evidence reuse retains the existing review
rules; it is not automatic approval. `finish` and terminal success reject stage
delivery. Final success requires full-scope acceptance and confirmed non-stage
delivery, followed by the existing terminal cleanup of all linked resources.

Regression coverage includes two actual local Git stage deliveries, both persisted
prefixes of the stage-completion checkpoint, stale/unknown/mismatched rejection,
CLI recovery and an isolated coordinator/broker/watcher workflow through fresh
independent review, final delivery and cleanup. These use synthetic native model
protocols; they add no live-model/Herdr acceptance claim. Early per-stage resource
reclamation, remote PR/CI integration, a publishing role and asynchronous adapter
execution remain follow-up work. Existing activity/artifact/history capacities
still apply; this increment does not promise unbounded stages.

## Queue availability during external delivery (2026-10-27)

The #130 / #112 continuation first removes external work from the shared queue
transaction. Preparation snapshots the queue under its lock, then verifies the
artifact and runs the read-only adapter preflight without that lock. Before
saving an intent, it reloads the queue, requires the same parent and queue identity,
and rechecks cleanup references. Updates to unrelated tasks are preserved.

Execution performs its read-only checks outside the queue transaction. It then
revalidates the current approval, version, authorization, queue/control state and
PR/batch/writer prerequisites, freezes that same parent, and persists `unknown`
before releasing the lock and submitting. Adapter execution and observation do
not hold the queue lock. The environment handle and instance lifecycle permit
still exclude another writer or terminal cleanup of this operation. A crash after
reservation leaves the original intent for observation; it does not authorize a
second submission. An ordinary control resume cannot reopen the frozen parent.

Four new regression tests cover adapter calls, concurrent changes during both
preparation and execution, and an actual Git push paused in a private receive
hook. While that hook is held, another queue transaction updates an unrelated
task, the saved delivery remains `unknown`, a second environment handle is
refused, and the running Git process prevents a claim of absence. Releasing the
hook confirms the accepted commit after one submission, followed by normal
cleanup. The delivery suite passes **27 tests**. The initial hook test incorrectly
expected the isolated environment's error kind from a legacy fixture; its
assertion now checks the existing exclusive-handle refusal.

This increment does not make the synchronous caller asynchronous: a queue watcher
invoking delivery still waits for its adapter call. Independent observation and
other queue transactions can proceed. A publishing role, remote PR/CI integration,
and early reclamation of unreferenced stage resources remain separate work.
Stage delivery is described above. There is no new live model
or Herdr acceptance evidence in these protocol/local Git tests.

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
whole-environment destructor releases registered files, worktree registrations
and both roots, in that order. Files within a worktree are unlinked before Git
removes their parent directories. The destructor and cleanup-plan retry first
require a successful process stage. Both the registered parent and its recorded
descendants remain cleanup obligations, even after the parent exits. The same
check gates quiescence before shared queue records are removed. If signalling
fails, a known process cannot be observed reliably, or one of those recorded
processes is still live without its marker,
they preserve pending worktrees, files and roots along with the plan and
occupied slot. They do not unlink those resources beneath the surviving process.
Local supervisor file handles may already be closed; that is separate from path
removal. After the process obstruction is resolved, retry resumes the same plan.
This ordering does not make marker-based discovery exhaustive or grant authority
to signal an unmarked process.

Recorded liveness reads state and start time from one stat observation; a
known mismatch is not that recorded process. Legacy records without a start
time conservatively block while that PID is observed live, without authorizing
a signal. Read errors and malformed identities propagate through target
filtering, signal checks, survivor verification and registered-family checks.
A missing stat path counts as absence only when `pidfd_open` reports `ESRCH`;
a live handle or another syscall error blocks cleanup. There is no numeric-PID
signal fallback. Public boolean liveness helpers remain best-effort diagnostics
and are not used as proof of exit in cleanup.

The supervisor's `direct_children` snapshot can include unrelated
tasks and is not used as an ownership or cleanup-blocking claim. Children absent
from registration and no longer marked remain outside this discovery guarantee.
Cleanup's marker/descendant scans and local delivery's running-operation scan
propagate errors opening or enumerating `/proc`; a failed or partial directory
scan cannot count as an empty scan. Cleanup preserves pending resources. Delivery
reconciliation records `unknown`, and an explicit retry's fresh observation
fails before changing the journal to `prepared` or invoking execution.
The public diagnostic marker list and registration snapshot remain best effort.
Individual PID marker/parent visibility remains best effort too: an unreadable
unrelated process is not ownership evidence, and these checks do not establish
exhaustive discovery of all task processes.

A partially removed environment retries the saved destruction plan; it does
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
