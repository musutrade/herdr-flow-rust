# 显式协调会话换班：最小实现契约

状态：2026-10-29 修订设计，尚未实现或实跑。依据
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
或未经登记的会话。`safe_rotation` 和 `context_telemetry` 仍为 false；显式换班的能力与触发方式单列，
不让调用者据 `safe_rotation` 推断已经支持自动阈值或活跃 turn 迁移。
保留当前 Codex/provider/model/effort/权限与已有服务、父 Herdr workspace。
不复制登录、不新建服务、不改变代理或全局配置。

## 现有权威及不可改写事实

| 现有位置 | 换班必须保留的含义 |
| --- | --- |
| `guard::scope` / `queue::ensure_receiver_scope` | task-owned scope 包含精确 session/runtime/instance/model。换班是经过核验的显式 scope 事务，不能放宽相等检查。 |
| `queue::ensure_delivery_session` / `ensure_ack_receiver` | 已发送通知只能由原 session ACK。必须先完成旧通知；历史 delivery 的收件人不改写，新换班通知绑定新 session。 |
| `receiver::recovery` | 当前 rebind 只允许 broker PID/start 改变并保留原会话；继续保持原契约，不借此偷偷替换 session。 |
| `receiver::control::bound_permit` | 只隔离 control scope；还须新增 launch/native/broker/TUI 准入 fence，不能声称该函数已经阻止所有原生写入。 |
| `retirement::selection::original_result` | 旧 collection receipt 包含原协调 runtime 路径。不能改写旧回执来适配新路径；须用同 Owner 下实际提交的前任/后继关系证明其来源。 |
| `recovery::observer_context` / `report::receiver_state` | Recovery 仍是原已配置角色；接收目的地改变须明确登记与核验，不能让旧 observer 持续向旧协调者回传或误诊计划内退出。 |
| `human::observe` 与 terminal finalizer | 旧 runtime 证据、仍在使用的 Recovery observer 和已退役执行者须区分；新旧会话、展示、进程均保持精确归属与最终回收义务。 |

## 记录与控制流程

下述新增字段、入口和规则都是实现要求，不是现有能力声明。父任务新增严格验证
的 `receiver_handoff`；根字段固定为 `version: 1`、完整 `owner`、`next_generation`、
`active_generation`（整数或 null）、按代号排序的 `records`。每个 Owner 最多
**8 代、8 条前任边**，取消也占一代；不截断旧记录。`next_generation` 必须等于
records 长度加一，取值 1..9；最多一个非终态请求。第 9 次请求在预约/创建前拒绝，
同 ID/同输入返回原事实，改变输入或新 ID 不能绕过未决请求或容量。

这是父任务的**可选扩展**。queue load 对缺少该字段或值为 null 的任务直接跳过
本扩展验证，包括未请求换班的父任务、activity、legacy 和 terminal 记录，不要求
升级或补空对象。非 null 时必须满足现有 progress::lineage::is_parent 判据：
schema_version 为整数 2 且存在 requirements 字段；再验证 requirements 是有效的
父需求对象及本扩展的 Owner/结构。activity/legacy 上的非 null 记录拒绝。已有
记录不能通过删除/改 null 绕过 fence；save 的前后比较须阻止这种退化。终态父任务
已有记录继续作历史结构验证，不要求原 runtime/observer 仍存活，也不触发新动作。
既有终态 finalizer 在 EX 下完成精确 cleanup obligation 后整体退役父任务的路径
仍允许；这与活动任务单独删除 receiver_handoff 字段区分，不能堵住最终回收。
新记录的 save 还核对更新前后父任务均未终结，首代 from 等于原队列的
guard_dir/receiver，并读取同 Owner 受保护 runtime、guard、receiver 和原 session
配置记录核验。load 不读取历史 runtime：活动父任务核对本代当前接收端与队列
相等，终态只检查保留历史。后代 from 必须等于最近已完成代的完整 to（包括
guard），取消不改变前任；不能将首代历史 from 与已经换班后的当前接收端混比。

每代固定字段如下；尚未产生的值为 null，不能靠缺省推断成功：

