# Plux 终端复用器设计方案

## 1. 项目定位

Plux 是一个运行在现有终端程序中的终端复用器，目标是替代 tmux/Zellij，重点解决：

- 多个 shell、分屏和 session 管理
- client 断开后 session 继续运行
- 简单可靠的滚动历史
- 历史搜索和复制
- UTF-8、中文宽度、组合字符和 emoji
- 大量输出时保持输入响应
- Vim、Less、Top、SSH 等程序兼容

Plux 第一阶段不负责终端窗口、字体、GPU、输入法和完整的图形协议。这些属于终端模拟器，而不是终端复用器。

如果乱码来自外层终端字体，或者应用输出的是 GBK 等非 UTF-8 编码，Plux 无法完全修复。第一版明确以 UTF-8 为默认和主要支持目标，其他编码只通过显式配置扩展，不做不可靠的自动猜测。

### 当前实现边界（2026-07-22）

当前 MVP 已落地以下核心链路：

- Rust daemon/client、用户私有 Unix socket 和版本化 JSON 帧协议
- PTY shell、VT100 屏幕状态、primary/alternate screen 和行/字节滚动限制
- session 创建、attach、detach、list、kill，以及 daemon 信号清理
- 水平/垂直分屏、焦点切换、关闭 pane 和 zoom
- scroll mode、分页/方向键滚动、未读输出计数、搜索和 `n/N`
- 字符/整行/矩形坐标选择复制、系统剪贴板命令回退、应用鼠标模式识别和滚轮路由
- 可配置 prefix、创建 pane、session rename、分割比例调整和最小 pane 尺寸
- 按 `refresh_rate` 合并 PTY 输出快照，session metadata 保存 layout、时间和 pane 启动信息
- `plux run -- ...` 的临时 daemon-managed PTY session
- 本地 client 通过 SSH bridge 连接服务器 daemon，使用 client token、连接代际、心跳
  租约和自动重连恢复远程 session

以下内容仍属于后续增强，而不是当前 MVP 的完成条件：可视化选择高亮、pane
标题/工作目录状态栏、屏幕 diff、Windows/TCP listener/插件/图形协议，以及在安装了
Vim/Less/SSH 等程序的兼容性环境中的长期回归。

## 2. 设计目标

### 必须做到

- Linux 和 macOS 上可用
- 一个 daemon 管理多个 session
- 支持 attach、detach 和重新连接
- 支持水平、垂直分屏
- 每个 pane 独立运行 PTY 和 shell
- 每个 pane 独立保存 scrollback
- 支持鼠标滚轮、分页滚动、搜索和复制
- 支持 ANSI/VT 基础控制序列
- 支持 UTF-8 和常见中文场景
- 支持窗口 resize
- 外层终端退出时恢复 raw mode 和光标状态
- 大量输出不能阻塞其他 pane 的输入

### 第一阶段不做

- Windows 支持
- 远程 session
- 插件系统
- Lua 或脚本扩展
- Kitty graphics、iTerm 图片协议
- 自动识别任意字符编码
- 机器重启后的 shell 进程恢复
- 完整 tmux 命令兼容层

## 3. 用户体验

### Session

建议命令：

```bash
plux
plux new work
plux attach work
plux list
plux detach
plux kill work
plux run -- cargo test
```

默认行为：

- `plux` 连接默认 session
- 默认 session 不存在时自动创建
- client 关闭后，daemon 和 shell 继续运行
- 重新 attach 后恢复 pane、屏幕内容和进程状态
- 持久 session 的焦点 shell 退出后，再次 attach 自动启动新 shell；临时命令 session 退出后清理

### Pane

支持：

- 新建 pane
- 关闭 pane
- 切换焦点
- 调整分割比例
- 放大当前 pane
- 显示 pane 标题
- 显示当前工作目录和退出状态

默认前缀键为 `Ctrl-A`，可在配置中改为其他控制键：

```text
Ctrl-A c       新建 pane
Ctrl-A x       关闭 pane
Ctrl-A h/j/k/l 切换 pane
Ctrl-A v       垂直分屏
Ctrl-A s       水平分屏
Ctrl-A z       zoom 当前 pane
Ctrl-A d       detach
Ctrl-A r       重命名 session
Ctrl-A [       滚动/复制模式
```

快捷键需要可配置，但第一版只实现静态键位映射，不设计插件式命令系统。

### 滚动和复制

