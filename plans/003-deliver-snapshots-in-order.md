# Plan 003: Deliver Incremental Snapshots In Order Without Blocking Daemon State

> **Executor instructions**: Write regression tests before changing delivery.
> Preserve Snapshot ordering until the protocol explicitly gains a full-state
> replacement frame. Mark each task after its verification succeeds and update
> Plan 003 in `plans/README.md` when complete.
>
> **Drift check (run first)**:
> `git diff --stat 83689a7..HEAD -- src/client.rs src/daemon.rs src/session.rs src/protocol.rs tests/daemon_lifecycle.rs tests/client_ui.rs`
> Stop if Snapshot has become a full-state frame or the daemon already owns a
> dedicated interactive-client writer.

## Status

- **Priority**: P0
- **Effort**: L
- **Risk**: HIGH
- **Depends on**: `plans/001-real-client-pty-harness.md`,
  `plans/002-coalesce-terminal-resize.md`
- **Category**: bug, perf, tech-debt
- **Planned at**: commit `83689a7`, 2026-07-25
- **Status**: DONE

## Why This Matters

`Session::render` emits row-level deltas relative to its own last rendered
state. The client currently removes earlier queued Snapshots and keeps only the
last one. A later delta does not contain rows changed only in a discarded
frame, so one backlog event can permanently corrupt the displayed screen.
Meanwhile the daemon writes each frame synchronously from its state-owner loop;
a slow terminal or SSH pipe blocks input, heartbeats, resize and all sessions.
The fix must establish one invariant: deltas are delivered in order, while a
slow client can block only its writer thread, never daemon state progression.

## Current State

`src/client.rs:639-670` explicitly discards earlier same-generation snapshots:

```rust
let mut latest = event;
loop {
    match server_events.try_recv() {
        Ok(next) if snapshot_generation(&next) == Some(generation) => latest = next,
        // ...
    }
}
Ok(latest)
```

The behavior is codified by
`src/client.rs:1688 latest_server_snapshot_replaces_stale_snapshots`.

`src/session.rs:353-444` compares each pane row with `rendered_rows`, emits only
changed rows, then advances the cache before transport delivery is known.

`src/daemon.rs:1018-1053` renders and writes synchronously while holding daemon
execution:

```rust
let data = session.render()?;
let result = self.send(ServerMessage::Snapshot { data, /* ... */ });

let result = match client.writer.lock() {
    Ok(mut writer) => write_message(&mut *writer, &message),
    // ...
};
```

The interactive socket uses a fixed five-second write timeout. This is failure
detection, not flow control.

## Commands You Will Need

| Purpose | Command | Expected on success |
|---|---|---|
| Client ordering tests | `cargo test --locked --lib client::tests -- --nocapture` | all pass |
| Daemon backpressure | `cargo test --locked --test daemon_lifecycle backpressure -- --nocapture` | all matching tests pass |
| Real client | `cargo test --locked --test client_ui slow_output -- --nocapture` | all matching tests pass |
| Full tests | `cargo test --locked --all-targets` | all pass |

## Scope

**In scope**:

- `src/daemon.rs`
- `src/client.rs`
- `src/session.rs` only for render-cache/full-render coordination
- `tests/daemon_lifecycle.rs`
- `tests/client_ui.rs`
- `plans/003-deliver-snapshots-in-order.md`
- `plans/README.md`

**Out of scope**:

- PTY queue ownership and asynchronous PTY input; Plan 004 owns them.
- Pane-bounded ANSI rendering; Plan 005 owns it.
- Binary protocol, compression or a larger frame limit.
- Multiple interactive clients.
- Async runtimes or a generic actor framework.

## Git Workflow

- Suggested branch: `hardening/003-ordered-snapshots`
- Suggested commit: `Deliver snapshots without blocking daemon state`
- Do not push unless requested.

## Task Status

| Task | Description | Status |
|---:|---|---|
| 1 | Prove that dropping deltas corrupts state | DONE |
| 2 | Add a bounded interactive-client writer owner | DONE |
| 3 | Materialize only one ordered Snapshot at a time | DONE |
| 4 | Remove client-side Snapshot dropping and restore fairness | DONE |
| 5 | Validate slow stdout, final output and reconnect behavior | DONE |

## Target State Model

Use standard-library threads and channels. The interactive client should have:

```text
daemon state owner
  - small pending control-message queue
  - at most one queued/in-flight Snapshot
  - dirty session set for state that changed while a frame is in flight
  - shutdown UnixStream clone
        |
        v
sync_channel(1)
        |
        v
one writer thread owning the socket writer
        |
        v
ClientWriteComplete { client_id, kind, result }
```

Do not render a second delta while the previous Snapshot is queued or being
written. This keeps `Session::rendered_rows` aligned with transport order
without introducing acknowledgements from the terminal renderer.

## Steps

### Step 1: Replace The Incorrect Snapshot Test

Remove `latest_server_snapshot_replaces_stale_snapshots`. Add tests proving:

1. Two same-generation Snapshot events are returned in original order over two
   calls to `receive_client_event`.
2. A control message between snapshots keeps its exact position.
3. Input remains responsive but cannot starve server events forever; use a
   small round-robin or fixed input burst rather than unconditional input-first
   selection.
4. Applying two Session deltas to a fresh `vt100::Parser` produces the same
   final visible screen as a forced full render of the final Session state.

The first and fourth assertions must fail on commit `83689a7` when the first
delta is discarded.

**Verify before implementation**: targeted new tests fail for the documented
reason, not because of timing.

### Step 2: Give The Interactive Client A Writer Thread

