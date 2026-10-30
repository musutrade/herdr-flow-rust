# light P1b 记录 CLI

对应 [#172](https://github.com/musutrade/herdr-flow-rust/issues/172)，在 [存储 API](light-storage.md) 上提供独立 `herdr-flow light` 路由。没有 Herdr 发现调用、消息发送、业务重试、旧格式导入或后台服务。项目根、绑定位置、session 和引用均由调用者提供；绑定不授予控制权。根帮助将记录命令与旧控制命令分开。

## 初始化与选择数据根

```sh
herdr-flow light --data-root /absolute/private/light init
herdr-flow light --data-root /absolute/private/light project register --root /existing/project --label project-a
herdr-flow light --data-root /absolute/private/light project list --limit 20
```

所有命令接受 `--data-root DIR`、可重复的 `--forbid DIR` 和 `--busy-timeout-ms N`（0..5000，默认 **250ms**）。选项可放在命令前后，不支持 `--key=value`；除 forbid 外重复选项或不适用于该命令的选项报错。缺省数据根为绝对 `$XDG_STATE_HOME/herdr-light`；变量缺失或为空时用绝对 `$HOME/.local/state/herdr-light`。相对 HOME/XDG_STATE_HOME 拒绝；显式相对路径按当前目录规范化。目录/库权限及路径重叠检查完全沿用 P1a，不扫描旧目录来构造禁止表；部署者应显式传入需保护的目录。

只有 `init` 可以创建缺失的数据根/库；查询不隐式初始化。`init` 可重复调用，返回 database_path、schema_version、sqlite_version。已有根必须私有；新根 0700、库 0600。数据根和每个 forbid 双向检查祖先/子孙重叠，备份还禁止与源根重叠。不会更改既有目录权限。项目登记规范化现存根，别名和重复登记返回**首次 id 和 label**，不会更新 label；不同 worktree 不自动合并。

## 主动报告和关联

`task add`、`task report`、`binding attach` 都要求 `--input FILE` 或 `--input -`（stdin）。文件必须是常规文件，最多读取 65536 字节；不读取输入中 refs 指向的文件或网页。成功返回 Receipt：event_id、task_id、revision、可空 binding_id。下列 JSON 可以保存到文件并传入；示例 id 应替换为实际返回值。

创建任务（无需既有 revision）：

```json
{"request_id":"batch-a-create","reporter":"developer","summary":"开始实现","refs":[],"change":{"kind":"create_task","task":{"project_id":1,"title":"Issue 172","source_ref":"https://github.com/musutrade/herdr-flow-rust/issues/172","batch_ref":"batch-a","reported_state":"in_progress"}}}
```

```sh
herdr-flow light --data-root /absolute/private/light task add --input create.json
```

更新报告（此命令只记录已经发生的事实）：

```json
{"request_id":"batch-a-report-1","reporter":"developer","summary":"本地检查完成","refs":[{"target":"https://example.test/commit/abc","commit":"abc","result":"passed","temporary":false}],"change":{"kind":"report","task_id":1,"expected_revision":1,"reported_state":"review_ready"}}
```

```sh
herdr-flow light --data-root /absolute/private/light task report --input - < report.json
```

附加绑定（缺失 session 可省略或为 null，不猜测 session）：

```json
{"request_id":"batch-a-auditor","reporter":"developer","summary":"登记审计位置","refs":[],"change":{"kind":"attach_binding","task_id":1,"expected_revision":2,"binding":{"role":"auditor","herdr_location":{"service":"local","workspace":"wR","pane":"p3"},"session":null,"cwd":"/absolute/audit-worktree"}}}
```

```sh
herdr-flow light --data-root /absolute/private/light binding attach --input binding.json
```

可用 session 的结构为 `{"agent":"hermes","kind":"id","source":"herdr:hermes","value":"opaque-session-id"}`。允许同角色多个绑定；不检查 Herdr 是否在线，不启动/接管会话。CLI 不暴露 UpdateBinding 或 SetResponsible 操作。文本长度等约束沿用存储 API。未知 JSON 字段、缺少必需字段、重复结构字段、错误类型及不匹配的 change.kind 均拒绝。

request_id 在库内唯一。相同结构化内容重试返回原 Receipt，不增加事件，不要求当前 revision 仍等于旧 expected_revision；同 ID 不同内容返回 request_conflict。比较包含 summary、refs（含数组顺序）及预期 revision；空白或 JSON 对象键顺序不影响幂等。新请求才检查 revision。不同批次/项目应使用独立 request_id；Issue、batch_ref、role、pane 都只是标签，不能用来选定写入任务。初始 revision=1，报告/关联成功后递增。

## 查询与分页

```sh
herdr-flow light project list --after-id 0 --limit 20
herdr-flow light task list --project 1 --batch batch-a --after-id 0 --limit 20
herdr-flow light task show --project 1 --task 1 --after-id 0 --limit 20
herdr-flow light binding show --project 1 --task 1 --after-id 0 --limit 20
```

这些例子使用缺省库；选择其他库时仍需 `--data-root`。task/binding 查询必须显式指定正整数 project；show 还需 task。无 batch 筛选表示项目全部任务（包括空标签）。分页按 id 升序，after-id（默认 0）为排除式游标，limit 默认100、范围1..100。页面返回 `{items,next_after_id,limit}`；不足 limit 时 next_after_id=null，满页时返回最后 id，最后一页刚好满时还需查询一次空页。无跨请求快照保证，查询期间新记录可能出现在后续页。

`task show` 返回 `{task,events:页面}`，游标针对事件；binding show 游标针对 binding。沿用 P1a：未知 project 的 task list、未知 task 的 binding show 返回空页面；task show 未知 task 报 not_found。若 task 存在但不属于指定项目，两个 show 都报 not_found，不泄露其他项目内容。show 里的 revision 是当前摘要版本，与历史事件自己的 revision 区分。

## 输出、错误与补报

成功：退出 0，stdout 单行 `{"ok":true,"data":...}`，stderr 空。帮助为人类可读文本，退出 0。错误：非零退出，stderr 单行 `{"ok":false,"error":{"code":"...","message":"..."}}`，正常情况下 stdout 空；message 供人阅读，调用者应按 code 分支。

| 退出码 | error.code | 含义 |
| --- | --- | --- |
| 2 | usage / invalid_input | 参数、JSON、字段/路径/权限约束错误 |
| 3 | not_found | 写入关联对象缺失，或详情任务不属于指定项目 |
| 4 | request_conflict | request_id 内容不一致 |
| 4 | revision_conflict | 包含 expected / actual；重新阅读后由调用者决定新报告 |
| 5 | busy | SQLite Busy/Locked；等待不超过配置时限，无自动重试 |
| 6 | unsupported_schema | 包含 version，不自动迁移/降级 |
| 6 | read_only / storage_full / storage_unavailable | SQLite/文件访问故障（包括输入文件 I/O 错误） |
| 6 | output_unavailable | 输出失败，写入可能已经提交，stdout 也可能只写出部分 JSON |

数据库不可用时明确报告记录失败，原开发、沟通、push/merge 可继续。保存原请求，恢复后按原 request_id 和原内容补报；CLI 只重试记录，不执行这些业务动作。调用者不应把记录失败自动映射为业务重做。stdout/连接中断也可能发生在提交以后，此时按原请求查询/补报获得原 Receipt；不要生成新 request_id 来盲试。

## 备份与验证边界

```sh
herdr-flow light --data-root /absolute/private/light backup --output-root /absolute/private/snapshot
herdr-flow light --data-root /absolute/private/snapshot task list --project 1
```

备份是 SQLite 在线一致性副本，不直接复制活跃 WAL；目标库不可覆盖。恢复是把副本根作为数据根打开，需保留原备份时先 backup 到另一个新根。复制/校验失败不发布库；最终发布后 sync/权限复核失败时可能已有目标，应先核验，不盲目重复。详见存储 API 文档。

`tests/light_cli.rs` 通过真实二进制验证两项目/多批次、同 pane 多任务、多角色/无 session、冲突、Busy、schema、路径保护、缺库失败后幂等补报及备份查询。环境清空 Herdr 配置，PATH 放置 herdr/git/gh/curl/herdr-flow 替身，捕获任何业务进程调用；在旧系统完全缺席和存在坏旧状态哨兵两种环境运行。边界测试解析整个 src/light 模块的 Rust AST，限制仓库依赖闭包，并用 strace 跟踪所有命令的文件/进程/网络系统调用，拒绝旧路径访问、目录扫描和衍生进程。Linux 测试要求 `/usr/bin/strace`，CI 显式安装。静态闭包核对与动态路径测试互补，不宣称覆盖任意未来代码或构成 OS 沙箱；真实断电/宿主磁盘满不在本项 CLI 验收内。
