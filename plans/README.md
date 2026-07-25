# Plux Core Reliability Plans

Generated on 2026-07-25 from commit `83689a7` after a whole-path audit of the
client, SSH bridge, daemon event loop, PTY ownership, terminal state, renderer,
protocol, tests and CI.

These plans are deliberately limited to Plux's basic multiplexer contract:
stable attach/reconnect, responsive input, resize, rendering, scrollback,
selection and common full-screen terminal applications. Do not add a TCP
listener, an async runtime, plugins, simultaneous interactive clients or a
tmux-compatible command language while executing them.

## Status Rules

| Status | Meaning |
|---|---|
| `TODO` | No implementation work has started. |
| `IN PROGRESS` | At least one task is being implemented, but all done criteria have not passed. |
| `DONE` | Every task and verification gate in the plan passed and was recorded. |
| `BLOCKED: <reason>` | A STOP condition was reached and requires review. |
| `REJECTED: <reason>` | The finding was fixed independently or the approach was abandoned. |

Every executor must update both the plan's task-status table and the row below
as work progresses. A task may be marked `DONE` only after its own verification
command succeeds.

## Execution Order And Status

| Plan | Title | Priority | Effort | Risk | Depends on | Status |
|---|---|---:|---:|---:|---|---|
| [001](001-real-client-pty-harness.md) | Establish a real client PTY regression harness | P0 | M | LOW | - | DONE |
| [002](002-coalesce-terminal-resize.md) | Coalesce terminal resize before touching PTYs | P0 | M | MED | 001 | DONE |
| [003](003-deliver-snapshots-in-order.md) | Deliver incremental snapshots in order without blocking daemon state | P0 | L | HIGH | 001, 002 | DONE |
| [004](004-bound-pty-and-daemon-flow.md) | Bound PTY flow and keep input and leases responsive | P0 | L | HIGH | 001, 003 | DONE |
| [005](005-render-pane-bounds-safely.md) | Render pane deltas without erasing adjacent panes | P0 | M | MED | 001, 003 | DONE |
| [006](006-harden-client-lifecycle.md) | Harden handshake, bridge, reconnect and search lifecycle | P1 | M | MED | 003, 004 | DONE |
| [007](007-fix-scrollback-resize-semantics.md) | Enforce scrollback memory limits and preserve content across resize | P1 | L | HIGH | 002, 005 | DONE |
| [008](008-complete-terminal-compatibility-gates.md) | Add minimal terminal replies and release-grade compatibility gates | P1 | M | MED | 004, 006, 007 | DONE |

## Dependency Notes

```text
001 real client harness
  |
  v
002 resize coalescing
  |
  v
003 ordered snapshot delivery
  |
  +-----------> 005 pane-safe rendering ----+
  v                                         |
004 bounded PTY flow                        v
  |                                      007 scrollback/reflow
  v                                         |
006 lifecycle/bridge/search ----------------+
                    |
                    v
             008 terminal/CI gates
```

- Plan 001 must land first because the current suite does not run the real
  `client::enter` loop inside a pseudo-terminal.
- Plan 002 must precede Plan 003 so resize storms do not obscure frame-delivery
  behavior during slow-client tests.
- Plan 003 must precede Plan 004 because both change daemon scheduling, and the
  snapshot writer completion event becomes part of the bounded event model.
- Plan 008 depends on Plan 004 because terminal query replies must use the
  non-blocking PTY input path instead of writing from the daemon loop.

## Shared Verification Gates

Run these after every plan unless the plan specifies a narrower command first:

| Purpose | Command | Expected result |
|---|---|---|
| Format | `cargo fmt --check` | exit 0 |
| Debug tests | `cargo test --locked --all-targets` | all tests pass |
| Release tests | `cargo test --locked --release --all-targets` | all tests pass |
| Lint | `cargo clippy --locked --all-targets --all-features -- -D warnings` | exit 0, no warnings |
| Fuzz build | `cargo check --locked --manifest-path fuzz/Cargo.toml` | exit 0 |
| Release build | `cargo build --locked --release` | exit 0 |

Do not update static test counts in documentation. They become stale after the
next test is added.

## Historical Plans

- `plans/reliability-hardening.md` remains a historical record. Its tasks 6,
  7 and 13 are superseded by Plans 002-007 because later failures proved that
  fixed socket timeouts, count-bounded client events and search slicing did not
  establish correct frame delivery or flow control.
