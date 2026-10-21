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

#47 remains open pending independent acceptance of the review and merged
checks. #38 real-workflow acceptance is outside this source refactor.

## Second batch: TUI wire observation

After the frame codec batch, isolate `WireObserver` and its close/binary frame
rejection helpers in private `codex/tui/wire.rs`. Re-export `WireObserver` from
`codex::tui`; `new` and `feed` signatures remain unchanged. The full moved block
is byte-for-byte unchanged. This is the narrow wire decoding boundary from the
TUI candidate above, not a move of session persistence or OS ownership.

Dependency review: the observer owns header/upgrade/decoder state and borrows the
same `Rc<RefCell<SessionObserver>>`. It calls only the existing public `request`
and `response` methods; no private visibility needs widening. Parent forwarding
state stores `Option<WireObserver>` and constructs one per direction. No socket,
child, signal, lock or marker persistence code moves. Upgrade detection, frame
iteration order, mutable borrow duration and close/binary error strings remain
unchanged. Parent still needs `protocol_error` and `MAX_MESSAGE` for its own
validation and forwarding buffer bound.

Existing `tui_bridge` tests exercise split handshake/frame observations and
close/binary rejection through the preserved public path; the full validation
gates remain required. Shared session state and persistence extraction remain
deferred: this small boundary does not justify a broader TUI state-machine move.

## Final candidate disposition and acceptance evidence

Reviewed against main `0ac9a64acf863a0db264c270815d66b1459fc9eb`, after
PR #67 (frame codec) and PR #68 (wire observation). The remaining candidates
above are deliberately not selected for this issue, for the concrete reasons
below. They are not a requirement to split every large file in a later batch.

- `queue::tick` loads state, collects observations, applies `progress::stalls`,
  updates the heartbeat, saves, and then dispatches. Save-before-dispatch is a
  meaningful ordering boundary. `queue::locked` returns a caller-owned guard;
  moving or merging persistence cannot silently take over that guard lifetime.
  Keep these entrypoints and ordering together; no demonstrated gain from a
  generic persistence abstraction justifies a change.
- `progress::apply` selects `LocalArtifacts` for `apply_with_adapter`. That
  wrapper preserves an adapter substitution/test seam. The latter stages a
  cloned state, runs acceptance preparation, applies the successor and records
  lineage, then commits `*s` only after success. Its helper chain separates
  validation and mutation. Flattening it or removing the clone would need a
  separate behavioral/performance justification that this review does not have.
- `pr::mutable` validates the spec, task/version, ownership and control state;
  `pr::store` increments the version, records an audit event and replaces the
  task entry. These are not equivalent to queue JSON persistence or arbitrary
  object wrappers. Keep their error/mutation boundaries and public API; no
  identical-semantic duplicate has been established for extraction.
- `crap::analyze_source` parses the AST, collects functions in traversal order,
  scans source declarations and consumes `SpanMatcher` while analyzing each
  function. Moving just one half would separate paired ordering assumptions.
  No correctness or review benefit sufficient to justify changing this gate
  has been established; embedded source fixtures remain tests, not dead code.
- TUI session/review persistence shares observer state with marker updates.
  The narrow byte observation boundary was selected; further state-machine
  extraction would require exposing or relocating that shared mutable state.
  Retain it instead of introducing visibility changes without a demonstrated
  review benefit. RPC handshake/deadline/connection ownership likewise stays
  together; only the reusable frame codec was selected.

No public helper was removed or narrowed. The five moved public symbols retain
original re-exports without introducing new platform conditions. Other public
API contracts, platform-specific implementations and document references remain
unchanged. Repository call counts cannot establish whether downstream callers
exist; this review makes no dead-code claim from those counts.

| #47 acceptance item | Evidence / boundary |
| --- | --- |
| Current candidates and justified selection | Six-module table, two selected byte-processing boundaries, explicit rejected candidates and dependency chains above. Old duplicate/dead-code counts are not treated as evidence. |
| Clear responsibilities and compatibility | PR #67 and #68 preserve implementation bodies and old public import paths; private modules introduce no new API surface or cfg gates. Internal diagnostic/type-name strings are not promised stable. |
| Error, lock, state, timeout and cleanup semantics | Moved blocks were checked byte-for-byte; parent changes only extraction/imports/re-exports. Socket, lock, signal, process and persistence ownership remain in place. Existing frame, RPC, TUI and runtime integration tests cover consumers. |
| Public exports, CLI, MSRV, docs and gates | Both batches passed full tests (453 passed, 0 failed, 5 ignored), absolute CRAP threshold 10 without baseline/exemptions, Clippy and rustdoc with warnings denied, and stable/Rust 1.85 release builds; exact-head and post-merge CI passed. CLI and dependency files were unchanged. |

This final evidence update is Markdown only and reuses the verified PR #68
runtime evidence; it does not claim another runtime suite was run for prose.
Independent review and merged checks determine closure, not this document alone.
The public short-mask input concern remains tracked for #48; #38 real-agent
acceptance remains separate.

### Retained failure evidence

PR #68's first local rerun failed the existing `output_budget` test
`output_limit_reclaims_group_and_preserves_unrelated_process` at its absent-marker
assertion; targeted and full reruns and exact CI passed. The test starts a child
that writes after 300 ms and checks for absence after capture returns. A delayed
parent could allow that write before reclamation, but the retained assertion
alone does not distinguish scheduling delay from a reclamation defect. Keep the
failure log and investigate this separately with process/readiness evidence;
neither the unchanged source move nor a passing rerun proves its root cause.
