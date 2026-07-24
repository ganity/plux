# Plux 本地 Client + SSH Bridge 详细设计与执行计划

状态：`进行中`

设计日期：2026-07-24

本文档定义 Plux 远程连接的第一阶段方案：PTY、session 和终端状态继续由服务器
daemon 持有，本地 Plux client 通过 SSH 字节流连接服务器 bridge，并在网络中断后
自动重连。该方案不开放 Plux TCP 端口，不自行实现 TLS、用户认证或远程 shell。

## 1. 决策摘要

采用以下结构：

```text
本地终端
   │
   ▼
本地 plux client
   │  长度前缀 Plux 协议
   ▼
本地 ssh 子进程
   │  SSH 加密连接
   ▼
服务器 plux __bridge
   │  用户私有 Unix socket
   ▼
服务器 plux daemon
   │
   ├── session
   ├── PTY / shell / command
   └── terminal state / scrollback
```

核心原则：

1. SSH 是可替换的传输，不是 session 生命周期的 owner。
2. 网络断开只销毁传输连接，不销毁 daemon、session、PTY 或 scrollback。
3. 本地 client 保持终端界面并自动重新建立 SSH bridge。
4. 相同 client 实例可自动替换自己的旧连接，不得误踢其他 client。
5. 旧连接的迟到消息必须被连接代际隔离。
6. 心跳用于确认端到端可用性；事件队列是否饱和不能作为存活依据。
7. 断线期间不缓存和重放用户输入，避免命令重复执行。

## 2. 目标

第一阶段必须做到：

- 本地执行 `plux attach --ssh <target> <session>`，连接服务器已有 session。
- 本地 client 负责 raw mode、键盘、鼠标、终端渲染和本地剪贴板。
- 服务器 daemon 继续负责 session、PTY、终端状态和 scrollback。
- SSH 短暂断开后，本地 client 不退出 alternate screen，而是自动重连。
- 重连后恢复当前终端尺寸并获得完整终端快照。
- 同一个本地 client 自动重连时，可以接管自己的旧连接。
- 其他 client 在有效租约内不能无提示抢占当前连接。
- `--force` 保留为用户明确授权的跨 client 接管操作。
- SSH 子进程退出后必须回收，不能产生后台进程或 zombie。
- 所有本地 Unix socket 使用路径保持现有行为。

## 3. 非目标

第一阶段不实现：

- 裸 TCP listener。
- TLS、mTLS、token 登录或 Plux 自有用户系统。
- QUIC、UDP、Mosh 协议或连接迁移。
- 同一个 session 同时附加多个交互 client。
- daemon 崩溃或服务器重启后的 shell 恢复。
- 断线期间输入的可靠重放。
- SSH 密码保存、SSH agent 实现或 host key 管理。
- Windows client 或 Windows server。
- 自动安装远端 Plux binary。

## 4. 前置条件和用户假设

- 本地和服务器均安装兼容版本的 `plux`。
- `ssh <target>` 已能正常登录，host key 已确认。
- 自动重连需要 SSH key 或 agent 等非交互认证；不能在每次重连时等待密码输入。
- 远程 daemon 和 bridge 使用同一个 Unix 用户运行，因此仍受现有运行目录和 socket
  `0600` 权限保护。
- SSH target 使用用户现有的 `~/.ssh/config`，端口、ProxyJump、IdentityFile 等配置
  不在 Plux 中重复实现。

## 5. 用户界面

第一阶段新增：

```bash
plux attach --ssh server work
plux attach --ssh user@server work
plux attach --ssh server --force work
plux attach --ssh server --create work
```

语义：

- 普通远程 `attach` 只连接已有 daemon 和 session。
- `--create` 允许服务器 bridge 在 daemon 不存在时启动 daemon，并按现有语义创建
  session。
- `--force` 明确接管不同 client 持有的 session。
- SSH target 原样传给系统 `ssh`，复杂连接参数通过 `~/.ssh/config` 配置。
- 第一阶段不增加 `--ssh-port`、`--identity-file` 或任意 SSH option 透传参数。

隐藏的服务器命令：

```bash
plux __bridge
plux __bridge --start
```

