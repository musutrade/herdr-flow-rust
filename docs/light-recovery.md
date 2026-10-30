# Restricted Codex continuation: implementation status

The recovery kernel is implemented, but **there is no enabled production sender
or recovery command yet**. This is not a declaration that #176 is complete.
Record/query/TUI/observation commands retain their existing behavior. The kernel
does not read legacy control-plane files, alter reported state, start a service,
or inject input into a terminal.

## Authorization and evidence

Recovery is limited to an explicitly authorized project and exact Codex thread
UUID. A task binding or Herdr pane is not authorization. The activation timestamp
excludes old failures. `read_latest` reads only that thread's latest ordinal and
its items from an explicitly named history database with SQLite READ_ONLY,
NOFOLLOW and query_only. It does not read terminal logs or scan conversations.
SQLite may coordinate through WAL/SHM; no logical Codex database writes occur.

The history database is an internal projection, not a live execution fence.
Missing, malformed, stale or inconsistent evidence must not authorize sending.
The caller must compare it with the **owning runtime**, including thread UUID,
project, unchanged model, latest failed turn, no active/pending input, no approval
and no unresolved tool/background effect. A terminal item status alone cannot
prove external side effects are known. Unknown item types remain ineligible.

Only `failed` with a structured `serverOverloaded` or an explicitly recognized
terminal transport error is eligible. Structured transport wrappers must have
null or 5xx status; 401/403/429 are excluded. Quota exhaustion, policy, context
exhaustion, unknown errors, warnings, retry messages and interrupted/active turns
are not triggers. Network terminal classification has synthetic tests; the
examined real disconnect examples eventually completed and must not trigger.

## Durable budget

`Controller` owns a private recovery ledger and nonblocking flock until dropped.
Its authorization tuple is immutable. Each send is preceded by a FULL-synchronous
SQLite commit reserving an Unknown attempt. The controller invokes the sender
only after this commit. Exceptions, process exits and ambiguous replies therefore
consume a slot and block further sends, including after restart. Unknown cannot
be cleared by observing an unrelated new turn or by automatic retransmission.

The delays are 30, 60 and 120 seconds after the corresponding failure completion.
At most three send calls occur in one chain. Accepted successor turn IDs keep the
same budget across turns and process restarts. Unrelated failures do not create a
fresh budget; a verified completed terminal state ends a known chain. Old
ordinals remain fenced. Definitive rejection conservatively consumes a slot and
does not permit resending the same failed turn. A missing database behind an
existing lock is an error, not initialization. Corrupt/unknown schemas fail.

An eventual executable must select **one fixed private ledger per authorized
thread**, outside both the project and Codex home. The library accepts a path for
isolated testing; distinct roots do not share a lock or budget. Deleting or
restoring a ledger must never be an automatic recovery procedure. Backup of the
record store is not backup of recovery authorization/attempts.

The continuation message asks the agent to check actual progress/results before
continuing existing authorized work. It explicitly forbids blindly replaying
tools, messages, CI, push, merge or cleanup and preserves model/permissions.

## Production transport gap

The installed Codex schema provides exact `threadId` addressing. This solves the
Herdr pane/session replacement problem; it does **not** provide a latest-failed-
turn precondition. `turn/start` may steer an already active turn. Comparing its
response afterwards cannot undo an unintended injection: the new active turn's
ID can differ from the original failed turn too. `clientUserMessageId` is not,
without an established server contract, proof of idempotency or a turn fence.
`turn/steer.expectedTurnId` applies to an active turn, not recovery of a failed one.
Queue start checks idle, but a newer turn could have completed since the read.

There is also an ownership gap in this environment: an active `--no-daemon` TUI
thread was returned as `notLoaded` by the separate shared daemon. Loading that
thread with daemon `thread/resume` cannot be assumed to continue the existing
runtime and is not performed by this implementation.

`ConstrainedSender` describes the necessary server-side contract, not an existing
Codex capability. The test sender is an isolated substitute. No implementation
of this trait calls the actual Codex or Herdr input endpoints. The minimum missing
primitive is a conditional continuation in the owning runtime: atomically compare
thread UUID, latest failed turn and quiescence, reject pending approval/input/tool
effects, and accept one continuation with a queryable attempt ID and successor
turn. Exposing or modifying that runtime is a separate platform change; it is not
silently implemented by resuming another process or sending to a pane.

## Lifecycle and acceptance still required

After a suitable transport is independently verified, activation must name the
exact target, owning instance, fixed ledger and authorization boundary in a
reviewable launch command. It should run in the foreground with bounded reads,
timeouts and delays; no service installation is implied. Exit releases the lock,
retains all attempts and cancels future sends. Any in-flight send at exit remains
unknown. Restart uses the same ledger; it does not resume an uncertain send.
Budget exhaustion, rejection and unknown must be visible to the operator without
automatically escalating privileges or activating a notification provider.

Current tests exercise the kernel with isolated data and substitute senders:
success and failure chains, three slots across turns/restarts, process exit after
reservation, concurrent processes competing for slot three, unknown quarantine,
target/turn race rejection, old/unrelated failures, error exclusions and read-only
thread-scoped history. They do not establish live sending, idempotency of Codex
input endpoints, a live platform fence, or actual recovery of a production turn.
