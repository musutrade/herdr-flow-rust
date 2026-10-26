# Cleanup observation races — 2026-10-26

This supplements #130/#38. Full reclamation and preservation of unrelated user
processes/assets remain simultaneous requirements. No login, configuration,
provider, billing, proxy, installed entrypoint or existing watcher is changed.

## Additional environment-lock race and unresolved Git timeout (2026-10-26)

The later documentation-only commit `bf1788a` reproduced another fixture race in
[CI coverage](https://github.com/musutrade/herdr-flow-rust/actions/runs/40363636016):
`upstream_thread_cleanup_preserves_service_config_and_other_sessions` expected
one `finalization::drive` call to finish, but received the explicit lifecycle
permit contention report. The active receiver watcher uses short shared permits;
the finalizer correctly requires an exclusive permit and leaves cleanup pending.
The fixture now retries only that exact pre-mutation contention, bounded to ten
seconds. All other errors, including the deliberately lost deletion reply, still
reach the original assertions. Both deletion scenarios deliberately hold a
shared permit, assert unchanged queue state and retained resources, release it,
then exercise the retry and eventual cleanup. Production locking and deletion
semantics are unchanged. This is separate from the historical Git timeout below.

A fresh default-concurrency coverage run reproduced a different failure in
`coordinator_dispatches_collects_and_finalizes_without_driver_task_mutations`:
the watcher temporarily owned the environment lock while the fixture restored
it to send the first coordinator turn. `restore_lock` classified a failed
nonblocking lock acquisition as `Guard`, and the fixture unwrapped it.

Environment restoration and terminal cleanup now classify that known contention
as `StateBusy`. Other lock errors still propagate. The fixture shares its existing
10-second deadline across restore/start and retries only `StateBusy` before any
`receiver-start.json` intent exists. No uncertain send is retried. A regression
holds the environment lock, verifies both classifications and unchanged state,
then restores the same instance and performs ordinary terminal cleanup.

This does **not** explain the separate historical 10-second Git reply-loss
checkpoint timeout. That test passed once alone and 80 times across four
independent processes (20 per process), and the default-concurrency suites did
not reproduce that timeout. Its root cause remains unproven. The wrapper now
records each Git command's start/exit to a private fixture trace, included in
timeout diagnostics; the original deadline and identity assertions are unchanged.
The trace is outside the command's captured stderr so a killed supervisor does
not discard it. Do not count repeated passes as a root-cause fix.

Evidence: `/mnt/dev-ssd/dev-tmp/recovery-baseline-coverage.log` (reproduced lock
race), `git-removal-stress-20261026/` (80 passes),
`recovery-terminal-suite.log` (21 terminal tests passed), and
`recovery-continuation-test.log` (both continuation scenarios passed).

## Reproduced failures

The terminal cleanup scan observes same-user processes born after the task's
recorded boundary. A process can change state between the directory/identity
observation and reading its marker; unrelated processes are also inspected before
they can be excluded by their marker.

1. Rapid user-namespace process startup reproduced `EACCES` while reading
   `/proc/<pid>/environ`. An immediate subsequent stat still showed the same PID
   and start time alive. Ordinary short-lived `true` processes passed 20 runs;
   repeated `bwrap --unshare-user ... true` reproduced the failure on the first
   run. The old implementation turned that one transient read failure into a
   pending cleanup obligation.
2. After addressing that read, stress reproduced `ESRCH` specifically from
   `/proc/<pid>/stat`: the process exited during observation. The old absent-file
   branch recognized `ENOENT` only. Both Git partial removal and cleanup retry
   tests can therefore encounter a genuinely transient, unrelated process race.

The original failures remain in local investigation logs; no failed run is
reported as a successful acceptance. Fixture-only lock contention, a torn test
checkpoint publication, and a five-second dynamic-tool test deadline were also
identified separately from these production observation races.

## Fix and preserved boundaries

- Marker observations retry permission denial at most five times, with 10 ms
  between attempts. Each attempt checks the original PID/start identity again.
  Persistent denial, malformed observation and identity replacement remain errors.
- `ENOENT` and `ESRCH` are absence candidates, never sufficient evidence by
  themselves. A kernel `pidfd_open` absence check is required. A live or reused
  PID does not gain cleanup authority from an error.
- A positively observed dead process can be excluded after the marker read;
  unknown live processes still block destructive cleanup. Error diagnostics now
  include the observation stage and PID, without logging environment contents.
- Allocation crash fixtures publish checkpoint markers with an atomic rename.
  Codex fixtures explicitly reap a stopped broker before restarting it after
  restoring its original queue, and use the existing bounded lifecycle-lock retry.
- The tool bridge fixture retains the requirement for all four actual replies,
  with a bounded 15-second client deadline inside its 30-second server deadline;
  failures report the received count and journal. Under stress, all four replies
  were observed after 11.07 seconds, proving the former five-second fixture budget
  could expire despite progress. The shared-service fixture also needs to survive
  the intervening cleanup work: its configurable accept deadline is 60 seconds
  for this test, while ordinary RPC fixtures keep their 15-second default. This
  changes no production timeout, request replay or completion criterion.

## Regression coverage

Deterministic fault injection verifies recovery after transient denial for both
owned and unrelated processes, preservation of unrelated live processes, rejection
of persistent denial, a maximum of five reads, stale start-time rejection, and
`ESRCH` remaining an error while the kernel still confirms a live process.

The stress matrix repeats Git partial removal, exact upstream-thread cleanup and
replacement-receiver preservation while unrelated user-namespace processes start
and exit. This is process/protocol testing, not model execution or complete real
issue-development acceptance.

## Final validation

- Fresh complete `cargo llvm-cov --locked --json ...` run with default test
  concurrency passed after the production fixes: 191 library tests plus binary
  and integration suites. No single-thread setting is required.
- Absolute CRAP threshold 10, without a baseline: passed; selected source line
  coverage 83.4%.
- Format, Clippy for all targets/features with warnings denied, lint-policy,
  rustdoc with warnings denied and release build: passed.
- Namespace-churn stress: Git partial removal, replacement-receiver protection
  and exact upstream cleanup each passed 20 consecutive iterations after their
  applicable fixes. Final fixture-only deadline changes were checked by repeated
  actual protocol/cleanup execution; no acceptance assertion was removed.

Local logs are retained under `/mnt/dev-ssd/dev-tmp/`: the initial
`cleanup-race-namespace.log` and `cleanup-exit-race-stress.log` demonstrate the two
production races. Final evidence is in `cleanup-final-stress-git.log`,
`cleanup-final-stress-receiver.log`, `cleanup-upstream-direct-stress.log`,
`cleanup-final-full-coverage.log`, `remove-opencode-crap.log` and
`cleanup-release-check.log`. Earlier failed and interrupted attempts remain separate.

These results complete the removal/regression slice, not #130/#112/#38/#32's
remaining complete real-task, failure/recovery/parallel and consecutive-task
acceptance obligations. The PR remains a draft.

The first remote CI run after removal failed before starting the isolated Codex
fixtures because the runner lacked `/usr/bin/bwrap`. Both Rust and coverage
workflows now install bubblewrap and probe user-namespace launch in the disposable
runner. No test is skipped and no host security restriction is disabled. Local
passing results above remain distinct from the remote CI outcome.

The runner's namespace probe then exposed Ubuntu's `uid map: Permission denied`
restriction. CI installs and loads the distribution's `bwrap-userns-restrict`
profile from `apparmor-profiles` for bubblewrap. It does not disable AppArmor or
change a global user-namespace sysctl; production Flow and the local host remain
unchanged. The dependency probe must succeed before any Rust test runs.

On Noble, that profile is packaged under
`/usr/share/apparmor/extra-profiles/bwrap-userns-restrict`, not loaded from `/etc`
by package installation. CI therefore downloads/extracts the distribution package
into `RUNNER_TEMP` and loads only this profile. It does not install the whole
profile set or upgrade AppArmor/restart unrelated services. The source location
is also documented by [the VS Code sandbox maintainers](https://github.com/microsoft/vscode/issues/316046).