Refactor `Client` in `src/daemon.rs`:

1. The writer thread owns the writable `UnixStream` and reads from a
   `sync_channel` of capacity one.
2. `Client` retains a cloned stream only for `shutdown(Shutdown::Both)`, plus
   client id, token, session and output state.
3. After each attempted frame, the writer sends
   `Event::ClientWriteComplete { client_id, kind, result }` to the daemon.
4. Remove `CLIENT_WRITE_TIMEOUT` from the interactive daemon socket. A takeover,
   detach or lease expiry must call shutdown, which wakes a blocked writer.
5. Writer failure detaches only the matching client generation and never exits
   the daemon.

Short `list/new/kill/stop` request sockets may remain bounded synchronous writes
because their responses are small and do not own the interactive session. Do
not temporarily replace the interactive client's writer to serve them.

**Verify**: add a lifecycle test that stops reading the attached socket for
longer than five seconds while a separate `plux list` succeeds within one
second and the daemon remains alive.

### Step 3: Enforce One Ordered Snapshot In Flight

Add explicit output state; do not infer it from channel fullness.

Required rules:

1. Control messages are kept in a bounded `VecDeque` of small messages. Set a
   fixed internal maximum such as 64; overflow detaches the client with a debug
   diagnostic rather than growing memory indefinitely.
2. `send_snapshot` must not call `Session::render` when another Snapshot is
   queued or in flight. It leaves the session in `pending_snapshots` instead.
3. Once the writer completion for a Snapshot succeeds, clear the in-flight
   marker and render the latest pending state relative to the delivered frame.
4. `Attached` is written before the initial full Snapshot.
5. Pending pane output is delivered before its `ProcessExited` message. Keep
   exit notifications behind a Snapshot barrier until the relevant final
   Snapshot write completes.
6. Scroll/search/copy actions that request a Snapshot mark it pending when the
   writer is busy; they do not create an extra delta.
7. On writer failure or takeover, discard queued messages and force a full
   render on the next successful attach.

Do not add a Snapshot acknowledgement protocol. Socket write completion plus
strict client ordering is sufficient for this phase.

**Verify**:

- existing `final_snapshot_arrives_before_process_exit` passes;
- add `slow_client_does_not_block_daemon_or_lose_final_snapshot` and make it
  pass;
- attach still receives `Attached` before Snapshot.

### Step 4: Make Client Consumption Ordered And Bounded

In `src/client.rs`:

1. Remove same-generation Snapshot replacement logic and the helper used only
   for that behavior.
2. Preserve exact server event order.
3. Split capacities into a larger input queue and a smaller server queue; use a
   server capacity no greater than eight because each protocol frame may be up
   to 8 MiB.
4. Use fair selection: after a bounded number of consecutive input events,
   service one pending server event when available.
5. Remove `data.replace("\x1b[2J", "")`. Session rendering already has a test
   that it does not emit `2J`; the replace allocates another frame-sized String.
6. Do not treat server queue saturation as peer death. The reader thread may
   block and allow socket backpressure to reach the dedicated daemon writer.

**Verify**: client ordering, input fairness, selection overlay and reconnect
generation tests all pass.

### Step 5: Validate Real Slow Output

Using `tests/client_ui.rs`, add
`real_client_recovers_after_slow_output_without_losing_session`:

1. Fill the screen with rich ANSI output.
2. Pause reading the outer PTY for more than the old five-second timeout but
   less than the 30-second lease.
3. While paused, use an isolated short command to prove the daemon responds.
4. Resume output, wait for a current marker, send another shell command and
   observe its output.
5. Detach and reattach the same session; verify it survived.

Also run the existing large-resize backpressure test. It must no longer depend
on extending a timeout.

**Verify**:

```bash
cargo test --locked --test client_ui slow_output -- --nocapture
cargo test --locked --test daemon_lifecycle backpressure -- --nocapture
cargo test --locked --all-targets
```

All pass.

## Test Plan

Required coverage:

- ordered same-generation deltas;
- input/server event fairness;
- blocked interactive writer does not block `list`, heartbeat or daemon state;
- final Snapshot precedes `ProcessExited` under backpressure;
- reconnect after write failure receives a full render;
- real outer PTY pause/resume preserves session and current output.

Use existing client unit tests, `tests/daemon_lifecycle.rs`, and the Plan 001
real-client harness. Do not replace integration coverage with mocks.

## Done Criteria

- [x] No client code discards an earlier incremental Snapshot.
- [x] Interactive socket writes never occur on the daemon state-owner thread.
- [x] At most one Snapshot is queued or in flight per interactive client.
- [x] Session render cache advances in exactly the transport order.
- [x] Client event queues have a bounded worst-case frame count.
- [x] Input cannot starve server events indefinitely.
- [x] Slow-output and final-output regressions pass.
- [x] All shared verification gates pass.
- [x] Task statuses and `plans/README.md` are updated.

## STOP Conditions

Stop and report if:

- A test produces a legitimate Snapshot larger than the 8 MiB protocol limit.
  Do not raise the limit; record dimensions and serialized size for a separate
  chunking decision.
- Correct ordering appears to require terminal-render acknowledgements rather
  than socket write completion.
- Serving short requests without blocking requires redesigning the public CLI
  protocol.
- Any solution drops a delta, PTY byte or final process output.

## Maintenance Notes

The important review question is not whether queues are bounded; it is whether
the Session diff base corresponds to the last frame actually handed to the
writer in order. Future full-state Snapshot or protocol chunking work may allow
latest-wins replacement, but must change the protocol semantics and tests first.
