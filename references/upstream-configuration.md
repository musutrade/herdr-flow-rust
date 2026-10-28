# Existing upstream configuration and owned task resources (#130)

> **Current support decision (2026-10-26): only Codex is supported.** OpenCode adapters, CLI, native session/HTTP integration and mixed live harnesses have been removed. OpenCode examples and acceptance records below are historical and must not be used as current instructions. Existing user installations, accounts, files and services are not migrated or cleaned.

## Independent publisher slice (2026-10-28, current)

The current implementation adds an independent publisher to fresh, lifecycle-owned
upstream coordinators. Its source of truth is `task_runtime::publication`, the
existing PR ledger, CI observer and delivery journal. The earlier acceptance
records below retain their original local-delivery scope. The new implementation
and local protocol fixtures do not establish a real coordinator → publisher →
GitHub PR/CI → receipt → reclamation acceptance run. Full-suite and remote CI
results for this slice must be recorded separately when available.

Resolve `roles.publish` explicitly into `role_models.publisher`; it does not
inherit the coordinator, developer or reviewer model. Merge the following fragment
into a complete fresh coordinator launch spec, retaining its required explicit
developer/reviewer selections:

```json
{
  "role_models": {
    "publisher": {
      "providerID": "openai",
      "id": "PUBLISHER_MODEL",
      "reasoning_effort": "high"
    }
  },
  "publication": {
    "host": "github.com",
    "repo": "OWNER/REPO",
    "base_ref": "main",
    "endpoint": "https://github.com/OWNER/REPO/tree/main",
    "merge_method": "merge",
    "workflows": [
      {"file": "rust.yml", "event": "pull_request", "decision": "required_jobs", "jobs": ["test"]},
      {"file": "crap.yml", "event": "pull_request", "decision": "required_jobs", "jobs": ["crap"]},
      {"file": "audit.yml", "event": "pull_request", "decision": "required_jobs", "jobs": ["audit"]}
    ],
    "landing_workflows": []
  }
}
```

The model/provider and repository are placeholders, not defaults or availability
claims. The operator selects the actual existing workflow/event inventory and
exact job names. `workflows` must be nonempty. Each premerge entry requires
`file`, `event`, `decision: "required_jobs"` and a nonempty list of unique exact
`jobs`, matching the existing PR observer's contract. Whole-workflow decisions
are rejected for premerge publication before any remote action.
`landing_workflows` may be omitted or explicitly `[]` when no postmerge inventory
is agreed. A nonempty landing inventory uses the general CI schema, for example
`{"file":"rust.yml","event":"push"}`; it may observe the whole workflow or
select `required_jobs`. Both inventories are validated through
`ci::validate` before creating the publication child, querying GitHub or freezing
the parent. Configuration selects existing checks; it does not create workflows
or grant permission to change their rules.

This slice accepts `github.com` and `merge_method: "merge"`. The endpoint must
exactly equal the immutable requirement's `basis.endpoint` and the policy's
`https://github.com/OWNER/REPO/tree/BASE_REF`. The model cannot choose a repository,
branch, executable, shell command or endpoint. Remote publication uses this real
target rather than a synthetic local delivery path. The accepted Git artifact
must belong to the disposable task environment.

After independent acceptance plans delivery, the coordinator requests
`herdr_queue` action `publish` with only these spec fields:

```json
{
  "state_version": 12,
  "authorization": "EXISTING_DELIVERY_ID",
  "publication_authorization": "EXISTING_PUBLICATION_ID",
  "merge_authorization": "EXISTING_MERGE_ID",
  "issue": 130,
  "title": "Reviewed change",
  "body": "Implements the agreed scope."
}
```

Replace the version and issue with the current parent version and actual issue.
All three IDs must already exist in the requirement with actions `delivery`,
`publication` and `merge`, each with its recorded source. This request creates
no authority. The host pins revision, scope epoch, acceptance round, candidate,
exact approval and a single delivery ID. It registers `publish-<id>` and its
result wait before freezing the parent. Repeating the exact preparation may
finish a persisted local prefix; changed inputs and inactive parents are rejected.

The publisher uses a fresh, separately configured read-only Codex session with
network access disabled for its model turns and no queue-control tools. It makes
two bounded typed requests: `publish` permits host-validated push/PR creation;
after observed exact-head CI, `merge` requests the existing PR's merge. Either
turn may report a blocker. The host rechecks the live PR, candidate, target and
successful CI before submitting a merge. Premerge observations bind the accepted
head and PR; nonempty landing observations bind the actual merge commit with
`pr: null`. A command's successful exit and a model's text are not receipts.

After real landing verification and the agreed landing CI, a host-confirmed,
matching delivery receipt marks the operation complete directly. There is no
required final model-report turn. The publication retains its same-target slot
from sending through this confirmation, including the interval after merge while
landing CI is pending. Its result wait has no business timeout. The supervisor
signals that original wait; status can then show `signalled` with outcome
`complete`. Unknown push/create/merge outcomes are retained for observation of
the original operation, never automatic resubmission or replacement PR creation.
An unavailable remote read retains the original phase and pending result wait;
the supervisor retries only observation after 30 seconds. A verified changed PR
identity or failed CI requires a decision. The `planned` startup prefix can
launch its watcher only while both the saved send marker and watcher identity
are absent; uncertain watcher or model sends are not automatically replayed.

`publish` returns asynchronous dispatch status. The coordinator stops and awaits
the original result notification rather than polling or calling `publish` again.
Inspection includes publication state, and operators can query it without
starting or resuming any remote operation:

```sh
herdr-flow task-runtime publication-status --task TASK --state-dir COORDINATOR_ROOT
```

