# End-to-end acceptance readiness (#38)

This checklist separates protocol evidence from real agent completion. #38
remains open. It does not replace its issue acceptance matrix or close #32.

## Current evidence and remaining work

| Boundary | Existing/current evidence | Still required for #38 |
| --- | --- | --- |
| Old task state | `tests/lineage.rs::legacy_migration_requires_quiescence_and_preserves_no_approval` refuses migration with a writer, pending operation, executing phase or open PR; preserves old task state and does not create a new approval. | The registered legacy-resource fixture below covers an actual undelivered file and preserved user asset. Unknown legacy paths remain outside inferred authority; no home-directory sweep. |
| Delivery and restart | `tests/delivery.rs` covers real local file/Git endpoints, frozen candidates, uncertain delivery, restart and cleanup faults with protocol reviewer fixtures. Real native delivery, repair, recovery and restart runs are documented below. | Recorded native tests cover injected lost observation and workspace-displacement cleanup failure, including eventual final registration removal after partial cleanup. Arbitrary transport/filesystem failures and customer production delivery are not claimed. |
| Repeated control state | The twenty-round protocol regression reuses one control directory and shared queue, alternating normal delivery, restore after preparation, and restore after confirmation; confirmed delivery cannot be executed again. A twenty-round real native campaign is recorded below. | Real twenty-task completion is observed below; whole-campaign disk stability and reliable global outside-root cache/session discovery remain unproven. |
| Native execution | Opt-in installed tests and `tests/common/acceptance_live.rs` run real native roles. OpenCode native coordinator scenarios below execute ACK/application themselves; the driver only registers work and collects. | OpenCode native coordinator behavior is evidenced; Codex coordinator/session bootstrap remains blocked by the host nested-userns/product gap. |
| Resource lifetime | Protocol regression checks task registration/slots, workspace/temp roots, plan/env/lock records, shared entries, previous deliveries and a pre-existing user asset before fixture teardown; native reports record marked processes and base resources. | Outside-root cache/session and complete process/worktree discovery remain observational; correct native delivery plus teardown is evidenced only for the recorded OpenCode scenarios. |
| Runtime compatibility | Local readiness review found Codex 0.163.0 and installed OpenCode 1.19.4; the native scenarios retain their recorded 1.19.3 baseline, and selected configuration is exercised unchanged. | OpenCode supports the conservative 1.19.x family at or above the 1.19.3 baseline after operation-level schema checks (only 1.19.4 has the new real smoke-test evidence); 1.20/2.x and Codex bootstrap remain unsupported. |
| Recovery/error matrix | Unit/protocol suites exercise timeout, late/partial results, unknown delivery and cleanup faults; native OpenCode runs cover ACK/stall, late rejection, unavailable restore and a phase-1 ambiguity implementation. | Actual approval-block observation, missing-report execution problems and injected delivery-unknown recovery are recorded below. Submitted-worker fresh CLI collection is merged in PR #98, with recorded evidence below; pre-POST ambiguity, takeover and server restart remain unsupported. User-confirmed requirement revision and resumed work have the scoped local native evidence recorded below; this is not customer delivery or a general policy change. |

## Outstanding acceptance decisions

The scenarios below do not justify closing #38 or #32. Keep these boundaries
separate rather than treating another passing normal run as missing evidence:

| Remaining boundary | Current status | Required next step |
| --- | --- | --- |
| Pre-POST dispatch ambiguity | A dispatch journal without a submission journal has no supported recovery that proves an unsent request. | Review a product recovery contract; fail closed without resend meanwhile. |
| Worker takeover / coordinator or server restart | Submitted-worker collection while the original server lives is a distinct boundary. | A supported takeover/transport restart contract and separate evidence are needed. |
| Codex native closed loop | Host nested-userns/bootstrap limitations remain. | Use a supported host or reviewed bootstrap support without weakening approvals or outer confinement. |
| Complete outside-root discovery | Known roots and sampled marked processes are observed; short-lived/unlinked paths and global caches are not exhaustively discovered. | Define and verify adapter-owned resource boundaries; do not infer completeness from root deletion. |
| Failure exactly during final registry/plan unlink | Native cleanup recovery proves eventual removal after a real partial-cleanup failure; it does not inject a fault at the final unlink instruction. | A reviewed test/product seam is needed for that narrower fault, if required; no journal mutation or timing race is presented as proof. |

The user explicitly accepted version B precedence for the disposable conflict.
The local confirmed-revision run below applies that decision through the real
coordinator and resumes independent review/development to B approval; its
publication checks remain separate from the local live result. No further user
answer is inferred or required for that fixture.

The original acceptance matrix remains authoritative. These classifications do
not waive a criterion or turn an external blocker into a passing result. Local
results in a batch awaiting publication must remain distinguishable from merged
checks; no additional model run is required merely to restate these boundaries.

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

The observed twenty-round OpenCode campaign below covers task-owned roots,
marked processes and exact delivery identities, but not complete outside-root
cache/session/process discovery, autonomous Codex coordination or a fully
upgraded legacy resource set. Those remain explicit limits rather than boxes
inferred from unit-test counts.

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
broad prefix is used (reports and the bounded summary are counted separately).
After each round and at the final check, every prior target ref/commit, user
device/inode/bytes and content are checked again.

The outer base, retained formal/user outputs, summary and reports live until final
assertions; there is no per-round `TempDir` teardown. After each round the fixture
enumerates base and shared-registry entries with fallible `read_dir` calls,
rejects unexpected retained entries, requires the exact rolling report pair,
round formal/user entries, the bounded campaign summary and allowed registry
files, and requires the registry index plus envs/plans/locks to contain no
retained task state. A normalized path/type footprint of the shared registry must
be equal after each round, while workspace/temporary roots must remain absent. The
non-live negative control adds an unexpected retained file and verifies the
retained-control check rejects it. Reports are counted separately from the exact
formal/user exclusions; no broad exclusion hides retained control. Registry file
byte sizes and lock contents are not compared; this is a normalized-state/path
check.

