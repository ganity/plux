# Changelog

## Unreleased

- Changed the default prefix from `Ctrl-Space` to `Ctrl-A` to avoid common IME
  shortcut conflicts; `Ctrl-]` and explicit `prefix = "Ctrl-Space"` remain
  supported.
- PageUp/PageDown and mouse wheel now enter scroll mode directly.
- Mouse scrolling now accepts both SGR and legacy X10 mouse reports.
- Added mouse drag selection with copy-on-release and OSC 52 clipboard fallback.
- Fixed SSH disconnect handling, forced takeover cleanup and concurrent daemon startup.
- Added stream-safe escape parsing, protocol version 2 diagnostics and row-level snapshots.
- Added local client + SSH bridge attach with client-token ownership, Heartbeat
  lease checks, automatic reconnect and full-snapshot recovery.

## 0.1.0 - 2026-07-22

- Added daemon-managed Unix sessions and PTY-backed panes.
- Added attach, detach, list, kill and temporary `run --` sessions.
- Added horizontal/vertical split, focus navigation, zoom, close and ratio adjustment.
- Added bounded scrollback, unread output counts, history search, repeat search and copy mode.
- Added SGR mouse capture routing, configurable prefix keys and session rename.
- Added signal cleanup, private runtime files, metadata persistence and a
  `cargo-fuzz` target for arbitrary terminal bytes.