| 字段 | 类型与约束 |
| --- | --- |
| `generation`, `request_id`, `input_digest`, `reason`, `phase` | 连续代号、唯一 ID、规范化请求 SHA-256、1..1024 字节原因、下表阶段。 |
| `from`, `to` | 各含 runtime/guard/session/scope；from 为完整旧 guard scope；to.runtime/guard 创建前唯一预约，session/scope 仅由原创建 journal 补齐。路径限定同 Owner 临时根 rt 下的独占子目录。 |
| `configuration` | 冻结并验证 provider/model/effort/approval/sandbox/cwd/上游 endpoint/角色模型/driver，以及原业务配置快照 business_session_configuration；新 session 创建后须核对目标配置。它与当前 session_configuration 分开，不能用握手后的 never/readOnly 反推业务权限。 |
| `snapshot` | path/sha256/bytes；受保护交接正文最多 16 KiB，引用原需求和未决义务，不复制父历史，不作为路径授权。 |
| `fence` | generation/owner/old_runtime/new_runtime；runtime 侧 receiver-handoff.json 为镜像，父记录为权威。 |
| `binding` | null 或 transaction_id/from_scope/to_scope/committed_at；只随同一队列事务的 receiver/guard_dir 更新成立。 |
| `notices` | accept/ready 各含 wait/event/delivery/turn/phase，绑定同代和控制种类；未知发送不清空。 |
| `recovery` | 原来未配置则 null，否则 observer/origin_coordinator/from_target/to_target/phase；目标含 generation/runtime/scope/transaction_id。 |
| `retirement` | 同 Owner 专用计划的 path/sha256/phase；使用下文协调者 selector，不复用 development/acceptance selector。 |

原业务 approval 只接受现有 untrusted/on-request；sandbox 为原 workspaceWrite
策略，networkAccess=false，writableRoots 仅空数组（cwd 隐含）或原 checkout；
可选 tmp 排除字段须为布尔值。endpoint 的 path/dev/ino/uid 和显式角色模型分别
验证；后续执行还须复核实际 endpoint inode。retirement 的 path/sha256 从首次
登记即不可更改，阶段进展写独立执行记录，不把可变执行日志当成固定计划摘要。
Recovery origin 固定为首代 from.runtime，其 observer 保留在原路径；from_target
引用最近已完成代的 generation/transaction（首代为 0/null），取消不推进它。
to_target 必须使用本代 generation 和已提交 binding 的 transaction，complete
要求 Recovery 已对齐，并在同一队列事务登记 ready 事件。

task-owned guard 的 pane/terminal/socket 可以全部为 null：原生接收端本身不代表
Herdr 展示。若提供则三个字段都须有合法的非空标识/绝对 socket 路径。实际 TUI
回收仍核验独立 presentation 记录，不能用 guard 的 null scope 推断展示已回收。

整个字段最多 256 KiB，单代最多 32 KiB；大的精确资源清单置于同 Owner 受保护
journal（每份最多 1 MiB、128 个资源），以路径/摘要引用。独立验证器在 queue
load、任务更新、恢复入口、准入缓存加载和 ACK 前执行，拒绝未知字段/版本、越界、
重复 ID、Owner 不符、非法阶段及循环/断裂前任关系。提交比较原记录，禁止阶段倒退
和改写冻结身份；operations 的现有上限不能代替此验证。终态历史按保留 Owner 与
cleanup obligation 验证，不因活动环境映射消失而赋予新权。原 journals 留在原位。