The same shared runner supports an opt-in fixed twenty-round campaign on one
outer base: rounds 1..20 are normal except rounds 5, 10, 15 and 20, which use
failed review, repair, fresh acceptance and confirmed restart. Every round uses
fresh formal/user paths and a recreated queue under the disposable runtime.
A bounded `campaign-summary.json` starts with zero records and records at most
twenty completed entries (normal or repair/restart, expected artifact/output
identity, actual before/after marked-process counts and incompleteness flags,
compact base file/byte/block observations, roots/state/cleanup/registry/queue
observations, elapsed time and measured report count/bytes). Records are appended
only after all round validation succeeds; a validation failure leaves the prior
completed count and cannot create a completed record. Report files remain a
bounded rolling pair; summary size is capped at 256KiB. An outer campaign
failure guard retains the rolling reports and summary when validation or a later
round fails, in addition to the per-round bounded logs. A failed round fails the
campaign immediately and retains bounded failure evidence; there is no retry,
skip or failed-round completion count. The observed twenty-round run is recorded
below. It does not establish whole-campaign disk stability, persistent shared
queue behavior or global-cache absence.

Observed validation (2026-10-22): the final two-round live run passed in 284.23s.
Each round sampled two marked processes before finish and zero matching processes
after finish/reaping; per-round retained-control and final prior-output checks
passed. The selected OpenCode configuration remained byte-identical. The first
attempt completed round 1 but failed in round 2 because the developer wrote
`INCOMPLETE` instead of the required literal `version INCOMPLETE`; its bounded
failure evidence was retained. The prompt was clarified without relaxing the
assertion, and the complete campaign was rerun successfully.

Twenty-round validation attempt (2026-10-22): the first actual campaign stopped
at round 4 after three complete rounds (681.70s total). The fourth developer
reported an acceptance verdict without changing the artifact; its collected
output ID equalled the input ID. Application rejected the duplicate artifact,
and the real event was acknowledged but not applied before the deadline.
The retained summary contains only rounds 1–3. Bounded failure evidence was
retained and the selected configuration bytes were unchanged. This is not
twenty-task acceptance; no failed round was retried or counted as complete.
The fixture now identifies the developer role explicitly despite the legacy
activity name `review`, uses a development-named cwd and developer/development
wait labels, and asks for the literal file contents with edit/add/commit while
forbidding verdicts. Immediately after developer collection the fixture requires
a new output commit, exact committed content, and no `.herdr-verdict.json`, and
adds that worker report to bounded failure evidence before coordinator waits.
Coordinator instructions require the exact collector output, forbid copying,
editing or probing queue/guard/state, and require stopping on non-busy errors.
The failed attempt remains recorded; assertions were not relaxed. Its primary
log and structured diagnosis remain available locally; the transient bounded
dump directories were no longer present at the later verification.


Observed twenty-round validation (2026-10-22): the second complete campaign
passed in 2601.78s, starting from round 1 with a fresh outer base. All twenty
consecutive tasks completed: sixteen normal delivery rounds and four
repair/confirmed-restart rounds (5, 10, 15, 20). Every round sampled two marked
processes before finish and zero matches afterward, with zero occupied slots
and owned roots, exact output/user preservation, and all prior outputs rechecked.
The report pair remained exactly two files, totaling 31,456–41,929 bytes across
rounds; the at-most-twenty-record summary and 256KiB bound passed. The selected
OpenCode configuration was byte-identical before and after the run. No failed
round from the first attempt was counted or resumed into this campaign.

The queue is recreated each round, and process sampling remains incomplete by
design. Registry comparisons concern paths/types and empty state, not lock-file
bytes. The summary validator checks completion-critical fields, not every field
of a general-purpose external JSON schema. If native collection fails before
returning its message ID, fixed server/submission diagnostics remain available
but the per-message report may not be captured. These limits and the remaining
Codex and failure-matrix acceptance work keep #38 and #32 open.

## Mixed Codex worker and OpenCode coordinator

An opt-in mixed scenario is designed to validate a real task-owned Codex developer
without claiming a Codex coordinator. The developer is created, dispatched and collected
through `task_runtime::native` using Codex/bubblewrap and a selected Codex config
and optional auth copied only into the disposable runtime `c/` directory. Its
actual turn journal and output commit are collected; the test does not synthesize
an output or use a driver Git command. The existing real OpenCode coordinator and
independent reviewer then receive the production queue event, ACK/readiness and
task-apply with the exact producer evidence, and the existing external Local
delivery/finish path runs when acceptance succeeds. The test must prove distinct
Codex worker, OpenCode coordinator and reviewer sessions, actual native ACK/apply,
exact accepted output, preserved target/user asset and complete owned-resource
cleanup.

Codex collection binds `turn.json`, not an OpenCode `message_id`/submission file;
the test uses that producer artifact for event, binding, report and output checks.
The test asserts the selected adapter, worker session, turn and report identity,
and the native launchers hold children behind a bounded RAII guard. Normal
completion requires successful termination and reaping. During unwinding the
guard attempts termination and a bounded reap, and reports failures without a
second panic. A final Drop failure can leave a child unreaped; there is no
background reaper or unconditional cleanup guarantee. The regression covers
successful Drop cleanup, not OS kill/wait failures. It never waits unbounded.
Codex approval policy is `on-request`; pending approvals are not answered automatically. Actual approval keys may be used only
for authorized in-scope prompts. A bounded no-model nested-userns preflight runs
before the expensive scenario and fails explicitly on unsupported hosts rather
than skipping or reporting success. This scenario is not a Codex-coordinator
proof, a twenty-task campaign, or evidence of global-cache absence; a task-owned
Codex receiver/session bootstrap is not provided by this change. #38/#32 remain
OPEN.

Codex mixed live attempt1 failed before model work at 24.80s because the Codex
app-server could not create a nested user namespace under the task-owned outer
bubblewrap boundary. A single-bwrap probe succeeded while the reproduced nested
probe failed; this is retained as a host/bootstrap limitation, not a model or
product-success claim. The mixed test now runs a bounded nested-userns preflight
and fails explicitly on unsupported hosts; it does not skip or report success.

