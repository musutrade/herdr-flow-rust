# Agent 长任务与故障恢复调研备忘

> **2026-10-27 流程已确认（优先适用）：** 五类角色、多阶段/PR 接续、独立故障恢复与人工交接的最新职责见[方案顶部](agent-reliability-proposal.md)。Issue #32、#130、#112、#38 已同步，链接见[索引](README.md)。下文“待讨论”、完整矩阵和前置策略数值属于历史研究，不新增交付门槛。原生 TUI 已有开发分支验证，新增恢复、发布和阶段接续仍待开发；参考项目的能力不等于本项目已经实现。

**2026-10-27 最新职责目标（优先适用）：** 用户要求异常发生后由独立故障恢复
Agent 根据现场证据处理并给出可见结果，不以穷尽故障边界或完整实测矩阵为前提。
程序提供基本异常暴露、独立触发与状态保存，Agent 承担现场诊断、处理和人工交接。
具体职责和验证范围以[方案草案顶部](agent-reliability-proposal.md)为准；下文调研
与源码评估保留为参考，不自动转化为新的业务限制或实施前置条件。

**2026-10-27 多 PR／长任务复核（优先适用）：** 对开发分支 `2f0d2b2` 的源码复核
发现，除了测试预算，原生 worker 的 turn 记录也写死了 600 秒截止时间；长执行会被
标成 `timed_out`，该分支不自动杀任务。还确认单环境单交付冻结、累计控制请求容量、
新协调者换班能力缺失和同步 Git 交付等待等场景限制。详细区分与改进建议已置于
[方案草案顶部](agent-reliability-proposal.md)。这是源码评估，未完成数小时／多 PR
真实验收；现有测试不能证明这些阈值适用于用户目标场景。

状态：临时保存，供讨论；不构成已批准的生产规则。整理日期：2026-10-26。
项目评审更新：2026-10-27。

本次用户已明确：参与流程的 Agent 必须在 Herdr 中可见，方便需要时人工介入。
这是接入验收的必要条件，不再是待确认的可选项。只有后台运行或只能查看日志，
不能完成这项验收；入口必须能定位原任务、原角色，并提供受控的人工操作。
具体恢复实现仍需验证；文档评审轮只更新文档，后续实机验证范围另见下述记录。

**2026-10-27 后续纠正：** 用户要求对照 Python 原项目和 Herdr skill。已核实
Python 的原生 Codex TUI／协议桥路径已经移植到 Rust，当前应优先复用它，暂停
扩展自制面板。独立试验的范围、失败及结果见
[Python/Rust 对照与验证记录](herdr-python-rust-verification.md)。本条优先于下文
历史评审的“控制面板优先”建议；不改变认证归上游和精确回收要求。

本文保存本轮讨论的关键事实、开源参考和未解决问题。配套设计见
[Agent 长任务与故障恢复方案草案](agent-reliability-proposal.md)。
文档评审轮未修改程序或运行服务。后续验证使用一次性资源和隔离试验代码，
未修改登录、代理、全局模型配置、已安装 hook 或既有队列，也未部署新服务。

后续已确认：既有 Rust TUI 桥的同会话显示、输入和重开通过；完整任务接入仍未
完成。共享 daemon 的 Herdr hook 继承窗口 ID 导致错投已隔离复现。两条 Codex
测试输入历史共 386 字节，Herdr 引用约 106 字节，均不是临时工作区文件副本。
历史保留与执行资源回收需分别说明，错误窗口引用则须修正。细节与失败记录见
[实机验证](herdr-python-rust-verification.md)。

## 为什么需要整体设计

开发和测试可能持续数小时。请求超时、Agent 停止响应、Git 网络失败、等待审批，
都不能直接等同于任务最终失败。逐个增加重试或放宽期限，会产生重复执行、
无限重试、错误清理和用户无法观察的风险。

当前优先问题是建立共同的状态、证据和恢复契约，再选择具体实现。
错误根因不一定可观察；系统必须允许“原因未知”和“操作结果待确认”。

## 已确认的职责边界

- 仅支持 Codex，OpenCode 已明确不接入。
- 登录、provider、订阅、计费和代理由既有上游管理。不复制认证，不另建凭据库，
  不自动更换模型、账号或计费路径。
- 协调、开发、评审角色模型可独立配置。
- herdr-flow 管理任务归属、工作区、执行状态和任务资源回收。
- 验收使用独立队列，不改全局入口、旧 watcher 和旧队列。
- 任务资源最终必须回收，同时不得删除用户配置、登录或共享资产。
- 协调、开发、评审角色必须在 Herdr 有可定位的展示及人工介入入口；后台验收通过
  不等于可见接入完成。界面形式可以评估，但不能以不可交互的状态视图替代该要求。

