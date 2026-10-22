# Task-owned Codex and OpenCode runtimes (#35)

The linked task model is described in [task-lineage.md](task-lineage.md).
A producer is now an explicit `{ "adapter": "codex" | "opencode", "session": "..." }`
object on its result wait. Legacy `producer_session` remains Codex-only. Supplying
both is allowed only when they name the same Codex identity. Registration writers,
successor intents, writer claims, command execution, takeover and result signals
use the same identity. Successor actions specify `adapter` (legacy default: codex).
`progress::accept_identity` is the adapter-qualified library entry point;
`progress::accept` remains the legacy Codex wrapper.

The CLI selects `OPENCODE_SESSION_ID` for an OpenCode producer or coordinator and
`CODEX_THREAD_ID` for a Codex identity. Task takeover selects the producer from the
pending operation, independently of the coordinator's adapter. Native collectors
verify their bound session directly; they do not set or impersonate either variable.

## Owned environment and fresh activity

`task_runtime` builds on the owned `TaskEnvironment` API from #33. It does not take
ownership of an existing shared server, user session, user home or global settings.
The new native path supports Git snapshots; other artifact adapters are rejected
explicitly here, while the generic lineage API still supports its own adapters.

1. Create/restore a task environment. Create the linked requirement, input artifact
   and activity through `task-lineage`. Its signal wait initially has no producer.
   Put the queue and coordinator state inside the disposable environment.
2. `task_runtime::prepare(&environment, assignment)` creates a private runtime root.
   Provision only the explicitly required provider configuration/credentials into
   that root (`c/config.toml`, `c/auth.json` for Codex;
   `config/opencode/opencode.json` for OpenCode). No user conversation, memory,
   plugins or global configuration is copied automatically.
3. `task_runtime::launch(&mut environment, &root, &spec)` starts a dedicated native
   server and records the marked process. `spec` contains `adapter`, absolute
   `binary`, owned `cwd`, and `model: {providerID, id}`. OpenCode also needs an
   explicit loopback `port`. Library embedders provide `driver_binary` pointing to
   herdr-flow for the Codex connection process; the CLI uses its own executable.
4. `task_runtime::native::create` takes `queue_dir`, `wait`, a concrete `instruction`
   and optional evidence references. It constructs an input packet from the current
   requirement and activity binding, creates a **new** native session, verifies
   the returned directory/model/session and empty conversation, then binds the
   wait. Development also acquires its workspace writer. Acceptance requires a
   separate checkout from the input artifact and all active writers.
5. `native::dispatch` verifies the immutable packet digest, current task binding,
   exact input HEAD and tracked contents, and coordinator notification readiness.
   It persists the send intention before submitting the packet once.
6. `native::collect` reconciles the existing submission and emits the existing queue
   signal. The coordinator still ACKs, inspects evidence and applies the result.
   A completed native turn is execution status, **not** acceptance approval.

CLI equivalents use `herdr-flow task-runtime prepare|launch|create|recover|dispatch|collect
--task TASK --state-dir ROOT [--spec FILE]`. `prepare` requires `assignment` in its
spec and returns the computed runtime root; subsequent commands use that root.
The environment must already be registered. There is no implicit new environment
or replacement session when a command fails.

## Isolation and lifetime

Runtime HOME, XDG configuration/data/state/cache/runtime directories, CODEX_HOME,
OpenCode configuration, temporary files, logs and output all live inside the
owned environment. Native processes inherit the environment instance marker.
Only the configured proxy and certificate environment is forwarded for provider
connectivity; other inherited runtime/session configuration is cleared.

OpenCode remains pinned to **1.19.3**. Its runtime receives a second Landlock
restriction limiting writes to its own runtime root and activity checkout.
Codex 0.163 also uses a fixed `/tmp/codex-daemon-UID` socket/lock fallback. Its
server therefore runs through `/usr/bin/bwrap` with a private `/tmp`, private
`/dev` and `/proc`, and a read-only outer filesystem; only its runtime root and
activity checkout are writable. Bubblewrap and usable unprivileged namespaces
are required. Missing confinement fails; no shared-directory fallback is used.
`/dev/null` is an allowed nonpersistent sink in the Landlock command path.