Primary verification of the preflight rejected this host in 0.03s before model
startup. The test is still unproven end to end here. Current app-server startup
has no verified external-sandbox bootstrap option that preserves native approval
policy; turn-level externalSandbox does not prevent the observed startup probe.
A supported host or separately reviewed upstream/product bootstrap is required.
No host security settings, approvals or outer task confinement were changed.

## Native same-session ACK/stall recovery

`installed_real_opencode_ack_stall_recovery_is_applied_by_native_receiver` is an
opt-in development-only scenario. It uses the existing real native worker and
receiver harness, but the coordinator is instructed to ACK the first delivery and
then stop without applying it. The task's `apply_timeout_seconds` is 60 seconds;
the deadline starts when the result arrives, not when it is ACKed. The production
queue watcher therefore emits a `progress_stall` event for the acknowledged but
unapplied result. The coordinator must ACK that alarm delivery, run
`queue event-current` for the exact stall event, select the original event by its
exact ID (both events share the same wait), and apply the original collector
output once. The driver never ticks, ACKs, applies, edits a deadline, or writes
watch/heartbeat state.

The test preserves an intermediate ACK-only snapshot before recovery. At that
snapshot it requires no stall event yet, exactly one delivery containing only the
exact original event, the original event acknowledged but unapplied, and no
completed first-turn task-apply command. The coordinator and worker sessions must
differ, and the worker's actual collector report and release receipt must name
that worker. Final checks require the original event applied, the stall event not
applied, `next_action.kind=complete`, one successful task-apply (failed attempts
are allowed only with lock-busy shell evidence), one new artifact equal to the
collector output, the actual release receipt, and exactly two distinct
acknowledged delivery identities for the coordinator session. Tool records are
ordered by their timestamps and use shell exit metadata: successful ACK,
event-current, task-apply and readiness calls must use the exact disposable queue
path. Readiness must name the stall delivery and coordinator session in
`last_confirmed_receipt`, not merely report `active_wakeup`. The first-turn ACK
snapshot is retained as bounded summary failure evidence (counts and identities,
not a full tool transcript). Readiness must execute after successful task-apply;
its checked_at must fall within that tool invocation and its returned JSON must
equal the persisted readiness document. Transient `active` and
`delivery_status` values are recorded only as observations and are not assumed
to persist.

This is same-session omission and stall recovery, not a process-crash or server-
restart proof. There is no supported OpenCode coordinator transport restart API;
do not delete runtime state or start a second server to simulate one. The scenario
does not claim independent acceptance, customer delivery, Codex parity, or a
twenty-task campaign. #38 and #32 remain OPEN.

Primary live verification passed in 126.75 seconds after the post-apply readiness
checks were added; selected OpenCode configuration bytes were unchanged. An
earlier run passed in 115.96 seconds before that tightening and is not used as
evidence for the final chronology assertions. The initial non-live compile
failure (undeclared Value::Null) was corrected to the existing J alias; its log
is retained during this unfinished acceptance work.

## Fixed-snapshot late-result rejection

`installed_real_opencode_fixed_snapshot_late_result_is_rejected_by_native_receiver`
is an opt-in development+acceptance scenario. A real OpenCode reviewer inspects a
fixed Git snapshot A and writes the actual immutable verdict. Its result is
collected and its event is attached while A is still current, but queue
notifications are disabled through the supported CLI after native dispatch and
before terminal collection. The driver then creates a real fixture Git commit B
in a separate clone, registers/selects B under the queue lock, and re-enables
notifications. B is driver-owned setup, not a concurrent developer claim.

This tests delayed delivery of a pre-collected result, not collection after
replacement. At collection the event is unacknowledged, unapplied, pending and
undelivered. After B selection `event-current` is false. The same real coordinator
receives the delayed delivery, ACKs it as receipt only, runs `event-current` for
the exact event (expected nonzero), and attempts exactly one stale application
with the registered verdict ID (expected nonzero stale rejection). It never
claims application success, runs no readiness, and does not rewrite the verdict.
The driver never ACKs, applies, writes a verdict, ticks, or manually edits
queue/task state; only the supported notification toggle and lineage
artifact/candidate setup are used.

Bounded assertions compare protected task/result fields rather than claiming
broad no-mutation across watcher heartbeats. The model-written
`late-application.json` is read and must name the exact task, stale event, child
state version and registered verdict ID; the actual failed command must use that
exact file as its single --spec argument. It must have the conclusion-derived
`next_action` and no fabricated output/acceptance fields. The coordinator must
have a completed late turn; exactly one terminal stale `task-apply` with the exact
production obsolete error is required, with no successful application and no
other non-busy rejection. Original event remains acknowledged/unapplied/pending,
parent approval is null, candidate is B, and A's fixed HEAD/content plus the
reviewer report bytes and verdict ID are unchanged. Event/delivery totals, the
unique accept event, exact delivery binding, and the signal/submission-message/
report session/output chain are checked. No immediate pre-deadline
`progress_stall` is observed; because the default apply deadline is 600 seconds,
overdue stall suppression is not demonstrated by this bounded scenario.
Actual failed tool terminal state/exit and output, separate shell invocations,
temporal evidence, and bounded child teardown are checked. The fixture is not
process/server crash recovery, a concurrent developer proof, independent delivery,
or #38 completion. #38 and #32 remain OPEN.

Primary live3 passed in 130.25 seconds, with selected configuration bytes
unchanged. Live1 failed after reviewer collection because the test read the
queue before its watcher had converted the signal into an event; the fixture now
waits read-only up to 30 seconds with notifications disabled. Live2 failed because
the coordinator appended status-printing shell commands, masking the native
nonzero exits as zero. Direct calls preserving tool exit metadata are now explicit
in the instructions; negative assertions were not relaxed. Both failure logs and
bounded diagnostics remain available during this unfinished acceptance work.

