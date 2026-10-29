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
recorded upstream thread and proven Guardian descendants. Review clones and
their original `review_workspaces` registrations remain until the parent's
existing terminal finalizer. The original coordinator, recovery and publication
roles, unknown creation outcomes and unsettled operations remain under that
terminal cleanup obligation.

Artifact registration can legally refer to an existing Git repository owned by
another task, including one in another queue. A queue snapshot and the original
parent's EX cannot prove that no other queue or future registration references
a review clone. This slice therefore does not rename, clear or unlink review
directories during stage retirement. It does not disable cross-task artifact
registration or introduce a cross-Owner pin protocol. Stage completion preserves
these sources; it does not extend their original disposable lifetime beyond
explicit parent finalization. Inputs that must outlive that parent need actual
delivery or materialization into an independently retained repository first.

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
plan and a retirement marker and captures the presentation's persistence identity.
Before closing the exact pane it requires the native bridge's durable request
ledger to be drained and verifies the thread identity and quiescent upstream
status (`idle` or `notLoaded`, with no active flags). It then observes both
recorded frontend processes gone and checks idle again before stopping the broker.
Active or uncertain native work keeps its existing presentation, including its
approval and input channel; it is not interrupted to complete stage cleanup.

The existing task TUI bridge participates in the same lifecycle SH/EX boundary.
It records admitted requests in the original attachment before buffering bytes
for upstream, and rejects new operations after the retirement marker without
closing the connection. Native approval/input responses and turn interruption
remain available. Temporary EX contention defers bounded raw input, retried by
the existing bridge loop even if no additional socket bytes arrive. A successful
turn/start or turn/steer response retains the exact turn until a matching native
completion; approval/input exchanges also retain their turn through completion.
Request IDs are typed and scoped to their original connection and direction.
Reconnects cannot erase or settle old unknown requests, and bounded completion
evidence handles a terminal event arriving before its request response.

Missing, changed or malformed drain evidence retains the presentation. This
includes a TUI launched by an older binary without this capability. Unknown
work-producing responses (including compact/start, shellCommand and review/start
without a supported turn proof) remain under the original parent's terminal
cleanup obligation. Only exact supported no-work RPCs can settle on their
response; even an unknown read/list method is retained. An acknowledged unknown
operation survives request-ID reuse or a later duplicate error. Such work is
not replayed or silently inferred complete from an idle snapshot.

The broker stop uses only its exact registered PID/start identity. Each later
tick deletes at most one recorded upstream session. Deletion and descendant
inventory use five-second RPC budgets including connection setup; inventory
shares one deadline across scopes and pages. Descendants require the existing
parent/rollout proof, never a matching working directory alone. Runtime,
presentation or frontend identity replacement and uncertain native operations
retain the original execution obligation instead of adopting a replacement or
repeating remote delivery. Directory destruction remains in the existing
terminal finalizer, with its descriptor and durable quarantine protections.

Retirement markers prevent prepare, launch, broker restart, new native RPC and
presentation reopening for the old runtime. An already-running broker keeps
observing and draining its original connection under the same parent Owner and
finalization gate, and can finish a prepared journal checkpoint. It may send only
an exact already-recorded completed tool result through that original connection.
Rejected new RPC requests do not terminate the observer. Even metadata RPC is
fenced because the subscription layer can otherwise resume a thread implicitly.
The watcher stops the broker only after the recorded frontends have exited and
a fresh native observation confirms idle. Completed original result waits
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
selection, execution identity replacement, source retention, uncertain deletion and exact
retry, lock contention, notification deduplication, retained evidence and
terminal takeover. Reference regressions register actual Git artifacts through
the coordinator's normal queue transaction before retirement, while the source
Owner EX is held at a fixed barrier, and after stage completion. Both same-queue
and cross-queue consumers retain valid Git objects and the original clone
registration. The original terminal cleanup tests continue to cover directory
and quarantine recovery.

A registered child process runs the real broker serve loop across the retirement
marker. Its regression covers checkpoint recovery, late native events, rejected
new RPC and broker restart, original-connection tool response, frontend exit,
active-work retention and the final idle-triggered broker stop. The original
sleep-process fixtures alone do not establish these broker-loop properties.

Native TUI regressions use the production task Policy and ProxyBridge with actual
Unix/WebSocket connections. They reproduce post-marker turn admission and early
pane closure, and cover buffered admission before retirement, approval/input
delivery on the original socket, contention deferral, exact completion and fresh
idle before pane closure. Separate ledger regressions cover reconnect ambiguity,
typed IDs, crossed request/response IDs, early completion and missing evidence.

The ordinary staged coordinator fixture also traverses production native
creation, binding, dispatch, collection, ACK/application and stage completion.
It checks the resulting `bound` creation and collection receipt, early deletion
of the stage reviewer, ACK of the cleanup notice, and a distinct new session for
overall acceptance under the same parent. Its shared protocol service records
each thread's actual creation identity and rejects reused thread IDs.

These regression fixtures are not real model/Herdr acceptance or power-loss
testing. The separate real-role evidence below does not expand their scope.

## Completed live slice (2026-10-29)

[PR #162](https://github.com/musutrade/herdr-flow-rust/pull/162) records candidate
`afa3cdc485f1a0bc42616d4ae9aed660412e4437`, merged as
`d2d63900cfc2446daa1e489fdc985d96ef8c4446`. Its fresh private five-role scenario
exited 0 in **403.41 seconds**. The original parent and later developer continued
after the settled stage developer/reviewer execution resources and proven
descendants were reclaimed. Review clones stayed available for final acceptance.
Stage candidate `7d88f2cdc119ce00b61d4a9e0d33cf72fd2f5b0c` and final candidate
`39fac55840cd69762504ec7121caec937340f022` received separate independent reviews.
Final local Git delivery contained only the agreed answer and explanation files;
the original parent then finalized successfully.

Independent final review passed the agreed five-role contract and the known
resource-release checks (report SHA-256
`50f7e326dacd92af04d0668248f6d6b61ba054ee921aa92aae5a02719d74ce4e`).
Fourteen PID/start identities, nine observed native session IDs, twenty complete
native inventories and nineteen queries across six databases showed no matching
residue. Shared assets remained intact.

An extra `final-dev-cleanup` activity removed an accidentally committed review
report before fresh final acceptance. Its exact native session/activity binding
and effective configuration were not fully captured and remain **ambiguous**.
The nine observed IDs' release does not prove a complete audit of all extra
executors. Three initial configuration comparisons also remain false: they read
pre-turn creation snapshots; independent review used sent parameters, exact turns
and applied records for the five contracted roles. The earlier 904.51, 608.46 and
194.93 second failed live attempts retain their failures. This supports the
bounded early execution-resource reclamation path, not early review-clone deletion
or exhaustive additional-role acceptance.
