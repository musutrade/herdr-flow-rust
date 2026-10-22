# End-to-end acceptance readiness (#38)

This checklist separates protocol evidence from real agent completion. #38
remains open. It does not replace its issue acceptance matrix or close #32.

## Current evidence and remaining work

| Boundary | Existing/current evidence | Still required for #38 |
| --- | --- | --- |
| Old task state | `tests/lineage.rs::legacy_migration_requires_quiescence_and_preserves_no_approval` refuses migration with a writer, pending operation, executing phase or open PR; preserves old task state and does not create a new approval. | Validate ownership association for actual pre-existing resources and undelivered artifacts; no home-directory sweep or inferred authority over unknown paths. |
| Delivery and restart | `tests/delivery.rs` covers real local file/Git endpoints, frozen candidates, uncertain delivery, restart and cleanup faults with protocol reviewer fixtures. | Run those transitions with actual native role output, including failure/repair and fresh acceptance sessions, not fabricated verdicts. |
| Repeated control state | The twenty-round protocol regression reuses one control directory and shared queue, alternating normal delivery, restore after preparation, and restore after confirmation; confirmed delivery cannot be executed again. | Twenty completed real tasks including repair and restart, with actual model takeover/ACK and customer-delivery evidence. The protocol regression does not count toward that real-task total. |
| Native execution | Opt-in installed tests and `tests/common/acceptance_live.rs` can run real native roles. Their coordinator and notification ACK/application are test-driver operations. | An actual coordinator must receive and act on the protocol, with model-generated actions distinguishable from driver actions. |
| Resource lifetime | Protocol regression checks task registration/slots, workspace/temp roots, plan/env/lock records, shared entries, previous deliveries and a pre-existing user asset before fixture teardown. | Observe native caches/sessions outside task roots, open files, process identities, worktree registrations and shared entries for every real round; check correct delivery plus complete task teardown. |
| Runtime compatibility | Local readiness review found Codex 0.163.0 and OpenCode 1.19.3; the existing opt-in inputs select binary/model/config/auth copies. | Execute exact selected configurations in disposable environments; do not infer model availability from binary versions or old successful provider calls. No OpenCode 2.0 upgrade. |
| Recovery/error matrix | Unit/protocol suites exercise timeout, late/partial results, unknown delivery and cleanup faults. | Real execution/approval/unavailable-environment and interruption boundaries, with no false completion, replay or unlimited environment creation. |

## Twenty-round protocol regression

```sh
CARGO_TARGET_DIR="$PWD/target" cargo test --locked --test delivery \
  twenty_protocol_deliveries_reuse_control_without_retained_task_state \
  -- --exact --nocapture
```

The test uses the same task name, control root and queue across all rounds.
It creates one new formal fixture output per round, checks every prior output
and the unrelated user asset, and calls cleanup assertions before the containing
`TempDir` is dropped. Thus per-round directory teardown cannot mask accumulation.
After removing only the queue's `updated_at`, shared state must equal the first
completed round. The non-delivery inventory compares paths, file lengths and
allocated filesystem blocks; queue content equality handles timestamp-width
variation without allowing task/history growth. Formal output paths are excluded
explicitly, not by a broad filename prefix.

The first local run observed 13 retained fixture entries and 88 filesystem blocks
in every round; these numbers are observations, not portable fixed thresholds.
The invariant is equality to that run's first completed round. Retained entries
belong to the intentionally preserved shared control/queue/user fixture, not to
completed tasks. Fixture review verdicts are synthetic, and no native model
session or process is created by this test. All fixture endpoints are disposable;
this is not evidence of customer production delivery or global cache cleanup.

## Real-run evidence boundaries

The real acceptance run must use task-owned roots and real sessions, preserve
user assets, distinguish native role actions from driver interventions, and
measure each round before moving on. A passing native response alone is not
independent acceptance, delivery or cleanup. Native approval remains pending
until actually handled under the existing authorization; the driver must not
press approval keys or manufacture an agent ACK.

Runtime configuration and credentials belong only in the selected disposable
native environments, never source, reports or commits. Existing working task
state must not be upgraded to a new passing verdict. During an unresolved run,
retain only the state needed for diagnosis/recovery; on completed task teardown,
remove task-owned transcripts, evidence, sessions and state as required by #38.
Repository development/CI failure logs are separate from those task-owned
resources and are retained for review under the ongoing development handoff.

