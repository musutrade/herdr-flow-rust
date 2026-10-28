# Real stage delivery and continuation acceptance

## Scenario and observation boundary

The opt-in test in `tests/common/stage_live.rs` starts one isolated requirement
with two existing criteria and a private local Git target. Its seed candidate is
empty and exists only for environment admission. The harness does not pre-create
a stage, activity, verdict or approval. After the initial instruction, the real
coordinator performs business mutations through `herdr_queue`; the harness only
observes and checks the result.

1. Select the existing `answer` criterion as a stage. A real developer commits
   `answer.txt` with exact bytes `42\n`; `explanation.txt` must remain absent.
2. A separate reviewer inspects that commit and supplies its own immutable stage
   verdict. The coordinator applies it, delivers the commit, then continues the
   same parent through the actual confirmed delivery ID.
3. A new development activity adds `explanation.txt` with exact bytes
   `6 * 7 = 42\n`. A new reviewer checks both criteria against the complete
   original requirement. The stage approval cannot authorize this final scope.
4. The coordinator delivers the new accepted commit to the same ref using the
   first commit as the expected old head, then requests final success. The normal
   supervisor reclaims both stages' task resources.

The read-only audit checks actual target refs and tracked bytes, the unchanged
Owner and agreement, scope epochs 1 and 2, two distinct independent reviewers,
and five distinct native sessions in labelled Herdr tabs in the parent's
workspace. It also checks registered process exit, task roots, worktree and
environment registration removal, an empty task/child queue, active and archived
native inventories, and absence of owned session IDs from Herdr saved state.
The original parent binding and upstream configuration bytes must stay unchanged.

Bounded audit snapshots and startup identities live in the private test fixture,
outside the disposable task roots, so failure can be inspected without inventing
a new task. Failed/unknown work is preserved; the 900-second test observation
bound does not create a product deadline or authorize replay. After inspection,
remove the fixture and raw audit material; it is not a permanent evidence store.

## Running against an existing service

Provide the existing `HERDR_UPSTREAM_SOCKET`, explicit `HERDR_UPSTREAM_MODEL` and
`HERDR_UPSTREAM_ROLE_MODELS` JSON, matching `HERDR_NATIVE_BINARY`, current
`HERDR_PANE_ID`, `HERDR_SESSION_STORE`, and the read-only comparison path
`HERDR_UPSTREAM_CONFIG`. Role models need `developer` and `reviewer` entries.
These are per-invocation settings; no global configuration is rewritten.

```sh
cargo test --locked --test lifecycle_authority \
  stage_live::real_stage_delivery_continues_same_parent_and_reclaims_visible_roles \
  -- --exact --ignored --nocapture
```

Ordinary CI runs the two fixture regressions and leaves the live case explicitly
ignored. CI success alone is not real-model acceptance. This scenario covers a
local Git endpoint, not a publishing role, remote PR/CI integration, early
per-stage reclamation, arbitrary stage counts or human writer handoff.

## Completed live acceptance (2026-10-28)

The corrected test passed **1 test, 0 failures in 323.87 seconds** (323.83 seconds
inside the scenario). It used the existing Codex **0.165.0** service/native binary,
with explicit `gpt-6.1-sol/high` for coordinator and development and
`gpt-6-astra/high` for independent reviewers. Five distinct role sessions appeared
in five labelled tabs in the original parent's workspace.

| Observation | Actual result |
| --- | --- |
| Stage delivery | `9f47994b56cd6bd9022566823ebc25d4e67c0ef4`; tracked `answer.txt` is exactly `42\n`, no `explanation.txt` |
| Continued parent | Same queue/task/epoch/instance and unchanged agreement; scope epoch advances from 1 to 2 |
| Independent review | Stage verdict checks `answer`; a different final reviewer checks both `answer` and `explanation` |
| Final delivery | `1f707db63f27fc7ff1b0ee6724de5c3d2e7365b8`; both exact file contents verified at the actual target ref |
| Final lifecycle | `success`, full-scope approval, no current stage, then empty parent/child queue |
| Cleanup | Five visible roles and six registered processes gone; task roots, worktree and registration removed |
| Preserved environment | Original parent binding and configuration bytes unchanged; existing service PID/start time and socket identity unchanged |

A separate read-only audit covered **12 recorded native session IDs** across both
attempts: ten visible role sessions plus two observed Guardian review children in
the fresh run. All **18 association checks in six shared SQLite databases** were
empty, as were the two task-cwd inventories. Matching rollout/shell-snapshot files
and instance-marked processes were absent. Active and archived native inventories
were empty; saved Herdr state contained no owned role session IDs. These checks
are observations of this installed version, not a promise about future schemas.

The initial failed attempt and its inspected continuation below remain separate.
Neither this single passed scenario nor the ordinary CI suite establishes an
arbitrary-length campaign or the still-unimplemented publishing role.

## Initial harness failures and inspected continuation (2026-10-28)

The initial fixture regression omitted the seed candidate required for environment
admission. Selecting that registered empty seed fixed the fixture; it does not
pre-approve or implement either requirement criterion.

The first live attempt failed after **4.66 seconds**: the audit used `fs::read` on
protected JSON stored as an authority directory. The real coordinator had already
started, so the task and original session/turn were preserved. The audit now uses
the existing bounded protected-document reader, with a regression exercising real
directory-backed storage. The first inspection/resumption also exposed that the
coordinator's native state lives below its receiver's `guard_dir`, while workers'
state lives at the runtime root; the audit now follows that registered location.

After those harness corrections, only the private queue watcher was resumed.
The coordinator retained session `01a17fe1-3765-7100-92ba-db29ae8a8459` and original
turn `01a17fe1-3869-7342-94f9-dfed6147a5c7`; no startup instruction or uncertain
business operation was redispatched. It delivered stage commit
`a286facb472c95e2601b6cabe170f7c1050eec73`, continued the same requirement, and
delivered final commit `a6036583e11f99d36a48c0b746c42bac003e16cd` after fresh overall
acceptance. The five visible role sessions and six registered processes were
reclaimed, task roots were absent, and active/archived native inventories were
empty. This inspected continuation does **not** turn the first live attempt into
a pass, and its lost in-memory configuration baseline cannot be reconstructed.

The inherited #153/#154 production implementation is unchanged by these harness
fixes. See [stage delivery protocol](delivery.md) for its separate regression
evidence and remaining scope.
