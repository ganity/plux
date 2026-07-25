# Plan 006: Harden Handshake, Bridge, Reconnect And Search Lifecycle

> **Executor instructions**: Keep session ownership and transport ownership
> separate. Add each lifecycle regression before its fix, update task statuses
> as verification passes, and update Plan 006 in `plans/README.md` when done.
>
> **Drift check (run first)**:
> `git diff --stat 83689a7..HEAD -- src/daemon.rs src/transport.rs src/client.rs src/protocol.rs tests/daemon_lifecycle.rs tests/client_ui.rs`
> Stop if the attach handshake, bridge forwarding or search task ownership has
> materially changed.

## Status

- **Priority**: P1
- **Effort**: M
- **Risk**: MED
- **Depends on**: `plans/003-deliver-snapshots-in-order.md`,
  `plans/004-bound-pty-and-daemon-flow.md`
- **Category**: bug
- **Planned at**: commit `83689a7`, 2026-07-25
- **Status**: DONE

## Why This Matters

Several lifecycle states are currently represented by incidental socket or
message timing. A connection is installed before it has sent a valid Attach,
and a client with no token never reaches lease expiry. The SSH bridge ignores
stdin EOF and leaves its socket open until the daemon lease closes it. Search
is cancelled by every non-Search message, including Heartbeat, without restoring
the previous viewport. These are separate symptoms of the same rule: handshake,
transport, lease and UI tasks need explicit transitions and cleanup.

## Current State

`src/daemon.rs:313-317` installs an unauthenticated connection immediately when
no client exists:

```rust
if self.client.is_none() {
    self.install_client(accepted, None)?;
    return Ok(());
}
```

`client_lease_expired` ignores `token=None`, so a silent peer can remain forever.

`src/transport.rs:136-163` copies stdin on a detached thread. When stdin ends it
does not call socket shutdown, and the main thread remains blocked reading the
daemon socket.

`src/daemon.rs:553-559` cancels search for every non-Search message:

```rust
if !matches!(&message, ClientMessage::Search { .. }) {
    self.search_task = None;
}
```

The client sends Heartbeat every five seconds. A full 20,000-line missing search
can therefore be cancelled before completion.

Smaller client lifecycle hazards are in the same paths:

- connection/status text is not width-bounded and can wrap the terminal;
- resize clamps the cursor but not `selection_anchor`;
- automatic create performs List then Create and treats a same-name race as an
  error instead of continuing to attach.

## Commands You Will Need

| Purpose | Command | Expected on success |
|---|---|---|
| Lifecycle tests | `cargo test --locked --test daemon_lifecycle lifecycle -- --nocapture` | all matching tests pass |
| Bridge tests | `cargo test --locked --test daemon_lifecycle bridge -- --nocapture` | all matching tests pass |
| Client tests | `cargo test --locked --lib client::tests -- --nocapture` | all pass |
| Real reconnect tests | `cargo test --locked --test client_ui reconnect -- --nocapture` | all matching tests pass |

## Scope

**In scope**:

- `src/daemon.rs`
- `src/transport.rs`
- `src/client.rs`
- `tests/daemon_lifecycle.rs`
- `tests/client_ui.rs`
- `plans/006-harden-client-lifecycle.md`
- `plans/README.md`

**Out of scope**:

- New transport protocols or authentication.
- Simultaneous different-token clients.
- Input replay across disconnect.
- A complete search index.
- Public CLI expansion.

## Git Workflow

- Suggested branch: `hardening/006-client-lifecycle`
- Suggested commit: `Harden client and bridge lifecycle transitions`
- Do not push unless requested.

## Task Status

| Task | Description | Status |
|---:|---|---|
| 1 | Require a valid first message before installing a client | DONE |
| 2 | Make bridge EOF terminate both forwarding directions | DONE |
| 3 | Separate search cancellation from Heartbeat and restore viewport | DONE |
| 4 | Harden resize/status/selection client UI state | DONE |
| 5 | Make automatic create resilient to same-name races | DONE |
| 6 | Validate reconnect and process cleanup repeatedly | DONE |

## Steps

### Step 1: Validate The First Frame Before Client Installation

Unify accepted-connection admission:

1. Read one complete first message with the existing short bounded read timeout
   before installing any interactive client, whether or not a client currently
   exists.
2. Validate token, session name and target before takeover or installation.
3. Install an interactive client only for a valid Attach or Takeover, with its
   token present from the beginning. Remove the `token=None` interactive state.
4. Route `Create/List/Kill/Shutdown` through the short-request path without
   replacing the active interactive client.
5. A silent, partial, invalid or timed-out peer is closed and never owns the
   attach slot.
6. Preserve same-token reconnect, different-token lease rejection and explicit
   takeover behavior.

Add tests for a silent socket, partial protocol header, invalid first message,
same-token reconnect and a subsequent valid attach.

**Verify**: `unauthenticated_interactive_client_cannot_hold_attach_slot` is
expanded to cover no bytes and partial bytes, and all admission tests pass.

### Step 2: Coordinate Bridge Half-Close And Exit

Refactor `transport::bridge` using existing UnixStream clones and one small
completion channel:

1. When stdin-to-socket copy reaches EOF, call
   `socket_writer.shutdown(Shutdown::Write)`.
