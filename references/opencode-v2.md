# OpenCode integration

OpenCode has two distinct integration paths in this skill. Do not mix their
capabilities or treat either one as a native Codex guard.

## External review producer (OpenCode v2)

Validated with OpenCode 2.0.32 and `opencode/deepseek-v4.1-flash-free` in an
existing user-owned session. Use the installed CLI's operation schema; v1 HTTP
routes and payloads are not interchangeable with v2. Do not replace the session
or change its model without the task's authorization.

Inspect the exact session and current model:

```bash
opencode api session.get --param sessionID=EXACT_SESSION_ID
```

When authorized, the v2 model operation is `session.switchModel`, with the same
session parameter and data `{"model":{"providerID":"opencode","id":"deepseek-v4.1-flash-free"}}`.
Verify the returned session through a new GET before dispatch. A v2
`session.prompt` uses that session parameter and a body with `text` and optional
`metadata`. Its response admits an inbox message; it does not prove completion.
An ambiguous response must be inspected, never blindly resubmitted.

Before sending the prompt, establish coordinator notification readiness and
register a bounded `kind: "signal"` wait, the managed task, assignment, full
candidate SHA, completion condition and report path. Record the real OpenCode
session/model/pane as `producer_identity` metadata. Omit `producer_session`:
that field currently verifies a native Codex `CODEX_THREAD_ID`, not OpenCode.
Give the producer its exact registered `herdr-flow queue signal` command and ask
it to publish a compact report before signaling. Keep evidence outside Git.

The coordinator ACKs the delivered batch, checks the report, independently
verifies the live OpenCode session/model and source input identity, then uses
`task-apply` to record findings, acceptance and a concrete authorized successor.
A complete signal means the review finished; it does not mean source PASS.
A command successor must register its result wait, persist `task-begin`, and
take over through the command wrapper before its result can advance the task.

Generic signals rely on local filesystem trust. The queue does not enforce
`producer_identity` metadata or authenticate the OpenCode process. Independent
coordinator checks are required; the signal is not a trusted App Check.
OpenCode automatic safe context rotation is unsupported. Aggregate API token
counters do not establish current context occupancy. Native event streams are
volatile, so this integration uses the existing durable signal/ACK mechanism.

## Monitoring-only session adapter (OpenCode 1.19.3)

`src/opencode.rs` provides the foundation for observing an existing
OpenCode 1.19.3 pane without claiming native Codex capabilities:

- Verifies the Herdr pane terminal, shell PID and process identity, and can
  report shell readiness.
- Reports its capability baseline through `check_capabilities`.
- Uses an idle shell as its only idle signal; OpenCode does not expose session
  or thread status to this adapter.

It does not support:

- the Codex app-server JSON-RPC surface (`thread/read`, `thread/resume`,
  `thread/settings/update`);
- TUI bridge identity/probing, runtime snapshots or exact setting restoration;
- context telemetry or automatic safe rotation;
- programmatic prompt, exit or successor launch.

Use it only while the OpenCode pane is in the monitoring phase, with no active
writes and no pending review. Keep the durable handoff outside the checkout.
If the session dies during handoff, recover by inspected manual restart. The
adapter is not wired into `herdr-flow guard`; automatic OpenCode rotation remains
unsupported (Issue #14). Do not register an OpenCode session as a Codex guard or
claim automatic waking/rotation for it.

Create it with `create_opencode_adapter(state)`, where `state` contains `pane`,
`shell_pid`, `cwd` and `terminal`. Unsupported operations raise `GuardError`
rather than silently degrading.

## Shared boundary

Both paths keep configuration, state, evidence and credentials outside the
repository. Neither path grants publication, merge, approval or infrastructure
authorization beyond the existing task contract. For notification, result
application and recovery rules, see [waiting](waiting.md) and
[task progress](task-progress.md).
