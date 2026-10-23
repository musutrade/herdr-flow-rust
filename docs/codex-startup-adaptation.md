# Codex 原生运行启动适配

日期：2026-10-23。实现位于 `fix/codex-owned-launcher`，集成基线为
`70bd58b87e5234d35211f1a4d4c81e34d1ad9e33`。
本机已验证 task-owned Codex worker → OpenCode coordinator → 独立验收 →
测试交付 → 环境清理闭环；未扩展 Codex coordinator，#38/#32 保持开放。

## 根因与修复

原有 `thread/start` 使用 `workspace-write`，`turn/start` 已传
`externalSandbox`。分阶段实测表明 initialize 和空线程创建成功；
首个可复现失败在命令执行，而非 app-server 直接退出。

固定 Codex 制品的 exec 跟踪显示：externalSandbox 已转换为
`{"type":"external","network":"enabled"}`，但命令同时带
`--managed-network`。宿主 `/etc/codex/requirements.toml` 启用管理级网络策略，
Codex 必须启动内层 `/usr/bin/bwrap` 来执行该策略。内层并非可以删掉的
纯重复文件系统隔离。

宿主内核审计将失败绑定到 `unpriv_bwrap` 的 sys_admin 能力拒绝：
系统 bwrap 的子进程继承该 AppArmor 限制，内层 namespace 创建返回 EPERM。
本机已有专用外层启动器 `/usr/local/libexec/codexsymphony/bwrap`；
用户在本会话明确授权将它复用于 Herdr 任务隔离验证。修复让任务显式选择
这个既有宿主支持路径，保留全部外层挂载参数及 Codex 内层网络保护。
没有安装二进制、改 AppArmor/sysctl、增加宿主权限或切换 full access。

另修正两个测量问题：

- 嵌套预检原先遗漏私有 /proc，专用启动器下会因 uid map 只读而误报失败；
  现与产品启动的 /dev、/proc 挂载一致。
- Codex 的 server.sock 可能是指向私有 /tmp 的绝对符号链接。旧测试在宿主
  直接 exists() 导致超时；现复用产品的私有路径解析并确认目标为 socket。
  这只证明 socket 存在，不替代 initialize 或派发接收证据。

## 固定制品与支持条件

| 对象 | 本轮实测身份 |
| --- | --- |
| Codex | 0.163.0；`/home/gem/.codex/packages/standalone/releases/0.163.0-x86_64-unknown-linux-musl/bin/codex` |
| Codex SHA-256 | `399ff69c431c066e40de318fba45c2b07f45a76b19939d247ee4828fc70c4215` |
| OpenCode coordinator/reviewer | 1.19.4；`/home/gem/.opencode/bin/opencode`；运行中服务日志亦报告 1.19.4 |
| OpenCode SHA-256 | `538fcb1b6408c8d4608822acee19f7ff5b3a3eb3383c0633c4f06170857312a7` |
| 专用外层 bwrap SHA-256 | `0abea81db798ebf6b4742ac0664802d97521547a353c2a0dbdc21d76cbbfd2c0` |
| 真实模型配置 | Codex `openai/gpt-6.1-sol`；OpenCode `magpie/deepseek/deepseek-flash` |

无需升级或降级。1.19.3 是原有文档基线，不是本轮 coordinator 实测版本。
本地上游源码快照不能证明与已安装制品完全同源，根因判断以固定制品的
实际请求、exec argv、退出结果及宿主审计为准。

支持宿主必须预先提供获授权、可安全运行内层 Codex sandbox 的外层 bwrap。
没有这种宿主条件时继续拒绝执行；产品不会自动寻找其他路径绕过宿主策略。

## 使用与改动范围

Codex 的 `task-runtime launch` spec 增加一个可选字段：

```json
{
  "namespace_launcher": {
    "path": "/usr/local/libexec/codexsymphony/bwrap",
    "sha256": "0abea81db798ebf6b4742ac0664802d97521547a353c2a0dbdc21d76cbbfd2c0"
  }
}
```

这是 operator 选定的可信启动器输入，应并入既有包含 adapter/binary/cwd/model
的 launch spec，不是 worker 自选的可执行程序。自定义路径必须为规范绝对路径，
文件及父目录须 root 所有且不可被 group/world 写入，文件须可执行并符合摘要。
该验证证明安装身份，不自动证明 bwrap 语义或宿主 profile 的授权；
管理员须事先验证它确实执行约定的隔离。规格保存在 runtime.json 中。

缺省仍用 /usr/bin/bwrap；显式选择格式错误、文件不可信、摘要不符均失败，
没有自动回退。OpenCode 拒绝这个 Codex 专用选项。

