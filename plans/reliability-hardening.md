# Plux 稳定性加固计划

状态：`进行中`

本计划源自 2026-07-23 的源码审计与隔离进程测试。目标不是增加功能，而是让
detach、SSH 断线、接管、鼠标/键盘输入和高输出场景符合 Plux 的基本承诺：
客户端问题不能摧毁 daemon 或 session，输入字节流不能依赖 read 边界。

## 状态规则

| 状态 | 含义 |
|---|---|
| `待开始` | 未修改实现或测试。 |
| `进行中` | 正在实施，尚未通过全部验收。 |
| `已完成` | 已通过本任务的专项验证和完整回归。 |
| `阻塞` | 需要外部环境或明确决策才能继续。 |

## 范围与非目标

本轮覆盖本地 Unix daemon/client、SSH 断线后的本地 client 行为、终端输入流、
渲染压力和历史搜索。不会实现远程 daemon、Windows、插件、终端图形协议或
daemon 崩溃后的 shell 恢复。重点是防止普通客户端 I/O 错误造成 daemon 崩溃。

## 总览

| 编号 | 批次 | 任务 | 状态 | 依赖 |
|---:|---|---|---|---|
| 0 | 准备 | 固化审计复现为自动化回归测试 | 已完成（审计） |— |
| 1 | 稳定性 | 客户端 I/O 错误不终止 daemon | 已完成 | 0 |
| 2 | 稳定性 | `attach --force` 完整终止旧连接 | 已完成 | 1 |
| 3 | 稳定性 | 区分交互 attach 与短命令；原子启动 daemon | 已完成 | 0 |
| 4 | 稳定性 | 统一输入转义序列流解析 | 已完成 | 0 |
| 5 | 兼容 | 协议版本与升级失败提示 | 已完成 | 1 |
| 6 | 性能 | 有界渲染与行级差分 | 已完成 | 1, 3 |
| 7 | 性能 | 非阻塞历史顶部与搜索 | 已完成 | 6 |
| 8 | 验收 | SSH、全屏应用和压力回归矩阵 | 进行中 | 1–7 |
| 9 | 稳定性 | 审计发现的接管、退出和最终输出顺序 | 已完成 | 0–7 |
| 10 | 兼容 | alternate screen 鼠标滚轮分页路由 | 已完成 | 4 |
| 11 | 语义 | 对齐 attach 与显式创建语义 | 已完成 | 2, 3 |
| 12 | 生命周期 | 安全启动、接管预检与停止 daemon | 已完成 | 2, 3, 11 |
| 13 | client | 断线诊断、状态栏恢复与有界事件背压 | 已完成 | 12 |

## 0. 审计基线

状态：`已完成`

已确认的复现：

- 活跃 attach 期间，一个未完成协议帧的 Unix socket peer 断开可使 daemon 退出。
- `attach --force` 后旧 client 不会收到 EOF，仍卡在 alternate screen。
- 将 `ESC[5~` 分两次输入会转发到 shell，不触发滚动。
- 32 个并发 `plux list` 中至少 10 个收到“another client is already attached”。

基线检查已通过：`cargo fmt --check`、`cargo clippy --all-targets -- -D warnings`、
`cargo test --all-targets`（39 passed）和
`cargo check --manifest-path fuzz/Cargo.toml`。

## 1. 客户端 I/O 错误不终止 daemon

状态：`已完成`

涉及文件：`src/daemon.rs`、`tests/daemon_lifecycle.rs`（新增）。

实施：

1. 将“对当前客户端的读/写失败”归类为连接结束，而不是 daemon 事件循环错误。
2. 让快照、错误响应、attach 响应和拒绝响应在写失败后统一 detach，并继续处理
   session 与其他连接。
3. 保持真正的 daemon 内部错误可见；若必须退出，统一调用 session 清理，而不是
   只删除 socket。
4. 为 raw Unix peer 的提前断开建立端到端回归：活跃 attach、竞争连接断开、原
   session 仍可列出和重新 attach。

验收：

```bash
cargo test daemon_lifecycle -- --nocapture
cargo test --all-targets
cargo clippy --all-targets -- -D warnings
```

成功标准：任一客户端在协议响应或快照期间消失后，daemon 仍存活；session 仍在
内存且可重新 attach。

## 2. 强制接管完整关闭旧连接

状态：`已完成`

涉及文件：`src/daemon.rs`、`tests/daemon_lifecycle.rs`。

实施：

1. 在替换 active client 前关闭旧 Unix socket 的两个方向，唤醒旧 client reader
   和旧 client 的 server reader。
2. 仅在 socket 已关闭后安装新 client，保留 client id 过滤作为最后一道竞态保护。
3. 回归测试使用两个真实 UnixStream：B takeover 后，A 必须在限定时间内得到 EOF；
   B 仍能收到 Attached 与 Snapshot。

