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
./target/release/plux
./target/release/plux new work
./target/release/plux attach work
./target/release/plux attach --create work
./target/release/plux attach --force work
./target/release/plux attach --ssh user@server work
./target/release/plux attach --ssh user@server --create work
./target/release/plux list
./target/release/plux kill work
./target/release/plux stop
./target/release/plux run -- cargo test
```

Running `plux` without arguments enters or creates the `default` session.
`new`, `run`, and `attach --create` start a daemon when needed. Explicit
`attach`, `list`, and `kill` only connect to an existing daemon. Explicit
`attach` never creates a missing session: use `plux new <name>` or
`plux attach --create <name>`. `plux stop` cleanly ends the daemon and all of
its panes. Runtime files are stored below `$XDG_RUNTIME_DIR` when available,
otherwise below the system temporary directory.

For a persistent session, attaching after its focused shell exits starts a new
shell in that pane. Temporary sessions created by `plux run -- ...` are removed
after their command exits.

Remote attach runs the client locally and uses SSH only as an encrypted bridge:

```bash
plux attach --ssh user@server work
```

The server must have a compatible `plux` binary available on `PATH`. Use SSH key
or agent authentication because automatic reconnect uses non-interactive SSH.
The local client keeps the session alive across SSH interruptions, reconnects
with the same client identity, and receives a fresh terminal snapshot. Input
typed while reconnecting is discarded instead of replayed, so commands cannot
be executed twice accidentally. `--force` still explicitly takes over a
session owned by another client.

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
```

The current implementation primarily supports UTF-8 and standard ANSI/VT terminal behavior. It does not automatically detect GBK/Big5, provide Windows support, restore shells after a daemon crash, or support terminal graphics protocols.

`plux run -- ...` executes the command inside a temporary daemon-managed PTY
session and removes that session after the command exits. Session metadata is
stored under `$XDG_RUNTIME_DIR/plux-<user>/sessions/` with mode `0600`.

## Checks

```bash
cargo fmt --check
cargo clippy -- -D warnings
cargo test
```

The current test suite contains 39 tests plus PTY smoke checks for attach,
scrollback, search, copy requests, split, zoom, close, detach, `run`, and
daemon signal cleanup.

Set `PLUX_DEBUG=1` when starting a command that auto-starts the daemon to keep
daemon diagnostics on stderr. Without it, the background daemon is silent.

## Report Issues

Include the command that reproduces the issue, `$TERM`, terminal size, shell,
OS, and output from a run started with `PLUX_DEBUG=1`. For compatibility issues,
also state the application and version. See [docs/compatibility.md](docs/compatibility.md)
for the currently verified environment and known limits.

Architecture and the execution checklist are documented in [DESIGN.md](DESIGN.md) and [EXECUTION_PLAN.md](EXECUTION_PLAN.md).
Key details and compatibility notes are in [docs/keybindings.md](docs/keybindings.md) and [docs/compatibility.md](docs/compatibility.md).
