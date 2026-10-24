# V2 consumer development and deployment boundary

This record describes the original **Gate-mode** consumer and its historical
evidence. The separate [Actions policy](actions-batching.md) reuses the lane but
requires an explicit workflow/job inventory instead of Gate verifier/App evidence.
Neither mode is activated by installing code; offline checks are not live rollout
or bounded-parallel acceptance.

The consumer implementation extends the V1 loop without changing GateHouse,
repository rules, remote publication or App credentials. Its CLI and recovery
contract are in [batching](batching.md); the operator-managed client boundary is
in [gate client](gate-client.md). This record does not close #12 or attest a
production rollout.

| Area | Development verification |
| --- | --- |
| External Checks | Checks-only CLI using a fake GitHub transport; pinned App/head/base/run; newer attempt, wrong source, revocation, neutral/skipped, suite-window overflow and configuration errors; direct observation rejects malformed repo/PR/SHA before API access |
| V1 reuse | External failure through existing result application, repair begin/exact takeover and attempt consumption; old external run cannot authorize repair |
| Publication validation | Shared read-only checks load without either state machine; cold imports preserve batch gate/review/revocation checks; V1 publication also requires released writer, applied results and resolved blocking findings |
| Bounded intake | Capacity refusal preserves source handoff; single member proceeds; exact applied source event, acceptance mapping, unique own ranges and declared dependencies; lane idle required at offer and member idle rechecked through publication |
| Member ownership | Accepted/frozen members cannot resume, dispatch, begin/accept legacy dispatches, reserve writers, enter V1 PR publication or own another lane; confirmed landing still allows original acceptance completion |
| Integration | Real Git source ranges, squashed predecessor, dependent members, private index, conflict preservation and deterministic commit journal |
| Crash recovery | Coordinator loss after private ref movement; actual task router reloads saved intent without duplicate commits; loss at the first conflict checkpoint reloads matching candidate/token/blocker and completes a V1 repair |
| Repair | Conflict and gate-failure workers use V1 dispatch/accept/result/release, isolated checkout and journaled ref update; repaired snapshots require a new gate |
| Gate transport | Actual one-shot command wrapper cannot grant quality approval; an independent stdio verifier is queried after result application/writer release |
| Lifecycle | Same PR on changed main; rebase retains previous landing ancestry and refuses an accepted repair awaiting import; uncertain gate needs trusted quiescence; closed-unmerged keeps its slot; expected Check and landing tree required for release |
| Continuation | Confirmed landing, member maps, slot release and freezing the pending successor on latest main are atomic; original acceptance gaps remain open |
| Runtime | Existing queue watcher deadlines/deduplication; paused/registered waits stay silent; missing notification readiness prevents new asynchronous gate intent |

Tests use temporary repositories, injected read-only GitHub adapters and a separate
stdio service fixture. They do not demonstrate protected host isolation, actual
App checks or real gate coverage. No source was merged to production or policy
migrated as part of this development.

The 2026-10-18 follow-up audit fixes and the 2026-10-19 security hardening were
verified by the **upstream Python** suite (428 tests at that snapshot). The Rust
port has 206 tests and does **not** contain equivalent regressions for all six
ownership/publication/conflict/rebase fixes; the 2026-10-19 Rust audit identified
unported batch repair/rebase gaps. The Rust tests do cover the related intake,
repair and direct-observer validation guards and the command/path/JSON boundary
tests. Do not read the Python 428-test result as Rust acceptance.
Among the earlier nonblocking improvements in `v2-review-feedback.md`, configurable
verifier timeout and fully configurable Git identity remain outstanding; Git
diagnostics and Check suite-window bounds are addressed.

The following remain required before V2 production enablement/issue closure:

- Confirm the V1/runtime live prerequisites, including safe session takeover, from
  their retained acceptance evidence rather than interpreting unit tests as live
  notification or rotation proof.
- Bind existing authoritative GateHouse results/current state through the
  operator-owned consumer client; validate policy/run/base/tree identity,
  invalidation and quiescence. Record the [consumer deployment evidence](gate-client.md#consumer-deployment-evidence);
  upstream issue closure or an installed Gate CLI is not deployment acceptance.
- Exercise real normal, failed/repair and interrupted publication/reporting paths
  with the actual restricted publisher and trusted App, including same-result
  report recovery without duplicate gate execution or PR creation.
- Confirm complete migrated validation coverage and current repository source/
  baseline protection before removing duplicate Actions.
- Run the #12 same-host cold/hot/source/toolchain/dependency/configuration cache
  measurements and compare sequential versus batch per-delivered-task resources,
  waiting, repairs, cancellations, cache maintenance and manual intervention.

These are deployment/pilot acceptance requirements, not optional observations.
Raw transcripts, private runtime identities and logs remain outside the checkout.