## 源码审计基线和事实纠正

审计基线为开发分支 `fix/130-upstream-config` 的 `e036bfc2fec0ce8040b794d8bfa2ff8f1d745d68`，
工作树位于 `/mnt/dev-ssd/herdr-flow-worktrees/issue130-upstream-config`，对应草稿 PR #142。
当前保存草案的 `/home/gem/herdr-flow-rust` 位于 `main`，不假定两处代码相同。

| 项目 | 已核查现状 | 证据位置，均指上述开发基线 |
| --- | --- | --- |
| 600 秒期限 | 测试有整条任务期限；生产 turn 也硬编码 600 秒，未完成时标记 `timed_out`。此前“只有测试有 600 秒”的说法不准确 | `tests/common/upstream_live_review.rs`、`src/task_runtime/native.rs` 的 `dispatch_codex` / `update_codex` |
| 队列等待期限 | 必须提供 1 秒至 7 天的 `timeout_seconds`，到期产生 timeout 观察结果 | `src/queue/mod.rs` |
| 发送结果不明 | 先记录意图，结果不明时拒绝盲目重发；部分传输错误会停止 broker | `src/task_runtime/native.rs`、`src/task_runtime/broker.rs` |
| 上游 turn 失败 | 保存报告，但主要按统一 `failed` 活动结果处理，网络与业务原因未充分分层 | `src/task_runtime/native.rs` 的 `update_codex` |
| 进程恢复 | 有 broker、receiver 恢复入口；接收者恢复受原身份和空闲状态限制，不能推导为任何场景均可自动恢复 | `src/task_runtime/cli.rs`、`native/receiver/recovery.rs` |
| watcher 错误 | 锁竞争可以延后；部分其他错误传播并停止监督循环 | `src/queue/cli.rs` |
| Git 交付 | 真实验收是本地目标，有意图、运行检查和引用核验；不等于远端 Git 网络恢复完成 | `src/delivery/local.rs` |
| 命令网络 | 当前上游 worker 的 workspaceWrite 策略设置 `networkAccess: false`；命令网络访问与模型服务网络是两件事 | `src/task_runtime/native.rs` 的 `turn_sandbox` |
| 清理 | 保留清理义务，运行中的 watcher 会继续处理；身份不明时拒绝删除 | `src/queue/finalization/`、`src/queue/cli.rs` |
| Herdr 可见性 | 最近协调、开发、评审通过既有 app-server 后台会话运行，没有创建可见角色窗口 | `src/task_runtime/upstream.rs`、`references/herdr-workflow.md` |
| Herdr 既有基础 | 已有 pane / terminal / session 绑定及 TUI 核验；native receiver 走另一条适配分支。已有实机测试只读检查绑定，没有覆盖新流程的展示、输入和交接 | `src/herdr.rs:1733`、`src/codex/tui.rs`、`tests/installed_herdr.rs` |
| 暂停和恢复 | `control` 更新控制标志与版本，不会中断上游 turn；`progress::resume` 注册恢复等待，不是恢复 Codex 执行。生命周期文件锁只约束遵守它的调用方 | `src/progress.rs:1866`、`:1922`、`src/lifecycle.rs` |
| 审批与输入 | broker 会记录审批／输入请求，worker 可显示 blocked；broker 白名单尚无中断或审批回应入口，`tool/respond` 只回应已记录的动态工具结果 | `src/task_runtime/broker.rs:127`、`:220`、`src/task_runtime/broker/tools.rs` |
| 取消终态 | `control=cancelled` 已存在，但终态回收只接受 `final_outcome=success\|failure`；不能把“已请求取消”直接解释成“已取消且可清理” | `src/queue/finalization/mod.rs:167`、`src/task_env/cleanup/terminal/mod.rs:69` |
| 长任务容量 | 动态工具与控制日志各限 64 条、1 MiB；事件日志也有数量边界。正常长任务是否触及这些边界取决于调用量，不能仅靠取消时间限制保证支持 | `src/task_runtime/broker/tools.rs:17`、`native/receiver/control.rs:133`、`src/task_runtime/broker.rs:209` |
| 其他 600 秒 | 结果应用与后继接纳也有默认 600 秒，分别用于监督进展；它们与 worker turn 期限含义不同 | `src/progress.rs:544`、`:706`、`:1040`、`:2102` |
| 上游重启 | 共享 socket 的设备、inode、UID 等身份变化会被拒绝；当前 broker / receiver 恢复不代表支持共享 app-server 重启后自动重绑 | `src/task_runtime/upstream.rs:12`、`:28`、`native/receiver/recovery.rs` |
| 评审边界 | 评审使用独立快照，核验 candidate、HEAD、内容与活动绑定；人工介入不能沿用旧候选的评审结论批准新改动 | `src/task_runtime/review.rs`、`src/task_env/review.rs:213` |

