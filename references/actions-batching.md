# Bounded publication with GitHub Actions

This is an explicit alternative quality policy for the existing bounded lane.
Choose exactly one of `gate` or `ci` at `batch-init`; existing Gate lanes are
unchanged and are never migrated automatically. CI mode needs no Gate verifier,
App ID, external Check or manufactured Gate receipt. It does need an authorized
inventory of **all required** PR-head and merge-main workflows and exact job names.
It does not edit Actions, branch protection, required checks or repository policy.
Auto-merge must be disabled; observation and existing-PR publication refuse it.

## Policy and ownership

Use the existing [lane commands](batching.md), exact coordinator identity, queue
lock, current task version, and isolated integration root. Replace the `gate`
property in the initialization spec with this repository's inventory:

```json
{
  "ci": {
    "kind": "github-actions", "version": 1,
    "workflows": [
      {"file": "rust.yml", "event": "pull_request", "decision": "required_jobs", "jobs": ["test"]},
      {"file": "audit.yml", "event": "pull_request", "decision": "required_jobs", "jobs": ["audit"]},
      {"file": "crap.yml", "event": "pull_request", "decision": "required_jobs", "jobs": ["crap"]}
    ],
    "landing_workflows": [
      {"file": "rust.yml", "event": "push", "decision": "required_jobs", "jobs": ["test"]},
      {"file": "audit.yml", "event": "push", "decision": "required_jobs", "jobs": ["audit"]},
      {"file": "crap.yml", "event": "push", "decision": "required_jobs", "jobs": ["crap"]}
    ]
  }
}
```

The canonical policy and its SHA-256 digest are saved in the lane. Unknown fields,
empty/duplicate inventories, mixed Gate/CI policy, and changed digests are refused.
There are 1..16 workflows per inventory and 1..64 unique required names per
workflow. This inventory is operator-authorized input, not autodiscovery or a
claim that arbitrary configured checks cover the repository's requirements.

The existing intake remains authoritative: one active batch, one accepted pending
list (default 3, configurable 1..16), stable `receipt_id`, one-use successor claim,
and released source tasks. Active CI does not serialize independent development.
Pending capacity counts accepted members, not developer sessions or reservations.
CI mode does not add a scheduler, task store or recovery service.

## Example: one active batch and one pending member

For a lane configured with `capacity=1`, use the following sequence. A and B
are distinct development roots; an output member names the exact committed
candidate when it differs from its root's input. This is an example contract,
not a claim that a live campaign has passed.

| Step | Coordinator and producer handoff | Lane state |
|---|---|---|
| Finish A | Actual producer signals A's bound wait; coordinator ACKs, applies acceptance and releases its writer. Verify any fixed-output member through its own wait. | No active batch; pending empty |
| Accept A | `batch-offer` accepts the exact released source and creates its receipt. A successor attempted now is refused for full pending; it consumes no receipt. | Pending A occupies the one slot |
| Freeze A | `batch-freeze` moves A to active; integrate and freeze its snapshot. | Active A; pending empty |
| Register B | `task-add` atomically registers B and consumes the exact accepted A source/epoch/candidate/receipt once. | Active A; pending empty; A receipt consumed by B |
| Develop B | B develops in a disjoint worktree while A remains active. | Active A; development reserves no pending slot |
| Accept B | B returns its own result; after ACK/application, writer release and fixed-output verification, `batch-offer` accepts B. | Active A; pending B occupies the one slot |
| Land A | Observe exact PR-head success, explicitly merge, verify merge-main checks and run `batch-reconcile`. | Confirmed landing releases A and atomically freezes B |
| Continue B | Continue that saved batch through integration, snapshot, publication, observation and landing. | One active B; no third publication |

To check duplicate receipt rejection, attempt another root using A's receipt
after B registration and before B offer, while pending is empty. After B offer
at capacity 1, the full-pending check rejects a new successor first; that result
does not demonstrate the duplicate-receipt check.

Only the registered coordinator mutates task/batch/ACK/application state. A
producer signals with its own identity. An external publisher uses the existing
[PR ledger](pr-ledger.md) and reviewed handoff; it must not impersonate the
receiver or turn terminal admission into task acceptance. An unused wait from
a rejected registration remains explicit state, not a successful claim.

Before publication, include the queue ID/path, lane task/current version, batch
ID/revision, frozen head/tree/base, policy digest and complete PR/main workflow
inventory in the handoff. Name the exact repository, base/head refs, publisher,
authorization and next owner/trigger/deadline. The `authorization` field in the
`pr-begin` spec names a key in `task.authorization` whose `action` is
`publication`; development or
review/commit authorization alone does not satisfy that check. Do not bypass a
missing authorization by creating another publication task or editing queue JSON.

After `pr-begin`, publish once with auto-merge disabled and reconcile the actual
PR before `batch-ci-observe`. Return PR/head and workflow/run/attempt/required-job
identities, then landing commit and merge-main evidence. All configured checks
remain required. An uncertain submission is inspected, never resent; a saved
failure uses the bounded same-PR repair path. Neither CI success nor a publisher
message frees the slot without the landing checks below.