The --spec check binds one exact quoted or unquoted application path and checks
tool exit/error evidence; it is not a complete shell parser. Direct calls without
wrappers are required by the prompt, but arbitrary trailing shell syntax is not
excluded by that path check. The changed completed guard submission is associated
by the observed timeline; exact ACK and event/tool identities provide the event
evidence, not a separately verified submission-context event binding.

## Native reviewer unavailable restore continuation

`installed_real_opencode_unable_restore_is_applied_by_native_receiver` is an
opt-in phase-1 scenario. The immutable requirement is recorded before any
dispatch with a test-only setup parameter: the product requirement remains the
file-version requirement, while a separate mandatory acceptance source requires
the operator-provided disposable tool `./required-dependency-fixture --check`.
The tool is not part of artifact A and is not created by the developer or
reviewer. The test verifies it is absent before review and after the scenario,
and that it is not in artifact A's committed tree.

A real OpenCode reviewer inspects a fixed snapshot of A and directly executes the
exact missing dependency command. Its actual tool record must be terminal with
nonzero exit 127 and the missing-path error; it must not use wrappers or echo to
mask the exit. The reviewer writes the actual immutable `unable` report with a
blocked check, exact requirement source, observed-path evidence, blocker and
restore condition. The test rejects another conclusion honestly; it never writes
or rewrites the verdict.

The real coordinator ACKs the notification as receipt only and applies the
unable verdict ID with `next_action.kind=restore`. Assertions require the child
to be blocked with empty pending, parent `approved=null`, restore/planned
continuation, exact verdict binding/report, unchanged candidate A, unchanged
requirement revision and authorization, and no repair, delivery or new
development child. The parent continuation has no execution-problem substitution;
the model's actual unable report is distinct from a no-verdict native crash.
Signal, submission message-id and reviewer report identity are checked, along
with bounded child/resource cleanup.

Phase 1 stops after the restore continuation is recorded. It does not claim that
the dependency was installed, that a fresh review ran, or that a restored review
passed. Actual restoration and a fresh review would be a separate phase. Existing
synthetic `unable`/restore and execution-problem coverage remains the version-matrix
overlap; this scenario adds the missing real reviewer dependency-failure path. No
product/API/isolation change is made and #38/#32 remain OPEN.

Primary verification: the real unavailable-dependency scenario passed in 127.05 seconds; the selected OpenCode configuration remained unchanged. The initial local compile failure was corrected and retained as failure evidence. The dependency is recorded as a requirement basis source and enforced by this scenario's reviewer instructions and observed execution; it is not a required parent acceptance criterion enforced by the core pass validator. This test therefore does not prove that the core rejects a hypothetical passing report that omits this dependency check. The relative command uses the fixed reviewer session cwd; tool cwd is not independently sampled. Actual dependency restoration and a fresh passing review remain outside this phase.

Independent non-live gates passed: stable coverage and Rust 1.85 all-target suites each reported 483 passed, zero failed, 17 ignored. CRAP checked 4800 functions at 77.2% line coverage with zero violations of the absolute threshold 10; formatting, lint policy, Clippy and rustdoc passed. Ignored live scenarios are not counted as executed by these suites; the unavailable-dependency scenario was run separately as reported above.

## Restored-review continuation

The restored-review phase builds on the phase-1 unavailable state without
rewriting it. The operator dependency is installed exactly once through a
registered foreground `env.command` installer (no detached `queue run`), with its
actual argv/cwd/exit/hash/mode retained in the receiver runtime. Only in phase-2
mode, the unassigned `restore-dependency-install` signal wait is registered through
the supported queue add before dispatch; phase 1 does not get this wait. It is then
produced by the test operator; it
is a producer event, not a driver ACK/apply/verdict. Its delivery is ACK-only and
must remain unbound/unapplied, leaving parent pending/version untouched. The same
coordinator wakes, ACKs receipt, and consumes the phase-1 restore intent with
`task-lineage kind=continue` using `verify`/`review`, the current parent version,
and actual install evidence. It registers only a new `accept-restored` acceptance
activity/wait.

The fresh checkout is cloned once from candidate A before continuation, and the
operator dependency is installed there once before the signal. The new activity
carries the phase-1 acceptance origin; candidate, round, revision, input and
authorization must remain unchanged. A fresh real reviewer, distinct from the
phase-1 reviewer/worker/coordinator, runs on that prepared checkout without
reinstalling; its `./required-dependency-fixture --check` and product check must
have actual successful tool evidence, and its actual passed verdict is required.
The real coordinator ACKs/applies that immutable verdict, setting parent approval
for candidate A while recording a delivery continuation; no delivery is executed.

The exact phase totals are four events/deliveries: phase-1 review and unable,
operator install signal, and fresh review. The install event remains
acknowledged-only and never becomes a verdict or pending result. Phase-1
limitations remain: the dependency is a basis source rather than a core-enforced
required criterion, and tool cwd is implied by the fixed checkout rather than
independently sampled. This phase does not claim customer delivery, context
rotation, Codex parity or #38/#32 closure. No product/API/isolation change.

Primary restored-review validation failures are retained locally. Initial non-live compilation exposed an incorrect `Command` unwrap, missing metadata trait imports, and a closure return-type mismatch; these were corrected. Live attempt 1 stopped when writing installation evidence to a group-writable receiver cwd; evidence now uses the existing private runtime root without widening permissions. Attempt 2 produced an actual unable report missing required top-level blocker/restore-condition fields; collection rejected it. Attempt 3 attributed the dependency check to the product-file source; the exact-source assertion rejected it. The reviewer instruction now names the required JSON fields and exact operator source. Attempt 4 registered the continuation but failed the strict tool-retry evidence check; the original error output was not retained, so its cause remains unclassified. Attempt 5 failed an exit-code assertion while the command selector could also match a report-validation command containing the dependency text. The selected command was not retained independently; source inspection and the server log exposed that ambiguity. Selection now requires the exact direct dependency command. None of these failures is counted as a successful restored-review run.

