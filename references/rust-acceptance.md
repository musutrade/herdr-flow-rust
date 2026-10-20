# Rust protocol fixtures and deployment acceptance

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
