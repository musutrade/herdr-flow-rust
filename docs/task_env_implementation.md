# Task Environment Lifecycle Management (Issue #33)

## 概览

本模块实现了任务环境的创建、恢复、停止和销毁功能，建立明确的资源归属和生命周期管理。

## 核心功能

### 1. 任务环境创建

```rust
use herdr_flow::task_env::{create_environment, DeliveryTarget};
use std::path::PathBuf;

// 定义正式交付目标（必须在disposable边界外）
let delivery_targets = vec![
    DeliveryTarget::new(
        PathBuf::from("/repo/main"),
        "branch".to_string()
    ),
];

// 创建任务环境
let env = create_environment(
    "task-123",                          // 任务ID
    &PathBuf::from("/tmp/task-123-work"), // 工作区root（disposable）
    delivery_targets,                     // 交付目标
    &PathBuf::from("/tmp/task-123-temp"), // 临时文件root（disposable）
)?;
```

### 2. 环境边界保护

```rust
use herdr_flow::task_env::{is_disposable, is_delivery_target, validate_path};

// 检查路径是否在可销毁边界内
if is_disposable(&env, &some_path) {
    // 可以安全删除
}

// 检查是否是交付目标
if is_delivery_target(&env, &some_path) {
    // 这是正式交付位置，应保护
}

// 验证路径在允许的边界内
validate_path(&env, &work_path)?;
```

### 3. 资源注册

```rust
// 注册Git worktree
env.register_worktree(worktree_path)?;

// 注册子进程
env.register_process(child_pid)?;

// 注册打开的文件
env.register_file(file_path)?;
```

### 4. 环境销毁

```rust
use herdr_flow::task_env::destroy_environment;

// 销毁环境，释放所有资源
destroy_environment(&env)?;
```

## 实现特性

### ✓ 已实现

- [x] 环境身份、根目录、交付目标定义
- [x] 创建与恢复绑定同一环境，不重复分配
- [x] 正式交付物与用户已有文件保护
- [x] 中断创建、重复销毁的崩溃恢复
- [x] 幂等的基础销毁操作
- [x] 环境状态持久化到~/.herdr/flow/task_environments/
- [x] 全局环境注册表
- [x] 资源边界验证（防止写入未管理路径）
- [x] Worktree、进程、文件句柄注册
- [x] 清理失败的重试机制

### 待集成（后续实施步骤）

- [ ] 集成到Git适配器（实施3）
- [ ] 集成到命令执行适配器（实施3）
- [ ] 集成到Codex/OpenCode适配器（实施3）
- [ ] 真实交付调用（实施5）
- [ ] 最终完成转换（实施5）

## 数据结构

### TaskEnvironment

```rust
pub struct TaskEnvironment {
    pub task_id: String,
    pub workspace_root: PathBuf,
    pub temp_root: PathBuf,
    pub delivery_targets: Vec<DeliveryTarget>,
    pub state: EnvironmentState,
    pub worktree_registrations: Vec<PathBuf>,
    pub child_processes: Vec<i32>,
    pub open_files: Vec<PathBuf>,
    // ...
}
```

### DeliveryTarget

```rust
pub struct DeliveryTarget {
    pub path: PathBuf,
    pub target_type: String,
    pub description: Option<String>,
}
```

### EnvironmentState

```rust
pub enum EnvironmentState {
    Creating,     // 环境创建中
    Active,       // 环境活跃
    Destroying,   // 环境销毁中
    DestroyFailed, // 销毁失败
}
```

## 崩溃恢复

系统在以下场景中支持恢复：

1. **创建中断**：下次调用自动检测并从断点继续
2. **销毁中断**：CleanupState保存剩余清理任务
3. **清理失败**：调用`cleanup_failed_environment(task_id)`重试

## 资源清理顺序

1. 终止活跃子进程（SIGTERM → SIGKILL）
2. 移除Git worktree注册
3. 删除workspace和temp目录
4. 取消环境注册
5. 删除状态文件

## 测试

运行单元测试：

```bash
cargo test --test task_env_tests
```

## 相关文件

- `src/task_env/mod.rs` - 公共API
- `src/task_env/env.rs` - 核心环境管理
- `src/task_env/boundary.rs` - 边界保护和路径验证
- `src/task_env/registry.rs` - 全局环境注册表
- `src/task_env/cleanup.rs` - 清理和销毁逻辑
- `tests/task_env_tests.rs` - 集成测试

## 下一步

本实现提供了Issue #33所需的基础能力。后续实施步骤需要：

1. **实施3**：在各适配器中使用TaskEnvironment
   - Git操作传递环境，worktree在环境内创建
   - 命令执行在环境内运行，注册进程
   - 验证所有写入路径在环境边界内

2. **实施5**：集成到任务完成流程
   - 在`progress::apply_complete`中调用销毁
   - 确认交付成功后才销毁环境
   - 任务成功 = 正式交付 + 环境已销毁
