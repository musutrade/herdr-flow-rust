# 显式协调会话换班：最小实现契约

状态：2026-10-29 设计候选，尚未实现或实跑。依据
[#112](https://github.com/musutrade/herdr-flow-rust/issues/112) 与
[#130](https://github.com/musutrade/herdr-flow-rust/issues/130) 的当前范围，以及
[有效范围对账](../end-to-end-readiness.md#current-execution-order-2026-10-29)。
实现前需独立核对以下事务和现有接口的接点；不能仅将能力标志改为 true。

## 首版交付范围

在一个 fresh format-3、上游 Codex、queue-tool-v2 的任务绑定接收端上，操作者
显式请求换班。原需求、Owner（queue/task/epoch/instance）、checkout、授权、
候选和独立验收事实保持；新的协调会话接收持久交接并在原队列继续工作。
首版要求原协调者及已派发活动处于已核实的静止边界，发布、人工交接、恢复处理、
阶段回收和已发送通知均已结算。无法观察或未知操作保持原记录，不以等待超时代替结算。

这不是自动上下文阈值换班，也不接管旧格式任务、任意共享接收队列、活跃业务 turn
或未经登记的会话。`context_telemetry` 仍为 false；显式换班的能力与触发方式单列，
不让调用者据 `safe_rotation` 推断已经支持自动阈值或活跃 turn 迁移。
保留当前 Codex/provider/model/effort/权限与已有服务、父 Herdr workspace。
不复制登录、不新建服务、不改变代理或全局配置。

## 现有权威及不可改写事实

| 现有位置 | 换班必须保留的含义 |
| --- | --- |
| `guard::scope` / `queue::ensure_receiver_scope` | task-owned scope 包含精确 session/runtime/instance/model。换班是经过核验的显式 scope 事务，不能放宽相等检查。 |
| `queue::ensure_delivery_session` / `ensure_ack_receiver` | 已发送通知只能由原 session ACK。必须先完成旧通知；历史 delivery 的收件人不改写，新换班通知绑定新 session。 |
| `receiver::recovery` | 当前 rebind 只允许 broker PID/start 改变并保留原会话；继续保持原契约，不借此偷偷替换 session。 |
| `receiver::control::bound_permit` | 工具调用须来自当前 scope 和 Owner；旧协调者在交接后不能通过旧 broker、直接 CLI 或 TUI 再派工。 |
| `retirement::selection::original_result` | 旧 collection receipt 包含原协调 runtime 路径。不能改写旧回执来适配新路径；须用同 Owner 下实际提交的前任/后继关系证明其来源。 |
| `recovery::observer_context` / `report::receiver_state` | Recovery 仍是原已配置角色；接收目的地改变须明确登记与核验，不能让旧 observer 持续向旧协调者回传或误诊计划内退出。 |
| `human::observe` 与 terminal finalizer | 旧 runtime 证据、仍在使用的 Recovery observer 和已退役执行者须区分；新旧会话、展示、进程均保持精确归属与最终回收义务。 |

## 记录与控制流程

只扩展既有受保护队列、runtime journals 和普通 queue watcher。父需求保存一份
当前 `receiver_handoff` 意图，包含请求 ID、原因、完整 Owner、旧 receiver/guard/
runtime/session 身份、冻结配置、交接内容摘要、新 runtime 预约、阶段与未决义务。
同请求 ID、同输入重复调用只返回原状态；改变输入或新 ID 不能绕开未决换班。
运行时目录和内容摘要是操作引用，不是独立授权。

新 runtime 使用同任务临时根内的独占子目录。原 runtime/control/collection
记录留在原位置。需要保留的前任关系记录于既有任务状态中，具有受控容量、精确
Owner 与实际绑定事务证明；不是任意路径白名单。容量满时在产生新副作用前明确
拒绝，不无限积累会话或借新 task ID 绕过。

| 阶段 | 必须成立的事实与动作 | 中断后处理 |
| --- | --- | --- |
| `requested` | 在 Owner EX 许可下核对原 scope，并持久化禁止新业务派发/新模型写入的 fence。已存在的审批响应、观察及原命令排空仍走原入口。 | 原请求继续观察；重复 request 不产生第二个意图。 |
| `draining` | 核对原 turn、tool/control journals、checkpoint、TUI 请求账本、后台命令、Git 锁、逻辑 writer、活动结果 ACK/apply，以及当前 delivery/Recovery/human/stage 状态。 | 未知保持在原请求；不强制打断或重放。允许在尚无新建/发送副作用时显式取消并按同身份解除 fence。 |
| `creating` | 冻结有界交接内容，预约新 runtime 并复用 launch/bootstrap 的先记意图后创建。会话必须匹配配置；任何创建结果不明均保留原预约。 | 查询原创建记录和已知精确身份；无法确定时报告阻塞，不重复 thread/start 或换路径创建。 |
| `prepared` | 新会话、broker 和 receiver watcher 身份已证实且健康，还未运行业务。再次核验原静止证明及队列版本；旧业务仍受 fence 约束。 | 已知准备阶段只完成未提交的内部绑定；原始创建 journal 不重置。 |
| `bound` | 一次短队列事务提交新 guard/scope、精确前任关系及唯一换班通知；保持所有需求/成果/验收/交付字段和旧通知原文。新会话只允许 inspect/ACK 交接，业务准入仍关闭。 | 只能依据已保存事务完成 guard/observer 对齐。各读写入口在跨文件部分提交时拒绝不匹配，不推断成功。 |
| `accepted` | 通过原 queue delivery 协议收到新 session 对该换班通知的精确 ACK。记录已接手，业务准入继续保持关闭直至旧执行资源回收完成。 | 发送/ACK 不确定沿用原 delivery 的观察与恢复，不重发模型输入；重复 ACK 不重复应用换班。 |
| `retiring` / `complete` | 复用精确进程/TUI/原生会话退出与删除能力，回收旧协调执行者；仍被引用的记录和原 Recovery observer 保留到其归属终态。新业务只在要求的回收义务完成后接续。 | 清理错误保留原换班/回收义务；不撤销事实、不产生另一后继。父任务终态优先并吸收全部新旧资源义务。 |

所有外部创建、发送、关闭和删除均在队列锁外执行；先保存原意图，后按精确对象
观察结果。Owner EX 与原生/工具准入共用生命周期协议，不能在已有 SH 上升级锁。
普通 queue watcher 驱动有界步骤，不再添加轮询 Agent 或另一个常驻调度器。

`requested` 到 `complete` 期间，受影响接收端暂存普通业务事件；只有精确绑定的
换班及回收完成通知走原通知通道。接手 turn 使用只读沙箱，只允许 inspect/ACK，
完成通知后才恢复原配置允许的业务 turn，不借握手开放任意新写入。不重复投递，
不调用 `queue stop`。其他独立 Owner 的执行不得因持有全队列长锁而
停止，但首版不承诺可替换还承担其他未完成 Owner 的共享接收端。

Recovery 的原模型会话、未决事实和去重记录不复制或清空。只有不存在进行中或
发送结果不明的 incident，才允许在其既有锁下将 observer 的当前接收目标从旧
runtime 改为已提交的后继。observer 的原路径、Owner、会话和进程身份保留；
新 receiver 的 status/human/finalizer 通过已核验的登记关系找到它。接收端绑定与
observer 对齐不是跨文件原子写，持久意图必须支持准确完成中断前缀。期间暂停
对计划内退出的误报，真实未知状态仍保留；不得另建恢复角色来消除不匹配。

## 最小完成证据

1. 静止任务完成一次实际旧会话 → 新会话接替：同 Owner/需求/候选/授权，明确不同
   session、新 Herdr tab，通知由新 session ACK，随后真实任务结果正常 ACK/apply。
2. 旧 scope、旧 session、错误 Owner、旧/错误 ACK、活动 tool/turn、未决发布或
   Recovery、未排空 TUI 及未应用结果均不能取得新业务写权。旧回执保持字节不变，
   同 Owner 的已登记前任仍可供阶段回收核验，伪造路径/跨 Owner 前任被拒绝。
3. 在创建预约、原生回复、scope 绑定、换班通知、ACK 和旧资源回收边界中断后，
   用原操作接续或明确报告未知；不重复创建、发送、应用或发布。生命周期终态与
   换班竞争时不复活旧 writer，也不遗漏新建但尚未绑定的任务资源。
4. 旧会话及最终新会话的进程、Herdr 展示、原生及共享存储关联按既有精确身份回收；
   原需求、不可变验收、已确认交付和用户共享资产保持。失败/清理受阻分别记录。
5. 本地针对性回归、现行完整质量检查、固定 head 独立评审和 CI 分别留存。
   真实模型/Herdr 验收须使用新私有任务与冻结二进制，和协议夹具分开报告。

这份设计不以一次通过证明自动阈值换班、活跃业务迁移、任意累计次数或全部故障。
父需求历史跨容量接续、发布积压集成证据及 #146 仍按原独立范围推进。