- `__bridge` 只连接现有 daemon，连接失败时退出。
- `__bridge --start` 使用现有安全启动逻辑，供远程 `attach --create` 使用。
- 该命令不进入 raw mode，不输出终端 UI，不解释 Plux 消息。
- 标准输入和标准输出只能承载协议字节；诊断仅写入标准错误。

## 6. 当前实现基础

现有代码已经具备以下可复用能力：

- `protocol::read_message` 和 `write_message` 对 `Read`/`Write` 泛型，不依赖
  `UnixStream`。
- 协议已有版本字段、8 MiB 帧上限和未知版本拒绝。
- daemon 已为每个连接分配递增 `client.id`。
- daemon 事件携带 `client_id`，已经会忽略旧连接的消息和断开事件。
- takeover 已按“先验证目标、再关闭旧连接”的顺序处理。
- client 和 daemon socket 写入已有 250 ms 超时。
- attach 时 daemon 能发送完整终端快照。

当前缺口：

- client 连接类型写死为 `UnixStream`。
- daemon 不知道两个不同连接是否属于同一个本地 client 实例。
- 没有长连接心跳和租约。
- server EOF 会直接结束本地 attach，没有重连状态机。
- 现有 `Ping/Pong` 是短请求，`Ping` 后 daemon 会 detach，不能用作心跳。
- 没有 SSH bridge 和 SSH 子进程生命周期管理。

## 7. 模块与 Seam

远程模式需要两个真实 adapter：本地 Unix socket 和 SSH 标准输入输出。因此在 client
连接建立处引入一个小的传输 seam 是合理的；协议、输入和渲染逻辑不感知具体 adapter。

建议新增内部模块：

```text
src/transport.rs
```

建议内部 interface：

```rust
pub struct Connection {
    pub reader: Box<dyn Read + Send>,
    pub writer: Arc<Mutex<Box<dyn Write + Send>>>,
    control: ConnectionControl,
}

impl Connection {
    pub fn close(&mut self);
    pub fn try_wait(&mut self) -> Result<Option<ExitStatus>>;
}
```

adapter：

- `LocalConnection`：连接 Unix socket，并通过 `try_clone` 分离 reader/writer。
- `SshConnection`：reader 使用 `ChildStdout`，writer 使用 `ChildStdin`，control 持有
  `Child`，负责 kill/wait。

外部 seam 保持很小：调用方只需要 reader、writer、关闭连接和检查子进程状态。
SSH 参数、pipe、child 回收和错误格式化全部隐藏在 adapter 内。

不设计通用 transport factory、插件接口或 TCP adapter。等真正增加第三种 transport
时再扩展。

## 8. SSH Adapter

本地建议启动：

```text
ssh
  -T
  -o BatchMode=yes
  -o ServerAliveInterval=15
  -o ServerAliveCountMax=4
  -o ConnectTimeout=10
  <target>
  PATH="$HOME/.cargo/bin:$PATH" exec plux __bridge [--start]
```

规则：

- `-T` 禁止远端分配 PTY，避免协议字节被终端行规程修改。
- `BatchMode=yes` 让自动重连在认证不可用时快速失败，不反复弹密码提示。
- SSH keepalive 用于发现无响应连接，但不承担 Plux client 所有权判断。
- `stdin`、`stdout` 必须设置为 pipe；`stderr` 继承本地 stderr 或捕获后展示最后一条
  可读错误。
- 每次重连创建新的 SSH 子进程。
- 放弃连接时先关闭 stdin，再 kill 尚未退出的 child，最后 wait 回收。
- 不使用 shell 拼接远端命令参数，使用 `Command::arg` 构建本地 ssh 参数。

## 9. Bridge

服务器 bridge 是无状态字节转发器：

```text
stdin  ───────────────> UnixStream writer
stdout <─────────────── UnixStream reader
```

实现要求：

1. 根据是否有 `--start` 调用现有 `connect` 或 `connect_or_start`。
2. clone Unix socket，建立双向 copy。
3. 任一方向 EOF 后关闭对应写方向，让另一端尽快得到 EOF。
4. 主流程返回后允许进程结束，不保留后台线程或 daemon 外的长期状态。
5. stdout 禁止输出日志、提示或换行，否则会破坏协议帧。
6. 错误写入 stderr，并返回非零退出码。

