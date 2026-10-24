# Trusted gate consumer adapter

The explicit [GitHub Actions lane policy](actions-batching.md) reuses the bounded
lane with PR-head and merge-main workflow/job checks. The trusted Gate/App
requirements below remain specific to Gate mode; modes are never mixed or
automatically migrated.

This is a stdio interface for an operator-managed consumer client, not a new
GateHouse evidence format or a claim about #98's native API. Its deployment can
provide a thin wrapper over existing machine results/invocations and trusted
current state. No service or credential code is implemented here. Protect the
wrapper/configuration/authoritative store from candidate and agent writes;
ordinary developer JSON files do not implement this trust boundary.

The pinned `gate.verifier` argv receives one JSON object on stdin, without a
shell, from `/`, with a 45-second timeout. Return one JSON object on stdout, at
most 64 KiB. Nonzero exit, malformed/oversized output or missing fields fail
closed. Stderr is not retained because it may contain credential diagnostics.

## Verify an existing operation

```json
{
  "version": 1, "action": "verify", "operation": "UNIQUE_OPERATION",
  "reference": "EXISTING_GATE_RUN",
  "binding": {
    "repo": "owner/repo", "host": "github.com", "base_ref": "main",
    "base": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
    "head": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
    "tree": "cccccccccccccccccccccccccccccccccccccccc",
    "policy": "DEPLOYED_POLICY_ID"
  }
}
```

Independently query the authoritative service and check operation/reference,
inputs/policy, coverage, Core decision, current validity/revocation and expiry.
Do not echo the request as an attestation. The response needs:

- `version: 1`, exact `operation` and complete exact `binding`;
- `valid: true`, `coverage_complete: true`, `decision: "allow"` or `"deny"`;
- actual integer `app_id`, exact `check_name`, `run`, `external_id`, `evidence`;
- `expires_at`: the service's finite Unix expiry timestamp.

Only allow can prepare publication. Incomplete runs cannot become complete deny
or success. Unknown/invalid/running results can return nonzero or `valid: false`.
Repeated verification is read-only: it starts no execution, push, PR or Check,
and does not extend validity. Stable receipt fields let publication compare the
saved result with current trusted state. The trusted publisher/App adapter owns
its final remote-head/base/rules checks and exclusive Check credentials.
Gate execution uses its separately journaled operation over the deployment's
existing authorized transport.

## Inspect uncertain execution

Before rebuilding after a target change, send
`{version: 1, action: "inspect", operation, binding}` without a result reference.
Independently look up the exact operation and input. Return exact version/
operation/binding, `quiescent: true`, `evidence`, and `status` in `completed`,
`failed`, `cancelled`, `never_started` only when no active or unaccounted execution
remains. Unknown/running operations must not claim quiescence. This response
grants no quality approval. Rebuilding uses a new operation; uncertain execution
is never replayed automatically.

