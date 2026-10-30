# 可选 light 精简记录调用

本文件是 [#173](https://github.com/musutrade/herdr-flow-rust/issues/173) 的唯一版本化记录调用说明源。协作流程继续使用当前团队已核实的 **herdr-team-flow**；本文只增加主动记录，不复制其派工、审计、发布、RESULT/ACK 或清理流程。[CLI 字段、分页与错误契约](../docs/light-cli.md)及[存储约束](../docs/light-storage.md)仍是接口依据。

## 采用和停止

由获准参与的现有 Agent 在里程碑、阻塞或原 RESULT/ACK **已经完成后**选择提交简短报告。先按原约定通知/核验/ACK，再单独执行记录调用；不要以 `record && notify`、`set -e` 下未捕获的记录错误或等待记录成功作为原协作的前提。不创建 Agent、队列、配置框架或自动扫描器，不自动读取/导入批次目录；light 不是发布、写权或清理控制面。

采用前，每个实际参与者亲自读取此文件，并向调度报告：源仓库及完整提交、源文件 SHA-256、实际读取的绝对路径和解析后路径、实际内容 SHA-256、当前协作入口、对下文 ACK/失败边界的理解。可直接读固定提交的检出，也可在获准安装时复制同一内容；不要假定 Codex 链接会同步 Hermes/Claude。源变更时重新确认内容与适用范围；只确认复制成功不算读取验收。实际安装的切换见[入口方案](light-entry-transition.md)。

停止 hook 就是不再调用记录命令，继续原 Herdr 消息与证据交接。不引入 enable/profile/TOML 开关，也不删数据库、请求文件或既有证据。下一项工作仍需原授权，记录成功/失败都不自动派工。

## 一次主动调用

1. 明确本次数据根和显式禁止目录，使用已验证的 light 二进制绝对路径；首次使用由拥有该目录的调用者显式 `light init` 和 `project register`。不要借用旧任务目录，不在共享 checkout 内保存库/证据。项目根别名登记返回首次 id/label。
2. 显式创建任务并保存返回的 task.id（CLI JSON 为 `task_id`）、revision 与 project id。batch_ref 和 Issue 仅是标签，不能选定写入目标；不同批次可分别创建 task。同一角色/pane 不把两个任务合并。参与者可用 `binding attach` 主动登记角色、实际 Herdr 位置及可用 session；缺失 session 留空，绑定不授予会话或 workspace 控制权。
3. 在一个独立、持久的请求文件中写 Request。request_id 在同一库唯一，建议含项目/批次/报告者/事实序号；不可因重试而随机更换。报告只含结论摘要与必要引用，不放完整对话、终端日志或凭据。
4. 单独调用 `task report --input`，保存退出码与原 Receipt/错误。查询后核对 task/project/batch、事件 reporter、binding 参与者及 refs；读取全部页面，不能把默认第一页当全量。

例如已知 project=1、task=7、当前 revision=3，下面对象保存为调用者自己批次目录下的 `report-01.json`；示例路径和标识必须换为本次实际值：

```json
{"request_id":"project-a/batch-b/developer/fact-01","reporter":"developer","summary":"已按原流程通知接收方；这里仅补记结果引用","refs":[{"target":"/absolute/evidence/batch-b/result.json","commit":"full-verified-head-sha","result":"reported_failure","recorded_at":"2026-10-30T00:00:00Z","sha256":"sha256-of-bytes-actually-read","temporary":false}],"change":{"kind":"report","task_id":7,"expected_revision":3,"reported_state":"result_reported"}}
```

```sh
# LIGHT_BIN / LIGHT_ROOT / PROTECTED_ROOT / REPORT_FILE 均由本次调用者显式选定。
# 通知和 ACK 已由原流程处理；记录命令失败只报告记录错误。
if "$LIGHT_BIN" light --data-root "$LIGHT_ROOT" --forbid "$PROTECTED_ROOT" task report --input "$REPORT_FILE"; then
    : # 保存 Receipt，不据此授予任何业务权限。
else
    record_exit=$?
    printf 'light report failed (exit=%s); retain the original request for reconciliation\n' "$record_exit" >&2
fi

"$LIGHT_BIN" light --data-root "$LIGHT_ROOT" task list --project 1 --batch batch-b --limit 20
"$LIGHT_BIN" light --data-root "$LIGHT_ROOT" task show --project 1 --task 7 --limit 20
"$LIGHT_BIN" light --data-root "$LIGHT_ROOT" binding show --project 1 --task 7 --limit 20
```

这是独立调用示例，不是新脚本入口、守护进程或对原协作的程序依赖。正式路径按 CLI 的通用组件重叠/权限规则检查；不得把示例文件当作待自动扫描的格式。

## 冲突、补报与 ACK

| 观察到的记录结果 | 后续动作 |
| --- | --- |
| 同 request_id、同结构化内容 | 返回原 Receipt，不新增事件；即使 revision 已前进也可补报 |
| request_conflict（退出 4） | 保留原请求和差异，核对事实；不能覆盖同 ID 的内容 |
| revision_conflict（退出 4，expected/actual） | 显式查询任务与事件，由报告者判断原事实是否仍适用。保留失败请求；若需基于新 revision 作新报告，另用明确的新 request_id，不静默改写重试 |
| Busy、库离线、I/O 或输出失败 | 保留原请求，原协作照常进行；恢复后先核对，再以原 ID/内容补报。输出失败可能已经提交 |
| 停用 hook | 不再写/查库，原 RESULT→核验→ACK 继续；只按原资源归属清理 |

消息送达仅表示收到；RESULT 的 ACK 表示接收方核对了约定结果及必要证据，**不增加发布/合并/清理授权，不等于独立验收或所有需求完成，也不以写库成功为前提**。失败 RESULT 也可核验并 ACK，但不能把 verified 解读为 CI 成功或允许继续发布。以原 request_id 补报仅写记录，绝不重放开发、消息、push、merge 或清理。

## 引用的身份与保留

refs 可只指向 handoff、result、ack、审计报告中的任意必要子集；不要求三件套齐全。commit/result/recorded_at/sha256 是报告者实际核验后填写的事实或来源描述，不由 light 推断。只有读取了那些字节才能声称该读取时 SHA-256；文件后来变化只说明引用内容变化，旧哈希仍描述原读取，不自动提升/撤销业务结论。

新 head 的 handoff 与旧 head 的失败 result/ack 可以同时存在。分别按其真实 batch/head 与内容引用，summary 明说“当前 head 尚无结果，旧失败仅作历史证据”；不要按文件名、mtime、出现顺序或最新 task revision 拼成“新 head CI 已过/已合并”。查询显示的是报告与历史事件，不是对当前外部业务状态的观测。不要把 writer/slot/phase 复制为新的控制模型。

正式引用应指向仓库外持久证据位置或提交链接。只有临时引用才标 temporary=true，清理前由拥有者处理必要证据；未保存成果和真实历史批次不能为测试而删改。测试使用独立库和受控证据副本，并明确其非生产事实。