以上是源码与指定验收范围的结论，不是完整故障矩阵已经通过。

## 2026-10-27 评审结论

总体方向可保留：复用现有状态、意图与资源归属，区分业务结果和基础设施故障。
原方案尚不能直接作为开发完成标准。以下 P1 是进入对应实现前必须补齐的契约，
P2 是必须纳入整体验收的容量和监督问题；修订方案已补入要求，但不代表程序已实现。

| 编号 | 级别 | 原草案缺口与影响 | 修订要求 |
| --- | --- | --- | --- |
| R1 | P1 | Herdr 允许降级成状态视图，无法满足本次明确的人工介入要求 | 将展示、真实操作、原身份绑定和交还控制作为硬门槛；最先验证交互通路 |
| R2 | P1 | 仅说暂停自动写入，未区分控制标志、运行中的 turn、工具和直接终端输入；人工与自动执行可能同时写 | 设计控制权交接与旧控制者失效机制；审批回应与全面接管分开验证 |
| R3 | P1 | 将取消列为终态，但当前终态／回收协议不接受它 | 显式设计版本、取消结果、交付竞态和停止证据；不能转写成业务失败来绕过校验 |
| R4 | P1 | 429／临时错误行没有统一受“结果是否确定”约束，容易被实现成整轮自动重跑 | 先核验接纳和副作用，再应用错误类别与预算；接续不是现有能力的别名 |
| R5 | P1 | 接管后的核验没有绑定现有 candidate／评审快照，可能把旧评审用于人工修改后的提交 | 改动形成新 candidate 后重新评审；保留旧证据和原授权边界 |
| R6 | P2 | 只拆分时间策略，遗漏等待关闭后的晚到结果、日志容量以及不同 600 秒监督期限 | 逐项映射现有等待／观察／应用协议，验证容量边界且保留防重证据 |
| R7 | P2 | 展示和报警可能依赖已经退出的 watcher；重连容易被误写成自动恢复一切 | 显示数据新鲜度与监督故障；区分客户端掉线、Herdr server 重启、broker 重启和共享上游重启 |

### Herdr 能力核查边界

本机 CLI 报告 `herdr 0.9.5`；同时读取本地上游源码
`/mnt/dev-ssd/dev-tmp/opencode/herdr-source`，提交
`2ecf19a92fc3a7fff75b6ce3655ef04f4625ec01`，其 Cargo 版本同为 0.9.5。
版本相同不证明已安装二进制与此提交逐字一致；此次只读核查，未启动或接管 Agent。

