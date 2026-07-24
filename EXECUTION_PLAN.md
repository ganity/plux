# Plux 执行计划

本文档将 [DESIGN.md](./DESIGN.md) 拆解为可执行、可验证的开发任务。

## 1. 执行原则

- 每次只推进一个主要任务。
- 每个任务完成后，先运行对应验证，再更新本文档状态。
- 未通过验证的任务不能标记为完成。
- 所有实现优先复用成熟库，不从零实现 PTY 或 VT parser。
- 第一阶段只支持 Unix/Linux 和 macOS。
- 暂不实现插件、远程 session、Windows 和图形协议。
- 每个任务保持最小改动，避免提前设计未来功能。

## 2. 状态定义

| 状态 | 含义 |
|---|---|
| `待开始` | 尚未实现 |
| `进行中` | 正在实现，可能有未完成代码 |
| `阻塞` | 依赖外部条件或明确技术问题，暂时无法继续 |
| `已完成` | 实现完成且通过该任务的验证 |
| `不适用` | 经评估后从当前版本移除 |

## 3. 总体状态

| 编号 | 任务 | 状态 |
|---:|---|---|
| 0 | 仓库和工具链检查 | 已完成 |
| 1 | Rust 工程和 CLI 骨架 | 已完成 |
| 2 | PTY 与终端状态核心 | 已完成 |
| 3 | daemon/client 协议 | 已完成 |
| 4 | 单 pane 交互闭环 | 已完成 |
| 5 | scrollback、滚动、搜索和复制 | 已完成 |
| 6 | 分屏和 pane 生命周期 | 已完成 |
| 7 | 配置、session 元数据和异常处理 | 已完成 |
| 8 | 性能、兼容性和模糊测试 | 已完成 |
| 9 | 最终验收和使用文档 | 已完成 |
| 10 | 稳定性加固：任务 1-7 已完成，任务 8 验收进行中 | 进行中 |
| 11 | alternate screen 分页键路由 | 已完成 |
| 12 | 终端稳定性专项审计：接管、pane 退出和最终输出顺序 | 已完成 |
| 13 | alternate screen 鼠标滚轮分页路由 | 已完成 |
| 14 | 对齐 tmux/Zellij 的 attach 与显式创建语义 | 已完成 |
| 15 | 基础生命周期对齐：安全启动、接管预检与停止 daemon | 已完成 |
| 16 | client 稳定性：断线诊断、状态栏恢复与事件背压 | 已完成 |
| 17 | 本地 client + SSH bridge：自动重连、连接代际与心跳租约 | 进行中 |

## 4. 任务 0：仓库和工具链检查

状态：`已完成`

### 目标

确认工作目录、Rust 工具链和依赖下载能力可用。

### 已确认

- 工作目录：`/home/jhz/tools/plux`
- 当前仓库只有设计文档
- `cargo`、`rustc` 和 `rustfmt` 可用
- `vt100`、`portable-pty` 和 `crossterm` 可以从 crates.io 获取
- 当前没有 `.codegraph/`，不使用 CodeGraph

### 验证

```bash
cargo --version
rustc --version
cargo search vt100 --limit 1
cargo search portable-pty --limit 1
```

## 5. 任务 1：Rust 工程和 CLI 骨架

状态：`已完成`

### 目标

建立可编译的 Rust binary，并定义最小 CLI：

```bash
plux
plux new <name>
plux attach <name>
plux list
plux kill <name>
```

### 预计文件

```text
Cargo.toml
src/main.rs
src/cli.rs
src/config.rs
src/error.rs
```

### 实现内容

- 初始化 Cargo binary 项目。
- 使用 `clap` 或手写参数解析；如果命令数量保持很少，优先使用标准库解析。
- 定义统一错误类型。
- 读取 `$XDG_CONFIG_HOME/plux/config.toml`，不存在时使用默认值。
- 定义运行目录：优先 `$XDG_RUNTIME_DIR/plux-<user>`，否则使用系统临时目录下的 `plux-<user>`。
- 创建安全目录，权限设置为用户私有。
- CLI 对未知命令返回清晰错误，不 panic。

### 默认配置

```text
default_shell = $SHELL 或 /bin/sh
prefix = Ctrl-A
scrollback_lines = 20000
scrollback_bytes = 64MB
mouse = true
refresh_rate = 60
```

