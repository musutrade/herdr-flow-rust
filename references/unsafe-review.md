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

## Directory descriptor and follow-up source review

`open_dir_fd` has one production caller: the TUI bridge immediately transfers
its newly owned raw descriptor into a File, opens the lock relative to it, and
retains that File in the parent until cleanup. No child-side directory-fd use
was found. The previous open flags omitted CLOEXEC; an isolated child exec
confirmed it could still identify the original temporary directory through
`/proc/self/fd/N`. The open now sets O_CLOEXEC atomically, and that same regression
passes. Its public contract states the raw ownership transfer and the limited
leaf-symlink check; it does not claim ancestor/owner validation on its own.

Additional source blocks reviewed:

| Source / operations | Preconditions and conclusion |
| --- | --- |
| `util/unix_socket.rs` (4) | Zeroed sockaddr_un has valid scalar/array fields; path bytes reject NUL and reserve a terminator; supplied sockaddr size matches storage. socket creates a fresh CLOEXEC/nonblocking fd, converted once to UnixStream; connect borrows it and failures drop the owner. Existing backlog/path/recovery tests apply. |
| `util/fs.rs` (7) | Four flock sites pass scalar flags and borrow live descriptors; FileLock Drop unlocks before its File closes. Three geteuid sites have no memory preconditions. Raw flock helpers leave ownership and lock release with caller; duplicate fd descriptions share locks. |
| `codex/rpc.rs` (2) | geteuid has no caller memory preconditions; poll receives one initialized slot while the stream remains borrowed. Timeouts are clamped before conversion; EINTR handling does not read uninitialized storage. |
| `task_env/registry.rs` (1) | geteuid compares owner without pointer arguments. This call's memory safety does not make the surrounding registry mutation a transaction; callers still need registry locks. |
| `util/process.rs`: directory open and readiness write/close | CString outlives open and success transfers one raw fd; directory exec inheritance fixed above. Readiness writes borrow the remaining serialized slice for its exact length, then close the transferred raw fd once; watch_service and CI notifier take their Option before calling. Raw API requires a dedicated fd and is best-effort, not an acknowledgment. |

### Landlock preparation before fork

The task environment, native runtime and delivery writer now prepare paths and
Landlock rules in the parent. `PreparedConfinement` transfers the fresh ruleset
fd into a File once, closes it on preparation errors or Command drop, and lends
it to the child callback. The callback uses only setsid/process-control,
PR_SET_NO_NEW_PRIVS and landlock_restrict_self syscalls with scalar errno errors.
Delivery supervisor disappearance reports ECHILD without an owned error string.
Preparation does not restrict the parent. Command construction requires existing
roots; TaskEnvironment defers preparation errors to spawn for API compatibility.

An isolated allocator sentinel reproduces the previous post-fork allocation with
long root paths (exit 99); the prepared implementation passes. The same regression
checks Command reuse, unchanged parent NoNewPrivs, owner release after drop and
failed preparation, and allocation-free propagation of ENOENT. A disappearing
root is renamed back before cleanup to preserve its registered inode identity.
Existing task environment, delivery and runtime boundary tests also pass.
This is not a claim of a reproduced production deadlock or a completed inventory:
#46 remains open pending the remaining unsafe and API reviews.

### Cleanup signal identity

The two numeric kill sites in cleanup are replaced by pidfd_open and
pidfd_send_signal. Every signal attempt requires a PID greater than one and a
recorded start time, opens an owned CLOEXEC handle, then checks the recorded
identity against /proc. After that check, PID recycling cannot retarget the
handle. The syscall uses a live borrowed fd and null siginfo with zero flags;
File owns the descriptor on all return paths. ESRCH is idempotent completion;
unsupported pidfd syscalls, permission failures and other errors propagate and
retain the cleanup plan/slot. There is no numeric-kill fallback.

An isolated regression passes an intentionally stale start time to the signal
stage: the old implementation killed its test-owned child; the new implementation
leaves it alive. This models stale input at the signal boundary, not a reproduced
kernel PID wraparound. Additional tests cover a retained handle after process
exit, invalid signal errors and rejected missing/group identities. Existing
cleanup tests cover actual marked-process termination and unrelated survivors.

This does not make /proc discovery atomic: start-time values have clock-tick
precision, and marker/parent discovery uses separate reads. Those ownership
observation limits still need separate review; no universal process identity
proof or completed #46 audit is claimed.