No actual twenty-task result, autonomous coordinator, or fully upgraded legacy
resource set is claimed by this document. These remain explicit next acceptance
work rather than boxes inferred from unit-test counts.

## Owned production watcher bootstrap

`tests/task_runtime.rs::production_foreground_watchers_are_owned_and_reclaimed`
starts the production guard and queue watchers in foreground mode through
`TaskEnvironment::command`. The harness retains each `Child`, registers its
exact PID, and checks that production `watch_service::health` identifies that
child. It does not fabricate `watch.json`, acquire the watcher locks itself, or
recover a detached process by reading a PID file. Guard registration and the
queue enable path use the HTTP fixture's session binding. Enable retries only
`StateBusy`, with a 15-second deadline, because the real guard watcher can hold
its state lock during observation.

Environment destruction must terminate and reap the server and both watcher
children, remove workspace/temp roots and release the registry slot. This
validates the existing foreground lifecycle path without adding a new watcher
API. The HTTP peer is still synthetic; this regression proves neither native
model execution nor model-generated readiness/ACK. The real receiver test must
use these production watcher processes with a fresh native session and retain
native approval behavior.

## Opt-in native receiver receipt check

`tests/common/receiver_live.rs::installed_real_opencode_receiver_readiness_and_ack`
uses `session::create_fresh` and production foreground watchers. It requires
`HERDR_LIVE_OPENCODE_BINARY`, `HERDR_LIVE_OPENCODE_MODEL` (a JSON model object),
and `HERDR_LIVE_OPENCODE_CONFIG` (an explicitly selected config file). Run only
this ignored test with a worktree-local target. The driver creates the signal,
then reads state; it never calls queue `tick` or `acknowledge`. The native
receiver must execute the notification's ACK command, then readiness. Readiness
is intentionally false while a delivery is awaiting ACK, so reversing that
order would deadlock the check. The test also requires completed shell tool
records in the exact native session and a completed native submission. This
fixture creates exactly one signal/delivery and selects it by exact session;
multi-delivery acceptance would need an explicit event/delivery selection.

The initial local real run was blocked by an OpenCode `external_directory`
approval for the task-owned queue/guard outside cwd. It did not ACK and is not a
passing native acceptance result. Native permissions are inherited from the
selected configuration by default. An operator can explicitly authorize
`HERDR_LIVE_ALLOW_OWNED_EXTERNAL_DIR=1` for this disposable test: the copied
configuration then allows only the generated runtime root and its descendants
through `permission.external_directory`. Other permission rules remain intact;
scalar permission policies are rejected rather than silently rewritten. The
Rust test reads the selected config file but never writes it; each external live
runner additionally compares the selected file bytes before and after the run.
The test never clicks approval prompts. Both queue and guard reside inside the
runtime root because native launch permits writes only there and in cwd. Placing them in a
sibling temporary directory is outside that kernel-enforced boundary even when
OpenCode's directory approval permits access. Bounded local failure diagnostics are retained separately from
the disposable environment, excluding the selected config/auth files. Passing
this one receipt test would still not prove twenty real tasks, independent
review/repair, customer delivery or the complete external-cache inventory.

## Legacy queue with an existing registered environment

`tests/legacy_resources.rs` persists a legacy queue task and a real undelivered
file in an already-registered environment. It checks that invalid migration
requirements leave the queue bytes, registry, environment document, root identities
and file intact. Successful queue migration also leaves those resources unchanged:
resource references in the legacy record are opaque to migration. A separate
restore check rejects concurrent ownership, then retains the registered instance,
roots and state-file identity after the original handle is dropped. The new
requirement explicitly receives caller-supplied authorization from the existing
user agreement; migration does not infer it from the preserved old record. Its
endpoint matches the registered formal output, while acceptance evidence,
candidate and delivery target remain unset. A path to an unrelated
user asset in the old record remains outside the disposable boundary. An explicit
fixture copy followed by environment destruction preserves that user asset and
the registered formal output while removing the owned roots and registry entry.

This covers legacy queue records referring to a current, positively identified
environment registration. It does not import arbitrary historical environment
schemas or adopt unregistered resources. The fixture copy is not product delivery
or independent acceptance, and no native session is used. Ownership that cannot
be established from actual registration/recovery evidence remains unresolved.

