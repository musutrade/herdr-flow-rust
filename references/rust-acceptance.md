# Rust protocol fixtures and deployment acceptance

> **Current support decision (2026-10-26): only Codex is supported.** OpenCode adapters, CLI, native session/HTTP integration and mixed live harnesses have been removed. OpenCode examples and acceptance records below are historical and must not be used as current instructions. Existing user installations, accounts, files and services are not migrated or cleaned.


## Repeatable protocol fixtures

Run `cargo test --locked --test rpc_client --test herdr_adapter --test tui_bridge`.

`tests/rpc_client.rs` exercises the production Client against a real Unix socket.
The server fixture independently implements its handshake and WebSocket framing;
it checks masked client requests and initialization, response matching, retained
notifications/unrelated responses, ping/pong, RPC errors, bad handshake/JSON,
bounded timeout, disconnect, close codes and event limits. It does not run a
model or contact a remote service.

`tests/herdr_adapter.rs` executes the production Herdr adapter with a temporary
CLI executable and real Unix RPC fixture. Its subprocess environment is isolated
from parallel tests. It checks command arguments and envelopes, pane/shell/TUI
foreground identity mismatches, session/cwd/status mismatches, active-session
prompt refusal, command rejection/failure and blank pane-run responses.
`tests/tui_bridge.rs` separately exercises the byte proxy on real Unix sockets.

These tests establish behavior against controlled peers. They do not establish
compatibility with every installed Herdr/Codex version or GitHub acceptance.

## Installed-service checks (2026-10-20)

Run the opt-in checks with:

```bash
cargo test --locked --test installed_codex --test installed_herdr -- --ignored --nocapture
```

They passed against Codex CLI **0.163.0** and Herdr **0.9.5**. The Codex check
launches and stops its own app-server at a temporary Unix endpoint. It verifies
upgrade/initialize, an ephemeral read-only thread, exact thread/cwd, invalid-thread
rejection and reconnect, without submitting a model turn. The installed schema
requires `sandbox: "read-only"`; the initial check rejected `readOnly`, and the
test was corrected to match the installed protocol. The Herdr check performs
read-only queries on its managed context, validating real pane/process binding
and rejection of a deliberately wrong terminal identity.

These scoped checks supplement the reproducible fixtures for Issue #18. They do
not establish live notification delivery, exact-session ACK/result application,
model-turn recovery or a GitHub publication/merge lifecycle. Those require their
own scenario and evidence. Keep raw logs, credentials and identities outside the
repository; the development evidence directory records the exact commit, commands
and logs for this run.

Historical upstream Python observations in `v1-acceptance.md` do not satisfy this
Rust deployment requirement. Coverage percentages, when reported, require a
commit, command and matching raw coverage artifact.

## OpenCode 1.19.3 baseline native session checks (2026-10-20)

`cargo test --locked --test opencode_session` covers the production loopback HTTP
transport, exact binding, blocked/busy states, ambiguous accepted submissions,
restart reconciliation without replay, failed/partial/late replies, stale task
and candidate rejection, and the result/delivery/driver-ACK/application path.

An owned 1.19.3 server and isolated Git workspace completed registration,
dispatch, completion verification, bound report/signal, queue delivery, driver
ACK and task application using **Magpie (LAN)**, provider `magpie`, model
`opencode-go/deepseek-v4.1-flash` (display name “DeepSeek V4.1 Flash (OpenCode Go)”).
The receiver watcher was stopped and recreated after the durable submission.
Recovery retained the same message ID, and the server history contained exactly
one user submission before collection and queue notification. The work turn's
tools were denied; the response was `ACCEPTED`.

Earlier attempts against the separately configured direct OpenCode providers
returned free-tier 403 and `401 api_key_expired`. The latter also exercised
restart recovery, failed-result collection, delivery and driver ACK without
marking the task complete. These failures are retained as separate evidence;
they do not describe the functioning Magpie configuration. OpenCode materializes
an omitted model variant as `default`; the adapter and regression fixture accept
that equivalent representation while rejecting a changed explicit variant.

ACKs in these scenarios were performed by the test driver with the exact
`OPENCODE_SESSION_ID`, not by the model itself. This establishes the scoped native
protocol and queue lifecycle, not autonomous model tool use, automatic rotation
or a GitHub publication/merge lifecycle. No OpenCode 2.0 process or API workflow
was used. Raw private evidence is indexed outside the repository under the
development run's `24-live-current.json`.
