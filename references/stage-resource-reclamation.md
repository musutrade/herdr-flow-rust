# Stage resource reclamation

This implements the next bounded slice of
[Issue #130](https://github.com/musutrade/herdr-flow-rust/issues/130): release
settled child execution resources after a confirmed stage delivery while the
original parent continues. It does not finish the parent requirement or replace
the [terminal finalizer](task-lifecycle.md).

## Selection and retained evidence

The stage checkpoint freezes a bounded plan in the existing environment
document before clearing the old scope. Eligible resources belong to ordinary
development or acceptance activities in that exact scope. Selection requires
the original wait and assignment binding, persisted signalled collection,
ACKed and applied result, bound worker creation, known session/turn, and a registered broker
PID with its start identity. A worker-local collection receipt preserves the
original collection proof when the coordinator prunes its operational journal.
The worker's `creation.json` must be `bound`; the separate upstream-session
journal remains `created`. A thread created without completing its queue binding
does not establish eligibility.

The plan may release the registered broker, its exact native presentation,
recorded upstream thread and proven Guardian descendants, and its unreferenced
Ready review clone. Review Owner, original wait, binding and directory identity
must match. Queue references, other runtime records and clone sources are
checked conservatively. The original coordinator, recovery and publication
roles, unknown creation outcomes, unsettled operations, and referenced clones
remain under the existing terminal cleanup obligation.

Runtime control directories, immutable reports, original result events and
deduplication records remain readable until parent finalization. In particular,
an applied result may still point at its runtime's `turn.json`. No record is
deleted merely because it belongs to an older scope.

## Watcher and recovery

Stage business completion records `stage_delivery.resource_cleanup`. Its phase
may remain `pending` after delivery is confirmed and the old scope is cleared.
New lineage consumers wait for cleanup; inspection, recovery and explicit final
outcomes remain available. The existing queue watcher drives cleanup before
ordinary control work and after terminal cleanup. It acquires the original
parent's nonblocking EX permit outside queue and environment locks, performs a
bounded step, and releases the permit between attempts. There is no new watcher
or per-runtime lease, and an SH operation never upgrades its own permit.

Before effects, the watcher rechecks the current presentation and frontend
identities, current Recovery incident, and original runtime. It persists the
plan and a retirement marker, captures the presentation's persistence identity,
closes the exact pane, and observes both recorded frontend processes gone. It
then verifies the thread identity and a quiescent upstream status (`idle` or
`notLoaded`, with no active flags). Active or uncertain native work retains the
resources; it is not interrupted to complete stage cleanup.

The broker stop uses only its exact registered PID/start identity. Each later
tick deletes at most one recorded upstream session. Deletion and descendant
inventory use five-second RPC budgets including connection setup; inventory
shares one deadline across scopes and pages. Descendants require the existing
parent/rollout proof, never a matching working directory alone. Clone removal
reuses descriptor verification and durable quarantine receipts. Identity
replacement, a new reference, or uncertainty leaves the original obligation
pending instead of adopting a replacement or repeating remote delivery.

Retirement markers prevent prepare, launch, broker restart, native requests and
presentation reopening for the old runtime. Completed original result waits
close with the checkpoint, so Recovery does not diagnose the planned broker
stop as a fault. The existing queue emits deduplicated host notices for cleanup
completion or a retained obligation. The coordinator ACKs these notices without
`task-apply`, inspects the same parent, and continues within its existing
authorization when cleanup completes.

Terminal finalization takes precedence. It absorbs frozen upstream obligations
and captured presentation identities, including a crash after pane closure but
before the ordinary journal update. If a presentation was reopened before the
retirement marker became durable, terminal cleanup preserves both verified
attachments. Stage cleanup never overwrites the new binding or silently drops
its cleanup obligation.

## Validation scope

Regression fixtures exercise protected format-3 owners, real broker processes,
actual Unix-socket RPC connections and disposable directories. They cover
selection, replacement and reference retention, uncertain deletion and exact
retry, lock contention, notification deduplication, retained evidence and
terminal takeover. Directory helper tests interrupt durable quarantine prefixes
and check that replacement resources remain untouched.

The ordinary staged coordinator fixture also traverses production native
creation, binding, dispatch, collection, ACK/application and stage completion.
It checks the resulting `bound` creation and collection receipt, early deletion
of the stage reviewer, ACK of the cleanup notice, and a distinct new session for
overall acceptance under the same parent. Its shared protocol service records
each thread's actual creation identity and rejects reused thread IDs.

These are protocol and process fixtures, not a real model or Herdr-visible
acceptance run, and not power-loss testing. Same-parent stage delivery evidence
from earlier work remains separate. Real-role early resource reclamation and
the human writer handoff remain acceptance work for the open lifecycle issues.