2. The daemon reader then observes EOF, detaches and closes its output side;
   socket-to-stdout reaches EOF and the bridge exits.
3. If socket-to-stdout fails first, call `Shutdown::Both` to wake the stdin
   direction.
4. Join the stdin forwarding thread before returning. Do not `drop` the handle.
5. Preserve stdout as pure protocol bytes and diagnostics on stderr only.
6. Ensure all exit paths return a useful error while still cleaning up.

Add `bridge_exits_when_stdin_closes_while_session_is_idle` and
`bridge_exits_when_daemon_closes_first`. Both must have short deadlines and
assert the child process exits.

**Verify**: `cargo test --locked --test daemon_lifecycle bridge -- --nocapture`
-> all bridge tests pass.

### Step 3: Give Search Explicit Cancellation Semantics

Replace blanket `search_task = None` with a helper that can restore the original
scrollback offset before dropping the task.

Rules:

1. Heartbeat, Ping and unrelated output do not cancel search.
2. A new Search replaces the previous task after restoring its original offset.
3. User navigation, input that leaves search mode, resize, detach, takeover,
   kill and shutdown cancel with restoration unless the search already found a
   result and intentionally moved the viewport.
4. A missing query completes and returns `SearchResult { found: false }` even
   when one or more Heartbeats arrive during scanning.
5. Search task state remains bound to the same session and focused pane; focus
   changes cancel and restore rather than scanning a different pane.

Add tests:

- `heartbeat_does_not_cancel_search` sends Search immediately followed by
  Heartbeat and expects both Ack and SearchResult.
- `cancelled_search_restores_scrollback` records the original offset, cancels
  through resize or scroll, and verifies restoration.

**Verify**: targeted search tests and existing non-blocking search test pass.

### Step 4: Keep Client UI State Inside The Physical Terminal

In `src/client.rs`:

1. `InputState::set_size` clamps or cancels an active selection anchor as well
   as the cursor. Choose cancellation when the anchor falls outside the new
   viewport; do not preserve invalid coordinates.
2. Connection and input status rendering accepts columns and truncates by
   terminal display width so it never wraps or scrolls the alternate screen.
3. Preserve a final reset and cursor restoration after truncated status text.
4. Add unit tests with one-column/two-row terminals, long SSH errors and an
   active selection during shrink.

Do not add a permanent status bar or layout row.

**Verify**: client unit tests pass and no status write exceeds the supplied
column count.

### Step 5: Close The Automatic-Create Race

`create_session_if_missing` and its SSH variant perform a List then Create.
Two clients can both observe absence and one receives `session already exists`.

Keep the current protocol and make the narrow fix:

1. If Create returns exactly the existing-session condition, treat it as a
   successful race and continue to attach.
2. Preserve all other create errors.
3. Apply the same behavior locally and through SSH.
4. Add a concurrent same-name create/enter integration test.

Do not introduce `AttachOrCreate` or another protocol message in this plan.

**Verify**: concurrent same-name startup results in one session and successful
attachment/takeover behavior, with no extra daemon.

### Step 6: Repeat Reconnect And Cleanup

Extend the real client harness or existing fake-SSH path:

1. Interrupt and recreate the bridge at least ten times with the same client
   token.
2. After every reconnect, wait for a full Snapshot before sending input.
3. Assert the old bridge exits within a bounded deadline.
4. Assert no bridge/SSH child count grows across iterations.
5. Resize and run a search during selected iterations to exercise cancellation
   and generation filtering.
6. Finish with a normal detach and daemon stop.

Use fake SSH for deterministic CI. A localhost SSH smoke may remain optional and
must be reported as skipped when unavailable.

**Verify**: `cargo test --locked --test client_ui reconnect -- --nocapture` ->
pass without leftover isolated child processes.

## Test Plan

Required coverage:

- silent and partial first connection cannot own the slot;
- valid same-token reconnect and explicit takeover still work;
- bridge exits on either direction's EOF;
- Heartbeat does not cancel search;
- cancelled search restores scrollback;
- selection and status text survive tiny resize;
- same-name auto-create race succeeds;
- repeated reconnect does not accumulate children.

## Done Criteria

- [x] No interactive `Client` exists without a validated token.
- [x] Silent/partial peers have bounded admission time and no ownership.
- [x] Bridge joins both forwarding directions and exits on either EOF.
- [x] Heartbeat cannot cancel search.
- [x] Every cancelled search restores its original viewport.
- [x] Status text cannot wrap and selection coordinates remain valid.
- [x] Same-name automatic creation is race-tolerant.
- [x] Repeated reconnect leaves no isolated bridge/SSH children.
- [x] All shared verification gates pass.
- [x] Task statuses and `plans/README.md` are updated.

## STOP Conditions

Stop and report if:

- Reading the first frame before installation causes valid SSH attach to exceed
  the current admission timeout on supported environments.
- Correct bridge cleanup requires platform-specific process inspection outside
  the isolated test runtime.
- Search restoration cannot identify the original focused pane after a layout
  change; report the state transition rather than guessing.
- Closing the create race requires a protocol change.

## Maintenance Notes

The lease answers "is this validated client still arriving at the daemon", not
"is a queue empty". The bridge owns only byte forwarding, never session life.
Future lifecycle changes should add explicit state transitions and tests rather
than another timeout or error-string branch.
