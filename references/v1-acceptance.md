# V1 basic single-PR loop

> **Current support decision (2026-10-26): only Codex is supported.** OpenCode adapters, CLI, native session/HTTP integration and mixed live harnesses have been removed. OpenCode examples and acceptance records below are historical and must not be used as current instructions. Existing user installations, accounts, files and services are not migrated or cleaned.


> **Evidence caveat (2026-10-19 Rust audit):** the live observations below were
> produced on 2026-10-17 by the upstream **Python** implementation, before the
> Rust port existed. They are retained as historical reference and do **not**
> constitute Rust live acceptance or a complete Rust Herdr/Codex/GitHub
> publication/merge loop. See the original `AUDIT_REPORT_2026-10-19.md`;
> current scoped Rust evidence is linked below.

> **OpenCode policy (2026-10-20):** the v2 review observation below is historical
> only. At the user's direction, 2.0 integration is deferred because of reported
> bugs. The current baseline is 1.19.3; this record does not validate its
> end-to-end integration. Follow the [current policy](opencode-v2.md).

For current Rust socket/process fixtures and the remaining deployment evidence,
see [Rust acceptance](rust-acceptance.md).

V1 combines durable result application, explicit successor takeover, local PR
provenance and required-job decisions. External publication still uses an
authorized publisher; these tools do not push, create PRs or merge them.

The 2026-10-17 acceptance used isolated test base branches in this repository.
It did not merge source into the production `main` branch.

| Stage | Recorded observation |
| --- | --- |
| Native coordinator | Actual result delivery, exact-session ACK, application, command takeover, command result application and writer release |
| Normal PR | [PR #16](https://github.com/musutrade/herdr-issue-flow/pull/16): required job succeeded, advisory job failed, native auto-merge and landing confirmed on the isolated base |
| Repair PR | [PR #17](https://github.com/musutrade/herdr-issue-flow/pull/17): inspected required failure, one isolated native repair worker, same-PR update, successful required job and confirmed landing |
| Interrupted dispatch | Existing `sending` operation accepted and recovered without replay; old-head update observation retained until convergence |
| Old candidate | Original failed-SHA result refused after repair candidate changed |
| Owned cleanup | Two temporary protections and four test refs removed, temporary auto-merge setting restored, owned worker and observers retired; evidence retained |
| External review | Existing OpenCode v2 session with DeepSeek free published its report, signaled the registered wait, and had its delivered result ACKed and applied by the successor coordinator |

The external review found no blocking issues. Follow-up corrections canonicalize
CI evidence paths and consistently give non-code decisions precedence over code
repair. Required failure remains visible, but missing/cancelled/skipped required
jobs need inspection before dispatch. Generic OpenCode signals remain local
coordination, with identity checked separately by the coordinator.

Final source verification uses the repository's full Rust test suite, including
real temporary Git repositories and injected GitHub responses. That offline
suite complements the retained live evidence; it does not reproduce GitHub
authorization, native arrival or merge acceptance. Private reports, identities,
raw logs and queue records stay outside the repository. Session rotation alone
does not invalidate unchanged completed evidence.

GateHouse quality results and the consumer's trusted App/check deployment remain
separate acceptance work; see the [deployment evidence](gate-client.md#consumer-deployment-evidence). V2
integration/batching, automatic OpenCode rotation and production source merge
are outside this V1 basic-loop delivery. Successful required Actions jobs do not
attest external checks or grant publication/merge permission.
