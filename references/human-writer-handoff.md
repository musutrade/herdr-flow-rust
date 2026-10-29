# Human checkout writer handoff

This slice adds explicit writer transfer for the registered development checkout
of an active, isolated V2 requirement. It continues the same queue/task/epoch/
instance and the existing Recovery Agent. Viewing a Herdr tab, answering an
approval, waiting for a timeout or receiving a model verdict never grants a human
the checkout writer.

## Operator sequence

`--state-dir` is the original
coordinator runtime directory; `--task` is the parent requirement, not an activity.
The task must already have its configured Recovery Agent. Store specs outside
the checkout so returning it does not itself change the returned fingerprint.

```sh
herdr-flow task-runtime human-status --task TASK --state-dir COORDINATOR
herdr-flow task-runtime human-request --task TASK --state-dir COORDINATOR --spec request.json
herdr-flow task-runtime human-acquire --task TASK --state-dir COORDINATOR --spec identity.json
# Edit only after the response explicitly reports phase: human.
herdr-flow task-runtime human-return --task TASK --state-dir COORDINATOR --spec return.json
```

`request.json` contains `{"id":"manual-1","actor":"operator","reason":"why intervention is needed"}`.
`identity.json` contains the same `id` and `actor`. `return.json` adds `note`, a
description of actual changes and remaining concerns. IDs cannot be reused for
another handoff. `actor` records the cooperating operator; it is not authentication
or a separate operating system identity. Unknown fields, including `force`, are
rejected. Repeating an already completed transition with the same identity is a
read of its committed result, not another execution.

| Phase | Checkout writer | Next action |
| --- | --- | --- |
| `draining` | Original execution may finish; new writes are fenced | Collect/apply original results, settle approvals and commands, then retry acquisition |
| `human` | Named human | Finish edits and stop child commands, then explicitly return |
| `verifying` | No new writer | Existing Recovery Agent inspects the explicit return |
| `blocked` | No new writer | Inspect the retained model/host finding; explicitly reacquire, repair and return again |
| `released` | Automated admission resumes | Original coordinator registers the current candidate and obtains fresh independent acceptance |
| `abandoned` | No new writer; human authority surrendered | Finalize as failure, then run the existing terminal cleanup |

A successful CLI exit does not imply acquisition: inspect `phase` and
`blocked_reason`. Acquisition that cannot establish quiescence retains the request
and fence. Unknown sends are inspected through their original records, never
replayed by these commands. There is no force grant or automatic deadline.

If verification cannot converge or the Recovery Agent is unavailable, explicitly
abandon the handoff using the same ID and actor:

```sh
herdr-flow task-runtime human-abandon --task TASK --state-dir COORDINATOR --spec abandon.json
```

`abandon.json` contains `{"id":"manual-1","actor":"operator","reason":"why the task cannot continue","acknowledge_cleanup":true}`.
The acknowledgement surrenders human writes and accepts subsequent failure
cleanup of the checkout; stop external editors/commands first. Abandon works from
`draining`, `human`, `verifying` or `blocked` without a usable recovery session or
successful native observation. It records the previous phase, evidence digest and
checkout fingerprint (or the observation error), invalidates current acceptance
and advances its round exactly once. It never restores automated writer authority
or deletes resources. Only failure finalization becomes available; the fence
remains until [terminal cleanup](task-lifecycle.md) releases the environment. A stale recovery result
cannot release an abandoned handoff. A new handoff cannot reopen it.

## Checks and continuation

The protected queue holds the authority record. A lifecycle exclusive permit
serializes request/acquisition/return with managed admissions. After it is dropped,
the durable fence remains at writer reservation, runtime creation/dispatch,
broker and native TUI frames, publication admission and delivery execution.
Metadata, original approval responses and interrupts remain available. The
existing Recovery Agent may run an explicit read-only verification turn with
network access disabled and approval escalation disabled.

Explicit commands retry lifecycle and queue lock acquisition every 20 ms for up
to two seconds per acquisition. A timeout reports contention without committing
the transition; retry the same command. Transitions and native operations are
never replayed as part of lock retry.

