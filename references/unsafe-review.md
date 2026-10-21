# Unsafe review and API contract work (#46)

This is an incremental review, not a statement that all unsafe code is safe.
The initial review follows merged PR57. The first batch fixes empty spawn argv
panics and readiness-pipe ownership/inheritance; #46 stays open for the remaining
inventory and API work.

## Reproduce the source inventory

Run `cargo run --locked --example unsafe_inventory -- src` and retain its TSV
locally. It uses the locked syn parser, walks regular `.rs` files recursively in
sorted order and reports explicit unsafe blocks/functions/impls/traits/extern
blocks. Recognized `#[test]`, `cfg(test)` and all/any combinations that require
test propagate through visited item scopes. Other conditional branches remain
production candidates; attribute line numbers allow manual platform review.
The classification is conservative, not a compiler cfg evaluator. In particular,
negation, cfg_attr, expression-level enclosing attributes and generated items
need manual classification.

Macros are not expanded. Each invocation is listed separately, and literal
`unsafe` tokens inside its token tree get `unsafe-token-in-macro` rows; those are
source candidates, not proof that expansion executes them. Derive/procedural
attribute output, dependency code and compiler-generated unsafe are outside this
source inventory. Inspect relevant generated behavior separately; do not count
an unexpanded macro as a reviewed safe block.

At the first batch's source snapshot the inventory reports 92 explicit
production-candidate unsafe blocks, five explicit test blocks and three unsafe
tokens inside test macros. Counts identify this snapshot only. Regenerate after
changes; line numbers are not stable identifiers. No fixed historical “94 sites”
claim or missing-comment count is used as a safety conclusion.

## Reviewed boundaries in this batch

| Source / operation | Preconditions and ownership checked | Result |
| --- | --- | --- |
| `util/fs/secure.rs`: geteuid, openat, FromRawFd, mkdirat | Live directory owner; NUL-terminated name lives across syscall; valid three-argument openat without O_CREAT; successful fresh fd converted once; failed syscalls transfer no owner | Four source blocks reviewed; existing ownership and symlink tests cover behavior |
| `util/process/capture.rs`: kill, waitid, siginfo access, fcntl, poll | Own child/group; WNOWAIT reserves leader PID until cleanup; initialized siginfo with successful syscall before field reads; borrowed pipe owners stay live; poll receives exactly three initialized slots and ignores absent fd=-1 | Eight source blocks reviewed under documented exclusive-waiter/SIGCHLD assumptions; existing output/deadline tests remain applicable |
| `codex/peer.rs`: geteuid, getsockopt | Live UnixStream; initialized ucred and socklen; exact returned size; expected invoking UID checked; non-Linux explicitly rejected | Two source blocks reviewed; existing same-UID/mismatch/query-failure tests retained; no duplicate peer-authentication mechanism added |
| `util/process.rs`: readiness pipe and child setup | pipe2(O_CLOEXEC) atomically creates two fresh owners; OwnedFd handles every parent error/drop path; pre_exec only dup2/fcntl/setsid and errno; fd 3 survives exec only after checked fcntl | Fixed inheritable creation window and ignored fcntl failure; targeted flag, EBADF, readiness and isolated fd-count tests |
| `util/process.rs`: captured executable descriptor setup | Arc<File> owners survive spawn; descriptor vector allocated before fork; closure performs scalar fcntl only; no post-fork allocation/locking | Reviewed with pinned-script/interpreter replacement and parent-CLOEXEC regressions |

The readiness creation window was established by source ordering, not a claimed
reproduction of a leaked pipe in a production workload. Its regression checks
creation flags; source now uses the atomic syscall. The isolated fd-count test
covers spawn failure, timeout and successful readiness without concurrent test
threads changing the process descriptor table. Empty argv is rejected before
logs or pipes are created; it is an input-contract panic fix, not a demonstrated
memory-corruption vulnerability.

## Still open

- Complete per-site review for task environment creation/destruction, TUI signal
  and terminal state, registry/delivery, socket helpers and remaining filesystem
  and process operations. Preserve unresolved assumptions rather than marking
  all listed candidates safe.
- Review raw-descriptor APIs such as `write_ready_fd` and `open_dir_fd`, including
  ownership transfer, intended exec inheritance and failure handling at callers.
- Check macro/conditional classification against the actual Linux build and
  decide which dependency/generated-code contracts require additional evidence.
- Finish task-environment, delivery/recovery and broker public API contracts:
  locks, resource ownership, platform, side effects and recovery after partial
  failures. New rustdoc here covers spawn ownership and atomic-write ambiguity.

No examples in this batch access real user directories, credentials or services.
Inventory generation reads source; process regressions use isolated temporary
roots and children created by the tests.
