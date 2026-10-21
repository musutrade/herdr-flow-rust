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

### TUI signal handoff and reaping

The signal handler no longer reads a global numeric child PID or calls kill. It
only stores the pending signal in a lock-free AtomicI32. The bridge owner checks
its Child and forwards the original signal from normal control flow before the
existing graceful-stop timeout. A reaped Child is skipped; a non-reaped exit
reserves the PID until the exclusive owner waits. This requires no external
waiter and no SIGCHLD auto-reap (SIG_IGN/SA_NOCLDWAIT), now stated on run().
Process-wide signal dispositions also require a single bridge and no concurrent
replacement. Forwarding happens at an event-loop boundary, not immediately in
the handler; per-poll timeouts, repeated interrupted-connect retries and
filesystem work are documented without claiming a hard overall deadline.

The prior signal handoff regression delivered directly to a test child in the
handler. The revised test observes a pending request without delivery, verifies
owner-forwarded SIGHUP, and confirms a reaped Child is ignored. It does not claim
an actual PID wraparound was reproduced. The new normal-flow kill site borrows
exclusive Child lifecycle authority and passes only scalar arguments.

The five sigaction setup/restore unsafe sites use fully initialized Linux C
records, live sigset storage and saved dispositions. The C handler has the
required one-integer ABI and now uses only a lock-free atomic store. Restoring
saved handlers is best effort; concurrent signal-disposition mutation is outside
the documented API contract. Remaining TUI FD and /proc discovery reviews are
still pending; #46 remains open.

### TUI FD and connect-wait review

| Site / unsafe operations | Preconditions and conclusion |
| --- | --- |
| `current_uid` / geteuid | No memory arguments; effective UID is an ownership input, not isolation from other processes of that UID. |
| `executable_access` / access | CString outlives the call; X_OK is scalar. The access probe is not a guarantee that a later pathname exec observes the same file. |
| `open_upstream_socket`, `finish_connect`, `close_socket` | Fresh SOCK_CLOEXEC descriptor has exactly one raw owner until success transfers it once to UnixStream. Ordinary errors close once; peer-verification errors drop the stream. This is not an unwind-proof RAII raw intermediate: panic=abort releases OS descriptors and no recoverable panic path is assumed between acquisition and transfer. |
| `socket_flags`, `set_nonblocking` / fcntl | Connect retains ownership; F_GETFL/F_SETFL only borrow the scalar fd, preserve existing flags and propagate errors. |
| `fill_address`, `sockaddr_path_offset`, `start_connect` | Zero is valid for the Linux integer/byte record. Checked name length leaves space for NUL or abstract prefix. Pointer subtraction uses fields of one live record; computed connect length fits that record. No raw pointer outlives the call. |
| `poll_once`, `read_socket_error` | Initialized single pollfd and c_int/socklen_t outputs remain live for the calls; socket remains owned. SO_ERROR returns the expected Linux integer layout. Poll errors are captured immediately before other work. |
| `install_directory_lock` | Fresh directory fd from open_dir_fd transfers exactly once to File before further fallible operations; O_CLOEXEC already established by PR59. |
| `poll_events`, listener listen | Poll vector is not resized during call, each fd remains owned by the bridge, length equals initialized elements. Listener remains borrowed through listen; failure drops its Rust owner through the caller. |
| `open_lock_file`, `lock_exclusive` | Live parent fd and CString; O_CREAT includes correctly typed mode. Fresh O_NOFOLLOW/O_CLOEXEC fd transfers to File once; nonblocking flock borrows it. Directory/File owners release both lock and fd on later errors/drop. |
| `send_sigterm` | Called after try_wait(None) under the documented exclusive-waiter/no-auto-reap contract; an exited unreaped child reserves its numeric PID. This complements the separately reviewed pending-signal handoff. |

