# Bounded parser and dependency review (#48)

## Existing supply-chain controls

Source review checkpoint: main `fcf13bd67697ac5e00ffe1b905993cdd6e76c9a5`.
`.github/workflows/rust.yml` checks formatting, lint policy, Clippy, all-target
tests and release builds; its second job tests/builds Rust 1.85. Cargo commands
use `--locked`. `Cargo.lock` is tracked. The CRAP workflow also uses locked Cargo
commands and an absolute threshold without a baseline. These are execution
paths, not guarantees that every dependency is safe.

`.github/workflows/audit.yml` runs on PRs, main pushes, weekly schedule and manual
dispatch. It installs `cargo-audit@0.22.2` and runs `cargo audit` against the lock
file and current advisories. Its Actions, and those in the Rust/CRAP workflows,
use full commit SHAs. `.github/dependabot.yml` requests weekly Cargo and Actions
updates. Passing audit means no reported advisory violation at that run, not
that the project has no vulnerabilities.

A locked, offline `cargo metadata --format-version 1` inventory at this checkpoint
found 87 dependency packages (including target-specific dependencies), all from
the crates.io registry, and no package names with multiple resolved versions.
Declared expressions include MIT/Apache alternatives, Unlicense/MIT,
Unicode-3.0, an LLVM exception alternative and an LGPL alternative; these are
metadata, not a legal approval or a choice of license alternatives. One package
uses the legacy `MIT/Apache-2.0` expression. The project manifest has no license
field and no repository license/allowlist policy was found.

Do not introduce a cargo-deny license allowlist by guessing a policy. A license
approval decision remains with the project owner; no blanket allow/ignore list
or exception is added here. The existing audit and lock/MSRV controls remain.
Revisit source/duplicate enforcement if the lockfile introduces a git/path source
or multiple versions; neither is currently a demonstrated gap requiring a new
tool. Re-run the inventory whenever dependency resolution changes.

## Selected boundary and regression

The shared WebSocket codec is used by RPC and the TUI observer. Its public
`apply_mask` helper panicked on a one-byte mask and a four-byte payload. The
minimal synthetic regression is `apply_mask(b"four", &[1], 4)`; no captured
traffic or task data is needed. Nonempty masks shorter than four bytes now
return a protocol error. Empty masks use a four-byte zero mask, preserving the
intended identity operation; masks longer than four bytes retain their existing
first-four-byte behavior. Empty payload and size-limit error precedence remain.
Production encoder/decoder callers already supply four-byte masks.

`tests/parser_bounds.rs` covers that regression, empty masks, size precedence,
masking involution, valid frame splitting at every boundary for seven payload
sizes (0 through 256, including the 125/126 length transition), and arbitrary
synthetic frame chunks. The arbitrary-input generator uses xorshift32 with seed
`0x4f525043`, 512 cases, at most 256 input bytes per case, chunks of 1–17 bytes
and a 128-byte decoded-message limit. No corpus is accumulated. Empty/incomplete
input may be accepted without producing a message; malformed-input protocol
errors are normal rejection, while a panic or unexpected error fails the test.
This finite corpus is a regression/property check, not exhaustive fuzzing.

## Running and interpreting results

Build separately from the bounded runtime check:

```sh
CARGO_TARGET_DIR="$PWD/target" cargo test --locked --test parser_bounds --no-run
```

Use the executable path printed by Cargo (under that dedicated target):

```sh
(ulimit -v 524288; timeout --kill-after=2s 30s PATH_TO_PARSER_BOUNDS_TEST --test-threads=1)
```

Runtime budget: 30 seconds, 512 MiB virtual address space, one test thread,
finite cases as above. Compilation is outside this runtime budget. The normal
all-target test suite also runs these finite checks; no service, credentials,
network requests or real-agent sessions are involved. No long fuzz campaign is
added to required CI.

Exit 0 means the finite assertions passed; protocol refusals are expected inputs
to that result. Rust assertion/panic failures normally return 101 and retain a
backtrace/diagnostic. Timeout status 124 means the deadline was exceeded (not
proof of an infinite loop); forced kill or allocator failures must be checked
against stderr and the applied limits. Do not label every signal as an OOM or
normal parser rejection. Preserve unexpected failure evidence separately from
successful reruns; minimize a failing synthetic input before adding a regression.
No unbounded raw log or corpus archive, extra runtime dependency, new license
policy, or real-workflow acceptance claim is introduced. #38 remains separate.