- 鼠标滚轮直接滚动当前 pane
- `PageUp/PageDown` 分页滚动
- 普通模式下鼠标滚轮和 `PageUp/PageDown` 直接进入滚动模式
- `Home/End` 跳到历史开头或最新位置
- `Ctrl-A [` 进入滚动/复制模式
- `Esc` 退出滚动模式
- 用户滚动历史时，新输出不能强制把 viewport 拉回底部
- 有新输出时显示未读行数
- 回到底部时清除未读计数
- 支持字符选择、行选择和矩形选择
- 复制时自动处理物理换行和逻辑换行

鼠标规则：

- 应用未开启鼠标模式时，滚轮由 Plux 处理
- 应用开启鼠标模式时，鼠标事件转发给应用
- 进入滚动模式后，Plux 暂时接管鼠标
- `Shift + 鼠标` 强制由 Plux 处理

## 4. 总体架构

```text
┌──────────────┐
│ plux client  │
│ 输入/渲染/复制 │
└──────┬───────┘
       │ Unix socket
┌──────▼───────┐
│   pluxd      │
│ session 管理 │
│ layout 管理  │
└──────┬───────┘
       │
┌──────▼────────────────────┐
│ 每个 pane                  │
│ PTY + shell                │
│ Terminal Parser            │
│ Screen + Scrollback        │
└───────────────────────────┘
```

### `pluxd`

- 创建和管理 session
- 启动 shell 和子进程
- 保存 pane 的终端状态
- 管理分屏布局
- 路由用户输入
- 处理 attach/detach
- 更新 PTY 尺寸
- 向 client 发送屏幕快照和增量更新

### `plux client`

- 设置外层终端 raw mode
- 读取键盘和鼠标
- 发送用户操作
- 接收屏幕状态更新
- 输出 ANSI 光标、颜色和字符
- 退出时恢复外层终端状态

终端状态必须由 daemon 持有，不能只存在于 client 中，否则 detach 后无法继续运行。

## 5. 推荐项目结构

```text
src/
├── main.rs
├── cli.rs
├── config.rs
├── daemon.rs
├── protocol.rs
├── session.rs
├── pane.rs
├── layout.rs
├── pty.rs
├── terminal.rs
├── input.rs
├── render.rs
├── scrollback.rs
├── copy_mode.rs
├── search.rs
└── platform/
    ├── unix.rs
    └── windows.rs
```

模块职责：

- `pty.rs`：创建 shell、读写 PTY、resize
- `terminal.rs`：控制序列和屏幕状态
- `scrollback.rs`：历史行、换行关系和内存限制
- `layout.rs`：pane 树和尺寸计算
- `input.rs`：快捷键、鼠标和应用输入
- `render.rs`：终端状态到 ANSI 输出
- `protocol.rs`：client 与 daemon 通信
- `session.rs`：生命周期、attach、detach
- `search.rs`：历史搜索
- `copy_mode.rs`：选择和复制

## 6. 核心数据模型

### Session

```text
Session
├── id
├── name
├── created_at
├── last_attached_at
├── layout_tree
├── panes
├── attached_clients
└── options
```

### Pane

```text
Pane
├── id
├── title
├── cwd
├── command
├── pty
├── process_id
├── terminal_state
├── scrollback
├── viewport
├── input_mode
├── last_output_time
└── exited
```

### LayoutNode

使用二叉分割树：

```text
LayoutNode =
    Leaf(PaneId)
    Split {
        direction: Horizontal | Vertical,
        ratio: 0.0..1.0,
        first: LayoutNode,
        second: LayoutNode
    }
```

该结构足够支持水平分屏、垂直分屏、调整比例、关闭 pane、zoom 和布局持久化。第一版不需要复杂网格布局系统。

## 7. 终端模拟策略

每个 pane 有独立的终端模拟状态：

```text
TerminalState
├── primary_screen
├── alternate_screen
├── cursor
├── current_attributes
├── scroll_region
├── modes
├── title
├── hyperlink_state
└── saved_cursor
```

必须支持：

- ANSI SGR 颜色
- 光标移动
- 清屏和清行
- 插入和删除字符
- 插入和删除行
- 滚动区域
- 自动换行
- 光标样式
- 备用屏幕
- Application Cursor Keys
- Bracketed Paste
- 鼠标报告
- OSC 标题
- 256 色和真彩色

第一版暂不支持 Kitty graphics、iTerm 图片协议等私有图形协议。

不要从零实现完整 VT parser。使用成熟终端解析库，并通过真实程序输出补充兼容测试。

## 8. Scrollback 设计

每个 pane 有独立历史环形缓冲区：

```text
Scrollback
├── logical lines
├── physical rows
├── wrap metadata
├── style references
├── line ids
└── byte/line limits
```

