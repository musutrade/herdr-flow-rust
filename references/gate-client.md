# Trusted gate consumer adapter

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
Production adapter compatibility and live deployment remain #98 integration work.

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

## Verifier identity review (#45, in progress; initial baseline #51)

The current deployment contract makes the operator-managed verifier, its
configuration, dependencies and authoritative service trust roots. An absolute
argv and a working directory of `/` alone do not enforce this contract.

| Principal | Required boundary | Baseline evidence |
| --- | --- | --- |
| Operator controlling verifier installation | Trusted to install and upgrade code and configuration | Deployment responsibility; no installer implemented here |
| Candidate task with a separate UID | Cannot write verifier, configuration, dependencies or ancestor directories | Filesystem permissions must be checked; current gate validation checks argv shape only |
| Candidate task sharing the operator UID | Must be constrained by an external sandbox or equivalent boundary | File ownership alone cannot isolate mutually hostile processes sharing this UID |
| Receipt storage writer | Stored results cannot independently authorize publication | Receipt fields are checked against operation, binding, App, check name and expiry; publication queries the verifier again and requires exact equality with the stored receipt |

Source review also finds that verifier execution inherits the caller environment
through the default `CommandOptions`. Interpreter search paths, loader settings
and runtime dependencies therefore belong in the execution trust review. This is
a source finding, not yet a demonstrated cross-principal exploit.

Before choosing an enforcement mechanism, isolated regressions must cover file
and ancestor replacement, symlinks, the validation-to-execution race, inherited
environment, legitimate upgrades and receipt mutation/replay. Checking a path
once and subsequently executing that path is insufficient to bind identity.
These review notes do not claim #45 acceptance or a deployed trust boundary.

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
arguments, imported modules, dynamic libraries or inherited environment.
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