成功标准：旧终端自动退出 alternate screen；没有遗留 reader 线程或无效 client。

## 3. 连接准入与原子启动

状态：`已完成`

涉及文件：`src/socket.rs`、`src/daemon.rs`、`tests/daemon_lifecycle.rs`。

实施：

1. 仅将 interactive attach 视为独占资源；`list`、`new`、`kill` 等短请求必须能在
   active attach 存在时完成或得到明确、稳定的序列化行为。
2. 用 bind 的原子性处理 daemon singleton：只在确认 socket 为残留且无法连接时
   删除，再重试 bind；禁止“任意 connect 失败即删除 socket”。
3. `connect_or_start` 在竞争启动时优先等待可连接 daemon，不把正常竞争当作错误。
4. 回归覆盖 32 个并发 list 与并发冷启动，断言所有请求成功且最终仅存在一个可达
   daemon。

成功标准：并发短命令不出现“another client”；socket 不会因启动竞争被误删。

## 4. 输入转义序列流解析

状态：`已完成`

涉及文件：`src/client.rs`、`docs/keybindings.md`。

实施：

1. 将普通模式、滚动模式、SGR/X10 鼠标和 PageUp/PageDown 放入同一个增量解析状态，
   不再假定一次 `stdin.read` 包含完整序列。
2. 保留完整且未知的终端序列原样转发；只有被明确识别的 Plux 动作才拦截。
3. 将单独 Escape 的语义与 CSI 序列区分，避免固定 50ms 在 SSH 延迟下退出滚动模式。
4. 为逐字节、任意切分、超时和应用鼠标捕获添加单元测试。

成功标准：`ESC[5~`、箭头、SGR/X10 滚轮无论如何切分都产生同一动作；未知序列不
污染 shell 输入。

## 5. 协议升级边界

状态：`已完成`

涉及文件：`src/protocol.rs`、`src/socket.rs`、`README.md`。

实施：

1. 为不兼容协议修改提升版本，并让 client 将版本不匹配转换为可执行的诊断信息。
2. 定义开发版升级策略：旧 daemon 必须被识别，不能把未知 enum 变体表现为随机
   的“连接关闭”。
3. 添加旧版本帧与未知消息的协议测试。

成功标准：更新二进制后遇到旧 daemon 时，用户能明确知道需结束/重启 daemon，
而不是误判为 attach 失败。

## 6. 有界渲染与行级差分

状态：`已完成`

涉及文件：`src/session.rs`、`src/daemon.rs`、`src/client.rs`、性能测试。

实施：

1. `pending_snapshots` 按 session 合并刷新请求，保持最新状态优先。
2. Unix client socket 使用 250ms 写超时；慢或断开的 client 不再无限期阻塞 daemon。
3. Session 缓存每个 pane 的上次行内容；attach、resize、分屏和焦点布局变化发送全量
   首帧，普通刷新只发送变化行，协议仍复用 ANSI `Snapshot.data`。

成功标准：常规 shell 输入不重绘整个屏幕；`yes`/编译输出期间仍可及时 detach、
输入和滚动。

## 7. 非阻塞历史顶部与搜索

状态：`已完成`

涉及文件：`src/terminal.rs`、`src/daemon.rs`、终端测试。

实施：

1. 已使用 `set_scrollback(usize::MAX)` 直接到达解析器允许的历史顶部，移除逐偏移探测。
2. 搜索已移除完整 offset 向量分配，并由 daemon 按每轮 64 个历史位置分片执行，保持
   主循环可处理输入、detach 和 pane 输出。
3. 20,000 行默认上限已有配置和大输出回归；真实 daemon 搜索结果回归已通过。

成功标准：`g`、`/`、`n/N` 不让 attach、resize 或 detach 长时间失去响应。

## 8. 兼容性与交付验收

状态：`进行中`

涉及文件：`tests/`、`docs/compatibility.md`、`README.md`、`CHANGELOG.md`。

实施：

1. 将本计划所有复现写为可重复测试，避免依赖人工终端观察。
2. 在具备环境时执行 Zsh、Bash、Vim/Neovim、Less、Top/HTop、tmux 外层和真实 SSH
   中断/重连 smoke test；缺少环境时明确记录，不把未测写成已兼容。
3. 更新快捷键、`attach --force`、升级和已知限制文档。
4. 最终运行格式化、完整测试、严格 clippy、fuzz crate 检查和 release build。

验收命令：

```bash
cargo fmt --check
cargo test --all-targets
cargo clippy --all-targets -- -D warnings
cargo check --manifest-path fuzz/Cargo.toml
cargo build --release
```

## 9. 审计修复：接管、退出与最终输出顺序

