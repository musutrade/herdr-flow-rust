# light P1a 记录存储 API

对应 [#171](https://github.com/musutrade/herdr-flow-rust/issues/171) 与 [P0 边界](light-p0-baseline.md)。实现基线为 `e808f19880b5171745a3995f74becbb5c06ab3f9`。这是进程内 Rust API；没有新增 CLI、观察器、消息发送、旧格式读取或业务控制动作。P1b 再接 CLI。

## 调用与一致性

入口为 `herdr_flow::light::{Store, Options, Request, Change}`。调用者显式传入数据根和禁止目录；本模块不读环境配置、不发现旧任务。`Options::new(root)` 默认可写、busy_timeout 为 250ms，最大允许 5s；`read_only=true` 仅打开已存在的库。每个进程/调用按需打开连接并及时 drop，不要求常驻服务。

1. `register_project(existing_root, label)` 用 Linux canonicalize 登记现存目录，别名返回原 id/label；不自动把不同 worktree 合并。`projects(Page)` 分页读取。
2. `apply(Request { request_id, reporter, summary, refs, change })` 提交报告。`CreateTask` 指定 project_id，每个独立批次调用一次（不同 request_id），产生独立 task.id；source_ref 和可空 batch_ref 不唯一。
3. `Report`、`AttachBinding`、`UpdateBinding`、`SetResponsible` 均指定 task_id 和 expected_revision。只有 CreateTask 不需既有 revision；初始 revision=1。关联修改、负责人和摘要变化与事件一起递增 revision。相同角色可以登记多次，session 可空；负责人可以清空，只能指向本任务 binding。
4. `task(id)`、`tasks(project_id, optional_batch, Page)`、`bindings(task_id, Page)`、`events(task_id, Page)` 查询记录。Page 是 `after_id` + `limit`（1..=100）的稳定 id 排序分页。None batch 表示该项目全部批次；返回值中的 batch_ref 仍能区分无批次标签的任务。

所有修改使用 SQLite IMMEDIATE 短事务和外键。先查 request_id：同一结构化请求重试返回原 Receipt（event_id/task_id/revision/binding_id），即使当前 revision 已前进；同 ID 不同请求返回 RequestConflict。请求比较包括预期 revision、报告者、摘要、refs 和 Change；数组顺序也是内容的一部分。新请求才检查 revision，冲突返回 expected/actual，不自动覆盖、合并或重放。创建项目按 canonical_root 唯一约束去重。

`reported_state` 是有界、不做业务推断的报告字符串；binding 的服务/workspace/pane、session、cwd 只是来源记录，不授予 writer 或会话控制权。文件 refs 不被打开，不按哈希、mtime 或链接内容自动更新报告。CI refs 可以只带 URL、commit、result、recorded_at；没有 config_hash 要求。记录失败由调用者决定是否以同 request_id 补报，不触发任何开发/push/merge 操作。

## 四表与边界

schema 使用 STRICT 表、复合外键和 `PRAGMA user_version=1`；表为 projects/tasks/bindings/events。events 内增加两个内部 TEXT 字段 `request_json` 和 `receipt_json`，用于保存有界的原请求与幂等返回值，兼作关联/负责人修改历史；不增加第五张表或状态机字段。它们不是原始对话或终端日志。

API 以 UTF-8 字节限制输入：summary 8192、title 512、request_id/reporter/项目 label/batch 256、reported_state/role 128、路径/URL 4096。refs 最多 32 项且序列化后不超过 32768 字节；完整请求最多 65536 字节。数据库另有非空、类型、长度、JSON 合法性与外键约束。refs target 仅接受绝对路径或 http(s) 前缀；不验证远端可达性，也不获取远端内容。数据库不可作为不可信同 UID 进程之间的身份认证边界。

## 路径、权限和 WAL

数据根与每个显式禁止目录规范化后按路径组件双向检查：相等、任一为另一祖先均拒绝。可解析已有符号链接；悬空链接报错，未存在的尾部按现存祖先解析。禁止目录可为空；旧系统完全缺席也可初始化。

目录创建逐组件使用 openat/mkdirat 和 O_NOFOLLOW，打开时保存目录 inode/device，在每次操作前复核原别名、禁止目录及目录身份。SQLite 以 SQLITE_OPEN_NOFOLLOW 打开规范化路径，数据文件以绑定目录 fd 创建；备份临时文件和最终发布也绑定目录 fd。已打开后路径被替换或别名改变会拒绝后续 API 操作。数据库、WAL/SHM/journal 的符号链接、硬链接及非私有文件被拒绝。相似字符串前缀但不同组件不算重叠。

新专用目录 0700，数据库与备份 0600；SQLite 按数据库权限建立 WAL/SHM。已有数据根或库权限不符合要求时拒绝，不 chmod；既有父目录保持不变。先核验侧文件再打开，连接前后和操作前再次检查。恶意同 UID 进程仍能直接修改/移动自己的文件：这不是操作系统沙箱，也不承诺抵抗任意同 UID 并发篡改；调用者须保有这些专用目录的写入归属。

WAL 使用 `synchronous=FULL`，启用 foreign_keys、关闭 trusted_schema，临时 SQL 数据放内存。只支持同机本地存储；检测到 NFS、SMB/CIFS、FUSE 则拒绝，其他文件系统仍由部署者确认本地语义。文件系统故障或只读连接错误原样保留在 `Error::Sqlite` 中，调用者可区分 Busy/ReadOnly/DiskFull 等。无全局锁、无旧状态标记扫描。

## 备份、恢复和版本

`Store::backup(new_root, additional_forbidden)` 使用 SQLite 在线备份 API；源数据根也自动加入备份禁止范围。分段复制有 5s 总时限；失败不发布 `records.sqlite3`。在私有临时文件上完成 integrity_check、foreign_key_check 和 schema 检查，关闭目标连接、sync 后以不覆盖方式发布最终文件名，再 sync 目录。已有目标库不会被覆盖；失败可能留下新建的空目标目录，调用者按归属处理。

恢复方式是把已验证备份根传给 `Store::open`，重新启用 WAL 后即可查询/补报；若需保留备份不动，先将它以只读 Store 打开，再调用 backup 到另一个新根并打开该根。测试对比恢复后的摘要、refs、事件、revision、schema 及原请求重试结果。

版本 0 只允许无用户对象的空库初始化；未知版本（包括未来更高版本）明确拒绝，不自动降级或改 schema。本阶段没有旧 light schema 可迁移。将来新增迁移必须先通过此在线备份机制生成可恢复副本，再增加显式版本迁移；本版本不能用于升级已有非空版本 0 库。

## 依赖与核验

锁文件解析：`rusqlite 0.41.1`（bundled、backup）、`libsqlite3-sys 0.39.1`；bundled SQLite **3.54.1**，不依赖本机 sqlite3 命令或系统 SQLite 动态库。由 C 工具链编译内置 amalgamation；构建依赖 cc 1.10.0、find-msvc-tools 0.1.15、shlex 2.0.1、pkg-config 0.3.34、vcpkg 0.2.15。运行依赖新增 fallible-iterator、fallible-streaming-iterator、smallvec 和 hashlink/hashbrown/foldhash；锁文件还包括未在当前 Linux 构建树启用的 WASM 目标依赖。准确版本与校验和以 Cargo.lock 和 `cargo tree --locked -p rusqlite` 为准。

2026-10-30 执行 cargo audit：RustSec 数据库提交 `ee1a49a2769a24a596afc1c20cb33748d2769586`，55 个锁定依赖，0 漏洞、无警告。此结论有时间范围，不代表不存在未知漏洞。[SQLite 3.54.1 发布说明](https://www.sqlite.org/releaselog/3_54_1.html)记录了 32 位构建堆缓冲区溢出修复；本次 bundled 版本已包含该补丁。[SQLite 安全建议](https://www.sqlite.org/security.html)和[在线备份 API](https://www.sqlite.org/backup.html)用于核对配置和备份行为。构建不安装或替换运行中的工具/skill。

行为测试见 `tests/common/light`：项目别名和隔离、同 Issue 独立 task、多角色和空 session、负责人复合外键、幂等/内容冲突、分页与输入限制、两个真实子进程竞争 revision、SIGKILL 中断事务、真实 SQLite Busy/ReadOnly/SQLITE_FULL 错误、事件插入失败后摘要/关联/负责人回滚、并发报告期间备份恢复、schema 拒绝、权限与路径别名/替换/硬链接/侧文件拒绝。SQLITE_FULL 由 max_page_count 触发，不填满宿主磁盘；事件失败用测试触发器注入；SIGKILL 使用仅编译到测试二进制的事务中间暂停点。这些均不调用业务控制面，不把模拟文件系统故障说成真实宿主断电验收。

沿用 fmt、lint-policy、Clippy -D warnings、all-targets 测试、release、绝对 CRAP ≤10（无 baseline）和依赖审计；具体候选运行结果在该批审计/发布证据中核对，不能用本文清单代替检查通过。
