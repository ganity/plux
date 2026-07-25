# Plan 004: Bound PTY Flow And Keep Input And Leases Responsive

> **Executor instructions**: Preserve every PTY output byte. Bounded flow means
> applying backpressure, not dropping output. Add stress regressions first,
> update each task status after verification, and update Plan 004 in
> `plans/README.md` when complete.
>
> **Drift check (run first)**:
> `git diff --stat 83689a7..HEAD -- src/daemon.rs src/pane.rs src/session.rs tests/daemon_lifecycle.rs tests/client_ui.rs`
> Reconcile this plan with Plan 003's live writer-completion event before work.

## Status

- **Priority**: P0
- **Effort**: L
- **Risk**: HIGH
- **Depends on**: `plans/001-real-client-pty-harness.md`,
  `plans/003-deliver-snapshots-in-order.md`
- **Category**: bug, perf, tech-debt
- **Planned at**: commit `83689a7`, 2026-07-25
- **Status**: DONE

## Why This Matters

Every pane reader allocates up to 32 KiB per output event and sends it through
two unbounded channels. The daemon then drains the unified queue until empty.
Continuous output can grow memory without limit and can prevent snapshots,
search, accepts and heartbeat processing from running. In the other direction,
the daemon writes user input directly to the PTY; a child that stops reading can
freeze the entire daemon. This plan bounds both directions and makes overload a
controlled backpressure state.

## Current State

`src/daemon.rs:132-153` creates two unbounded queues and a forwarding thread:

```rust
let (events_tx, events_rx) = mpsc::channel();
let (pane_events_tx, pane_events_rx) = mpsc::channel();
// pane_events_rx is forwarded into events_tx
```

`src/daemon.rs:193` drains until empty:

```rust
while let Ok(event) = self.events_rx.try_recv() {
    self.handle_event(event)?;
}
```

`src/pane.rs:165-180` copies each PTY read into a new Vec and sends it through
the unbounded queue. `src/pane.rs:115-118` writes input synchronously.

Client liveness currently uses `last_seen` updated only when the daemon handles
the queued message (`src/daemon.rs:488-501`), while lease expiry is checked
before processing the queue.

Design constraints from `DESIGN.md:424-437`:

- one clear PTY owner;
- input takes priority over screen refresh;
- PTY reads use bounded chunks;
- PTY bytes cannot be dropped.

## Commands You Will Need

| Purpose | Command | Expected on success |
|---|---|---|
| Flow tests | `cargo test --locked --test daemon_lifecycle flow -- --nocapture` | all matching tests pass |
| Paste/input tests | `cargo test --locked --test client_ui paste -- --nocapture` | all matching tests pass |
| Full tests | `cargo test --locked --all-targets` | all pass |
| Release tests | `cargo test --locked --release --all-targets` | all pass |

## Scope

**In scope**:

- `src/daemon.rs`
- `src/pane.rs`
- `src/session.rs` only for input queue access through focused panes
- `tests/daemon_lifecycle.rs`
- `tests/client_ui.rs`
- `plans/004-bound-pty-and-daemon-flow.md`
- `plans/README.md`

**Out of scope**:

- Snapshot ordering; retain Plan 003's model.
- Terminal parsing/reflow.
- Multiple clients or per-session daemon threads.
- Dropping or sampling PTY output.
- Async runtimes.

## Git Workflow

- Suggested branch: `hardening/004-bounded-pty-flow`
- Suggested commit: `Bound PTY flow and isolate pane input writes`
- Do not push unless requested.

## Task Status

| Task | Description | Status |
|---:|---|---|
| 1 | Add sustained-output and blocked-input regressions | DONE |
| 2 | Replace unbounded event forwarding with bounded queues | DONE |
| 3 | Add fixed event-loop work budgets | DONE |
| 4 | Move PTY input writes to a pane-owned writer | DONE |
| 5 | Renew leases when valid messages arrive | DONE |

## Target State Model

```text
client readers / signal reader / snapshot writer completion
              |
              v
      bounded control queue

PTY output readers (32 KiB maximum each event)
              |
              v
        bounded pane queue

daemon loop: accept -> bounded control work -> bounded pane work
             -> resize/search -> snapshot scheduling -> lease cleanup

daemon Input message -> bounded pane input backlog -> pane writer thread
```

Count-bounded PTY output is sufficiently byte-bounded because every output
event is capped at 32 KiB. Keep the constants private and documented next to
the channels.

## Steps

### Step 1: Add Flow-Control Regressions

Add deterministic tests:

1. `sustained_output_keeps_heartbeat_and_short_requests_responsive`: run a
   high-output child, attach without consuming every Snapshot, send Heartbeat,
   and concurrently run `plux list`. Both responses must arrive within bounded
   deadlines and the daemon must remain alive.
2. `sustained_output_preserves_tail_marker`: produce a finite large stream with
   a unique final marker, then assert the marker eventually reaches terminal
   state. This catches dropped PTY bytes.
3. `blocked_child_input_does_not_block_daemon`: attach to a child that does not
   consume stdin, send a paste larger than the PTY kernel buffer, and prove a
   short request and stop/takeover path remain responsive.
4. A real-client paste test sends bracketed paste through `tests/client_ui.rs`
   and verifies normal-size paste remains exact.

