# Execution and recovery

Read when planning validation, coordinating a failure, or diagnosing a stalled
handoff. Repository-specific requirements override these scheduling defaults.

## Explicit prerequisites

Record phase prerequisites once in the task contract. Avoid introducing new
approval checkpoints through ambiguous messages such as "wait for review".

| Action | Default prerequisites |
| --- | --- |
| Read-only review | Identified candidate and review scope |
| Targeted validation | Stable candidate, released writer, allocated local resources and authorized validation plan |
| Commit/push | Applicable local validation and the review required by repository policy; existing publication authorization |
| Draft PR while local evidence is pending | Publication authorized, candidate immutable, repository permits it; pending evidence clearly disclosed |
| Merge | Current head matches reviewed candidate, required evidence and applicable checks complete, final release review when required, merge authorization |
| Next issue writes in shared checkout | Previous checkout users explicitly released it; next base and writer assigned |

Review and validation may proceed concurrently on a stable candidate when they
do not mutate the same inputs or compete for exclusive measurement resources.
A source-review PASS is not final evidence approval. Conversely, final evidence
approval must not become a prerequisite for generating that evidence. Distinguish
source review, validation, and final publication review in every PASS/FAIL.

One assignment may authorize the whole sequence with these prerequisites. Once
met, the assigned role proceeds without repeated coordinator GO messages. The
coordinator intervenes for a changed candidate, blocker, ownership transfer, or
scope change. If the coordinator must edit, first transfer the writer explicitly;
it must not quietly become a second developer.

## Failure triage

Record the failing command/job, first useful diagnostic, exact input identity,
evidence path, owner and next action. Classify before choosing recovery:

- Product or test defect: dev proposes a focused repair; reviewer checks the
  changed assertions against the intended behavior. Preserve the failing input.
- Validation-tool/metadata defect: fix that producer or reader; do not label it a
  product regression or rerun unaffected product suites merely to repair a report.
- Transport/auth/platform issue: identify the actual failed request or platform
  requirement. Retry within the existing scope after evidence of a transient
  failure or relevant state change. A denied API call is not a successful setting
  change. Honor explicit user reports that permissions were changed and recheck.
- Scheduling/visibility delay: inspect workflow runs and their queued/running
  state before claiming CI did not trigger. Missing job rollups are neither PASS
  nor proof that workflow dispatch is necessary.

Stop dependent actions on failure; unrelated remote jobs may finish and retain
diagnostics. A local process that still owns the checkout must finish or be safely
stopped before repair. Avoid treating every failure in a matrix as one root cause;
cluster by actual diagnostics. Preserve raw stdout, stderr and exit status.

Before broad or costly tests, run the smallest check that can expose the suspected
failure. Cross-platform changes need platform-specific fixture assumptions checked
early (child stdout, IPC readiness, file lifetime, compiler/tool versions). Linux
success alone does not establish Windows/macOS acceptance. Use existing CI paths
for early platform feedback when authorized; avoid extra validation PRs unless a
concrete platform requirement justifies them. Record any auxiliary PR's purpose,
relationship to the delivery PR, owner and cleanup/closure action.

## Audits that converge

Each blocking finding needs location, violated acceptance/policy rule, evidence or
reproducer, and the condition that clears it. Label optional improvements and
unverified hypotheses separately. Reviewers may flag new substantive defects at
any stage, but should not add speculative acceptance criteria during final closure.

Batch findings from the current review pass when feasible. Re-review the changed
portion and its dependencies after a repair, referencing prior unchanged conclusions.
If the same concern returns without new evidence, resolve the disputed assumption
or run one discriminating check; do not circulate the same HOLD between roles.
Broader architecture work requires a demonstrated need for this issue or a separate
follow-up. Do not create that follow-up externally without applicable authorization.

Use Git commit/tree and existing manifests for identity when they suffice. Do not
invent several equivalent freeze formats or duplicate full evidence archives for
routine changes. Preserve the repository's real certification requirements. Choose
one evidence owner and reuse their retained downloads after appropriate verification;
transport diagnostics should not turn into an unrelated downloader development task.

## Publish a handoff, then announce it

Write files, finish them, validate paths/identities, and atomically publish the
handoff before sending its location. Never predict a timestamped path or paste a
placeholder SHA as an actual result. Give readers the exact manifest path rather
than asking them to search directories. Summaries derive counts from actual results;
state omitted, failed and unrun checks explicitly.