bridge 不应：

- 解析 `Attach`、`Heartbeat` 或 session 名称。
- 保存 client token。
- 决定是否允许 takeover。
- 管理自动重连。

这些行为全部属于 daemon 或本地 client，避免 bridge 成为第二个 session owner。

## 10. Client 身份、连接代际和租约

### 10.1 Client token

本地每次启动交互 attach 时生成一个 128 位随机 token：

```text
client_token = 32 个十六进制字符
```

要求：

- 从 `/dev/urandom` 读取 16 字节，不新增随机数依赖。
- 同一次本地 attach 的所有重连复用同一个 token。
- 本地进程退出后 token 不持久化。
- token 只表示 client 实例身份，不是认证凭证。
- token 通过 SSH 加密流传输，不放在命令行参数中。

### 10.2 连接代际

daemon 现有递增 `client.id` 直接作为 `connection_id` 使用。每次接受新的交互连接都
分配新值：

```text
旧连接：token=abc, connection_id=41
新连接：token=abc, connection_id=42
```

所有 reader 事件继续携带 `connection_id`。daemon 只处理当前 connection：

```rust
if event.connection_id != current.connection_id {
    return Ok(());
}
```

因此旧连接 41 的迟到 Input、Heartbeat 或 Disconnect 都不能改变连接 42 的状态。
不需要把 connection generation 放进每条网络消息。

### 10.3 租约

daemon 的 active client 增加：

```rust
struct Client {
    id: u64,
    token: String,
    last_seen: Instant,
    writer: Arc<Mutex<UnixStream>>,
    session: Option<String>,
}
```

固定参数：

```rust
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);
const CLIENT_LEASE_TIMEOUT: Duration = Duration::from_secs(30);
```

收到当前 connection 的任何合法消息时更新 `last_seen`。空闲 client 每 5 秒发送一次
Heartbeat，因此 30 秒租约允许多次网络抖动或调度延迟。

daemon 主事件循环定期检查：

```rust
if current.last_seen.elapsed() >= CLIENT_LEASE_TIMEOUT {
    detach_client();
}
```

租约过期仅 detach client，不关闭 session 或 PTY。

## 11. 协议版本 3

本功能包含不兼容消息结构变更，协议版本从 `2` 提升到 `3`。

修改 attach 消息：

```rust
Attach {
    name: String,
    rows: u16,
    cols: u16,
    client_token: String,
}

Takeover {
    name: String,
    rows: u16,
    cols: u16,
    client_token: String,
}
```

新增长连接消息：

```rust
ClientMessage::Heartbeat
ServerMessage::HeartbeatAck
```

保留现有短请求：

```rust
ClientMessage::Ping
ServerMessage::Pong
```

`Ping/Pong` 继续用于短连接 daemon 探测，不改变其现有断开语义。Heartbeat 只用于已
attach 的长连接。

协议校验：

- `client_token` 必须是 32 位小写十六进制字符串。
- 非法 token 在改变当前 client 之前返回 Error。
- Heartbeat 只能由当前 attach connection 续租。
- 短请求不能创建或续期交互 client 租约。

## 12. Daemon 连接准入算法

收到交互 Attach 时：

```text
1. 校验 token、session 名称和目标 session。
2. 检查当前 client。
3. 没有当前 client：正常 attach，分配新 connection_id。
4. token 相同：关闭旧 transport，安装新 connection，自动恢复。
5. token 不同且租约过期：清理旧 client，允许新 attach。
6. token 不同且租约有效：拒绝，提示 another client is already attached。
7. Takeover：目标校验成功后，无视 token 和租约，明确接管。
```

重要顺序：

```text
校验 token
→ 校验 session
→ 判断接管权限
→ 关闭旧 transport
→ 安装新 connection
→ attach session
→ 发送 Attached
→ 发送完整 Snapshot
```

任何校验失败都不能影响已有 client。

短请求 `List/Create/Kill/Shutdown` 保持现有非独占行为，不参与 active client token 和
租约。

## 13. 本地 Client 状态机