The tests' stdio script is a fixture, not a trusted GateHouse implementation.
Production adapter compatibility and live deployment belong to the consuming
operator. [GateHouse #98](https://github.com/musutrade/GateHouse/issues/98) defines
the upstream quality-result boundary; its completion does not attest this
consumer's verifier, App, publisher or isolation.

### Captured process output budget

The verifier's stdout and stderr each have a 65,536-byte budget enforced during
collection. Exceeding either budget stops the owned process group and fails
verification; partial JSON is never parsed as a receipt. The existing operation
must be preserved and uncertain publication queried before any retry. Diagnostics
identify the stream and budget without including captured verifier content.

The shared `CommandOptions` capture boundary defaults to 16 MiB per stream
(32 MiB combined payload), plus fixed read buffers and allocation overhead. Git
snapshot/diff/object operations, Herdr CLI responses and GitHub API responses use
this default; their data-dependent output has no intrinsic small maximum, so a
larger response now fails explicitly instead of growing memory without a bound.
A caller with a documented larger legitimate payload can set finite
`stdout_limit` and `stderr_limit` values. Zero permits only an empty stream;
exactly the configured number of bytes is accepted. Neither stream can consume
the other's budget. `run_command` reports `OutputLimitExceeded`; domain adapters
may wrap it in their existing failure category. This is not an automatic retry
signal. Timeout, fair pipe servicing and owned-group cleanup remain in effect.
This budget covers captured commands, not detached command logs or native RPC
streams, which have separate lifecycle/protocol contracts.

## Verifier identity review (#45; initial baseline #51)

The current deployment contract makes the operator-managed verifier, its
configuration, dependencies and authoritative service trust roots. An absolute
argv and a working directory of `/` alone do not enforce this contract.

| Principal | Required boundary | Baseline evidence |
| --- | --- | --- |
| Operator controlling verifier installation | Trusted to install and upgrade code and configuration | Deployment responsibility; no installer implemented here |
| Candidate task with a separate UID | Cannot write verifier, configuration, dependencies or ancestor directories | Filesystem permissions must be checked; baseline gate validation checked argv shape only |
| Candidate task sharing the operator UID | Must be constrained by an external sandbox or equivalent boundary | File ownership alone cannot isolate mutually hostile processes sharing this UID |
| Receipt storage writer | Stored results cannot independently authorize publication | Receipt fields are checked against operation, binding, App, check name and expiry; publication queries the verifier again and requires exact equality with the stored receipt |

The baseline inherited the caller environment through default `CommandOptions`.
An isolated regression demonstrated that `BASH_ENV` could replace a trusted Bash
verifier response. This establishes an invocation-context defect, not a claim
that a deployed service was compromised. The clean environment below addresses
this input path.

The review scope includes file and ancestor replacement, symlinks, the
validation-to-execution race, inherited environment, legitimate upgrades and
receipt mutation/replay. Checking a path
once and subsequently executing that path is insufficient to bind identity.
Acceptance must use the implemented checks and evidence below; these baseline
notes alone do not establish a deployed trust boundary.

### Confirmed consumer checks

`batch::validation::publication_receipt` invokes `gate::query` again before
publication. `publication_receipt_ok` requires both `decision == allow` and exact
JSON equality with the saved receipt. `gate::receipt_ok` checks version,
operation, full input binding, valid/coverage flags, App ID, check name, decision
and finite future expiry. Thus changing only a saved receipt does not by itself
provide a matching new trusted response. A compromised verifier/configuration
can still defeat those checks; authenticating a response with a key available to
the same attacker would not repair that boundary.

The existing member-checkout rejection resolves verifier argv paths relative to
the candidate checkout. It does not establish filesystem ownership, dependency
integrity or execution-time file identity. Those checks remain separate from
receipt validation and must preserve deliberate operator upgrades.

### Executable descriptor enforcement

Each invocation now checks the configured executable path and its canonical
path. Entries must be owned by root or the invoking user; ordinary files and
directories must not be group/other writable. Trusted-owner sticky directories
(such as `/tmp`) may contain private installations. Symlinks are allowed only
through these owner/path checks, and the resolved target must be a regular
executable with no group/other write permission.

The checked executable is opened without following a final symlink, checked
again through its descriptor, and executed through `/proc/self/fd`. Replacing the
pathname after this open cannot select a different executable for that call.
An operator upgrade must use atomic replacement: an existing call retains the
old file and the next call validates and opens the new file. In-place mutation
by the trusted owner is outside this guarantee. Scripts receive a descriptor
pathname, so wrappers must use absolute installation paths for dependencies
rather than locating adjacent files from `$0` or `__file__`.

This first enforcement step does not authenticate arbitrary interpreter
arguments, imported modules or dynamic libraries.
Those remain operator-controlled trust inputs and require the deployment review
above. It does not isolate hostile processes sharing the invoking UID and does
not by itself complete #45. The rejection regression demonstrates that a
world-writable executable was run on the baseline and is rejected before code
execution with this enforcement; descriptor tests cover scripts, trusted
symlinks and atomic upgrades.

### Operation identity validation

Both verify and inspect require a nonempty operation string of at most 500
characters before invoking the verifier. A missing, null, blank or non-string
operation is rejected; echoing a null operation is not identity binding. The
isolated receipt fixture reproduced acceptance of a null/null operation on the
baseline. Regression cases also cover changed input-binding fields, wrong
operation/App/check identity, expiry (including the exact boundary), invalid
coverage/validity flags and replay against a new operation. They do not claim
access to or compromise of a deployed authoritative service.

### Verifier execution environment

Verifier subprocesses start with a cleared environment containing only
`PATH=/usr/bin:/bin` and `LANG=C`, and run from `/`. Caller startup scripts,
loader variables, interpreter search paths, HOME configuration and service tokens
are not inherited. This applies to every verifier action, including inspect.
System executables and their dependencies remain operator-managed trust roots.

Deployments that previously relied on inherited credentials, proxy settings or
user configuration must move their configuration into a protected verifier
installation or credential service, accessed by explicit paths. A protected
wrapper can supply its own deliberate settings. Do not place secrets in queue
JSON or verifier arguments. There is no caller-environment passthrough option.
The client fails closed if required deployment configuration is unavailable.
This environment boundary does not authenticate scripts passed as interpreter
arguments, imported dependencies or their installation configuration, and does
not provide isolation from a hostile process with the same UID.

### Pinned shebang interpreter chains

Direct script clients now have each shebang interpreter opened and checked with
same owner, mode and ancestor rules as the initial executable. Every script and
the final interpreter remain open until spawn. Invocation passes descriptor
paths for the scripts and executes the final interpreter descriptor, so atomic
replacement of any checked script or interpreter cannot redirect that call.
Fresh calls see operator upgrades. The parent retains CLOEXEC on every file;
only the child keeps the descriptors it needs across exec.

At most four script layers are accepted. Each shebang must have an absolute
interpreter and a UTF-8, NUL-free line ending within the first 256 bytes. The
optional shebang argument remains one argument, including embedded spaces.
Relative, missing, unsafe, malformed and recursive/excessive chains fail closed.
These deliberately narrow script-format requirements may reject scripts that
some kernels accept. Use a conventional short absolute shebang when installing
clients. Script `$0` and interpreter script arguments remain descriptor paths.

This check follows kernel shebang edges; it does not infer dependencies from
arbitrary argv or program behavior. For example, `/bin/sh /path/client.sh`,
`sh -c CODE`, an env dispatcher, a Python import or an ELF loader can execute
additional code. Those inputs, configuration and installed dependencies must be
protected by the operator deployment boundary. An isolated probe confirmed that
a mode-0666 script explicitly passed to `/bin/sh` is not covered by executable
identity checks. Prefer a directly configured protected script for descriptor
pinning. This probe's private temporary ancestor does not demonstrate access by
a different UID. Deployment requirements are detailed below.

### Configuration ownership and deployment acceptance

The gate argv/policy/App/check configuration enters through operator-supplied
`batch-init` input (`lane::init_gate`) and is persisted in the lane in queue
state. The CLI spec file is ordinary input, not an authenticated configuration
format: the operator must control its source and contents before invoking the
command. Do not initialize a lane from candidate-supplied policy or verifier
configuration. There is no separate installer or trusted-account provisioning
service in this repository.

#### Consumer deployment evidence

Before enabling a real lane, the consuming operator records the following
bindings for that repository. Keep machine paths, runtime identities, raw logs
and credential material outside the checkout; reference evidence, not secrets.
This inventory documents the existing contract and adds no installer, service,
receipt protocol or alternative quality decision.

| Boundary | Required evidence | Insufficient evidence |
| --- | --- | --- |
| Verifier installation | Exact argv, executable/dependency identities, protected configuration and compatibility with this consumer interface | An installed `harness-gate` binary or a stdio test fixture |
| Quality result | Authoritative operation/run, repo/base/head/tree/policy binding, complete coverage and current validity/expiry; uncertain execution has independently checked quiescence | Agent PASS, command exit alone, echoed request or an unrelated historical result |
| App and reporting | Actual App ID, check name, run/external ID, required-check configuration and restricted publisher/credential ownership | Placeholder IDs, ordinary Actions success or upstream issue closure |
| Isolation | Candidate write restrictions protecting verifier/configuration/dependencies, queue state and authoritative result source; state the same-UID limitations below | Absolute paths, hashes or same-user directory ownership alone |
| Live consumption | V1 receiver/result/ACK/application evidence and real normal, repair and interrupted reporting/publication paths with the exact bindings | Native backend or offline fixture results described as Herdr deployment acceptance |

GateHouse evaluates quality and supplies results. The consumer verifies their
applicability and owns Check reporting, publication rules and batch continuation;
it must not change the upstream quality decision. Reporting recovery uses the
existing result and consumer adapter, without automatically re-executing the gate.
Missing bindings block the affected lane activation; they do not require
GateHouse to implement PRs, queues or agent scheduling. See
[V2 acceptance](v2-acceptance.md) for the remaining production prerequisites.

Binary rollout and lane activation are separate transitions. Before introducing
[schema 3 claims](claim-admission.md#compatibility-and-deployment), inventory all
queue/guard/result writers, select one compatible binary and account for pending
operations under the existing locks. A build or merged PR does not switch live
watchers or old commands saved in agent handoffs. Once claims are persisted, old
writers cannot return; do not restore stale queue snapshots or replay uncertain
dispatch. A binary-only rollout supplies neither a live lane nor the deployment
evidence above. Each transition needs its existing authorization and acceptance.

Batch mutations use `queue::task_action_loaded` → `queue::load_queue` →
`read_state_json`. That path opens directories by descriptor without following
symlinks, checks ancestor ownership and write modes, and requires an owned regular
queue file without group/other write permission, opened with O_NOFOLLOW. New
state directories/files use 0700/0600. This protects the persisted configuration
from a different UID under those filesystem assumptions. The configuration
regressions exercise the actual batch action entry with writable files and a
redirected queue symlink, preserving original bytes on refusal. Deliberate
replacement by the trusted owner remains allowed.

| Input or principal | Enforced here | Required deployment protection |
| --- | --- | --- |
| Operator, root, invoking account | Owner/mode validation and atomic upgrade semantics | Trusted to choose/install code, argv, configuration and service roots |
| Different UID candidate | Unsafe installation paths and writable/redirected queue state rejected | No privilege to change trusted owner, mounts or filesystem policy |
| Same UID candidate/task subprocess | No independent principal separation | External sandbox/account boundary must prevent changes to operator state and installations; unrestricted shared-UID tasks are not a supported hostile-task deployment |
| Direct client script and shebang chain | Bounded parsing; every layer checked and pinned through exec | No in-place edits by trusted owner during calls; use atomic upgrades |
| Interpreter argv, dispatchers, ELF loaders, imports | Clean initial environment and fixed system PATH only | Protect all selected scripts, libraries, runtime configurations and dispatch targets; these are trusted installed code, not data validated by argv heuristics |
| Stored receipt or candidate-provided JSON | Fresh verifier query, full binding checks, expiry and exact saved/current equality before publication | Authoritative client independently queries its protected service; never echo a request as proof |

The explicit writable-script-argument observation demonstrates why protecting
only the interpreter binary is insufficient for deployment. It does not show a
breach of a correctly protected installation. A protected direct script gets
descriptor pinning; a general interpreter command must obtain the same protection
for its script/dependencies from installation controls. Merely adding a signature
whose key is readable by the same trusted-account attacker would not improve
this boundary. No new receipt signature is required by the current direct,
trusted-client stdio boundary plus fresh publication query. A deployment adding
an untrusted transport/store between client and authoritative service must supply
its own authenticated service protocol and protected rotation mechanism.

Acceptance evidence is split by boundary: `gate_identity` and executable unit
tests cover writable installations, symlinks and atomic upgrades;
`gate_context` covers caller environment injection; `gate_dependencies` and
invocation unit tests cover unsafe and replaced interpreter chains;
`gate_configuration` and `p2_boundaries` cover persisted configuration protection;
`gate_receipt` and publication tests cover mutation/expiry/wrong binding/replay.
These are isolated local regressions, not proof of an installed production gate
or the real-model workflow acceptance tracked separately in #38.