### 完成标准

- `cargo build` 成功。
- `plux --help` 输出命令说明。
- `plux list` 在没有 daemon 时给出可理解结果。
- 配置文件不存在时可以正常启动。
- 配置目录和运行目录创建失败时返回错误。

### 验证命令

```bash
cargo fmt --check
cargo check
cargo run -- --help
cargo run -- list
```

## 6. 任务 2：PTY 与终端状态核心

状态：`已完成`

### 目标

启动一个 shell，将 PTY 输出交给成熟终端解析器，保存屏幕和历史状态。

### 预计文件

```text
src/pty.rs
src/terminal.rs
src/scrollback.rs
src/pane.rs
```

### 依赖

- `portable-pty`：创建 PTY、启动 shell、读取输出和发送 resize。
- `vt100` 或等价终端状态库：解析 ANSI/VT 数据并维护 screen。

### 实现内容

- 通过当前用户的 `$SHELL` 启动交互式 shell。
- 设置初始 PTY 尺寸。
- 设置 `TERM=xterm-256color` 和 `PLUX_*` 环境变量。
- 为每个 pane 创建 PTY reader 和 writer。
- 读取 PTY 数据时按块处理，不按字符读取。
- 处理 EOF、子进程退出和 reader 错误。
- terminal state 独立于 PTY 生命周期保存。
- 支持 primary screen 和 alternate screen。
- 限制 scrollback 行数和内存大小。

### 事件模型

```text
PtyReader -> OutputEvent(pane_id, bytes)
PtyWaiter  -> ProcessExited(pane_id, status)
Server     -> 更新 TerminalState
```

### 完成标准

- 可以启动 shell 并显示 prompt。
- 输入命令后可以看到输出。
- ANSI 颜色、清屏、光标移动至少正常工作。
- `printf` 输出中文不会 panic。
- Vim 或 Less 进入和退出备用屏幕后状态正确。
- PTY EOF 后 pane 被标记为退出，而不是让 daemon 崩溃。

### 验证命令

```bash
cargo test terminal
cargo test pty
cargo run -- run -- printf '中文\\n'
cargo run -- run -- sh -c 'printf "\\033[31mred\\033[0m\\n"'
```

## 7. 任务 3：daemon/client 协议

状态：`已完成`

### 目标

将 PTY 和 terminal state 放入后台 daemon，client 通过 Unix socket 连接。

### 预计文件

```text
src/daemon.rs
src/protocol.rs
src/session.rs
src/socket.rs
```

### 协议版本

第一版采用长度前缀帧：

```text
[u16 protocol_version][u32 payload_length][UTF-8 JSON payload]
```

控制消息可以使用 `serde` 序列化。协议只用于本地 socket，不开放 TCP。

### Client -> Server

```text
Hello
Attach { session }
CreateSession { session }
Input { pane_id, bytes }
Resize { cols, rows }
Detach
List
Kill { session }
```

### Server -> Client

```text
Hello
Attached { session, pane_id }
FullSnapshot
ScreenDiff
SessionList
ProcessExited
Error
```

### 实现内容

- daemon 监听一个用户私有 Unix socket。
- client 连接失败时可以启动 daemon 后重试一次。
- socket 断开不影响 session 和 shell。
- 同一 session 第一版只允许一个 attached client。
- attach 时发送完整屏幕快照。
- 后续只发送屏幕变化或完整帧，先保证正确性，再优化 diff。
- 协议版本不匹配时拒绝连接。

### 完成标准

- daemon 可以后台运行。
- client 可以 attach 已存在 session。
- client 强制退出后，shell 仍然运行。
- 再次 attach 后可以继续输入。
- 无效 session、协议版本错误和 socket 错误都有明确提示。

### 验证命令

```bash
cargo test protocol
cargo run -- new test-session
cargo run -- list
cargo run -- attach test-session
```

## 8. 任务 4：单 pane 交互闭环

状态：`已完成`

### 目标

让 Plux 作为一个完整的单 pane 终端运行，具备输入、渲染、resize 和安全退出。

### 预计文件

```text
src/input.rs
src/render.rs
src/client.rs
src/terminal_guard.rs
```

### 实现内容

