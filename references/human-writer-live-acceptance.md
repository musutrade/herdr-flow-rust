# Opt-in real human writer transfer acceptance

This is a prepared acceptance harness, **not a completed live acceptance**. Ordinary
CI ignores the real-service test. A model/service/native bridge startup failure is
a failed original attempt, not authorization to bypass admission or replace it.

The driver reuses the stage live fixture, queue watcher, upstream socket identity,
protected-document readers, complete native pagination/attachment inventory and
exact PID/start-time process audit. Recovery runs through the existing observer,
read-only Recovery Agent, host verification and original coordinator handoff. It
does not introduce another controller or write model verdicts, ACKs or applies.

## Single success scenario

The newly created private task declares all three criteria before any runtime:
`answer.txt` is exactly `42\n`, `explanation.txt` is exactly `6 * 7 = 42\n`, and
`operator-note.txt` may initially be absent. If the note exists, it must be tracked,
contain the exact predeclared script disclosure and remain in final delivery.
The initial seed commit only admits the fixture environment; it is not approval.

1. A real developer commits the two answer files. The original coordinator ACKs
   and applies its actual result, selects the candidate, then runs a separate
   full independent review against all three original criteria. That reviewer
   supplies its own immutable verdict; the coordinator ACKs and applies it.
2. The coordinator ends its completed, idle turn with `HUMAN-WRITER-READY`, before
   any delivery. The driver checks the real approval, original thread and turn,
   logical writer, tool/send journals and Git fingerprint. A prompt marker alone
   cannot grant the operator write access.
3. The **scripted operator, not a human UI user**, invokes the formal
   `task-runtime human-request` and `human-acquire` commands with the same ID and
   actor. Only a returned `phase=human`, exact Owner and host quiescence proof
   authorize the script to write. A draining acquisition fails this success
   scenario and preserves the original request for inspection; it does not
   automatically retry or repeat original model/Git/send work.
4. The operator adds and commits only the pre-agreed `operator-note.txt`, waits
   for every command to exit, then invokes formal `human-return`. It makes no
   further checkout changes. Return/fingerprint evidence remains under the same
   parent queue/task/epoch/instance.
5. The existing Recovery Agent supplies its real read-only conclusion. The host
   separately verifies quiescence and the returned fingerprint, releases the
   fence, clears the old candidate/approval and advances acceptance round. The
   original immutable verdict remains historical. The same original coordinator
   receives the ordinary recorded recovery handoff; the driver sends no prompt.
6. That coordinator registers the actual operator commit as the new artifact and
   candidate, obtains a **new independent full review**, and ACKs/applies that
   reviewer's actual immutable result. Both reviews check all three original
   criteria, including the final note's tracked bytes. No final development
   role is needed: the operator's commit is the changed artifact.
7. The coordinator performs and confirms delivery to the private bare Git
   repository's `refs/heads/accepted`, then requests terminal success. The normal
   lifecycle reclaims resources. The driver checks final target SHA, all file
   bytes and that the operator commit's parent is the original reviewed SHA.

There are five cumulative role sessions: coordinator, developer, old reviewer,
Recovery Agent and fresh reviewer, in distinct visible tabs in the existing
parent workspace. Development and reviews run sequentially; the test does not
raise the existing active-role concurrency cap. This is one basic success loop,
not a fault matrix or multi-run reliability threshold. Existing `human_writer`
protocol regressions cover blocked verification, late edits and abandonment.

## Explicit configuration and invocation

Provide the existing stage/recovery live settings:

- `HERDR_UPSTREAM_SOCKET`: existing private Unix app-server socket.
- `HERDR_UPSTREAM_MODEL`: explicit coordinator model JSON.
- `HERDR_UPSTREAM_ROLE_MODELS`: explicit `developer`, `reviewer`, `recovery` model
  objects, including desired provider/model/reasoning effort.
- `HERDR_NATIVE_BINARY`, `HERDR_PANE_ID`, `HERDR_SESSION_STORE` and
  `HERDR_UPSTREAM_CONFIG`: matching native binary, existing parent, Herdr saved
  state file and unchanged upstream configuration comparison file.

Additional read-only evidence inputs are required before fixture allocation:

- `HERDR_UPSTREAM_PID`: the original shared service PID; start ticks are pinned.
- `HERDR_NATIVE_DATA_ROOT`: canonical shared native data directory containing
  `sessions`, `archived_sessions` and `shell_snapshots` when they exist.
