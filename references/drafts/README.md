# 已确认流程与讨论记录

2026-10-27：本目录保存调研、方案和验证历史。最新流程方向已确认，新增能力仍待开发。
阅读时以各文档顶部的最新说明为准；下方旧建议、待确认项和失败记录保留为历史，
不能覆盖已确认方向，也不能把方案提交或旧测试通过当成新能力已经实现。

| 文档 | 用途 |
| --- | --- |
| [流程与故障恢复方案](agent-reliability-proposal.md) | 五类角色、长任务、多 PR、异常处理、人工交接和回收的最新职责；后附历史设计 |
| [调研备忘](agent-reliability-research.md) | 源码评估、600 秒与累计容量限制的纠正、开源参考及适用边界 |
| [Python/Rust 对照与验证](herdr-python-rust-verification.md) | 原生 TUI 路线、既有实现对照、验证结果及失败历史 |

Issue 已按同一流程置顶更新：

- [#32：总流程与开发顺序](https://github.com/musutrade/herdr-flow-rust/issues/32)。
- [#130：恢复 Agent、长任务、阶段交付与统一回收](https://github.com/musutrade/herdr-flow-rust/issues/130)。
- [#112：Herdr、人工介入与发布接续](https://github.com/musutrade/herdr-flow-rust/issues/112)。
- [#38：必要验收与资源回收](https://github.com/musutrade/herdr-flow-rust/issues/38)。

已完成的开发分支成果由 [PR #142](https://github.com/musutrade/herdr-flow-rust/pull/142)
和 [PR #144](https://github.com/musutrade/herdr-flow-rust/pull/144) 跟踪；当前未合并或部署。
原生 TUI 配置及最新证据见[接入说明](../native-tui.md)。本目录只补文档，
不修改登录、provider、代理、计费、全局入口、旧 watcher 或旧队列。

文中本机绝对路径仅定位当时的源码或证据，不是可移植配置，也不承诺临时目录仍存在。
已经回收的测试资源不因保留这份历史说明而重新创建。
