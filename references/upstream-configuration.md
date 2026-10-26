# Existing upstream configuration and owned task resources (#130)

> **Current support decision (2026-10-26): only Codex is supported.** OpenCode adapters, CLI, native session/HTTP integration and mixed live harnesses have been removed. OpenCode examples and acceptance records below are historical and must not be used as current instructions. Existing user installations, accounts, files and services are not migrated or cleaned.


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

Fresh upstream receivers expose `herdr_queue` through Codex dynamic tools. The
only operations are inspection of the assigned Owner's tasks/events, ACK of a
notification containing only that Owner's events, and application of an existing
result. The existing result/version/verdict checks still apply. The tool cannot
create work, grant authorization, execute shell commands, publish, change accounts
or accept arbitrary queue paths. Model turns remain workspace-write confined with
network and temporary/global writes excluded.

The task broker records native tool requests. The **new queue's driver**, while
holding its queue lock, validates the registered receiver, lifecycle Owner and
current native thread/turn, then calls the existing queue APIs. Model processes
receive no write permission to the external control directory. Existing queues
without a task-runtime receiver never enter this service. No session environment
variable is forged. Approval and user-input requests are never answered by it.

Request and result journals are bounded to 64 calls and 1 MiB reads. Mutation
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
