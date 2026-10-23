# Task handoff

Store outside the checkout in a private path whose ancestors pass the existing
directory ownership/write-permission checks. A conventional `~/.local/state/`
path is suitable only if its ancestors pass; do not relax the checks or change
shared directory permissions to register it.
The coordinator owns `current.json`; workers return concise results and do not edit it.
Write via a temporary file and atomic rename. Keep the previous revision until the
successor acknowledges the new epoch. This is a coordination protocol, not an OS lock.

Example shape (replace all example values):

```json
{
  "version": 1,
  "task": "owner/repo#123",
  "epoch": 2,
  "goal": "Fix the issue and complete the authorized merge",
  "authorization": [{"action": "merge", "source": "user instruction and scope"}],
  "done_when": ["review passed", "applicable CI passed", "merged", "main observed"],
  "phase": "ci_wait",
  "repo": "owner/repo",
  "cwd": "/absolute/checkout",
  "branch": "fix/123",
  "base_sha": "full SHA",
  "head_sha": "full SHA",
  "writer": null,
  "agents": {"coordinator": "live name and session identity"},
  "processes": [{"service": "ci-watch-123-e2", "state_dir": "/absolute/state"}],
  "notifications": {"queue_dir": "/absolute/queue", "guard_dir": "/absolute/coordinator-guard", "receiver_session": "registered session UUID", "readiness_evidence": "/absolute/readiness.json"},
  "completed": ["targeted validation: evidence path and input identity"],
  "review": {"result": "PASS", "sha": "full SHA", "evidence": "/absolute/path"},
  "blockers": [],
  "pending": [{"owner": "publish", "action": "consume CI result", "wait_for": "configured runs complete", "wait_id": "ci-e2", "fallback_check_used": false, "next_check": "observer terminal event or deadline notification"}],
  "phase_prerequisites": {"validation": ["stable candidate", "writer released", "resources allocated"]},
  "next_action": "Consume CI result, recheck current head, merge within authorization",
  "preserve": ["existing untracked files", "failed-run evidence paths"],
  "updated_at": "UTC timestamp"
}
```

Target a handoff of at most roughly 1,000 words. Large evidence and historical
reasoning stay in referenced files. Include unresolved tradeoffs and exceptions;
do not copy the whole chat. The successor verifies live branch/SHA, dirty paths,
process ownership and applicable repository instructions before writing. If they
conflict, resolve that specific conflict rather than replaying the entire audit.

Carry notification readiness, delivery faults and each dependency's fallback-use
record into successor handoffs. Readiness evidence describes the last verified
binding, not a permanent health guarantee. A new session or replacement wait ID
does not erase an already-used fallback for the same pending operation; follow
[delivery-unavailable recovery](waiting.md#delivery-unavailable-recovery).

Before the context guard seals a draft, its registered worker may request
[Luna quality advice](handoff-advice.md). Only the supplied draft text is assessed;
referenced evidence is not inspected. Model suggestions are untrusted editorial
feedback, never execution instructions, authorization, or a replacement for the
successor's verification. Keep suggestions out of the sealed handoff unless the
worker has reviewed and incorporated them into the actual task facts.

Use messages such as `task=owner/repo#123 epoch=2 phase=review result=FAIL
blocker=<specific issue> evidence=<path> next=<requested action>`. A reviewer can
report a critical stop recommendation; only the coordinator changes the recorded
write owner and communicates the stop. A detected immediate unsafe action should
still be interrupted without waiting for paperwork.

## Bind a handoff to an existing queue

When this workflow already has a verified receiver and queue, carry a small
reference in its handoff, result and acknowledgment records. Resolve these values
from that queue; this example is a handoff reference, not a new queue schema:

```json
{
  "queue_ref": {
    "dir": "/absolute/queue", "id": "REPLACE_QUEUE_ID",
    "task": "owner/repo#123", "epoch": 2,
    "wait": "REPLACE_WAIT", "assignment": "REPLACE_ASSIGNMENT",
    "candidate": "REPLACE_FULL_SHA", "operation": null,
    "producer": {"adapter": "codex", "session": "REPLACE_EXACT_SESSION"}
  }
}
```

Use the saved operation ID for a successor; leave it null for an initial task.
Add the actual event/delivery IDs when available. Re-read task state_version at
each mutation rather than treating a version copied into a handoff as current.
Queue/task/wait/assignment/candidate/producer disagreement stops that transition;
do not silently bind another queue or infer identity from prose, pane focus or
the latest file. These references summarize existing state and confer no batch
intake acceptance. Only the existing successful `batch-offer` receipt does that.

The receiver coordinator owns task-add, task-apply and task-begin. The designated
producer owns exact task-accept and signal. In the verified mixed path, OpenCode
uses its checked `OPENCODE_SESSION_ID` for receiver commands and Codex uses its
own `CODEX_THREAD_ID` for takeover/results. Neither borrows the other's identity.
Initial registration is distinct from successor takeover; see
[the command ownership table](herdr-workflow.md#已登记队列的命令归属).

For queue-managed results, signal the registered wait once with real evidence;
the queue notifies its receiver, which ACKs and applies the result with a concrete
next action. A manual HANDOFF_RESULT/HANDOFF_ACK is not that queue ACK or task
application, and HANDOFF_ACCEPTED is not a lane receipt. Preserve the existing
manual channel for assignments and return notifications to an agent that is not
the queue receiver; do not duplicate an uncertain queue delivery through it.

An applied result with a prepared successor is still unfinished work. Under
existing authorization, the coordinator must begin that same operation and send
the assignment; the producer accepts it before working. Do not add a new user
confirmation merely because a stage ended. Explicit user pause, a concrete
blocker or missing authorization must instead be recorded and reported. Every
pending handoff names the next owner, trigger, deadline and escalation owner.
Use existing waits/status/readiness for diagnosis; these fields do not install
an external timer, and a stopped watcher cannot report its own death.

Keep independent development in separate worktrees while one publication batch
owns CI/repair/merge. Queue integration does not impose a global one-task limit,
enable V2 or prove bounded pending intake. Record inspected writer-release evidence
before transferring a checkout. A local slice, ACK, successful PR or metadata
receipt cannot complete the parent issue while its original acceptance is unmet.
