# herdr-issue-flow（纯 Rust）

通过 Herdr 编排 issue 开发，以任务交接和独立 CI 监控减少 agent 长会话。

本仓库是 [musutrade/herdr-issue-flow](https://github.com/musutrade/herdr-issue-flow) 的纯 Rust 重写：没有 Python 运行时、没有 `.py` 脚本，所有命令由同一个可执行文件 `herdr-flow` 提供。持久化 JSON 文档继续使用原有字段形状，便于在检查后复用已有状态目录。当前自动编排仍以 Linux 上的本地 Codex app-server 为准；OpenCode 1.19.3 仅作为监控阶段适配器。

## 当前功能

- 按 issue 和稳定阶段轮换会话，保留已有授权和有效验证证据。
- 启动前探测 Linux `/proc`、Unix domain socket 和 app-server 方法；缺少所需能力时明确失败，不静默降级。
- 本地安全边界：Herdr 参数拒绝 shell 元字符；batch Git 只允许白名单命令、拒绝全局配置覆盖并设置超时；状态与交接文件原子写入、权限 0600、拒绝符号链接父目录；app-server socket 校验同用户且非全局可写；JSON 消息限制大小与嵌套深度；子进程错误输出脱敏。
- OpenCode 1.19.3 监控适配器：仅校验 pane/终端、shell PID 与进程身份；不支持 RPC、会话/上下文遥测、运行时快照、自动提示或自动换班，恢复需人工检查。
- 使用任务编号、epoch 和明确的写入归属协调开发、审计与发布。
- 独立观察 GitHub Actions，绑定完整 SHA、workflow、事件和 run attempt；外部 Checks 需要固定 App、精确 run/head 与基线。
- 可显式按必需 job 判定，保留 advisory 详情；取消、缺失等结果进入待决策而非代码修复失败。
- 将状态和去重事件持久化，支持恢复、进程互斥、超时及后台运行。
- 通过现有 Codex 登录调用 Luna 检查交接摘要；异步去重、归档检查线程，建议不改变换班门禁。
- 原生 Auto-review 审批接入：新 worker 启用受限沙箱与自动审批，换班继承权限，拒绝和超时通过等待队列通知。
- V1 单 PR 循环：来源范围登记、发布中断恢复、必需 job 失败仅派一次隔离修复、同 PR 更新及落地主线观察。
- V2 有界合批：冻结待验批次、任务自身变更整合、最终快照、可信门禁客户端边界及同 PR 恢复。
- 只读指标汇总：结果就绪 → 送达 → ACK → 应用 → 接手的持久时间、停滞/恢复计数、未完成等待与通知/遥测/换班能力；不新增常驻服务，不触发任务变更。
- Rust 原生 CRAP/复杂度门禁：`syn` 分析函数、可选 `cargo llvm-cov` 行覆盖、按 git diff 选择变更函数。

上下文守护进程支持注册后的本地 Codex 自动安全换班：检测阈值、请求交接、核对输入、建立空白会话并等待接手确认。等待服务可合并 CI、交接和命令结果，通过 Codex app-server 唤醒空闲调度并要求接收确认；事件文件本身不代表投递成功。CI 监控器不重跑 CI 或合并 PR，成功仅覆盖配置中的 Actions/Checks 清单。

## 安装

依赖：

- Linux（上下文守护/自动换班依赖 `/proc`、Unix domain socket 与 POSIX `flock`）
- Rust 1.80+ 与 Cargo
- 已认证的 GitHub CLI `gh`
- 自动编排需要运行中的本地 Codex app-server 与同版本原生 TUI（已验证 0.162.1）
- Herdr 编排需在 Herdr 管理的会话中进行

构建并安装：

```bash
cargo build --release
install -m 0755 target/release/herdr-flow ~/.local/bin/herdr-flow
```

将本仓库放入技能目录，或从现有 checkout 建立符号链接（目标位置须尚不存在）：

```bash
mkdir -p ~/.codex/skills
ln -s /absolute/path/to/herdr-flow-rust ~/.codex/skills/herdr-issue-flow
```

Claude 可同样链接至 `~/.claude/skills/herdr-issue-flow`；OpenCode 使用 `~/.config/opencode/skills/herdr-issue-flow`。已安装旧副本时，先确认并迁移，避免覆盖本地改动。

在新会话中使用：`使用 herdr-issue-flow 处理这些 issues`。

## 使用

所有命令都经由同一个二进制：

```bash
herdr-flow --help
herdr-flow queue --help
herdr-flow guard --help
herdr-flow ci --help
herdr-flow advisor --help
```

- [Skill 入口](SKILL.md)
- [架构与流程图](references/architecture.md)
- [最小启动、结果接续与恢复教程](references/tutorial.md)
- [角色与模型配置](references/team-config.md)
- [任务交接格式](references/handoff.md)
- [交接摘要质量建议](references/handoff-advice.md)
- [原生 Auto-review 配置与审批事件](references/auto-review.md)
- [执行、审计和故障处理](references/execution.md)
- [上下文检测与自动安全换班](references/context-rotation.md)
- [等待登记、结果通知与调度休眠](references/waiting.md)
- [V1 结果应用、后继接手与中断恢复](references/task-progress.md)
- [CI 配置、后台运行与恢复](references/ci-monitor.md)
- [V1 本地 PR 来源登记、同 PR 修复意图与恢复](references/pr-ledger.md)
- [OpenCode 接入、结果返回、监控适配器与限制](references/opencode-v2.md)
- [V1 基础循环验收与后续边界](references/v1-acceptance.md)
- [V2 有界合批、整合与恢复](references/batching.md)
- [可信门禁客户端接入契约](references/gate-client.md)
- [V2 消费端开发验证与生产验收边界](references/v2-acceptance.md)

监控一次：

```bash
herdr-flow ci --config /absolute/config.json --state-dir /absolute/ci-state --once
```

后台观察：

```bash
herdr-flow ci --config /absolute/config.json --state-dir /absolute/ci-state --background
```

初始化等待队列并启动 watcher：

```bash
herdr-flow queue init --state-dir /absolute/queue --guard-state-dir /absolute/guard --poll-seconds 30
herdr-flow queue enable --state-dir /absolute/queue
herdr-flow queue watch --state-dir /absolute/queue --background
herdr-flow queue readiness --state-dir /absolute/queue
```

读取任务与投递指标（只读）：

```bash
herdr-flow queue metrics --state-dir /absolute/queue
herdr-flow queue metrics --state-dir /absolute/queue --json
```

配置、状态、凭据和运行证据应保存在仓库外。后台模式脱离终端运行，不提供崩溃自动重启；可按监控说明使用 systemd 用户服务托管。

## 状态目录与兼容性

- 队列：`queue.json`、`results/`、`deliveries/`、`commands/`、`queue.lock`
- 上下文守护：`state.json`、`handoff.json`、`tui.json`、`review-events.json`、`history/`、`guard.lock`
- CI 观察器：`identity.json`、`status.json`、`events/`、`watch.json`、`watch.lock`
- 顾问：`advisor/<job>/job.json`、`result.json`、`launch.json`、`worker.log`
- 合批：`<workspace_root>/<batch-id>/objects.git`、`snapshot/`、`integration.index`

字段形状与旧实现保持一致；但它是独立实现，启用自动化前应在新的状态目录完成一次 `readiness`、一次结果投递/ACK 和一次只读 CI 观察。不要直接让新二进制接管存在未决 `*_sending` 相位的旧状态；先按文档检查并 `reconcile`。

## 安全边界

- **只读观察**：CI 观察器、指标和外部 Checks 不重跑 CI、不合并 PR、不修改仓库设置；发布与合并仍需既有授权。
- **本地信任不等于认证**：通用 `signal`、交接文件和 `producer_identity` 依赖本地文件系统信任，队列不认证生产者；协调者须独立核对 session/model、候选 SHA 和证据。
- **命令与路径**：原生 Codex 的 Herdr 客户端和 shell 插值路径拒绝元字符；batch Git 仅允许白名单命令、拒绝全局配置覆盖并设置超时；工作区须为绝对路径且不得重叠。
- **文件与传输**：状态/交接文件原子替换并限制为 0600，父目录不能是符号链接；app-server socket 必须属于当前用户且非全局可写；JSON-RPC 限制消息大小与嵌套深度，外部 Checks 清单最多 32 项，超过 1000 个 Check suite 时拒绝判定；错误输出会脱敏。
- **平台与能力**：上下文守护/自动换班仅在 Linux 验证；缺少 `/proc`、Unix socket 或 app-server 方法时明确失败。
- 历史 Python 审计快照见 [SECURITY_AUDIT_REPORT.md](SECURITY_AUDIT_REPORT.md)；Rust 移植的审计（含 live 验收缺口、CRAP 门禁口径和未修复项）见 [AUDIT_REPORT_2026-10-19.md](AUDIT_REPORT_2026-10-19.md)。

## 验证

```bash
cargo fmt --all -- --check
cargo run --locked -- lint-policy
cargo clippy --all-targets --all-features --locked -- -D warnings
cargo test --all-targets
```

Clippy 默认规则统一设为 `deny`，`too_many_arguments` 和 `type_complexity` 逐项设为 `forbid`；同时禁止覆盖已有 `forbid` lint 组的兼容性放行。源码和测试一律禁止通过 `allow`、`expect`、条件属性或宏中的属性豁免任何 Clippy 规则，也禁止用 `allow(warnings)` / `expect(warnings)` 隐藏诊断。`lint-policy` 检查整个仓库的 Rust 文件（包括未启用的条件分支），CI 在 Clippy 前执行；发现问题须修正代码，不能增加豁免或放宽阈值。

测试覆盖队列/投递/ACK、任务状态机、guard transcript 游标、换班、review 配置、CI 分页与 required-jobs 判定、外部 Checks 身份、PR 来源/合并、合批冻结/隔离整合/快照、门禁客户端与工具函数。第一版迁移保留了原有安全边界与故障语义。

## CRAP 门禁

Rust 实现使用 `syn` 计算每个函数的 McCabe 复杂度，并使用 `cargo llvm-cov`（或 `--coverage` 指定的 llvm-cov JSON）计算行覆盖。**默认阈值 10，全库绝对检查，不使用基线豁免**：

- 每个函数的 `CRAP = CC² × (1 - coverage)³ + CC` 必须不超过 10。
- CC 的统计口径：`if`（含 else-if）、非通配 match 分支、`for`、`while`、`loop`、`?`、`&&`、`||`、`try` 块。
- 高复杂度函数必须拆分为小函数；低覆盖函数需要补测试。
- 当前门禁测量 4,085 个函数（已包含 `src/crap.rs` 自身），均通过绝对阈值检查，行覆盖约 67.2%。注意：复杂度 visitor 不进入闭包，且 CC=1 函数即使零覆盖也按公式通过；因此“全绿”只是该门禁口径下的结果，不是行为或 live 验收的证明。

```bash
# 需要先安装 cargo-llvm-cov
cargo install cargo-llvm-cov

# 全库绝对检查（CI 使用同一命令）
cargo run -- crap --all --threshold 10

# 检查当前改动
cargo run -- crap

# 只算复杂度，不跑测试（覆盖率为零，仅用于显式检查）
cargo run -- crap --all --skip-tests
```

只检查 `src/**/*.rs`。CI 在 pull request 和 main 分支 push 时运行全库检查；任何函数超过阈值都会失败，不存在存量豁免。

## 目录结构

```text
src/
  main.rs                  多命令入口
  queue/                   等待、任务、投递、命令包装与 CLI
  progress.rs              任务状态机与诊断
  runtime.rs               接收者能力检查
  guard.rs                 上下文守护与换班状态机
  herdr.rs                 Herdr / app-server / TUI 运行时绑定
  codex/                   JSON-RPC、TUI 代理、交接顾问
  ci/                      只读 Actions/Checks 观察与策略
  pr.rs                    V1 PR 来源、发布与修复
  batch/                   V2 合批、隔离 Git、可信门禁与校验
  metrics.rs               只读指标
  crap.rs                  Rust CRAP 门禁
  util/                    原子写、锁、进程、Python 兼容 JSON 哈希
tests/                     集成测试
references/                行为契约与操作文档
```
