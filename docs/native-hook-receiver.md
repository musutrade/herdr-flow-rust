# Native hook local receipts (version 1)

This is a Python 3.10+ standard-library command for Linux. It stores one small
local receipt per supported stdin delivery. It does not notify an Agent, invoke
models/tools, read transcripts/configuration/credentials, make network requests,
start services, or recover work. It does not determine whether a business task
succeeded. Installation and native event delivery are **not verified** by the
synthetic tests.

Run the tests from the repository root:

```sh
python3 -B -m unittest discover -s tests -p test_native_hook_receiver.py -v
```

## Wire contract

The caller supplies `--source claude|codex|hermes --output /absolute/private/inbox`
in exactly that order. The output path cannot come from stdin. Its parent must
already exist. The receiver creates the final directory with mode 0700 or requires
an existing directory owned by this UID with mode 0700. All path components are
opened without following symlinks; spellings containing `..`, `.`, empty
components or a trailing slash are refused. Existing records are never replaced.
The program is not a security boundary against another process with this UID.

Input is one UTF-8 JSON object, up to 64 KiB, terminated by EOF (no newline
required). Duplicate JSON keys are rejected. Input reading has a one-second
monotonic deadline. The program does not read a second input or retry. A producer
that holds stdin open is refused even if its partial input looks like complete
JSON. Nesting/JSON/type errors are application failures, not Hook directives.

| Source / native event | Stored scope and status | Limits |
| --- | --- | --- |
| Claude `StopFailure` | `reported_api_turn_failure`, `failed` | Only the official error enum is copied; otherwise `unknown`. Native input has no turn ID, so turn is always null, even if an extra `turn_id` is supplied. Claude Stop is not accepted. |
| Codex `Stop` | `stop_observed`, `stop_observed` | Not task success or a guarantee no continuation follows. |
| Codex `PostModelRequest` | `model_request`, `request_completed/failed/cancelled` or `unknown` | Request result, not a terminal turn outcome. Error strings are discarded; error class is unknown. |
| Hermes `api_request_error` | `attempt_error`, `attempt_error` | Retry counts never establish terminality. Stringified or structured error/request bodies are discarded. |
| Hermes `on_session_end` | `reported_turn_outcome`, `completed/failed/interrupted` or `unknown` | Requires session and turn plus all three boolean flags with exactly one true. Missing, nonboolean or contradictory flags yield unknown. No aggregation with preceding request errors; not a session-lifetime end. |

Hermes adapter basis: v0.22.5 local `bf0203a794a0`,
`agent/shell_hooks.py:_payload_fields` (87–102) and `_serialize_payload` (620–623).
The seven top-level keys are `hook_event_name`, `tool_name`, `tool_input`,
`session_id`, `cwd`, `profile`, `extra`. Session can be the empty string; turn,
outcome flags and request fields are inside **extra**. This receiver rejects
outcome/turn/request-ID fields at the top level, even if they agree with extra.
A conflicting extra session is refused. Missing extra gives an unknown reduced
outcome; a non-object extra is refused. `turn_exit_reason` is not classified or
stored: it is free-form/version-dependent, not a structured result enum. Only the
three explicit consistent flags establish the reported outcome. These fixtures
are constructed from exact source, **not observed native deliveries**.