```text
                 ┌─────────────┐
                 │ Connecting  │
                 └──────┬──────┘
                        │ SSH bridge ready
                        ▼
                 ┌─────────────┐
                 │ Attaching   │
                 └──────┬──────┘
                        │ Attached + Snapshot
                        ▼
                 ┌─────────────┐
          ┌─────>│  Connected  │
          │      └──────┬──────┘
          │             │ EOF / write failure /
          │             │ heartbeat timeout / child exit
          │             ▼
          │      ┌─────────────┐
          └──────│Reconnecting │
                 └──────┬──────┘
                        │ Esc / signal / fatal auth error
                        ▼
                 ┌─────────────┐
                 │   Stopped   │
                 └─────────────┘
```

### Connected

- 正常处理输入、Resize 和 server 消息。
- 每 5 秒发送 Heartbeat。
- 收到 HeartbeatAck 更新 `last_heartbeat_ack`。
- 30 秒没有 Ack，主动关闭 SSH child 并进入 Reconnecting。

### Reconnecting

- 保持 `TerminalGuard`，不退出 alternate screen。
- 保留最后一帧内容，在底行显示连接状态。
- 继续消费 stdin 事件，但只识别 `Esc` 取消；其他输入丢弃，不缓存。
- 重连间隔固定为 `500ms, 1s, 2s, 5s`，之后持续每 5 秒重试。
- 每次尝试创建全新 SSH child 和 connection generation。
- 使用原 `client_token` 发送 Attach，允许 daemon 自动替换旧 transport。
- attach 成功后发送当前 rows/cols，并等待完整 Snapshot 后恢复输入。

不增加随机抖动：单个交互 client 不会形成大规模同时重连，固定退避更容易测试。

### Fatal errors

以下错误不无限重试：

- 本地 `ssh` executable 不存在。
- 远端 `plux` executable 不存在。
- 协议版本不兼容。
- session 不存在且没有 `--create`。
- token 或请求参数无效。
- 用户按 Esc、Ctrl-C 或收到终止信号。

认证失败可以有限重试一次后退出，并保留 SSH stderr 中最后一条可读诊断。

## 14. 本地连接代际

自动重连会产生多个 server reader 线程。为防止旧 reader 的迟到事件关闭新连接，本地
client 也维护递增 generation：

```rust
enum ClientEvent {
    Server {
        generation: u64,
        message: ServerMessage,
    },
    ServerClosed {
        generation: u64,
        message: String,
    },
    Input(Vec<u8>),
    InputClosed,
}
```

主循环只处理当前 generation 的 server 事件。旧 reader 退出后的 `ServerClosed` 会被
忽略，不能把已经恢复的新连接再次切换到 Reconnecting。

## 15. 输入与输出一致性

### 输入

Plux 不承诺断线边界上的 exactly-once 输入。原因是 client 无法知道最后一次 socket
写入的数据是否已经到达 shell。

规则：

- 已成功写入 SSH stdin 的数据不在本地保存副本。
- 发现断线后立即冻结应用输入。
- Reconnecting 期间除取消键外全部丢弃。
- 不自动重放 Enter、粘贴内容或 prefix 命令。

这可能丢失断线瞬间的少量按键，但不会重复执行命令，属于更安全的默认行为。

### 输出

- daemon 在 client 断开期间继续读取 PTY 并更新 terminal state 和 scrollback。
- 重连后发送完整 Snapshot，而不是尝试重放中间渲染帧。
- 用户可通过普通 scrollback 查看断线期间输出，但仍受行数和字节上限约束。
- alternate screen 应用仍遵循现有边界，不额外写入 primary scrollback。

## 16. 错误和诊断

建议用户可见错误：

```text
connecting to server...
connection lost; reconnecting in 1s (Esc to cancel)
reconnected to server/work
remote plux daemon is not running; use --create or start it on the server
remote plux executable was not found
remote plux protocol version is incompatible; upgrade both binaries
SSH authentication failed; configure key or agent authentication
another client is attached; use --force to take over
```

要求：

- 连接状态写入 Plux 状态行，不覆盖 terminal view 的永久内容。
- 恢复后重绘被状态行覆盖的底行。
- SSH stderr 不能混入协议 stdout。
- 默认不打印 token。
- `PLUX_DEBUG=1` 时可以记录状态转换、generation、重连次数和 child 退出状态，但不能
  记录 Input 内容。

