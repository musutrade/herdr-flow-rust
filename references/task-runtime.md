# Task-owned Codex and OpenCode runtimes (#35)

The linked task model is described in [task-lineage.md](task-lineage.md).
A producer is now an explicit `{ "adapter": "codex" | "opencode", "session": "..." }`
object on its result wait. Legacy `producer_session` remains Codex-only. Supplying
both is allowed only when they name the same Codex identity. Registration writers,
successor intents, writer claims, command execution, takeover and result signals
use the same identity. Successor actions specify `adapter` (legacy default: codex).
`progress::accept_identity` is the adapter-qualified library entry point;
`progress::accept` remains the legacy Codex wrapper.

The CLI selects `OPENCODE_SESSION_ID` for an OpenCode producer or coordinator and
`CODEX_THREAD_ID` for a Codex identity. Task takeover selects the producer from the
pending operation, independently of the coordinator's adapter. Native collectors
verify their bound session directly; they do not set or impersonate either variable.

## Owned environment and fresh activity

`task_runtime` builds on the owned `TaskEnvironment` API from #33. It does not take
ownership of an existing shared server, user session, user home or global settings.
The new native path supports Git snapshots; other artifact adapters are rejected
explicitly here, while the generic lineage API still supports its own adapters.

1. Create/restore a task environment. Create the linked requirement, input artifact
   and activity through `task-lineage`. Its signal wait initially has no producer.
   Put the queue and coordinator state inside the disposable environment.
2. `task_runtime::prepare(&environment, assignment)` creates a private runtime root.
   Provision only the explicitly required provider configuration/credentials into
   that root (`c/config.toml`, `c/auth.json` for Codex;
   `config/opencode/opencode.json` for OpenCode). No user conversation, memory,
   plugins or global configuration is copied automatically.
3. `task_runtime::launch(&mut environment, &root, &spec)` starts a dedicated native
   server and records the marked process. `spec` contains `adapter`, absolute
   `binary`, owned `cwd`, and `model: {providerID, id}`. OpenCode also needs an
   explicit loopback `port`. Library embedders provide `driver_binary` pointing to
   herdr-flow for the Codex connection process; the CLI uses its own executable.
4. `task_runtime::native::create` takes `queue_dir`, `wait`, a concrete `instruction`
   and optional evidence references. It constructs an input packet from the current
   requirement and activity binding, creates a **new** native session, verifies
   the returned directory/model/session and empty conversation, then binds the
   wait. Development also acquires its workspace writer. Acceptance requires a
   separate checkout from the input artifact and all active writers.
5. `native::dispatch` verifies the immutable packet digest, current task binding,
   exact input HEAD and tracked contents, and coordinator notification readiness.
   It persists the send intention before submitting the packet once.
6. `native::collect` reconciles the existing submission and emits the existing queue
   signal. The coordinator still ACKs, inspects evidence and applies the result.
   A completed native turn is execution status, **not** acceptance approval.

CLI equivalents use `herdr-flow task-runtime prepare|launch|create|recover|dispatch|collect
--task TASK --state-dir ROOT [--spec FILE]`. `prepare` requires `assignment` in its
spec and returns the computed runtime root; subsequent commands use that root.
The environment must already be registered. There is no implicit new environment
or replacement session when a command fails.

## Isolation and lifetime

Runtime HOME, XDG configuration/data/state/cache/runtime directories, CODEX_HOME,
OpenCode configuration, temporary files, logs and output all live inside the
owned environment. Native processes inherit the environment instance marker.
Only the configured proxy and certificate environment is forwarded for provider
connectivity; other inherited runtime/session configuration is cleared.

OpenCode remains pinned to **1.19.3**. Its runtime receives a second Landlock
restriction limiting writes to its own runtime root and activity checkout.
Codex 0.163 also uses a fixed `/tmp/codex-daemon-UID` socket/lock fallback. Its
server therefore runs through `/usr/bin/bwrap` with a private `/tmp`, private
`/dev` and `/proc`, and a read-only outer filesystem; only its runtime root and
activity checkout are writable. Bubblewrap and usable unprivileged namespaces
are required. Missing confinement fails; no shared-directory fallback is used.
`/dev/null` is an allowed nonpersistent sink in the Landlock command path.