| 阶段 | 必须成立的事实与动作 | 中断后处理 |
| --- | --- | --- |
| `requested` | 在 Owner EX 许可下核对原 scope，并持久化禁止新业务派发/新模型写入的 fence。已存在的审批响应、观察及原命令排空仍走原入口。 | 原请求继续观察；重复 request 不产生第二个意图。 |
| `draining` | 核对原 turn、tool/control journals、checkpoint、TUI 请求账本、后台命令、Git 锁、逻辑 writer、活动结果 ACK/apply，以及当前 delivery/Recovery/human/stage 状态。 | 未知保持在原请求；不强制打断或重放。允许在尚无新建/发送副作用时显式取消并按同身份解除 fence。 |
| `creating` | 冻结交接和配置，预约新 runtime 并复用 launch/bootstrap 的先记意图后创建；不调用会发送业务 turn 的 receiver-start。任何创建结果不明均保留原预约。 | 查询原创建记录和已知精确身份；无法确定时报告阻塞，不重复 thread/start 或换路径创建。 |
| `prepared` | 新会话、broker 和 receiver watcher 身份已证实且健康，还未运行业务。再次核验原静止证明及队列版本；旧业务仍受 fence 约束。 | 已知准备阶段只完成未提交的内部绑定；原始创建 journal 不重置。 |
| `bound` | 短队列事务提交 receiver/guard_dir、本代 binding 和唯一 accept 控制事件；新 guard 已 monitoring，独立业务 fence 仍关闭。需求/成果/验收/交付和旧通知不变。 | 依据队列 binding 完成 guard/observer 对齐；跨文件不一致不发送、不取得写权。 |
| `accepted` | 原 queue 协议收到新 session 对本代 accept delivery 的精确 ACK；还须核实接手 turn 已结束、工具响应结算和 checkpoint ready。 | ACK 已保存但 turn 未静止则保留；发送未知不重发，重复 ACK 不推动另一代。 |
| `retiring` | Recovery 已对齐且无未决 incident，按专用计划回收旧执行者，新业务仍关闭。 | 保留原义务，按原精确对象接续；父任务终态优先并吸收全部新旧资源。 |
| `complete` | 所需旧执行资源回收完成，Recovery 指向新 scope；保存完成事实和唯一 ready 控制事件。只准入已登记的 ready 恢复输入，普通新输入等其配置恢复日志证实后才开放。 | ready 发送/恢复未知则保留 fence，不重发；旧 runtime 永久拒绝新工作。 |

阶段只按表前进；draining 可在尚无创建/发送/关闭副作用、原 scope 未变时转为
cancelled，解除本代 fence 但保留代号。cancelled/complete 不能重新活动。

## Owner EX 与两层准入

显式 CLI 为 receiver-handoff-request/status/cancel。request/cancel 在
task_runtime::cli::owned_action 分流：用 runtime_observer_permit 解析 Owner 并
释放其 SH，再直接 `lifecycle::acquire(&owner, true)`。只对获取 EX 的 StateBusy
每 20 ms 重试，最多 2 秒；到期可见地返回 StateBusy/同请求重试提示，不提交、不
降级 SH。普通 watcher 每 tick 非阻塞尝试一次。获取 EX 前不得持 queue/guard/
observer/environment 锁；获得后按 environment、guard/observer、短 queue 事务
顺序获取，碰忙释放本步锁。不能在现有 SH 上升级，也不能用 mutation_permit
（实际是 SH）冒充 EX。EX 不跨模型等待或子进程启动；外部步骤先记意图，在 queue
锁外执行，再获取 EX 核对原操作并提交。启动子进程前释放 EX，内部创建准入只接受
该代预约路径/配置，不是公开 CLI bypass。被准入的原生写请求持 SH 到送入原连接。
EX 内的 guard/observer 锁和保存须新增接收已持 Permit 的内部入口，验证精确 Owner
后复用该许可；不能调用当前会隐式再取 SH 的 guard::locked/locked_existing/save，
也不能把失败降级为无锁写入。TUI bridge 若先解析再发送缓冲字节，许可必须覆盖
最终 socket 写入，不能仅覆盖 protocol::forward_permitted 的解析阶段；EX 等待
需核对这些已准入缓冲，不能在解析函数返回时误认为旧输入已经排空。

新增 fence 检查接入 launch/native create/dispatch/新 TUI、broker/TUI drain 的
每条原生请求，以及 control 业务动作；`bound_permit` 继续验证原 scope，不能单独
承担隔离。父记录与 runtime 镜像不符/半提交一律拒绝新业务。旧、新 receiver 都
禁止普通 turn/start、改线程配置和派工/发布；旧审批响应、interrupt、只读查询
及原操作 collect 走单列观察/排空入口，用 runtime_observer_permit 并核对原请求。
status、receiver watcher、Recovery 观察也使用观察许可，不能套禁止全部 RPC 的
stage marker 阻断排空，也不能因观察豁免开放新工作。新 TUI 仅展示/读取，旧 TUI
核实排空后关闭。普通 watcher 驱动有界步骤，不加常驻调度器，不调用 queue stop。