A task-owned Codex broker retains the original native connection, preserving a
fresh thread before its first turn. It records exact-thread turn events and
pending native approvals without answering approvals. Its request surface is
limited to the native thread/turn methods used by this adapter. Losing this
process does not authorize a new thread or another turn. The old session must be
verified on recovery; an unpersisted thread may require explicit cancellation
and a fresh activity, never implicit replay.

The native socket's private `/tmp` fallback is resolved inside the owned runtime
root. A held directory descriptor permits connecting even when the host pathname
is longer than a Unix socket address. No external symlink or cleanup exception is
created. Servers, connection processes, logs, native session databases/rollouts,
configuration copies, queue records and journals are reclaimed by
`destroy_environment`; no completed-task archive is created. Namespace bootstrap
runs before the read-only mount policy (it must configure its uid map); the native
agent is executed only after that policy is installed.

## Output and recovery guarantees

Development pins an **input**, then reports its actual output commit with the input
identity. Collection does not require development HEAD to remain at the input.
A collected output stays stable across repeated collection even if development
subsequently advances. Applying the output still uses the lineage transaction.
Acceptance keeps the submitted HEAD and tracked contents fixed; native collection
rejects drift. Untracked build/test outputs may exist in the isolated checkout.

Creation, submission, native completion, signal persistence, coordinator ACK and
acceptance are distinct facts. Creation and turn sends are never retried after
ambiguous responses. Only read-only startup health observations are retried.
`recover` binds only a recorded, verified session and the original assignment.
Approval/user-input blockers, partial responses, unknown sends, timeouts, failed
turns and terminal reports retain their separate meanings. Collectors never turn
a timeout into success and never automatically answer a native approval.

Codex metadata reads avoid deprecated full-history hydration. Collection uses
paginated turns when supported, otherwise exact-thread broker events. An owned
native rollout is a final recovery source: session metadata, turn context,
model/cwd and terminal turn id must all match. A partial rollout is not completion.
An OpenCode terminal journal is revalidated against its producer binding on every
collection, including retries.

## Verification scope

`tests/opencode_session.rs` and `tests/lineage.rs` cover initial/successor OpenCode
writers, adapter mismatches, takeover, real Git output commits, fixed acceptance
HEAD/contents and non-replayed submission recovery. `tests/task_runtime.rs` contains
explicit synthetic HTTP/WebSocket fixtures for normal CI and ignored installed
runtime tests. The installed tests create dedicated real sessions, optionally run
model inference, and destroy the entire environment. Their coordinator is a
protocol fixture; they are not evidence of a fully autonomous multi-agent
acceptance/delivery loop (that remains the later end-to-end scope).

The installed test takes `HERDR_LIVE_{CODEX,OPENCODE}_BINARY`, corresponding
`_MODEL` JSON objects and optional `_CONFIG` paths. `HERDR_LIVE_CODEX_AUTH` supplies
an explicitly selected auth copy; `HERDR_LIVE_INFERENCE=1` requires an actual
successful native model response. `HERDR_LIVE_ADAPTER` can select one adapter.
`HERDR_LIVE_ROLE=development` additionally requires native tools to modify the
fixture file and create a new Git commit, collected as the activity's output.
These inputs are copied only into disposable test environments. Failures are
reported and the test environment is cancelled/destroyed, not marked delivered.

