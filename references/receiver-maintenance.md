# Receiver and watcher maintenance

> **Current support decision (2026-10-26): only Codex is supported.** OpenCode adapters, CLI, native session/HTTP integration and mixed live harnesses have been removed. OpenCode examples and acceptance records below are historical and must not be used as current instructions. Existing user installations, accounts, files and services are not migrated or cleaned.


Plan maintenance against the exact queue ID, receiver session and executable.
Use Herdr for the visible agent lifecycle and the existing OpenCode or Codex
adapter for its verified native binding. A running terminal or a stored
`phase=monitoring` record does not establish receiver health.

## Pause a watcher without ending its queue

`queue stop` is a terminal queue operation. It rejects an active delivery or an
unfinished, non-cancelled managed task. A stopped queue cannot be watched again;
there is no unstop command. Do not cancel an unfinished task merely to perform
maintenance, and do not edit `stopped` in `queue.json`.

For an authorized process pause:

1. Freeze the known coordinator, producer, publisher and wrapper write entries.
   Inspect active deliveries and unfinished submissions; retain ambiguous work
   for inspection instead of resending it.
2. Identify the watcher by its executable hash, PID and process start identity.
   Stop only that process using the platform's process controls. On Linux a
   pidfd can retain process identity through signal delivery. Do not kill by name.
3. Confirm exit and that both `watch.lock` and `queue.lock` can be acquired.
   Take the authoritative preservation snapshot only after quiescence and lock
   acquisition. A copy taken while a watcher is running is provisional.
4. Preserve any final tick between the initial inspection and exit. Compare its
   semantic changes; do not overwrite them with the provisional copy.

The watcher currently has no graceful signal handler. After a process signal,
`watch.json` may still say `running`. Check process identity, lock ownership and
heartbeat age together; the stale record is not proof of a live watcher. Do not
manually rewrite it to manufacture health.

Locks serialize participating processes. They do not enforce a binary allowlist
or exclude another process running as the same user. Keep the
[writer compatibility requirements](claim-admission.md#compatibility-and-deployment)
explicit when selecting the replacement executable.

## Keep receiver identity explicit

Queue initialization stores the receiver scope, including the OpenCode endpoint,
session, directory and model. Enabling notifications or delivering against a
changed scope is refused. There is currently no queue rebind action, and
`queue init` refuses an existing queue.

Resuming an authorized existing session requires verifying the same binding
again. If the selected receiver is a fresh session, use a separately authorized
new queue identity with the existing registration/init commands. Retain the old
queue and its unfinished tasks as history; do not copy old waits, receipts or
deliveries into the new queue, or count the new queue as completion of the old
tasks. Preserving an original queue while changing its receiver requires a
supported rebind contract that this implementation does not provide.

For a Herdr-visible OpenCode TUI, verify that its HTTP endpoint and Herdr pane
refer to the same exact session before registering it. Follow the
[OpenCode registration sequence](opencode-v2.md#register-a-receiver-and-watcher).
Use the chosen binary by absolute path for every writer, and verify each new
watcher's actual executable and process start identity after launch. Do not
start a notification-enabled watcher against an unavailable receiver.

## Report the boundary reached

Record the old state snapshot, new queue/session identities when applicable,
binary source/hash/build profile, watcher identities and readiness evidence.
An empty session and queue with `active_wakeup=true` establish deployment and
binding readiness only. They do not demonstrate delivery, ACK, result application,
bounded publication or successful context rotation. Keep those acceptance items
open until their own evidence exists.
