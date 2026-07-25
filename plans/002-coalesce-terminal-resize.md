# Plan 002: Coalesce Terminal Resize Before Touching PTYs

> **Executor instructions**: Complete and mark each task in order. Add the
> failing regressions first, then implement the smallest state change that
> applies only the final resize. Update Plan 002 in `plans/README.md` when done.
>
> **Drift check (run first)**:
> `git diff --stat 83689a7..HEAD -- src/client.rs src/daemon.rs src/session.rs tests/daemon_lifecycle.rs tests/client_ui.rs`
> Stop if resize handling has already moved to another owner or protocol shape.

## Status

- **Priority**: P0
- **Effort**: M
- **Risk**: MED
- **Depends on**: `plans/001-real-client-pty-harness.md`
- **Category**: bug, perf
- **Planned at**: commit `83689a7`, 2026-07-25

## Why This Matters

The client currently sends every observed intermediate terminal size and the
daemon immediately resizes every pane PTY and vt100 grid. Full-screen programs
then redraw for each drag event, flooding the output path that is already under
backpressure. Snapshot coalescing does not help because the child process has
already received every resize. The required invariant is: while the user is
dragging, retain only the newest dimensions and apply them once after a short
stable interval.

## Current State

`src/client.rs:153-161` polls terminal size in the main loop and sends every
change:

```rust
let (next_cols, next_rows) = terminal_size()?;
if (next_rows, next_cols) != (rows, cols) {
    rows = next_rows;
    cols = next_cols;
    // ...
    send(&writer, &ClientMessage::Resize { rows, cols })?;
}
```

`src/daemon.rs:659-667` applies every message immediately:

```rust
ClientMessage::Resize { rows, cols } => {
    if let Some(session) = self.sessions.get_mut(&name) {
        session.resize(rows, cols)?;
        self.pending_snapshots.insert(name);
    }
}
```

`tests/daemon_lifecycle.rs:162` only limits the number of outbound Snapshot
messages. It does not observe how often the child PTY receives SIGWINCH.

Design constraints from `DESIGN.md:406-437`:

- resize must update layout, terminal grid and TIOCSWINSZ;
- PTY bytes cannot be dropped;
- screen refreshes may be merged.

## Commands You Will Need

| Purpose | Command | Expected on success |
|---|---|---|
| Daemon resize tests | `cargo test --locked --test daemon_lifecycle resize -- --nocapture` | all resize tests pass |
| Real client test | `cargo test --locked --test client_ui resize -- --nocapture` | all resize tests pass |
| Full validation | `cargo test --locked --all-targets` | all tests pass |

## Scope

**In scope**:

- `src/client.rs`
- `src/daemon.rs`
- `tests/daemon_lifecycle.rs`
- `tests/client_ui.rs`
- `plans/002-coalesce-terminal-resize.md`
- `plans/README.md`

**Out of scope**:

- Snapshot writer architecture; Plan 003 owns it.
- vt100 reflow; Plan 007 owns it.
- New resize configuration options.
- Layout redesign or pane minimum-size policy.

## Git Workflow

- Suggested branch: `hardening/002-resize-coalescing`
- Suggested commit: `Coalesce terminal resize before updating PTYs`
- Do not push unless requested.

## Task Status

| Task | Description | Status |
|---:|---|---|
| 1 | Add child-visible resize burst regressions | DONE |
| 2 | Deliver resize events without polling every loop | DONE |
| 3 | Debounce daemon PTY resize and suppress stale snapshots | DONE |
| 4 | Validate large shrink/grow behavior in the real client | DONE |

## Steps

### Step 1: Add A Regression That Observes The Child PTY

Extend `tests/daemon_lifecycle.rs` with
`resize_burst_applies_only_the_final_pty_size`:

1. Create a session whose shell installs a WINCH trap and prints
   `WINCH:<rows>x<cols>` using `stty size`.
2. Attach and send at least 100 alternating Resize messages followed by a
   final unique size such as `31x97`.
3. Read snapshots until the final marker appears.
4. Assert the final size is observed and the child reports no more than three
   WINCH events for the burst. Permit startup noise but not one event per
   message.

The test should fail on commit `83689a7` for the correct reason.

**Verify before implementation**:
`cargo test --locked --test daemon_lifecycle resize_burst_applies_only_the_final_pty_size -- --nocapture`
-> fails because too many child resize events are observed.

