# No-model role observations (#112, 2026-10-26)

Baseline: main `765b71eaa67770be817d9fcc287a173a8bfcd73c` (PR #137/#138).
This slice extends `queue readiness`; it does not install binaries, switch
writers, activate receivers, send prompts, create threads or accept a real task.

## Invocation and evidence boundary

Run the absolute candidate binary **inside the actual role's existing tool
execution**, with its existing cwd, environment, sandbox and namespaces:

```text
/absolute/candidate/herdr-flow queue readiness --state-dir /absolute/queue --spec /absolute/role-spec.json
```

The command does not launch a shell, recreate a sandbox, enter another process's
namespace or try an alternate transport. It emits its observation to stdout;
the role-spec mode does not overwrite the existing receiver `readiness.json`.
It records PID/start ticks, cwd, namespace IDs and selected process policy fields.
These fields describe this invocation, not an attestation of another process's
seccomp filters, Landlock policy or model tool permissions. Calling from a parent
controller, copying a session environment variable or matching namespace IDs
cannot establish role execution equivalence. Identity environment variables
reuse the existing adapter-qualified identity check; they are not authentication
between same-UID processes.

Current adapters have no supported no-model operation that independently
reproduces/attests a future model tool invocation or queries repository trust.
Those fields remain `unknown`. A known local failure makes overall `status`
`blocked`; successful local checks still produce overall `status=unknown`,
`ready=false`, `active_wakeup=false` and exit 1. There is deliberately no spec
boolean or copied receipt that can promote this to complete readiness. A real
role/tool observation and its externally verified provenance must be reviewed
together. If such an invocation cannot be obtained without a model prompt, this
slice cannot certify that role. No prompt is authorized by this command.

Without `--spec`, existing receiver/watcher readiness behavior and exit codes
remain available. Its new `role_context=unknown` field makes the limit explicit:
legacy `active_wakeup` proves the existing receiver checks, not the P1 role
sandbox requirement. Neither mode establishes ACK, application or business success.

## Selected paths and effective configuration

Common role-spec fields, using placeholders rather than credentials:

```json
{
  "role": "receiver",
  "cwd": "/absolute/existing/role-checkout",
  "executable": "/absolute/candidate/herdr-flow",
  "sha256": "EXPECTED_SHA256_OF_CANDIDATE",
  "identity": {"adapter": "opencode", "session": "EXACT_EXISTING_SESSION"},
  "endpoint": "http://127.0.0.1:EXISTING_PORT",
  "model": {"providerID": "EXISTING_PROVIDER", "id": "EXISTING_MODEL"}
}
```

The executable selection must be absolute. Linux hashes `/proc/self/exe`, the
executing inode, rather than trusting a newly replaced file at the same pathname.
Path and digest mismatch are `executable_conflict`. The report separately shows
the first executable PATH candidate; a shadowed PATH entry does not block an
already verified absolute selection. Shell aliases cannot be recovered after
exec, remain unknown, and must be inspected at the real shell/launcher entry.
Non-Linux executing-inode verification is unsupported and returns unknown.

| Selected role | Required observation | Deliberately unproven |
| --- | --- | --- |
| OpenCode receiver | Existing queue/guard scope, exact environment session, cwd, explicit endpoint and model; existing `SessionAdapter::verify_binding` reads health/version and exact session/directory/model via bounded `HttpApi` | POST capability, model authentication, pending approvals, repository trust, ownership of the server's running executable and equivalence to a future tool sandbox |
| Codex/OpenCode signal producer | Set `role=producer`, `identity` to the wait's producer, and `wait` to the existing open signal wait. Reuse identity and lifecycle wait permits; create, sync and remove one `.readiness-*` temporary file in the existing results namespace | Actual signal admission, task completion, receive/ACK/apply; no receiver HTTP/control socket access is attempted |
| Other receiver, including task-owned Codex bootstrap | `unsupported_channel` | No receiver activation or fallback is attempted |

The probe does not create a missing result directory. No real result, wait,
dispatch marker, submission or receipt is published. A crash during the temporary
write probe can leave its distinct scratch file; this is not a signal or receipt.
Temporary writes are scoped to an existing queue's results directory and retain
the existing isolated lifecycle permit. Queue and guard ownership checks are
reused; the result is a snapshot, never a durable permission for later execution.

The endpoint and model are explicit spec values checked against the registered
guard; session reads also verify the server's effective identity. No default
configuration file is hashed, loaded or compared for these selected operations.
Changing an unrelated default file cannot invalidate a proved explicit override.
This is not a general-purpose semantic comparison of Codex/OpenCode config files;
configuration used by unprobed tools remains unknown. The running adapter binary
and its effective launch argv must still be inventoried independently.

## Errors and credentials

| Evidence | Diagnostic |
| --- | --- |
| Running binary path/digest differs | `executable_conflict` |
| Missing required local input/path | `missing_configuration` |
| Different cwd, session/model/scope or explicit endpoint | `cwd_mismatch`, `identity_mismatch`, `endpoint_mismatch` |
| OpenCode HTTP 401 | `authentication_failed` |
| HTTP 403 or typed socket/filesystem permission denial | `policy_denied` (does not identify which policy layer denied it) |
| Typed timeout | `timeout` |
| Other typed HTTP transport failure | `transport_failure` |
| Invalid/unrecognized protocol or insufficient evidence | `unknown` |
| Repository trust confirmation state | `unknown`: no supported no-model query; never infer trust pending from transport/auth failures |

The existing HTTP transport now retains typed authentication, denial and timeout
categories. Neither credentials, response bodies nor arbitrary transport error
strings are emitted. Successful HTTP observations report only the authentication
source (`OPENCODE_SERVER_PASSWORD` or `none`); an accepted GET does not establish
model-provider authentication. HTTP remains explicit loopback with its existing
direct transport policy. No global proxy/login/trust/security configuration is
read for modification or changed. Known request budgets and GET-only interrupted
observation retries are reused; no submission retries are added.

## Writer inventory and deployment prerequisites

Read-only host sample on 2026-10-26; these are observations, not installation
claims for this PR:

| Entry/process | Observed identity |
| --- | --- |
| PATH `/home/gem/.local/bin/herdr-flow` | SHA-256 `5c184bd5a069a7a21fed37deeb3e6804b9eef85d911ddd8624afbdb8d086d43e` |
| Queue watcher PID `2502762`, start ticks `114690164` | Executing inode `/mnt/dev-ssd/herdr-flow-releases/e056b63c5f57d405f7e90d9c1f036260dee805d4/herdr-flow`; `/proc/PID/exe` SHA-256 `701985740d88e4fb5c6f6bd1379af40bc949756ad6d2f0875c668f03a14adea7` |
| That watcher's selected queue | `/mnt/dev-ssd/herdr-flow-state/issue112-actions-acceptance/queue`; queue ID `4e9240b82fe642499a27700707e82535`, queue version 1, no isolated lifecycle format |

The release directory name is a source label, not proof of reproducible build
provenance. The watcher and PATH entry differ, and neither is this PR candidate.
The sample found one running flow writer; `/proc` snapshots miss short-lived,
unreadable, differently wrapped and future writers. Existing skill entry files
also reference the bare `herdr-flow` command. No matching service/timer/script
was found in the sampled user systemd/local-bin paths; this is not exhaustive
startup coverage. None of these processes/files/services was changed or stopped.

Before any separately authorized deployment:

1. Inventory **every** writer for each target queue: watcher/observer, coordinator
   task/PR/batch/ACK CLI, producer signal/result CLI, wrappers, skills, shell
   aliases/functions/hashed commands, absolute paths, timers/services and queued
   future launch entries. Record queue ID, role, canonical binary path, executing
   SHA, build/source provenance, effective safe argv/config sources, cwd and
   relevant namespaces. For residents re-read PID/start ticks before and after
   hashing `/proc/PID/exe`; a replaced disk inode does not switch that process.
   Credential values and unrestricted argv/environment dumps are excluded.
2. Agree a deployment window and all-writer quiescence/switch/resume sequence.
   Pin compatible short-lived and future entry points too. Existing #130 barriers
   protect their supported initialized isolated queues and task/epoch/instance
   owners; they cannot make an old executable understand new semantics, and do
   not retrofit the sampled legacy queue. Do not migrate, adopt old ownership or
   restore snapshots as part of this readiness slice.
3. Preserve and compare current task/claim, receipt/claimed_by, wait/event/delivery,
   ACK/application and uncertain operations across the authorized change. Never
   reissue uncertain dispatch or restore a pre-consumption snapshot as rollback.
4. Re-inventory running inodes and future entry points, verify exact-candidate CI,
   then observe each required path from its real role/tool context. If that
   context cannot be obtained without extra permissions, pairing credentials or
   a new unsupported channel, report the boundary and seek a minimal supported
   integration plan. Do not relax to full access or start a substitute service.

No production writer switch, deployment, service stop, real model task or item-4
acceptance was performed. #112/#38/#32/#130 retain their outstanding acceptance
requirements and remain open.

## Regression evidence

`tests/role_readiness.rs` uses private disposable queue/guard/cwd fixtures and a
loopback GET-only server. It covers normal connection, exact identity/endpoint
and binary mismatch, missing input, timeout, unknown protocol, HTTP auth/denial,
PATH shadowing with explicit selection, irrelevant defaults, and producer file
access without receiver access. The Linux child-only seccomp negative denies
`connect` after a successful parent-context observation, then checks the parent
again; no privileges are broadened and no model prompt/thread is created.

First targeted run: 5 passed, 1 failed because the producer fixture omitted the
existing results directory. The probe correctly reported unknown; the fixture
was corrected to create its own directory. This failure is retained here rather
than replacing it with the later successful run. CI and remaining gate results
are recorded on the PR for its exact head.