- client 进入 raw mode。
- 使用外层终端尺寸初始化 pane。
- 原始输入字节发送给 daemon。
- daemon 处理前缀键，其他输入原样写入 PTY。
- daemon 输出 terminal snapshot。
- client 使用 ANSI 控制序列渲染。
- 定期检查终端尺寸变化。
- 使用 RAII guard 恢复 raw mode、光标和 alternate screen。
- client 收到 SIGTERM、SIGINT、EOF 时执行清理。

### 输入策略

- 普通字节原样转发。
- `Ctrl-A` 进入 prefix 状态。
- prefix 后的命令由 daemon 处理。
- 未知 prefix 命令提示并返回普通模式。
- 必须保证用户输入不会因为渲染刷新而丢失。

### 完成标准

- shell 交互体验接近直接运行 shell。
- Ctrl-C、Ctrl-D、Tab、方向键、退格正常。
- resize 后 `stty size` 输出正确。
- client 退出后外层终端光标和输入模式正常。
- daemon 运行时 client 可以重新连接。

### 验证命令

```bash
cargo run -- new interactive
cargo run -- attach interactive
stty size
printf '\033[31mred\033[0m\n'
```

## 9. 任务 5：scrollback、滚动、搜索和复制

状态：`已完成`

### 目标

解决项目最核心的用户痛点：历史查看和复制。

### 预计文件

```text
src/scrollback.rs
src/search.rs
src/copy_mode.rs
src/input.rs
src/render.rs
```

### 实现内容

- 每个 pane 单独维护历史。
- 支持按行数和字节数限制。
- 保留逻辑行和物理换行信息。
- `Ctrl-A [` 进入 scroll mode。
- PageUp/PageDown、Home/End、上下箭头滚动。
- 普通模式下 PageUp/PageDown 和鼠标滚轮直接进入 scroll mode。
- 新输出时保持用户当前 viewport。
- 显示未读输出行数。
- `/` 搜索，`n/N` 跳转结果。
- 支持字符、整行和矩形坐标选择复制。
- 复制时输出清理后的逻辑文本。
- 使用系统复制命令，缺失时回退为内部剪贴板提示。

### 暂不实现

- 正则搜索
- 跨 session 搜索
- 搜索结果持久化
- 富文本剪贴板

### 完成标准

- 生成至少十万行输出后可以稳定滚动。
- 用户滚动时不会被新输出拉回底部。
- 中文、宽字符和长行复制正确。
- 搜索不会修改 pane 的实际 shell 状态。
- 退出 copy mode 后应用可以继续正常输入。

### 验证命令

```bash
seq 1 100000
printf '%s\\n' '中文日志测试'
```

## 10. 任务 6：分屏和 pane 生命周期

状态：`已完成`

### 目标

在单 pane 稳定后加入最小可靠分屏功能。

### 预计文件

```text
src/layout.rs
src/session.rs
src/pane.rs
src/input.rs
src/render.rs
```

### 实现内容

- 二叉分割树。
- 水平分屏和垂直分屏。
- 根据外层尺寸重新计算所有 pane。
- 焦点切换。
- 关闭当前 pane。
- 最后一个 pane 关闭时退出 session 或保留 session，行为固定且写入文档。
- zoom 当前 pane。
- pane 边框和焦点样式。
- 每个 pane 独立维护 terminal state、PTY 和 scrollback。
- `Ctrl-A c` 创建默认垂直 pane，`Ctrl-A +/-` 调整当前分割比例。

### 推荐默认行为

- 关闭最后一个 pane 时关闭 session。
- 新 pane 继承当前 pane 的工作目录。
- 新 pane 使用当前 session 的 shell 和环境。
- 分屏比例默认为 50%。
- pane 最小尺寸为 10 列、3 行。

### 完成标准

- 两个 pane 可以同时运行独立 shell。
- resize 后所有 pane 尺寸正确。
- 一个 pane 退出不影响其他 pane。
- 关闭 pane 不会破坏 layout tree。
- zoom、恢复和焦点切换状态一致。

### 验证命令

```bash
printf 'pane-a\\n'
printf 'pane-b\\n'
```

然后分别在两个 pane 中执行：

```bash
echo "$PLUX_PANE"
stty size
```

## 11. 任务 7：配置、session 元数据和异常处理

状态：`已完成`