Continuation diagnostics retain only recent same-queue lineage tool records with bounded fields and a 60 KiB JSON limit; failure messages reference that evidence rather than expanding full tool output. Exact `--state-dir`, `--delivery` and `--spec` argument checks bind the observed calls to the intended queue, receipt and application. These argument checks are not a complete shell parser proving the absence of every possible wrapper or trailing construct. Installer cleanup uses bounded kill/reap attempts; failures are reported, not a claim that every OS-level cleanup error is recoverable.

Attempt 6 retained the actual continuation error: `result wait must be an object` (exit 2), followed by a model correction and retry. The strict non-lock-busy retry assertion rejected that run. The coordinator instruction now supplies the complete nested activity/result-wait schema, while the model must still read the actual parent version, candidate and installation evidence and write its own spec. Attempt 7 passed in 187.86 seconds with the selected OpenCode configuration unchanged: actual developer, unable reviewer, operator installation, same coordinator continuation, fresh reviewer pass, approval and bounded cleanup. This proves the bounded restored-review scenario only; no customer delivery was performed. Earlier failures remain retained and are not reclassified by this success.

After live attempt 7, the only test-code change was diagnostic truncation: when JSON escaping or multibyte text would exceed 60 KiB, oldest diagnostic records are removed until the serialized snapshot fits, preserving the most recent records and the original total/truncated metadata. A non-model regression covers escaped control characters and multibyte text. This does not change the native scenario, prompts, state transitions or acceptance assertions. The first full-gate attempt and an overlapping diagnostic-test compilation failed with `No space left on device`; those failures were retained. After checking process references, only the completed legacy-resources worktree's build target was cleaned, freeing 6.7 GiB; source and evidence were preserved. Final gates are rerun separately.

Final non-live verification after diagnostic truncation: stable coverage and Rust 1.85 all-target suites each passed 484 tests with zero failures and 18 ignored. The escaped-text diagnostic regression passed. Formatting, lint policy, Clippy and rustdoc passed; CRAP checked 4800 functions at 77.3% line coverage with zero violations of the absolute threshold 10. The real scenario evidence remains attempt 7, whose prompts and acceptance logic were unchanged by the later diagnostic-only cap fix.

## Real ambiguous requirement clarification (phase 1)

The opt-in `installed_real_opencode_ambiguous_requirement_is_applied_by_native_receiver`
scenario covers the remaining real-role acceptance branch without inventing a
user decision. It creates one disposable fixed Git artifact A and a requirement
whose original reference requires version A while a confirmed change and project
specification keep version B binding with no stated precedence. The existing
review authorization is retained.

One fresh real OpenCode reviewer receives the fixed basis and must inspect the
actual artifact with a direct `git show HEAD:file` invocation, preserving its
terminal exit, output and timestamps. It writes the actual `.herdr-verdict.json`
itself; the driver never supplies or rewrites the conclusion. For a genuinely
unresolved conflict the report must use `conclusion: ambiguous` with nonempty
top-level `question` and `unresolved_after_review`, blocked checks bound to each
exact conflicting basis source and the valid `goal` criterion, empty
`gaps`/`resolutions`/`reuses`, and `agreements_checked` containing every
original/change/specification reference, `basis.confirmation` and every
authorization source. A different conclusion fails the scenario honestly.

The same registered real coordinator receives the single production notification,
ACKs its exact delivery as receipt, applies the exact immutable verdict with
`next_action.kind=clarify`, runs readiness, and stops. The test checks exact
queue/delivery/spec arguments, terminal native exits and timestamps, one successful
task-apply with only lock-busy failed attempts, no `task-lineage`/revise/continue,
one event/delivery, unchanged artifact/requirement/candidate/revision/authorization,
`approved=null`, and a blocked child with empty pending. No dependency installation,
new activity, requirement revision, user-confirmed resolution or delivery occurs.

Primary live validation (2026-10-22): attempt 1 passed in 77.79s including build,
with the selected OpenCode configuration unchanged. The actual reviewer produced
an ambiguous report, and the native coordinator acknowledged and applied its
immutable verdict to a planned clarify continuation. Snapshot HEAD/file and report
bytes remained unchanged; the actual model-written application was bound to the
exact event, verdict and child version. Cleanup assertions passed.

This is phase 1 only. A real operator/user confirmation, revised requirement and
resumed work remain a separate scenario. Exact argument checks are not a complete
shell parser and do not prove the absence of every trailing shell construct.
#38 and #32 remain OPEN; the Codex host/bootstrap blocker and all prior evidence
limits are unchanged.

Independent non-live verification: stable coverage and Rust 1.85 all-target suites
each passed 484 tests with zero failures and 19 ignored. Formatting, lint policy,
Clippy and rustdoc passed. CRAP checked 4800 functions at 77.2% line coverage with
zero violations of the absolute threshold 10. Stable and Rust 1.85 release builds
passed; all builds used this worktree’s own target directory.

## Concurrent native development and fixed-A review

The opt-in `installed_real_opencode_concurrent_developer_while_reviewer_fixed_a`
scenario adds the missing real concurrent-role proof. Unlike the fixed-snapshot
late-result test, which creates B with driver setup after collecting reviewer A,
this scenario starts one real OpenCode developer and one real OpenCode reviewer
against separate checkouts/roots of the same fixed candidate A. Both production
native dispatches are issued before either result is collected. Notifications are
then disabled through the supported queue CLI and both native results are
collected; reviewer A signals only after that point, so the coordinator cannot
apply an unwanted result. Receiver readiness is checked against the real
coordinator session before dispatch, and no watcher or readiness state is
fabricated.

The developer must create an actual new commit B from A; the reviewer must write
its own actual verdict and inspect `git show HEAD:file` as A. The test binds
parent candidate/revision, both activity inputs, exact sessions and submitted
message IDs, reviewer report/verdict binding, developer output `input`, distinct
checkout HEADs, and the unchanged A source. No driver creates B or a report, and
no ACK, task-apply, candidate selection, delivery or user answer occurs.