At least the first or third test should fail on commit `83689a7` without relying
on process-wide memory measurements.

**Verify before implementation**: targeted regressions fail for daemon
responsiveness, not fixture startup.

### Step 2: Use Separate Bounded Queues

In `Daemon::new`:

1. Replace the unified unbounded event queue with a bounded control queue for
   client messages, disconnects, signals and Plan 003 writer completions. A
   capacity around 256 is sufficient; use one named constant.
2. Replace the PaneEvent queue with `sync_channel` capacity 128. With 32 KiB
   chunks this caps queued PTY bytes near 4 MiB globally.
3. Remove the pane-event forwarding thread entirely. `Daemon` owns both
   receivers and polls them directly.
4. Update `Pane` and reader signatures to accept `SyncSender<PaneEvent>`.
5. A blocked pane sender is intentional backpressure: the PTY kernel buffer
   fills and the child slows instead of daemon memory growing.

Do not place PTY output into the control queue.

**Verify**: `rg -n "mpsc::channel\(\)" src/daemon.rs` returns no event/pane
channel matches, and pane unit tests pass.

### Step 3: Budget Every Event-Loop Turn

Refactor `event_loop` so one producer cannot monopolize a turn:

1. Accept at most one new socket each turn.
2. Process at most a named number of control events, for example 64.
3. Process at most a named number of PaneEvents, for example 64.
4. Then always run due resize, one search step, Snapshot scheduling and lease
   cleanup.
5. If no immediate work exists, wait no more than 10-20 ms for one control
   event, then begin another turn. Pane-only output may wait one tick; that is
   acceptable and bounded.
6. Handle shutdown as local daemon state rather than sending into the daemon's
   own bounded queue, which could deadlock when full.

Preserve client-id/generation filtering before applying any stale event.

**Verify**: sustained-output tests receive HeartbeatAck and `list` within their
deadlines while the final marker is preserved.

### Step 4: Give Each Pane An Input Writer

Move blocking PTY writes out of the daemon loop:

1. A pane writer thread owns `Box<dyn Write + Send>`.
2. A capacity-one channel carries one input chunk to that writer.
3. `Pane` owns a `VecDeque<Vec<u8>>`, byte count and in-flight flag. The daemon
   enqueues input and pumps one chunk when the writer is idle.
4. The writer reports `PaneEvent::InputWritten { pane_id, result }`; on success
   the pane pumps the next queued chunk.
5. Cap pending input at 1 MiB with a named constant. If exceeded, return a clear
   `pane input backlog exceeded` error, keep daemon/session alive and do not
   allocate more. Do not silently drop bytes.
6. Pane close/kill drops queued input and shuts down the writer with the pane.
7. Plan 008 terminal replies must later use this same queue.

Move the already-owned `Vec<u8>` from `ClientMessage::Input`; avoid an extra
copy where practical.

**Verify**: blocked-child test proves daemon responsiveness; ordinary typing
and bracketed paste remain byte-for-byte correct.

### Step 5: Make Lease Renewal Independent Of Queue Delay

For each installed interactive client:

1. Create a small shared arrival timestamp, such as
   `Arc<Mutex<Instant>>`, held by the reader thread and `Client`.
2. After successfully decoding and validating a complete client message, update
   the timestamp before attempting to enqueue the event.
3. `client_lease_expired` reads this arrival timestamp. It must not use channel
   fullness or daemon handling time as evidence of death.
4. Keep EOF, socket error, SSH child exit and missed HeartbeatAck as the actual
   connection-failure signals.
5. Ignore timestamp updates from stale client ids after takeover.

**Verify**: add a test where control processing is deliberately delayed while
heartbeats have already arrived; the valid client must not be expired.

## Test Plan

Cover:

- sustained infinite and finite PTY output;
- final marker preservation;
- heartbeat and short-request latency under output load;
- blocked child stdin and input backlog cap;
- normal and bracketed paste integrity;
- lease renewal at message arrival;
- takeover wakes blocked pane/client writers and leaves no daemon hang.

Use release-mode tests for at least the sustained-output scenario because debug
channel scheduling can hide throughput behavior.

## Done Criteria

- [x] No unbounded queue carries PTY or client protocol traffic.
- [x] No drain-until-empty loop can starve periodic daemon work.
- [x] PTY output bytes are backpressured, never dropped.
- [x] PTY input writes cannot block the daemon loop.
- [x] Pending pane input has a documented byte cap and explicit error.
- [x] Lease timestamps update on valid message arrival.
- [x] High-output, blocked-input and paste regressions pass.
- [x] Debug and release shared gates pass.
- [x] Task statuses and `plans/README.md` are updated.

## STOP Conditions

Stop and report if:

- `portable-pty` cannot safely move the writer handle to a dedicated thread on
  either Linux or macOS.
- Bounded output loses the final marker or any PTY bytes.
- The input backlog cannot be bounded without silently dropping input.
- Plan 003's writer-completion events cannot share the bounded control queue
  without deadlock.
- The implementation grows into a general scheduler or async runtime.

## Maintenance Notes

Review queue capacity in bytes, not only item count. PTY output items are fixed
at 32 KiB, while protocol messages are not. Future work that increases PTY read
chunk size must revisit the capacity calculation. Queue saturation is overload,
not a liveness signal.
