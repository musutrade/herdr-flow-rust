# 独立故障观察与诊断入口

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
