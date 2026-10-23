# Herdr 工作流边界与最小验收

2026-10-23 源码审计基线：`55c6ba073400dff1a1840fd9f82515f6a00412f3`。
本节记录证据分类和后续执行条件，不表示新增模型验收已经通过。

## 职责与现有能力

Herdr 管理 agent/session 通信、终端和 worktree 生命周期。herdr-flow 复用
guard、queue delivery journal、ACK、task-apply、成果验收、交付及 CI 观察。
不新增调度器、数据库、独立 controller、进程监督器或平行 ACK 事务系统。
生产程序仍为 Rust，无 Python 运行时依赖。

| 入口 | 已有能力 | 使用条件或缺口 |
| --- | --- | --- |
| Herdr agent/worktree CLI | 提供启动、身份查询、消息、读取、生命周期观察及 worktree 管理 API | 本次只观察现有 agent 通信与 worktree list/query，未验收 worktree 完整生命周期；prompt 成功仅表示终端接纳，wait 只观察 agent 生命周期 |
| Herdr-managed Codex receiver | 现有 Herdr adapter 绑定 pane、原生 TUI bridge 与 thread；通过原生 turn/start 取得 turn id | 必须有匹配的 bridge/guard；watcher 必须处于匹配的 Herdr env/socket；在 Herdr 可见本身不等于 queue ready |
| 原生 OpenCode receiver | HTTP session adapter、独立 watcher、队列 ACK/application | 不等于 Herdr-managed OpenCode receiver；当前 factory 不通过 Herdr CLI 绑定它 |
| queue/task 命令 | 登记结果、持久派发意图、接手、signal、ACK、task-apply | 复用已有身份、锁和状态门禁；不把终端接纳伪装为 turn id、ACK 或完成 |
| task-owned Codex bootstrap | 创建一次线程并记录身份，明确拒绝 ready | 保持 inactive；不作为当前工作流的接收端，也不继续补独立 controller |

当前宿主观察为 Herdr client 0.9.5 / server 0.9.4、protocol 22。历史混合原生
运行固定 Codex 0.163.0 / OpenCode 1.19.4；这些版本记录不要求升级。

## 历史证据如何解读

| 测试或协作 | 实际拓扑 | 证据限度 |
| --- | --- | --- |
| `tests/common/receiver_live.rs` OpenCode 原生场景 | 后台 serve + create_fresh；原生 coordinator/worker/reviewer | 后期真实 coordinator 自己 ACK/task-apply；测试驱动启动、收集、清理，并执行 Local 交付。没有 Herdr pane agent |
| 同文件 mixed Codex worker 场景、PR #103 | task-owned app-server worker + 原生 OpenCode coordinator/reviewer | 原生模型与工作流链路，不能称为 Herdr agent 全链路或 Codex coordinator 验收 |
| `tests/task_runtime.rs` fresh-session 场景 | 原生子进程，协调端为协议 fixture | 会话身份/回收证据；不是 Herdr 接收、ACK/application 证据 |
| `tests/herdr_adapter.rs` | fake Herdr CLI 与 RPC fixture | 适配器协议及错误边界 |
| `tests/installed_herdr.rs` | ignored/opt-in 的真实 Herdr pane/process 只读测试 | 测试范围为身份绑定及错误拒绝，未启动任务或验证投递/ACK；本轮未运行，仅检查源码，历史执行证据须另有记录 |
| 开发者与发布者的现有 Herdr 会话 | 实际可见 agent，相互主动交接与通知 | 开发/发布协作证据；不能替代测试角色的完整验收 |

早期 fixture 中由测试驱动执行的 ACK/application 应继续按原记录标注，不能
追认成后期真实 coordinator 的证据。审批、结果 ACK、任务应用和交付分别核验；
agent idle、普通命令成功或日志不存在都不能代替这些证据。

审计涉及的路线包括 #33 TaskEnvironment、#35 task-owned runtime、#44 broker、
#98 原生结果回收、#103 启动适配、#104–#109 接入/清理及文档、#110 inactive
bootstrap。已验证的启动、安全和回收修复保留；后续 socket/stdio/controller
调查没有产品 PR，不继续扩展。不能把这些原生实现整体宣称为 Herdr 集成。

## 下一次最小验收步骤

1. **固定角色和实际身份**：从现有 Herdr agent 查询开始，记录任务角色、pane、
   exact agent_session、cwd/worktree、版本及候选 SHA。需要新角色时使用已安装
   Herdr CLI 的受支持入口，不在后台另起原生服务来代替可见角色。
2. **先核验现有接收端条件**：按 [context rotation](context-rotation.md) 核对
   Codex bridge/guard/thread 与 watcher 环境，再按 [tutorial](tutorial.md)
   检查 queue readiness。缺 bridge 的普通 TUI 仍可正常 Herdr 通信，但不能
   宣称具备自动 queue 唤醒；不复制 marker、不借用其他 session，不为此重启
   当前写入中的 agent。迁移必须先完成工作、交接并符合既有静止条件。
3. **只执行一个既定任务的结果回传**：接收端就绪后，复用已有结果 wait 和
   [task progress](task-progress.md) 的派发/接手/结果协议。记录发送前意图和
   实际返回的证据；Herdr prompt 的接纳不填作 native turn id。不确定发送保留
   原状态，禁止重发。普通双 agent 的 Herdr 交接可以独立进行，不冒充 queue 验收。