Missing runtime documents, malformed task maps and invalid handoff records fence
new writes while metadata, original approval replies and interrupts remain usable.
Existing legacy runtime documents keep their legacy behavior; an isolated runtime
cannot omit its Owner. Native admission caches derived fences for up to 16 queues,
keyed by the protected payload's inode, size, nanosecond mtime/ctime and access
metadata. Replacement or observation failure cannot reuse a stale cached permit.

Acquisition checks the checkout directory identity, original logical writer and
dispatch/publication journals, Git locks, native requests/turns, background terminal
inventory, overlapping loaded sessions and task process observations. A native
`idle` result alone is insufficient. Unsupported or unavailable background-terminal
observation blocks acquisition. An unregistered overlapping loaded native session,
including a native child session, also blocks rather than inheriting ownership.
This first slice does not close or resume those sessions to make a check pass.

The registered task processes and processes born within the task boundary are
checked. This is a protocol among cooperating managed writers, not an OS lock
against arbitrary same-user programs, pre-existing editors or direct connections
to the shared server. The operator must stop external editing/commands before
return and must not write after return. All queue writers must use compatible
code, as described in [writer admission](claim-admission.md#deployment-writer-selection-checklist).
The command's own PID is excluded, so acquire/return can run inside the checkout.
Parent shells and other processes are still observed under the same birth-boundary
rules; they are not automatically exempted by the command's ancestry.

Return records the actual checkout fingerprint and a fresh return ID. The original
Recovery Agent receives one incident for that ID. A recovered verdict is combined
with another host quiescence/fingerprint check. A failed host check remains blocked
even if the model reported success; both conclusions are retained. Failed
verification is visible in the original coordinator tab and durable status without
starting a new coordinator turn. Successful verification releases admission and
uses the existing result handoff to that coordinator.

While a handoff is pending, the idle recovery scanner prioritizes explicit human
returns and suppresses unrelated receiver/queue watcher health, broker liveness
and turn fault scans. It does not clear the saved `seen`
episodes without a healthy observation. An already active incident still advances;
no new unrelated recovery incident or repair is started during the handoff.
Inspect the original watcher/broker status directly during a long intervention.
Normal scans resume after release; terminal failure ends observation after abandon.

Release clears the current candidate, delivery target and approval and advances
the acceptance round. Original requirements, immutable verdicts, operations,
publication observations and delivery history remain. The current model result
and effective verdict remain complete for idempotent result delivery. Verification
history retains return/incident/turn IDs, dispositions, bounded UTF-8 excerpts and
SHA-256 digests, using at most 512 KiB and the remaining space in the 1 MiB record.
Oldest summaries are pruned by serialized byte size with a count and chained digest
receipt; this history is not an unlimited archive of full model output. Existing
oversized history is compacted by the next transition. `human-status` instead emits
a compact diagnostic view with a warning and the original record digest without
modifying authority. Completed handoff archives retain their separate 64-entry/
1 MiB bound; reaching that bound refuses a new request while admission is released.
Reacquiring a blocked handoff clears the active return and verification, retains
historical summaries and gives the next return a fresh ID. Terminal cleanup of an
active human handoff requires release or explicit abandonment and failure.

## Validation scope

Local protocol tests exercise actual isolated ownership, Git worktrees, RPC
observation and live process competition, including unknown background observation,
late edits, stale verification, repeated result application and historical
approval invalidation. Synthetic protocol peers are not real model acceptance.
This change does not claim a new live model/Herdr acceptance or support for manual
remote publication, existing ambiguous side effects, arbitrary external editors,
automatic repair of shared services, or heterogeneous providers.

The first full local run exposed a pre-existing broker fixture lock race (the
fixture unwrapped a nonblocking lock result). The fixture now uses its existing
bounded wait before installing the checkpoint; that failed run is not counted as
a pass.

Review-fix coverage also exposed a fixture startup timeout: its RPC accept timer
started before acquisition of the shared test-environment lock. Human fixtures now
initialize that environment before starting RPC deadlines. Their bounded accept
budget also covers the large-history compaction exercise; production RPC and peer
read/write limits are unchanged. The failed coverage attempt is retained in the
validation record and is not counted as a pass.

The separately opt-in [real human writer acceptance harness](human-writer-live-acceptance.md)
prepares one full model/Herdr success loop with a disclosed scripted operator.
It remains a prepared harness until that exact live run passes; ordinary CI and
protocol fixtures do not establish native/model notification readiness.
