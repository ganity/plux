# Compatibility

## Supported Target

The current implementation targets Unix systems with a native PTY and a
UTF-8 outer terminal. ANSI/VT parsing is delegated to `vt100`, and PTY
creation is delegated to `portable-pty`.

## Verified Locally

- Interactive shell input and output through attach/detach.
- ANSI color, clear-screen sequences, Chinese text and malformed escape bytes.
- 1000-line PTY output with scrollback, search, copy request and unread output.
- Vertical/horizontal split, focus, zoom, ratio adjustment and pane close.
- SGR mouse wheel routing and application mouse capture forwarding.
- Snapshot rendering coalesces pending refreshes, uses row-level updates after the
  initial frame, and disconnects slow clients after a bounded write timeout.
- SIGTERM cleanup, 0-size pseudo-terminal clamping and private metadata.
- Protocol bridge forwarding, client-token reconnect takeover, Heartbeat Ack and
  stale-connection event filtering through the daemon lifecycle tests.

## Not Installed in the Verification Environment

The local environment did not provide `fish`, `vim`, `nvim`, `less`, `top`,
`htop` or an SSH server. The SSH adapter and reconnect path are covered by
build and protocol/daemon tests, but a real SSH server interruption/reconnect
smoke test remains pending.

## Known Limits

- The local daemon protocol is versioned. After upgrading Plux, an older daemon may need to be
  stopped before retrying; the client reports this as a restart request.
- The first release does not restore shell processes after daemon crash or reboot.
- GBK/Big5 detection, terminal graphics protocols, Windows, direct TCP listeners
  and plugins are outside the current scope. Remote attach is supported through
  the SSH bridge described in the README.
- History search still scans the vt100 scrollback synchronously; very large histories
  can briefly occupy the daemon while a search is running.