Codex capability basis: official `rust-v0.166.0`, commit
`6ff1e1ca3b30fc8b914720681fcf5fe62e95f856`,
[request hooks](https://github.com/openai/codex/blob/6ff1e1ca3b30fc8b914720681fcf5fe62e95f856/codex-rs/hooks/src/events/model_request.rs)
and [Stop](https://github.com/openai/codex/blob/6ff1e1ca3b30fc8b914720681fcf5fe62e95f856/codex-rs/hooks/src/events/stop.rs).
Interactive terminal-failure command hooks remain unsupported by this receiver;
exec JSON is not substituted for interactive support.
Claude enum basis: [official StopFailure reference](https://code.claude.com/docs/en/hooks#stopfailure),
local assessment of running 2.1.312 (PATH 2.1.314), not a version-pinned trigger
sample. Unknown future error values remain unknown.

## Receipt, failures and storage

Only these fields are stored: schema_version, receive_id, source, native_event,
session_id, turn_id, received_at, event_scope, reported_status, error_class,
identity_status. IDs must be 1–128 ASCII letters/digits/`_.:-`; absent or empty IDs
are null. Identity is `source_claimed` when session exists, otherwise
`missing_session`; it is **not authenticated**, and missing session is never
attributed to another Agent. No pane/workspace/instance association is inferred.
The timestamp is local receipt time, not the source event's occurrence time.

No cwd/path/transcript/message/error details/request content or preview is stored.
The fixed error enum is only used for Claude. Unknown events are refused. There
is no semantic deduplication: every accepted delivery has a fresh UUID receive_id,
including repeated identical input. There is no exactly-once claim across writes,
process exit, or stdout. Receipt files are at most 4 KiB, created 0600 with
O_EXCL/O_NOFOLLOW. A nonblocking flock serializes writers; contention may drop a
delivery with a fixed short diagnostic. At 256 directory entries excluding
`.lock`, subsequent delivery is refused; no automatic cleanup/retry/root rotation.
A leftover unlocked `.lock` does not mean the receiver is busy.

The file is directly written; a concurrent reader can see a partial file and
must treat invalid JSON as unknown. Crash/power loss can leave an incomplete or
lost receipt. No fsync/transaction/durability guarantee is made. Partial writes on
ordinary exceptions are removed if possible; interrupted writes may consume a
slot. Operators may archive their own receipts under separately approved cleanup
rules. Missing receipt does not imply success, absence of error, or no delivery.

The receiver always emits **empty stdout**, with no decision/block/context output.
Handled parse, argument, input timeout, directory and write failures exit **0**,
with one bounded program-owned stderr code and no exception/input/path text.
External kills, unavailable Python and interpreter startup errors cannot promise
exit0. Physical filesystem IO cannot be forcibly bounded by this script; use a
short native command timeout. Synchronous waiting can add latency. This is
best-effort local storage, not automatic forwarding to the current conversation.

## Configuration examples — review only, do not apply

The paths below are proposed installation destinations, **not installed files**.
Freeze a full audited SHA before replacing `<SHA>`. Create the approved private
output parent separately. Use an explicit interpreter; do not change PATH.

Claude: append a **separate** element to `hooks.StopFailure` in
`/home/gem/.claude/settings.json`, preserving Orca entries and SessionStart's
Herdr reporter. Never replace the existing array with this standalone fragment.

```json
{
  "hooks": [{
    "type": "command",
    "command": "/usr/bin/python3 -I -B /home/gem/.local/lib/herdr-flow/native-hooks/<SHA>/receive_native_hook.py --source claude --output /home/gem/.local/state/herdr-flow/native-hooks/claude",
    "timeout": 5
  }]
}
```

Hermes: only an explicitly approved isolated profile is a future test candidate.
Do not edit the main profile or restart its gateway. Add to that profile's hooks
mapping (both entries use the same standalone receiver):

```yaml
hooks:
  api_request_error:
    - command: "/usr/bin/python3 -I -B /home/gem/.local/lib/herdr-flow/native-hooks/<SHA>/receive_native_hook.py --source hermes --output /home/gem/.local/state/herdr-flow/native-hooks/hermes-test"
      timeout: 5
  on_session_end:
    - command: "/usr/bin/python3 -I -B /home/gem/.local/lib/herdr-flow/native-hooks/<SHA>/receive_native_hook.py --source hermes --output /home/gem/.local/state/herdr-flow/native-hooks/hermes-test"
      timeout: 5
```

Codex: a separately approved independent command handler under `Stop` or
`PostModelRequest` can use the same command with `--source codex` and a distinct
approved inbox. Preserve existing Orca/Herdr entries and hook trust settings.
Never return exit2 or blocking JSON from a wrapper. No Codex failure-terminal
support is asserted, and current-session config reload remains unverified.

These 5s timeouts are external waiting ceilings, **not zero blocking**. No prompt,
agent, HTTP or MCP hook is proposed. No current configuration has been modified.

## Separate installation/validation decision

1. Freeze script SHA, destination, Python path, inbox ownership/permissions and
   per-Agent scope. Back up exact current config bytes; append only the new
   handler and revalidate JSON/YAML, keeping existing entries.
2. Start with Claude StopFailure only. Confirm reload behavior without assuming
   an already-running Agent has picked up changes. Installation, native Hook
   self-test commands, Agent/model starts and configuration edits need their own
   concrete authorization; this code task authorizes none of those.
3. Synthetic stdin tests prove normalization/storage/failure behavior only.
   Later native dispatch and natural failure are separate evidence levels. No
   natural failure observed means unverified; do not inject one to fill a table.
4. Local receipt creation, summary forwarding, and this conversation receiving
   the summary are three distinct facts. No receiver feature forwards anything.
5. Roll back only the approved new entry/script; preserve Orca, Herdr, old
   candidates and evidence. Do not blindly restore a backup over intervening user
   edits. No old control system, service, hook scanner or recovery is introduced.
