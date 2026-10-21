# Module responsibility review (#47)

Checkpoint: `ac87d5ca8e3c2ed345d876105a1cc385590c1f3b` (after #46).
This is a current-source candidate review, not a claim that file length, helper
count, or old duplication counts establish defects. No dead-code or performance
claim is made. Public helpers may be used outside this repository.

## Candidates and decision

| Module | Concrete responsibilities / boundary | Benefit and compatibility risk | Decision |
| --- | --- | --- | --- |
| `codex/rpc.rs` | `apply_mask`, `encode_frame`, `validate_close`, `FrameDecoder` implement frame bytes and fragmentation; `decode_message` parses JSON; `Client` owns socket, handshake, deadlines and event delivery. | Frame parsing is also used by TUI `WireObserver`. Reviewing shared byte rules should not require traversing connection lifecycle. Public imports and decoder fields/methods must remain available. | Select the codec boundary first; keep JSON, handshake and connection lifecycle in the parent. |
| `codex/tui.rs` | `ReviewTracker` / `SessionObserver` track protocol observations; `WireObserver` decodes streams; `ProxyBridge` and `run_bridge` manage forwarding, child ownership and deferred signals. | An observation submodule could isolate protocol state from OS ownership. Its shared `Rc<RefCell<_>>` state and marker writes need a separate dependency review; splitting across those links could obscure readiness ordering. | Defer to a dedicated batch; do not combine with the recent signal/deadline changes. |
| `pr.rs` | `prepare`, `publication_begin`, `reconcile`, `observe`, `repair_intent` and `check_repair` share task versions, provenance and audit updates through `mutable` / `store`. | Publication and repair boundaries may make state transitions easier to follow. Moving shared mutation helpers casually could hide checks or change error ordering. | Defer until transition/caller dependencies are mapped; retain public entrypoints. |
| `queue/mod.rs` | `load_queue`, `locked`, `save` provide persistence; `collect`, `dispatch`, `tick`, `acknowledge`, `resolve` drive observation/delivery state. | Persistence is a possible boundary, but `save` updates timestamps and `locked` returns a caller-owned guard. Generic JSON helpers would not preserve these semantics automatically. | Do not merge with generic persistence helpers or move lock ownership in this batch. |
| `progress.rs` | Registration/attach/begin/sent/accept/control/release implement lifecycle; `audit` and validation helpers are reused by PR operations; diagnosis is another read path. | Read-only diagnosis could be separated after mapping shared state interpretation. Public wrappers carry error semantics and external API compatibility. | Defer; absence of internal callers does not justify deletion. |
| `crap.rs` | CLI option parsing, source/AST span matching, coverage ingestion, changed-line selection and reporting are distinct stages (`parse_options`, `analyze_source`, `execute`). | Source analysis could be isolated, but lexer spans and AST ordering must remain paired; embedded Rust test fixtures must not be mistaken for production declarations. | Candidate for a separate batch with source-span fixture tests; do not alter the gate while validating this move. |

No pair of helpers has yet been demonstrated to have identical semantics suitable
for merging. In particular, queue persistence and progress/PR mutation differ in
lock ownership, timestamp/version updates and errors. No helper deletion, broad
state-machine rewrite, or unmeasured JSON clone optimization is selected.

## First batch: private frame codec

Move the complete contiguous frame codec into `codex/rpc/frame.rs`, a private
module. Re-export `apply_mask`, `encode_frame`, `validate_close`, and `FrameDecoder`
from `codex::rpc`, preserving existing import paths, signatures, public fields and
methods. The moved body is byte-for-byte identical to the checkpoint. It depends
only on the existing protocol error constructor, size limit, result type and
random-byte helper. No socket, lock, deadline, persistence or cleanup code moves.
No new dependency, feature, CLI command or minimum Rust version is introduced.

Existing integration tests already import the old public paths: `refactor_c`
exercises fragmentation, close validation and extended/masked frames;
`rpc_client`, `tui_bridge` and `task_runtime` cover consumers. Use those existing
checks plus the complete stable/MSRV suite, rustdoc, Clippy, release builds and
absolute CRAP gate, rather than duplicating implementation-shaped tests.
Source module/type diagnostic names can change after a move; stable Rust import
paths are preserved, not an ABI or `type_name` string guarantee.

## Follow-up observations

The existing public `apply_mask` accepts an arbitrary slice but indexes it modulo
four; empty or short masks deserve a separate input-contract regression review
(the internal encoder supplies four bytes). This batch intentionally preserves
that behavior. Track the finding with #48 parser/input review; it is not evidence
that a source move fixes malformed-input behavior.

#47 remains open: this first boundary and candidate list do not claim completion
of the remaining dependency and compatibility reviews. #38 real-workflow
acceptance is outside this source refactor.