“原工作排空”限于已准入命令、原审批/工具响应、观察，以及 requested/draining
中的原结果 ACK/apply；它不授权新的 checkout claim，即使 claim 使用同一 Owner。
creating 的前置条件包括所有原活动结果已收集、ACK、apply，且不再需要任何新的
writer claim。若后来出现与静止证明矛盾的迟到结果，保留其原记录并阻塞，不在
creating 之后额外放行 apply 或重新申请 writer。基础切片仅提供这一保守准入；
后续驱动必须实现并检验静止证明，不能单凭 phase 标签认定此前提成立。

## 控制事件、投递与只读握手

复用 stage cleanup 的 host signal-wait/event 基础，新增控制种类
receiver_handoff_accept 和 receiver_handoff_ready。wait/event 的 control 字段
固定为 kind/owner/generation/transaction_id/target_scope，无业务结果 binding；
ID 由 Owner instance、generation、kind 确定。专用 host observation 核对 EX 与
父记录后创建；普通 signal/register、native result 和 task-apply 不得伪造/应用
此类事件，同名冲突不覆盖。消息明确“读取交接并 ACK，无需 task-apply，不扩权”，
不能使用默认业务结果模板。

control 是**附加对象**，不替换现有 wait/event 字段。wait 必须包含已有必填
id/task/epoch/owner/reason/condition/kind/timeout_seconds，其中 task=Owner.task_id
（父需求）、epoch=Owner.epoch、owner="coordinator"（现有文本角色）、kind="signal"、
timeout_seconds=604800；control.owner 才是完整 Owner 对象。event 仍由既有
observation_event/attach_observation 产生 id/wait/task/epoch/owner/outcome/summary/
evidence/observed_at/acknowledged 和 lifecycle_owner，再附加已验证 control；
task/epoch 必须继承同一父 wait，不能省略而被 isolation::closed 过滤。wait ID
按队列既有字符/长度限制编码，event ID/去重仍沿用原 observation 规则。

queue::dispatch 先处理原 active delivery，再筛选 pending：requested..prepared
不发新事件，bound 只选本代 accept，accepted/retiring 只观察原 accept，complete
优先 ready 再恢复普通事件。每个控制 delivery 只含一个事件，普通事件原样暂存。
新 guard 为 monitoring 加独立业务 fence，不能用 non-monitoring 屏蔽整个通道；
receiver_can_deliver/adapter readiness 须接受经验证的控制目的。delivery 保留
events/session/phase，追加 control 身份；session 必须等于
target_scope.codex_runtime.session（guard::scope 的实际嵌套结构）。
ACK 检查原 delivery.session、当前完整 scope、Owner、代号、wait/event 和 binding
事务后记录事实；原收件人、旧 ACK 和旧通知不改写，重复 ACK 不推进另一代。

新增内部 prompt_handoff(delivery)，不能将自由文本交普通 Adapter.prompt 后
假定它只读。accept 唯一 turn/start 显式使用 sandboxPolicy={type:readOnly,
networkAccess:false} 和 approvalPolicy=never；model/effort/服务保持原配置。
发送前保存精确输入摘要、session、代号、delivery、参数的 journal；broker 只准入
匹配该已登记意图的一次握手请求，TUI/普通 dispatch 无此授权，sending 未知不重发。
接手 turn 的动态工具仅允许 inspect 和本代 ACK，所有业务动作拒绝。

runtime 原权限与冻结的 business_session_configuration 不改写；但不能假设握手
覆盖不会改变原生 session 默认值。握手必须走现有 broker::execute_native →
record_native → subscription::completed/turn_configuration，不另走遗漏日志的
direct RPC，也不把已成功覆盖从 session_configuration 中隐藏。后者准确记录当前
已证实的 never/readOnly；独立的受保护 handoff 配置 journal 保存 generation/
delivery/session、before/expected 配置、精确 params 摘要、turn 和 sending/applied。
每次转换只更改登记的 approval/sandbox 字段，其余配置仍精确核对。

新连接的 subscription::prepare/rejoin 仍只做无覆盖 thread/resume：握手已 applied
时按当前只读 session_configuration 核验，不能与冻结的业务配置错比；恢复已
applied 后按原业务配置核验。配置 journal 与 session_configuration 部分提交时
只依据原成功回复/turn 记录完成对齐；不接受“原配置或临时配置任选一个”的宽松
比较。回复缺失、恢复状态未知或实际配置不符时，仅保留无需 resume 的查询与
诊断，阻止自动 resume、新 turn 和重放；不能用一次观察猜测发送成功。

