# OpenCode upstream integration verification — 2026-10-26

> **Current support decision (2026-10-26): only Codex is supported.** OpenCode adapters, CLI, native session/HTTP integration and mixed live harnesses have been removed. OpenCode examples and acceptance records below are historical and must not be used as current instructions. Existing user installations, accounts, files and services are not migrated or cleaned.


This verification supplements #130/#112/#38/#32. Login, provider, organization,
subscription and billing stay upstream. Complete task reclamation and preservation
of user/shared assets are simultaneous requirements. This is an investigation,
not a successful integration or real-task acceptance report.

## Verified host observations

Read-only inspection of executable identities, selected non-secret path variables
and open file descriptors established:

| Process at inspection | Executable version | Observed storage |
| --- | --- | --- |
| Interactive OpenCode, PID 158839 | 1.19.4 | Existing `~/.local/share/opencode/opencode.db`, WAL/SHM and `log/opencode.log` |
| Listening service, PID 1309301, port 49374 | 2.0.32, running from an unlinked executable | The same database and log |

The installed `opencode --version` reports 1.19.4. Inspecting only the installed
path would therefore misidentify the existing service. Its `/global/health`
response is HTTP 200 **HTML**, not a compatible 1.19.x JSON health response.
No compatible shared HTTP service was established by these observations.

Neither inspected process overrides HOME/XDG/OPENCODE_DB configuration paths.
A SQLite connection opened with `mode=ro` queried account-table existence and
counts only: one account row, one account-state row, and non-null active account
and organization references. No token, account ID, organization ID, email,
credential file or configuration contents were printed or copied. This confirms
stored selection state, **not** which billing route any particular model uses.

No process was stopped/restarted, no session was created/deleted, no model was
called, and no login/configuration/proxy file was modified. Existing queues and
watchers were untouched. No synthetic test is represented as a real-model test.

## Version-pinned implementation

`git ls-remote` resolves `v1.19.4` to
`148842ef7cb1bc34192b3fff57d0f9721ab9c801`. The previously recorded commit was
correct, but it contains both v1 and v2 implementations: the v1 session API uses
`packages/opencode/src/session/session.ts`, not the v2 service in
`packages/core/src/session.ts`.

| Verified behavior | Source at the pinned commit |
| --- | --- |
| `OPENCODE_DB` selects the common database service | [database implementation](https://github.com/anomalyco/opencode/blob/148842ef7cb1bc34192b3fff57d0f9721ab9c801/packages/core/src/database/database.ts) |
| Account repository uses that same database service; account tables contain tokens and active organization selection | [account repository](https://github.com/anomalyco/opencode/blob/148842ef7cb1bc34192b3fff57d0f9721ab9c801/packages/opencode/src/account/repo.ts), [schema](https://github.com/anomalyco/opencode/blob/148842ef7cb1bc34192b3fff57d0f9721ab9c801/packages/core/src/account/sql.ts) |
| Active organization configuration/token can participate in configuration loading | [configuration loader](https://github.com/anomalyco/opencode/blob/148842ef7cb1bc34192b3fff57d0f9721ab9c801/packages/opencode/src/config/config.ts) |
| DELETE session calls session removal and returns `true` | [HTTP handler](https://github.com/anomalyco/opencode/blob/148842ef7cb1bc34192b3fff57d0f9721ab9c801/packages/opencode/src/server/routes/instance/httpapi/handlers/session.ts) |
| Removal cancels background jobs when instance state exists, recursively removes children, publishes deletion and removes aggregate events | [v1 session removal](https://github.com/anomalyco/opencode/blob/148842ef7cb1bc34192b3fff57d0f9721ab9c801/packages/opencode/src/session/session.ts) |
| Deletion projection removes the session row; foreign-key cascades cover related rows; event removal deletes aggregate records | [projector](https://github.com/anomalyco/opencode/blob/148842ef7cb1bc34192b3fff57d0f9721ab9c801/packages/core/src/session/projector.ts), [session schema](https://github.com/anomalyco/opencode/blob/148842ef7cb1bc34192b3fff57d0f9721ab9c801/packages/core/src/session/sql.ts), [events](https://github.com/anomalyco/opencode/blob/148842ef7cb1bc34192b3fff57d0f9721ab9c801/packages/core/src/event.ts) |
| Snapshot storage is keyed by project and worktree; cleanup performs Git pruning, not session-owned deletion | [v1 snapshots](https://github.com/anomalyco/opencode/blob/148842ef7cb1bc34192b3fff57d0f9721ab9c801/packages/opencode/src/snapshot/index.ts) |
| Tool outputs go to the global tool-output directory; periodic retention cleanup is separate from session removal | [truncation](https://github.com/anomalyco/opencode/blob/148842ef7cb1bc34192b3fff57d0f9721ab9c801/packages/opencode/src/tool/truncate.ts), [directory](https://github.com/anomalyco/opencode/blob/148842ef7cb1bc34192b3fff57d0f9721ab9c801/packages/opencode/src/tool/truncation-dir.ts) |
| The file logger appends to `opencode.log`; printing logs additionally enables stderr | [logging](https://github.com/anomalyco/opencode/blob/148842ef7cb1bc34192b3fff57d0f9721ab9c801/packages/core/src/observability/logging.ts) |

The inspected removal chain does not reclaim snapshots, truncated-output files
or shared log entries. It also catches failures inside the removal body and logs
them. Consequently a successful DELETE response alone cannot establish absence;
a future adapter needs post-deletion checks and must retain uncertain obligations.
Recursively deleting child sessions also requires a proven task ownership tree.

## Integration consequences and unresolved work

1. Reusing an existing service can retain authentication/configuration, but the
   current HTTP deletion contract does not establish complete resource cleanup.
   Deleting shared project directories or truncating shared logs is not a fix.
2. A task-specific process could place writable runtime files in task-owned
   mounts while reading original configuration paths. This is **not implemented
   or validated**. Redirecting its common database to an empty file also removes
   its account/organization selection, so that shortcut is unacceptable here.
3. Copying account rows, extracting tokens into environment variables, or
   re-creating a login would violate the agreed authentication boundary.
4. Before enabling upstream OpenCode launch, establish a supported upstream
   contract that separates task storage/reclamation from account storage without
   altering provider selection. Then implement durable creation/session ownership,
   server identity validation, interrupted cleanup recovery and failure tests.
   Finally run a real task in a new isolated queue and verify both reclamation
   and preservation of shared assets.

No upstream capability change, separate authentication store or relaxed cleanup
requirement is inferred from this investigation. The current launch rejection
remains appropriate until that contract is established.
