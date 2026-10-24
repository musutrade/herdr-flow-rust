# V2 bounded batch consumer

The consumer is implemented here. GateHouse owns quality evaluation and its
results; the consuming deployment owns the verifier binding, Check publication,
App credentials and publication workflow. Production use requires V1/runtime
acceptance and the [consumer deployment evidence](gate-client.md#consumer-deployment-evidence)
for this repository. Closing an upstream issue does not supply that evidence.
Offline fixtures establish neither deployment nor publication authorization.
Installing this code changes no rules or Actions workflows.

## Register a lane and accept tasks

All commands use `herdr-flow queue COMMAND --state-dir QUEUE --spec
SPEC.json`, the existing queue lock and exact coordinator identity. Register a
managed lane task with its original acceptance/authorization first. Every spec
requires that `task` and current `state_version`; active-batch commands also
require its exact `batch` ID. Use `batch-init` with:

```json
{
  "task": "owner/repo#12-pilot", "state_version": 1,
  "repo": "owner/repo", "base_ref": "main",
  "workspace_root": "/absolute/private-runtime/integration",
  "capacity": 3, "max_repairs": 2, "progress_timeout_seconds": 600,
  "gate": {
    "verifier": ["/absolute/operator-managed/gate-client"],
    "policy": "DEPLOYED_POLICY_ID", "app_id": 123,
    "check_name": "DEPLOYED_REQUIRED_CHECK_NAME"
  }
}
```

Use actual deployed identities. The integration root must be separate from the
coordinator/developer checkouts; other tasks cannot reserve a writer in it.
The verifier and executable arguments must come from outside member code. Protect
the wrapper/configuration/service independently of agents; same-user directories
do not establish that isolation. One queue refuses duplicate repository/host/
target ownership. Multiple queues still need an external owner contract.

Apply a successful source result through V1, inspect writer release and leave
the source task blocked at its publication handoff. `batch-offer` takes:

```json
{
  "task": "owner/repo#12-pilot", "state_version": 2,
  "member": {
    "task": "owner/repo#101", "state_version": 5,
    "event": "APPLIED_SUCCESSFUL_SOURCE_EVENT",
    "checkout": "/absolute/development/task-101",
    "original_base": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
    "head": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
    "own_commits": ["bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"],
    "dependencies": [], "acceptance": ["ORIGINAL_CRITERION_ID"],
    "evidence": "/absolute/runtime/task-101-handoff.json"
  }
}
```

Own ranges must exactly match the nonempty linear `original_base..head`, oldest
first. Dependencies name exact earlier accepted task/head pairs or confirmed
landings on this target. For B based on A, list only B's own commits and use A's
original head as B's base; keep A's squash landing separate. Candidate, epoch,
applied source event and original acceptance IDs are checked.

Acceptance saves the member receipt with both tasks atomically. Development takes
another task only after this receipt. A full pending list rejects without
consuming the handoff. The default pending capacity is 3 (configurable 1..16),
with at most 256 source commits. Only one active batch and one pending list exist.
The lane must have no writer or unapplied results before accepting another task.
An accepted/frozen member cannot resume, dispatch a worker, reserve a writer,
publish an independent V1 PR or initialize another lane. Freeze, integration and
publication recheck that members remain released at their handoff without a PR
or lane. After confirmed landing, the member can resume to finish its original
acceptance; its delivery receipt still prevents an independent PR or lane.

Newly accepted members carry a stable `receipt_id` in both the lane member and the
source task's `batch_delivery`. It is created by successful `batch-offer` intake;
freezing, integration and landing preserve it. Existing records without this field
are not retroactively assigned an ID. The ID identifies acceptance of that source
task; it is not a native turn, a transport receipt, publication approval, or proof
that the next task was claimed. Explicitly admitted V1 tasks use the
[claim contract](claim-admission.md) and a source `claimed_by` record for one-use
succession. Legacy receipts do not authorize that path. Deployment and live
bounded-parallel acceptance remain separate. Do not treat a `HANDOFF_ACCEPTED`
message as a substitute for this persisted intake record.

## Freeze members, integrate, freeze snapshot

`batch-freeze` requires an empty active slot and at least one pending task. It
queries latest target H, checks dependencies and freezes members/order/H together.
It does not wait to fill a batch. Passing tests, ACK, push and closed-unmerged do
not release the slot; it remains occupied throughout repair and merge observation.
Later offers enter the next bounded list.

`batch-integrate` imports pinned objects into an owned bare repository and applies
only each source commit's changes using a private index. Developer checkouts stay
independent. Each deterministic integrated commit is journaled before moving the
private ref. Recovery compares the ref to the journal's parent/intended commit and
records the mapping without blindly replaying changes. Unaccounted refs,
workspaces and index locks require inspection rather than reset/deletion.

Conflicts retain members, source/parent/offset and the conflict index for one
isolated V1 repair assignment. The first durable conflict checkpoint includes
the matching task candidate, failure token and explicit repair blocker, so a
restart can dispatch the repair directly from the saved state.
After integration, `batch-snapshot` with the
tracking `issue` creates an owned clean checkout, freezes final S/tree and prepares
the V1 PR ledger with branch `herdr/BATCH_ID`. Member and snapshot freezes are
separate transitions. No gate result can attest a later repaired candidate.

## Execute the gate and publish

Register a fresh signal wait for the lane's current candidate/assignment, pin the
producer session, and verify ordinary receiver watcher readiness. Pass its `wait`
and a globally unused `operation` to `batch-gate-begin`. The operation is saved
before external execution, and the task selects that wait. Use the existing
one-shot command wrapper or registered producer transport to invoke deployed
GateHouse with the exact operation/input. Inspect ambiguous execution; never
replay a wrapper reservation or operation.

ACK/apply the result and inspect writer release before `batch-gate-record` with
the existing run `reference`. Zero command exit is not quality approval. The
[operator-managed verifier](gate-client.md) independently checks exact operation,
repo, head/tree, base, policy, complete coverage and current validity. Only a
complete, unexpired `allow` prepares publication. Complete `deny` permits an
inspected code-repair decision; missing/incomplete/invalid results remain blocked.

Use `pr-begin` with the original publication authorization and the authorized
external publisher. The consumer rechecks dependencies, target, blocking findings
and trusted receipt before saving `sending`. The actual publisher must enforce
the frozen head/authorization at the write. This code does not push, create/merge
PRs, publish Checks, acquire App credentials or replace trusted publication.

The existing `herdr-flow ci` supports checks-only observation without querying or
triggering Actions. Supply exact task/epoch/repo/PR/sha, `base_ref`, `target_base`
and this inventory (plus the usual version/interval/deadline fields):

```json
{"checks": [{"name": "DEPLOYED_REQUIRED_CHECK_NAME", "app_id": 123, "external_id": "TRUSTED_RUN_EXTERNAL_ID"}]}
```

Obtain `external_id` from the trusted receipt. The newest run from that App/name
supersedes earlier success and is independently GET-checked. Wrong run/head/App,
changed base, revocation, neutral and skipped cannot supply success; absent
reporting stays pending. Mixed Actions/Checks inventories require both. V1
`pr-observe` and repair takeover also recheck exact external runs. Other GitHub
rules remain independent. Recover missing reporting through the deployed consumer
publisher/App adapter for the existing result/PR; do not start a new gate or PR
to fix an upload. Reporting failure does not change the saved quality conclusion.

`batch-reconcile` reuses exact PR recovery and merged/landing ancestry checks,
then requires the expected App/run's current success and landing tree equal to S.
Merge/squash/rebase are accepted under this conservative equality rule. No
unimplemented input-equivalence inference can release the slot. Landing mappings,
slot release and freezing the pending successor against then-current main are
one queue update. The coordinator immediately continues that prepared successor's
integration under existing authorization, without a user reminder.

## Repairs, baseline changes and recovery

Writing lane dispatches use normal `task-apply` plus `batch_repair` containing
`batch`, `candidate`, `base` and the active batch's `failure_token` as `token`.
Add an inspected `repair_diagnosis` of
kind `code`. Attempts are bounded for the entire batch. V1 begin/exact takeover
recheck the binding and current target; read-only review remains possible.

The worker resolves on the pinned conflict parent or S in an isolated checkout.
After its exact accepted result is ACKed/applied and writer release inspected,
`batch-repair` takes `operation`, applied `event`, `checkout`, new `head` and
verification `evidence`. It saves an update intent. `batch-repair-finish` updates
the private ref or observes that update already happened. Continue integration,
freeze the new snapshot and run a new gate. Gate-failure correction ranges survive
later baseline rebuilding.

`batch-rebase` freezes current main for the same members in a fresh owned
directory. It keeps the previous input/run index and preserves a published PR's
number/branch/previous head for same-PR update. Reconcile uncertain publication
first and import/finish an accepted repair before rebuilding. Like the next
batch freeze, rebasing requires the latest target to retain the previous batch's
confirmed landing. Independently inspect uncertain gate execution as quiescent through the
trusted client. Rebuild/revalidate with a new operation before publishing.

Pause/cancel preserves ownership, members and remote state; it kills no process,
closes no PR and does not take the merged-success release path. Cancellation
requires an inspected external disposition of processes, PR and member handoffs
before establishing a replacement bounded task contract.

Preparation deadlines and deduplication reuse queue watch/tick. Registered long
commands and approval waits retain their own deadlines; paused tasks stay silent.
There is no additional daemon or periodic model prompt. A stopped watcher must
be restored and readiness verified before automatic notification is claimed.

## Bounds and remaining live acceptance

A lane keeps 32 completed batch records with an older count; source landing
mappings remain on their bounded managed tasks. Limits are 128 batches, 256 gate
operations, 16 rebuilds per batch, 256 final PR commits, and existing queue/task
limits. Preserve unresolved state at a limit and reconcile it before continuing.
For environment-managed version 2 tasks, these records are temporary: use a
task-owned lane/queue and [delivery destruction](delivery.md) after confirmed
landing. Workspaces, logs, gate caches and completed batch records are destroyed
with the environment; never archive them or retain full queue backups at completion.
The legacy batch consumer itself still does not delete active data.

Real temporary Git repositories, private refs/indexes, a stdio verifier fixture
and injected GitHub GETs cover capacity, dependency validation, some repair guards,
exact takeover and a single happy-path integrate/snapshot. They do **not** cover
all crash boundaries, successful same-PR rebasing, gate revocation, landing-tree
verification or successor freeze; the Rust audit found unported regressions in
these areas. They do not attest deployment, credential isolation, coverage or
production delivery.
After #98 is available, complete normal/failure/recovery live paths and the #12
same-host cold/hot/cache-invalidation and per-delivered-task cost comparison.
Include failed/cancelled runs, cache maintenance, waiting, repairs and manual
interventions; no performance benefit is inferred from offline batching tests.