A separate opt-in mixed test is designed to run the Codex developer through task-owned
`native::create/dispatch/collect` and then the existing real OpenCode coordinator
and reviewer. It copies only selected Codex config/auth into the worker `c/`
directory, uses Codex `on-request` approvals without automatic answers, and
checks the actual `turn.json`/queue event/apply evidence. Its intended scope is a
mixed Codex-worker/OpenCode-coordinator flow, not a Codex coordinator or twenty-task
claim. It has not completed on the tested host; the startup blocker is described below.

Codex turns declare `externalSandbox` because the enclosing task-specific
bubblewrap namespace is the enforced filesystem boundary. Native approval policy
defaults to `untrusted`. An explicitly authorized runtime may select
`approval_policy: "on-request"` (used by the disposable development test); these
are the only supported policies. This does not answer pending requests or change
a user's settings. A native command that needs approval stays blocked for explicit
handling.

The Codex app-server is itself started through the outer `/usr/bin/bwrap` task
boundary. On the tested host, a single outer user namespace succeeds but the
Codex/bootstrap invocation that needs a nested user namespace fails before any
model turn (observed live1 attempt, 24.80s). The mixed preflight now reproduces
that nested namespace probe with a bounded child and fails explicitly instead of
treating an unsupported host as success. A secure fix without weakening the
runtime would require either a host that permits nested unprivileged user
namespaces or a reviewed product bootstrap that lets Codex use the outer task
sandbox as authoritative while suppressing only its redundant inner bwrap.
Disabling the sandbox, removing the outer boundary, adding CAP_SYS_ADMIN, or
changing sysctl/AppArmor/global policies is not an acceptable workaround.

Formal reviewer reports and their four continuation outcomes use the
[independent acceptance protocol](independent-acceptance.md). Native collectors
register the bound report separately from execution completion.

### Broker 请求、故障与恢复边界

Broker 每条 Unix 连接只收一帧以 LF 结束的 JSON；请求 JSON 最多 1 MiB
（不含 LF），整体读取期限 15 秒。空帧、错误 JSON、超长/未结束帧、读超时、
早断开和响应写失败只结束该连接。方法白名单或 params 对象校验失败可返回
错误，不建立原生连接。持续滴流不能重置读取期限。

客户端读取响应的预算为原生 RPC 的 16 MiB 加 1 KiB 封套空间，允许合法响应
大于请求上限；读期限 30 秒，请求写期限 10 秒。服务端响应写期限 10 秒。
所有期限均覆盖整个帧，超限不截断后继续解析。Broker 仍同步串行处理；原生 RPC 的握手与调用分别使用已有 10 秒期限；
Unix connect 非阻塞，监听队列满时直接报错。客户端断开不证明原生操作没有执行，不得重放
`turn/start` 或自动重新创建 session；先依据现有 creation/turn journal 查询。

原生连接故障或结果不明、事件持久化失败、任务 instance/当前 socket 归属变化
属于服务级故障，停止 broker。`broker-events.turns/pending` 必须是对象，分别
最多 64 项；已有损坏 journal 在执行原生操作前被拒绝，合法空对象保留。
退出尽力写单个 `broker-status.json` 的错误类别并移除自己持有的 socket；
无法写盘时仍保留原始非零退出。状态只覆盖当前运行，不追加历史或请求内容。

整个 broker 生命周期持有 `broker.lock`。`broker-owner.json` 绑定任务 instance、
PID、进程启动时间和 socket 的 device/inode。启动器等待当前子进程的 running
状态，不把成功 spawn 当成 broker 已可用。正常错误退出后可重新启动；SIGKILL
留下的 socket 仅在独占锁空闲、旧 owner 身份已失效、socket 身份匹配且不存在
活跃 listener 时回收。活跃服务、其他 instance、符号链接、普通文件和不明
归属的旧版 socket 均拒绝替换；需检查已有资源，不能另建任务绕过。退出不删除
已被替换的 socket，新 broker 同样必须遵守该锁。任务内其他同 UID 进程不得
绕过锁修改 broker 文件；该协议不是同 UID 恶意进程之间的隔离机制。

