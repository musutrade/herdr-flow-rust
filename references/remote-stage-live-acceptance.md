# Same-parent remote stage acceptance

## Scenario and fixed inputs

This 2026-10-28 acceptance uses main
`6e01da08a8a11e7207943ced57ed5ed47b2c82ca`, including the independent publisher
from [PR #156](https://github.com/musutrade/herdr-flow-rust/pull/156). It extends
the earlier [local stage scenario](stage-live-acceptance.md) and #156's one-PR
remote run with a stage PR followed by overall delivery in the same requirement.

The target is the isolated
[`acceptance/remote-stage-130-20261028-avzzor`](https://github.com/musutrade/herdr-flow-rust/tree/acceptance/remote-stage-130-20261028-avzzor)
branch. Each PR requires the existing `rust.yml/test`, `crap.yml/crap` and
`audit.yml/audit` jobs for its exact candidate. Landing workflow inventory is
explicitly empty: the existing push workflows target `main`. This does not
establish postmerge CI on the acceptance branch or customer-main publication.

The existing Codex 0.165.0 service supplies explicitly configured roles:
coordinator and both developers use `gpt-6.1-sol/high`, both independent reviewers
use `gpt-6-astra/high`, and both publishers use `gpt-6.1-sol/medium`. Seven distinct
sessions appear in labelled tabs in the original parent's Herdr workspace.

The private driver seeds only the unchanged agreement, initial Git object and
owned environment. After the first instruction it observes; the real coordinator
performs all business mutations through the existing queue tools. The two
developers create the commits, independent reviewers produce their immutable
reports, and separately configured publishers request host-verified effects.

1. Select the existing `answer` criterion as a stage. Add only
   `acceptance/remote-stage-130.txt`, containing exactly
   `remote stage answer 42\n`; the explanation file remains absent.
2. Obtain independent stage acceptance, publish the first PR, verify its CI and
   actual merge receipt, and ACK/apply the original publication result.
3. Complete that exact stage delivery. Keep the original Owner, environment and
   agreement; advance the scope epoch to 2 and invalidate the prior approval.
4. Use a new developer to add `acceptance/remote-stage-130.txt.explanation`, with
   exact bytes `remote stage explanation 6 * 7 = 42\n`. A new reviewer checks
   both criteria and the complete diff from the original baseline.
5. Publish a second PR through a new publisher. Only its confirmed non-stage
   receipt and applied result permit final success and unified cleanup.

## Observations

The inspected original task completed successfully. From the first ready model
presentation to the completed audit it took approximately 1,819 seconds,
including both remote CI waits. The startup failure below is excluded from that
interval and remains a failed invocation.

The first stage passed through [PR #159](https://github.com/musutrade/herdr-flow-rust/pull/159):
candidate `8baa68ab82aba62d075e629ae5f898e8d7cca35c`, actual merge
`6aa137a5562681e3bbbbe609110f5ee61b186752`. The original result was ACKed and
applied before the same parent entered scope epoch 2.

The final candidate `cc7a7fa62260c04d8c7fb917327a43924e4a746f` received fresh
overall acceptance through a different reviewer.
[PR #160](https://github.com/musutrade/herdr-flow-rust/pull/160) merged as
`be548d893c45a2eb1eeaa4186fa21bd6cc5785dd`. The same original coordinator ACKed
and applied its original publisher result, then finalized success at scope epoch
2 with full-scope approval. Both actual merged objects and accepted candidates
have exactly the expected file bytes and no out-of-scope tracked differences.

| Candidate | Rust checks | CRAP gate | Dependency audit |
| --- | --- | --- | --- |
| #159 / `8baa68a` | [passed](https://github.com/musutrade/herdr-flow-rust/actions/runs/40644347461) | [passed](https://github.com/musutrade/herdr-flow-rust/actions/runs/40644347678) | [passed](https://github.com/musutrade/herdr-flow-rust/actions/runs/40644347451) |
| #160 / `cc7a7fa` | [passed](https://github.com/musutrade/herdr-flow-rust/actions/runs/40645824260) | [passed](https://github.com/musutrade/herdr-flow-rust/actions/runs/40645824471) | [passed](https://github.com/musutrade/herdr-flow-rust/actions/runs/40645824431) |

The normal finalizer removed the workspace and temporary roots, registered
worktree, environment registration, seven visible role sessions and ten recorded
processes. The task/child queue, waits, events and deliveries were empty. Active
and archived native inventories and Herdr saved-state references were empty;
the original parent binding was preserved.

An additional read-only check covered nine known native session IDs, including
two observed Guardian children: all eighteen association queries across six
shared SQLite databases returned zero, as did the owned-cwd inventory. Recorded
rollout paths and matching shell snapshots were absent. Upstream configuration,
`.env` and hooks bytes matched their original hashes; both shared service socket
identities and original owner process start times matched. These are observations
of the installed version, not a claim about unknown future storage schemas.

The read-only audit binds each report's recomputed digest, candidate, scope,
revision, round and reviewer session to the exact saved delivery approval. It
checks each publication's original applied event, actual PR/base/head/merge,
remote file bytes and complete diff. Previously witnessed runtime identities
are revalidated, including the original coordinator. A persistent stage
checkpoint is checked without requiring a transient empty-candidate observation.

## Startup failure and inspected continuation

The initial driver stopped on queue lock contention before the first turn was
reserved. The original coordinator session existed; both durable first-turn
markers were absent, broker turn/pending records were empty, and the queue had
only its parent with no activities, waits, events or delivery. The broker and
receiver watcher remained live; the private queue watcher had exited.

Independent review approved continuing that exact unsent prefix. An attempted
read with `includeTurns: true` returned `list_turns is not supported yet`, before
any recovery mutation. This unsupported query is retained as a failed read, not
treated as evidence of an empty native history. Durable pre-send markers and
original host records establish the unsent prefix; the supported identity/status
read establishes only that the same session is healthy and interactive.

Only the verified stopped private queue watcher was restarted. The existing
session then received its first instruction. Bounded retries handled only
`StateBusy` while both start markers remained absent; any reservation or unknown
send blocks replay. No task/session, remote publication or business result was
recreated. The initial failed invocation remains separate from this inspected
continuation and is not relabelled as an uninterrupted harness pass.

No production implementation changed for this run: the task used the frozen
main binary. The subsequent repository changes document evidence and correct
stale execution instructions; they do not create a second live result.

Early stage resource reclamation, human writer handoff, arbitrary stage counts
and global installation switching remain outside this scenario. The umbrella
issues stay open. Temporary drivers, queues and raw observations are disposable;
the formal remote outputs and this scoped result are the retained deliverables.
