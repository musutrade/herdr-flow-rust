# Human checkout writer handoff

This slice adds explicit writer transfer for the registered development checkout
of an active, isolated V2 requirement. It continues the same queue/task/epoch/
instance and the existing Recovery Agent. Viewing a Herdr tab, answering an
approval, waiting for a timeout or receiving a model verdict never grants a human
the checkout writer.

## Operator sequence

Run the commands from outside the task checkout. `--state-dir` is the original
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

A successful CLI exit does not imply acquisition: inspect `phase` and
`blocked_reason`. Acquisition that cannot establish quiescence retains the request
and fence. Unknown sends are inspected through their original records, never
replayed by these commands. There is no force grant or automatic deadline.

## Checks and continuation

The protected queue holds the authority record. A lifecycle exclusive permit
serializes request/acquisition/return with managed admissions. After it is dropped,
the durable fence remains at writer reservation, runtime creation/dispatch,
broker and native TUI frames, publication admission and delivery execution.
Metadata, original approval responses and interrupts remain available. The
existing Recovery Agent may run an explicit read-only verification turn with
network access disabled and approval escalation disabled.

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

Return records the actual checkout fingerprint and a fresh return ID. The original
Recovery Agent receives one incident for that ID. A recovered verdict is combined
with another host quiescence/fingerprint check. A failed host check remains blocked
even if the model reported success; both conclusions are retained. Failed
verification is visible in the original coordinator tab and durable status without
starting a new coordinator turn. Successful verification releases admission and
uses the existing result handoff to that coordinator.

Release clears the current candidate, delivery target and approval and advances
the acceptance round. Original requirements, immutable verdicts, operations,
publication observations and delivery history remain. Handoff history and
verification attempts are bounded and retained; reaching a bound fails closed.
Terminal cleanup is refused while the human owns the checkout or a return remains
blocked/under verification. Resources are not deleted by a handoff command.

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