4. **由真实接收角色 ACK 与应用**：关联 queue、delivery、session、task/assignment
   和候选；先 ACK，再核验并 task-apply。测试驱动代操作必须明确标为 fixture。
   出错保留状态并停止，不增加新的自动 ACK 恢复或重放机制。
5. **独立观察结果**：记录角色在 Herdr 中的实际身份、传输、ACK、应用和交付
   各自的证据。最小结果回传不代表 PR/CI/合并已验证；发布继续由现有发布者
   按精确提交的交接完成，结果必须主动通知，开发者核验后 ACK。

先完成接收端条件核验再决定最小改动。若选择 Herdr-managed OpenCode receiver，
现有原生 HTTP 接入不能充当证明；必须单独确认所需身份与接纳语义是否可满足，
不以新建后台协调器填补缺口。此次文档修订不启动模型、watcher 或新会话。

### 当前会话限制

本次观察的 Codex TUI 使用 `--no-daemon`，没有匹配的 app-server bridge，不能
作为现有 queue receiver。普通 Herdr 消息往返仍可用。仓库已有 `herdr-flow tui`
启动及绑定说明，缺少的是本次宿主上的兼容 bridge/thread 绑定验证，不是完全
没有启动入口。要使用该 receiver，需要另行建立兼容的 bridged 会话，或在工作
结束、交接后按既有迁移条件处理；不能直接借用另一个 daemon 或不同版本线程。

### 条件满足后的命令顺序（示例，未执行）

以下使用 Herdr-managed Codex coordinator 和 Codex worker；`producer_session`
绑定 Codex UUID，不把 OpenCode session 填入该字段。先按已有教程准备 spec，
用实测值替换所有 `REPLACE_...`，状态目录放在 checkout 外。每段分别在所注明的
角色内执行，检查返回结果后再进入下一段；不是可整段运行的安装脚本。

接收端已有匹配的 bridge、thread、rollout 后，从匹配的 Herdr 环境注册并观察：

```bash
herdr-flow guard init --state-dir /absolute/guard --pane REPLACE_PANE \
  --session REPLACE_SESSION --transcript /absolute/rollout.jsonl \
  --sessions-dir /absolute/sessions --task REPLACE_TASK --epoch 1 \
  --role coordinator --app-server-socket /absolute/app-server.sock \
  --codex-binary /absolute/matching/codex
herdr-flow guard watch --state-dir /absolute/guard --background
herdr-flow queue init --state-dir /absolute/queue --guard-state-dir /absolute/guard --poll-seconds 30
herdr-flow queue enable --state-dir /absolute/queue
herdr-flow queue watch --state-dir /absolute/queue --background
# 从 exact coordinator session 检查；不通过就停止。
herdr-flow queue readiness --state-dir /absolute/queue
```

初始 wait/task 先用 `queue add --spec` / `queue task-add --spec` 登记，字段见
[tutorial](tutorial.md#4-register-one-result-wait-and-task-before-dispatch)。以下
`task-begin` 段仅用于已经由 `task-apply` 保存的 **prepared 后继 dispatch intent**，
不能对刚 task-add 的初始任务凭空 begin。每份 spec 使用当时的 state_version。

```bash
herdr-flow queue task-begin --state-dir /absolute/queue --spec /absolute/begin.json
herdr agent get REPLACE_WORKER
herdr agent prompt REPLACE_WORKER 'REPLACE_EXACT_ASSIGNMENT_AND_RETURN_INSTRUCTIONS'
# 仅在明确收到接纳返回、且 operation 仍为 sending 时记录；response 如实写终端接纳。
herdr-flow queue task-sent --state-dir /absolute/queue --spec /absolute/sent.json
```

`task-sent` 是可选的传输观察，不证明接手；若 worker 已接手或状态版本改变，
检查当前状态，不为补写记录再次发送。无返回、超时或发送状态不明时同样禁止重发。

worker 在 exact producer session 接手后工作并回传：

```bash
# 针对上述后继 operation：接手，不是成果验收。
herdr-flow queue task-accept --state-dir /absolute/queue --spec /absolute/takeover.json
# 完成实际任务后，选择真实 outcome 并提供存在的证据。
herdr-flow queue signal --state-dir /absolute/queue --wait REPLACE_WAIT \
  --outcome complete --summary 'REPLACE_FACTUAL_RESULT' --evidence /absolute/result.json
```

真实 coordinator 收到对应 delivery 后读取证据，再分别记录收件和应用：

```bash
herdr-flow queue ack --state-dir /absolute/queue --delivery REPLACE_DELIVERY_ID
herdr-flow queue task-apply --state-dir /absolute/queue --spec /absolute/application.json
```

`task-accept` 只确认派发接手；通常的结果路径是 **signal → ACK → task-apply**。
ACK 不等于验收通过，task-apply 必须按原要求和实际证据决定后继动作。

## 安全与平台范围

- 不修改全局安全、代理或 Codex 配置，不切 full access；不重发不确定 dispatch。
- 既有 task-owned 隔离主要约束文件写入，不能扩称为机密读取或网络隔离。
- Herdr CLI 是复用的平台接口；herdr-flow 当前 guard、锁、进程观察及 task_env
  仍有 Unix/Linux 依赖。Linux 宿主通过不代表跨平台验收通过。
- #38/#32 保持开放；inactive bootstrap、协议 fixture 与后台原生测试都不能
  填补 Herdr 管理角色的实际验收缺口。