默认配置：

```toml
scrollback_lines = 20000
scrollback_bytes = "64MB"
```

历史必须保存逻辑行和物理显示行之间的关系，否则长命令换行后搜索和复制会错误。

主屏幕和备用屏幕必须分开：

- shell、日志输出进入 primary screen 和 scrollback
- Vim、Less 等程序通常使用 alternate screen
- alternate screen 默认不写入普通 scrollback
- 应用退出后恢复 primary screen

用户正在查看历史时：

- 新输出继续处理，但不改变当前 viewport
- 记录新输出行数
- 显示未读行数
- 用户回到底部后清除未读状态

## 9. Unicode 和编码

处理流程：

```text
PTY bytes
    ↓
VT/control-sequence parser
    ↓
UTF-8 decoder
    ↓
grapheme cluster + width calculation
    ↓
terminal cell grid
```

必须测试：

- 中文和日文
- 中英文混排
- combining mark
- emoji 和 variation selector
- zero-width joiner
- 非法 UTF-8
- 长行自动换行
- 光标位于宽字符中间
- 删除宽字符
- resize 后重新换行

规则：

- 默认 UTF-8
- 非法 UTF-8 使用替换字符
- East Asian Ambiguous Width 提供配置
- 不自动猜测 GBK、Big5 等编码
- 应用自身的终端模式优先于 Plux 快捷键

## 10. PTY 和进程管理

启动 shell 时设置：

```text
TERM=xterm-256color
COLORTERM=truecolor
PLUX=1
PLUX_SESSION=<session-name>
PLUX_PANE=<pane-id>
```

必须处理：

- shell 启动失败
- PTY EOF
- 子进程退出
- SIGWINCH
- SIGHUP
- SIGTERM
- session 关闭
- pane 关闭
- 外层窗口 resize

resize 流程：

```text
外层窗口变化
    ↓
client 发送新尺寸
    ↓
layout 重新计算 pane 尺寸
    ↓
更新 terminal grid
    ↓
调用 TIOCSWINSZ
    ↓
发送 SIGWINCH
```

PTY 不应由多个线程同时读写。每个 pane 使用明确的 owner，减少状态竞争。

## 11. 并发和性能

建议模型：

- 一个 daemon 主事件循环
- 每个 pane 一个 PTY reader
- 所有终端状态由 session owner 串行修改
- 输入事件优先于屏幕刷新
- PTY 分块读取，例如 32KB～64KB
- 屏幕刷新按 30～60 FPS 合并
- 不丢弃 PTY 字节，只合并渲染帧

关键原则：

```text
PTY 数据不能丢
渲染帧可以合并
```

这样可以避免 `yes`、编译日志或大文件输出拖死其他 pane。

## 12. Client/Daemon 协议

使用 Unix Domain Socket，消息采用长度前缀二进制帧：

```text
[u16 protocol_version][u32 payload_length][UTF-8 JSON payload]
```

Client 到 Server：

```text
Attach
Detach
Input
Resize
Split
ClosePane
FocusPane
Scroll
Search
Copy
Rename
SetLayout
```

Server 到 Client：

```text
Hello
FullSnapshot
ScreenDiff
PaneCreated
PaneClosed
FocusChanged
SearchResult
Bell
TitleChanged
ProcessExited
Error
```

协议必须包含 protocol version、client id、session id、pane id 和 snapshot generation。

首次 attach 发送完整快照，之后发送增量更新。版本不一致时直接拒绝连接并提示升级，不设计复杂兼容逻辑。

## 13. 渲染器

client 启动时：

```text
保存外层终端状态
进入 raw mode
进入 alternate screen
隐藏光标
```

退出时：

```text
恢复颜色
恢复光标
清除状态栏
退出 alternate screen
恢复 cooked mode
```

渲染器负责：

- 光标定位
- SGR 属性
- 颜色切换
- 宽字符占位
- pane 边框
- 状态栏
- dirty region 更新
- resize 后全屏重绘

不建议用普通 TUI 表格组件直接渲染 terminal pane，因为它们通常不理解终端内部光标、备用屏幕和宽字符状态。

## 14. Session 持久化

第一版只承诺：

> client 断开后，daemon 和 shell 继续运行，可以重新 attach。

第一版不承诺：

> daemon 崩溃或机器重启后，原有 shell 进程自动恢复。

可以保存：

```text
$XDG_RUNTIME_DIR/plux/sessions/<id>.json
```

保存 session 名称、pane id、layout、工作目录、启动命令和配置快照即可。不需要把完整屏幕历史频繁写磁盘。