After explicit operator authorization of the scoped runtime-directory permission,
the native receipt check passed locally on OpenCode 1.19.3. It observed real
completed ACK/readiness shell calls in the exact session, matched the persistent
delivery receipt and readiness identity, then reaped the server/watchers and
checked absent owned roots and zero occupied slots. The external live runners
checked the selected file's bytes before and after; the Rust test only validates
the disposable copied configuration. This is one receipt check, not twenty real
tasks or independent acceptance/customer delivery.

The earlier runs remain failure evidence: one driver supplied the wrong model
field, two runs hit the kernel write boundary with queue/guard outside the runtime,
and one observed a real ACK but timed out on a transient top-level queue status.
The final assertion uses the delivery's persistent phase/session plus readiness's
matching receipt; a watcher advancing to `waiting_for_events` cannot erase the
receipt. This corrects the test without weakening production confinement.
Writable queue/guard are disposable coordinator-owned test state; production
isolation policy is unchanged.

## Native developer result application

The opt-in `installed_real_opencode_developer_result_is_applied_by_native_receiver`
regression uses distinct real developer and coordinator sessions. The driver
registers a disposable task, launches the developer, collects its real Git output,
and stops/reaps that server. Production watchers notify the coordinator, which
must ACK and invoke `queue task-apply` itself. The driver never ACKs or applies
that result. Native completed tool records, persistent receipt identity, the
complete applied artifact object, and the exact event/result/producer-report
chain are checked; the parent must remain unapproved.

The explicit directory opt-in additionally permits the exact disposable worker
runtime and two fixture checkouts so the coordinator can inspect actual evidence.
It grants no access to operator repositories. Production native confinement is
unchanged: the coordinator's writable roots remain its own runtime and cwd.
The test does not add a new negative write probe for sibling directories.

Queue watcher health is required before dispatch. Driver preflight retries are
bounded and stop once the corresponding creation/dispatch journal exists.
Inside OpenCode runtime dispatch, queue-lock contention after `dispatch.json`
is retried only while `submission.json` is demonstrably absent. The adapter
writes that submission journal before its single native POST. Existing journals,
other errors and exhausted retry budgets stop without replay; the dispatch
marker is never deleted to authorize another attempt.

This is a developer-result/application boundary check, not independent acceptance,
customer delivery, a real repair cycle, or twenty completed real tasks. Previous
failures remain retained, including the fixed fixture clock and report-path
assertion mistakes and native approval stops.

The final local OpenCode 1.19.3 run passed this single scenario in 45.26 seconds,
including server/watcher reaping, absent owned roots and zero occupied slots.
The external runner also verified the selected configuration file was unchanged
byte-for-byte. Earlier failed runs are not counted as successful scenarios.

## Native independent acceptance application

The opt-in `installed_real_opencode_independent_acceptance_is_applied_by_native_receiver`
regression extends the real developer application scenario with a separate native
reviewer snapshot/session. The driver selects the collected candidate and registers
acceptance under the queue lock, releases it, and launches/collects the reviewer.
The same native coordinator receives two production notifications and executes
ACK, task-apply and readiness for each. The driver never ticks, acknowledges or
applies either result. The reviewer writes the actual `.herdr-verdict.json`; the
coordinator names its immutable registered verdict and chooses the continuation
from its conclusion. A non-passing report cannot satisfy this passing scenario.

Assertions bind both events and deliveries, all three sessions, candidate,
collector report chain, immutable reviewer envelope, approval round/verdict and
planned continuation. Development alone leaves approval null. The second-stage
check requires exactly two acknowledged/applied events and two exact-session
deliveries, an empty acceptance pending list and real native tool invocations.
Teardown removes owned roots and reaps watchers/server with zero occupied slots.
The selected configuration file is checked unchanged by the external live runner;
only disposable configuration copies receive the existing scoped permission opt-in.

This is an acceptance-to-delivery **continuation**, not an executed customer
delivery. It does not establish repair/restart, twenty completed real tasks,
Codex-native equivalence or a complete inventory of resources outside task roots.
#38 and #32 remain open. Native directory approval blocks remain evidence of a
blocked run; the fixture does not press approval keys or widen permissions to
let a worker read a different role's instructions.

The first-stage single-delivery check is valid because acceptance is registered
only after development application completes. The final exact-two-events check
intentionally rejects extra watcher events. Final local live3 passed in 105.81s;
live1 blocked after the developer searched sibling coordinator instructions and
was retained, while live2 passed before the final review assertions were added.
No permission widening was used to resolve live1.

