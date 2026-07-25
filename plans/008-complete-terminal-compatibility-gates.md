# Plan 008: Add Minimal Terminal Replies And Release-Grade Compatibility Gates

> **Executor instructions**: Implement only the terminal replies listed here.
> Do not expand into full xterm emulation. Update each task after verification,
> then update Plan 008 and the completion record in `plans/README.md`.
>
> **Drift check (run first)**:
> `git diff --stat 83689a7..HEAD -- src/terminal.rs src/pane.rs src/daemon.rs vendor/vt100/src/callbacks.rs vendor/vt100/src/perform.rs tests .github/workflows/build.yml README.md docs/compatibility.md`
> Stop if terminal callbacks or the pane input owner differ from Plans 004/007.

## Status

- **Priority**: P1
- **Effort**: M
- **Risk**: MED
- **Depends on**: `plans/004-bound-pty-and-daemon-flow.md`,
  `plans/006-harden-client-lifecycle.md`,
  `plans/007-fix-scrollback-resize-semantics.md`
- **Category**: bug, tests, dx, docs
- **Planned at**: commit `83689a7`, 2026-07-25
- **Status**: DONE

## Why This Matters

Plux advertises `TERM=xterm-256color`, but TerminalState constructs vt100 with
unit callbacks. Unsupported CSI/OSC operations are silently discarded, so
applications that query terminal identity, readiness, cursor position or size
may wait, time out or mis-detect capabilities. The repository also lacks a CI
gate for format, strict Clippy, fuzz-target compilation and the real client PTY
harness. This final plan implements only common request/reply behavior and makes
the repaired interaction paths release gates.

## Current State

`src/terminal.rs:24` uses:

```rust
parser: Parser::new(rows, cols, history_lines),
```

`vendor/vt100/src/callbacks.rs:58` defines `impl Callbacks for () {}`, so
unhandled CSI/OSC, bell, title and query callbacks do nothing.

`src/pane.rs:75-82` advertises:

```rust
command_builder.env("TERM", "xterm-256color");
command_builder.env("COLORTERM", "truecolor");
```

`.github/workflows/build.yml:30-38` currently runs only tests and release build
on the platform matrix. README still states a static 39-test count, and
compatibility documentation describes real SSH verification as pending even
though the product now depends on that path.

## Supported Reply Set

Implement only these replies through vt100 callbacks:

| Request | Reply | Purpose |
|---|---|---|
| `CSI 5 n` | `CSI 0 n` | terminal status OK |
| `CSI 6 n` | `CSI <row>;<col> R` | one-based cursor position |
| primary DA `CSI c` / `CSI 0 c` | fixed truthful VT-style identity | basic terminal identity |
| secondary DA `CSI > c` | fixed Plux-compatible identity tuple | version/capability probe |
| `CSI 18 t` | `CSI 8;<rows>;<cols> t` | character-cell window size |

Choose fixed DA response constants once, document them beside the callback and
test exact bytes. Do not claim support for sixel, graphics, colors beyond the
existing parser, or a terminal feature Plux does not implement.

## Commands You Will Need

| Purpose | Command | Expected on success |
|---|---|---|
| Terminal replies | `cargo test --locked --lib terminal::tests -- --nocapture` | all pass |
| PTY query integration | `cargo test --locked --test daemon_lifecycle terminal_query -- --nocapture` | all matching tests pass |
| Full validation | commands in `plans/README.md` | all exit 0 |
| Workflow syntax | `git diff --check` | exit 0 |

## Scope

**In scope**:

- `src/terminal.rs`
- `src/pane.rs`
- `src/daemon.rs` only to route parser replies through Plan 004's pane input
  queue
- `vendor/vt100/src/callbacks.rs` or adjacent callback integration points
- terminal and daemon integration tests
- `tests/client_ui.rs`
- `.github/workflows/build.yml`
- `README.md`
- `docs/compatibility.md`
- `CHANGELOG.md` only for the resulting release note
- `plans/008-complete-terminal-compatibility-gates.md`
- `plans/README.md`

