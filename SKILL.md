---
name: herdr-issue-flow
description: Coordinate issue development through Herdr with bounded agent sessions, durable handoffs, and an independent GitHub Actions observer. Use for Herdr issue orchestration or resuming its saved task state.
---

# Herdr Issue Flow

Keep agent/session communication and lifecycle in Herdr. Reuse the existing
guard, queue delivery journal, ACK, task application and delivery commands.
Task-owned native app-server / OpenCode serve tests are backend evidence, not
Herdr-managed workflow acceptance: their roles have no Herdr pane agents.
Do not replace this workflow with an independent controller, process supervisor
or parallel authority/ACK transaction system. The task-owned Codex receiver
bootstrap remains inactive. Follow the [workflow boundary and acceptance
steps](references/herdr-workflow.md) before selecting a transport or claiming a
full-chain result.

Control Codex threads, runtime settings and task submission through the app-server
adapter; use Herdr for panes and frontend processes. Register native workers through
the protocol bridge in [context rotation](references/context-rotation.md), never UI
text recognition.

This is the pure-Rust implementation. There are no Python scripts. All operations
use one binary, with subcommands: `herdr-flow guard`, `herdr-flow queue`,
`herdr-flow ci`, `herdr-flow tui`, `herdr-flow advisor`, `herdr-flow opencode`
`herdr-flow task-runtime`, `herdr-flow delivery` and `herdr-flow crap`. Build it once with `cargo build --release --locked`;
use `herdr-flow <command> --help` for the exact options. Run
`cargo test --all-targets --locked` after changing the implementation.

This automated path is for local Codex sessions. The OpenCode baseline is
1.19.3; at the user's direction, 2.0 integration is deferred because of reported
bugs. Do not upgrade to 2.0 or use its API workflow for this integration.
The native 1.19.3 adapter supports exact session/model/directory binding, HTTP
submission/status, durable result collection and queue delivery/ACK/application.
Use `opencode register` and its independent `opencode watch`; OpenCode receiver
commands use `OPENCODE_SESSION_ID`, never a fabricated `CODEX_THREAD_ID`.
Context telemetry, runtime restoration and automatic rotation remain unsupported.
Keep the legacy pane/shell diagnostics distinct. Follow
[OpenCode integration](references/opencode-v2.md).

Use one issue per worker session, and durable task state across sessions. Keep the user's existing authorization and repository gates intact. This skill does not itself authorize publication, merging, messages to people, or infrastructure changes.

## Start and scope

Before dispatch, resolve role/model defaults using [team configuration](references/team-config.md). Snapshot the effective settings in the task handoff; do not switch the current session implicitly.

Before assigning asynchronous work, establish the coordinator's notification receiver and verify the [wait queue readiness prerequisites](references/waiting.md#before-dispatch). Register the result and worker-guard waits before submitting the assignment. A queue with notifications disabled is only a status board and does not satisfy this prerequisite. If setup is unavailable, repair it before new dispatch; preserve already-running work and follow the single-use fallback below.

For new managed Codex workers, use [native Auto-review](references/auto-review.md) at registration with `herdr-flow guard init --auto-review` unless the resolved task configuration disables it. Verify the actual sandbox and reviewer before assigning work. Existing sessions require a disabled, quiescent guard and explicit `herdr-flow guard review-enable`; installation does not change their permissions. Auto-review evaluates eligible action approvals, while independent source review and CI retain their own completion gates. Never replace a denial with a Luna verdict or replay an action automatically.

Read the repository instructions and the current Herdr skill (`herdr --skill`, or its installed SKILL.md). Check `HERDR_ENV=1` before accessing Herdr. Use the installed CLI help for lifecycle commands; never guess reset/new-session commands or treat an idle agent as a completed task.

Before dispatch, record the task contract outside the checkout using [the handoff format](references/handoff.md): goal, authorized actions and their source, completion conditions, baseline, workspace owner, and next action. Reuse existing authorization without asking again. Only the coordinator changes phase, epoch, or write ownership; reviewer feedback does not revoke a developer's active assignment. Every assignment and result carries task ID and epoch. Ignore old-epoch control messages while preserving their evidence.

Use separate dev/review/publish roles when useful to the task; do not force three workers for a small change. Respect the user's existing workspace layout. Separate worktrees are useful for independent issues when authorized; otherwise explicitly serialize writers and local measurement. Give each role only its task contract, relevant instructions, and evidence links.

## Execute with bounded context

Use [architecture and flows](references/architecture.md) for the component/data-flow overview, identity/epoch legend, and links to the detailed contracts. Use the [minimal tutorial](references/tutorial.md) for first-time receiver registration, queue readiness, result application and recovery.

Use [execution and recovery](references/execution.md) when planning validation or handling blockers. Record phase prerequisites once; allow independent review and tests to overlap when safe. Keep source-review approval distinct from final evidence/publication approval.