## Accepted native result to external Local Git delivery

The opt-in delivery scenario builds on the final real developer/reviewer/coordinator
acceptance run. Its environment task id is the queue parent (`project`), and its
formal Git target is registered at environment creation. A bare target is seeded
at the source repository's base commit before native work; a separate user asset
remains outside both disposable roots.

The native coordinator owns both ACK/task-apply notifications and the approval
continuation. It does not execute delivery and cannot write the formal target
because its write boundary remains its runtime root and cwd. After the acceptance
application, the queue watcher is stopped and reaped. A trusted unmarked driver
then owns the environment and invokes `delivery::prepare`, `delivery::execute`,
and `delivery::finish` with `local::Local`. Preparation does not change the target;
execution must produce the exact accepted commit at `refs/heads/main` with an exact
receipt; finish removes the owned roots, registry/state and queue task while
preserving the target and unrelated user asset. The test does not call
`destroy_environment` after finish.

This is a narrow external-driver/existing-API boundary proof. It does not claim
that a native coordinator can execute customer delivery, that a driver can retain
environment ownership while a CLI restores it, or that the fixture covers
repair/restart, twenty tasks, Codex equivalence or a complete outside-root
inventory. Failure evidence and prior live/scenario failures are retained.

Final local delivery live1 passed in 95.52s with the actual native roles and Local
Git executor. The selected configuration file remained byte-for-byte unchanged
under the external runner. The formal ref and version B content survived finish;
the unrelated asset retained its bytes and device/inode identity. This one run
does not establish twenty completed tasks, repair/restart or Codex equivalence.

## Native failed review, repair and fresh acceptance

The opt-in `installed_real_opencode_failed_review_is_repaired_and_delivered_by_external_driver`
scenario uses the same native coordinator session for four notifications: development `review`, failed acceptance `accept-first`,
development `repair-native`, and fresh acceptance `accept-final`. The driver only
registers work under the queue lock and runs native workers; every ACK, verdict
application and continuation remains native-coordinator work.

`accept-first` records a real grounded failure and an immutable verdict. The
driver then registers the recorded repair continuation with the failed gaps'
activity/ids/evidence, using the incomplete candidate as input. The coordinator
reads the actual bounded per-development release receipt; no static release
status is supplied. The repair worker produces and
commits B; the coordinator applies that development output. The driver selects B
and registers `accept-final` on a separate reviewer snapshot. The fresh reviewer
must resolve the exact prior gap with `activity`, `gap`, `evidence` and
`explanation`; the coordinator applies the real passed verdict. The old A artifact
and failed verdict remain historical, while B becomes the current candidate and
approved artifact. Four exact events/deliveries and coordinator command records
are checked; no driver ticks, ACKs or applies.

After final approval, the queue watcher is stopped/reaped and the external Local
Git delivery/finish path runs. The formal target and unrelated user asset must be
unchanged through failure, repair and final acceptance, then exactly B is the
confirmed formal ref/receipt with the user asset preserved. Finish removes owned
roots/registry/state/queue and reaps processes. This remains a disposable fixture
proof, not customer publication, repair/restart breadth or twenty-task acceptance.

Final local repair live2 passed in 267.08s, including applied release evidence
matching each actual stopped/reaped worker receipt and its session, all gap
records/resolutions, and unchanged failed report bytes. Live1 passed in 196.22s
before the release-evidence assertions were added; final evidence uses live2.
The selected global configuration bytes were unchanged under both external
runners. No product isolation or permission policy was changed.

## Confirmed delivery external-supervisor restart

The opt-in restart scenario reuses the real native repair/approval flow and stops
the queue watcher before delivery. The trusted driver prepares and executes the
Local Git delivery exactly once to `confirmed`, then transfers custody through a
test-only handoff guard instead of dropping an environment that would be
destroyed. The inner ownership lock is released; the guard suppresses normal
destructive Drop only for the handoff window and can restore/finish a confirmed
environment or resume its cleanup plan if the handoff fails. Cleanup errors are
reported; the guard does not erase state through a fallback destroy. No unsafe
operation or `mem::forget` is used.

