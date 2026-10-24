# Explicit V1 claim admission

New V1 registration stores `task.claim` with task schema 3. This is the V1
execution model with an admission contract, not V2 lineage or Gate activation.
Use coordinator `queue task-add` with an existing bound result wait. Unknown
claim kinds/versions/fields are errors. Admission evidence is an absolute path
recording a coordinator decision; its text is not an identity or capability.

## Task-level records

Independent work outside a lane's succession, including its owner task:

```json
{"version":1,"kind":"independent","admission_evidence":"/absolute/decision.json"}
```

First development root for an already configured lane:

```json
{"version":1,"kind":"root","lane":"lane-owner-task","previous":null,
 "admission_evidence":"/absolute/decision.json"}
```

Next root consuming a real accepted source:

```json
{"version":1,"kind":"root","lane":"lane-owner-task",
 "previous":{"task":"accepted-source","epoch":1,
   "candidate":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","receipt_id":"exact-id"},
 "admission_evidence":"/absolute/decision.json"}
```

Related review/commit/fixed-output handoff:

```json
{"version":1,"kind":"member","root":"development-root","root_epoch":1,
 "admission_evidence":"/absolute/decision.json"}
```

A member refers directly to a root and consumes nothing. The root's input
candidate can differ from its fixed-output member's candidate. Existing waits
and operations retain their own task/epoch/assignment/candidate/producer checks;
they do not carry independent claim authority or a new global chain identity.

The coordinator explicitly decides independence. Missing classification, a new
session or a new checkout never implies first-root admission. Code enforces
declared links; it cannot establish semantic independence if the authorized
coordinator misclassifies an issue. Independent tasks cannot be managed sources.

## Receipt consumption and parallelism

Only `batch-offer` creates acceptance receipts. A managed source must match its
root's lane; one root has at most one accepted source. All tasks of that root must
be at released blocked/completed checkpoints without writers, unapplied results
or unfinished dispatches. Historical accepted operations need matching applied
results. Successor registration and freeze/integration/publication recheck
same-root release, including work attached after intake acceptance.

Next-root registration verifies source task/epoch/full head/lane/receipt and
pending/active member provenance or retained landing. It writes the source's
`batch_delivery.claimed_by={task,epoch}` and next root on one staged copy under
the existing queue lock and outer atomic save. Both directions must agree.
Validation failures leave no new task/writer/consumption. The earlier registered
result wait may remain unused. A save error remains uncertain; readback alone
does not establish durability and never permits dispatch replay.

Capacity counts accepted pending members only. Full pending refuses a new
successor without consuming its receipt. This reserves no future capacity,
does not block a first independent root, cancel admitted work, or count active
CI/developers. `batch-offer` remains the final intake decision. Same-task repair
before intake and the existing lane's exact repair flow remain available.
After intake, same-root tasks cannot restart development, create another source,
or publish an independent PR. ACK, blocking, cancellation, writer release and
completion are separate from new execution.

Cancellation does not refund a receipt. Cancelled accepted members still count
and prevent freezing. Cancelled successors retain `claimed_by`; no automatic
replacement is authorized. Freeze/landing retain receipt/marker, and history
truncation does not mint another authorization.

## Compatibility and deployment

Existing tasks without claims keep their completion behavior and are never
backfilled. Even an old A1 receipt cannot authorize a new root without a managed
claim. Every new V1 registration requires a claim, including tasks following
legacy work. Existing V2 activities keep their contracts; they cannot adopt a
V1 claim or authorize a new root. Claim tasks cannot migrate or discard admission.

Schema 3 makes old `progress::task` paths reject new tasks. **It does not make an
old binary safe to run against this queue.** Old queue loaders/watchers have no
queue-wide writer-version barrier and could register legacy tasks or save
observations. Before production use, inventory and switch every queue writer to
the supporting binary. Do not mix versions or roll back a claim queue to an old
writer. This is a deployment prerequisite, not a new supervisor or migration.

### Deployment writer-selection checklist

Complete this checklist for the exact queue before admitting claim tasks. A
version string or a successful build alone does not identify the running code.

- Record the queue ID/path and the chosen executable's absolute path, SHA-256,
  source commit and build profile. Verify the running watcher executable as well
  as the file on disk; record its PID and process start identity.
- Inventory every current and registered future writer: queue/watch lifecycle,
  coordinator task/PR/batch commands (including observation commands that save
  queue state), ACK/task-apply/resolve, producer signal/results and queue wrappers.
  Include scripts, skills, shell/PATH selection and service launch entries.
- Give every entry the same verified compatible binary selection. Record the
  coordinator's exact receiver session and each wait's actual producer identity;
  selecting a binary does not authorize borrowing another agent's session.
- Quiesce known writers before switching. Preserve pending results and ambiguous
  submissions, confirm the old watcher exits, then inspect the queue/watch locks.
  Locks serialize cooperating writers; they do not exclude an old same-user CLI.
  Use the [receiver maintenance procedure](receiver-maintenance.md) for process
  pause and snapshots; terminal `queue stop` is not a maintenance switch.
- Resume only verified entries and check receiver binding, fresh watcher health
  and readiness. Record any unaccounted writer as a deployment gap. A healthy
  receiver does not prove that all short-lived or future writers were switched.
- After claim registration or receipt consumption, never restore an old queue
  snapshot or select an old writer as a shortcut. A snapshot can lose
  `claimed_by` and make a consumed receipt appear reusable. Preserve evidence and
  inspect external effects and current state; there is no automatic semantic
  rollback or migration contract.

This inventory is an operational constraint, not a product-enforced global
writer barrier. Keep shared caches, unrelated sessions and host configuration
outside this task's resource ownership.

This code does not create a live lane, attest consumer Gate/App configuration,
or establish #112 bounded-parallel production acceptance. Those still require
authorized rollout and evidence. The coordinator/filesystem trust boundary is
unchanged.