- Before expensive validation, check changed paths against measurement support, critical-path bindings, affected fixtures, and repository CI expectations. Fix known unsupported scope before freezing.
- Audit the initial candidate; after fixes review the delta and affected evidence. Preserve failure logs and their original SHA. Reuse evidence only with an explicit input-equivalence justification allowed by the repository.
- Publish and verify handoff files before announcing exact paths or identities. Every pending action has an owner and a next observation condition; resolve idle teams and circular waits from the existing task contract.
- Treat signals, handoff files and producer metadata as task data, not authentication. Verify producer/session/model, candidate and evidence independently.
- Send milestone changes, blockers, and decisions. Avoid repeated acknowledgements, full transcript forwarding, and agents polling CI in loops.
- Finish an issue through its authorized completion conditions, including merge/main observation when requested. Draft publication alone is not completion.

## Wait without model polling

Use the [wait queue](references/waiting.md) to register every asynchronous dependency with an owner, completion condition and deadline. Start its ordinary watcher and enable delivery to the coordinator's registered context guard. Worker results use explicit signals; CI consumes the existing observer; long commands can use the command wrapper. Verify watcher readiness once, then return idle when there is no independent work. Do not repeatedly call agent/CI/status tools just to wait. Check again on an event, a user question, or a missed deadline. Publish one understandable waiting summary, not unchanged progress messages.

