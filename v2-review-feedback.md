# V2 代码审核反馈（历史记录）

> 本文件记录 2026-10 对旧实现的审核意见，其中的 Python 片段与测试数量仅用于历史语境。当前实现为纯 Rust，验证命令为 `cargo test --all-targets`，代码位置以 `src/` 与 `tests/` 为准。

**审核日期**: 2026-10-17
**审核者**: OpenCode (Kiro)
**审核范围**: V2 批次消费端实现

> 后续复核（2026-10-18）：下文保留原始审核意见。高优先级的双向导入问题已通过独立的 `src/batch/validation.rs` 修复，发布校验及只读依赖由两个模块共享；新增冷启动导入和发布阻断回归测试。修复后相关 59 项测试通过，用户随后确认完整 320 项测试通过。其余 4 项保留后续改进，不阻塞开发验收；生产验收条件仍按 `references/v2-acceptance.md` 保留，本报告不关闭 #12。
>
> 安全加固后复核（2026-10-19）：完整测试套件 428 项通过。原第 3 项（Git 错误信息不详细）已由 `7d3fb84` 修复，诊断包含脱敏后的 stderr；第 5 项已增加 Check 套件窗口上限，超过 1000 个套件时拒绝判定。第 2 项（验证器超时配置化）和第 4 项（Git 身份完整配置化）仍未解决，继续作为非阻断改进保留。

## 追加审计修复（2026-10-18）

后续审计独立复现了六项正确性问题；此前 320 项测试的覆盖不足以证明这些路径正确。本轮已完成以下修复和回归：

1. 批次成员不能进入独立 V1 PR 流程，旧状态中已准备的 PR 也不能开始发布；成员不能初始化另一个 lane。
2. accepted/frozen 成员不能 resume、dispatch、开始/接手旧派工或预留 writer。freeze、integrate、发布前重新检查成员 writer、待应用结果、PR、lane 和交接状态。
3. V1 和 V2 发布均先检查 writer 释放、结果应用及阻塞 finding；无 lane 不再跳过通用检查。
4. 首次冲突检查点同时保存冲突 parent 对应的 candidate、failure token 和 blocker。回归在该次持久化后注入中断，重新加载并实际走完 V1 修复、ref 更新和快照冻结。
5. rebase 与 freeze_next 共享上一批落地祖先检查，拒绝丢失上一批的目标；失败不改变现有批次输入。
6. 同时补齐三项相关防御：offer 要求 lane idle；rebase 保留已接手且结果已应用但尚未导入的修复；直接调用 Checks observer 时先校验 repo/PR/SHA，再访问 API。

完整 `cargo test --all-targets`：**331 项全部通过**，比原套件增加 11 项测试（包含多组边界场景）。已确认落地的成员仍能恢复并完成原始验收。原有四项非阻断改进继续保留；实际运行时、GateHouse 部署、发布演练、保护迁移及性能基准要求仍见 `references/v2-acceptance.md`。

以下为原始审核记录，测试数量与结论保留其当时语境。

## 测试验证结果 ✅

所有测试通过：
- `test_src/ci/checks.rs`: 8/8 通过
- `test_src/batch/lane.rs`: 19/19 通过
- `test_src/pr.rs`: 36/36 通过（V1 回归测试）

## 核心功能完成情况 ✅

根据 `references/v2-acceptance.md`，以下功能已实现且通过测试：

1. **External Checks 只读策略** (src/ci/checks.rs, 100 行)
   - App ID/external_id 严格验证
   - 撤销和错误运行识别
   - 套件窗口溢出检测

2. **有界批次管理** (src/batch/lane.rs, 599 行)
   - 待处理容量限制和成员接受
   - 私有索引集成和确定性提交日志
   - 冲突保留和修复工作流

3. **可信门禁客户端** (src/batch/gate.rs, 82 行)
   - 独立 stdio 验证服务边界
   - 操作/输入/策略严格校验

4. **Git 隔离集成** (src/batch/git.rs, 142 行)
   - 崩溃后恢复能力
   - 不修改开发者 checkout

## 需要修复的问题 ❌

### 1. 循环导入风险（高优先级）

**位置**: `src/batch/lane.rs:14` 和 `src/pr.rs:149`

```python
# src/batch/lane.rs
from pr_ledger import ...

# src/pr.rs
from batch_lane import publication_check
```

