# 独立故障观察与诊断入口

## 受管 watcher 恢复与复核（2026-10-27，当前开发增量）

PR #147 已合入 main `5ae45a1`。本轮继续 #130 的异常处理通路：恢复角色在只读诊断后，
可以请求恢复同一任务中已经停止的 **receiver watcher**。宿主执行已有的受管 watcher
启动入口；不授予模型 shell 写入、进程控制或开发工作区写入权限。

诊断结构新增 `requested_action`。普通结论填写 `none`；请求修复必须同时使用
`disposition: repair_requested` 和 `requested_action: restart_receiver_watch`。例如：

```json
{
  "incident": "EXACT_INCIDENT_ID",
  "disposition": "repair_requested",
  "requested_action": "restart_receiver_watch",
  "facts": "原接收端 watcher 已停止，任务和会话仍保留",
  "actions": "只读取现场；尚未执行重启",
  "current_state": "等待宿主核验原操作和进程身份",
  "next_step": "恢复原 watcher 后复核实际状态"
}
```

目标、可执行程序和启动参数均来自原任务登记，模型不能另行指定。执行前核验：

- 原 Owner、runtime、receiver 与队列登记仍一致，任务未进入终态；原 broker 和上游会话可观察。
- 异常来源确为该 receiver watcher；原 PID/start ticks 在任务进程登记中，且通过可报错的
  进程观察确认旧执行已退出。心跳过期或读取失败不提供替换依据；不终止仍运行的 watcher。
- 既有 watcher 锁可取得，原 coordinator 首次发送已记录提交，原 turn 能按同一会话读取；
  未决控制执行或回复发送会阻止本次修复。原 turn 可以仍在执行，恢复的 watcher 仅只读观察。

宿主先保存 `repair.phase: executing`，再创建并登记子进程；记录新 PID/start ticks 后为
`started`，核验同一 watcher 的锁、心跳和接收端观察后为 `verified`。环境锁占用等执行前
拒绝记录为 `blocked`；启动后结果不明记录为 `unknown`。不阻塞等待原环境锁，不循环重启。
中断后已有 `started` 身份只允许重新观察；仅有启动意图、没有可信子进程身份时保留未知状态。

同一恢复会话随后收到实际修复记录，进行一次复核并返回原三类结论。首次诊断及其 turn
保存在 `diagnosis` / `diagnosis_turn`，实际动作保存在 `repair`；不能再次请求修复，也不能
把未经宿主核验的动作报告为 `recovered`。复核发送不明时保留记录，不重新发送。
最终结果仍通过既有一次性交接回原协调者；ACK、独立验收、交付和业务终态的含义不变。

本轮范围只包含 receiver watcher。队列 watcher、broker、共享服务、Git、网络配置、
人工写入权及累计记录接续仍按现有边界处理；该动作不会重发原开发、评审或交付操作。
新 watcher 归原任务登记和统一终态回收，不新建任务或恢复角色来绕开故障。

定向验证：首轮 14 项恢复模块单元回归通过；5 项独立进程协议回归通过（97.84 秒），覆盖正常诊断、
模型失败、实际 watcher 重启及复核、环境锁占用、未决控制操作保留，以及两个会话和任务资源
回收。协议回归运行真实 observer、broker 和 watcher，上游模型输出为夹具；最终失败由夹具
显式选择用于收尾，不冒称业务成功。本轮尚无新增真实模型 / Herdr 修复动作验收，下面的
109.26 秒诊断及 81.27 秒 tab 证据保持原覆盖范围。

随后新增中断接续复验 **20.33 秒通过**：watcher 已启动、复核尚未发送时停止本次 observer，
恢复到已登记新 PID 的 `started` 检查点，再启动同归属 observer；只核验并继续使用同一个
watcher，没有重启原任务或再创建 watcher。启动记录按同一 session 和原/新 PID 幂等更新，
其他身份不能被覆盖。交回协调者的消息包含宿主实际修复记录。最终提交的完整覆盖率、
Clippy、release 和 CRAP 绝对阈值 10（无 baseline）由本轮 PR 跟踪。