Fresh unmarked CLI processes restore the environment and run delivery
`reconcile`, then `finish`; neither process runs `execute` again. Confirmed
reconcile returns the persisted intent/receipt, while the test independently
checks the formal Git target B and the user asset dev/inode/bytes before and
after the handoff. The same single intent and receipt must survive, and finish
must remove owned roots, registry/state, queue task and slot while preserving the
target and user asset. Server/guard children are quiesced by finish and reaped by
the parent; child failures are killed/reaped before fallback restore/finish.
This is a disposable local delivery restart proof, not a general crash-recovery
guarantee, supervisor daemon, or twenty-task acceptance.

The handoff guard starts after confirmation succeeds. An assertion failure during
prepare/execute, before custody transfer, still uses the fixture's ordinary
environment destruction; it does not prove delivery-specific recovery of a
frozen queue or partial delivery journal. Native child waits require successful
reaping, not a zero exit code: cleanup may terminate those processes. CLI
reconcile/finish must both exit successfully. Cleanup diagnostics are bounded;
the enclosing disposable fixture is not a durable recovery archive.

Local verification on 2026-10-21 passed the opt-in confirmed restart scenario
(1/1, 228.44s): four native notifications, actual failed review and repair,
fresh passing review, one confirmed Local Git delivery, fresh CLI reconcile and
finish, exact persisted intent/receipt, preserved target/user asset, and empty
owned roots/registry/queue/slots before fixture teardown. The selected OpenCode
config bytes were unchanged. Stable coverage and Rust 1.85 all-target tests each
passed 473 tests with 11 ignored; the absolute CRAP threshold of 10 had no
violations. These results do not close #38 or #32.

## Bounded resource inventory collector

`tests/common/resource_inventory.rs` is a test-only, metadata-only collector for a
stable campaign base. It observes task identity, owned/runtime roots, registry,
environment state and cleanup plans, slots, registered worktrees/open files,
marked process pid/start/cwd/fd targets, shared-queue ownership counts, explicit
exclusions, scoped base file/byte/allocated-block counts and a noisy filesystem
free-byte observation. It reads queue structure to classify ownership, but does
not capture file payloads, configuration, auth or credentials. It does not create,
write, chmod, rename or delete observed resources; reads may update access times,
so atime stability is outside this assertion. Registry/env/plan roots are supplied explicitly and read
with non-creating metadata/JSON access; missing directories are not created or
chmodded. Queue observations distinguish owned, unrelated, mixed and unknown
entries, and malformed or missing references are errors/unknown, not unrelated.
Base absence and missing explicit worktree repositories are reported errors.

The synthetic two-round test in `tests/resource_inventory.rs` reuses one campaign
base and shared queue, creates a real marked child with an outside open fd,
registers a real Git worktree and open file, and excludes exact formal/user/config
paths. It checks nonempty observations before cleanup, empty owned roots/processes/
worktrees/open files and zero slots afterwards, retained unrelated queue entries,
and a stable base between rounds. Unknown outside FD/cwd paths observed on the
sampled marked processes are reported, never removed. Base usage is a lexical,
no-follow inventory: symlink targets (including a symlinked base) are not traversed
or inventoried unless separately supplied. Symlink counts are not proof that their
targets are empty. This is collector validation only: `/proc` samples can miss short-lived or
unlinked files, known-root observations do not prove absence of global caches, and
filesystem free bytes are not a leak proof. No live model or twenty-round claim is
made by this batch.

The fixture resets its synthetic queue records explicitly; that reset is not
production lifecycle cleanup. It compares normalized shared queue contents and
retains the pre-cleanup runtime/open-file/worktree paths for post-cleanup checks.
The inventory subtree's usage is compared separately from the external fixture
repository and registry; this does not establish whole-campaign disk stability.
Queue and worktree key lists expose totals and truncation. Process discovery uses
`marked_processes`, whose internal read errors are not surfaced, so a zero count
is not proof of complete process discovery. Git enumeration currently uses
`Command::output` without a timeout or output-byte limit against the explicitly
selected fixture repository; the collector is not a general bounded subprocess
runner. These limits must be addressed or accounted for before live campaigns.

Local collector verification on 2026-10-21 passed six focused tests, including
mixed/unknown queue ownership, key truncation, missing control roots, existing
permission preservation, symlink alias classification and the two-round fixture.
Stable coverage and Rust 1.85 all-target tests each passed 479 tests with 11
ignored; the absolute CRAP threshold of 10 had no violations. An initial fixture
compile error was corrected, and an earlier gate run was stopped for observer
side-effect corrections; these are not live-task completion evidence. No model
run was performed for this collector-only batch, and #38/#32 remain open.

