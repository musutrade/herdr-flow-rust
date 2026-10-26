# Existing upstream configuration and owned task resources (#130)

Login, provider configuration, subscription and billing remain with the existing
Codex/OpenCode installation. No task imports account files, creates a credential
store, changes a proxy, or switches billing routes on failure.

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