accept turn 已结束且旧资源回收完成后，唯一 ready turn 通过同一 broker 管线，
**显式携带冻结的 approvalPolicy 和 sandboxPolicy**，以及原 model/effort；
不能依赖省略字段继承，因为当前默认值可能还是 never/readOnly。发送前保存
ready 配置恢复意图；成功回复由 subscription::completed 写回准确的
session_configuration 并将匹配本代/该 turn 的恢复 journal 标为 applied，随后
才开放普通新输入/TUI 写入。恢复回复未知保持原意图与 fence，不新建恢复 turn。
receiver::active::Adapter.prompt 与 native::dispatch_codex 对后续业务 turn 都
须显式使用原 approvalPolicy/sandboxPolicy，不继续沿用握手覆盖。ready 自身只
ACK、不 task-apply，接续行为仍限原授权；不得在旧只读 turn 中途放开业务工具。

## Recovery 接续与共同解析器

当前 watch 循环外缓存 context、activate 总是 start，必须实际修改。context 移到
每 tick 的 Owner SH + 原 observer 锁内重读，校验固定 owner/observer/origin_coordinator，
再读取新增 receiver_target（generation/coordinator/scope/transaction_id）。同 tick
不得混用目标。原 context.runtime 角色配置和 Recovery agent/session/journals 保留；
observe/repair/report 中的 coordinator 引用均使用该 tick 已验证的 target。

bound 后在 EX + 原 observer 锁下确认无进行中或发送未知 incident，再把 target
从本代 from_target 改为匹配 binding 的 to_target。部分提交时报告 handoff_pending，
暂停目标诊断/发送，不把计划内旧 watcher 退出误判事故；真实未知事实不清空。
每 tick 重读使原 observer 无需停止/重建，不制造 watcher_unavailable。

新 receiver activate 增加内部 RecoveryBinding::Existing，核对父记录中原 observer/
Owner/session/进程后登记引用，不调用 recovery::start；原来未配置则保持未配置。
普通 recovery-start 也先经 resolver，已有角色时拒绝创建第二份；原 observer
不健康不能用新建角色掩盖。resolver 由当前 Owner 的已提交前任链解析原 observer，
核对 origin、target、scope，不按路径扫描猜测。recovery-status、report::receiver_state、
human::observe/return、stage 回收检查和 terminal finalizer 全部用它；历史 root
只读 status 使用 observer permit。finalizer 按 observer/session/PID 精确去重并
最终回收，早期退役不删除仍使用的原 observer。

## 旧协调者退役与历史回执

新增 coordinator retirement plan/selector，只从本代实际 binding.from 选择；
不放宽 stage selector 对 receiver.json 的拒绝。计划固定 Owner/代号/binding 事务、
旧 runtime/guard 快照、broker/receiver watcher PID/start、TUI/前端精确身份、
upstream session journal 和保留清单。receiver-retirement.json 标记只约束同代
旧执行者，不用 stage marker 或递归删除旧 runtime 代替。

顺序：复核原请求/TUI 账本与 session idle → 捕获/关闭旧 TUI 并核实前端退出 →
在 Recovery target 已对齐后按 PID/start 停旧 receiver watcher → 停旧 broker →
按原 upstream journal 删除旧 coordinator session 及精确存储关联 → 保存完成。
每步先保存义务、操作后观察，重复只针对原对象。保留旧 runtime/control/creation/
collection 证据、前任链、原 recovery-observer 目录及其全部进程/session、其他角色、
checkout 和共享服务。归属或删除结果未知则保留 retiring，不按名称/目录猜测。
terminal finalizer 合并本计划与 upstream/stage/TUI 义务、按身份去重，并覆盖预约
未绑定资源；只有终态才删除仍引用的历史目录和 observer。