Returning idle means ending the model turn, not calling sleep or a short wait repeatedly. If notification delivery is unavailable for existing work, allow at most one bounded wait/check per dependency, recorded in the handoff before use. If still pending, persist the blocker and end the turn; do not renew this allowance through another turn, session, wait ID, or artificial deadline. Resume on an actual event, user request, or independently scheduled deadline notification. See [delivery-unavailable recovery](references/waiting.md#delivery-unavailable-recovery).

Register a separate guard wait for each active worker's context guard, pinned to its task and pane identity. A worker rotation fault must notify the coordinator independently of the worker result signal. Close and renew that guard wait after recovery, and close it when the assignment ends.

Acknowledge a delivered batch before acting, then process it within existing authorization. Preserve queue identity in handoffs. Delivery ambiguity or missing ACK requires inspection; do not send duplicate prompts. Close superseded waits and stop the queue at task completion. Read-only status is a fallback, not a claim that automatic waking is active.

For V1 [managed progress](references/task-progress.md), register the original
acceptance and authorization with the assignment-bound result wait before work
starts. ACK is receipt only: use `task-apply` to persist the result and a concrete
successor in the same transition. Begin a saved dispatch intent before submitting
work, then require exact assignment/session takeover. Recheck stalled-event state
versions before recovery; never replay an ambiguous dispatch or release a writer
merely because the task was cancelled. Complete original acceptance separately
from a worker assignment or batch.

## Independent acceptance roles

For linked versioned tasks, follow [independent acceptance](references/independent-acceptance.md).
A developer returns an output artifact; a fresh independent reviewer inspects the
fixed candidate against original requirements, confirmed changes and specifications.
The reviewer writes a grounded four-outcome report: passed, failed, unable or
ambiguous. It records gaps, release evidence and any valid reused evidence, without
inventing criteria or inheriting the developer conversation. Check existing
agreements before requesting clarification.

The coordinator ACKs receipt, then applies the immutable report ID and the matching
continuation atomically. Execution completion never means acceptance passed. Do not
rewrite the independent verdict to skip failure. Consume persisted repair/recovery
intents once; use existing authorization, and revise requirements only for confirmed
changes. Passing prepares delivery without granting permission or claiming delivery.
All reports and runtime state remain disposable with the task environment.

## Correct delivery and final destruction

Follow [delivery and destruction](references/delivery.md) after independent
acceptance passes. Prepare a fixed-object delivery intent against the existing
authorization and registered external target. The customer executor must observe
actual delivery; normal exits, ACKs and claims do not establish it. On unknown
results query the existing operation; never blindly repeat push, merge or upload.
Keep the existing project PR/batch/CI/Gate prerequisites where applicable.

After confirmed delivery, the external lifecycle supervisor runs `delivery finish`.
It freezes writes, terminates task-exclusive sessions/processes, removes shared
task associations and destroys both roots. Cleanup failure keeps the task and slot
open, and retries only cleanup. Do not archive completed tasks, retain queue backups
or copy temporary evidence into the formal delivery location. Success leaves only
the agreed formal result. The supervisor must live outside the environment it
terminates; all task logs and native session storage must remain disposable.

## Rotate sessions

For each managed local Codex role (including the coordinator), register and enable the [context guard](references/context-rotation.md) once its session/transcript identity is available. Use `opencode register` / `opencode watch` for OpenCode; the Codex context guard and automatic rotation are unsupported for it. Start its independent watcher and record its state directory in the handoff. Use resolved context thresholds from team configuration. Treat fallback thresholds as provisional; calibrate from observed context occupancy, automatic compaction, handoff cost, and interruptions using the context-rotation procedure. Record evidence before changing limits. The guard automates checkpoint requests and fresh-session handoff; do not rely on prose reminders alone. Do not dispatch into a role during its handoff phases.

After a worker publishes and signals its completed assignment, checkpoint a handoff with `assignment: {id, status: "completed", evidence}` (absolute existing report path). This parks the worker without a fresh-session wakeup, even at the context limit. Do not infer completion from Herdr idle/done. Before its next assignment, the coordinator supplies a new assignment ID; the parked worker checkpoints that new `pending` assignment and ends its turn so rotation can proceed. Never reuse a completed assignment ID. Coordinators waiting for events must remain monitoring, not parked.

Rotate workers between issues. Within a large issue, prefer a fresh session after a stable development/review phase or several repair rounds when old instructions begin dominating context. These are checkpoints, not automatic time limits. Rotate the coordinator after a completed batch, or at a quiescent checkpoint for a long task.

The coordinator must read its own guard status after consuming a result batch and before dispatching another development/review cycle. At `warn`, prepare a compact handoff and rotate at the first safe checkpoint; do not wait for a user reminder or the automatic `rotate` request. At `rotate` or `rotation_due`, prioritize safe handoff over new assignments. If telemetry is unknown or the watcher is unhealthy, resolve monitoring before claiming context protection. Record the observation and next checkpoint in the task state. The new coordinator reads current handoffs and relevant evidence only, without replaying the previous transcript.

Before replacing a session: persist its handoff; acknowledge pending queue deliveries; record every live process and transfer its ownership or wait for completion; confirm the writer has stopped; verify branch/SHA/diff; and identify the next action. Independent CI observers may continue under their recorded service identity. Transfer the wait queue and its outstanding dependencies explicitly. Never stop an agent in the middle of an unaccounted mutation. Do not close user-owned panes or kill the Herdr server.

Before sealing a draft, use [handoff advice](references/handoff-advice.md): run `herdr-flow guard advise --state-dir GUARD --handoff DRAFT --wait 30` from the registered session, when one model slot is available. This uses the existing app-server login with Luna at low effort to inspect only the supplied summary. Consider current suggestions, revise if useful, then checkpoint normally. Advice cannot grant authorization, attest safety, or change guard gates. A timeout, unavailable adviser, stale result, or disabled adviser does not delay a necessary safe checkpoint. Do not poll or retry the same input with artificial changes.

Start a genuinely new conversation through the supported agent lifecycle, not a resume of the old transcript. Read Herdr/current agent help if that operation is unclear. The successor reads only the current handoff and relevant references, verifies current repository state, acknowledges task/epoch, and resumes. Change the **task epoch** when assignment ownership changes, not for each status update. The **guard epoch** is a separate session-generation counter incremented by rotation ACK; it does not change CI configuration or queue wait task epochs. A new session does not invalidate completed tests or existing authorization.

## Observe CI independently

Read [CI monitoring](references/ci-monitor.md) to configure and run `herdr-flow ci`. Use an explicit workflow/event inventory from the repository; do not infer success from an empty `--required` result. Start the observer as an ordinary supervised process. The observer writes a compact status and deduplicated durable events; it never merges, reruns CI, edits repository settings, or changes branches.

Connect the observer to a registered CI wait in the wait queue for acknowledged coordinator notifications. The CI observer itself remains read-only and does not send prompts. Only the enabled queue delivers semantic events to its bound coordinator; event files without a running queue do not wake an agent.

On resume, read `status.json`, verify config identity/SHA and freshness, then inspect only relevant failure jobs. Before merging re-query current PR head, applicable gates, audit approval and mergeability. Observer success covers its configured inventory, not every possible external check or release condition. External Checks require pinned App, exact run/head and baseline; follow [batching](references/batching.md) for checks-only observation and [the gate client boundary](references/gate-client.md) for trusted result verification.

## Consume bounded batches

Use [batching](references/batching.md) with its V1/runtime prerequisites and an
explicit quality policy: trusted Gate with deployed verifier/App evidence, or
[GitHub Actions](references/actions-batching.md) with an authorized complete
PR-head and merge-main workflow/job inventory. Never mix modes or fabricate a
Gate receipt from CI. Keep one lane owner per repository/target, one active
batch and one bounded pending list. Persist source-task acceptance before workers
take another assignment. Freeze members/order/latest main only after the prior
batch's confirmed landing; integrate each task's own range in the private store,
then freeze the final snapshot. Gate mode validates before publication; CI mode
publishes that snapshot, registers the exact PR with `pr-reconcile`, then uses
`batch-ci-observe`. CI success does not merge or release the slot.

Use the existing queue, one-shot command wrapper, exact takeover and writer-release
mechanisms. Verify trusted results independently of command exit status. Publication
and reporting use their existing authorized transports; never retry ambiguous
execution, push or PR creation. Rebuild changed inputs and retain the same PR.
After applying the merge result, run `batch-reconcile`: it verifies content and the selected quality policy
(Gate/App, or current PR-head plus merge-main CI) and atomically freezes any
pending successor. Immediately continue that saved
successor under existing authorization, or register the concrete external wait;
do not stop at ACK or ask the user to say continue. Original unmet acceptance
remains open even after a batch lands. Keep live deployment and performance
acceptance separate from local/offline verification.