## Resource observations during a native confirmed restart

The existing opt-in delivery/restart scenarios now record two metadata reports:
after actual Local delivery confirmation and before finish (and before custody
transfer in the restart path), then after finish and child reaping, before fixture
teardown. Captured paths cover the actual runtime roots (three for the normal
round, five for repair/restart) and the actual environment registrations; ordinary
clones are not invented as registered worktrees. The live snapshot checks the
scenario-specific parent/role task set and owned waits/events/deliveries, with no
mixed, unknown or unrelated queue entries. Existing candidate, receipt, target and
user asset identity assertions still run.

The post-finish snapshot requires absent owned roots/state/cleanup/queue and
registered paths, zero occupied slots and zero matching marked processes. Only
the expected missing queue read is allowed, after independent absence checks;
all other post-finish observation errors fail the scenario. Live process sampling
errors/truncation remain visible in the report; only structural observation
errors fail the pre-handoff snapshot. Reports save the actual confirmed candidate,
intent/receipt and queue IDs; collector keys must match those production IDs.
Registry absence means this task's entry is absent, not deletion of the shared
registry file or root. Reports use one rolling before/after pair, capped at 1 MiB
each; they remain counted in fixture usage rather than being silently excluded.
The fixture checks that the pair is bounded and does not grow per round. Existing
failure diagnostics retain at most 64 KiB per report, potentially a partial JSON
report. Successful runs leave the pair until fixture teardown.

Outside FD/cwd targets and per-process completeness remain observations, not
proof of global absence. Reads may update atime; symlink targets are not traversed
by the base inventory. This is one observed real restart, not a shared-base or
twenty-task campaign, whole-campaign disk stability, or complete global-cache and
process discovery proof. No new permission or product isolation policy is added.

Local verification on 2026-10-22 passed the final identity-bound native scenario
(1/1, 227.60s). It sampled two marked processes before custody transfer and zero
matches after finish; the actual expected delivery/queue identities and owned
path cleanup assertions passed. Selected OpenCode configuration bytes were
unchanged. An earlier run passed in 240.01s before the expected-identity report
checks were tightened; the final claim uses the later run. Neither run counts as
a twenty-task campaign or proof of complete process/global-cache discovery.

## Two-round shared-base campaign fixture

The opt-in two-round test uses one outer `TempDir`/base for both actual native
rounds. `base/workspace`, `base/temporary` and `base/registry` are reused; each
round's task environment is created only after the previous finish cleanup has
removed those disposable paths. Round 1 uses normal independent acceptance and
external Local delivery; round 2 uses failed review, repair, fresh passing review
and the confirmed external-supervisor restart. The shared queue path lives under
the recreated disposable runtime and is recreated per round; this is not evidence
of a persistent shared queue.

Each round records a live pre-finish snapshot and a post-finish snapshot after
all server/guard children have been reaped. The inventory test observes the
actual runtime set (three roots for the normal round, five for the repair/restart
round), derives expected task/notification counts from the scenario (normal three
tasks/two notifications; repair five/four), and compares exact production queue
IDs. Both transcripts use the same rolling before/after report pair; the fixture
asserts exactly two bounded report entries after both rounds. Formal targets and
user assets are created per round under the outer base and retained. Exclusions
enumerate every round's formal target and user asset exactly, cumulatively; no
broad prefix is used (reports are counted, not excluded). After each round and at
the final check, every prior target ref/commit, user device/inode/bytes and
content are checked again.

The outer base, retained formal/user outputs and reports live until final
assertions; there is no per-round `TempDir` teardown. After each round the fixture
enumerates base and shared-registry entries with fallible `read_dir` calls,
rejects unexpected retained entries, requires the exact rolling report pair,
round formal/user entries and allowed registry files, and requires the registry
index plus envs/plans/locks to contain no retained task state. A normalized
metadata footprint of the shared registry must be equal after round 1 and round
2, while workspace/temporary roots must remain absent. The non-live negative
control adds an unexpected retained file and verifies the retained-control check
rejects it. Reports are counted separately from the exact formal/user exclusions;
no broad exclusion hides retained control. This is two observed rounds, not a
twenty-round campaign, a whole-campaign disk-stability proof, or a claim of
persistent shared queue or global-cache absence.