## 17. 安全模型

- 网络认证、加密、host identity 和用户登录完全交给 SSH。
- bridge 不监听 TCP 端口。
- bridge 只能连接当前 Unix 用户私有的 Plux socket。
- `client_token` 不是授权凭证；同一个服务器 Unix 用户本来就可以访问该 socket。
- `--force` 是本地用户明确操作，不在自动重连时对不同 token 使用。
- 远程命令和 target 使用 `Command::arg`，禁止通过本地 shell 拼接。
- 协议仍执行版本、帧长度、session 名称和消息字段校验。

## 18. 资源和背压

- 每次 SSH connection 最多一个 reader 线程。
- writer 仍通过互斥锁串行写入。
- server event queue 继续使用现有串行 daemon 事件循环。
- client event queue 保持 128 项有界容量。
- 队列饱和表示 consumer 落后，不表示 connection 已死亡。
- 存活判断只使用 EOF、I/O 错误、SSH child 状态和 HeartbeatAck 超时。
- 关闭连接时必须 kill/wait SSH child，避免累积后台进程。
- 重连成功后必须确认旧 child 已进入回收流程。

## 19. 竞态与处理规则

| 场景 | 处理 |
|---|---|
| 旧 SSH 黑洞，新 SSH 使用相同 token 连接 | 新 connection 自动替换旧 connection |
| 旧 connection 的 Input 迟到 | connection_id 不匹配，丢弃 |
| 旧 connection 的 Disconnect 迟到 | connection_id 不匹配，忽略 |
| 新 Attach 的 session 不存在 | 返回错误，保留旧 client |
| 不同 token 在租约有效期连接 | 拒绝，不自动 takeover |
| 不同 token 在租约过期后连接 | 清理旧 client，允许 attach |
| 用户在另一台机器执行 `--force` | 验证目标后关闭当前 client 并接管 |
| Heartbeat 写成功但 Ack 不返回 | client 30 秒后主动重建 SSH |
| daemon 写 Snapshot 超时 | detach transport，session 保持运行 |
| SSH child 已退出但 reader 尚未报告 EOF | child 状态触发 Reconnecting |
| 重连时用户输入命令 | 除 Esc 外丢弃，不重放 |
| 重连期间收到 SIGTERM | 关闭 child、恢复终端并退出 |

## 20. 测试策略

### 协议单元测试

- protocol v3 Attach/Takeover token round-trip。
- Heartbeat/HeartbeatAck round-trip。
- v2 frame 被明确拒绝。
- 非法 token 在分配 client 前被拒绝。

### Daemon 单元和 socket 测试

- 相同 token Attach 自动替换旧 connection。
- 不同 token 在有效租约内被拒绝。
- `Takeover` 可接管不同 token。
- 租约过期仅 detach，不删除 session。
- 任意合法 client 消息刷新 `last_seen`。
- Heartbeat 返回 Ack。
- 旧 connection Input 被忽略。
- 旧 connection Disconnect 不影响新 connection。
- 错误 session 的恢复请求不关闭旧 connection。

### Transport 测试

- Local adapter 完成现有 request/attach 回归。
- 模拟 reader/writer pipe 验证 SSH adapter 的 split I/O。
- close 后 child 被 wait，不遗留进程。
- child 非零退出状态被转换为可读错误。

### Bridge 测试

- 使用 `UnixStream::pair` 验证 stdin 到 socket、socket 到 stdout 的双向字节一致。
- 任一方向 EOF 后另一端最终退出。
- bridge stdout 没有任何额外诊断字节。
- daemon 不存在时 `__bridge` 非零退出。
- `__bridge --start` 可以按现有启动规则连接 daemon。

### Client 状态机测试

- ServerClosed 从 Connected 切换到 Reconnecting。
- 旧 generation 的 ServerClosed 被忽略。
- HeartbeatAck 超时触发 close 和重连。
- 重连期间普通输入不转发，Esc 取消。
- 退避序列为 500ms、1s、2s、5s、5s。
- attach 成功但 Snapshot 未到达前不恢复输入。
- Snapshot 到达后恢复 Connected。