## 15. 配置文件

```toml
default_shell = "/bin/zsh"
prefix = "Ctrl-A"
scrollback_lines = 20000
scrollback_bytes = "64MB"
mouse = true
copy_command = "wl-copy"
ambiguous_width = 1
refresh_rate = 60
pane_border = "rounded"
```

第一版配置只包括 shell、前缀键、scrollback、鼠标、复制命令、默认布局、颜色主题和快捷键。

不加入运行时脚本、Lua、插件和远程配置。

## 16. 测试策略

### 单元测试

- ANSI 控制序列
- 光标移动
- 清屏和清行
- 插入删除行
- 备用屏幕
- 自动换行
- 中文宽度
- 组合字符
- scrollback 环形缓冲
- layout 尺寸计算
- 快捷键解析

### Golden 测试

记录 PTY 输入字节流，然后比较最终 terminal grid：

```text
ANSI 字节流
    ↓
终端状态
    ↓
屏幕快照
```

样例包括 shell、`ls --color`、Vim、Less、Top、HTop、SSH、进度条、编译输出和中文日志。

### 压力测试

- 连续输出一百万行
- 多 pane 同时输出
- `yes`
- 不断 resize
- attach/detach 循环
- client 强制退出
- daemon 收到 SIGTERM
- shell 异常退出

### 模糊测试

重点测试任意 ANSI 字节流、截断的 escape sequence、非法 UTF-8 和超长 OSC 字段。

目标是任何输入都不能 panic、死锁或破坏外层终端状态。

## 17. 开发里程碑

### 阶段 0：技术验证，1～3 天

- 启动一个 shell
- 读写 PTY
- 接入终端解析库
- 输出到外层终端
- 支持 resize

验收：能够运行 shell、Vim、Less，退出后外层终端正常。

### 阶段 1：单 pane 闭环，1～2 周

- client/daemon
- detach/attach
- ANSI 渲染
- 基础快捷键
- scrollback
- UTF-8
- 基础测试

### 阶段 2：分屏，1～2 周

- 二叉 layout tree
- 新建和关闭 pane
- pane focus
- resize
- zoom
- pane 标题

### 阶段 3：滚动、搜索、复制，1～2 周

- 鼠标滚轮
- 复制模式
- 搜索
- 中文和长行处理
- 新输出提示
- alternate screen

### 阶段 4：稳定性，3～6 周

- Vim/Neovim
- Less
- Top/HTop
- SSH
- 鼠标模式
- Bracketed Paste
- 高吞吐优化
- 异常退出处理
- 安装包和文档

## 18. 验收标准

达到 MVP 的标准：

- 能创建、连接、断开和关闭 session
- 多个 pane 可以同时运行 shell
- client 断开后进程继续运行
- 100 万行日志可以滚动和搜索
- 中文复制结果正确
- Vim、Less、Top、SSH 基本可用
- `yes` 输出时键盘仍然响应
- resize 后应用能收到正确尺寸
- client 异常退出不会留下 raw mode
- daemon 和 client 不发生死锁
- ANSI parser 对异常输入不会崩溃

## 19. 主要风险和解决策略

| 风险 | 解决策略 |
|---|---|
| ANSI 兼容不完整 | 复用成熟终端引擎，增加 golden 测试 |
| 中文宽度错误 | 固定 Unicode 版本，加入宽度配置和测试 |
| 大输出卡死 | PTY 数据不可丢，渲染帧限频合并 |
| 鼠标行为混乱 | 区分应用鼠标模式和 Plux copy mode |
| daemon 崩溃丢 session | 第一版只保证 client detach，后续再做监督进程 |
| 外层终端兼容差 | 使用标准 ANSI，减少终端私有扩展 |
| Windows 拖慢开发 | Unix-first，Windows 单独适配 |
| 功能范围失控 | 暂不做插件、远程、图片协议 |

## 20. 最终技术决策

第一版固定以下决策：

1. Plux 是终端复用器，不是完整终端模拟器。
2. Linux/macOS 优先，Windows 后做。
3. Rust 实现。
4. 复用成熟 PTY 和终端解析库。
5. daemon 持有所有 PTY 和终端状态。
6. client 只负责输入和渲染。
7. 使用二叉布局树管理 pane。
8. 每个 pane 独立维护 scrollback。
9. 默认 UTF-8，不自动猜测编码。
10. 不丢弃 PTY 数据，只合并渲染帧。
11. 第一版不做插件、远程和图片协议。

原型预计 1 周，可用 MVP 预计 6～10 周，稳定日常工具预计 3～6 个月。
