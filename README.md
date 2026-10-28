# herdr-issue-flow（纯 Rust）

> **独立发布切片（2026-10-28，当前实现）：** 发布使用显式 `role_models.publisher` 和冻结的 GitHub 目标／CI 策略。独立只读模型分两轮请求 `publish`、`merge`，宿主执行固定候选的推送、PR、CI 观察和实际落地确认；真实回执直接完成发布，无需最后一轮模型报告。阶段仍需 ACK／apply 原发布结果，再 `stage-complete` 接续同一父需求，最终交付才回收。当前实现与本地夹具验证尚不构成真实远端角色闭环验收，也不代表本切片已通过全套测试或远端 CI。[配置、状态与边界](references/upstream-configuration.md#independent-publisher-slice-2026-10-28-current)。

> **真实阶段交付验收（2026-10-28）：** 同一父需求的阶段交付 → 新开发 → 新的整体验收 → 最终交付与回收已通过真实模型 / Herdr 验收，**1 条通过，323.87 秒**。协调与开发使用显式 `gpt-6.1-sol/high`，两轮独立评审使用 `gpt-6-astra/high`，复用现有 Codex 0.165.0 服务；五个角色在同工作区独立 tab 中可见。实际本地 Git 交付、范围版本及旧批准失效、任务和共享会话回收均已核查。首次驱动器失败与原任务接续单独保留，不补记首次通过。该次实测未覆盖独立发布角色或远端 PR/CI。详见[实测结果、失败与回收记录](references/stage-live-acceptance.md)。

> **阶段交付接续（2026-10-27）：** 基于 #153 的恢复与交付锁争用修复，本轮增加同一父需求内的阶段范围、独立验收和交付确认接续。阶段只能选用已有验收条件；真实回执经可恢复检查点保存后，清除当前候选与批准、推进范围版本，再继续同一实例。旧结果不能用于新阶段；最终成功仍需总体验收和非阶段交付。协议／本地 Git 测试覆盖连续阶段、检查点中断恢复，以及 coordinator 重新验收到终态回收；本轮未新增真实模型／Herdr 验收。该次本地验证未涵盖独立发布或远端 PR/CI；阶段资源提前回收仍待后续实现。详见[阶段交付](references/delivery.md)及[范围选择](references/task-lineage.md)。

> **当前开发基线与恢复接续（2026-10-27）：** #142、#144（含 #145）和 #147 已合入 `main`。本轮在独立诊断后增加受管修复：恢复角色可请求重启已确认停止的本任务接收端 watcher，宿主核验原进程、会话、turn 和未决操作后执行，再由同一恢复会话复核实际结果并交还协调者。#151 的协议 CI 通过后，新增真实模型 / Herdr 串行验收 **2 条通过（157.48 秒）**，覆盖持锁拒绝、实际恢复、同会话复核及回收；首次配置比较失败与夹具锁泄漏保留记录。每个事件只尝试一次；未知启动结果不重复启动。开发工作区写入权、共享服务、配置和代理保持原边界。更广的运维修复、人工写入权交接、多阶段发布及累计记录接续仍待完成。[配置、状态、验证范围与后续工作](references/recovery-agent.md)。下方旧 PR 状态和失败记录保留为历史；全局入口、旧 watcher 和旧队列尚未切换。

> **最新流程方向（2026-10-27，优先适用）：** 已确认协调、开发、独立验收、发布、故障恢复五类 Agent 的分工，角色模型分别配置，并在 Herdr 原生 Codex TUI 中可见、可介入。需求可跨阶段和多个 PR 持续推进；异常由独立恢复 Agent 根据现场处理并给出结果。固定 20 条、穷举故障矩阵和未经任务约定的总时限不再作为通用门槛，历史失败记录保留。实现与验收范围以上方当前记录为准；人工交接仍待实现，原生 TUI 的已有验证不代表所有流程能力已经完成。详见[已确认流程与讨论记录](references/drafts/README.md)，本条覆盖下文过时的可选展示、旧验收顺序与次数要求。

> **原生 TUI 接入（2026-10-27，优先适用）：** 独立任务现在可在 Herdr 中打开原生 Codex 界面，连接已有上游服务并恢复同一任务会话。真实验收已通过同会话输入、队列工具调用、关闭重开、上下文连续性及终态回收；没有新增共享输入历史或修改全局配置。角色模型仍分别配置。[配置、操作与证据范围](references/native-tui.md)。本轮不扩展长任务、网络故障恢复和人工接管流程；旧入口、watcher 和队列尚未切换。

> **最新验收进展（2026-10-26）：** 新增真实评审失败后的最终失败回收、worker 提交后队列监督进程重启接续通过；另修复环境锁竞争的错误分类。真实失败后的修复复审也已通过。并行测试一过一超时；连续验收在第 3 条超时后停止（2/20），均未通过，超时任务已另行恢复并回收。见[失败与恢复验收记录](references/upstream-configuration.md#failure-and-recovery-acceptance-2026-10-26-latest)。

Linux 协议夹具测试需要 `/usr/bin/bwrap`（bubblewrap）及可用的用户命名空间。CI 会在临时 runner 中安装并检查此依赖；已有上游 Codex 连接不会因此安装程序或更改本机安全策略。

以任务为生命周期单位编排 issue 开发，复用任务交接、受管执行与独立 CI 监控；Herdr 是可选适配。

**支持范围：仅 Codex，明确不支持 OpenCode。** OpenCode 适配器、命令入口、HTTP 会话接入及混合验收驱动已移除。旧 OpenCode 注册会被拒绝，不会自动转成 Codex；已有用户文件、登录、配置、服务和队列不做迁移或清理。历史验收记录保留，不代表当前支持。

**认证与任务资源边界（#130）**：登录、provider、订阅和计费由既有 Codex
管理。新的隔离队列可连接已有 Codex app-server，复用其认证并按任务登记、删除会话；
不复制配置或凭据，不改变代理。独立队列已通过一条真实开发 → 独立评审 → 自动结果应用 → 本地 Git 交付 → 终态回收验收，
包括上游自动审批产生的 Guardian 子会话；候选提交由真实开发 Agent 生成。该条实测仅覆盖本地交付，真实远端发布及更广的故障／并行接续仍需分别验证。
详见[上游配置与资源回收](references/upstream-configuration.md)。

新建 Codex 协调者已接通按 Owner 限定的开发派发、自动收集、独立评审、本地 Git 交付、独立发布和终态回收；
协调、开发、评审、发布模型可分别配置；自动派发读取任务中固定的角色模型，缺失或不可用时不静默替换。
[角色配置与启动示例](references/upstream-configuration.md#role-model-configuration-2026-10-26-current)。
支持任务 broker 重启后对同一空闲接收者恢复绑定。新入口和未知结果的恢复边界见
[协调者接续说明](references/upstream-configuration.md#coordinator-continuation-implementation-2026-10-26)。
协议集成测试与真实模型总验收分别记录，旧接收者不会自动升级控制能力。

**工作流范围**：herdr-flow 权威管理 task/epoch/instance、专属 Git worktree、
受管 Agent/子进程及终态收尾，复用现有 guard、队列、ACK、任务应用及交付。
Herdr 只接入可核验归属且可释放的展示、会话和通信能力；不支持的适配明确标明，
不作为核心生命周期的前置依赖，也不接管未知共享资源。此前 task-owned 原生测试直接启动
后台 Codex app-server / OpenCode serve，没有创建 Herdr pane agent，不能作为
Herdr 开发流程全链路验收。当前开发/发布 agent 之间的 Herdr 交接也是单独证据。
证据分类和下一步验收入口见 [Herdr 工作流边界](references/herdr-workflow.md)。

本仓库是 [musutrade/herdr-issue-flow](https://github.com/musutrade/herdr-issue-flow) 的纯 Rust 重写：没有 Python 运行时、没有 `.py` 脚本，所有命令由同一个可执行文件 `herdr-flow` 提供。legacy 状态与 fresh isolated authority 是不同支持域；不能凭 JSON 形状复用旧目录或自动迁移。当前自动编排仅支持 Linux 上的 Codex app-server。

## 实现与验收进度（2026-10-26）

基线 `5645b695`（PR #136）：PR #131–#136 已交付实例/创建归属、终态写入屏障、
统一收尾、跨模块恢复和固定槽原子移交，并补同队列 seed 根预留竞争。沿用这些
实现，不重做 #33–#37，不另造 runtime、scheduler 或永久证据库。

新创建协议限 fresh format-3、已初始化 protected queue、逐次同挂载原子移交；
不迁移 legacy/旧 isolated 实例，不提供跨队列预留或任意同 UID 隔离。
参见[任务创建](references/task-instance.md)与[终态生命周期](references/task-lifecycle.md)。
最终成功（依既有交付契约）或显式最终失败进入同一自动收尾；中间测试/CI失败、
修复和结果不明不等于终态。系统停止所属执行者、核验身份并删除整个专属根，
不按项目类型或 dirty/untracked/build 内容挑选文件。清理故障只续做收尾，不改业务结果。
只有约定正式交付物长期保留；临时日志、证据、关联及恢复登记随任务收尾清除。

**代码合并、局部回归、真实接入分别记证据。** 当前 CI/SIGKILL fixture 通过不证明
本机安装及全 writer 切换、断电恢复、真实 Agent 闭环或连续任务总验收。
后续按已确认的五角色、跨阶段交付方向核验真实角色通道、远端发布、异常接续及任务回收；
历史验收顺序与固定次数不是通用门槛，既有失败记录仍保留。#130/#112/#38/#32 均保持开放。

## 当前功能

- 按 issue 和稳定阶段轮换会话，保留已有授权和有效验证证据。
- 启动前探测 Linux `/proc`、Unix domain socket 和 app-server 方法；缺少所需能力时明确失败，不静默降级。
- 本地安全边界：Herdr 参数拒绝 shell 元字符；batch Git 只允许白名单命令、拒绝全局配置覆盖并设置超时；状态与交接文件原子写入、权限 0600、拒绝符号链接父目录；app-server socket 校验同用户且非全局可写；JSON 消息限制大小与嵌套深度；子进程错误输出脱敏。
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
- Rust stable 与 Cargo（不再承诺或单独验证固定的最低 Rust 版本）
- 已认证的 GitHub CLI `gh`
- 自动编排需要运行中的本地 Codex app-server 与同版本原生 TUI（已验证 0.162.1）
- 仅启用 Herdr 适配时需要匹配的 Herdr 会话/能力；核心生命周期不依赖 Herdr

CI 在 stable 上使用锁文件构建、测试，不再运行独立的 MSRV 作业。第三方 Actions
固定到完整 commit SHA，由 Dependabot 每周提出 Actions 与 Cargo 更新；更新需要
通过相同检查。依赖审计在 PR、main 更新和每周定时执行，使用当时的 RustSec
数据库；历史审计通过不代表之后没有新公告。

构建并安装：

```bash
cargo build --release --locked
install -m 0755 target/release/herdr-flow ~/.local/bin/herdr-flow
```

**历史验收（已移除对应混合驱动）**：任务专属 Codex worker 的启动适配合入 PR #103 时，组合为 Codex
0.163.0 worker 与 OpenCode 1.19.4 coordinator/reviewer；宿主须支持外层与
Codex 内层 sandbox 的嵌套执行。需要专用外层启动器的宿主，应在每个任务的
launch spec 中显式指定已获授权的 `namespace_launcher` 路径和 SHA-256。
安装二进制不会自动选择该路径。参见[启动适配与本机接入](docs/codex-startup-adaptation.md)
和[任务运行时使用说明](references/task-runtime.md)。

以上组合验证的是后台原生 worker/coordinator 链路。它不证明 Herdr-managed
OpenCode 接入，也不证明 task-owned Codex coordinator 已可用；已合入的 receiver
bootstrap 仍未激活。当前不继续建设独立 controller 或另一套 ACK 事务系统。

将本仓库放入技能目录，或从现有 checkout 建立符号链接（目标位置须尚不存在）：

```bash
mkdir -p ~/.codex/skills
ln -s /absolute/path/to/herdr-flow-rust ~/.codex/skills/herdr-issue-flow
```

已安装旧副本时，先核对链接目标、源码提交和实际二进制；在另行批准的安装切换中处理，避免覆盖本地改动。文档 PR 不自动更新现有技能链接。

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
- [OpenCode 历史记录（已移除、不支持）](references/opencode-v2.md)
- [V1 基础循环验收与后续边界](references/v1-acceptance.md)
- [V2 有界合批、整合与恢复](references/batching.md)
- [现有 GitHub Actions 驱动的有界发布](references/actions-batching.md)
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

以下为 legacy 等待队列示例，不创建 format-3 任务生命周期；fresh isolated 配置见[生命周期](references/task-lifecycle.md)。

初始化等待队列并启动 watcher：

```bash
herdr-flow queue init --state-dir /absolute/queue --guard-state-dir /absolute/guard --poll-seconds 30
herdr-flow queue enable --state-dir /absolute/queue
herdr-flow queue watch --state-dir /absolute/queue --background
herdr-flow queue readiness --state-dir /absolute/queue
```

无模型角色检查使用同一入口加 `--spec /absolute/role-spec.json`，须从实际角色
工具上下文运行。当前只探测 Codex producer 结果文件
通道；沙箱等价性与仓库信任证据不足时仍为 unknown。参数、退出码和实际 writer
盘点见[角色就绪检查](references/role-readiness.md)。

读取任务与投递指标（只读）：

```bash
herdr-flow queue metrics --state-dir /absolute/queue
herdr-flow queue metrics --state-dir /absolute/queue --json
```

配置、状态、凭据和运行证据应保存在仓库外；任务临时状态仍须在收尾时清除，不因放在根外而永久保留。后台模式脱离终端运行，不提供崩溃自动重启；可按监控说明使用 systemd 用户服务托管。

## 状态目录与兼容性

以下文件布局为 legacy。fresh format-3 使用 protected authority 和显式 queue-owned control，
见[创建协议](references/task-instance.md#fresh-isolated-allocation-protocol)；旧队列及旧环境不自动升级。
安装/技能链接不证明实际进程、PATH 和后续启动入口已切换，须逐 writer 核验后才启用。

- 队列：`queue.json`、`results/`、`deliveries/`、`commands/`、`queue.lock`
- 上下文守护：`state.json`、`handoff.json`、`tui.json`、`review-events.json`、`history/`、`guard.lock`
- CI 观察器：`identity.json`、`status.json`、`events/`、`watch.json`、`watch.lock`
- 顾问：`advisor/<job>/job.json`、`result.json`、`launch.json`、`worker.log`
- 合批：`<workspace_root>/<batch-id>/objects.git`、`snapshot/`、`integration.index`

legacy 的兼容说明不能用于 isolated 迁移；启用所选拓扑前应在新的状态目录完成一次 `readiness`、一次结果投递/ACK 和一次只读 CI 观察。不要直接让新二进制接管存在未决 `*_sending` 相位的旧状态；先按文档检查并 `reconcile`。

P2 修复后的升级规则：

- checkout 指纹采用带版本域的 v2 编码；旧 checkpoint/handoff 的指纹不会被兼容接受，须重新检查工作区并生成 checkpoint、handoff 和相关 advice。
- JSON 身份按 Python 的浮点格式及 ASCII 转义计算。原先字节一致的身份保持一致；含受影响浮点或 DEL 的旧 CI 配置须核对后在新 epoch 重新绑定。遇到旧 Rust 编码的 advisor reservation 会拒绝自动重放；先确认已有 thread/turn、结果及 cleanup，再使用新 epoch/状态目录，不自动修改旧 ID 或复用不匹配的哈希。
- 新状态目录及中间目录以 0700 创建；既有目录须属于当前用户，允许 0700/0755，拒绝 group/world writable（包括常见的 0775）和符号链接。祖先目录也须可信（允许 root 所有的 sticky 临时目录）。已有不安全目录须先由操作者检查来源与内容，再修复权限或迁移到真实目录；程序不自动 chmod 后信任其内容。

## 安全边界

- **只读观察**：CI 观察器、指标和外部 Checks 不重跑 CI、不合并 PR、不修改仓库设置；发布与合并仍需既有授权。
- **本地信任不等于认证**：通用 `signal`、交接文件和 `producer_identity` 依赖本地文件系统信任，队列不认证生产者；协调者须独立核对 session/model、候选 SHA 和证据。
- **命令与路径**：原生 Codex 的 Herdr 客户端和 shell 插值路径拒绝元字符；batch Git 仅允许白名单命令、拒绝全局配置覆盖并设置超时；工作区须为绝对路径且不得重叠。
- **文件与传输**：状态/交接文件在已打开的目录内原子替换并限制为 0600，拒绝末级及祖先符号链接；状态读写和锁统一检查目录信任，状态/锁文件须是当前用户所有的不可共享写普通文件；app-server socket 必须属于当前用户且非全局可写；JSON-RPC 限制消息大小与嵌套深度，外部 Checks 清单最多 32 项，超过 1000 个 Check suite 时拒绝判定；错误输出会脱敏。
- **平台与能力**：上下文守护/自动换班仅在 Linux 验证；缺少 `/proc`、Unix socket 或 app-server 方法时明确失败。
- 历史 Python 审计快照见 [SECURITY_AUDIT_REPORT.md](SECURITY_AUDIT_REPORT.md)；Rust 移植的审计（含 live 验收缺口、CRAP 门禁口径和未修复项）见 [AUDIT_REPORT_2026-10-19.md](AUDIT_REPORT_2026-10-19.md)。

## 验证

```bash
cargo fmt --all -- --check
cargo run --locked -- lint-policy
cargo clippy --all-targets --all-features --locked -- -D warnings
cargo test --all-targets --locked
```

Clippy 默认规则统一设为 `deny`，`too_many_arguments` 和 `type_complexity` 逐项设为 `forbid`；同时禁止覆盖已有 `forbid` lint 组的兼容性放行。源码和测试一律禁止通过 `allow`、`expect`、条件属性或宏中的属性豁免任何 Clippy 规则，也禁止用 `allow(warnings)` / `expect(warnings)` 隐藏诊断。`lint-policy` 检查整个仓库的 Rust 文件（包括未启用的条件分支），CI 在 Clippy 前执行；发现问题须修正代码，不能增加豁免或放宽阈值。

测试覆盖队列/投递/ACK、任务状态机、guard transcript 游标、换班、review 配置、CI 分页与 required-jobs 判定、外部 Checks 身份、PR 来源/合并、合批冻结/隔离整合/快照、门禁客户端与工具函数。第一版迁移保留了原有安全边界与故障语义。

## 需求修订与成果换版

`queue task-lineage` 支持需求依据修订、经适配器核验的 Git／文件成果、显式候选换版，以及同一需求下隔离的开发／验收活动。开发新产出不会替换正在验收的版本；旧需求、候选或轮次的结果不能推进当前目标。命令及迁移契约见 [任务版本与活动关联](references/task-lineage.md)。

## CRAP 门禁

Rust 实现使用 `syn` 计算每个函数的 McCabe 复杂度，并使用 `cargo llvm-cov`（或 `--coverage` 指定的 llvm-cov JSON）计算行覆盖。**默认阈值 10，全库绝对检查，不使用基线豁免**：

- 每个函数的 `CRAP = CC² × (1 - coverage)³ + CC` 必须不超过 10。
- CC 的统计口径：`if`（含 else-if）、非通配 match 分支、`for`、`while`、`loop`、`?`、`&&`、`||`、`try` 块。闭包（含嵌套闭包）中的分支计入所属函数；嵌套命名函数单独计量，避免重复统计。
- 高复杂度函数必须拆分为小函数；低覆盖函数需要补测试。
- 门禁包含 `src/crap.rs` 自身，输出当前源码的函数数量、行覆盖率和超过绝对阈值的函数。闭包分支参与计量；宏展开中的分支仍不可见，且 CC=1 函数即使零覆盖也按公式通过。因此门禁通过不能代替行为测试或 live 验收证据。

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
  guard.rs                 守护公共入口、共享状态与锁
  guard/                   transcript、review、checkpoint、换班、观察与 CLI
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

Task-owned native Codex sessions, adapter-qualified producer identities
and development output collection: [runtime contract](references/task-runtime.md).

Fixed-version independent acceptance and four-outcome continuation:
[acceptance contract](references/independent-acceptance.md).

Confirmed-delivery legacy finish: [delivery contract](references/delivery.md).
Fresh isolated final success/failure and unified cleanup: [task lifecycle](references/task-lifecycle.md).