### 端到端测试

- 本机 SSH localhost 或可控 fake ssh 脚本连接真实 daemon。
- 创建 session、远程 attach、输入命令并看到输出。
- kill SSH child 后本地 client 保持界面并自动重连。
- 中断期间 shell 持续输出，重连后状态和 scrollback 可见。
- 反复断开 20 次，后台 SSH/bridge 进程数量不增长。
- 两个不同 client 的自动重连不会相互误踢。
- `--force` 能明确接管并使旧本地 client 退出或进入错误状态。

## 21. 验收标准

功能验收：

- 本地 client 能通过 SSH attach 服务器 session。
- 本地复制功能使用本地 clipboard，而不是服务器 DISPLAY。
- 5 秒级网络中断后无需人工执行命令即可恢复。
- 服务端 session 和 shell 在连接中断期间持续运行。
- 重连后终端尺寸、当前画面和普通 scrollback 正确。
- 旧连接迟到事件不影响新连接。
- 不同 client 不会因自动重连而被无提示踢下线。
- 20 次断开/重连后没有额外 plux daemon、ssh、bridge 或 zombie 累积。

质量验收：

```bash
cargo fmt --check
cargo test --all-targets
cargo clippy --all-targets --all-features -- -D warnings
cargo check --manifest-path fuzz/Cargo.toml
cargo build --release
```

现有本地 attach、list、new、kill、stop、force takeover、scrollback、alternate screen、
selection 和 pane 测试必须继续通过。

## 22. 实施计划

状态定义：

| 状态 | 含义 |
|---|---|
| `待开始` | 尚未修改实现 |
| `进行中` | 已开始修改，但未通过该任务全部验证 |
| `阻塞` | 需要外部环境或用户决策 |
| `已完成` | 实现和测试完成，并记录验证结果 |

总体任务：

| 编号 | 任务 | 状态 | 验证重点 |
|---:|---|---|---|
| 0 | 固化远程 client 设计与执行计划 | 已完成 | 本文档完成并登记到总计划 |
| 1 | 协议 v3、client token 与 Heartbeat 消息 | 已完成 | 协议 round-trip、版本拒绝、token 校验 |
| 2 | daemon 连接所有权、代际接管与租约 | 已完成 | 同 token 恢复、异 token 拒绝、旧事件隔离 |
| 3 | 提取 client transport seam 并保持本地回归 | 已完成 | Local adapter 通过全部现有 attach 测试 |
| 4 | 实现无状态 `__bridge` | 已完成 | 双向 copy、EOF、错误输出和进程退出 |
| 5 | 实现 SSH adapter 和 `attach --ssh` CLI | 进行中 | 远程 attach、参数安全、child 回收 |
| 6 | 实现 client 自动重连状态机 | 进行中 | EOF、Ack 超时、退避、输入冻结、全量恢复 |
| 7 | 完成竞态、进程泄漏和端到端测试 | 待开始 | 20 次重连、双 client、无后台进程增长 |
| 8 | 更新用户文档、兼容性说明和发布记录 | 已完成 | README、compatibility、CHANGELOG 与实测一致 |
| 9 | 全量验收与 release build | 待开始 | fmt、tests、clippy、fuzz check、release |

### 任务 1：协议 v3

涉及：`src/protocol.rs` 和协议测试。

步骤：

1. 协议版本提升到 3。
2. Attach/Takeover 增加 `client_token`。
3. 新增 Heartbeat/HeartbeatAck。
4. 增加 token 解析和校验 helper。
5. 更新所有构造消息的调用点和测试。

完成标准：协议测试通过，旧版本错误明确，非法 token 不进入 daemon 状态修改流程。

### 任务 2：Daemon 所有权与租约

涉及：`src/daemon.rs`、daemon lifecycle 测试。

步骤：

1. `Client` 保存 token 和 last_seen。
2. 复用现有 client id 作为 connection generation。
3. 相同 token 自动替换旧 transport。
4. 不同 token 按租约和 force 规则处理。
5. Heartbeat Ack 和任意合法消息续租。
6. event loop 周期性清理过期 client。

完成标准：所有准入和迟到事件竞态均有回归测试。