Overlap is asserted only post hoc from actual timestamps: each exact
`submission.json` `created_at` is normalized from seconds to milliseconds, and the
assistant completion is selected by the final journal’s exact reply ID, session,
parent message ID and cwd (not an arbitrary historical assistant row). Missing, nonfinite, reversed or
non-overlapping intervals fail the proof. This describes overlapping native turns;
it does not claim simultaneous CPU or model execution. With no supported
scheduling barrier, overlap is observed rather than forced; a non-overlapping run
is retained as an honest failed proof, not retried with sleeps or synthetic
reports. B remains unselected and unapplied, leaving A current and approval null.
#38/#32 remain OPEN and all prior Codex/resource-discovery limits are unchanged.

Primary validation (2026-10-22): live attempt 1 failed after 39.35s at the exact
developer commit-command assertion. The retained server log contains a commit
permission evaluation, but the complete selected tool command was not retained;
the command-shape cause remains unclassified. The prompt now explicitly separates
staging from the exact commit invocation, and bounded private diagnostics retain
only matching Git command metadata, without tool output or selected configuration.
Attempt 2 passed in 50.62s including build, with selected configuration bytes
unchanged. Actual turn overlap, fixed-A inspection, actual new B, both producer
evidence chains, no delivery/application and cleanup assertions passed.

After attempt 2, only diagnostic truncation accounting and its non-model regression
changed: count omitted commands before capping at eight per role, retain the most
recent records, mark field truncation, and cap serialized diagnostics at 60 KiB.
The live scenario, prompts and acceptance assertions were unchanged by this fix.

Final non-live gates: stable coverage and Rust 1.85 all-target suites each passed
486 tests, zero failed and 20 ignored; CRAP checked 4800 functions at 77.3% line
coverage with zero absolute-threshold-10 violations. Formatting, lint policy,
Clippy and rustdoc passed. The final diagnostic ordering/count correction passed
its focused regression on both toolchains and a fresh Clippy check; no model
scenario or acceptance assertion changed after live attempt 2.

## Native permission-block observation

The opt-in `installed_real_opencode_approval_block_stops_without_result` scenario
uses one real OpenCode reviewer on fixed artifact A and a test-owned sentinel
inside the owned temporary environment but outside its cwd/runtime, in a dedicated
`approval-probe` directory. OpenCode 1.19.3 requests external-directory permission
using the containing directory’s `/*` pattern. The disposable configuration copy
sets exactly that dedicated pattern to `ask`; other permission rules and the
selected file remain unchanged. The request must contain only that exact pattern;
the bound bash command must name the exact sentinel file.
The reviewer is asked to inspect A and attempt the exact sentinel with `cat`; it
must not approve or answer any permission prompt and must not write a verdict.

The production session/collector must report `native_status=blocked` for the exact
session while the native phase remains nonterminal (not `phase=blocked`). The test
binds the actual `/permission` request id, session, message, call ID, path and tool
input to the exact submitted native message, with no driver ACK, apply, verdict or
tick. It asserts no report, event, delivery, verdict or applied result; parent
requirement, candidate, round, revision, authorization and approval remain
unchanged and the task pending list is empty. Detection is bounded; the child is
then stopped/reaped and the owned environment, including the sentinel, is destroyed.
It claims neither a partial-result execution_problem nor any acceptance conclusion.
This is execution blockage only. #38/#32 remain OPEN.

Primary live validation (2026-10-22): attempt 1 reached actual native permission
blockage, then failed after 18.44s because the test expected a file pattern while
the retained request used the containing directory’s `/*` pattern. The fixture
now confines the probe to its own disposable directory and checks the exact
singleton directory pattern plus exact file command; suffix and extra-pattern
negative controls reject broader matches. Attempt 2 passed in 22.25s including
build. Selected configuration bytes remained unchanged, no request was answered,
no report/event/verdict/application was produced, and cleanup assertions passed.
Private permission metadata is serialized within 60 KiB, reports count/field
truncation, and is registered for failure capture before schema assertions.
The original failed run remains retained locally and is not counted as success.

Independent non-live verification: stable coverage and Rust 1.85 all-target suites
each passed 489 tests, zero failed and 21 ignored. Formatting, lint policy, Clippy
and rustdoc passed. CRAP checked 4800 functions at 77.2% line coverage with zero
violations of the absolute threshold 10. Permission-pattern negative controls
and escaped/multibyte diagnostic-bound regressions passed in both suites.

## Native completed turn without a verdict report

The opt-in `installed_real_opencode_missing_verdict_is_restored_by_native_receiver`
scenario uses one real OpenCode reviewer on fixed artifact A. The reviewer is
explicitly instructed to inspect A with a direct `git show HEAD:file` and then
finish the turn without writing `.herdr-verdict.json`. This is a prompt-induced
missing-report execution result, not provider truncation, a general partial-output
recovery claim, or an acceptance conclusion.

The actual production collector still signals the completed native turn with no
registered verdict. The same real coordinator ACKs the exact delivery, reads the
child/event evidence, and applies an application with no verdict/report/output/
acceptance fields, `execution_problem` naming the missing report, and
`next_action.kind=restore`. The test requires the exact event/signal/submission
chain, one successful task-apply with only lock-busy failed attempts, post-apply
readiness, parent `approved=null`, null parent verdict, blocked child with empty
pending, and a planned restore continuation bound to the exact event. Requirement,
candidate, round, revision, authorization and source/reviewer snapshots remain
unchanged; exactly one event/delivery exists and no new activity, real restore,
delivery or user decision occurs. Report absence is observed at sample points
before dispatch, after collection and after final apply; this does not prove the
file was never transiently created and deleted, and the driver never deletes it.
Bounded native/child cleanup and failure evidence are retained. #38/#32 remain OPEN.

Primary live validation (2026-10-22): attempt 1 failed after 47.83s when the native
coordinator requested external-directory permission to read the actual collector
submission. This mode had not prepared the reviewer runtime before generating the
coordinator configuration. It now prepares that exact task-owned runtime and uses
the existing explicit owned-directory opt-in; kernel isolation is unchanged.
The reviewer history query also uses its own server endpoint. Attempt 2 passed
in 59.29s including build, with the selected configuration unchanged. The actual
completed reviewer reply, observed missing report, native ACK/application,
post-apply readiness receipt/output/timestamps and cleanup checks passed. The
failed run remains retained locally and is not counted as success.