**Out of scope**:

- OSC 52 clipboard handling from child applications.
- Window title forwarding, audible/visual bell UX and graphics protocols.
- Changing TERM or shipping a new terminfo entry.
- Binary protocol/chunking.
- Claiming compatibility with an application not actually tested.

## Git Workflow

- Suggested branch: `hardening/008-terminal-compatibility`
- Suggested commit: `Answer basic terminal queries and enforce compatibility checks`
- Do not push unless requested.

## Task Status

| Task | Description | Status |
|---:|---|---|
| 1 | Add exact parser reply unit tests | DONE |
| 2 | Route callback replies through the pane input owner | DONE |
| 3 | Add child-process terminal query integration tests | DONE |
| 4 | Add all quality and real-client gates to CI | DONE |
| 5 | Refresh compatibility and verification documentation | DONE |
| 6 | Run final debug/release and platform acceptance | DONE |

## Steps

### Step 1: Define A Small Terminal Callback State

In `src/terminal.rs`:

1. Add a concrete callback state containing only a bounded reply queue. A small
   `VecDeque<Vec<u8>>` is sufficient.
2. Construct `Parser::new_with_callbacks` instead of unit callbacks.
3. Implement exact handling for the five request classes in the Supported Reply
   Set. Cursor replies read the current parser Screen position and convert to
   one-based coordinates.
4. Bound queued reply bytes with a small named limit. Repeated malicious queries
   must not grow memory indefinitely; on overflow retain one explicit terminal
   error state for the pane owner to report.
5. Add `take_replies()` or make `process()` return drained replies. Keep normal
   terminal output parsing behavior unchanged.
6. Continue ignoring unsupported callbacks deliberately and document the list;
   do not log every unknown escape under normal operation.

Add exact-byte unit tests for each query, multiple queries in one parser chunk,
queries split across chunks and cursor position after wide characters.

**Verify**: `cargo test --locked --lib terminal::tests -- --nocapture` -> pass.

### Step 2: Send Replies Through The Pane Input Queue

Integrate with Plan 004's pane-owned input writer:

1. After `Pane::process_output` feeds bytes into TerminalState, drain generated
   replies.
2. Enqueue replies through the same bounded, ordered pane input path used for
   user input. Never write directly from the daemon loop.
3. Preserve order between a query reply and subsequent user input as observed
   by the child PTY.
4. If reply backlog overflows, surface one clear pane/daemon diagnostic and keep
   the daemon alive.
5. Replies are internal terminal protocol traffic; do not send them to the
   external Plux client protocol.

**Verify**: pane unit tests prove query replies enter the input queue in order
without blocking daemon processing.

### Step 3: Add A Child That Waits For Replies

Add integration tests using `/bin/sh` and standard terminal tools available on
Linux/macOS, or a small test-helper mode in the Plux test binary if shell
portability is insufficient.

Required cases:

1. Child switches its PTY to raw/no-echo mode, writes `CSI 5 n`, reads the exact
   status reply and prints `DSR_OK`.
2. Child moves the cursor, writes `CSI 6 n`, validates one-based coordinates and
   prints `CPR_OK`.
3. Child requests size, validates the dimensions applied by Plux and prints
   `SIZE_OK`.
4. Child sends a split query across writes to prove parser chunk boundaries do
   not matter.
5. While a query is pending, Heartbeat and `list` remain responsive.

All test subprocess reads must have deadlines.

**Verify**: `cargo test --locked --test daemon_lifecycle terminal_query -- --nocapture` -> pass.

### Step 4: Make Quality Checks CI Gates

Update `.github/workflows/build.yml` with the smallest clear structure:

1. Keep the four Linux/macOS release target builds.
2. Run `cargo test --locked --all-targets --target <matrix target>` as today,
   including `tests/client_ui.rs`.