- [`agent attach` 的实现](https://github.com/herdrdev/herdr/blob/2ecf19a92fc3a7fff75b6ce3655ef04f4625ec01/src/cli/agent.rs#L486)
  从已有 Agent 取 `terminal_id` 后附着终端。CLI 的 `--takeover` 不能充当 herdr-flow
  的任务控制权交接，也没有据此证明可直接接入当前后台 app-server thread。
- `agent start --help` 要求一个已有、处于交互 shell 的 pane；成功表示预期 Agent
  在该终端可输入，不表示 queue-control、任务授权和回收已经接通。
- `pane input --help` 当前控制的是右键路由，不是暂停任务写入的权限闸门。
- [Herdr 会话恢复说明](https://github.com/herdrdev/herdr/blob/2ecf19a92fc3a7fff75b6ce3655ef04f4625ec01/docs/versions/0.9.3/website/src/content/docs/session-state.mdx)
  区分客户端 detach 与服务重启：前者保留运行进程，后者不能保留原进程，可能按
  native session 引用启动恢复命令。因此 Herdr 的会话自动恢复与 flow 的恢复必须
  由同一任务控制契约协调，不能各自再启动一个控制者。

建议优先验证由 Herdr 承载、连接原 broker 的交互控制面板，复用现有后台执行和
任务状态；这是一项设计建议，不是已验证功能。原生 Codex TUI 也可作为候选，
但必须同样通过身份、模型、审批、控制互斥和完整回收验收。两条路线都不能只用
截图、日志窗口或打开第二个 Agent 来声称完成接入。

上段保留为历史评审记录，优先级已由本文顶部修订取代。此前只核查 `agent attach`
不足以评估原生接入：Python 已使用 `resume THREAD --remote ENDPOINT` 和透明
协议桥，Rust 也保留同一路径。后续应验证已有实现与新增任务 runtime 的衔接，
不能把缺少接线误判为需要重建交互界面。

## 已有验收证据如何解释

- 历史两队列并行测试一条通过、一条触及 600 秒期限；连续测试前两条通过，
  第三条触及期限，后续未启动。应保留原始测试失败，同时将业务结论记作
  “在该保护时限内验收未完成”，不能单凭此认定产品不支持长任务。
- 网络采样后的单条任务 143.13 秒通过，完成本地交付和回收；开发 turn 观测约 31 秒，
  评审约 22 秒。它不能补计此前连续验收，也不能证明历史故障已修复。
- 2026-10-26 采样中，本地读取 RPC 最大约 49.7 ms；共享 daemon 到代理 TCP RTT 为 0.143–4.945 ms，
  未观察到重传、队列积压或新服务错误。采样间隔约 3 秒。
- 实际连接使用代理端口 10812；此前按进程环境探测的 10811 不代表实际连接路径。
  两套配置均未修改。
- 代理到上游的链路、请求 DNS/TLS、首 token 和服务端排队耗时不可见。
  首次可见 item 不是首 token；没有日志不能证明没有网络异常。
- 本地 Git 10 秒检查点超时是独立问题；80 次压力通过不能替代根因定位。

本地网络证据：`/mnt/dev-ssd/dev-tmp/network-probe-20261026/`。
详细历史记录位于开发分支 `references/upstream-configuration.md` 和
`references/cleanup-observation-races.md`，不重写此前失败记录。

## 开源参考及建议借鉴范围

以下是调研摘记，不表示已经完成框架选型或源码级全面评估。

| 项目 | 参考能力 | 对 herdr-flow 的建议借鉴范围 |
| --- | --- | --- |
| [Temporal](https://github.com/temporalio/temporal) | Workflow / Activity 分层；排队、执行、整体尝试和心跳期限分开；进度可通过心跳供恢复使用 | 优先研究任务与执行尝试分层、重试预算、取消和长任务监控。不要把可重试 Activity 直接等同于可重复执行的 Agent turn |
| [OpenHands Software Agent SDK](https://github.com/OpenHands/software-agent-sdk) | 持久会话事件、状态和事件服务 | 优先研究编码 Agent 事件模型和可见性；展示使用同一份事实，恢复时校验会话与配置。不是重新接管模型认证 |
| [LangGraph](https://github.com/langchain-ai/langgraph) | 持久检查点、interrupt 与恢复同一 thread | 参考人工审批、持久等待和恢复语义。恢复可能重新进入节点，副作用仍要单独防重 |
| [DBOS](https://github.com/dbos-inc/dbos-transact-py) | 持久化步骤结果；恢复跳过已记录完成的步骤，未记录结果的步骤可能重试 | 参考完成记录、恢复责任和执行次数限制。步骤需要幂等，数据库记录无法自动证明外部动作没有重复 |

主要资料：

- [Temporal Activity 故障检测](https://docs.temporal.io/encyclopedia/detecting-activity-failures)
- [Temporal Tasks 与工作流失败区别](https://docs.temporal.io/tasks)
- [OpenHands 事件存储](https://github.com/OpenHands/software-agent-sdk/blob/main/openhands-sdk/openhands/sdk/conversation/event_store.py)
- [OpenHands 事件服务](https://github.com/OpenHands/software-agent-sdk/blob/main/openhands-agent-server/openhands/agent_server/event_service.py)
- [LangGraph 中断与恢复](https://docs.langchain.com/oss/python/langgraph/interrupts)
- [DBOS 架构与恢复约束](https://docs.dbos.dev/architecture)
- [DBOS 恢复次数限制](https://docs.dbos.dev/golang/reference/workflows-steps)

建议以 Temporal + OpenHands 为主要参考，LangGraph + DBOS 作补充。
现阶段先复用既有队列、Owner、意图日志和清理事务；不预先决定部署工作流服务，
不引入第二套任务状态权威。若后续评估外部框架，应单列 Rust 支持、部署、授权、
运维、迁移和可观测性成本，不凭“有持久工作流”就认为能替代会话接续。

2026-10-27 复核官方期限、中断、架构材料与事件服务源码后，借鉴范围保持不变：它们帮助定义期限、
事件、人工等待和步骤恢复，没有替本项目验证 Codex 会话附着、外部 Git 副作用
或 Herdr 控制权交接。不得以参考项目具有某能力，代替本项目的协议和实机验收。

## 调研结论

成熟项目提供的是可解释的执行和恢复语义，不是自动识别一切故障根因。
下一步必须明确：任务何时终止、操作何时可重试、未知结果如何核验、
人工接管如何避免并发写入，以及资源何时可以释放。配套方案围绕这些契约展开。
