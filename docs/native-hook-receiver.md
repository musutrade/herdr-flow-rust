# Minimal native Hook error records (version 1)

This is a Python 3.10+ standard-library command for Linux. It stores one small
local receipt per supported stdin delivery. It does not notify an Agent, invoke
models/tools, read transcripts/configuration/credentials, make network requests,
start services, or recover work. It does not determine whether a business task
succeeded. Installation and native event delivery are **not verified** by the
synthetic tests. The initial use path is Claude StopFailure local recording.
This closes the small implementation scope of Issue #176's 2026-10-31 decision:
use it in actual projects after the basic checks, without waiting for a natural
failure or adding a monitoring platform. Hermes is not connected; Codex has no
reliable failure-terminal event and is **not enabled** for this delivery.

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

## Claude single-instance use

The following commands are **operator instructions**, not an installation already
performed by this change. They prepare a private local bundle and start one new
Claude instance. They do not edit `~/.claude/settings.json`, Orca, Herdr or project
configuration. Run them only when choosing to use this receiver in your project.
Use a normal shell, not an already running Agent's prompt.

The receiver code is pinned to the audited commit
`817097e2b92a4b9653e28f342f8285da87fe52ee`; later documentation-only commits do not
change its bytes. The following example uses fixed, explicit paths. Check that
the parent paths are real directories (no symlink components), owned as expected,
and that this example bundle does not already exist. If it exists, inspect it
rather than overwrite it or create a second copy to bypass a full inbox.

From a checkout containing that commit:

```sh
# Preparation only: no Agent or Hook is run by this block.
BUNDLE=/home/gem/.local/lib/herdr-flow/native-hooks/817097e2b92a4b9653e28f342f8285da87fe52ee
INBOX=/home/gem/.local/state/herdr-flow/native-hooks/claude-project-trial
(
  set -eu
  test ! -e "$BUNDLE"
  test ! -L "$BUNDLE"
  install -d -m 0700 /home/gem/.local/lib/herdr-flow/native-hooks
  mkdir -m 0700 "$BUNDLE"
  # Create the actual output parent AND final directory before using the Hook.
  install -d -m 0700 /home/gem/.local/state/herdr-flow/native-hooks "$INBOX"
  git show 817097e2b92a4b9653e28f342f8285da87fe52ee:scripts/receive_native_hook.py > "$BUNDLE/receive_native_hook.py"
  chmod 0600 "$BUNDLE/receive_native_hook.py"
  printf '%s  %s\n' 5d4504d4fde24582ce9424b03c3c27d3cca1e7e06a30801bb3a9b4889a5d4bf6 "$BUNDLE/receive_native_hook.py" | sha256sum -c -
  /usr/bin/python3 -I -B - "$BUNDLE" "$INBOX" <<'PYSETTINGS'
import json, pathlib, shlex, sys
bundle, inbox = map(pathlib.Path, sys.argv[1:])
command = shlex.join(['/usr/bin/python3', '-I', '-B',
    str(bundle / 'receive_native_hook.py'), '--source', 'claude', '--output', str(inbox)])
settings = {'hooks': {'StopFailure': [{'hooks': [
    {'type': 'command', 'command': command, 'timeout': 5}]}]}}
with (bundle / 'claude-instance.json').open('x') as output:
    json.dump(settings, output, indent=2)
(bundle / 'claude-instance.json').chmod(0o600)
PYSETTINGS
)
```

Failure in preparation means **do not launch**; inspect/retain the partial bundle,
fix the specific preparation problem, and do not treat exit0 from a later Hook
as proof of storage. No executable mode is required: Python reads the script.
Existing parent permissions should be checked before the `install -d` commands;
these commands set permissions on the listed directories, so do not substitute
shared directories without reviewing their ownership/usage.

Start from the actual project's directory, with the assessed binary explicitly
selected (on this machine the binary is the version file itself, not a nested
`.../2.1.312/claude` executable):

```sh
/home/gem/.local/share/claude/versions/2.1.312 --settings /home/gem/.local/lib/herdr-flow/native-hooks/817097e2b92a4b9653e28f342f8285da87fe52ee/claude-instance.json
```

This adds a StopFailure command to that new process's settings. Do not use
`--restricted` or exclude user settings to obtain this coexistence: those options
can exclude the existing user-level Orca/Herdr hooks. Existing Orca StopFailure
and SessionStart, Herdr SessionStart, and other configured hooks remain in their
original files. Hook entries merge across settings levels; managed restrictions
still apply. The command is not a claim of team-wide isolation or a change to an
already-running instance. PATH currently resolved to 2.1.314 during assessment;
using `claude` alone would not pin the assessed 2.1.312. Do not install/downgrade a
binary to follow this example; reassess an available version if the path differs.