Report task, epoch, role, phase, result, next owner/action and evidence link. Avoid
broadcasting full hashes/logs to every agent. The coordinator owns the current
summary; detailed records remain with their evidence owner.

## Detect stalled coordination

Every pending action has an owner, a concrete dependency, and a next observation
condition (process completion, run change, promised deliverable or bounded deadline).
If an event or permitted checkpoint reveals all agents idle while the task is
unfinished, inspect current state then. Do not poll agent states to detect this;
use the registered result/guard waits and deadlines in [waiting](waiting.md).
If two roles wait on each other, resolve the cycle from the recorded prerequisites
and existing authorization. Agent idle/done is not an issue completion signal.

When a pane looks idle, use the persisted task and delivery diagnosis rather than
the terminal label. `herdr-flow queue status` distinguishes an executing owner, an
unapplied result, an unaccepted successor, an uncertain dispatch, a registered
wait and an explicit blocker; `readiness` lists actionable `next_steps` for any
receiver gap. Do not resend an uncertain delivery, apply an ACKed result only by
`task-apply`, or turn a valid long command into a restart merely because the
pane is idle.

At a missed checkpoint, request a concise blocker and next concrete deliverable.
Narrow an expanding investigation to the next discriminating experiment or patch.
Do not interrupt a valid long-running test merely because its wall time is large.
User-facing updates explain what is blocking completion and what resolves it;
internal freeze identifiers alone are not useful progress reports.

## Lightweight metrics

Use the existing durable records for a read-only report:

```bash
herdr-flow queue metrics --state-dir /absolute/queue
herdr-flow queue metrics --state-dir /absolute/queue --json
```

It reads `queue.json`, optional `readiness.json` and the registered guard state;
it never writes, dispatches, applies, approves or reruns work. The report keeps
per-task segments and also aggregates completed samples:

- `ready_to_delivery`: event `observed_at` to delivery `sent_at`.
- `delivery_to_ack`: delivery `sent_at` to `ack_at`.
- `ack_to_applied`: ACK to event `applied_at`; ACK remains receipt only.
- `prepared_to_started` / `prepared_to_accepted`: successor intent, dispatch and
  exact takeover from operation timestamps.
- `accepted_to_next_result`: takeover to the next event observed for that wait.
- `created_to_first_result`, `created_to_completed` and
  `last_applied_to_completed`: task-level continuity.
- Counts cover events, applied/pending/archived/stale results, failed,
  decision-needed and timeout outcomes, operations, recorded recovery/control/
  writer-release transitions, and `progress_stall` reasons.

Missing timestamps display `unknown` with a reason, not zero. A span whose end
precedes its start is a reported clock anomaly, not a negative duration.
Ongoing tasks show `ongoing_since_created` separately and are excluded from
completed-sample aggregates; open waits show age, deadline and overdue state.
Chat urging and unrecorded manual work cannot be derived from the queue; the
report says so instead of inventing a count. Duplicate delivery associations are
disclosed.

The capability block reports the queue watcher, notification state, guard
watcher, receiver identity, optional readiness (`active_wakeup`,
`safe_rotation`, gaps, next steps) and context telemetry/auto-rotation. It
records availability, not a claim that a capability is safe for a new dispatch.

Statistics are observations: they do not replace acceptance, validation or the
stall detector. The queue does not contain build/validation resource usage, so
those values are reported as not derivable rather than zero; use command or CI
evidence when they are needed. Chat urging is likewise not recorded. A
successful notification is transport state, not proof of application,
takeover or completion. Keep raw logs, full sessions and private paths outside
GitHub. Retain the compact task report and referenced evidence IDs while the
task is active or unresolved; they must not become a permanent task archive.
After confirmed delivery, dispose of task-owned records through the supported
[delivery and cleanup lifecycle](delivery.md). If ownership or cleanup support
is missing, report the blocker and preserve the affected resources; this text
does not authorize direct deletion. See [task progress](task-progress.md) for
the underlying state and recovery contract.

Shared OpenCode transcripts and caches are shared resources outside this task's
evidence and cleanup authority. Keep them untouched. Separate repository
development/CI validation evidence remains available while its review is pending;
that review retention does not make task-owned runtime evidence permanent.
