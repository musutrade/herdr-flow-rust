# 任务接入原生 Codex TUI

本说明优先于历史文档中“新任务只能后台运行”的描述。本次只接通原生界面；
任务调度、人工接管时的写入权交接、长任务超时和网络重试规则另行讨论。
OpenCode 不支持。

2026-10-27 本地真实接入验收通过，最终代码复验 25.25 秒（先前两次通过 25.80 / 26.61 秒）。实现位于
`fix/native-tui-attachment`，基于现有上游接入分支 `e036bfc`。这不表示全局部署或完整业务流程验收完成。

## 配置和使用

在独立任务的 `task-runtime launch` spec 中增加 `native_tui`：

```json
{
  "adapter": "codex",
  "configuration": "upstream",
  "socket": "/absolute/path/to/existing-codex.sock",
  "cwd": "/absolute/path/to/task/checkout",
  "model": {"providerID": "openai", "id": "YOUR_MODEL", "reasoning_effort": "high"},
  "native_tui": {
    "binary": "/absolute/path/to/matching/codex",
    "parent_pane": "YOUR_HERDR_PANE_ID",
    "direction": "right"
  }
}
```

`socket` 必须是现有 Codex 服务的规范 Unix socket 路径；`binary` 使用与服务匹配的
原生 Codex。Herdr socket 从现有 `HERDR_SOCKET_PATH` 取得并记录 inode/设备/所有者。
`parent_pane` 明确指定父窗格，`direction` 为 `right` 或 `down`，新窗格不抢焦点。
工作目录继续使用任务隔离工作树，审查角色继续使用自己的快照。

协调者首次 `receiver-start`、开发/审查角色 `dispatch` 提交成功后自动打开原生界面。
开发/审查角色继承显示配置，模型继续分别取 `role_models.developer/reviewer`。
启动返回值中的 `presentation` 记录显示结果；显示失败不撤销已提交的 turn，也不授权重发任务。

同一运行时可直接管理界面：

```sh
herdr-flow task-runtime tui-status --task TASK --state-dir RUNTIME_DIR
herdr-flow task-runtime tui-close --task TASK --state-dir RUNTIME_DIR
herdr-flow task-runtime tui-open --task TASK --state-dir RUNTIME_DIR
```

`tui-close` 只关闭本次创建的窗格和前端，不取消任务；`tui-open` 恢复同一会话。
关闭前检查服务、终端、会话和前台进程归属。窗格已被其他会话/进程使用时拒绝关闭。
启动结果不明确时只检查原尝试，不自动新建第二个窗格或重发输入。

## 原生交互及职责边界

Herdr 内运行真正的 `codex resume THREAD --remote unix://BRIDGE`。界面输入、审批、
提问、Esc 中断由原生 Codex 处理；Herdr 的 `agent get/read/prompt/send-keys` 可定位该窗格。
协议观察核对会话 ID、模型、推理强度、cwd 和上游 socket，再上报 Herdr 会话绑定。
同时等待 Herdr 的 `agent get` 能定位相同终端和会话，才返回显示就绪；不能把收到
`thread/resume` 响应等同于前端启动完成。实际输出绘制仍可能稍晚于业务 turn 完成。
任务附件不允许新建/切换会话、改账户或全局配置；模型由角色配置选择。

接回已有会话不传 `--cd`：cwd 从该会话恢复，再校验其与任务记录一致。
这避免把已有任务的恢复误当作新目录的首次信任流程；不生成或保存新的信任配置。
Codex 0.164.0 在首条真实消息落盘前不能从另一连接恢复空会话，因此不发送“预热任务”，
界面在真实任务提交后打开。启动等待只是界面就绪检查，不是任务执行时限。

原生 TUI 尚不处理动态工具 `item/tool/call`。任务桥只过滤这类发给前端的请求，
已有 broker 保留唯一响应权；审批/提问请求及响应照常转发。
原生目录响应实测超过 16 MiB，任务桥按当前 Codex remote 客户端的 128 MiB 上限处理；
普通 broker 的 16 MiB 限制不变。被拒绝的辅助请求返回 JSON-RPC 错误，保持主连接可用。
原先不带任务策略的 `herdr-flow tui` 仍是透明桥。

共享 app-server 可能继承其他窗格的环境。只对本任务会话，替代精确匹配的 Herdr
SessionStart 上报 hook，并由实际前端上报所属窗格；其他 hook 保持原样。
遇到自定义复合或 managed hook 时拒绝自动替代。没有编辑、升级或重装全局 hook。

登录、provider、订阅、计费和代理仍归现有 Codex；不复制认证、不另建凭据目录、
不切换计费路径、不启动第二套服务。任务配置使用 `history.persistence=none`，避免将
测试/任务输入追加到用户共享输入历史；已有历史不修改。

## 回收和验证

新窗格的精确身份列入原有终态回收计划，先停止任务进程和关闭所属窗格，再删除所属
上游会话和任务目录。服务不可用或归属不确定时保留回收义务，不删用户窗格来凑成功。
历史 Herdr 快照和以前已有的输入历史记录不因本次接入而被批量修改。

自动化测试覆盖协议分片、动态工具响应权、原生审批消息透传、会话边界与 hook 选择。
真实验收入口（默认 ignored，必须显式指定现有服务和模型）：

```sh
cargo test --locked --test lifecycle_authority \
  native_tui_live::native_tui_same_thread_input_reopen_and_cleanup \
  -- --exact --ignored --nocapture
```

环境参数：`HERDR_UPSTREAM_SOCKET`、`HERDR_UPSTREAM_MODEL`（模型对象 JSON）、
`HERDR_NATIVE_BINARY` 以及现有 Herdr 环境。测试在独立队列里验证原生显示、同会话输入、
队列工具处理、关闭重开、上下文连续性和终态回收；不代表整个业务流程已验收。

本次使用 Herdr 客户端 0.9.5 / 服务 0.9.4、Codex 0.164.0、显式选择的
`gpt-6.1-sol/high`。三个真实 turn 分别确认首条消息、从原生界面调用 `herdr_queue inspect`、
关闭重开后回忆首条消息的口令。重开保持同一 session，终端身份改变，首次任务未重发。
测试最后以明确的夹具最终失败状态驱动同一收尾路径；它不是一次业务成功交付。

最终验收证据在本机 `/home/gem/.cache/herdr-flow-native-tui-20261027/.tmp3QHQxv/`：
`native-result.json` 保留原生输出及工具回执，`native-cleanup.json` 记录根目录和注册回收。
父目录 `after-audit.json` 记录本轮全部尝试的归属检查：任务根和测试窗格无残留、共享输入
历史无新增测试行、原有窗格终端/会话身份不变；Codex config、hooks、上报脚本和 `.env`
前后哈希一致。此前失败尝试的证据保留，不补计通过。

本地全 targets 覆盖率回归通过，最终调整另跑 TUI 定向回归；格式、全 targets/features
Clippy、lint-policy、release 构建和 CRAP 绝对阈值 10（无 baseline）通过。测试种子库和已空的夹具
控制目录在归属复核后清除，只保留上述有界验收证据；没有把任务残留转移到共享目录。

审批/提问协议已有透传回归，真实模型验收本次没有触发人工审批，不据此声称人工接管
决策规则或故障恢复方案已验证。原生界面可展示和输入，与任务写入权交接是不同验收范围。