The selected bounded diagnostic covers the later reviewer tool/reply and
coordinator readiness checks. It is written after the stage wait and initial
application/event/parent assertions; earlier failures retain the registered raw
paths, not that selected metadata snapshot.

## Native accepted delivery with injected lost observation

The opt-in
`installed_real_opencode_accepted_delivery_unknown_is_reconciled_by_fresh_supervisor`
scenario extends the real native acceptance and external Local Git delivery
fixture. A test-only `Executor` delegates Local preflight and performs the actual
Local mutation exactly once, then returns an injected `Unknown` observation
instead of observing the endpoint. The target is therefore at accepted artifact B
while the persisted intent remains `unknown` with no receipt. This is a
test-injected lost observation, not a provider/network crash or a fabricated
confirmation.

After releasing custody and before fresh reconciliation, the test records a
truthful `before_unknown` inventory
with a null receipt and unknown observation metadata; it does not call the
confirmed-result inventory API or invent a receipt. A fresh unmarked CLI process
then runs `delivery reconcile` with the real Local observer, which confirms the
actual B target and receipt, followed by `finish`. No execute path is invoked after
restart. Assertions bind the exact intent/artifact/request/ref and confirmed
receipt, target commit/content, unchanged user asset, one wrapper execution, and
absent owned roots/registry/state/queue/slots. The existing confirmed restart
scenario remains separate and unchanged. #38/#32 remain OPEN.

Primary live validation (2026-10-22): attempt 1 passed in 106.81s including
build, with selected configuration unchanged. Real native development/acceptance,
actual Local mutation, persisted UNKNOWN, fresh reconciliation and finish passed.
After this run only diagnostic limits changed: environment state reads are capped
at 1MiB, selected evidence at 60KiB, and raw error text is omitted. A payload over
the diagnostic cap is replaced with an explicit truncation marker; that marker
does not preserve exact identity fields. The live run does not cover these later
diagnostic limits; focused non-model validation covers them separately. No general
provider/network-failure or process-discovery completeness claim is made.

## Native confirmed delivery cleanup-fault recovery

The opt-in
`installed_real_opencode_confirmed_delivery_cleanup_fault_is_resumed_by_fresh_supervisor`
scenario reuses the real failed-review/repair/fresh-acceptance and confirmed
Local Git delivery fixture. After confirmation and a fresh unmarked reconcile,
the test atomically renames only the disposable workspace root to a fixture-owned
sibling. The temporary root, queue, state, delivery intent/receipt, target and
user asset are not moved or edited. This is a fixture filesystem fault, not a
product or adapter failure.

The first fresh CLI `finish` must fail after entering cleanup: the persisted
environment is `destroy_failed`, delivery remains `cleaning` with
`shared_removed=true`, the exact intent and receipt are retained, the cleanup plan
and registry slot remain, owned queue entries are gone, and the workspace is
missing at its original path while the displaced directory retains its exact
device/inode. A bounded private snapshot of the failed state/plan and CLI statuses
is registered before the fault and written before the assertions; it contains no
raw error output. The assertions bind the failed document and plan to the
original environment instance and the registry entry to the same instance, roots
and state file. They compare the plan's recorded roots with its pending/completed
sets instead of assuming which later root was visited. A pre-cleanup rejection
would leave phase `confirmed`, no plan and the queue entries intact; the displaced
workspace alone is not accepted as proof of partial cleanup.

An attempted `delivery execute` must be rejected without changing the target or
user asset. A queue directory that is already gone is accepted only when the
expected temporary root was actually removed; otherwise the persisted queue must
contain exactly empty owned sections. The test then renames the same displaced
directory back to the exact original workspace path and verifies the inode, runs
a fresh unmarked CLI `finish` that returns `finished:true`, and requires removal of
the remaining roots, registry/state/plan, queue and slots while preserving the
delivered ref and user asset. A bounded RAII guard
restores the displaced root before child reaping and handoff cleanup on unwind;
it never overwrites an unexpected replacement path. No product journal, receipt,
ACK or application is fabricated, and no raw evidence is uploaded.

Primary attempt 1 (2026-10-22) failed after 226.28s at the occupied-slot
assertion, after the real native stages and first failed cleanup. The fixture
expected one slot, but `occupied_slots()` counts the environment document and
cleanup plan separately. The corrected assertion requires exactly two, exactly
one registered environment, and matching document/plan/registry instance identity.
This was a fixture assertion failure, not evidence of a second leaked environment;
the failure is retained locally. Attempt 2 passed in 189.46s including build,
with selected configuration unchanged. It verified actual partial cleanup,
retained cleaning state and plan, replay rejection, restoration of the original
workspace inode, fresh `finished:true`, and final resource/target/user checks. The twenty-round campaign and earlier native delivery/restart evidence
remain separate. #38/#32 remain OPEN.

The final inventory sample includes the bounded failure snapshot in base usage.
The snapshot is removed only after that sample passes, so this sample proves the
asserted owned-root/registry cleanup, not an exact formal-output-only base listing.
The outer disposable fixture is removed at teardown; the snapshot is preserved
only on a failed assertion through the bounded development failure evidence path.

## Submitted native worker recovery through fresh CLI collection

The opt-in
`installed_real_opencode_submitted_worker_is_collected_by_fresh_cli`
scenario uses the real development-only coordinator flow. It creates one actual
OpenCode worker session, binds it, and dispatches the immutable packet exactly
once through the existing native start path. It checks the exact session, packet
hash, queue id, wait binding, runtime endpoint/cwd and submitted message/context
before recovery.

The test releases only the parent environment ownership; the original native
server remains alive. A fresh plain CLI process runs `task-runtime collect` and
repeats only after a known `StateBusy` message or a nonterminal submission phase,
until the journal is `signalled`. It never calls parent `native::collect`,
dispatches again, creates or relaunches a session, edits a journal, or ACKs/applies
from the driver. The resulting submission retains the same session, message id and
context; creation, dispatch and runtime journals are unchanged; the session
history shows one matching user prompt and one terminal reply. The existing
coordinator ACKs/applies the single event through the normal receiver path.

