# Native automatic review

Use the existing authenticated local Codex app-server. Native Auto-review routes
eligible approval requests to Codex's own reviewer; no Decisions endpoint or
additional API key is involved. The Luna handoff adviser remains editorial only.
Code review, tests, CI and publication authorization retain their existing gates.
See the [official Auto-review documentation](https://learn.chatgpt.com/docs/sandboxing/auto-review).

## New worker setup

Resolve `permissions.auto_review` from team configuration (default true for new
managed workers). Launch the current native protocol bridge in the task's available
Herdr shell, as in [context registration](context-rotation.md#register-and-run).
Wait for its exact ready UUID and `review_events_version: 1` marker. Before sending
any task work, materialize its blank durable rollout without a model turn:

```json
{"id": 1, "method": "thread/inject_items", "params": {
  "threadId": "EXACT_MARKER_UUID",
  "items": [{"type": "message", "role": "assistant", "content": [
    {"type": "output_text", "text": "Worker registration initialized; no task work performed."}
  ]}]
}}
```

Send this request over the registered Unix WebSocket with the app-server client in
`src/codex/rpc.rs`; do not use a Python helper.

Resolve that UUID's transcript, then register and configure before assignment:

```bash
herdr-flow guard init --state-dir /absolute/guard --pane PANE --session UUID --transcript /absolute/rollout-UUID.jsonl --task owner/repo#123 --role dev --auto-review
herdr-flow guard enable --state-dir /absolute/guard
```

Initialization selects `approvalPolicy: on-request` and
`approvalsReviewer: auto_review`. Existing read-only permissions stay read-only;
workspace roots remain scoped as before. Full Access becomes workspace-write with
no extra writable roots and no implicit temporary-directory writes. Sandbox network
access is disabled. Custom named profiles and unsupported environments fail without
changing settings; configure those profiles explicitly before registration.
The helper reads back the complete runtime and checks permissions, model, effort
and other unrelated settings before reporting success. No task turn is sent by it.

Select the role model through the existing settings API, verify it, register its
guard wait with the coordinator, then send the assignment. The saved runtime is
restored and verified before the successor receives work on every rotation.
When `permissions.auto_review` is false, omit `--auto-review`; this preserves the
current runtime and does not forcibly switch to manual approvals.

## Existing registered worker

Installation does not change running workers or reload their bridge processes.
At a safe milestone, settle all commands and write ownership, acknowledge queue
deliveries, disable its guard, and end the worker turn. If its native bridge lacks
`review_events_version: 1`, perform the documented quiescent reattach using the
updated bridge, preserving its exact thread. Rebind through `migrate` before
configuration if its launch identity changed. Do not restart the shared server.

From the coordinator, with the worker confirmed idle:

```bash
herdr-flow guard review-enable --state-dir /absolute/guard --handoff /absolute/quiescent-handoff.json
herdr-flow guard enable --state-dir /absolute/guard
```

The handoff must match its task, epoch and checkout, attest no writer/running
commands, and name independently owned processes. A fresh thread with zero user
messages does not need this attestation. Active workers, enabled guards, unresolved
deliveries and in-flight rotations are rejected before any settings change.

Each settings mutation persists `review_config_sending` first. Failure or timeout
leaves the guard in `needs_attention`; no retry or permission fallback occurs.
Inspect the exact native thread, then use:

```bash
herdr-flow guard review-reconcile --state-dir /absolute/guard
```

Reconciliation only observes. It accepts either the exact requested runtime or the
unchanged previous runtime, records `verified` or `not_applied`, and leaves automation
disabled. Other drift requires inspection. `not_applied` is not successful activation.
The general `reconcile` command cannot bypass this check.

## Notifications and evidence

The byte-transparent native bridge observes `item/autoApprovalReview/started`,
`item/autoApprovalReview/completed` and `guardianWarning` for its bound main thread.
It never answers approval requests. `review-events.json` retains the latest 100
sanitized events with launch nonce, thread/turn/review IDs, status and a bounded
rationale. Commands and tool arguments are omitted. Native transcripts retain the
full evidence. Reset snapshots are preserved in `review-history/`; this is not a
complete log of events evicted by the 100-event window.

The coordinator's existing guard wait emits separate `decision` events for denied,
timed-out or aborted reviews and uncorrelated warnings. Informational warnings
associated with an active structured review share that review's outcome; their
English text is never parsed to decide approval. It deduplicates by review event ID, so a
later independent denial is still delivered. Approved requests are observable but
do not wake the coordinator. Overflow, missing observation for configured workers,
and malformed snapshots produce inspection notices. Delivery still requires the
existing running queue, healthy registered receiver and ACK; no model polling is
added. A restored queue can observe only retained events, so inspect native history
after observer downtime or a retention warning. Old launch/thread events are not
replayed into a different task.

An explicit denial remains a denial. The worker may take a materially safer path
under existing authorization or report the block. Queue ACK confirms receipt only;
it does not approve the action. The integration does not call the denied-action
override API, retry denied commands, or replace native policy. Timeouts are distinct
from denials and do not establish that an action was unsafe.

The validated local server/native version is 0.162.1. Real login-backed tests covered
approval of harmless printing and denial of a second harmless marker under an
isolated stricter fixture policy. Runtime configuration and successor inheritance
were verified through the real API. The denial test establishes the notification
and execution path, not the reviewer's general security accuracy.
