# light P0：实施基线、数据根与旧路径边界

本文件落实 [#170](https://github.com/musutrade/herdr-flow-rust/issues/170)，范围以 [#169](https://github.com/musutrade/herdr-flow-rust/issues/169) 为准。2026-10-30 核对；仅提交决策与核对结论，不包含原始对话、终端日志或现场交接材料。P0 不改程序、不建库、不安装或修改 skill、不切换或清理现有任务。

## 基线与事实口径

| 对象 | 核对结果与依据 | 限制 |
| --- | --- | --- |
| 实施源码 | `git fetch origin main` 后，`git rev-parse origin/main` 与 `git ls-remote origin refs/heads/main` 均为 `784891d76c9345507e710fd2b8c537ec657dbb0e`；[固定提交](https://github.com/musutrade/herdr-flow-rust/commit/784891d76c9345507e710fd2b8c537ec657dbb0e) | 后续实施重新核对，不把本次快照视为永久 main |
| 开发隔离 | 从上述提交建立独有分支 `light-170-p0-20261030`，worktree `/home/gem/herdr-light-170-dev` | 共享 checkout 保持原状；不是安装来源 |
| 核准方案 | 只读方案 `docs/herdr-light-refactor-plan.md` 的 SHA-256 为 `82556ad2feddc7c3beef182f3dafb6d6b4f4c3cb2113d45f4bd13d0626534927`，与 #169 一致 | 该原始文件未入库；本文件独立整理决策，不提供不存在的远端方案链接 |
| 安装二进制 | `command -v` 与 `readlink -f` 均指向 `/home/gem/.local/bin/herdr-flow`；`--version` 为 `herdr-flow 1.0.0`；SHA-256 为 `5c184bd5a069a7a21fed37deeb3e6804b9eef85d911ddd8624afbdb8d086d43e` | 构建提交、构建参数、安装命令未核实；版本、哈希和 mtime 不能单独证明源码来源 |
| 旧 skill 安装 | Codex 的 `/home/gem/.codex/skills/herdr-issue-flow` 是指向共享 checkout 的链接；Hermes 的 `/home/gem/.hermes/skills/claude-code-imports/herdr-issue-flow` 是普通目录 | 未证明二者内容一致或各会话已读取哪个版本；未修改安装 |
| 当前团队契约 | 本批明确使用 `herdr-team-flow`，开发、审计、发布由现有 Agent 承担；唯一通信测试已收到相同 batch/head 的 RESULT→ACK | 仅证明这次通信与结果核对；不证明 light 接入、恢复或后续发布成功 |
| light 实现 | 固定基线的 [Cargo.toml](../Cargo.toml) 无 SQLite 驱动，[main.rs](../src/main.rs) 无 light 入口 | 本文接口与验收为待实现要求，不是可用命令声明 |

本机结论只适用于本次检查。未盘点其他项目、运行会话的环境变量或全部旧回收目录；没有把旧 Issue 关闭解释为任务已排空。P1 增加 SQLite 驱动时另记实际解析版本、锁文件、编译依赖及依赖审计结果。

## 产品与依赖边界

同一 Rust 二进制增加独立 `herdr-flow light` 路由；首版 SQLite + CLI，无必需常驻进程。Agent/用户报告事实，现有 skill 负责开发、独立审计、写入权交接、PR/CI、合并和清理。记录成功不授予执行权，记录失败也不阻断协作。RESULT 的 ACK 仅表示核对约定结果和证据，不新增权限、不代替完整验收、不以写库为前提。

| 源码 | 处理 | 约束 |
| --- | --- | --- |
| [main.rs](../src/main.rs)、[lib.rs](../src/lib.rs) | 增加独立入口 | 帮助区分记录查询与旧控制命令 |
| [util/process.rs](../src/util/process.rs)、[error.rs](../src/error.rs)、[logging.rs](../src/logging.rs) | 选择性复用叶子工具 | 参数数组、超时、输出限制与脱敏；检查间接调用 |
| [util/fs.rs](../src/util/fs.rs) | 按函数抽取或复用 | 仅无旧格式语义的路径、权限、锁工具；事务与备份用 SQLite 接口 |
| [herdr.rs](../src/herdr.rs)、[identity.rs](../src/identity.rs) | 抽薄只读查询边界 | 不沿用 Codex-only 运行时校验；session 可空，不构建新身份或控制模型 |
| [watch_service.rs](../src/watch_service.rs) | 后置抽取 | heartbeat/watch 写入调用 lifecycle permit，不能整块复用 |
| [ci/watch.rs](../src/ci/watch.rs)、[ci/checks.rs](../src/ci/checks.rs) | 保留独立工具 | 首版只记录 CI 引用，不导入 PR/queue 逻辑 |
| [metrics.rs](../src/metrics.rs)、[util/protected.rs](../src/util/protected.rs) | 留在旧路径 | 读取 queue/guard 或 authority 语义，不是 light 查询后端 |
| queue、progress、pr、delivery、task_runtime、task_env、batch、guard、lifecycle | 冻结 light 依赖，旧用途保留 | light 不直接或间接调用控制面，不新增 light 模式包装 |
| [coordinator_handoff](../src/task_runtime/coordinator_handoff/mod.rs) | 旧路径保留 | request/status/cancel 与围栏不补成 light 接任能力 |

core 不读取或解析旧 queue/authority/guard/receiver/lifecycle 文件，不扫描标记、不要求兼容清单或迁移工具存在。检查间接依赖与进程行为；同 crate 拆模块不等于权限隔离。自动导入、调度器、原生产品适配器、恢复 Agent 均不在首版范围。

## 独立数据根与路径检查

本机选定绝对数据根 **`/home/gem/.local/state/herdr-light`**，数据库为 `/home/gem/.local/state/herdr-light/records.sqlite3`。本会话 `XDG_STATE_HOME` 未设置，符合拟议默认 `${XDG_STATE_HOME:-$HOME/.local/state}/herdr-light`；其他部署显式解析自己的绝对路径，不照搬本机路径。数据库与备份须在同机本地文件系统，备份也放在回收范围外；不假定任意 XDG 位置满足条件。

本批显式禁止目录如下。它们是操作者提供的通用路径集合，不要求 core 识别任何旧格式：

| 绝对目录 | 禁止理由 |
| --- | --- |
| `/home/gem/.herdr/flow` | 覆盖旧默认控制根；[registry.rs](../src/task_env/registry.rs) 默认为其下 `task_environments`，可由 `HERDR_TASK_ENV_ROOT` 覆盖 |
| `/home/gem/herdr-flow-rust` | 用户共享源码与未跟踪方案，不存持久记录 |
| `/home/gem/herdr-light-170-dev` | 本批可清理 worktree |
| `/home/gem/herdr-light-handoffs` | 外部批次交接目录，与数据库独立保留/轮换 |
| `/tmp`、`/var/tmp` | 临时回收目录 |

2026-10-30 以 `Path.resolve()` 解析上述路径和数据根，逐项检查 `a == b or a in b.parents or b in a.parents`，六项均无父子重叠，现有路径未解析到不同位置。数据根尚不存在；解析采用现存祖先加未创建后缀，本次未创建目录或数据库。`/home/gem/.local` 与 `/home/gem/.local/state` 均为 0775，本次未改权限。

这份名单是本批明确的排除范围，不是全机回收根发现结果。本会话 `HERDR_TASK_ENV_ROOT` 未设置，不证明其他任务也未设置。旧运行时可使用 queue 所有的控制根和另配的 disposable roots；实际旧对象切换前，负责者必须将其准确回收目录补入显式禁止集合并重新检查。若范围不明，不宣称该旧对象的路径安全已验收；不因此要求普通新项目依赖旧系统盘点。

P1 路径实现须：绝对化并规范化数据根、数据库/备份目标与每个禁止目录；处理 `.`、`..`、符号链接与不存在的末级；按路径组件双向拒绝相等/父子重叠（不能用字符串前缀）。解析失败、悬空链接等不确定情况应报错。创建和打开前重新核验，避免检查后替换符号链接绕过边界。新建专用目录 0700、数据库及 WAL/SHM/备份 0600，不 chmod 用户已有父目录。P0 只核对静态快照，不宣称已实现抗路径替换保护。

## 适用切换对象与回滚

- 当前文件交接团队及获准的新任务沿用现有协作，可选增加记录，无需迁移团队、重开会话或排空旧队列。本批无已确认需要切换的旧受管对象，未执行旧任务 status/recover 等可能创建目录或产生副作用的命令。
- 真正使用旧 queue/format-3 受管运行时的具体任务，切换时由负责者核对在册任务、准确会话、未决操作、writer、watcher、回收义务及目录归属。按原契约自然完成或真实取消/失败收尾，不伪造 final failure。两类任务可按对象与目录隔离共存。
- [旧 CLI](../src/task_runtime/cli.rs) 明确 request/status/cancel 只到请求、draining 围栏和取消；successor 创建与完成 replacement continuation 未实现。关联 session、角色或负责人不会补齐接任，不授予 writer、终止或删除权限。
- 盘点是切换动作，不是 light 常驻能力或启动依赖；关闭旧 Issue 不等于退出义务完成。P0 不删除旧模块、不更换已安装 skill。
- 回滚停用可选报告调用及明确归属的 light 观察器，保留库和备份，原团队继续工作；不自动恢复旧快照或把记录转成旧队列授权。旧模块退役另需退出义务清楚、试点通过与明确默认入口切换决定。

## P1 可执行验收清单

以下为 P1a/P1b 必须实现并运行的场景；P0 未运行这些尚不存在的接口。测试使用独立临时目录/数据库和替身进程，不接管真实任务。

| 场景与操作 | 必须断言 |
| --- | --- |
| 注册两个项目；对项目 A 用 realpath、符号链接、`.`/`..` 和尾斜杠重复登记；写未知项目 id | A 别名返回同 id，B 独立；未知 id 拒写，目录大小写保留 |
| 同项目同 Issue 建两批 task，再建 batch_ref 为空的 task；分别报告/分页查询 | task.id 独立；source_ref/batch_ref 仅标签，不能合并或猜写入目标；显式项目筛选不串数据 |
| 建同任务多个同类/不同角色 binding，session 为空；指定负责人，再尝试引用另一任务 binding | 空 session 可保存；负责人只能引用本任务 binding；角色/负责人不产生控制权限 |
| 四表 projects/tasks/bindings/events；提交报告与关联修改 | 外键有效；事件和摘要/关联在同一事务更新，revision 增长；关联历史保留 |
| 同 request_id 同内容重试，再以同 id 不同内容重试；两个进程用同 revision 并发更新 | 原结果可重用、不增写；不同内容冲突；并发最多一个成功，旧 revision 不覆盖新摘要 |
| 事务中断进程；模拟 busy、只读、写入失败 | 无半条报告；busy 有有限超时；失败明确可补报，绝不重放开发/push/merge |
| 活跃 WAL 写入期间用 SQLite 备份接口备份，恢复到独立目录并查询；旧程序打开不支持的新 schema | 摘要、引用、版本一致；不能仅复制主库；不支持 schema 明确退出而非降级改库 |
| 数据根与禁止目录相等、任一为另一祖先；符号链接别名、缺失末级、检查期间链接替换 | 所有重叠/绕过拒绝；相似字符串前缀但不同组件不误拒；新文件权限符合约定，已有父目录不变 |
| 完全无旧目录/清单/迁移工具且 Herdr 离线，初始化、报告并查询本地记录 | 可以运行；不读取旧格式；显式 Herdr 查询才检查环境和目标 |
| 静态审查 light 依赖及共享函数间接调用；替身记录子进程调用，文件访问审计覆盖 init/report/query/backup | 不导入旧控制面，不读取 queue/authority/guard/receiver/lifecycle；不派工、唤醒、合并或清理 |
| 记录仅含链接、准确提交及报告结果的 CI refs；记录临时/持久文件引用；故意使库不可用 | 不要求 config_hash，不裁定 CI 门禁；失效/变更引用如实标注，不按 mtime 推断新结果；原 RESULT/ACK 协作继续 |

摘要和 refs 有长度限制，查询分页，schema 有版本；不保存原始对话或终端全文。P2 才接入现有 skill 的可选报告调用并核对各运行时读取版本，不新建 light skill；P3 才做只读观察/通知，观测不得覆盖报告状态；P4 按需。可靠原会话目标约束、停止/错误/副作用证据与授权预算未满足前，自动恢复关闭。这些后续能力不阻塞 P1。

实现质量门沿用 [Rust checks](../.github/workflows/rust.yml)、[CRAP](../.github/workflows/crap.yml)、[依赖审计](../.github/workflows/audit.yml)：fmt、lint-policy、Clippy `-D warnings`、all-targets 测试、release 构建、绝对 CRAP ≤ 10（无 baseline）、依赖审计。P0 验证文档引用、事实和范围；文档合入、CI 通过、既有团队协作成功均不等于上述 P1 行为已验收。