Record when B work actually occurs and A's contemporaneous batch phase. A
predrafted B or two idle panes is not parallel-development evidence; A being
active also does not by itself prove that CI was running during B development.

## Publication and observation

1. `batch-offer`, `batch-freeze`, `batch-integrate`, `batch-snapshot` keep their
   existing ownership, dependency, candidate and private Git checks.
2. In CI mode, `pr-begin` may publish the released `snapshot_frozen` head. This
   creates the PR on which PR CI can run; it is **not** quality or merge approval.
   The external publisher must retain every required check. Uncertain publication
   is still never resent. Same-PR repair updates use the existing PR ledger.
3. `pr-reconcile` must register the exact real PR before `batch-ci-observe`.
   The new action takes the usual `task`, `state_version`, and exact `batch` spec.
   It performs bounded, read-only GitHub queries; it has no background loop and
   does not start/rerun CI or merge the PR.
4. It checks PR number/repository/head/ref/base/ref/open state before and after
   collecting CI. The current target must still equal the frozen base. It uses
   the newest run for each exact workflow/head/event, exact run attempt and job
   identity, head repository and branch (PR source or push target branch).
   PR runs must list the registered PR number. A newest run for another branch/PR
   is refused, not replaced with an older success. It applies the existing
   `required_jobs` decision policy. Two collections must agree. Changed identity, API error or incomplete evidence never passes.
5. The batch becomes `ci_pending`, `ci_failed`, or `ci_passed`; `batch.gate` stays
   null. The latest observation retains the policy digest, exact PR/snapshot/base,
   workflow/run/attempt/job results and observation time. At most 256 observations
   are accepted per active batch, including its revisions. Each successful
   observation refreshes the existing progress deadline; expired CI phases report
   `batch_ci_observation_overdue`. An external owner must supply the next
   trigger/deadline; this command is not an automatic continuation mechanism.

Only required-job `failure` in a completed run with a terminal required-job
decision authorizes repair, and every configured workflow must have a terminal
success/failure decision in a completed run. A terminal failure alongside another
missing or still-running workflow stays pending and creates no repair token.
Missing, cancelled, skipped,
neutral or ambiguous required jobs remain pending/needs-attention evidence.
Non-required advisory job failure does not override successful required jobs;
a still-running workflow is held pending even if its required jobs have passed.

## Repair and landing

A repair intent binds the batch/revision/head/base/policy and exact failure
observation (run/attempt/job/time). The saved failure must be fresh at dispatch;
task/epoch and remote failure identities are checked again at begin/takeover,
repair import and
private-ref update. A new observation, newer run or new attempt invalidates the
old token. Repair limits and same-PR update semantics remain unchanged. A new
snapshot clears prior CI evidence and must be observed again.

`batch-reconcile` releases the slot only after an actual merge, a successful
recorded pre-merge observation, fresh successful PR-head checks, and successful
merge-main `push` checks at the exact landing commit. It also verifies the landing
is on the current target, descends from the recorded base, and has the exact
validated snapshot tree. After merge, the mutable PR `base.sha` is **not** compared
to the old frozen base; the recorded base and landing ancestry are used instead.
Only then are member landings applied and the pending successor frozen atomically.

Successful strict premerge observations record `batch.ci.pr_membership=verified`.
At landing only, after the exact PR is confirmed merged, its workflow runs may
have an empty `pull_requests` array. This is accepted only with that saved marker
and the same successful workflow/run/attempt and required-job identities. Two
fresh observations must agree; repository, head, branch and workflow path still
match the saved binding. Wrong nonempty, missing or malformed membership,
superseding runs/attempts, changed jobs or revoked success retain the slot.
Open-PR observation and repair remain strict. Legacy observations without the
marker retain the previous strict membership check; no marker is inferred or
backfilled. Merge-main checks and ancestry/tree validation remain required.


Merge-main failure/missing checks, revoked head success, tree mismatch or API error
retain the active slot. There is no automatic post-merge repair, replacement PR,
rollback or auto-merge. An operator must inspect the existing PR/check evidence.
No fake success, automatic receipt replacement or dispatch replay is permitted.

## Deployment and evidence limits

Offline fake-API/real-local-Git regressions prove code contracts, not deployed
GitHub permissions or a live bounded-parallel campaign. Before live use verify all
queue writers run compatible claim/schema code, the exact visible receiver and
watchers are healthy, the complete workflow/job policy is authorized, and the
publisher has the existing explicit publication authorization. Preserve the
[claim writer compatibility boundary](claim-admission.md).

Verify on the intended repository's live open, premerge PR runs that
`pull_requests` includes the exact PR number. An empty membership list is rejected
even if the branch and commit match; offline fixtures and merged historical runs
cannot establish this deployment prerequisite.
The external publisher must create PRs with auto-merge disabled. The observer
rejects auto-merge on an existing PR, but cannot prevent an externally created PR
from auto-merging before its first observation; that case retains the slot.

A Gate-mode deployment still requires its own [consumer verifier/App evidence](gate-client.md#consumer-deployment-evidence).
Those Gate-specific prerequisites are not CI-mode inputs. Changing an existing
lane policy or switching a live runtime is a separate deployment action. This
implementation alone does not complete #112's live acceptance or close #38/#32.