### Step 2: Make Client Resize Event-Driven

Use the existing `signal-hook` dependency:

1. Add SIGWINCH to the client signal reader.
2. Change the signal reader to loop: termination signals emit
   `ClientEvent::InputClosed`; SIGWINCH emits a dedicated resize event and
   continues listening.
3. On the main client thread, read `terminal_size()` only when processing the
   resize event, compare with the last physical dimensions and send Resize only
   when they changed.
4. Remove the unconditional size poll from every main-loop iteration.
5. Preserve the existing minimum clamp in `terminal_size()`.

Do not add a generic event framework. One `ClientEvent` variant is enough.

**Verify**: client unit tests for repeated SIGWINCH/coalesced equal sizes pass,
then `cargo test --locked --lib client::tests` exits 0.

### Step 3: Debounce The Actual Daemon Resize

Add one explicit pending-resize state owned by `Daemon`. Only one interactive
client exists, so do not introduce a per-client map.

Required behavior:

1. A `ClientMessage::Resize` stores/replaces `{session_name, rows, cols,
   received_at}` and does not call `Session::resize` immediately.
2. A constant `RESIZE_DEBOUNCE` of 100 ms defines the stable interval. Keep it
   internal; do not add configuration.
3. Each event-loop turn checks whether the newest pending resize is old enough.
   If so, call `Session::resize` exactly once, force a full render and queue one
   snapshot.
4. While an attached session has a pending resize, do not emit an old-size
   output snapshot for that session. Continue processing and storing PTY bytes.
5. Attach and initial session creation still apply their dimensions
   immediately. Detach, takeover, kill and session exit clear an irrelevant
   pending resize.
6. Heartbeats, input and short commands remain responsive during the debounce.

**Verify**:

```bash
cargo test --locked --test daemon_lifecycle resize_burst_applies_only_the_final_pty_size -- --nocapture
cargo test --locked --test daemon_lifecycle resize_burst_is_coalesced_without_detaching_client -- --nocapture
```

Both pass; the first observes the final child size with at most three WINCH
events.

### Step 4: Exercise Large Real-Client Shrink And Growth

Add `real_client_coalesces_large_resize_drag` to `tests/client_ui.rs`:

1. Produce a screen containing many colored rows, similar to the existing
   `large_resize_snapshot_survives_temporary_backpressure` fixture.
2. Resize the outer PTY rapidly through at least 20 dimensions, including a
   large shrink and growth.
3. Finish at a known size, run `stty size`, and assert the final dimensions are
   rendered.
4. Assert the local client and daemon remain alive, input still works and the
   session can detach normally.

The test must not depend on Codex being installed.

**Verify**: `cargo test --locked --test client_ui resize -- --nocapture` -> pass.

## Test Plan

- Extend the existing daemon resize tests rather than replacing them.
- Add one real-client resize test in the Plan 001 harness.
- Cover:
  - burst replacement;
  - final PTY dimensions;
  - heartbeats/input during pending resize;
  - detach while resize is pending;
  - large shrink and growth.

## Done Criteria

- [x] The client no longer polls terminal size every loop.
- [x] Resize messages replace one pending daemon state.
- [x] Only the final stable size is applied to pane PTYs.
- [x] Old-size snapshots are held until the resize is applied.
- [x] Final dimensions reach the child and client.
- [x] Targeted daemon and real-client resize tests pass.
- [x] All shared verification gates pass.
- [x] Task statuses and `plans/README.md` are updated.

## STOP Conditions

Stop and report if:

- SIGWINCH is not delivered reliably to the PTY client on a supported CI OS.
- Correctness appears to require changing snapshot ordering before Plan 003.
- The child-visible resize count remains unbounded after daemon debouncing.
- The implementation requires a new event-loop or async dependency.

## Maintenance Notes

The debounce constant controls when PTYs see the final size, not frame rate.
Reviewers should confirm no code path still calls `Session::resize` for every
interactive Resize message. The direct daemon regression uses an interactive
shell's `stty size` because a `portable-pty` pane is not guaranteed to have a
foreground process group for SIGWINCH delivery; the real client regression
explicitly signals the client after resizing the outer PTY. Future multi-client
work would require pending resize state per attached view, but that is
explicitly outside this plan.
