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