For a stage delivery, ACK the notification and apply the original publication
result first, then invoke `stage-complete` with the exact delivery ID. The same
parent and environment continue with a new scope epoch, candidate and independent
acceptance. A confirmed receipt alone does not bypass the applied-result gate.
Overall acceptance and a confirmed non-stage final delivery are required before
success finalization reclaims task resources. Stage completion does not reclaim
the parent or manufacture a new requirement. Login, credentials, provider,
proxy and shared service configuration remain upstream and unchanged.


## Failure and recovery acceptance (2026-10-26, latest)

### Network-instrumented single-task retest (2026-10-26)

One fresh real task passed in **143.13 seconds** with unchanged Sol/high
coordination/development, Astra/high review, existing service/proxy and the
600-second task deadline. Delivered commit:
`9bc8bf488f875e3721e546ed12d94a423b9dc3a4`. The fixture verified local delivery,
owned-root/registration removal and empty active/archived upstream inventories.
This single run does not replace the failed parallel campaign or the 2/20 result.

An external read-only observer sampled every three seconds, independently of
the task supervisor. It retained phase/tool status and thread item **types**,
local read-RPC duration, daemon TCP metrics and newly appended daemon errors.
No prompts, credential copies, model changes or service restarts were required.
This is an investigation harness, not newly installed production monitoring.

- Developer turn first observed at 15.25 seconds, first visible non-user item at
  21.37 seconds, completed at 45.86 seconds (roughly 31 seconds observed turn time).
- Reviewer turn first observed at 82.67 seconds, first visible item at 91.92,
  completed at 104.21 seconds (roughly 22 seconds observed turn time).
- Review applied by 119.61 seconds; finalizing observed at 141.24; task passed
  at 143.13. Observation times have approximately three-second sampling error;
  first visible item is **not** network time-to-first-token.
- Local read RPC maximum was 49.7 ms. Sampled shared-daemon TCP RTT to its proxy
  was 0.143–4.945 ms, with no retransmitted bytes or receive/send queue backlog
  observed. No new daemon errors or sampled turn errors were recorded.
- Actual daemon connections used proxy port **10812**. The earlier port-10811
  reachability probe came from process environment and did not identify the
  actual connection path. Neither proxy configuration was changed.

Conclusion: the long delay did not recur; there is no evidence of a local-to-proxy
network fault in this run. Shared sockets cannot be attributed exclusively to
this task, and proxy-to-upstream transport, request DNS/TLS/TTFT and upstream
queue time remain unobserved. This cannot exclude intermittent historical network
trouble or prove that earlier waits were model computation. The earlier slow
developer intervals were about 367/460 seconds; their missing transport telemetry
cannot be reconstructed from this successful run. The separate Git 10-second
fixture timeout uses a local repository/worktree and no model/remote-Git call;
its root cause is still unproven.

Evidence: `/mnt/dev-ssd/dev-tmp/network-probe-20261026/` contains the bounded
observer, raw `samples.jsonl`, `analysis.json`, `result.json` and `task.log`.
Task evidence: `/mnt/dev-ssd/dev-tmp/.tmpbMGXb3/full-task-result.json` and `audit/`.

### Earlier failure/recovery matrix (retained)

New explicit live scenarios cover failed review followed by repair/re-review,
final failure cleanup, and restarting only the owned queue watcher after a real
worker turn has been submitted. They use the existing Sol/high coordinator and
developer and Astra/high reviewer. Bounded private queue snapshots are retained
outside the task roots, including immutable applied verdicts and terminal state.
The watcher-restart case records the submitted session/turn/binding and rejects
identity changes; it is not a shared app-server restart test.

The first final-failure attempt (`/mnt/dev-ssd/dev-tmp/.tmp6TEsva`) failed because
the new evidence directory inherited a group-writable mode. This fixture error
is fixed with explicit private creation and a non-model regression. On inspected
recovery, the coordinator's upstream turn had definitively failed with
`serverOverloaded` (selected model at capacity), while its developer completed.
No model, account, provider, or billing fallback was used. A resumed queue watcher
could collect that development result but could not notify the unhealthy
coordinator. The ordinary CLI finalizer also rejected the inactive receiver.
Explicit inspected fixture finalization through the existing library API recorded
failure and reclaimed its exact registered resources. Active and archived
upstream inventory was empty afterward. This is **not** an automatic failure
acceptance pass or proof that an unhealthy coordinator can recover itself.

A fresh final-failure attempt (`/mnt/dev-ssd/dev-tmp/.tmpn3gavG`) passed in
231.40 seconds: a real developer produced the deliberately wrong first artifact,
a real independent reviewer rejected it, the coordinator applied that immutable
failed verdict and finalized failure as instructed. The predeclared local target
remained at its original commit; no failed artifact was delivered. Owned roots,
registration, and active/archived task/review sessions were absent. Evidence is
in `audit/`, `full-task-result.json`, and
`/mnt/dev-ssd/dev-tmp/recovery-live-final-failure-2.log`.

The first repair and watcher-restart attempts (`.tmpkpCUFK` and `.tmpbJOdDN`)
failed in the evidence collector at 295.10 and 253.56 seconds respectively:
authority-version and heartbeat-only writes exhausted the 256-snapshot bound.
The collector now compares business state without those three top-level clock/
authority fields, retaining the full original snapshot only for actual changes.
A regression processes 1,000 heartbeat updates without losing a later business
transition. The bound is unchanged. These attempts are not passes. Their original
requests were observed without redispatch, remained incomplete at their original
600-second deadlines, and were explicitly finalized as failed through inspected
fixture recovery. Owned roots and active/archived inventories were empty after
cleanup (`recovery-failed-attempts-inventory.jsonl`). No model fallback was used.

Validation after the lock/trace changes: the complete default-concurrency
coverage suite passed (201 library tests plus all binary/integration suites),
with CRAP absolute threshold 10/no baseline, format, Clippy, and lint policy.
The later heartbeat-only fixture correction passed its focused regression.
These automated checks do not count ignored live cases as passing.