A user-level entry is an **optional alternative**, not needed for this first use.
If separately chosen, append one independent element to `hooks.StopFailure` in
`~/.claude/settings.json`; never replace the file, the whole hooks map, the array,
or an Orca entry with the standalone example. User-level settings affect every
session/project loading them, not just this team. Before/after, compare the exact
existing entries/command bytes (or their hashes), not merely counts. Do not edit
Herdr-managed reporter scripts. The single-instance path above needs no such
user-level write.

## Query, capacity and stopping

Receipts are local files, not messages to this conversation. For the example
inbox, query only receipt JSON files (requires `jq`; the receiver itself does not):

```sh
find /home/gem/.local/state/herdr-flow/native-hooks/claude-project-trial -maxdepth 1 -type f -name '*.json' -exec jq -r '[.received_at,.source,.native_event,.session_id,.turn_id,.event_scope,.reported_status,.error_class,.identity_status,.receive_id] | @tsv' {} +
```

No rows means no available receipts, **not** no errors. An invalid/partial JSON
file is unknown, not a successful event. The eleven stored fields are described
above; `error_class=unknown` on Hermes/Codex means no classification is attempted,
not necessarily a parsing failure. IDs are source claims. Invalid IDs can cause
the entire delivery to be refused. Every repeated delivery gets a different ID.

At 256 entries (excluding `.lock`) the receiver refuses new writes. Lock
contention, invalid input and write failures may lose a delivery; some share the
fixed `input_or_storage_failed` code. No automatic retry, rotation or recovery is
provided. To reclaim space, first stop the specified instance, ensure no other
instance uses this inbox, then archive selected receipts you own and remove only
those successfully archived files. Keep relevant evidence; do not delete a shared
parent or remove `.lock` while writers may still use it. No blanket deletion
command is supplied. If ownership/activity is uncertain, retain the files.

**Stop:** exit the specific Claude instance normally; future launches omit this
`--settings` argument. Deleting the settings file alone does not prove a running
instance unloaded it. Once no process uses this bundle/inbox, remove only the
new instance settings/script/bundle if no longer needed, and retain or archive
receipts as desired. Original Orca/Herdr configuration remains untouched. If the
optional global approach was used, remove only its exact appended element;
never restore an old whole-file backup over subsequent user changes.

The external timeout is 5s, not zero latency. Interpreter/receiver failures cannot
be taken as successful receipt delivery. No fault injection or model call is
needed to complete the basic receiver validation; naturally encountered events
can be checked later during real project use.

## Validation and unsupported scope

- **Synthetic receiver:** nine deterministic tests passed, independently rerun
  by the auditor at 817097e2. They prove direct stdin normalization/local storage
  and failure behavior, not Claude native emission. One syscall trace contained
  only the Python interpreter exec, no network or child-process creation.
- **Native startup scope:** the existing Claude 2.1.312 A/B SessionStart probe
  recorded one temporary probe event for the process with `--settings`, none for
  the process without it. Existing Orca and Herdr SessionStart commands also ran
  successfully in both. This is **SessionStart startup-scope evidence only**,
  not execution of this receiver or StopFailure.
- **StopFailure unverified:** an earlier effective-list comparison did not show
  the added handler (cause unknown). A later normal successful turn produced no
  natural StopFailure and no receipt. Neither a TUI count, SessionStart probe nor
  that successful turn proves StopFailure loaded or delivered. Natural failure
  remains unverified; this is disclosed rather than blocking the minimal release
  pending a deliberately manufactured failure.
- **Hermes:** not installed/configured/connected. Request errors are attempt-level;
  `on_session_end` is a best-effort turn outcome and may be absent on early exits.
  A separate profile is not a prerequisite of the receiver. Any future default-home
  configuration must disclose its effect on all sessions/projects loading it.
- **Codex:** no reliable interactive failure-terminal command event established;
  **not enabled** in this delivery. Stop/request adapters exist only for their
  explicitly described nonterminal scopes; no observer or exec substitute is added.

Basic local recording is the delivered scope, not an unattended notification,
diagnostic/recovery service or durable audit system. Actual project use can
provide later natural-event evidence. No further monitoring platform, new model
experiment, or installation is required to close this code delivery. Installation
and project use follow the operator's chosen scope; closing the Issue does not
claim an installation already occurred.