A test-only reversible custody guard reacquires the exact environment instance on
success and on error/panic with bounded lock retries, storing the same instance
back into the outer owned-environment wrapper. It never reuses the delivery
`HandoffGuard`, whose cleanup contract is deliberately different. On failure the
worker child is reaped by RAII before the outer wrapper destroys that same
instance; a bounded private snapshot retains the pre-failure journal metadata and
CLI statuses without raw error output. On success the worker is stopped before the
normal final teardown. Selected config and final roots/slots must be
unchanged/zero. A no-model catch-unwind control verifies outer ownership cleanup,
and a registry-root guard restores `HERDR_TASK_ENV_ROOT` and tears down on test
assertion failure.

Primary live validation (2026-10-22): attempt 1 passed in 65.63s including build,
with selected configuration unchanged. The fresh CLI collection, exact native
identity, actual coordinator ACK/application and final cleanup assertions passed.
The first non-live compile attempt failed on seven missing path borrows; these
were corrected before the passing non-live/live runs and the compiler log is
retained. After the live run, the shared CLI timeout diagnostic was changed to
omit stdout/stderr; scenario and success assertions were unchanged.

It is not a crash
or server-restart proof, and it does not cover a dispatch interruption after
`dispatch.json` but before `submission.json`: no supported API
can prove that the POST was not sent, so that boundary remains fail-closed.
Worker takeover, coordinator process restart, a second POST and invention of a
replacement session are not claimed. #38/#32 remain OPEN.

## Confirmed requirement revision with native B development

The opt-in
`installed_real_opencode_confirmed_revision_resumes_with_b_development`
scenario is separate from the phase-1 ambiguity stop and does not change that
fixture. Its newly configured disposable fixture records both verification and
development authorization, sourced from the user's standing authorization to
implement this scenario and their explicit acceptance of the version-B
recommendation. That development authority is labeled newly configured fixture
authority, not historical phase-1 authority and not authority over user assets.

The actual sequence is: real ambiguous review and clarify; an owned foreground
operator signal carrying bounded immutable evidence of the user's B choice;
native coordinator ACK-only receipt of that signal, revision 2 with explicit B
precedence and the genuine confirmation, candidate reset/reselection of A, and
continuation into a fresh acceptance using the existing verify/review action; a
real reviewer's honest failed verdict for A under B; coordinator application of
that failure and continuation into development under the preconfigured
develop/development action; a real developer producing and committing B; then a
fresh real reviewer and coordinator application of the actual passed verdict.
No driver presses ACK/application/verdict buttons and no customer delivery occurs.

Assertions freeze revision-1 requirements, authorization, basis, verdict and
report bytes. Revision 2 must retain exactly the same authorization, the B
precedence and exact user confirmation, while the prior artifact A remains
version A. The signal event remains ACK-only with no binding/application. Each
native role must have its own real session and exact collector/event identity;
the coordinator must run the revision/candidate/continuation/application commands.
A genuine failed A report and a genuine passed B report are required; if the
model concludes differently the scenario fails. The final B report must resolve
every actual gap from the failed A review by its exact activity/gap/evidence, and
the parent open-gap set must be empty. Coordinator command evidence separately
checks the signal ACK, model-written lineage specs, one successful application
per result, exact queue/delivery identities and chronological lock-busy-only
failures. B acceptance is not inferred from the developer's commit, and no
delivery or permanent evidence archive is created.

Live attempt 10 passed on installed OpenCode 1.19.4 in 260.30 seconds (257.51
seconds in the test), with the selected config bytes unchanged. The full native
sequence, strict command audit and final cleanup passed; no customer delivery
was performed. Failed attempts remain
retained locally and are not counted as acceptance. Attempt 1 hit the former
exact OpenCode version pin; compatibility was subsequently published separately.
Attempt 2 hit a source-access approval request. Attempt 3 failed the exact reviewer
inspection selector; the actual command was not independently retained, so its
cause remains unclassified. Attempt 4 read the development event before the
watcher attached it; the fixture now writes the factual stopped/reaped receipt
immediately and waits for the real coordinator application. Attempt 5 failed a
non-lock-busy application audit, with the underlying error unclassified. Attempt
6 rejected a real report that used confirmation/authorization as check sources;
the prompt now separates those agreements from the three valid basis check
sources. Attempt 7 reached final B approval but failed the strict command audit
on a non-lock-busy continuation attempt; no successful-run claim is made from
that final state. Attempt 8 likewise reached final B approval but failed the
strict audit on a non-lock-busy candidate selection attempt; its exact underlying
error remains unclassified. Attempt 9 also reached final approval but failed
the same strict audit on a revise attempt; its underlying error is unclassified.
Candidate selection, revision and final activity registration
now also have complete JSON interface examples. Bounded private command metadata
now includes failed attempts
as well as successful ones, with selected error classifications rather than raw
output. Complete application/continuation schemas clarify the interface without
weakening product validation or the lock-busy-only audit. A bounded final-file
spec-shape sample reports only fixed field types from canonical paths inside the
coordinator cwd; it does not claim to preserve invocation-time spec bytes.

Independent final local gates passed: stable coverage and Rust 1.85 all-target
suites each reported 498 passed, zero failed, 26 ignored. CRAP evaluated 4805
functions at 77.3% line coverage with zero violations of the absolute threshold
10. Formatting, lint policy, all-target/all-feature Clippy, and rustdoc with
warnings denied passed. Stable and Rust 1.85 release builds passed in 2m44s and
2m53s respectively. The ignored live scenario was run separately as recorded
above; no model run was repeated solely for the final documentation update.

This is not a general authorization or product-policy change, not a customer-asset migration,
and not evidence for pre-POST dispatch ambiguity, takeover, server restart, or
Codex parity. #38/#32 remain OPEN.