错误传播沿用现有 watcher 契约：queue/guard 通过 `watch_service::run` 的错误
收尾记录不健康状态；全局持久化错误向上返回。CI observer 的 `WatchState`
只对只读观察按错误次数退避，并受整体 deadline 限制；persist 失败立即返回。
不把这些错误改成忽略或重试有副作用操作。状态字段修复的任务保持未完成，
清理失败保留槽位，修复后沿原状态恢复。

隔离回归在 `tests/broker_lifecycle.rs`（真实 broker 子进程）和 broker transport
单元测试中。可设置 `HERDR_TEST_RELEASE_BIN` 为对应 release 二进制路径重跑
生命周期回归，验证 `panic=abort` 下的存活、退出和重启；这些协议测试不等同于
#38 的真实模型全流程验收。

### #44 acceptance evidence

The reliability changes were delivered in PRs #49 (captured output budgets),
#50 (persistent state validation) and #51 (broker isolation and ownership).
The follow-up acceptance tests exercise real broker subprocesses without a live
model. They use the same optional `HERDR_TEST_RELEASE_BIN` override as the earlier
lifecycle suite.

| Contract | Regression evidence |
| --- | --- |
| Bad connections do not poison later native calls | `malformed_connections_are_followed_by_successful_native_requests` sends malformed, empty, truncated and abandoned frames, then checks actual `thread/read` results on the same broker |
| Journal write failure is fatal and does not replay | `journal_write_failure_stops_without_replaying_and_can_recover` removes directory write permission after the native request arrives, checks EOF and exit 2, preserves the old journal, and restores permissions before a fresh broker recovers the stale socket |
| Bounded reads and writes | Broker transport tests cover exact byte limits, fragmented frames, slow drip deadlines, nonreading peers and response size; lifecycle tests cover the service read deadline |
| Ownership and recovery | Lifecycle tests cover duplicate owners, SIGKILL recovery, replaced socket and lock paths, and runtime instance loss |
| Corrupt persistent state | `lineage`, `acceptance`, `delivery` and broker lifecycle tests exercise invalid containers and field types; CLI variants check failure without partial state writes |
| Output budgets and cleanup | `output_budget` covers stdout/stderr limits, exact boundaries, descendant cleanup and preservation of an unrelated process |

The write-permission regression runs under the ordinary unprivileged test
account, matching the runtime deployment contract. Read-only storage can prevent
both failure-status persistence and socket unlink; the test deliberately checks
recovery after storage becomes writable instead of promising cleanup during the
failure. These checks do not replace #38's live model workflow acceptance.

OpenCode dispatch retries pre-submission `StateBusy` for a bounded interval while
holding the runtime guard lock. It never deletes `dispatch.json`, and it stops
retrying once `submission.json` exists (the adapter writes it before its POST).
This at-most-once guarantee assumes exclusive use of the task-runtime sender:
other callers must honor that runtime guard and must not send directly through
the same session concurrently. The absence check alone is not an atomic claim
against a sender that ignores this ownership contract. This is not same-UID
isolation or a generalized guarantee for independent concurrent send APIs.

### ACK/stall recovery boundary

The opt-in `installed_real_opencode_ack_stall_recovery_is_applied_by_native_receiver`
scenario exercises a same-session omission, not a process restart. A real native
development result arrives and is ACKed by the coordinator, which then stops
without applying it. After the task's 60-second apply deadline (measured from
result arrival, not ACK), the production queue watcher emits a `progress_stall`;
the same coordinator session ACKs that alarm, checks the exact stall event with
`event-current`, and applies the exact acknowledged original event once with the
actual collector output. The driver does not tick, ACK, apply or mutate the
deadline/watch state. The test retains an ACK-only snapshot and requires exactly
two deliveries for that session, a unique final artifact, the recorded release
receipt, and completed tool records. It does not cover a coordinator/server crash
or restart; no supported OpenCode transport restart API is added.