### 任务 3：Transport seam

涉及：`src/client.rs`、新增 `src/transport.rs`。

步骤：

1. 把 UnixStream reader/writer 拆分隐藏到 Local adapter。
2. client attach 主循环仅依赖 Connection interface。
3. 为 server reader 事件加入本地 generation。
4. 保持所有本地命令和现有测试不变。

完成标准：尚未加入 SSH 时，本地功能行为和测试结果完全一致。

### 任务 4：Bridge

涉及：`src/main.rs`、`src/socket.rs`、新增 bridge 实现和测试。

步骤：

1. 增加隐藏命令解析。
2. 实现 connect/connect_or_start 两种模式。
3. 双向 copy，并正确传播 EOF。
4. 保证 stdout 纯协议、stderr 纯诊断。

完成标准：bridge 测试通过，退出后没有残留 bridge 进程。

### 任务 5：SSH adapter 和 CLI

涉及：`src/main.rs`、`src/client.rs`、`src/transport.rs`。

步骤：

1. 解析 `attach --ssh <target>`。
2. 用固定安全参数启动系统 ssh。
3. 建立 pipe reader/writer 和 child control。
4. 支持 `--create` 映射到远端 `__bridge --start`。
5. 初次 SSH/attach 错误返回清晰诊断。

完成标准：在稳定连接下，远程 attach 与本地 attach 交互一致。

### 任务 6：自动重连

涉及：`src/client.rs` 和状态机测试。

步骤：

1. 将 attach 主循环显式化为连接状态机。
2. 定时发送 Heartbeat 并检查 Ack。
3. EOF、写失败、child exit、Ack 超时统一进入 Reconnecting。
4. 重连时冻结输入并显示状态。
5. 使用相同 token、更新 generation、重新 attach。
6. 收到完整 Snapshot 后恢复 Connected。

完成标准：模拟断线无需退出 client 即可恢复，旧 reader 事件不能破坏新连接。

### 任务 7：端到端和泄漏测试

涉及：`tests/`。

步骤：

1. fake ssh 覆盖命令参数、pipe、退出码和重启。
2. 可用时增加 localhost SSH smoke test；环境不可用时明确标记。
3. 20 次断开重连检查进程数和 session 状态。
4. 两 client token、force 和租约过期组合测试。

完成标准：自动化场景全部通过，实机限制有明确记录。

### 任务 8：文档

涉及：`README.md`、`docs/compatibility.md`、`CHANGELOG.md`、本计划。

完成标准：命令、SSH 前置条件、自动重连行为、输入不重放限制和排障方式全部可查。

### 任务 9：最终验收

运行第 21 节全部命令，并将具体测试数量、日期和实机 SSH 结果写入下面的完成记录。

## 23. 完成记录

| 日期 | 任务 | 验证 | 结果 |
|---|---|---|---|
| 2026-07-24 | 0. 固化远程 client 设计与执行计划 | 设计、状态机、协议、竞态、测试和实施任务完成评审 | 已完成 |
| 2026-07-24 | 1. 协议 v3、client token 与 Heartbeat 消息 | `cargo test --all-targets`（73 passed）；严格 Clippy | 已完成 |
| 2026-07-24 | 2. daemon 连接所有权、代际接管与租约 | 同 token 恢复、旧连接 EOF、异 token 拒绝、Heartbeat Ack；`daemon_lifecycle` | 已完成 |
| 2026-07-24 | 3. Transport seam | Local connection、通用 server reader、全量本地回归；`cargo test --all-targets` | 已完成 |
| 2026-07-24 | 4. `__bridge` | 真实 bridge 子进程转发 List 帧，无额外 stdout；专项测试通过 | 已完成 |
| 2026-07-24 | 8. 用户文档、兼容性说明和发布记录 | README、`docs/compatibility.md`、`CHANGELOG.md` 已同步 SSH bridge 和未验证环境限制 | 已完成 |
| 2026-07-24 | 2. 未完成握手连接准入 | 红测复现 Heartbeat 占用 active slot；修复后 `daemon_lifecycle` 通过；全量 75 passed | 已完成 |

后续任务只有在实现完成且专项验证通过后，才能标记为 `已完成`。