- `HERDR_HUMAN_AUDIT_DATABASES`: JSON array of **all actual native and Herdr
  association SQLite database paths for this installation**. Pin the complete
  inventory from the actual service/Herdr configuration, including independent
  state databases in use; do not provide a sample just to make the test pass.
  Each database is opened `mode=ro`/`query_only`; its complete table/column
  schema is recorded. Authoritative direct references use exact equality through the installed native
  table/column contract: `threads.id/attached_thread_parent_id`, dynamic tools,
  spawn parent/child edges, attachments, Guardian feedback/read receipts, the
  four history tables, logs, stage1 memories, queued items/revisions and goal
  tables. Optional tables/columns are checked when present. Migration cursors
  and `jobs.ownership_token` are explicitly outside this ownership relation.
  JSON associations require a reviewed explicit relation entry, for example:
  `{"path":"/absolute/state.sqlite","json_relations":[{"table":"panes","column":"agent_session","path":"$.value","meaning":"native-session-association","source":"reviewed product schema"}]}`.
  This is a schema example, not a claim about an installed table: use actual
  product association fields. Plain path entries have no extra JSON relations.
  Freeform log/message text that mentions a task ID or path is outside task
  ownership; it is neither an absence requirement nor deletion authority.
  Only schema and typed association counts are emitted. Unsupported, missing
  or over-budget inputs fail.

```sh
CARGO_TARGET_DIR=/mnt/dev-ssd/herdr-flow-targets/human-writer-live \
  cargo test --locked --test lifecycle_authority \
  stage_live::human_writer::real_human_writer_return_reaccepts_delivers_and_reclaims \
  -- --exact --ignored --nocapture
```

The invocation uses existing login and shared services. It never rewrites proxy
settings, `~/.codex/.env`, account files, or upstream configuration, and never
restarts shared services. Run serially only after reviewing/fixing the real native
bridge dependency and freezing the source/configuration to be accepted.

## Passing evidence and retained failures

`human-live-audit.json`, outside disposable task roots, records the exact Owner,
agreement/config/service/socket baseline, original handshake digest and journals,
role sessions/turns/effective native runtime markers, complete pre-cleanup native
inventories (including Guardian/native children), operator acquisition/return,
model and host verification conclusions, invalidation/round, ACK/apply events,
confirmed delivery and terminal state.

Before Fixture Drop, the test requires exact registered broker/descendant and
frontend PID/start identities to be gone, no instance-marked processes, empty
private root registry and terminal obligations, absent task roots and worktree,
empty parent/child queue, no live Herdr tabs/panes/terminals, no typed Herdr v3 pane `agent_session={agent:"codex",value:SESSION}`
associations (including snapshots and backups), no configured SQLite associations
or matching rollout/shell artifacts, and **complete** ordinary/archived/attached
native inventories empty for every exact private cwd. A `thread not loaded`
response only documents memory state; it does not prove durable deletion.
The original parent binding, upstream service PID/start/socket and configuration
bytes must remain unchanged. CI alone cannot establish any of these live claims.

The 1,200-second bound only limits observation. It does not create a business
expiry, interrupt work, finalize failure, replay an unknown operation or clean up
a failed attempt. A failed fixture remains private and prints its retained root;
`human-live-failure.json` records the precise original Owner, coordinator, queue,
registry and recovery direction. Original protected runtime logs and the audit
remain intact, including an unavailable or failed original recovery result.

Inspect that exact original coordinator, with its retained registry override:

```sh
HERDR_TASK_ENV_ROOT=RETAINED_ROOT/queue/queue.json/task-environments \
  herdr-flow task-runtime human-status --task task \
  --state-dir RETAINED_ROOT/scratch/rt/coordinator
HERDR_TASK_ENV_ROOT=RETAINED_ROOT/queue/queue.json/task-environments \
  herdr-flow task-runtime recovery-status --task task \
  --state-dir RETAINED_ROOT/scratch/rt/coordinator
```

If only the private queue observation process exited, inspect its original state
before restarting `herdr-flow queue watch --state-dir RETAINED_ROOT/queue`; this
resumes observation and does not resend receiver-start or a business operation.
There is intentionally no general automatic live replay/resume entrypoint.
The existing `stage_live_resume` attach/socket/process checks are reused where
applicable, while its pre-send-only replay contract is not weakened for a task
that has already sent work. Never rerun this test against a retained root.

After reviewing the evidence, the operator can decide the explicit original-task
recovery/abandon/failure cleanup action under the existing product protocol. Raw
private audit material is diagnostic evidence, not a permanent public store.

The inventory regression deliberately leaves an external supervisor's log text
mentioning the task UUID and unrelated Herdr saved diagnostic text in place.
Those mentions, a migration cursor and a job ownership token must pass. It separately rejects
an exact native child-thread edge, an exact configured JSON session
association, an archived owned rollout filename and a typed Herdr backup pane
association, and proves the read-only audit leaves SQLite bytes unchanged. This
prevents the audit from requiring deletion of user/observer logs merely because
they describe the experiment. It is not a generic machine-wide text erasure test.

The final live Herdr audit reuses exact JSON string identity matching: a longer
pane/tab/terminal ID or prose mentioning an owned ID is not a retained identity.
The registered-process exit proof checks each root and every saved descendant
by exact PID/start time, even after the root exits or a marker changes. Missing
identity or procfs observation errors fail closed. Diagnostic `direct_children`
are not ownership. Regressions exercise these boundaries with actual processes.