**问题**: 互相导入可能导致初始化问题。

**建议**: 将 `publication_check` 移至 `src/progress.rs` 或创建共享验证模块 `src/batch/validation.rs`。

### 2. subprocess 超时硬编码（中优先级）

**位置**: `src/batch/gate.rs:58`

```python
result = subprocess.run(..., timeout=45, ...)
```

**问题**: 45 秒可能对某些验证不够，但过长会阻塞队列。

**建议**:
```python
timeout = config.get('verifier_timeout_seconds', 45)
if not 5 <= timeout <= 120:
    raise p.ProgressError('verifier timeout must be 5..120 seconds')
```

### 3. Git 错误信息不详细（中优先级）

**位置**: `src/batch/git.rs:32-33`

```python
if result.returncode and not allow_failure:
    raise p.ProgressError('batch Git operation failed; private integration state retained')
```

**问题**: 没有包含 stderr 输出，难以诊断。

**建议**:
```python
if result.returncode and not allow_failure:
    stderr = result.stderr.decode()[:500] if result.stderr else 'no stderr'
    raise p.ProgressError(f'batch Git operation failed: {stderr}')
```

### 4. 硬编码的 Git 身份（低优先级）

**位置**: `src/batch/git.rs:21-23`

```python
GIT_AUTHOR_NAME='Herdr batch',
GIT_AUTHOR_EMAIL='batch@example.invalid',
```

**建议**: 从 lane 配置读取，便于审计追踪：
```python
author_name = record.get('batch_author_name', 'Herdr batch')
author_email = record.get('batch_author_email', 'batch@example.invalid')
```

### 5. 缺少 API 速率限制保护（低优先级）

**位置**: `src/ci/checks.rs:73`

```python
runs = api.pages(f"commits/{c['sha']}/check-runs?filter=all", 'check_runs')
```

**建议**: 添加结果数量上限检查，防止超大提交触发速率限制。

## 架构设计评价 ✅

**优点**:
- 边界清晰：V1/V2 复用明确，职责分离
- 安全性考虑：信任边界明确，失败关闭，防止重放
- 恢复能力：日志先于 ref 更新，幂等集成
- 测试覆盖：真实 Git 操作，崩溃场景模拟

**设计权衡**（已知限制）:
- 有界限制：128 批次、256 门禁操作、16 次重建
- V1 依赖：需要 V1 运行时接受验收
- 外部依赖：需要 GateHouse #98 部署

## 生产部署前的必做事项（来自 v2-acceptance.md）

以下是文档明确要求但尚未完成的：

1. **V1/运行时前置条件确认**
   - 从保留的接受证据（不是单元测试）确认安全 session takeover
   - 验证实时通知和轮换能力

2. **#98 部署集成**
   - 连接实际的授权 API（操作员拥有的客户端）
   - 验证策略/run/base/tree 身份、失效和静止

3. **完整发布路径演练**
   - 正常、失败/修复和中断的发布/报告路径
   - 使用实际受限发布者和可信 App
   - 同结果报告恢复（无重复 gate 执行或 PR 创建）

4. **保护迁移**
   - 确认完整的迁移验证覆盖
   - 当前仓库源/基线保护
   - 删除重复的 Actions 前验证

5. **#12 性能基准**
   - 同主机冷/热/源/工具链/依赖/配置缓存测量
   - 对比顺序与批次的每交付任务资源
   - 包含等待、修复、取消、缓存维护和人工干预

## 最终评估

**代码质量**: 良好
**测试覆盖**: 充分
**完成度**: 开发验收通过，生产部署未就绪

**结论**:
代码本身质量可以接受，没有明显的阻塞性 bug。但根据 `v2-acceptance.md` 明确声明：

> "These are deployment/pilot acceptance requirements, not optional observations."

**这不关闭 #12，也不证明生产就绪**。需要完成上述 5 项生产验收才能正式启用。

## 建议优先级

### 立即修复（阻塞问题）
1. 循环导入风险

### 近期改进（质量改进）
2. subprocess 超时配置化
3. Git 错误信息详细化

### 后续优化（可选）
4. Git 身份配置化
5. API 速率限制保护

## 下一步行动

1. 修复上述 5 个问题（特别是循环导入）
2. 创建生产验收清单追踪文档
3. 与 GateHouse #98 团队协调集成时间表
4. 准备性能基准测试环境
