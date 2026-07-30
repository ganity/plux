# Plux

Plux is a small Unix-first terminal multiplexer implemented in Rust. It keeps shell processes and terminal state in a background daemon, while the attached client handles input and ANSI rendering.

## Build

```bash
cargo build --release
```

For a local installation:

```bash
cargo install --path .
```

The only build dependency is the Rust toolchain. Runtime requires a Unix-like
system with a native PTY and a UTF-8 terminal. `wl-copy`, `xclip`, `xsel`, or
macOS `pbcopy` is optional and is used for system clipboard integration.

## Use

```bash
plux
plux work
plux --ssh user@server work
plux list
plux kill work
plux stop
plux run -- cargo test
```

`plux [name]` enters the named session, creates it when missing, and takes over
from an older attached client. Without a name it uses `default`. `plux list`
and `plux kill` only manage existing sessions; `plux stop` cleanly ends the
daemon and all panes. Runtime files are stored below `$XDG_RUNTIME_DIR` when
available, otherwise below the system temporary directory.

For a persistent session, attaching after its focused shell exits starts a new
shell in that pane. Temporary sessions created by `plux run -- ...` are removed
after their command exits.

Remote attach runs the client locally and uses SSH only as an encrypted bridge:

```bash
plux --ssh user@server work
```

The server must have a compatible `plux` binary available on `PATH` or installed
at `~/.cargo/bin/plux`. Use SSH key or agent authentication because automatic
reconnect uses non-interactive SSH.
The local client keeps the session alive across SSH interruptions, reconnects
with the same client identity, and receives a fresh terminal snapshot. Input
typed while reconnecting is discarded instead of replayed, so commands cannot
be executed twice accidentally. Starting another client for the same session
automatically replaces the older client.

During remote attach, bracketed text pastes keep their original bytes. A local
clipboard image or file paste is uploaded over a multiplexed SSH channel and
the remote temporary path is inserted into the focused application. The bottom
status line shows reading/upload progress; `Esc` cancels an active upload, and
input typed while it runs is replayed in order after completion. The default
upload limit is 64 MiB and can be changed with `clipboard_upload_max_bytes`.
Remote copies use mode `0600`; files older than 24 hours are removed when the
next upload starts.

## Default Keys

The default prefix is `Ctrl-A`. Set `prefix = "Ctrl-]"` or
`prefix = "Ctrl-Space"` explicitly if you prefer another binding.

```text
Ctrl-A d  detach
Ctrl-A [  scroll mode
Ctrl-A /  search history
Ctrl-A c  create a vertical pane
Ctrl-A v  vertical split
Ctrl-A s  horizontal split
Ctrl-A h/j/k/l  focus left/down/up/right
Ctrl-A +/-  adjust the focused split ratio
Ctrl-A r  rename the attached session
Ctrl-A z  zoom current pane
Ctrl-A x  close current pane

`PageUp`, `PageDown`, and the mouse wheel scroll directly and enter scroll
mode. `Ctrl-A [` remains available for explicit scroll/copy mode entry.
```

Inside scroll mode:

```text
j/k        scroll down/up
g/G        history top/bottom
PageUp/Down scroll by page
/          search
n/N        repeat search forward/backward
v          start/end a coordinate selection
h/j/k/l    move the selection cursor
y          copy the selection, or the visible pane when no selection exists
q          leave scroll mode
```

When `mouse = true`, Plux captures SGR mouse reports. The wheel scrolls Plux
history when the focused application does not request mouse input; otherwise
the complete mouse report is forwarded to that application.

## Configuration

Configuration is read from `$XDG_CONFIG_HOME/plux/config.toml` or `$HOME/.config/plux/config.toml`.

```toml
default_shell = "/bin/zsh"
prefix = "Ctrl-A"
scrollback_lines = 20000
scrollback_bytes = "64MB"
mouse = true
refresh_rate = 60
copy_command = "wl-copy"
clipboard_upload_max_bytes = "64MB"
```

The current implementation primarily supports UTF-8 and standard ANSI/VT terminal behavior. It answers basic status, cursor-position, device-attributes, and character-cell-size queries. It does not automatically detect GBK/Big5, provide Windows support, restore shells after a daemon crash, or support terminal graphics protocols.

`plux run -- ...` executes the command inside a temporary daemon-managed PTY
session and removes that session after the command exits. Session metadata is
stored under `$XDG_RUNTIME_DIR/plux-<user>/sessions/` with mode `0600`.

## Checks

```bash
cargo fmt --check
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo test --locked --all-targets
cargo test --locked --release --all-targets
cargo check --locked --manifest-path fuzz/Cargo.toml
```

The checks cover daemon protocol and lifecycle, real client PTY attach/input/
resize, SSH bridge forwarding, ordered snapshots, bounded PTY flow, split
rendering, scrollback reflow, selection and common terminal query replies.

Set `PLUX_DEBUG=1` when starting a command that auto-starts the daemon to keep
daemon diagnostics on stderr. Without it, the background daemon is silent.

## Report Issues

Include the command that reproduces the issue, `$TERM`, terminal size, shell,
OS, and output from a run started with `PLUX_DEBUG=1`. For compatibility issues,
also state the application and version. See [docs/compatibility.md](docs/compatibility.md)
for the currently verified environment and known limits.

Architecture and the execution checklist are documented in [DESIGN.md](DESIGN.md) and [EXECUTION_PLAN.md](EXECUTION_PLAN.md).
Key details and compatibility notes are in [docs/keybindings.md](docs/keybindings.md) and [docs/compatibility.md](docs/compatibility.md).