Connect wait now uses one elapsed-time budget across EINTR and a loop, replacing
recursive retries that restarted the entire timeout. The deterministic regression
injects an interrupted poll that consumes its budget: the extracted old retry
logic incorrectly polls again; the new loop returns timeout. This tests the retry
policy through its poll-operation seam, not delivery of real OS signals. A 10,000
interrupt sequence verifies constant-stack progress to readiness, and tests cover
zero-budget real writable sockets and preserved non-EINTR errors. Scheduling may
overshoot a timeout, but retries no longer reset it or grow the call stack. This
resolves the repeated-EINTR limitation recorded in the previous section; filesystem
work still prevents a hard overall signal-delivery deadline.

Together with the signal section, this records the TUI production unsafe sites.
Remaining task-environment /proc discovery, descriptor cleanup and key API reviews
are still pending. #46 remains open.

### Task environment descriptor walk review

| Site | Preconditions and conclusion |
| --- | --- |
| `register_process` getpgrp; `process_group` getpgid; `ancestor_is_trusted` geteuid | Scalar process/UID queries have no memory arguments. They provide observations, not atomic ownership proofs. Registration's marker/group/start-time reads remain a separate discovery concern. |
| `open_component_raw`, `file_from_fd` | Parent File remains live; CString survives openat. Only normal relative components enter via component_name, rejecting parent/root components. O_DIRECTORY/O_NOFOLLOW/O_CLOEXEC prevents following that leaf and inheritance. Fresh nonnegative fd transfers once to File. |
| `kind_at` zeroed stat and fstatat | Linux stat is valid zeroed integer storage and fully writable during the call. Its fields are read only on success. CString and borrowed directory remain live; AT_SYMLINK_NOFOLLOW inspects the leaf itself. |
| `open_child_dir` openat and File transfer | Same borrowed parent/name lifetime and flags. Every successful fd gains exactly one File owner; absent/non-directory/symlink leaves return None without an owned fd. |
| `unlink_at`, `unlink_at_dir` | Directory stays borrowed and CString outlives call; unlink flags distinguish files/links from directories. Leaf names come from directory entries, Path::file_name or internally generated quarantine names. The helpers do not themselves enforce single-component names; caller provenance is required. |
| `make_dir_writable` fchmod | A pinned directory File supplied by directory-open helpers is retained through metadata and chmod. Only owner rwx bits are added; this changes permissions and is not a read-only check. |
| cleanup `rename_noreplace` | Both CStrings and parent File remain live; identical pinned parent and RENAME_NOREPLACE avoid destination overwrite. Sources are leaf names and destinations internal quarantine names. This is an atomic rename, not an atomic transaction with journal persistence. |

Creation, persistence, restoration, process registration, descriptor registration,
lock release and recovery now state caller ownership, partial-failure and recovery
contracts. Restore reconstructs metadata but deliberately cannot reconstruct live
OS file descriptors. This review identified alignment between restored open_files
and the new empty in-memory file_handles list as a follow-up; the regression and
correction are recorded below.
Landlock syscall layout and remaining public delivery/broker contracts still need
an explicit inventory reconciliation. #46 remains open.


### Restored descriptor registration correction

A deterministic lifecycle regression confirmed that restoring metadata for path A,
registering a new descriptor for B, then unregistering A closed B's descriptor.
The durable path vector and transient descriptor vector were indexed together even
though restore clears only the latter. The same mismatch after explicit descriptor
release could also leave B's descriptor open when B was unregistered first.

Transient descriptors are now owned by a map keyed by the resolved registration
path. Unregistration removes only that path's owner, independently of metadata
order or the presence of metadata-only paths. The map is still serde-skipped;
existing durable documents require no migration. Duplicate-path registration
retains its existing no-op/drop semantics, and restore still does not reopen files.
The tests exercise both restore/old-first removal and release/new-first removal,
including repeated unregistration and normal cleanup. Persistence errors still do
not roll back an already closed descriptor, as the API contract now states.