A fresh watcher-restart attempt (`/mnt/dev-ssd/dev-tmp/.tmpUWTX56`) passed
in 522.35 seconds. The harness killed and reaped only its owned queue watcher
after the real development turn was durably submitted, then started a replacement.
The submitted worker session, turn and binding stayed unchanged; the existing
result was collected and independently reviewed, and the coordinator delivered
commit `2e0b2ede49bf39d0783b9452496a91eade41d7b4` to the predeclared local target
before automatic cleanup. Task roots/registration and active/archived inventories
were empty. `audit/watcher-restart.json` preserves the exact pre-restart tuple.
The worker was slow to produce its first output, but did eventually complete;
that delay is not classified as another capacity error. This proves queue-watcher
restart after submission, not shared-service restart or unknown-send replay.

Commit `35ddcda` also passed all remote checks:
[Rust checks](https://github.com/musutrade/herdr-flow-rust/actions/runs/40349678949),
[CRAP](https://github.com/musutrade/herdr-flow-rust/actions/runs/40349680505), and
[dependency audit](https://github.com/musutrade/herdr-flow-rust/actions/runs/40349679063).

The second repair attempt (`/mnt/dev-ssd/dev-tmp/.tmpwSuYgr`) hit its
original 600-second deadline (600.82 seconds) and remains a failed test record.
Inspected continuation of that same task, without replaying initial or worker
requests, produced a genuine repair commit with exact bytes `42\n`. The new
independent reviewer still rejected it: the fixture's original goal combined
artifact acceptance with completed delivery, although delivery requires passed
acceptance. Its first review had recorded both a content gap and a delivery gap;
the repaired content resolved only the former. The failed reports are legitimate
under that circular fixture criterion and were not rewritten or overridden.

The fixture now asks the independent reviewer to accept the committed artifact;
the existing separate delivery authorization, receipt gate, target-content
assertion and success finalization still verify actual delivery afterward. This
changes only new fixture requirements, not existing reports or product approval
rules. The old attempt was explicitly finalized as failure using inspected
recovery, with the transient lifecycle lock retried only after `StateBusy`.
Its registered roots and all active/archived task/review inventories were then
empty (`inspected-upstream-inventory-final.jsonl`). The original timeout and
both failed reports remain in its evidence directory.

The fresh corrected repair scenario (`/mnt/dev-ssd/dev-tmp/.tmpWxJg62`)
passed in 395.35 seconds, within the unchanged 600-second task deadline.
The real first reviewer rejected the deliberately incorrect artifact with a
content gap. A real developer repaired and committed it; a new independent
reviewer passed the fixed snapshot and resolved the exact original gap. The
harness verified that the first failed verdict stayed immutable, reviewer
identities differed, and terminal open gaps were empty. The coordinator delivered
`bd1bd640f4be91b6bbe12884de49e48fe2b2c965` (exact `42\n`) and finalized success;
owned roots/registration and active/archived inventories were absent. No driver
ACK, verdict, repair, candidate selection or delivery substituted for the models.
The earlier failed attempts remain separate and are not relabeled as passes.

The first two-queue campaign (`/mnt/dev-ssd/dev-tmp/.tmpvlffJX`) did **not**
pass: `.tmp7JKTeu` completed automatic local delivery and reclamation in 484.61
seconds, while `.tmp4Itx1x` exceeded the original 600-second deadline after its
review passed (600.76 seconds). The latter's existing coordinator turn was still
in progress, not replayed. A separately recorded bounded watcher resumption
completed its already-approved delivery (`d28026cf1c9574e9132056c58f8dabb2e2523269`)
and reclamation. The passed verdict stayed immutable; both queues ended empty
and both task/review active/archived inventories were empty. This recovery does
not turn the original failed parallel campaign into a pass, nor prove shared-queue
claim backpressure. `campaign.json` retains first_success=false/second_success=true;
`inspected-recovery-verified.json` and `final-upstream-inventory.jsonl` record the
separate verified recovery and final inventory.

Current code commit `d887265` passed all remote checks:
[Rust checks](https://github.com/musutrade/herdr-flow-rust/actions/runs/40356035346),
[CRAP](https://github.com/musutrade/herdr-flow-rust/actions/runs/40356035369), and
[dependency audit](https://github.com/musutrade/herdr-flow-rust/actions/runs/40356035469).

The sequential campaign (`/mnt/dev-ssd/dev-tmp/.tmpeZ71E2`) stopped at its
first failure: **2 of 20 requested tasks passed; the campaign did not pass**.
Rounds 1 and 2 completed in 193.12 and 375.53 seconds. Round 3 (`.tmpxEDvGG`)
exceeded the unchanged 600-second fixture deadline (600.72 seconds); rounds
4–20 were not started, and round 3 was not retried to increase the pass count.

Before inspected recovery of round 3, the local delivery was already confirmed
and its finalize tool response was durably recorded; the queue terminal transaction
and reclamation had not completed. Restarting only that task's watcher committed
the existing terminal request and reclaimed resources in approximately 5 seconds.
The target remained the already-approved `c78176c2c88db430b9098c4044a844fb9ba1d9f0`
with exact `42\n`; the passed verdict was unchanged. This recovery does not
retroactively satisfy the original 600-second whole-task test. It is a fixture
wall-clock budget, not a newly established product performance requirement.
`campaign.json`, each child log, `inspected-before-recovery.json`,
`inspected-recovery-verified.json`, and `final-upstream-inventory.jsonl` retain
that distinction. No model/effort/provider/billing fallback was used.

Final read-only inventory across all twelve real attempt roots from this phase
returned empty active and archived task/review lists, with no pagination
(`/mnt/dev-ssd/dev-tmp/recovery-all-owned-inventory.jsonl`). The four inspected
failure terminations retained their original local output refs. Shared Codex
service processes remained alive. Seed/output repositories, empty fixture queues
and bounded evidence remain intentional harness assets, not renamed upstream
session residues.

Remaining acceptance work includes the unexplained historical Git timeout,
a passing parallel campaign and twenty consecutive passing tasks. Whole-task
model latency and terminal reclamation time must be assessed separately before
changing any test budget; these failures are not waived. Shared-queue claim
backpressure, remote publication and global migration remain unverified here.

The two-queue and twenty-consecutive-task campaign entrypoints are explicit
ignored tests in `upstream_live_campaign.rs`. Every child uses a separate process
and queue. A sequential campaign stops at its first failed task and retains that
attempt; it never redispatches uncertain work or retries to manufacture twenty
passes. Two independent queues do not prove shared-queue claim backpressure.
The above campaign failures remain unresolved; test entrypoints alone are not
completion evidence. Remote publication and global migration remain outside
these local acceptance scenarios.

The separately reproduced environment-lock race and the still-unexplained
historical Git timeout are distinguished in
[the cleanup observation record](cleanup-observation-races.md).

## Real continuation acceptance and Guardian cleanup (2026-10-26)

The configured team uses gpt-6.1-sol/high for coordination and development and
gpt-6-astra/high for independent review. Read-only inspection of the existing
Codex 0.164.0 service advertised these models and high effort; its shared default
remained gpt-6-astra/medium. Explicit per-role effort is therefore now transmitted
and verified instead of silently accepting the service default.

The first real run developed a new commit, independently reviewed its fixed
snapshot, applied results, delivered locally, finalized and removed task roots.
However, additional inventory found a `codex-auto-review` Guardian descendant
that the original three-thread cleanup omitted. **This run is not an automatic
no-residue acceptance pass**, even though its original harness passed. Evidence
and the correction are retained at `/mnt/dev-ssd/dev-tmp/.tmp5mRmfM`, including
`post-run-audit.json`; no historical result was erased or reclassified as upstream
owned data. The exact Guardian was recovered through `thread/delete` only after
its rollout metadata parent matched the retained development-session evidence.
Activity/archived inventory then returned no sessions for either task cwd.

The fix discovers task descendants after deleting recorded parents, inventories
active and archived sessions for each recorded cwd with bounded pagination, and
requires exact rollout ID/cwd/parent metadata. Cwd alone never authorizes deletion;
foreign-parent sessions are preserved. The installed service omits Guardian parent
IDs in `thread/list`, so a bounded, canonical, same-user regular rollout's first
metadata line supplies the missing relationship. No authentication files are read.
Ambiguous/malformed metadata or inventory errors retain cleanup obligations.

Discovered identities are added to the existing durable cleanup plan before any
delete, then processed through the existing exact-ID deletion/retry mechanism.
The scan repeats after deletion before roots are released. Guardian model choice
belongs to the upstream approval service; it is distinct from the independent
reviewer role. Approval behavior, accounts, proxies and shared defaults are not
changed to avoid generating this resource. Protocol integration tests now include
an actual synthetic Guardian rollout with a missing API parent and assert its
removal. The live harness additionally requires empty active/archived task-cwd
inventories before declaring success.

The second fresh real task passed the strengthened harness in 190.42 seconds.
Evidence: `/mnt/dev-ssd/dev-tmp/.tmpKox0sK`; output commit
`9ee9e5ce69161e02f52b7893e6aa3a30ae9db6ac` at local `refs/heads/accepted`.
Sol/high developed `answer.txt` with exactly `42\n`; Astra/high reviewed the fixed
commit; the real coordinator ACKed/applied results, delivered and finalized.
The harness supplied the requirement, initial empty repository/registered activity,
and predeclared local authorization, not the finished candidate or later decisions.
Some malformed tool arguments were rejected and corrected by the coordinator;
those are retained as observed usability limitations, not hidden successful calls.

An observed Guardian descendant was automatically removed by the revised cleanup.
`upstream-inventory-before-cleanup.log` records its identity;
`upstream-inventory-after-cleanup.log` records empty active and archived inventories,
and its rollout file is absent. `full-task-result.json` records the exact output
and removed task workspace/scratch/registration. Retained seed/output repositories,
empty harness queue and bounded acceptance evidence are test fixtures, not renamed
upstream task residue. Shared service processes and default settings remained intact.

This is one complete real local-delivery scenario. It does not complete remote
publication, parallel/failure/restart matrices or 20 consecutive no-residue tasks.
The inventory implementation covers descendants in recorded task/review cwds;
this run does not establish arbitrary child-cwd or other upstream-version support.

## Role model configuration (2026-10-26, current)

Coordinator, developer and reviewer models are independently configurable;
the [current publisher slice](#independent-publisher-slice-2026-10-28-current)
also accepts a separately resolved `role_models.publisher`.
This corrects the previous continuation implementation, which incorrectly reused
one coordinator model for every worker. The following selection takes precedence
over earlier same-model examples; historical evidence remains unchanged.

Use the existing [team configuration](team-config.md) precedence to resolve the
role models, then pass the resolved snapshot in the coordinator's launch spec:

```json
{
  "adapter": "codex",
  "configuration": "upstream",
  "socket": "/absolute/existing/app-server.sock",
  "cwd": "/absolute/owned/checkout",
  "model": {"providerID": "openai", "id": "COORDINATOR_MODEL", "reasoning_effort": "high"},
  "role_models": {
    "developer": {"providerID": "openai", "id": "DEVELOPER_MODEL", "reasoning_effort": "high"},
    "reviewer": {"providerID": "openai", "id": "REVIEWER_MODEL", "reasoning_effort": "high"}
  }
}
```

These model IDs are placeholders, not availability claims or defaults.
`roles.coordinator.model` maps to `model.id`, `roles.dev.model` to
`role_models.developer.id`, and `roles.review.model` to
`role_models.reviewer.id`. Provider IDs identify the explicitly selected existing
upstream configuration; they do not configure accounts or billing. As before,
the coordinating agent resolves TOML; the runtime does not automatically read or
merge configuration files. This spec is a task snapshot, not another config store.

The persisted activity role selects the worker model: `development` uses developer,
`acceptance` uses reviewer. Each worker runtime/native creation record captures its
selection; the returned model/provider must match before dispatch. Coordinator
turns also explicitly request their recorded model. Missing role configuration,
unknown roles, malformed selections or a substituted upstream model are errors.
There is no coordinator-model default or availability fallback. Even when all
roles intentionally use one model, supply both role entries explicitly.

Standalone runtime launches still need only `model`. Existing coordinators without
`role_models` can inspect/ACK/apply but cannot auto-dispatch workers; their stored
configuration is not silently migrated or edited. New defaults take effect in new
explicitly prepared tasks. Receiver recovery rejects changed role selections.
Model-issued `run` requests cannot override the model, provider or endpoint.

The local delivery executor makes no model call. The independent publisher maps
`roles.publish` to `role_models.publisher` and uses the frozen `publication`
policy described above; it is not launched through a generic development or
acceptance `run` request.
The optional `reasoning_effort` field in each model selection snapshots that
role's resolved effort. It is passed as a per-thread `model_reasoning_effort`
setting and as `effort` on every turn, without editing shared configuration.
An explicit effort must match the thread creation response; omission preserves
the upstream default. This does not implement a TOML resolver.
Login, subscription, proxy and billing remain entirely upstream.

The synthetic continuation tests use three distinct model IDs and assert actual
thread/turn RPC selections, including after coordinator broker recovery. The
explicit real-task harness additionally requires `HERDR_UPSTREAM_ROLE_MODELS`
with developer/reviewer selections; see the real-run evidence above.

Validation: the full default-concurrency coverage suite passed (197 library tests
plus binary/integration suites), as did format, all-target/all-feature Clippy and
suppression policy. Absolute CRAP threshold 10 passed without a baseline; selected
source line coverage is 83.6%, and the role-selection module has 100% line coverage.
Logs and reports are retained under `/mnt/dev-ssd/dev-tmp/role-model-*`.

## Implemented support boundary (2026-10-26)

Only Codex is supported. The OpenCode module, CLI dispatch, HTTP/session creation,
collection and journal observer, receiver readiness adapter and OpenCode-only
mixed live harnesses have been removed, together with their HTTP dependencies.
Old OpenCode registrations and runtime records fail closed, including when the
caller has a Codex session identity. There is no automatic migration or fallback.

Generic queue ownership, receiver bootstrap, fixed reviewer snapshots and terminal
cleanup tests now use a synthetic Codex Unix-socket protocol fixture. These tests
exercise task lifecycle behavior without logging in or calling a model. Historical
OpenCode acceptance matrices are retired requirements, not passed acceptance.
The earlier real Codex evidence below retains its original scope; this removal
adds no new real-model or complete issue-development acceptance claim.

Login, provider configuration, subscription and billing remain with the existing
Codex installation. Historical OpenCode account settings remain untouched.
No task imports account files, creates a credential
store, changes a proxy, or switches billing routes on failure.

## Coordinator continuation implementation (2026-10-26)

Fresh upstream receivers now use `queue-tool-v2`. Existing v1 receivers retain
inspect/ACK/apply only; their sessions are not upgraded or given new tool
capabilities. No old queue, watcher or global entrypoint is changed.

The existing queue watcher executes the coordinator's bounded continuations:

- `lineage`: existing artifact/candidate/activity/continue operations for
  the same Owner. Creating or revising requirements and authorization is refused.
- `run`: starts an already registered development or acceptance activity, using
  its persisted assignment. It uses the coordinator's pinned upstream socket and approval policy,
  with the operator-configured model/provider for the registered activity role. The model cannot supply a new
  executable, endpoint, credentials or sandbox. Acceptance creates the registered
  fixed Git snapshot through the existing review API.
- `inspect`: includes collected native results. The watcher collects dispatched
  workers and publishes their ordinary queue events; a successful run response
  means dispatch, not task completion. The coordinator stops until notification,
  then ACKs and applies the immutable result. Development application can register
  the collected output and release its writer through the existing checks.
- `deliver`: invokes the existing local Git executor with an already recorded
  authorization and predeclared delivery target. Current independent acceptance,
  candidate/version, target identity and expected ref are still required. It does
  not add remote publication or manufacture an authorization/receipt.
- `publish`: dispatches the independently configured publisher using current
  acceptance and existing delivery/publication/merge authority. The operator's
  frozen policy supplies the remote target and CI inventory. Its asynchronous
  reply requires waiting for the original publication result; see the
  [publisher contract](#independent-publisher-slice-2026-10-28-current).
- `finalize`: validates success against confirmed delivery, or records an explicit
  final failure. After its exact tool response is recorded sent, the watcher commits
  the terminal obligation under the existing exclusive lifecycle gate. Subsequent
  cleanup ticks reclaim the task; a pending cleanup does not re-enter role control.

Runtime, collection and delivery operations run outside the outer queue lock;
existing APIs retain their own transactions. Task lifecycle permits protect the
control journal and child spawning. The existing bounded request/result journal
provides at-most-once execution; native journals protect the individual launch,
creation and turn attempts. Child brokers are reaped. No new scheduler, service,
credential store or permanent task archive is introduced.

After bootstrap, activation and binding to the new queue, explicitly start the
first coordinator turn with:

```sh
herdr-flow task-runtime receiver-start --task TASK --state-dir COORDINATOR_ROOT --spec START_SPEC
```

`START_SPEC` contains only `{"instruction":"Proceed with the registered task"}`.
This creates a model turn, and is used only within the task's existing execution
authorization. Intent is persisted before sending; an uncertain response is not
replayed. Later result notifications drive the same coordinator.

### Transport restart and unresolved operations

`task-runtime broker-recover` restarts only the owned transport using the recorded
runtime; it neither starts nor changes the shared upstream service. After that
restart the queue watcher automatically rebinds an already loaded, idle receiver.
`task-runtime receiver-recover` exposes the same operation explicitly. Both use
`--task TASK --state-dir COORDINATOR_ROOT`; an optional spec supplies the existing
Flow executable as `driver_binary` for operator-side invocation only.

Rebinding requires unchanged runtime configuration and upstream socket identity,
the same Owner/thread, kernel-confirmed absence of the old broker, a stopped old
watcher, and no unresolved tool operation. The old/new local images are recorded
before publication so an interrupted rebind can resume exactly. It starts only a
replacement task-owned watcher. Active, unloaded or unknown thread states remain
blocked; no turn is resent and no replacement thread is created.

Existing `task-runtime recover` reconciles known creation identities, and
`delivery reconcile` observes an existing delivery without resubmission. Unknown
thread creation with no returned ID and ambiguous tool response transmission still
require inspected recovery. The implementation cannot infer non-execution from a
missing response; those cases are not automatically retried or declared reclaimed.

### Evidence scope

New protocol integration tests run the actual queue watcher, brokers, Git commits,
fixed review workspaces, local Git delivery and finalizer. A synthetic upstream
peer supplies developer/reviewer behavior and coordinator tool requests. They
cover successful delivery, explicit final failure, broker restart after activation,
forbidden endpoint/requirement changes, foreign tasks and premature success.
They are not real-model acceptance or the required continuous 20-task run.
The earlier real-model evidence below retains its original limited scope.

Concurrent continuation tests also exposed lifecycle-gate contention being
classified as a permanent guard error. Only an actual nonblocking lock
`WouldBlock` is now `StateBusy`: periodic queue watching defers that attempt,
while `--once` reports it and other OS errors remain errors. Regression tests
verify both contention and malformed-path behavior. A separate existing Git
removal fixture once exceeded its unchanged ten-second deadline; it now reports
the worker state and log on failure. Later passing runs do not establish the
cause of that isolated timeout.

`live_upstream_coordinator_full_task` is an explicitly ignored real-model harness
for development through local delivery and cleanup. It requires the operator's
existing socket and model selection. Its presence is not evidence that the real
scenario passed; an uncertain run retains state rather than redispatching.

The final full coverage run passed with default concurrency (195 library tests
plus binary/integration suites). Absolute CRAP threshold 10, without a baseline,
passed at 83.6% selected source line coverage; continuation and recovery modules
reached 93.6% and 93.8%. Format, all-target/all-feature Clippy with warnings denied,
suppression policy, release build and rustdoc with warnings denied passed. Local logs and JSON
reports are retained as `/mnt/dev-ssd/dev-tmp/coordinator-validated-coverage.*`,
`coordinator-validated-crap.json` and `coordinator-*-final.log`. Failed development
runs remain investigation evidence, not passing acceptance.

The following sections retain earlier implementation/evidence history. Where
control capabilities or restart limitations differ, this section takes precedence.

## Historical OpenCode verification (before the removal decision)

The installed/interactive binary is 1.19.4, but the observed listening service
is a still-running 2.0.32 executable. They are not interchangeable endpoints.
Both processes open the same upstream database and log. The database contains
an active account and organization reference. Replacing it with an empty task
database cannot be assumed to preserve upstream organization configuration.
This observation does **not** establish the billing route of a particular model
request. No such request was sent during this verification.

The pinned 1.19.4 session deletion implementation removes database session data
and events, but is not complete task-resource reclamation. Task-specific process
mounts are only a proposal: they do not yet solve the shared account/session
database boundary. OpenCode upstream launch remains disabled; no credential
copy, alternate provider, shared-data deletion or 2.x fallback is authorized.
See [verified sources, host observations and remaining work](opencode-upstream-audit.md).
Historical observations below remain intact.

See also [cleanup observation races and their regression boundaries](cleanup-observation-races.md) for the transient process-read failures found during this removal.

## Current Codex path

An isolated queue Owner can attach a task broker to an **existing**, same-user,
canonical Unix app-server socket. Flow starts only its connection broker and, on
explicit activation, a task receiver watcher. It does not launch, register, restart
or stop the shared app-server. The broker inherits the caller environment without
changing HOME, CODEX_HOME or XDG configuration. Authentication stays inside the
already-running server; Flow never reads or copies its credentials.

Prepare an isolated task instance and runtime, then launch with:

```json
{
  "adapter": "codex",
  "configuration": "upstream",
  "socket": "/absolute/existing/app-server.sock",
  "cwd": "/absolute/owned/checkout",
  "model": {"providerID": "openai", "id": "configured-model"},
  "approval_policy": "on-request"
}
```

Select the actual configured model/provider; the example does not authorize a
provider change. Account settings, arbitrary configuration overrides and unknown
launch fields are rejected. Library callers provide `driver_binary` for the Flow
executable. Reviewers first call `review-snapshot`, then use its registered `cwd`
and `review_snapshot` key. Native worker turns use an upstream workspace-write
sandbox, with temporary/global writes and network access excluded; they never
reuse the fixture path's external-sandbox assumption.

`receiver-bootstrap` creates one fresh receiver identity. `receiver-activate`
starts its separate owned polling process and enables the native receiver adapter.
It does not activate old guards or change a global entrypoint. Context telemetry
and safe rotation are explicitly unsupported on this new receiver path. The old
Herdr/TUI watcher remains unchanged. Queue notification/ACK policy is independent.

Broker restart reuses the recorded upstream attachment and refreshes its process
identity. Restart after receiver activation invalidates the old receiver binding;
that receiver stops on the identity mismatch. Automatic receiver rebinding is not
implemented. The restart test covers a broker restarted before bootstrap.

## Bounded coordinator control tools

Upstream receivers expose `herdr_queue` through Codex dynamic tools. For retained
`queue-tool-v1` receivers, the only operations are inspection of the assigned Owner's tasks/events, ACK of a
notification containing only that Owner's events, and application of an existing
result. The existing result/version/verdict checks still apply. The tool cannot
create work, grant authorization, execute shell commands, publish, change accounts
or accept arbitrary queue paths. Model turns remain workspace-write confined with
network and temporary/global writes excluded.

Fresh `queue-tool-v2` receivers additionally expose the bounded continuations
above, including the current independently configured `publish` path. They do
not gain arbitrary shell, target selection or authority creation.

The task broker records native tool requests. The **new queue's driver**, while
holding its queue lock, validates the registered receiver, lifecycle Owner and
current native thread/turn, then calls the existing queue APIs. Model processes
receive no write permission to the external control directory. Existing queues
without a task-runtime receiver never enter this service. No session environment
variable is forged. Approval and user-input requests are never answered by it.

Request and result journals retain at most 64 current/referenced calls and 1 MiB
of serialized JSON each. Completed previous turns are reclaimed through a
[restartable task-local checkpoint](record-continuation.md); the limits no longer
count every call over the task's lifetime. An unfinished turn and unresolved
operations still consume the bounded window. Mutation
intent precedes execution; an interrupted operation with unknown outcome blocks
without replay. Completed results can be returned again, but a response whose
send outcome is unknown is not sent again automatically. Changed request IDs,
foreign threads/turns, mixed-Owner deliveries and unregistered responses fail.
These journals are task resources and are removed by normal terminal cleanup.

## Ownership and reclamation

The socket device/inode/user identity is sealed; replacement does not silently
rebind the task. Before thread/start the broker durably records creation intent.
It records the returned thread ID before replying, including responses later
rejected for model/cwd mismatch. Requests for other thread IDs are rejected. An
unknown creation outcome blocks further creation and terminal release; it is not
assumed to mean no thread exists.

Terminal cleanup freezes these obligations under the exclusive lifecycle gate,
stops task-owned processes, and calls upstream thread/delete for exactly the
recorded root IDs (upstream also handles their child-thread subtree). Deletion
intent and completion are persisted in the cleanup plan outside disposable roots.
A lost response can be reconciled with the exact upstream thread-not-found result
on retry. Other RPC errors, changed endpoints and unknown creation retain the plan,
slot and local evidence. No account directory or shared service is a deletion target.
Only after upstream obligations succeed can worktrees and runtime roots be removed.

This uses the upstream deletion contract, not manual editing of shared databases.
The inspected Codex 0.164.0 implementation deletes rollouts and thread-associated
state/logs. Shared server availability is required for cleanup; service replacement
or ambiguous creation still needs inspected recovery, never a new login.

## Validation and remaining scope

Latest acceptance on 2026-10-26, evidence root
`/mnt/dev-ssd/dev-tmp/.tmpBaYRBc`, completed this path:

1. A real `gpt-6-astra` reviewer checked a registered fixed Git snapshot.
2. The queue automatically notified the distinct coordinator thread.
3. That coordinator invoked inspect, ACK and apply through the dynamic tool;
   `coordinator-tools.json` records three successful native requests.
4. The acceptance supervisor used the predeclared local-only authorization to
   deliver the accepted commit to its new `output.git`, verified the actual ref,
   and recorded a confirmed receipt before success finalization.
5. Task threads, brokers, receiver watcher, workspaces, scratch roots and registry
   were reclaimed. The output repository and acceptance evidence remain.

The retained artifact is commit `8a58ac3d90345b67076475db3332ecac629e2afa` at
`refs/heads/accepted`. Evidence includes `notification-application.json`,
`coordinator-tools.json`, `local-delivery.json` and `result.json`. The earlier
`review-result.json` in that run was written before notification completion and
retains the old caller-driven scope label; completion is established by the later
artifacts above. The harness now labels that checkpoint as pending.
The candidate was seeded by the harness, not developed by a model. Collection,
local delivery and terminal supervision were performed by the driver. This proves
that concrete review/notification/ACK/application/local-delivery/cleanup path; it
is not an autonomous issue-development or remote-publication acceptance.

An earlier run at `/mnt/dev-ssd/dev-tmp/.tmptipGAq` encountered collector lock
contention, then resumed the same review without redispatch. Its coordinator ACKed
but submitted an unsupported apply field and stopped. The tool contract was made
explicit before the successful fresh run. Failed tool/turn and queue evidence were
saved, and `recovered.json` confirms that run's resources were reclaimed. The
harness now retries only read-result lock contention within its deadline.

Earlier checkpoints below retain their original, narrower validation scope.

Synthetic real-socket tests cover shared service/foreign-session preservation,
receiver activation, exact-ID requests, socket replacement, deletion response loss
and terminal retries. Fixed reviewer snapshots and external queue-control binding
are included in this branch.

On 2026-10-26 a no-model thread lifecycle was exercised against the installed
Codex 0.164.0 service. A second run exercised receiver activation; initial terminal
lock contention retained its state and was recovered through the normal cleanup
path. A real `gpt-6-astra` review then read a fixed Git snapshot, checked that the
tracked file contained exactly `review candidate\n`, and produced a validated
`passed` report. The harness used caller-driven collection and disabled only its
new queue's notifications after dispatch: **automatic notification, ACK, result
application and customer delivery were not claimed as tested**.

The real review's first teardown failed because the harness still named the old
receiver registration. Administrative teardown subsequently used the actual new
queue registration; both task threads, brokers, the receiver watcher, workspaces
and environment registration were reclaimed. Evidence roots:

- `/mnt/dev-ssd/dev-tmp/.tmpOBFceZ`: no-model lifecycle success.
- `/mnt/dev-ssd/dev-tmp/.tmpvUIgkF`: activation/lock contention and `recovered.json`.
- `/mnt/dev-ssd/dev-tmp/.tmpeiPWpr`: real verdict, snapshot/turn evidence and `recovered.json`.

Those directories retain the acceptance driver's control/evidence, not task
runtime roots; `workspace` and `scratch` were removed. Historical failures remain
part of the evidence. No global installation, old queue/watcher, proxy or account
operation was performed. This is not completion of the full #112/#38/#32 loop.

OpenCode's new shared-session cleanup adapter remains unimplemented. Its new
upstream task-runtime launch stays blocked; existing external-session integrations
are not migrated. Neither fixtures nor the Codex result establish OpenCode acceptance.

## Historical first checkpoint (superseded by the current path above)

### Initial fail-closed repair

Login, authentication refresh, provider configuration, subscriptions and billing
belong to the existing Codex/OpenCode installation. Flow calls supported upstream
interfaces and selects configured models; it must not create a login, copy
credentials/configuration into disposable storage, keep a second credential store,
or automatically change provider or billing route.

The first repair is a **fail-closed capability boundary**, not completed upstream
integration. `task-runtime launch` defaults to upstream configuration and currently
rejects both native adapters before creating a launch journal, acquiring a runtime
lock or spawning a process. Unknown configuration modes also fail. No user's
configuration or authentication files are opened by this check.

```sh
herdr-flow task-runtime configuration-status --adapter codex
herdr-flow task-runtime configuration-status --adapter opencode
```

These commands need no task, queue or account access. Their JSON reports
`supported: false`, the inspected version and concrete storage blockers.
They do not probe account availability, inspect the installed version or send a
model request. A successful command exit means the report was produced, not that
the integration is ready.

## Why simply inheriting HOME is insufficient

The inspected Codex 0.164.0 configuration uses CODEX_HOME for user configuration
and persistent thread storage. `sqlite_home` and `log_dir` can move individual
stores but are not evidence that all task artifacts are relocated. See the
[official configuration reference](https://learn.chatgpt.com/docs/config-file/config-reference)
and [fixed-version configuration implementation](https://github.com/openai/codex/blob/rust-v0.164.0/codex-rs/core/src/config/mod.rs).
The [thread/delete interface](https://learn.chatgpt.com/docs/app-server#delete-a-thread)
is a potential integration building block, not proof of complete task-resource
reclamation, exact ownership or crash recovery in this project.
The fixed 0.164.0 implementation already
[deletes rollouts under writer/reference checks](https://github.com/openai/codex/blob/rust-v0.164.0/codex-rs/thread-store/src/local/delete_thread.rs)
and [removes per-thread logs and other state](https://github.com/openai/codex/blob/rust-v0.164.0/codex-rs/state/src/runtime/threads.rs).
The remaining work is this project's integration: shared-server identity without
process ownership, creation-response loss reconciliation, exact thread registration,
terminal deletion before releasing that registration, and crash recovery. It is
not a request to add another login manager or a claim that Codex lacks deletion.

OpenCode 1.19.4, source commit `148842ef7cb1bc34192b3fff57d0f9721ab9c801`, stores
[auth.json in Global.Path.data](https://github.com/anomalyco/opencode/blob/148842ef7cb1bc34192b3fff57d0f9721ab9c801/packages/opencode/src/auth/index.ts).
[OPENCODE_DB](https://github.com/anomalyco/opencode/blob/148842ef7cb1bc34192b3fff57d0f9721ab9c801/packages/core/src/database/database.ts)
moves the database, while
[global paths](https://github.com/anomalyco/opencode/blob/148842ef7cb1bc34192b3fff57d0f9721ab9c801/packages/core/src/global.ts)
and task snapshot/tool-output stores still use the data directory.
[Logging](https://github.com/anomalyco/opencode/blob/148842ef7cb1bc34192b3fff57d0f9721ab9c801/packages/core/src/observability/logging.ts)
retains the file logger even when `--print-logs` enables stderr output.
Keeping that directory read-only can fail execution; making it writable can
leave task data in user storage. Neither is a completed integration.

These findings describe the inspected versions and the current adapter, not a
claim that upstream can never support the requirement. A future path must prove
existing-login reuse, exact task ownership, shared-service preservation and
complete reclamation, including failure and interrupted cleanup. Unsupported
capabilities remain failures; task residuals cannot be renamed “upstream data”.

## Fixture and historical boundaries

Explicit `configuration: "isolated-fixture"` retains the existing confined,
disposable execution path for synthetic protocol peers. It is a caller-declared
test mode, not attestation of the executable's behavior, and must not be used
with real credentials or counted as real-model acceptance. Its marker is retained
in runtime/creation records. Unknown or omitted modes never select it implicitly.

Historical live harnesses no longer copy provider configuration or Codex auth.
They fail the capability check instead of trying a new login or changing billing.
Historical evidence remains historical; it does not validate the new contract.
This change does not migrate, resume or delete existing runtimes, queues, watchers,
or shared services, nor modify global entrypoints.

Next: finish the supported upstream integration before declaring #130's
configuration/task isolation requirement met. Then validate the existing
queue-control and reviewer snapshot changes, and run one real task in a new
private acceptance queue. No fixture, CI result or this guard completes #38/#32.
