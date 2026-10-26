# Upstream configuration and task storage (#130)

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
