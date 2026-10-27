# Independent acceptance and continuation (#36)

> **Current support decision (2026-10-26): only Codex is supported.** OpenCode adapters, CLI, native session/HTTP integration and mixed live harnesses have been removed. OpenCode examples and acceptance records below are historical and must not be used as current instructions. Existing user installations, accounts, files and services are not migrated or cleaned.


This protocol extends [task lineage](task-lineage.md) and
[owned native runtimes](task-runtime.md). It does not change legacy unlinked tasks.
An execution outcome (`complete`, `failed`, `decision`), delivered notification,
ACK, and independent acceptance conclusion are separate facts. Linked acceptance
can no longer become approved by applying ordinary `acceptance` strings and
`next_action: complete`.

## Reviewer role

Start a fresh acceptance activity on the selected candidate, in a separate
snapshot workspace. Give it the current requirement's original references,
confirmed changes, specifications, completion criteria, relevant evidence and
open gaps. Never copy the developer's conversation. The native packet includes
`acceptance_protocol`, `open_gaps` and a repair's `acceptance_origin`.

Derive checks from those references. A check names an existing criterion and an
exact original/change/specification reference; it cannot add a completion criterion.
The role determines what inspection is useful. The core never requires a specific
CI service, hosting platform, test tool or publication provider.

The native reviewer writes `.herdr-verdict.json` in its own checkout before its
turn ends. The reserved path must not exist at fresh creation or dispatch, so a
committed or inherited report cannot masquerade as the new review. Both native
collectors verify their existing session/model/directory and fixed candidate,
read a bounded regular report file, and register it under the queue lock before
signalling. A missing report remains execution completion only. Invalid reports
fail explicitly; they do not produce a passing verdict or cause another native
submission. Library adapters can use `progress::acceptance::submit` only after
independently verifying the actual producer; arbitrary identity JSON is not
transport authentication.

Example report (all four arrays are explicit, even when empty):

```json
{
  "conclusion": "failed", "scope": "requested document content",
  "checks": [{"id":"content", "criterion":"goal", "source":"user request reference",
    "status":"failed", "evidence":"current-task/check-output"}],
  "gaps": [{"id":"missing-section", "criterion":"goal", "source":"user request reference",
    "evidence":"current-task/check-output", "release_condition":"required section exists"}],
  "reuses": [], "resolutions": []
}
```

The four conclusions are:

| Conclusion | Required additional support | Coordinator continuation |
| --- | --- | --- |
| `passed` | Every required criterion checked; all checks passed; no new gaps; every previous open gap resolved with evidence | `delivery` preparation |
| `failed` | At least one grounded gap with evidence and a release condition | `repair` |
| `unable` | `blocker` and `restore_condition`; runtime trouble is not a product failure | `restore` |
| `ambiguous` | `question`, `unresolved_after_review`, and `agreements_checked` covering all original/change/specification references, confirmation and existing authorization sources | `clarify` |

Reuse entries contain `activity`, `check`, the original `evidence`, and
`applicability`. They must refer to a recorded prior verdict under the same
requirement revision. A changed candidate additionally requires
`equivalence_evidence`; this records the reviewer's justification, not a core
claim of semantic equivalence. A resolution contains `activity`, `gap`, `evidence`
and `explanation`, referencing an actual previous gap. The independent reviewer
is responsible for checking that release evidence satisfies that gap's condition.

The stored envelope adds the exact task/assignment/requirement revision/artifact/
round/session-epoch binding and adapter-qualified reviewer. Its digest identifies
an immutable report. Repeating the same submission is idempotent; changing it is
rejected. A new opinion requires a new activity/round, not editing a sealed report.

## Coordinator role

Receive and ACK the execution result through the existing queue. Inspect the
independent report, applicability, artifact and current authorization. Apply the
report by ID using the ordinary `queue task-apply` transaction:

```json
{
  "task":"review-1", "state_version":2, "event":"received-event-id",
  "verdict":"immutable-report-digest",
  "next_action":{"kind":"repair", "owner":"coordinator",
    "reason":"independent recorded gap", "condition":"recorded release condition is met"}
}
```

The coordinator cannot provide replacement `acceptance`, `findings` or
`resolve_findings` for this role, change the conclusion, or choose a contradictory
continuation. The artifact is verified again during application. The transaction
consumes the event, records the verdict/open gaps, and saves the parent
`continuation` intent together. Only a valid independent pass sets `approved`.
This is delivery preparation, not customer delivery or new permission. Existing
authorization remains unchanged; final delivery/cleanup belongs to #37.

If execution failed, was interrupted, or completed without a valid report, omit
`verdict`, supply `execution_problem`, and choose `restore`. This records an
execution recovery intent, not a formal product failure/pass. A registered report
from an unsuccessful execution is not promoted into the applied verdict history.

Use `queue task-lineage` with `kind: continue` to consume a saved repair or recovery
intent. Supply parent `task`/`state_version`, its `intent` ID, an existing exact
`authorization`/`action`, `resolution` evidence, and an `activity` specification
in the usual lineage shape (without its parent task/version/kind). Repair requires
a development activity; restore/clarify require an acceptance activity. The intent
and linked activity registration are saved atomically. After restart the identical
request returns the same activity, while a different request is rejected. This
registers work only: native creation/dispatch still use their own non-replayable
journals and notification readiness checks.

Apply a repair's output as usual, explicitly select the repaired candidate to
start a new round, then create a fresh independent acceptance activity. Its input
packet includes the unresolved gaps. Old passing reports cannot approve the
replacement artifact or requirement revision. A clarification first checks all
existing agreements, then records an explicitly confirmed requirement revision
and reselects the candidate before continuing. `revise` inherits authorization and
criteria when those fields are omitted; do not ask again for existing permission.
An explicit supplied authorization list still records an actual authorization
change, not permission inferred from a verdict.

## Lifetime and verification

Reports, open-gap records, intent state, native output files and evidence belong
to the running task environment. They remain available for recovery and closing
the task, then are destroyed with that environment; no acceptance archive is added.

`tests/acceptance.rs` exercises all four conclusions, immutable identity/basis,
execution-only recovery, atomic application, persisted continuation, repair,
release evidence and clarification. `tests/task_runtime.rs` exercises both native
collector protocols with explicitly synthetic servers. Its opt-in
`acceptance_live::installed_real_roles_fail_repair_and_pass` runs real OpenCode
1.19.3 development, independent failure, repair and a fresh independent pass,
including native file inspection/tool commits, actual queue delivery/ACK/application
and complete environment destruction. The coordinator/notification receiver is a
protocol test harness; this does not claim an autonomous model coordinator or
customer delivery. Configure the same `HERDR_LIVE_OPENCODE_BINARY`, `_MODEL` and
`_CONFIG` inputs described in task-runtime.md. The test disables internal task delegation only in its disposable configuration
copy, so each role performs its own tools. No native approval is automatically
answered by the test.