### 目标

把运行时行为整理成可配置、可诊断的产品状态。

### 预计文件

```text
src/config.rs
src/session.rs
src/daemon.rs
src/error.rs
src/logging.rs
```

### 实现内容

- 读取 `$XDG_CONFIG_HOME/plux/config.toml`。
- 默认值在缺少配置时生效。
- session 元数据写入 `$XDG_RUNTIME_DIR/plux/sessions/`。
- session 和 socket 目录设置为用户私有权限。
- session metadata 文件明确设置为 `0600`。
- shell 启动失败时保留错误信息。
- PTY EOF 后显示退出状态。
- daemon 收到 SIGTERM 时关闭子进程并清理 socket。
- socket 已存在时区分“daemon 正常运行”和“残留 socket”。
- 日志默认写 stderr，必要时支持 debug 文件。

### 明确限制

daemon 崩溃后不保证 shell 进程恢复。第一版只保证 client detach/attach。

### 完成标准

- 配置错误不会导致 panic。
- session 文件损坏时可以重新创建 session。
- daemon 正常退出后 socket 不残留。
- shell 异常退出后用户可以看到退出原因。
- 运行目录权限不会允许其他用户访问。

## 12. 任务 8：性能、兼容性和模糊测试

状态：`已完成`

### 目标

验证 Plux 在真实终端工作负载下不会卡死、乱码或破坏外层终端。

### 测试对象

- Bash
- Zsh
- Fish
- Vim/Neovim
- Less
- Top/HTop
- SSH
- Git pager
- 编译输出
- 进度条程序
- 中文日志

### 压力测试

```bash
yes >/dev/null
seq 1 1000000
find / -type f 2>/dev/null
```

### Golden 测试

输入固定 ANSI 字节流，比较最终 terminal grid：

```text
ANSI bytes -> parser -> terminal state -> snapshot
```

覆盖：

- 光标移动
- 清屏
- 颜色
- 滚动区域
- alternate screen
- 长行换行
- 宽字符
- 非法 UTF-8
- 截断 escape sequence

### 模糊测试

终端解析器对任意字节流都必须满足：

- 不 panic
- 不死锁
- 不越界
- 不产生无法恢复的终端状态

### 完成标准

- `cargo test` 通过。
- `cargo fmt --check` 通过。
- `cargo clippy -- -D warnings` 通过，或已记录合理例外。
- 大输出时输入仍然响应。
- 终端尺寸变化不会导致崩溃。
- client 强制退出后外层终端状态可恢复。

当前版本已完成 parser 异常输入、0 尺寸 PTY、百万行 PTY 输出和交互 detach
验证，并提供独立 `fuzz/` target；Vim/Less/SSH 的长期兼容性仍受本机未安装
这些程序限制，后续在兼容性环境中继续运行。

## 13. 任务 9：最终验收和使用文档

状态：`已完成`

### 目标

完成日常使用前的最后整理。

### 预计文件

```text
README.md
CHANGELOG.md
LICENSE
docs/compatibility.md
docs/keybindings.md
```

### README 必须包含

- 安装方法
- 依赖
- 启动 daemon 的方式
- 创建和连接 session
- 默认快捷键
- 配置文件位置
- 已知限制
- 如何收集 debug 日志
- 如何报告终端兼容性问题

### 最终验收

- 能创建、连接、断开和关闭 session。
- 多个 pane 可以同时运行 shell。
- client 断开后进程继续运行。
- 100 万行日志可以滚动和搜索。
- 中文复制结果正确。
- Vim、Less、Top、SSH 基本可用。
- `yes` 输出时键盘仍然响应。
- resize 后应用能收到正确尺寸。
- client 异常退出不会留下 raw mode。
- daemon 和 client 不发生死锁。
- 异常 ANSI 输入不会让程序崩溃。

## 14. 每次任务的执行流程

每个任务必须按以下顺序执行：

1. 将任务状态改为 `进行中`。
2. 阅读该任务涉及的现有代码和调用关系。
3. 只实现该任务需要的最小代码。
4. 添加该任务对应的最小测试或可运行检查。
5. 运行格式化、编译和专项验证。
6. 修复验证过程中发现的问题。
7. 将任务状态改为 `已完成`。
8. 在“完成记录”中写入日期、验证命令和结果。

