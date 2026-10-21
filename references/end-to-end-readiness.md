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
