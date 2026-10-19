# V1 local PR ledger and repair intents

This is the local/offline single-PR component of #11, after the result/takeover
slice. It reuses `queue.json`, the queue lock, exact receiver identity, task state
versions, existing CI observer, dispatch intents and writer reservations. It has
no external write commands and never enables auto-merge, pushes, creates a PR,
changes rules, or publishes Checks. Those actions need their existing specific
authorization and an external publisher. GateHouse #98 trusted execution/reporting
remains separate; [V2 integration/batching](batching.md) reuses this ledger.

Use `herdr-flow queue COMMAND --state-dir /absolute/queue --spec
/absolute/spec.json`. Every command below requires the current coordinator and
`task`/`state_version`. The queue persists one atomic update after all checks;
errors leave the saved state untouched. Inspect the current version after an
error or interruption.

## Provenance and publication recovery

First register an ordinary managed task with its original acceptance,
authorization, full candidate SHA, and result wait as described in
[task progress](task-progress.md). `pr-prepare` adds this record **before** any
external publication:

```json
{
  "task": "owner/repo#123", "state_version": 1,
  "repo": "owner/repo", "host": "github.com", "issue": 123,
  "checkout": "/absolute/task-checkout",
  "original_base": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
  "head": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
  "own_commits": ["bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"],
  "dependencies": [], "base_ref": "main", "head_ref": "task-123",
  "target_base": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", "max_repairs": 2
}
```

`original_base` identifies the source range; `target_base` identifies the target
branch version used for validation. They may differ for dependent work. V1
requires a nonempty linear range (at most 256 own commits), exact oldest-first
commit list, matching origin and absolute checkout root. Missing source, merges
inside the range and ambiguous scope fail. Fork PR publication is not supported
by this first adapter.

Dependencies explicitly name registered task/head pairs, for example
`[{"task":"owner/repo#122","head":"FULL_ORIGINAL_TASK_HEAD"}]`. Each dependency
head must be at/before this task's original base. Its confirmed PR landing map is
retained separately, including a squash landing with a different SHA. Thus B
based on A names A's head as its original base and lists only B's commits. This
does not implement rebasing/integrating B after A lands; a changed target base
requires update/revalidation rather than inheriting an old green result.
Publication and observation refuse to advance while a dependency is pending,
changes its source head or pinned landing, or has no confirmed landing ancestry
on the validated target. Repair dispatch and takeover recheck dependencies too.

`pr-begin` with `authorization` referencing an existing exact `publication`
action persists `sending` before the caller uses its authorized publisher. A
returned command does not prove PR creation. Interrupted sends remain uncertain;
calling begin twice is refused.

`pr-reconcile` uses GETs to recover the existing PR by exact repo/head branch/base
branch (including closed PRs), then verifies full head. Multiple matching PRs are
ambiguous. No match records `publication_unconfirmed` and does not reset the send
or grant permission to create another. Once registered, the exact PR number is
retained for all repairs. One queue refuses two task owners for the same PR.

Checks satisfied, native auto-merge enabled, and confirmed merge are distinct.
Reconciliation records head/base changes, conflicts, disabled native auto-merge
and closed-unmerged as decisions. A merge is recorded only after GitHub confirms
`merged`, a landing SHA, and that SHA's ancestry on the current target branch.
Original task acceptance still requires separate result application; PR-managed
tasks cannot complete before confirmed landing. Queries never execute merge.

## Required failure to one isolated repair

`pr-observe` takes `source`, the absolute existing CI observer directory. It
reconciles the PR, verifies the observer config hash/task/epoch/repo/PR/head and
explicit `required_jobs` or pinned external Checks inventory, accepts observations
up to 120 seconds old, then queries latest workflow run/attempt or App/check-run
identities. External checks additionally bind target baseline and trusted run
`external_id`; see [checks-only observation](batching.md#execute-the-gate-and-publish).
It retains exact failed
required job IDs and a deterministic failure token. Advisory, cancelled, skipped,
missing, API, changed-base and unsupported integration results do not produce an
automatic code repair. Disabled native merge/conflicts remain explicit decisions.
Additional failed jobs in the same run/attempt cohort retain the failure token;
changing the observer config cannot bypass an already consumed failed attempt.

Register the CI wait to this task/current assignment so the existing watcher
delivers its failure. ACK remains receipt. Inspect the diagnostic, then
`task-apply` the failed result with the normal `next_action: dispatch` fields and:

```json
{
  "pr_repair": {"number": 7, "head": "FULL_HEAD", "base": "FULL_TARGET_BASE", "token": "RECORDED_TOKEN"},
  "repair_diagnosis": {"kind": "code", "evidence": "/absolute/inspected-diagnostic.json"}
}
```

Use an isolated repair checkout, a fresh assignment and registered pinned result
wait. The intent includes the exact authorized action, failure/run/attempt/job
and the existing single writer reservation. A failure token is used at most
once; `max_repairs` is 1..10. A writing PR dispatch requires this binding; a
read-only review can set `writes: false`. `task-begin` rechecks current PR head,
target base and latest run/attempt before recording `sending`. Normal exact
assignment/session takeover is required before work. The observation must still
be current at takeover; inspect and refresh the observer/ledger if it expired,
then accept the existing operation, without resending it. An ambiguous send is
never reset, even when its deadline expires. Pause/cancel never releases a writer.
Takeover also queries the current PR and latest run/attempt before authorizing work.
Any `pr_repair` field on a read-only dispatch is rejected, including empty values.

After the accepted repair publishes its result, ACK/apply it with inspected
writer release and a concrete blocked next state for publisher verification.
`pr-update` requires that applied exact successful repair event, operation,
isolated `checkout`, `head`, full `own_commits`, and verification `evidence`. It
requires the repair to append to the original own range and persists the update
intent before publication. It retains the original provenance and previous
observations, advances the task candidate, and clears old candidate acceptance
claims. The original acceptance inventory and authorization remain intact.

With publication authorization, `pr-begin` for this prepared update rechecks that
the same PR is open at the expected prior head/base. The external publisher must
also enforce its applicable gates and head/ownership preconditions at the actual
write. `pr-reconcile` finds the new head on the same PR; an old head remains
`update_unconfirmed`, never an invitation to replay the push. Register a fresh
observer configuration/wait for the new SHA and resume the blocked task through
`task-resume`. Old events cannot advance it.

## Current evidence boundary

Tests use real temporary Git repositories, atomic queue files and result/ACK/
application/takeover transitions with an injected read-only GitHub adapter.
They cover a happy-path prepare/observe/reconcile flow, repair-intent units and
some writer/head/base boundaries. They do **not** cover same-PR failure repair,
publication or dispatch interruption, duplicate/head/attempt recovery or the full
V1 regression set; the Rust audit found these unported. They do not establish a
live GitHub publication, repair, auto-merge, trusted Check or running-watcher
reload. A fresh session reuses unchanged evidence; raw logs and task state stay
outside the repo.