3. Add one Linux quality job or guarded matrix step for:
   - `cargo fmt --check`;
   - `cargo clippy --locked --all-targets --all-features -- -D warnings`;
   - `cargo check --locked --manifest-path fuzz/Cargo.toml`.
4. Do not run the same Clippy/fuzz work four times.
5. Preserve artifact packaging only after tests/build succeed.
6. Keep fake SSH/reconnect tests mandatory; localhost SSH remains an optional
   documented smoke test, not a CI dependency.

Do not add third-party CI action dependencies beyond those already used unless
strictly required.

**Verify**: inspect workflow diff, run all commands locally and run
`git diff --check` -> exit 0.

### Step 5: Make Documentation Match Reality

Update docs after tests pass:

1. Remove the static test count from README; list commands and critical test
   layers instead.
2. Update `docs/compatibility.md` with actual automated coverage: daemon
   protocol, real client PTY, fake SSH/reconnect, resize, slow output, split,
   scrollback and terminal replies.
3. Keep manual applications/environment explicitly marked verified, unverified
   or known-limited. Do not claim Codex/Vim/HTop compatibility from parser tests
   alone.
4. Document the supported terminal query set and unsupported graphics/title
   behavior.
5. Update CHANGELOG with user-visible reliability changes only after all prior
   plans are DONE.
6. Mark superseded historical-plan tasks through `plans/README.md`; do not
   rewrite their history.

**Verify**: README commands run exactly as written and `rg -n "39 tests" README.md`
returns no matches.

### Step 6: Run Final Acceptance And Measure Frame Ceiling

Run every shared verification gate in debug and release. In addition:

1. Run `tests/client_ui.rs` repeatedly on Linux and macOS CI.
2. Perform an optional localhost SSH interruption smoke where available and
   record whether it ran.
3. Measure the largest serialized Snapshot produced by the resize/ANSI stress
   tests after Plans 002-005. Record the size in the completion entry.
4. If any legitimate supported test exceeds the current 8 MiB frame limit,
   mark this plan BLOCKED and create a separate Snapshot chunking plan. Do not
   raise the limit or add compression during this plan.
5. Confirm no isolated test daemon, bridge, SSH child or socket remains after
   the test suite.

**Verify**:

```bash
cargo fmt --check
cargo test --locked --all-targets
cargo test --locked --release --all-targets
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo check --locked --manifest-path fuzz/Cargo.toml
cargo build --locked --release
git diff --check
```

All exit 0.

## Test Plan

Required coverage:

- exact DSR, CPR, DA and size replies;
- split parser input and multiple queries;
- reply ordering through the pane input owner;
- child process waiting for replies without hanging;
- real client PTY tests on Linux and macOS;
- fake SSH/reconnect and process cleanup in CI;
- final frame-size measurement under rich ANSI resize stress.

## Done Criteria

- [x] Common terminal status, cursor, identity and size queries receive exact
  bounded replies.
- [x] Replies use the non-blocking pane input owner.
- [x] Unknown terminal operations remain bounded and intentionally unsupported.
- [x] Real client PTY tests run in the platform build matrix.
- [x] Format, strict Clippy and fuzz-target compilation are CI gates.
- [x] README and compatibility docs describe actual verified behavior.
- [x] Largest stress Snapshot remains within the current protocol limit, or the
  plan is marked BLOCKED with measured evidence.
- [x] All shared verification gates pass.
- [x] Task statuses, completion record and `plans/README.md` are updated.

## STOP Conditions

Stop and report if:

- A common query cannot be answered truthfully without changing TERM or adding
  a terminfo entry.
- Reply routing bypasses Plan 004's bounded pane input owner.
- macOS and Linux require materially different protocol replies.
- A legitimate supported Snapshot exceeds 8 MiB.
- CI requires privileged SSH service or access to external infrastructure.

## Maintenance Notes

Keep advertised capabilities aligned with implemented replies. Adding a new
terminal query is a protocol behavior change and needs an exact-byte unit test
plus a child-process integration test. CI should test the real client path, not
only daemon messages, because that was the source of repeated escaped defects.
