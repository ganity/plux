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
- Snapshot rendering preserves frame order, coalesces only pending state, uses
  row-level updates after the initial frame, and applies bounded backpressure to
  slow clients without blocking daemon control requests.
- Real client PTY attach/input/detach, SIGWINCH resize bursts, large resize
  drags, pane-safe split rendering, scrollback resize preservation and common
  terminal query replies are covered by automated tests.
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
- History search advances in bounded daemon steps; very large histories still
  require more steps and may take longer to complete.
- Remote clipboard image/file upload relies on the outer terminal delivering
  bracketed-paste markers. If the terminal or focused application does not
  enable bracketed paste, Plux cannot distinguish a pasted path from typed text.

## Terminal Query Replies

Plux responds to the common query set used by shells and full-screen programs:

- `CSI 5 n` status OK;
- `CSI 6 n` cursor position;
- primary and secondary device attributes;
- `CSI 18 t` character-cell window size.

Graphics protocols, OSC 52 clipboard requests from child applications, title
forwarding and other unsupported queries remain outside the compatibility claim.