改动限于 task_runtime 启动器选择、task_env 内部 namespace 命令构造、
原有 socket 映射上的只读存在检查、针对性测试与文档。
thread/turn 策略、原生审批 broker、managed network、dispatch 行为不变。
真实混合测试通过 `HERDR_LIVE_CODEX_NAMESPACE_LAUNCHER` 接收同一 JSON 对象，
预检也调用相同身份验证。

## 安全边界与验证结果

外层保持根文件系统只读，仅 runtime 和活动 checkout 可写，/tmp、/dev、/proc
私有化。它主要限制持久文件修改；不据此宣称隔离宿主机密读取或网络。
管理级网络约束继续由 Codex 原生内层执行，不删除、不禁用。

| 检查 | 结果与证据边界 |
| --- | --- |
| 同一路径 initialize / 空 thread/start | 成功；返回 untrusted、user reviewer、workspaceWrite；不是审批行为证明 |
| 无模型 command/exec，externalSandbox | 成功，exitCode 0；工作区创建/重命名/删除成功；挂载外专用哨兵的创建/覆盖/重命名/删除均 errno 30，宿主检查未变 |
| 模拟模型驱动的真实原生审批 | 真实 app-server 发出 requestApproval；核对 thread/turn/item；pending 无副作用；decline 后 resolved、command declined、无副作用；另一独立操作 accept 后 resolved、command completed、exitCode 0，预期文件生成 |
| 批准后的外层边界 | 另一独立模拟工具操作获准执行后，四种越界修改仍被拒绝；命令完成、exitCode 0、哨兵未变 |
| 真实混合闭环 | 105.44 秒通过；真实 Codex 工具修改并创建输出提交，生产 collector 精确绑定 turn，OpenCode 自行 ACK/apply，独立验收和 Local 测试交付完成；驱动器不代 ACK/apply |
| 清理 | 闭环后任务归属路径不存在，标记进程匹配归零；测量保留其发现范围限制；本轮外层临时目录也已删除，选定配置/认证源文件字节未变 |

模拟审批使用本地固定 SSE 工具响应，不调用模型推理服务；
它验证的是实际 Codex 审批执行机制，不算真实模型闭环，也未覆盖所有文件变更
或网络审批类型。真实闭环另由上表的 installed mixed test 提供。

中间失败未伪装成通过：初始探针目录权限、AF_UNIX 长路径连接、旧预检缺少
/proc、宿主 exists() 跟随私有 socket 链接均分别修正；
对应模型派发前失败未被用来恢复或重发任何不确定 turn。
已有明确终止并清理的测试环境才重新建立新的验证活动。

`~/.codex/.env`、链接目标、工具代理配置、全局安全策略及常驻服务未改动。
原工作目录已有的 .cargo/config.toml 修改不属于本补丁。
临时原始日志与会话随探针/测试环境回收，不建立长期完成证据库。

## 代码验证记录

稳定 Rust 与最低支持版本 Rust 1.85.0 的全量测试均已通过：各 519 项通过，
26 项按原有规则忽略。上述真实混合闭环是另行显式运行的 ignored 测试，
没有把普通测试的 ignored 计作真实闭环通过。
拆分校验函数后的覆盖率运行另通过 518 项测试、忽略 26 项；该命令不运行
all-targets 中额外的 example 测试。绝对 CRAP 门禁通过（阈值 10、无 baseline，
行覆盖率 77.9%；另有两个函数无行数据，门禁按 0% 处理）。
最终源码的 `cargo fmt --all -- --check`、`lint-policy`、
`cargo clippy --all-targets --all-features --locked -- -D warnings` 均通过；
稳定 Rust 和 Rust 1.85.0 的 `cargo build --release --locked` 均通过。
修复仅保存在独立工作区和本地分支，尚未发布或替换已安装的 Herdr 二进制。

首次并行构建期间，MSRV 的既有 advisor 用例
`terminal_job_and_duplicate_request_never_reinfer` 失败，随后同一全量命令
单独运行通过；初始覆盖率命令也曾以测试退出码 101 结束。
未改这类用例的断言或超时，也未确认初始失败的根因。
首次绝对 CRAP 门禁指出新增 resolve 函数复杂度超标，已将路径校验和
文件身份校验拆开，保持相同安全要求；拆分时的借用编译错误也已修正。

## dispatch 与剩余范围

不确定 dispatch 继续禁止重发。日志不存在、空历史、连接中断或 request id
均不证明服务端未收到。以后若增加恢复，必须另有服务端幂等契约或绑定原操作的
可核验接收证据；正向接收证据用于关联原执行，不用于重新发送。

本次解决已验证宿主路径上的 Codex worker 原生运行缺口。
一次混合闭环不等于 #38/#32 全部完成；没有增加普通模型矩阵或 Codex coordinator。