A task-owned Codex broker retains the original native connection, preserving a
fresh thread before its first turn. It records exact-thread turn events and
pending native approvals without answering approvals. Its request surface is
limited to the native thread/turn methods used by this adapter. Losing this
process does not authorize a new thread or another turn. The old session must be
verified on recovery; an unpersisted thread may require explicit cancellation
and a fresh activity, never implicit replay.

The native socket's private `/tmp` fallback is resolved inside the owned runtime
root. A held directory descriptor permits connecting even when the host pathname
is longer than a Unix socket address. No external symlink or cleanup exception is
created. Servers, connection processes, logs, native session databases/rollouts,
configuration copies, queue records and journals are reclaimed by
`destroy_environment`; no completed-task archive is created. Namespace bootstrap
runs before the read-only mount policy (it must configure its uid map); the native
agent is executed only after that policy is installed.

## Output and recovery guarantees

Development pins an **input**, then reports its actual output commit with the input
identity. Collection does not require development HEAD to remain at the input.
A collected output stays stable across repeated collection even if development
subsequently advances. Applying the output still uses the lineage transaction.
Acceptance keeps the submitted HEAD and tracked contents fixed; native collection
rejects drift. Untracked build/test outputs may exist in the isolated checkout.

Creation, submission, native completion, signal persistence, coordinator ACK and
acceptance are distinct facts. Creation and turn sends are never retried after
ambiguous responses. Only read-only startup health observations are retried.
`recover` binds only a recorded, verified session and the original assignment.
Approval/user-input blockers, partial responses, unknown sends, timeouts, failed
turns and terminal reports retain their separate meanings. Collectors never turn
a timeout into success and never automatically answer a native approval.

Codex metadata reads avoid deprecated full-history hydration. Collection uses
paginated turns when supported, otherwise exact-thread broker events. An owned
native rollout is a final recovery source: session metadata, turn context,
model/cwd and terminal turn id must all match. A partial rollout is not completion.
An OpenCode terminal journal is revalidated against its producer binding on every
collection, including retries.

## Verification scope

`tests/opencode_session.rs` and `tests/lineage.rs` cover initial/successor OpenCode
writers, adapter mismatches, takeover, real Git output commits, fixed acceptance
HEAD/contents and non-replayed submission recovery. `tests/task_runtime.rs` contains
explicit synthetic HTTP/WebSocket fixtures for normal CI and ignored installed
runtime tests. The installed tests create dedicated real sessions, optionally run
model inference, and destroy the entire environment. Their coordinator is a
protocol fixture; they are not evidence of a fully autonomous multi-agent
acceptance/delivery loop (that remains the later end-to-end scope).

The installed test takes `HERDR_LIVE_{CODEX,OPENCODE}_BINARY`, corresponding
`_MODEL` JSON objects and optional `_CONFIG` paths. `HERDR_LIVE_CODEX_AUTH` supplies
an explicitly selected auth copy; `HERDR_LIVE_INFERENCE=1` requires an actual
successful native model response. `HERDR_LIVE_ADAPTER` can select one adapter.
`HERDR_LIVE_ROLE=development` additionally requires native tools to modify the
fixture file and create a new Git commit, collected as the activity's output.
These inputs are copied only into disposable test environments. Failures are
reported and the test environment is cancelled/destroyed, not marked delivered.

Codex turns declare `externalSandbox` because the enclosing task-specific
bubblewrap namespace is the enforced filesystem boundary. Native approval policy
defaults to `untrusted`. An explicitly authorized runtime may select
`approval_policy: "on-request"` (used by the disposable development test); these
are the only supported policies. This does not answer pending requests or change
a user's settings. A native command that needs approval stays blocked for explicit
handling.

Formal reviewer reports and their four continuation outcomes use the
[independent acceptance protocol](independent-acceptance.md). Native collectors
register the bound report separately from execution completion.