首个提交 `d50ac63` 的本地完整 all-targets 覆盖率回归为 **812 通过、24 显式忽略**，
远端 Rust checks 和依赖审计通过，但 [CRAP 工作流](https://github.com/musutrade/herdr-flow-rust/actions/runs/40493530552)
在中断接续测试阶段失败，不能据本地通过将它补记成功。新 watcher 取得原锁、尚未发布
自己的心跳时，旧记录仍可能被通用健康查询判为 healthy；原 `await_watch` 仅检查 healthy，
会过早返回，随后的精确身份核验因仍读到旧 PID 而正确拒绝并保留 unknown。
现在就绪等待也必须匹配刚启动子进程的 PID/start ticks，仍使用原 10 秒就绪期限。
确定性回归先保留健康的旧身份，再发布新子进程身份，确认等待只在新身份出现后返回。

首轮本地 CRAP 另报告 `Receiver::load`、`advance` 和结果 `validate` 三处超限，分别为
16.12、11.00 和 10.01。已按 runtime/登记核验、动作推进/交接、字段/结论校验拆分职责，
不放宽阈值、不添加 baseline 或忽略项。上述失败与后续最终提交的检查分别记账。

以下记录保留此前入口和实机验收历史；本节优先说明当前新增能力。

> 状态查询修正（2026-10-27）：`recovery-status` 与观察者使用同一非阻塞状态锁。
> 写入期间返回 `phase: updating`，只需稍后重新查询，不授权重发原操作；查询占锁时
> 观察者保留心跳并留待下轮继续。无写入者的残缺文档或损坏 JSON 仍拒绝读取。
> 远端首次文档发布竞态及确定性回归见[故障记录](cleanup-observation-races.md)。

> 后续布局修正（2026-10-27）：包括 Recovery 在内的新角色会话统一在父窗格所属工作区的
> 新 tab 打开，保持焦点并标注角色/任务。关闭重开和保存记录回收使用同一身份检查，见
> [原生 TUI 的最新说明](native-tui.md)。下方 109.26 秒恢复通路实测使用的是此前分屏布局。

## 真实 Herdr 验收与修复（2026-10-27，最新）

本轮使用现有 Herdr 0.9.5 客户端 / 0.9.4 服务、Codex 0.164.0，协调者与恢复角色
分别显式配置 `gpt-6.1-sol/high`，没有切换全局入口或共享服务。新私有队列中的
receiver watcher 被精确停止，原环境锁保持占用；独立 Recovery Agent 读取现场，
返回事实、实际动作、当前状态和下一步，并在自己的原生 Herdr 会话展示。
原协调者收到同一事件的结果，读取队列后明确等待人工核实，没有重放原操作或宣布业务完成。

结论是 `needs_human`：模型发现自己的 PID namespace 与宿主不同，不能根据看不见
宿主 PID 或空锁列表断言原进程已退出。它只读取记录和现场，没有修改文件、删除锁或
重启进程。这验证了真实诊断/可见结果/原协调者交接，不代表自动运维修复或人工写入权移交。

实机发现并修复三类问题：

- 恢复 runtime 的较长路径超过 Unix socket 上限，导致 broker 启动失败。绑定和连接改用
  持有目录 fd 的短路径，socket 仍在任务目录；原有独占、身份检查和非阻塞连接保留。
- 上游在新可写会话创建时自动持久化项目 trust。现在用会话内的 `projects.<cwd>`
  配置明确禁用任务目录的项目配置加载，保留角色原有沙箱/权限；不写全局配置或复制认证。
- Herdr 关闭窗格后仍可能在恢复快照保留任务会话。新增显式 `native_tui.session_store`
  指向现有 `session.json`，记录存储目录身份；在终态停止进程前核验并登记实际保存的
  pane 映射，之后等待 Herdr 自己保存关闭状态，仅清除 v3 快照/备份中的本任务窗格及
  相应布局叶子。Agent 退出后的 shell 记录仍用原映射核验，不能只按 cwd 猜测归属。
  其他窗格、布局、文件权限和修改时间保留；格式、会话或文件身份变化则保留清理义务。

旧绑定没有配置 `session_store` 时保持兼容，但不能据此声称 Herdr 共享存储无残留。
当前只支持经核验的本机 v3 保存格式；不会猜测保存路径、整库删除或修改用户目录权限。

真实验收入口：

```sh
cargo test --locked --test lifecycle_authority \
  recovery_live::stopped_watcher_real_diagnosis_handoff_and_cleanup \
  -- --exact --ignored --nocapture
```

必须显式传入 `HERDR_UPSTREAM_SOCKET`、`HERDR_UPSTREAM_MODEL`、
`HERDR_UPSTREAM_ROLE_MODELS`（含 recovery）、`HERDR_NATIVE_BINARY`、
`HERDR_SESSION_STORE` 和只读校验用的 `HERDR_UPSTREAM_CONFIG`，并保留现有 Herdr 环境。
驱动器负责创建/故障注入和最终失败收尾，诊断与协调者回应来自真实模型；收尾不冒称业务成功交付。
观测预算只约束验收驱动器，不是产品任务时限。

早期尝试保留失败事实：broker 长路径启动失败、Herdr 纯文本被夹具误当 JSON、初步
104.98 秒通路通过后发现共享 trust/快照残留、保存目录权限与异步保存等待的夹具问题。
本轮新增 trust 条目已按精确路径移除并恢复配置原哈希；真实遗留快照用同一新回收逻辑
清理，两条任务会话以外的工作区/窗格数据逐项保持不变。未把早期失败补计为通过。

最终真实复验 **109.26 秒通过**：环境锁仍被持有，恢复角色完成一次诊断，原协调者
收到一次交接；Herdr 中可查询对应会话并读取同一 incident 的结果。原启动记录未改写，
恢复会话共一个 turn，协调会话共两个 turn；显式终态前队列没有业务终态。
驱动器随后选定最终失败，仅用于关闭本次验收任务，系统完成原终态的资源回收。

独立核查涵盖本轮全部尝试创建的 **9 个会话、6 个共享 SQLite 库中的 17 类关联**，
任务条目均为零；任务窗格、进程、worktree、两类根目录、登记、rollout、shell snapshot
及 Herdr 保存记录/快照/备份均无本任务残留。另按六个夹具路径核查 shell 状态记录，
没有只剩 cwd 的遗漏。五个原有窗格的 terminal/session 绑定、共享服务身份和受保护配置
哈希保持不变；原工作区、旧 watcher/队列与 Cargo 配置 stash 保留。

真实复验后审查补齐了 shell 快照候选筛选：没有 session 文本时仍按已登记的 pane 映射
精确回收，同 cwd 的其他 shell 保留。该差异不改变已验收的诊断/交接协议；现场最终扫描
已证明没有此类遗漏，并新增实际文件回收回归。其余收尾差异是 helper 提取与冗余借用修正。
完整 all-targets 覆盖率回归通过，最终 TUI/保存记录定向回归 **20 通过、2 显式忽略**；
格式、lint-policy、全 targets/features Clippy、release 构建通过。CRAP 绝对阈值 **10**、
无 baseline，**6138 个函数、零违规**；无覆盖数据的函数仍按零覆盖计入。
初次 CRAP 的目录扫描函数为 10.05，补充“同目录用户文件必须保留”的回归后通过，未放宽门槛。

临时夹具和原始诊断材料按任务生命周期清理，失败事实保留于本节及 Issue，不另建永久
任务证据库。PR #147 尚未合并/部署，远端检查在 PR 上按最终提交单独跟踪；下方保留
首版协议证据与未完成边界。

## 首版入口与协议证据（历史）

2026-10-27，基于已合入 main 的 #142、#144 和 #145，开发分支为
`feat/130-independent-recovery`。本页描述首版入口，#130/#112/#38/#32 继续开放。

## 配置与运行

在既有协调者 launch spec 的角色配置中增加 `recovery`，使用当前上游已有的
provider、模型和明确的推理强度。例如：

```json
{
  "role_models": {
    "developer": {"providerID": "EXISTING_PROVIDER", "id": "YOUR_DEVELOPER_MODEL"},
    "reviewer": {"providerID": "EXISTING_PROVIDER", "id": "YOUR_REVIEWER_MODEL"},
    "recovery": {"providerID": "EXISTING_PROVIDER", "id": "YOUR_RECOVERY_MODEL", "reasoning_effort": "high"}
  }
}
```

团队配置的 `roles.recovery` 由协调者按既有规则解析，Rust 不新增 TOML 自动加载器。
缺少该角色时报告 `not_configured`，不继承协调者模型、不换账户或计费路径。
Herdr 接入需要保留协调者已有的 `native_tui` 配置；恢复角色继承同一展示配置，
使用自己的会话和模型。没有配置 TUI 的协议夹具只能证明控制通路，不能证明用户可见验收。

`receiver-activate` 为已配置角色准备独立 observer、broker 和会话。准备会话不发送
模型工作 turn；首次真实异常才启动诊断。这样原 watcher/broker 停止，或者原任务
持有环境锁时，诊断仍有自己的入口，不必等待故障路径释放那把锁。

状态查询无需取得原环境锁：

```sh
herdr-flow task-runtime recovery-status --task TASK --state-dir COORDINATOR_RUNTIME
```

已有协调者也可显式执行 `recovery-start`；同一观察入口只保留一次启动意图，不能
用重复启动覆盖未明结果。协调者的 `herdr_queue inspect` 同时返回 `recovery` 状态。

## 观察、结果与接续

独立 observer 读取原有 watcher 健康状态、任务 runtime/turn 记录及现有上游线程。
观察不持有队列事务锁，也不把没有输出或经过某段时间当成业务失败。上游读取的
3 秒通信等待只表示本次观察可用性；watcher 心跳沿用既有健康检查语义。

worker 不再写入固定 `now + 600` 截止时间。上游 turn 为 `inProgress` 时保持
`pending`，旧记录里的 `deadline` 仅作历史字段。未知 turn 状态为 `unknown`；
读取失败单独保存 `observation_error`，不因此修改业务结果或重发原任务。

异常以原路径、进程/会话/turn 等身份去重，并保留明确的 `operation_replay_allowed=false`。
事件及去重标记在同一份原子记录中保存，避免中断留下只有事件、没有去重依据的窗口。
每次处理只启动一次诊断 turn，正常期间不持续调用模型。恢复角色读取真实现场，
返回绑定本次事件的结论：

- `recovered`：经观察确认已恢复，给出事实及下一步；不等于程序已经重放原操作。
- `needs_human`：明确需要的人、外部条件和下一步。
- `insufficient_evidence`：列出证据不足之处以及下一步核实动作。

结果必须包含事实、实际动作、当前状态和下一步。协调者原会话可接收输入时，
程序先持久化发送意图，再把结果交还原协调者。协调者忙或不可用时保留结果；
交接响应丢失时标为不明，不能再次发送来猜测是否成功。

恢复模型失败、结果缺失或恢复服务不可用时，保留原会话/尝试和明确的
`unavailable` 状态，不递归创建恢复 Agent。Herdr 恢复会话显示诊断；原协调窗格
标题提示当前恢复状态。所有展示通道也不可用时，状态查询和任务内记录仍保留，
但不能承诺实时通知。

## 首版边界与后续

本版先接通独立诊断和结果交接。恢复会话使用只读任务诊断权限，不取得开发者
工作区的写入权；TUI 也不能将该会话切换成工作区写入或其他权限配置。
需要修改进程、Git、代码或写入对象时明确交给人工/后续恢复执行步骤。
自动运维修复和人工写入权移交仍需按已确认方案完成，不能用这条诊断通路宣称全部故障恢复已完成。

以下仍由既有 Issue 跟踪：任务约定截止时间、等待期限接续、累计控制/turn 记录回收、
多 PR 阶段交付、发布 Agent、队列锁跨 Git 等外部操作的问题、人工处理后的受管复核，
以及恢复观察进程自身的外部监督。首版一次只推进一份恢复结论；未明恢复发送保留现场，
不自动换新会话绕过。#146 的异构 Codex/Claude Code 适配优先级仍在后面。

## 归属与验证

observer、恢复 broker、会话、Herdr 窗格、故障记录都属于同一任务 Owner。
最终成功/最终失败使用既有终态回收；等待人工和未知结果不提前销毁资源。
上游登录、provider、代理、订阅和计费不由 Flow 管理，不复制或删除用户配置。

定向协议回归使用实际注册的 observer、broker 和回收逻辑，但上游回复是合成数据：
原 watcher 停止且环境锁仍被持有时，独立恢复角色提交一次诊断、给出人工处理结果、
交还原协调者；重复观察不重复派发。另验证恢复模型失败不会递归创建 Agent。
回收断言覆盖两个会话、任务根目录与登记，独立服务及夹具共享配置保留。
这不代替真实模型/Herdr 恢复角色验收，也不补计历史失败。

本轮本地 all-targets 覆盖率回归通过；最后的原子去重调整另外通过 9 条恢复单元测试
和 2 条独立进程集成测试。格式、lint-policy、全 targets/features Clippy、release 构建
及 CRAP 绝对阈值 10（无 baseline、零违规）通过。既有 closing-owner 测试的
生命周期锁竞争已用确定性持锁断言及精确重试修复，见[回收竞争记录](cleanup-observation-races.md)。
历史 Git 10 秒超时仍未确认根因；本轮结果不将它追认为已修复。