## 15. 完成记录

| 日期 | 任务 | 验证 | 结果 |
|---|---|---|---|
| 2026-07-22 | 仓库和工具链检查 | cargo、rustc、依赖搜索 | 已完成 |
| 2026-07-22 | Rust 工程和 CLI 骨架 | `cargo fmt --check`; `cargo check`; `cargo test`; `plux --help`; `plux list` | 已完成 |
| 2026-07-22 | PTY 与终端状态核心 | `cargo test`（6 tests passed） | 已完成 |
| 2026-07-22 | daemon/client 协议 | daemon 自动启动、new/list/attach/kill 进程级 smoke test | 已完成 |
| 2026-07-22 | 单 pane 交互闭环 | PTY smoke test；shell 输入；`Ctrl-A d` detach；终端恢复序列 | 已完成 |
| 2026-07-22 | scrollback、滚动、搜索和复制 | PTY smoke test；滚动、搜索、复制请求和退出滚动模式 | 已完成 |
| 2026-07-22 | 分屏和 pane 生命周期 | PTY smoke test；split、独立输入、焦点、zoom、close、detach | 已完成 |
| 2026-07-22 | 配置、session 元数据和异常处理 | 非法名称、daemon 连续响应、metadata 创建/删除、socket 权限 | 已完成 |
| 2026-07-23 | 回归：退出 pane 后重新 attach | `repro-work` shell 退出后再次 attach，成功启动新 shell 并执行 `printf restarted`；`cargo test`（35 passed） | 已完成 |
| 2026-07-22 | 性能、兼容性和模糊测试 | `cargo test`（35 passed）；`cargo check --manifest-path fuzz/Cargo.toml`；`cargo clippy -- -D warnings`；百万行 PTY 输出、golden ANSI、非法 ANSI、0 尺寸和 kill waiter 测试 | 已完成（MVP 范围） |
| 2026-07-22 | 最终验收和使用文档 | attach、scroll/search/copy、split/zoom/close/detach、`plux run --`、SIGTERM 清理、README/DESIGN/PLAN/docs 同步 | 已完成（MVP 范围） |
| 2026-07-22 | 增强：scrollback 与输入协议 | 字节预算、未读计数、搜索方向、`n/N`、坐标选择复制；`cargo test` | 已完成 |
| 2026-07-22 | 增强：鼠标与信号生命周期 | SGR 鼠标单元测试、应用捕获路由、daemon/client SIGTERM、独立 ChildKiller；PTY smoke test | 已完成 |
| 2026-07-22 | 增强：临时命令 session | `script` 伪终端执行 `plux run -- ...`，0 尺寸终端不 panic；连接 id 竞态回归 | 已完成 |
| 2026-07-22 | 增强：安全边界 | session metadata 明确设置 `0600`；socket/运行目录保持私有 | 已完成 |
| 2026-07-22 | 增强：交互和布局 | 可配置 prefix、创建 pane、rename、调整比例、最小分屏尺寸、layout/时间元数据 | 已完成 |
| 2026-07-22 | 增强：刷新策略 | 使用 `refresh_rate` 合并 PTY 输出快照，保留完整帧正确性 | 已完成 |
| 2026-07-22 | 交付文档 | `CHANGELOG.md`、`LICENSE`、`docs/keybindings.md`、`docs/compatibility.md` | 已完成 |
| 2026-07-23 | 回归：连续多页滚动 | `TerminalState` 连续执行多个 12 行滚动后偏移持续增加并可到达历史顶部；`cargo test`（38 passed） | 已完成 |
| 2026-07-23 | 兼容：X10 鼠标滚轮 | 增加 `ESC[M` X10 滚轮上/下解析；SGR/X10 端到端 PTY 验证；`cargo test`（39 passed） | 已完成 |
| 2026-07-23 | 回归：SSH 中断后的 session 接管 | `attach --force` 进程级接管验证；旧 client 消息按 id 丢弃；`cargo test`（39 passed） | 已完成 |
| 2026-07-23 | 回归：alternate screen 应用分页 | Snapshot 增加 alternate screen 状态；Codex/Vim/Less 中 PageUp/PageDown 转发给应用；`cargo test --all-targets`（57 passed）；Clippy、release build | 已完成 |
| 2026-07-23 | 终端稳定性专项审计 | 非法 takeover、最终输出顺序、分屏 pane 退出和 alternate-screen 滚轮路由均已修复；62 项全量测试、Clippy、release build | 已完成 |
| 2026-07-24 | attach 语义对齐 | 普通 attach/list/kill 不启动 daemon；`attach --create` 显式创建；移除会泄漏 daemon 的冷启动测试；64 项全量测试、Clippy、release build | 已完成 |
| 2026-07-24 | 基础生命周期对齐 | socket 仅在缺失/拒绝连接时启动；错误 force 保留旧 client；新增 `plux stop`；裸命令创建 default；67 项全量测试、Clippy、release build | 已完成 |