retirement::selection::original_result 调用 predecessor resolver：先原样验证
receipt Owner/runtime/wait/binding；receipt.coordinator 等于当前 runtime 时沿用
原校验，否则从当前完整 queue scope 沿已提交且 complete 的记录逆行最多 8 步。
每边同 Owner，to.scope 等于当前节点，from/to 身份与 binding 一致，代号递减且
无循环；命中 receipt.coordinator 后仍验证原协调身份、原结果 ACK/applied 和 child
binding。取消/未提交/未 complete 的边不提供历史来源证明，断链/跨 Owner/镜像
缺失不得变成路径白名单。不改写 collection-receipt.json 或替换旧 coordinator 路径。

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
6. 控制事件不能伪造/混批/作为业务结果应用；accept 在原生参数与动态工具两层
   只读；直接 broker/CLI/TUI 写入拒绝而原审批与观察可用。EX 两秒有界返回，等待
   不持 queue 锁，其他 Owner 仍可进展。第 9 次请求无副作用拒绝。
7. 同一 Recovery agent/session/observer PID 经 retarget 后继续工作；在 binding/
   observer 半提交、ACK 但 turn 未结束、旧 watcher 停止等边界不误开业务或重复
   创建 Recovery。合法前任回执可回收，字节不变；伪造/断链/跨 Owner 均拒绝。
8. 覆盖握手后断开 broker → 按临时只读配置 resume → ready 显式恢复原 approval/
   sandbox → 再次断开/resume → 业务 turn 的回归；配置变更、恢复未知或 journal
   半提交均不放宽校验/重放。控制 wait/event 完整保留父 task；无该可选字段的
   parent/activity/legacy/terminal 队列照常加载，非法非 null 及删除原 fence 拒绝。

这份设计不以一次通过证明自动阈值换班、活跃业务迁移、任意累计次数或全部故障。
父需求历史跨容量接续、发布积压集成证据及 #146 仍按原独立范围推进。

## 首个实现切片的边界

当前实现先提供父记录的可选扩展校验、容量/前任链/单调更新约束，以及现有 native、
control 和 checkout 准入中的保守隔离。仅有换班意图、complete 阶段文字或 ready
ACK 均不授予新输入权限；在完整配置恢复驱动具备证据前只保留原请求排空和观察。
首个基础切片未包含新换班 CLI、后继创建、控制通知、Recovery retarget、配置恢复
及协调者退役驱动；safe_rotation/context_telemetry 保持 false。

原生 TUI 的生命周期 SH 许可现在覆盖代理中已准入但尚未写完的缓冲字节；全部
送出且 HTTP/WebSocket 解析器无残留后，或连接清理同时丢弃解析/输出缓冲后才
释放。半个握手、半帧和未完成碎片消息均持有许可。连接丢弃不会清除原请求账本中的未知结果，因此
获得 EX 不等于原生执行已结束，回收仍需原账本和静止观察。协议夹具覆盖这些
边界；本切片不包含新的真实模型/Herdr 换班验收。

输入保留没有时间上限：若 peer 不再读取，或一直不补齐分段消息，SH 会继续保持；
申请换班的 EX 在两秒上限后报告 StateBusy。此处选择保留原输入，不设自动丢弃
计时器。操作者可按原精确 presentation 关闭该 TUI 连接来丢弃缓冲；这只释放
传输许可，原已登记请求的未知执行结果仍须核实，不能以关闭或超时代替完成证明。

## 请求入口开发检查点

后续开发已加入 receiver-handoff-request/status/cancel。request 的 spec 仅含
request_id/reason，cancel 仅含 request_id。请求在精确 Owner EX 下分别保存
requested 与 draining，冻结配置并预约唯一后继路径；同 ID 改输入拒绝，同 ID
同输入返回已有历史。EX 每 20 ms 重试、最多两秒，guard 锁借用已持 EX，不再
隐式申请 SH。原 scope/父状态变化、其他未结束 Owner、创建路径已经存在等情况
拒绝推进。取消保留代号，第九代无新资源副作用地拒绝。

这个检查点只完成请求和取消，目前停在 draining，不自动创建后继。独立观察、
fence 镜像对齐、控制通知、权限恢复、Recovery retarget 和协调者退役仍待实现。
状态与 CLI 帮助明确此边界。真实隔离队列/Git 工作区及本地 Unix 端点夹具验证
请求幂等、取消、容量和 EX/CAS；它们没有执行原生模型 turn，也不是完整换班验收。