状态：`已完成`

实施：接管前校验 session 名称；PTY reader 在 EOF 后再发送退出事件；daemon 先发送最终 snapshot；`ProcessExited` 标明整个 session 是否结束，client 只在全部 pane 退出时关闭。

成功标准：非法接管不影响已有 client；短命令的最终输出在退出前可见；分屏中关闭一个 pane 不离开其他 pane。

## 10. alternate screen 鼠标滚轮

状态：`已完成`

实施：在 alternate screen 且不在 Plux 选择模式时，将 SGR/X10 滚轮转换为 PageUp/PageDown 转发给应用；不将原始鼠标报告注入未启用鼠标协议的应用。

成功标准：Codex、Vim、Less 等应用可收到分页键，普通 shell 保持 Plux scrollback 行为。

## 11. attach 与显式创建

状态：`已完成`

实施：普通 `attach`、`list`、`kill` 只连接既有 daemon；`new`、`run` 和
`attach --create` 才允许启动 daemon。daemon 不再为缺失的 session 隐式创建 shell。

成功标准：缺失 session 的 attach 明确失败且不创建 session；显式创建仍可进入；测试
不会遗留无人管理的 daemon。

## 12. 基础生命周期

状态：`已完成`

实施：只在 socket 缺失或拒绝连接时启动 daemon；`attach --force` 在断开旧 client 前
验证目标 session；新增 `plux stop`；裸 `plux` 等价于创建或进入 `default`。

成功标准：权限错误不触发 daemon 启动；错误接管不影响当前 client；stop 后 socket 和
pane 均被清理。

## 13. client 断线与背压

状态：已完成

实施：区分 server 断线与 stdin/signal 关闭；断线返回明确错误；撤销输入状态栏时重绘
底行；client socket 写入 250ms 超时；事件通道限制为 128 项并采用阻塞背压。

队列饱和只表示 client 渲染落后，不能作为 client 已死亡的依据。存活仍由 Unix socket
的 EOF、读错误或写错误判断；SSH 黑洞下进程仍持有 socket 时保留 force attach 接管。

## 完成记录

| 日期 | 任务 | 验证 | 结果 |
|---|---|---|---|
| 2026-07-23 | 0. 审计基线 | 静态审计、隔离 socket/PTY/并发测试、39 项现有测试 | 已完成 |
| 2026-07-23 | 1. 客户端 I/O 错误不终止 daemon | `cargo test --test daemon_lifecycle`、全量 52 tests、严格 clippy | 已完成 |
| 2026-07-23 | 2. 强制接管关闭旧连接 | 双 UnixStream takeover 回归，旧连接 EOF | 已完成 |
| 2026-07-23 | 3. 连接准入与原子启动 | 活跃 attach 下 `list/new/kill`、32 并发冷启动 | 已完成 |
| 2026-07-23 | 4. 输入转义序列流解析 | 分片 PageUp、SGR 鼠标、未知 CSI 单元测试 | 已完成 |
| 2026-07-23 | 5. 协议升级边界 | 版本、未知消息、超限帧测试；协议版本 2 | 已完成 |
| 2026-07-23 | 6. 有界渲染与行级差分 | 250ms 写超时、快照合并、Session 行缓存、52 项全量回归 | 已完成 |
| 2026-07-23 | 7. 非阻塞历史顶部与搜索 | 顶部直接定位、搜索移除 offset 向量、64 项分片搜索、daemon 回归 | 已完成 |
| 2026-07-23 | 8. 兼容性与交付验收 | Bash/Zsh/Vim/Less/Top/Htop/tmux PTY smoke、全量构建检查 | 进行中：本机 SSH 公钥认证失败，未执行中断重连 |
| 2026-07-23 | 9. 审计修复：接管、退出与最终输出顺序 | 三条 daemon/socket 回归；`cargo test --all-targets`（62 passed）；严格 Clippy；release build | 已完成 |
| 2026-07-23 | 10. alternate screen 鼠标滚轮 | SGR/X10 单元测试，将滚轮映射为 PageUp/PageDown；`cargo test --all-targets`（62 passed） | 已完成 |
| 2026-07-24 | 11. attach 与显式创建 | 缺失 session attach、无 daemon list、受控并发创建回归；`cargo test --all-targets`（64 passed）；严格 Clippy；release build | 已完成 |
| 2026-07-24 | 12. 基础生命周期 | socket 错误分类、错误 force 保留旧 client、stop 清理 daemon/socket；`cargo test --all-targets`（67 passed）；严格 Clippy；release build | 已完成 |
| 2026-07-24 | 13. client 断线与背压 | server EOF、状态栏恢复回归；128 项有界背压、250ms client 写超时；69 项全量测试、严格 Clippy、release build | 已完成 |
