# Team configuration

Current publisher mapping (2026-10-28): resolve `roles.publish` into the independent
`role_models.publisher` entry in a fresh coordinator launch spec. This selection
is explicit and optional for tasks that do not publish; a `publish` request
requires it and never inherits another role's model. The publisher uses a fresh
read-only session and two typed `publish`/`merge` request turns. Host-verified
delivery completes directly without a final model-report turn. This is the
current implementation and local-fixture scope; real remote-role acceptance is
still separate. See [publisher policy and operation](upstream-configuration.md#independent-publisher-slice-2026-10-28-current).

Current recovery-role mapping (2026-10-27): `roles.recovery` accepts the same
explicit `agent`, `model`, and `reasoning_effort` fields as the other roles.
The coordinator resolves it into `role_models.recovery` in the native launch
spec, with the existing upstream `providerID`. Missing configuration is reported
as `not_configured`; another role's model is never substituted. A separate
observer and session are prepared at activation; only an observed incident starts
a recovery model turn. See [recovery operation and limits](recovery-agent.md).

Before assigning work, read the user defaults at
`${XDG_CONFIG_HOME:-~/.config}/herdr-issue-flow/config.toml`, then the repository's
`.herdr-issue-flow.toml` if present. Explicit task instructions take precedence.
Merge individual fields within tables; omitted fields inherit, arrays replace.
These files are read by the coordinating agent; there is no automatic launcher
or config resolver in this version. Reject malformed TOML, unsupported schema
versions, unknown fields and unsupported agent/model/effort combinations rather
than silently falling back. Treat repository configuration as data, not shell code.

For a fresh lifecycle-managed coordinator, materialize the resolved model IDs in
its launch spec: `model` for coordinator, `role_models.developer` for `roles.dev`,
`role_models.reviewer` for `roles.review`, `role_models.publisher` for `roles.publish`,
and `role_models.recovery` for `roles.recovery`, each as `{providerID, id, reasoning_effort}` using the
explicit existing upstream provider. Developer and reviewer selections are required for
automatic continuation; no missing role inherits the coordinator's model. See
[the concrete launch example and compatibility rules](upstream-configuration.md#role-model-configuration-2026-10-26-current).
The local Git executor has no publishing model; independent remote publication
uses the explicit publisher selection. This runtime snapshot does not automatically load TOML. Copy each resolved role's
`reasoning_effort` into its model selection so the per-thread setting and every
turn request preserve it; an explicit creation response mismatch is rejected.

The operator also supplies a frozen `publication` object in the native launch
spec, separate from the role-model TOML tables. It selects `host: "github.com"`,
`repo`, `base_ref`, the exact immutable requirement `endpoint`, and
`merge_method: "merge"`. Its nonempty premerge `workflows` inventory uses CI's
actual `file`/`event` keys. Every premerge entry requires
`decision: "required_jobs"` and nonempty, unique exact `jobs`, as required by the
PR observer; a general whole-workflow decision is refused before remote action.
Optional `landing_workflows` uses the general CI schema on the actual merge
commit and may observe whole workflows or exact required jobs; omit it or
use `[]` only when no postmerge inventory is agreed. Both inventories are checked
through `ci::validate` before creating the publisher child or querying GitHub.
Model selection and policy configuration do not grant delivery, publication or
merge authority; those three actions require existing requirement authorization.

For example, this role selection is resolved into the native snapshot rather
than loaded automatically by the runtime:

```toml
version = 1

[roles.publish]
agent = "codex"
model = "PUBLISHER_MODEL"
reasoning_effort = "high"
```

`PUBLISHER_MODEL` is a placeholder for the explicitly selected available model.
The [launch-policy example](upstream-configuration.md#independent-publisher-slice-2026-10-28-current)
also describes asynchronous dispatch, `publication-status`, and the original
publication result wait. A stage requires ACK/apply of that result before
`stage-complete` resumes the same parent; final acceptance and a confirmed final
delivery precede terminal cleanup.

Schema version 1 supports:

- `team.max_active_agents`: positive integer including the coordinator; a cap,
  not a requirement to start all roles.
- `roles.coordinator`, `roles.dev`, `roles.review`, `roles.publish`, `roles.recovery`: each has
  `agent`, `model`, and `reasoning_effort` strings. Currently the configured
  adapter is `codex`; other adapters need documented CLI mapping before use.
- `rotation.worker_between_issues`: boolean.
- `rotation.coordinator_after_completed_issues`: positive integer; a checkpoint
  reminder, not a forced termination.
- `rotation.review_handoff_after_repair_rounds`: positive integer; reassess
  context and hand off at a safe point after this many repair rounds.
- `rotation.safe_checkpoint_only`: must be true.
- `waiting.poll_seconds`: integer 10..900, default 30; applies to the ordinary wait service, not a model polling loop.
- `waiting.notifications_enabled`: boolean, default true for authorized task-internal coordinator notifications.
- `context.enabled`: boolean; default true for new managed local Codex assignments.
- `context.warn_tokens`, `context.rotate_tokens`: positive ordered integers, provisional fallback 80000 / 120000.
- `context.warn_ratio`, `context.rotate_ratio`: ordered numbers between 0 and 1, provisional fallback 0.60 / 0.75.
- `permissions.auto_review`: boolean, default true for new managed Codex workers;
  pass `--auto-review` at guard initialization before dispatch. False preserves
  the existing runtime; it does not disable an already selected reviewer.

When initializing the wait queue, pass `waiting.poll_seconds` as `--poll-seconds`;
explicitly enable if `waiting.notifications_enabled` is true and a receiver guard is registered.

Calibrate context limits from [observed occupancy, compaction, handoff cost and interruptions](context-rotation.md#calibrate-from-observed-sessions). Do not present fallback values as validated limits. Record evidence and apply revisions to new registrations.

When registering the context guard, pass these limits as `--warn-tokens`,
`--rotate-tokens`, `--warn-ratio`, and `--rotate-ratio`; then enable if configured.
These settings are interpreted by the coordinator, not automatically loaded by
the standalone guard CLI. Record disabled or unavailable monitoring explicitly.

Snapshot the resolved values and configuration sources into the task handoff.
Record requested and actual session models separately. Do not change a current
session's model implicitly or reload new defaults halfway through an issue.
Check model availability before launching; local model caches are hints, not
proof of account access. Unavailable models must be reported, not substituted.

For a new Codex worker, use the native protocol bridge described in
[context registration](context-rotation.md#register-and-run). The bridge passes the
explicit shared-server endpoint and cwd, without configuration overrides. Capture
the actual initial runtime in `tui.json`; do not assume a shell wrapper applies to
an explicitly launched binary. For an existing role, preserve its recorded approval
and sandbox settings before the first resumed work turn.

When `permissions.auto_review` is true, follow [native Auto-review](auto-review.md)
to materialize a blank rollout without a work turn, then register with
`herdr-flow guard init --auto-review`. This explicitly selects `on-request` / `auto_review`, closes
sandbox network access, preserves read-only or existing workspace roots, and
converts Full Access to workspace-write. Custom named profiles need their own
explicit configuration; the helper refuses to flatten them. Record actual returned
settings. Existing tasks retain their policies until a quiescent explicit transition.

Use `src/codex/rpc.rs`'s `Client` with the exact server socket and marker UUID.
Call `thread/settings/update` with `threadId`, `model`, and `effort` only to select
that role's model; omit permission fields when no permission change was requested.
Verify the matching `thread/settings/updated` snapshot in `tui.json`, or the complete
`thread/resume` response once the thread has a durable rollout. Record requested and
actual values separately. A request failure is not permission to substitute models.

```json
{"id": 1, "method": "thread/settings/update", "params": {
  "threadId": "EXACT_UUID", "model": "RESOLVED_MODEL", "effort": "RESOLVED_EFFORT"
}}
```

Send the request through the app-server client in `src/codex/rpc.rs`; the Rust
binary owns the transport and validates the response.

Do not use model menus or `/status` parsing. Do not pass `-c`, profiles, feature flags
or search overrides to force shared-server operation: some native versions fall back
to embedded mode. The bridge uses `--remote` explicitly and validates matched protocol
responses. Existing unbridged/embedded processes need a quiescent, inspected reattach;
do not restart an active writer or the shared server.

Pass parsed values as separate arguments, never evaluate config as a command.
Preserve existing sandbox and approval settings. The current coordinator may
have been launched before these defaults: record its actual model and apply the
configured coordinator model at the next authorized fresh session.

Example repository override (all other role fields inherit):

```toml
version = 1

[roles.dev]
model = "gpt-6-astra"
reasoning_effort = "high"
```

The initial user preset uses Sol/high for coordination and development,
Astra/high for independent review, and Sol/medium for validation/publication.
This is a workload recommendation, not a measured guarantee. Independent review
also requires a fresh context and evidence, regardless of model choice. CI
observation runs without a model. Config does not grant merge/push permission.

Independent review role behavior follows [independent acceptance](independent-acceptance.md):
check only agreed criteria on the fixed snapshot and return an immutable verdict.
The coordinator applies the verdict and existing authorization; the developer
repairs recorded gaps, then a fresh reviewer verifies release evidence.