## 16. 审计发现（2026-07-23）

### P1：非法 takeover 会退出 daemon

状态：`已完成`。接管前先校验名称；接管请求的 attach 错误仅返回给新连接，不再从 daemon 主循环冒泡。

已有 client 时，`accept_client` 在接收 `Takeover` 后先断开旧 client，再直接使用 `?` 调用 attach 流程。非法 session 名称会把错误返回到 daemon 主循环，daemon 退出，原会话也被断开。隔离测试已复现，退出码为 1。

涉及：`src/daemon.rs` 的接管入口和 `attach_session`。

### P1：瞬时命令的最终输出可能丢失

状态：`已完成`。PTY reader 在 EOF 后等待 child 状态再报告退出，daemon 先 flush 最终 snapshot，再发送 `ProcessExited`。

PTY reader 和 child waiter 是两个线程。短命令输出后立即退出时，`ProcessExited` 可能先发送；事件循环会先处理退出事件，再 flush pending snapshot，client 收到 `ProcessExited` 后立即退出，因此看不到最后一帧。隔离测试中 `FINAL_MARKER` 在 `ProcessExited` 前不可见。

### P1：分屏中单个 pane 退出会关闭整个 client

状态：`已完成`。`ProcessExited` 带 `session_finished`，client 仅在所有 pane 都结束时退出。

daemon 对任意 pane 的退出都发送 `ProcessExited`，client 收到后直接退出 attach。分屏场景下其他 pane 仍然存活，但用户会被迫离开整个 session。

### P2：alternate screen 没有 Plux 历史

状态：`已完成（边界明确）`。未被应用捕获的滚轮被转换为 PageUp/PageDown 并转发给应用；`Ctrl-A [` 仍用于选择、复制当前屏幕。alternate screen 的应用自有内容不写入 Plux primary scrollback。

Codex、Vim、Less 等程序的 alternate screen 不进入普通 scrollback；PageUp/PageDown 已转发给应用，但显式 `Ctrl-A [` 和未被应用捕获的鼠标滚轮仍进入 Plux scrollback 分支，而 alternate screen 通常没有可滚动历史。这是交互边界不一致，不是普通 shell scrollback 的故障。

### 已执行验证

- `cargo test --all-targets`：62 passed。
- `cargo clippy --all-targets --all-features -- -D warnings`：通过。
- terminal fuzz：1000 runs，无崩溃。
- 回归测试：非法 takeover 保持旧连接和 daemon 存活；最终 snapshot 先于退出事件；分屏中单 pane 退出不结束 attach；SGR/X10 滚轮在 alternate screen 映射为分页键。

## 17. 当前下一步

MVP 和本轮审计修复已完成。稳定性计划中的 SSH 实机中断重连验收仍受本机认证环境限制，未将其标记为已验证。

详细任务、状态、验收命令和完成记录见
[稳定性加固计划](./plans/reliability-hardening.md)。远程 session 已作为任务 17 进入
下一阶段；Windows、插件和图形协议仍不在当前范围内。

## 18. 任务 17：本地 Client + SSH Bridge

状态：`进行中`

目标是在不开放 Plux TCP 端口的前提下，由本地 client 通过 SSH bridge 连接服务器
daemon；网络中断后自动重建 SSH transport，并使用 client token、连接代际和心跳租约
安全恢复原 session。

详细架构、协议修改、状态机、竞态处理、测试矩阵、验收标准和逐项状态见
[远程 Client + SSH Bridge 计划](./plans/remote-client-over-ssh.md)。该计划的设计任务已
完成，代码任务尚未开始。
