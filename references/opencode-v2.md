# OpenCode 1.19.3 integration

## Current version policy

The integration baseline is OpenCode **1.19.3**. At the user's direction on
2026-10-20, **2.0 integration is deferred because of reported bugs**. Do not
upgrade to 2.0 or copy its API workflow. Reopening 2.0 requires a new user
decision and separate validation. This file retains its historical
`opencode-v2.md` name to preserve links; earlier 2.0.32 observations are historical.

## Native session adapter

`src/opencode/session.rs` and its HTTP transport implement native 1.19.3
session binding, status, dispatch and durable result collection. They use the
installed `/global/health`, `/session`, `/session/status`, `/permission` and
`/question` APIs. The production receiver factory selects this adapter when the
registration declares `adapter: "opencode"`; it does not send Codex RPC methods.

The endpoint must be an explicit loopback HTTP address and port, such as
`http://127.0.0.1:4096`. It cannot contain credentials, a path or query. The
transport disables proxies and redirects, bounds each request to ten seconds
and limits responses to 8 MiB. For a password-protected server, provide
`OPENCODE_SERVER_PASSWORD` and optionally `OPENCODE_SERVER_USERNAME` to the
client/watcher environment. Credentials are not stored in registration JSON.
The endpoint and local files remain a local trust boundary; this does not
protect against a malicious process with the same user identity.

Use an existing explicitly chosen session. Inspect the installed interfaces:

```bash
opencode --version
opencode run --help
opencode serve --help
opencode session --help
```

Configure the server endpoint when starting an owned session; do not restart an
active writer, replace its session, or change its model merely to enable this
integration. Registration checks a healthy server at exactly 1.19.3, the exact
session ID, directory and model (including a specified variant). Mismatches fail
before submission. Pending questions/permissions block dispatch; no approval is
answered automatically. Unknown or malformed status cannot establish readiness.

## Register a receiver and watcher

Keep registration, state and evidence outside the workspace. A registration spec
contains:

```json
{
  "endpoint": "http://127.0.0.1:4096",
  "session": "ses_EXACT_EXISTING_ID",
  "cwd": "/absolute/workspace",
  "model": {"providerID": "EXACT_PROVIDER", "id": "EXACT_MODEL"},
  "task": "org/repo#123",
  "role": "coordinator"
}
```

The model must already match the session. Registration never switches it:

```bash
herdr-flow opencode register --state-dir /absolute/receiver --spec /absolute/registration.json
herdr-flow opencode status --state-dir /absolute/receiver
herdr-flow opencode watch --state-dir /absolute/receiver --background
herdr-flow queue init --state-dir /absolute/queue --guard-state-dir /absolute/receiver
herdr-flow queue watch --state-dir /absolute/queue --background
herdr-flow queue enable --state-dir /absolute/queue
OPENCODE_SESSION_ID=ses_EXACT_EXISTING_ID herdr-flow queue readiness --state-dir /absolute/queue
```

Use `opencode watch`, not the Codex context guard, for this registration. It
maintains the ordinary watcher lock, heartbeat and native submission observation.
Contention with a queue operation skips that tick without terminating the watcher.
`active_wakeup` can be true while `context_telemetry` and `safe_rotation` remain
false. No exact context occupancy or automatic rotation is claimed.

When OpenCode is the queue receiver, `queue ack`, `queue readiness` and managed
task commands use `OPENCODE_SESSION_ID`, checked against its registration.
Codex queues continue using `CODEX_THREAD_ID`. Do not fabricate a Codex identity.
A received notification includes the exact OpenCode session environment setting.
ACK confirms receipt; result acceptance still requires `task-apply`.

## Dispatch a managed producer task

First establish the coordinator's notification readiness, register an open
`kind: "signal"` wait with task, epoch, assignment and full candidate SHA, and
register the managed task. See [task progress](task-progress.md). The producer
can have its own registration; the receiver can be Codex or OpenCode. Omit
`producer_session`, which remains the Codex-specific producer check.

A dispatch spec contains:

```json
{
  "queue_dir": "/absolute/queue",
  "wait": "registered-result-wait",
  "prompt_file": "/absolute/prompt.txt",
  "timeout_seconds": 600
}
```

```bash
herdr-flow opencode dispatch --state-dir /absolute/producer --spec /absolute/dispatch.json
herdr-flow opencode collect --state-dir /absolute/producer
```

Dispatch verifies coordinator readiness, the current task binding and workspace
HEAD before writing its submission journal and sending one `prompt_async`
request. The journal records the message ID, prompt digest, exact session/model,
source fingerprint, result channel and deadline. An accepted request does not
prove completion. A timeout or ambiguous response preserves that journal;
subsequent dispatch is refused until its exact outcome can be reconciled.
Absence from message history never authorizes automatic resubmission.

The watcher or `collect` reconciles the exact user message and linked assistant
reply. It checks session, model, directory and prompt identity; partial replies,
blocked sessions, API failures and unknown outcomes remain distinguishable.
Only a completed, idle-session reply with a successful stop is `complete`;
reported errors become `failed`, other terminal finishes become `decision`.
The last 200 messages are queried within the response limit; if correlation is
no longer available, preserve the unknown outcome for inspection.

`collect` rechecks the queue, current task/epoch/assignment/candidate, workspace
HEAD and live session binding before saving the report and signaling. Repeated
collection of the same still-current result is idempotent. Reports retain the
before/after source fingerprints; completion means the agent finished, not
source PASS. The coordinator checks the report, ACKs delivery and explicitly
applies the result. Late or superseded results cannot advance a new binding.

Generic signals still trust local filesystem access. Their descriptive metadata
alone does not authenticate an OpenCode process; the native collector supplies
its separate live checks. Keep reports and credentials outside Git.

## Monitoring diagnostics and unsupported lifecycle operations

The older `OpenCodeAdapter` in `src/opencode.rs` remains available for read-only
pane/terminal, shell PID and process checks through `capabilities` and `verify`.
Its static baseline reports `available: false` and `probe_required: true`; it is
not a live server probe and no longer advertises checkpoint restoration or
successor startup. Shell readiness is not OpenCode agent idleness or session
continuity.

Neither adapter provides exact runtime snapshot/restoration, automated exit or
successor launch, safe context rotation or review-policy changes. These require
separate feasibility evidence and implementation on 1.19.3. Never infer current
context occupancy from cumulative token counters. Recovery that needs a session
replacement remains an inspected manual operation.

## Validation scope

Offline HTTP/process tests cover identity mismatches, busy/approval/unknown
states, partial/failed responses, ambiguous submissions without replay, stale
candidates, queue delivery/ACK/application and model variants. Rust-specific
installed-service observations are recorded separately; historical Python/v2
observations do not validate this implementation. See
[Rust acceptance](rust-acceptance.md) and [waiting](waiting.md).
