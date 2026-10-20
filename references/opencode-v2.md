# OpenCode 1.19.3 integration

## Current version policy

As of 2026-10-20, the integration baseline is OpenCode **1.19.3**. At the
user's direction, OpenCode **2.0 is deferred because of reported bugs**. Do not
upgrade to 2.0 or use its API workflow for this integration. Reopening 2.0
integration requires a new user decision and separate validation.

This file retains its historical `opencode-v2.md` name to preserve existing
links. The earlier 2.0.32 external-review observation is historical evidence,
not the current integration procedure or acceptance of the Rust adapter.

## OpenCode capabilities and repository coverage

The locally installed 1.19.3 CLI help was checked on 2026-10-20. It exposes
`run --session`, `run --attach`, `run --format json`, `serve`, `acp` and
`session list`. Inspect these interfaces without submitting work:

```bash
opencode --version
opencode run --help
opencode serve --help
opencode session --help
```

These are programmatic integration entry points; their presence does not mean
this repository has implemented or validated dispatch through them. The Rust
adapter currently implements only monitoring diagnostics. Its unsupported
prompt/status/lifecycle operations are limitations of this adapter, not proof
that OpenCode itself has no programmatic interface. Codex app-server methods
are not an OpenCode protocol.

For any future 1.19.3 dispatch implementation, verify the exact installed
interface and live session binding before use. Do not copy 2.0 API commands or
payloads. Safe automatic rotation, context occupancy measurement and exact
runtime restoration still require separate implementation and validation.

## External producer result return (manual coordination)

An independently operated 1.19.3 session can use the generic signal mechanism
to return a report. This is a result-return contract, not a built-in OpenCode
dispatch client or a claim of a validated 1.19.3 end-to-end workflow. Verify the
exact session, model and pane independently. Do not replace the session or
change its model without the task's authorization. An accepted prompt does not
prove completion; inspect an ambiguous submission instead of blindly retrying.

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
- Uses an idle shell as its only idle signal; the adapter does not query
  OpenCode session status and returns `unknown` for agent status. Shell readiness
  does not establish whether a running OpenCode session is idle.

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

The current static capability report overstates support: it lists checkpoint
save/restore and successor startup, while runtime snapshot/restoration and
successor launch return unsupported. Treat the implemented monitoring checks
above as the supported scope; `available: true` is not a live capability probe.

## Shared boundary

Both paths keep configuration, state, evidence and credentials outside the
repository. Neither path grants publication, merge, approval or infrastructure
authorization beyond the existing task contract. For notification, result
application and recovery rules, see [waiting](waiting.md) and
[task progress](task-progress.md).