- `plans/remote-client-over-ssh.md` remains the SSH design record. Its unfinished
  adapter, reconnect and leak testing tasks are superseded by Plans 003, 006
  and 008.

Do not delete either historical plan. Their completed protocol and CLI work is
still useful context.

## Findings Considered And Deferred

- Multiple simultaneous interactive clients: intentionally deferred. The
  current basic product can keep a single active client while reliability is
  repaired.
- Raw TCP, QUIC or a Plux authentication layer: rejected for this cycle. SSH is
  already the transport and identity boundary.
- Async runtime or generic transport framework: rejected. Standard threads,
  bounded channels and explicit state are sufficient.
- Binary protocol rewrite or compression: deferred until ordered delivery and
  resize coalescing are measured. Keep the current 8 MiB frame limit explicit;
  do not silently raise it to hide another bug.
- Terminal graphics protocols and complete xterm emulation: deferred. Plan 008
  adds only the replies needed by common shell and full-screen applications.
- Daemon-crash or reboot process restoration: outside the MVP contract.

## Completion Record

| Date | Plan | Verification | Result |
|---|---|---|---|
| 2026-07-25 | 001 | `cargo fmt --check`; `cargo test --locked --all-targets`; `cargo clippy --locked --all-targets --all-features -- -D warnings`; `cargo test --locked --test client_ui -- --nocapture` | DONE: real client PTY attach/input/render/detach and resize coverage added; 98 debug tests passed; Clippy and formatting passed. |
| 2026-07-25 | 002 | `cargo fmt --check`; `cargo test --locked --all-targets`; `cargo clippy --locked --all-targets --all-features -- -D warnings`; resize tests in `daemon_lifecycle` and `client_ui` | DONE: SIGWINCH-driven client resize, 100ms daemon debounce, final PTY-size verification and large real-client drag coverage; 100 debug tests passed. |
| 2026-07-25 | 003 | `cargo fmt --check`; `cargo test --locked --all-targets`; `cargo test --locked --release --all-targets`; `cargo clippy --locked --all-targets --all-features -- -D warnings`; real-client slow-output and daemon ordering tests | DONE: dedicated FIFO client writer, one Snapshot in flight, bounded client event flow, ordered client consumption, final-output barrier and slow outer-reader coverage. |
| 2026-07-25 | 004 | `cargo fmt --check`; `cargo test --locked --all-targets`; `cargo test --locked --release --all-targets`; `cargo check --locked --manifest-path fuzz/Cargo.toml`; `cargo clippy --locked --all-targets --all-features -- -D warnings` | DONE: bounded control/pane queues and work budgets, pane-owned input writer with 1 MiB cap, arrival-time lease renewal, blocked-input and paste coverage. |
| 2026-07-25 | 005 | `cargo test --locked --bin plux session::tests::left_pane_delta_does_not_bleed_into_right_pane`; `cargo test --locked --test client_ui`; shared gates | DONE: bounded pane erases, attribute resets, stale-row cleanup, parser-backed adjacent-pane protection and tiny-layout fallback. |
| 2026-07-25 | 006 | `cargo test --locked --test daemon_lifecycle bridge`; lifecycle admission, search, reconnect and cleanup tests; shared gates | DONE: validated first-frame admission, half-close bridge cleanup, search restoration, bounded UI status, create race handling and repeated same-token reconnect. |
| 2026-07-25 | 007 | `cargo test --locked --bin plux terminal::tests`; `cargo test --locked --test daemon_lifecycle scrollback`; `cargo check --locked --manifest-path fuzz/Cargo.toml`; release gates | DONE: actual-cell history budget, width-aware logical-line reflow, wide-cell handling, row-shrink preservation and viewport/cursor remapping. |
| 2026-07-25 | 008 | `cargo fmt --check`; `cargo test --locked --all-targets`; `cargo test --locked --release --all-targets`; `cargo clippy --locked --all-targets --all-features -- -D warnings`; `cargo check --locked --manifest-path fuzz/Cargo.toml`; `cargo build --locked --release`; `git diff --check` | DONE: exact common terminal replies, pane input routing, daemon child-query integration, CI quality job and compatibility documentation; largest stress Snapshot measured at 3,716,192 bytes, below the 8 MiB frame limit. |
