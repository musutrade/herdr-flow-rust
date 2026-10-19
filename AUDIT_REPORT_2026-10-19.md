# herdr-flow-rust 代码审计报告

- **审计日期**：2026-10-19
- **Rust 审计基线**：[`ed3d2e6`](https://github.com/musutrade/herdr-flow-rust/commit/ed3d2e6)（工作区另有本轮审计修复，见第 8 节）
- **上游对照**：[`musutrade/herdr-issue-flow@0d7253a`](https://github.com/musutrade/herdr-issue-flow/commit/0d7253a)
- **Issue 对照**：46 个 Issue（4 个 open：#9、#11、#12、#13）
- **方法**：逐模块对照 Python/Rust、静态审查、`cargo llvm-cov` 覆盖率、syn 复杂度分析、上游 428 项测试与 Rust 206 项测试、差分 CLI/状态探测、针对性的权限/并发/超时/损坏状态复现

## 0. 结论摘要

这是一份**覆盖面很广、结构上基本忠实**的 Rust 移植：队列/任务状态机、guard、TUI/RPC、PR/合批、CI/Checks 和 advisor 都有对应实现，CLI 与 JSON 字段大体兼容，原有安全边界（argv-only 执行、原子写、权限检查、门禁绑定）大多保留。`cargo test`、`clippy`、`cargo audit` 和 CRAP 在当前范围内可以通过。

但它**还不能被视为生产验收完成**，并且此前“所有代码都通过 CRAP”的表述过强：

- CRAP 门禁**排除了自身 `src/crap.rs`**，且**不统计闭包**；把自身纳入后仍有 22 个函数超标。
- 真实 Herdr / Codex app-server / RPC / 后台 watcher / guard CLI **几乎没有行为测试**；全部外部边界使用 fake。
- 文档把上游 Python 的 live 验收和测试数量错写成了 Rust 结果；`v2-review-feedback.md` 中甚至出现不存在的 `test_src/*` 路径。
- 发现若干可在离线/本地复现的 data-loss、锁释放、状态目录权限、Git 环境隔离和超时失效问题。

**审计结论：代码可运行、可作为迁移基础；但在修复 Critical/High 项并补齐真实接入测试前，不应按 #9/#11/#12/#13 关闭生产验收。**

## 1. 量化对比

| 指标 | 上游 Python | Rust 移植 |
|---|---:|---:|
| 非测试生产行数 | 8,463 | 40,503 |
| 测试行数 | 6,193 | 8,158 |
| 生产函数数 | 554 | 4,065（CRAP 门禁测量口径，已含 `crap.rs`） |
| 测试数 | 428 | 225（27 单元 + 198 集成） |
| 行覆盖率 | 未配置 | 65.9% |
| 零覆盖函数 | 未测量 | 约 1,400（审计基线口径） |
| CC=1 函数 | — | 1,427（其中 530 个零覆盖） |
| `cargo audit` | — | 39 个依赖，0 漏洞 |

代码量约为上游的 4.8 倍、函数数约 7 倍，是 CRAP 驱动的过度拆分的直接结果。测试数只有上游的 48%，且高风险路径覆盖更薄。

## 2. Critical

### C1. CRAP 门禁自排除且忽略闭包（自排除已修复；闭包盲区仍在）

- 审计基线 `src/crap.rs:415` 跳过 basename 为 `crap.rs` 的文件。门禁测量 **3,817 / 3,913** 个函数，自身 96 个函数不在门禁内；把文件改名后用同一份 llvm-cov 数据运行得到 **3,913 个函数、21–22 个 violations、exit 1**，最高包括 `char_literal_end` CRAP 182、`changed_ranges` 110、`resolve_base` 90、`run` 51、`report` 40。
- **本轮已修复 self-exclusion**：门禁现在测量 **4,065 个函数（30 个文件）**，包含 `src/crap.rs` 的 244 个函数；`crap --all --threshold 10 --no-baseline` 在 65.9% 行覆盖下 **0 violations，exit 0**。`src/crap.rs` 最高 CRAP 约 9.07，离阈值很近但确定。
- `src/crap.rs:573` 的复杂度 visitor 仍不进入闭包；50 个函数的真实 CC 被隐藏。例如 `guard::watch` 报告 CC 1/CRAP 2，实际 CC 9/CRAP 90；`queue::run_watch` 报告 CC 2/CRAP 6，实际 CC 9/CRAP 90；`advisor::cleanup_thread` 报告 CC 4，实际 CC 22。
- CRAP 公式下 CC=1 恒为 CRAP 1；**约 530 个 CC=1 函数零覆盖**也必然通过。因此“全绿”不代表这些函数被测试。
- README 原先“3,817 个函数全部通过、全库绝对检查”的表述过强；已改为“被测量范围”，并注明闭包/CC1 口径限制。

**建议**：继续修门禁——统计闭包、加门禁自测（对真实树跑并断言自身被包含）、加模块覆盖率下限。当前门禁可以证明被测量函数的 CRAP，但仍不能作为行为/测试充分性的证据。

### C2. guard watcher 在 adapter 初始化失败时直接退出，而不是落 `needs_attention`

- `src/guard.rs:2877` 在 `guarded_once` 外构造 `Herdr::new`。`HERDR_ENV`/socket/version 校验失败时错误逃出 tick，`watch_service::run` 以 exit 2 退出。
- 上游 `guarded_once` 捕获 `GuardError`，`_guard_failure` 持久化 `phase=needs_attention`、`phase_before_error` 和 error。
- 差分复现（有效 v2 state，未设置 `HERDR_ENV`）：Rust `guard watch --once` exit 2、phase 仍为 `monitoring`；Python exit 0、phase 变 `needs_attention`。
- 影响：guard/后台自动化在配置漂移或 adapter 故障时静默死亡，而不是留下可检查、可恢复的故障。
- **本轮已修复**：`watch` 现在捕获 `Herdr::new` 错误并走 `guard_failure_without_adapter`；新增 `watch_unmanaged_environment_persists_needs_attention` 回归测试。

### C3. queue 命令的 `result.json` 写失败被吞掉，CLI 仍返回成功

- `src/queue/command.rs:384/417/451/463` 丢弃 `result.json` 写入错误，`process.json` 同样被忽略。
- 复现：子进程把 `commands/w1` 改为只读后，Rust 退出 0，`result.json` 不存在，但 `results/w1.json` 已写入并指向这个不存在的 evidence；Python 的 `atomic()` 抛错并 exit 2。
- 影响：队列最需要保护的失败恢复证据丢失；消费者可能把命令当作完成。
- **本轮已修复**：写失败现在终止执行并返回错误，不再 signal 成功。尚无只读目录/ENOSPC 回归测试，建议补。

### C4. advisor 的 per-job/global flock 在 inference 前被释放

- 审计基线 `src/codex/advisor.rs:2408-2462` 的 `_job_lock`/`_global_lock` 是 helper 局部变量，helper 返回即释放；`execute_run` 在无锁状态运行。
- 上游 `handoff_advisor.py:392-454` 用 `with` 让两把锁覆盖 `_start_thread`/`wait_result`/`_cleanup_thread`。
- 影响：第二个 worker 可在第一个 inference 期间取得 global lock，看到 `active.json`，写 `fault.json` 并返回 `skipped/global_fault`；该 circuit breaker 会阻塞后续 advice，直到人工 clear-fault。同 job 双执行、clear-fault 与 live worker 并发也可能发生。
- **本轮已修复**：两把锁现在作为参数持有到 `run_with_global_lock` 结束；`execute_run` 在锁内。尚无并发回归测试，建议补一个“第二个 worker 必须 busy 而不是 fault”的测试。

## 3. High

### H1. 批次 `corrections` / `versions` 未初始化，gate 修复和 rebase 证据会静默丢失

- 审计基线 `batch` 创建（`src/batch/lane.rs:1134-1146`）没有 `corrections`/`versions`；`repair_corrections` 和 `push_version` 只在数组已存在时 append。
- 上游 `batch_lane.py:493/564-574` 使用 `setdefault`；修复 commit 只存在于 `corrections`，rebase 后新 bare workspace 从 members 重建，修复会丢失；`versions` 缺失使 16 次 rebuild 上限不可达，旧 base/snapshot/receipt/quiescence 证据也被丢弃。
- 审计发现同时缺少对应 Rust 测试：gate-failed repair → rebase → re-integrate、成功 rebase/版本上限、merged reconcile → 下一批 freeze 均未覆盖。
- **本轮已修复**：创建批次时初始化两个数组，append 前兜底；新增 `lane::audit_tests` 以及 `batch_offer_freeze_integrate_and_snapshot` 断言。完整的 repair→rebase 回归仍未补。

### H2. `batch-integrate` 不推进 `state_version`，并丢弃 checkpoint audit/history

- 审计基线 `src/batch/lane.rs:1366-1388` 在 checkpoint 中 `save` 一个克隆并递增版本，随后用捕获的旧 `t` 覆盖 `s["tasks"]`，版本回退、checkpoint audit 丢失。
- 复现：`before=4 after=4 persisted=4 checkpoints=4 phase=integrated`；checkpoint 已写入版本 5。
- 影响：持有旧 `state_version` 的客户端仍被视为 current；崩溃后磁盘版本可能倒退；#11/#12 的 stale-intent/版本保证被削弱。
- **本轮已修复**：`persist` 现在回写 `save` 返回的最新任务，并在进入 integrate 前执行初始 persist；测试断言集成后版本前进。

### H3. trusted gate verifier 只校验“绝对路径”，没有文件身份/权限绑定

- `src/batch/gate.rs:19-58` 只要求 `verifier[0]` 绝对、参数非空；`gate::call` 直接执行。没有 regular-file、same-uid、非 group/world-writable、symlink 或 pinned dev/ino/hash 检查；receipt 是无签名的 JSON。
- 复现：一个 0777 脚本可通过 validate 并被执行，返回任意 bound receipt；本地可替换路径即可伪造 `allow`。
- `src/batch/lane.rs:539-550` 的 checkout 隔离在基线是词法 `starts_with`，可被 `..`/symlink 绕过。
- **本轮已修复**：verifier 路径和 checkout 都先 `resolve_path` 再比较；但 uid/mode/regular-file/pinned identity 与 receipt 认证仍未实现，属于剩余 High（条件性）。

### H4. 文档把 Python 的 live 验收和测试数量写成 Rust 结果

- `references/v1-acceptance.md` 的 live 记录（PR #16/#17、OpenCode v2 review）产生于 2026-10-17，早于 Rust 移植；`v2-review-feedback.md:11/24` 的 428/331 是上游 Python 数量；`:31-33` 的 `test_src/*` 在 Rust 树中不存在；`tests/pr_batch.rs` 只有 4 个测试、1 个 PR 生命周期路径。
- `references/v2-acceptance.md:29-33` 声称六项 ownership/publication/conflict/rebase 回归由“full Rust suite”验证；这些是上游 Python commit 的修复，Rust 没有等价的六项回归。
- **本轮已更正文档**：加入“Python live evidence 不是 Rust 验收”的显著说明，修正 428/331/`test_src` 的语境，并收窄 batching/pr-ledger/task-progress 的测试覆盖声明。详细差异见 `AUDIT_REPORT_2026-10-19.md` 与本轮 diff。
- 这仍是**验收证据缺口**，不是文字小问题：#15 的关闭不能转移到 Rust，#11/#12/#13 的 live 前置条件仍未满足。

### H5. 真实 adapter、RPC、watch/guard CLI 基本没有行为测试

覆盖率审计（基线）显示：

- `src/herdr.rs` 6.1% 行覆盖，245/249 函数零覆盖，包括 `Herdr::new`、pane/process identity、TUI launch/verify、agent/snapshot/restore。
- `src/codex/rpc.rs` 25%，167/213 零覆盖；`Client::connect`、handshake、`call`、`drain`、`wait_event`、`close` 均 0%。只有 frame codec 有测试。
- `guard::watch`/`guarded_once`/`guard_failure*`/`cli_parsed`/`cmd_*` 0%；`watch_service::run/background`、`queue::run_watch`、`ci::run/observe_loop/background` 0%。
- `tests/guard_flow.rs` 使用 `FakeGuard`；`tests/queue_flow.rs` 使用 `FakeAgent`；CI/PR 使用 fake GitHub；TUI 用真实 Unix socket 但 peer 是合成的；gate 用 shell fixture。
- 结果：真实 Herdr 通知、Codex app-server/TUI rotation、GitHub/App Check、后台 watcher 的时序、锁、readiness、重连和 close 语义都未验证。

### H6. 状态解析仍有 panic 路径（本地 DoS / release abort）

- `src/queue/mod.rs:427` 在 `queue add` 时直接 `s["waits"][&id]`；`waits: []` 会 panic（debug exit 101；release `panic=abort`）。
- `src/queue/command.rs:307` 直接 `t["writer"]["evidence"]`，writer 为 truthy 非对象时 panic。
- **本轮部分修复**：`queue add` 和 `claim_store` 改为 checked object，新增 corrupt `waits` 测试。其余状态 mutation 仍有大量 unchecked indexing，建议系统性改造。

### H7. 状态/工作区目录没有统一的 ownership/mode 检查

- `queue`/`guard`/`ci` 在写状态前 `create_dir_all`；umask 022 下新目录 0755，预先存在的 0777 目录保持 0777。`ensure_private_dir` 存在但未被这些入口调用。
- 若目录可被其他本地用户写，可替换 `queue.json`/`state.json`；guard 会使用其中的 `codex_binary`，batch 会执行 lane 的 `gate.verifier`；`watch.log`/command stdout/stderr 的 open 也没有 `O_NOFOLLOW`，可被 symlink 利用。
- batch workspace/bare repo 默认 0755/0644，私有仓库内容可能被其他用户读取。
- 这是**条件性 High**（共享可写目录时），单用户默认 0755 目录为 Medium。

### H8. Git 子进程的环境隔离不一致

- `batch::git` 清理了 `GIT_*`、固定 config/hooks，但 `pr::git` 和 `util::{checkout_root,fingerprint}` 继承 `GIT_DIR`/`GIT_WORK_TREE`/`GIT_*` 和 `PATH`。
- 复现：`GIT_DIR=repoB/.git` 时，对 checkout A 的 `pr::provenance` 成功使用 B 的 commit；`GIT_WORK_TREE` 可改变 `inside_checkout`；`PATH` 无 git 时 fallback 让检查变成“不在 checkout 内”。本地 `core.fsmonitor` 会在 fingerprint 的 `git diff` 中执行。
- 影响：provenance/来源校验可能读到错误的仓库；状态目录隔离检查可被环境变量绕过。

### H9. 超时不是硬上限，子进程后代和输出未受限

- `run_command` 超时只 kill 直接子进程，然后 join reader 线程直到 pipe EOF；子进程若派生了继承 stdout/stderr 的后台进程，调用会一直阻塞。
- 复现：`timeout=200ms` + `sh -c 'sleep 3 & exit 0'`，实际 3.0s 后返回 `Ok(0)`。
- stdout/stderr 无上限；batch `diff-tree` 的 binary patch 会整体进内存。可能导致 guard/CI watcher 挂起或 OOM。
- 被影响的常量包括 gate 45s、batch Git 60s、Herdr 30s。

### H11. advisor job 预留不是原子创建

- `write_new_file` 在最终 `job.json` 上 `create_new` 后原地写；并发 480 次相同请求的 probe：328 成功、152 失败，失败读取到 0 字节 `job.json`（EOF）。崩溃/ENOSPC 会留下永久损坏的 reservation。
- 上游 `handoff_advisor.py:86-108` 先写临时文件+fsync，再 `os.link`，读者只会看到完整文件。
- **本轮已修复**：`write_new_file` 改为临时文件+fsync+`persist_noclobber`，并新增 no-clobber/完整内容测试。

### H12. TUI 上游 socket 没有 RPC client 的 socket 类型/owner/mode 检查；transcript 切片可 panic

- `src/codex/tui.rs` 的 `connect_unix_stream` 基线直接 connect；RPC client 至少检查 socket/owner/world-writable（仍有 TOCTOU、无 SO_PEERCRED）。
- `src/guard.rs:710-714` 的 `data[start..]` 在 cursor 超出文件长度时 panic（release abort）；可由 symlink/truncate 竞态触发。
- **本轮已修复**：TUI connect 前复用 `rpc::validate_socket`；transcript cursor 越界改为返回 GuardError。RPC/TUI 的 `SO_PEERCRED` 仍未实现。

### H13. Python 兼容 JSON 的浮点格式导致持久化 hash/identity 失配

- `src/util/pyjson.rs:107-130` 用 Rust `Display` 格式化浮点；Python `json.dumps` 用 `repr`。差分 223 个随机 double 有 208 个不同（`123.45678901234567` → Rust `…568`，Python `…567`；`1e-7`/`1e20` 也不同）。
- 这些 hash 用于 CI 配置身份、PR/batch 标识、advisor handoff digest、CI 事件去重。结果：同一份含浮点的 CI wait 配置，Python 生成的事件可被 Python 消费，HEAD Rust 却报 `CI task/epoch/config mismatch`。
- **本轮未修**；跨实现状态目录复用会受影响。建议用与 Python `repr` 等价的 shortest-round-trip + exponent 规则，并加 Python/Rust 双实现的 hash 差分测试。

### H14. `resolve_path` 先做词法 `..` 折叠，语义与 Python `Path.resolve` 不同

- `src/util/fs.rs:46-79` 先规范化 `..`，再对已有前缀 canonicalize。Python 是逐段解析 symlink，再应用 `..`。
- 复现：`<base>/link/../child`（`link -> <base>/elsewhere/inner`）在 Rust 落到 `<base>/child`，Python 落到 `<base>/elsewhere/child`。所有 CLI state root、guard/queue 目录、writer workspace overlap、`inside_checkout`、command cwd 都使用它。
- 影响：状态目录可能写错位置；单 writer/checkout 边界与 Python 不一致。
- **本轮已修复**：`pop_resolved` 在应用 `..` 前先 canonicalize 当前路径；新增 symlink/`..` 回归测试。

## 4. Medium

- **M1 CI/供应链**：Actions 使用可变 tag（`actions/checkout@v4`、`dtolnay/rust-toolchain@stable` 等），`cargo test/clippy` 未加 `--locked`，没有 MSRV 1.80 job，没有 `cargo-audit`/`cargo-deny`/license 检查；手动 `cargo audit` 为 0 漏洞。
- **M2 CRAP 驱动的代码质量退化**：310/3,817 函数是单调用转发；32 组重复体覆盖 74 个函数；92 个 `too_many_arguments` 允许 + crate-wide allow；存在 `_unused_map`、`_path_buf`、`_fd_from_raw`、`run_bridge`、`sha256_hex_text`、`FlowError::category` 等死代码/未用 public helper，以及 `let _ = ...` 空操作。拆分提高了 CC 指标，但增加了参数顺序、重复语义和 review 成本。
- **M3 OpenCode 能力报告自相矛盾**：`opencode capabilities` 输出 `available:true` 且列出 “checkpoint state save/restore”“successor session start”，但 `launch_successor`/`snapshot` 实际返回 unsupported；上游 baseline 也这样，但 Rust 新增了 CLI 输出，容易误导 #14 的能力判断。
- **M4 pyjson 不是完全 Python canonical**：U+007F 在 ensure_ascii=True 下未转义；`|x|<1e-4`/`>=1e16` 的浮点格式与 Python repr 不同。差分 2000 个随机值有 180 个 hash 不一致；影响 advisor job identity/TUI event_id 的跨实现兼容。
- **M5 错误类型/JSON 兼容漂移**：spawn 失败保存 `"NotFound"` 而非 Python `"FileNotFoundError"`；`LogError`/`WaitError` 为合成值；RPC timeout 被映射为 missing/available=false；`decision` 非字符串被静默默认；`terminal_id`/thread `sessionId` 的部分缺失被接受。建议做持久化 shape 的差分测试。
- **M6 脱敏不完整**：长 token 正则不匹配含 `/`、`+`、`=` 的密钥；logging metadata、socket_path、RPC error 原文未统一脱敏。
- **M7 其他已确认问题**：
  - 投递事件顺序使用 `BTreeMap` 排序而非 Python dict 插入顺序；插入 `c,a,b` 时 Python 批次 `["c","a","b"]`，Rust 为 `["a","b","c"]`；超过 20 个 pending 时批次成员不同，可能造成旧结果延迟/饿死。
  - `guard advise --wait` 在 guard 锁内等待（Rust），Python 在等待前释放锁；最长 60 秒阻塞 ACK/watch/checkpoint。
  - `guard init --auto-review` 传 `require_bridge=false`，跳过 marker `review_events_version` 检查；上游会强制。
  - watch health 把 numeric `start_ticks` 转成字符串，数值型 identity + 持锁在 Rust 视为 healthy，Python 视为 identity changed。
  - ACK 缺失 event ID 时仍把 delivery 标为 acked/清 active；Python KeyError fail-closed。
  - readiness 的 `session=None` 跳过 guard 验证；空字符串也未按缺失处理。
  - queue/guard CLI 不接受 `--opt=value`（argparse 接受）；`guard status` 对损坏 review-events 仍 exit 0 返回 unavailable（Python exit 2）。
  - `apply_review` 在验证后再次做未验证 snapshot，可能失败或写入未验证的 review_policy。
  - `apply_mask` 空/短 mask panic（内部 4 字节，暂不可达）；read-all-before-limit（handoff/state/gh stdout）；`uuid_hex` 的 `/dev/urandom` fallback 用于 bridge nonce；`queue init` exists-then-save 竞态；`std::env::vars()` 对非 UTF-8 环境 panic；`progress::paths_overlap` 未 resolve。

## 5. Issue 映射（#42–#65）

> 详细逐项证据见 `AUDIT_REPORT_2026-10-19.md` 的审计记录；状态：**A** 已处理，**P** 部分，**N/A** 不适用/误报，**SP** 仍存在。

| # | 状态 | 摘要 |
|---|---|---|
| 42 | A | shell 字符串仅 `herdr pane run`，POSIX 引号正确；元字符拒绝 |
| 43 | A | 全部 argv-only；batch Git cwd/环境隔离 |
| 44 | P | RPC/CI 有 16MiB/10MiB+depth10；state/gh stdout 未在读取前限长 |
| 45 | P | 原子写/0600/父目录 symlink 检查有；CLI state root 先 resolve、不检查 root symlink |
| 46 | A | temp+persist / create_new，无 mkstemp 竞态 |
| 47 | P | socket owner/world-writable 有；无 token、无 SO_PEERCRED、group-writable 允许；TUI 额外检查本轮已补 |
| 48 | P | 文件 0600；目录 0755/0777 可被接受（H7） |
| 49 | A | workspace owner marker O_EXCL；残留 exists-read 非授权竞态 |
| 50 | A | full SHA 严格校验 |
| 51 | A*/P | `util::overlap` resolve；`progress::paths_overlap`/baseline verifier 词法比较（本轮 verifier 已修） |
| 52 | P | stderr 脱敏有；slash token/logging/RPC error 未统一 |
| 53 | N/A | 本地 CLI 常量合理；但超时不是硬上限（H9） |
| 54 | P | 有结构化安全事件；缺 gate allow/deny、超时/kill 事件 |
| 55 | N/A | 无网络服务；watch 间隔有界 |
| 56 | N/A | advisor 内容寻址去重；queue ACK/gate binding/expiry |
| 57 | N/A | SHA-256 内容标识；SHA-1 仅 RFC6455 accept |
| 58 | A* | gate 失败只记录计数；残留脱敏缺口 |
| 59 | A | typed FlowError，无 blanket catch |
| 60 | A | 无凭据/硬编码 home；batch email 可配 |
| 61 | P | writer/lane 要求绝对路径；root symlink 先 resolve |
| 62 | P | 有字段检查；无持久化 state schema 校验 |
| 63 | A* | transcript cursor dev/ino/size/head；残留 slice panic（本轮已修） |
| 64 | P | frame/mask/close 检查；apply_mask 空/短 mask panic |
| 65 | P | CI fmt/clippy/test/release/CRAP；无 audit/deny/fuzz/ENOSPC/SIGKILL |

**#30–#36**：30/32/36 为重组/复杂度项，指标上完成但引出 M2；31 partial（stderr 已保留，但 Herdr CLI 全部 transient=true）；33 已处理；34 partial（无故障注入/规模测试）；35 已处理（batch_git）。

## 6. 开放 Issue 与 Rust 状态

| Issue | 代码 | Rust 离线证据 | Rust live/生产验收 |
|---|---|---|---|
| #15 结果应用/接手/停滞 | 有 | `refactor_e` 18 项、`queue_flow` 10 项；状态级 | 无 |
| #11 V1 PR 登记/修复/合并 | ledger/repair 有 | 1 条 happy path + 少量 repair-intent | 无 push/PR/merge/Check 上报 |
| #12 V2 冻结/整合/门禁/发布 | 有 | happy path + guard 级；缺 repair→rebase/版本上限/merged reconcile | 无 GateHouse #98/App/publisher/成本测量 |
| #13 真实通知与安全换班 | 有 | FakeGuard + FakeAgent | 无真实 Herdr/Codex app-server/TUI |
| #14 OpenCode 自动换班 | 未实现（设计上不支持） | monitoring adapter 23 项 | 无；能力报告有 overclaim |
| #9 总览 | 广覆盖 | 206 测试 | 未验收 |

## 7. 建议优先级

1. **先修门禁**：纳入 `src/crap.rs`、统计闭包、加门禁自测和模块覆盖率下限；在门禁可信前不要再用“全绿”作为重构依据。
2. **修 Critical 运行时路径**：guard watcher（本轮已修）、queue result 持久化（已修）、advisor 锁生命周期（已修）、state 目录权限/`O_NOFOLLOW`（未修）。
3. **修 High 数据/安全边界**：batch corrections/versions（已修）、integrate 版本（已修）、verifier 身份/权限/receipt 认证（部分）、Git 环境隔离、超时硬上限。
4. **补真实接入测试**：RPC client 的 Unix socket WebSocket server、真实 Herdr adapter、guard CLI/daemon、watch/queue/CI 后台生命周期；至少一组 live-gated 测试。
5. **补故障注入**：SIGKILL 在 journal/atomic 之间、ENOSPC、只读目录、损坏/截断 state、socket swap、子进程后台持有 pipe。
6. **清理重构债务**：死代码、重复 helper、`too_many_arguments`、未用 public API；用 context struct 替代 10+ 参数 helper。
7. **收紧 CI**：action SHA pin、`--locked`、MSRV、`cargo audit/deny`、license 检查。

## 8. 本轮已在工作区修复（相对 `ed3d2e6`）

- C1 CRAP 门禁移除 `crap.rs` 自排除；重构门禁自身并新增 13 个单元测试；`crap --all --threshold 10 --no-baseline` 自包含测量 4,065 函数、65.9% 覆盖、0 violations。
- C2 guard watcher 捕获 adapter 初始化错误并落 `needs_attention`；新增测试。
- C3 queue command 的 result/process 写失败改为终止并返回错误。
- H1 batch 创建时初始化 `corrections`/`versions`，append 兜底；新增数组/集成断言。
- H2 batch integrate 持久化 checkpoint 返回的最新任务版本，并在 integrate 前执行一次 persist；测试断言版本前进。
- H3 verifier/checkout 路径 resolve 后比较。
- H10 advisor job/global 锁持有到 inference 结束。
- H11 `write_new_file` 改临时文件+fsync+`persist_noclobber`，新增 no-clobber 测试。
- H12 transcript cursor 越界返回错误；TUI connect 前复用 RPC socket 校验。
- H14 `resolve_path` 在 `..` 前解析 symlink；新增回归测试。guard watcher 对 `parked`/`needs_attention` 不再构造 adapter，新增 parked 测试。
- H6 queue `waits`/writer evidence 的 unchecked index 部分改为错误返回；新增 corrupt waits 测试。
- PR/batch 校验：严格 host/own_commits/decision 类型、dependency landing 持久化、landing number 整数比较；CI ready 消息绑定 pid。
- 文档修正：v1/v2 acceptance 的 Python live evidence 标注、428/331/`test_src` 的语境、batching/pr-ledger/task-progress 的覆盖声明、tutorial typo、README “static” 措辞。

这些修复已通过 `cargo fmt`、`cargo clippy -D warnings`、`cargo test --all-targets`（225 项）。**C1 的 self-exclusion 已修复；闭包统计和覆盖率下限仍未修复**；其余未列出的 Medium/Low 项保持原状。

## 9. 方法边界

- 上游 live 证据（PR #16/#17、GateHouse/App Check）是 Python 实现的时点记录，不是 Rust 运行记录；本报告不把它们当作 Rust 验收。
- Rust 测试中的 fake/脚本 fixture 只能证明状态机分支，不证明真实协议、权限、进程、网络和 GitHub 行为。
- 本报告基于公开源码与本地可复现实验；未对生产仓库做写操作，未使用真实 App/发布凭据。
