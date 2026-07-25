# Plan 001: Establish A Real Client PTY Regression Harness

> **Executor instructions**: Follow this plan in order. Update each task in the
> task-status table immediately after its verification succeeds. Update Plan
> 001 in `plans/README.md` when all done criteria pass. Do not implement any
> product fix in this plan.
>
> **Drift check (run first)**:
> `git diff --stat 83689a7..HEAD -- tests src/client.rs src/transport.rs Cargo.toml`
> If the real client loop or integration-test setup changed, compare the live
> code with the excerpts below and stop on a material mismatch.

## Status

- **Priority**: P0
- **Effort**: M
- **Risk**: LOW
- **Depends on**: none
- **Category**: tests
- **Planned at**: commit `83689a7`, 2026-07-25

## Why This Matters

The existing integration suite connects directly to the daemon protocol. It
does not execute raw mode, the local terminal renderer, client event queues,
SIGWINCH handling or SSH-style stdout backpressure. That is why all 96 tests
can pass while real terminal resize, selection and rendering still fail. This
plan creates one reusable pseudo-terminal harness and only passing baseline
tests; later plans add their own failing regression to this harness.

## Current State

- `tests/daemon_lifecycle.rs:20-120` starts an isolated daemon and communicates
  through `UnixStream`, bypassing `client::enter`.
- `src/client.rs:119-380` contains the real attach loop, `TerminalGuard`, stdin
  reader, server reader, renderer and reconnect state.
- `Cargo.toml:12` already includes `portable-pty = "0.9"`; no new dependency is
  required for an integration-test PTY.
- There is no test reference to `client::enter` or `TerminalGuard::enter`.

Existing test convention to match:

```rust
// tests/daemon_lifecycle.rs:80
Command::new(env!("CARGO_BIN_EXE_plux"))
    .args(args)
    .env("XDG_RUNTIME_DIR", &self.runtime)
    .env("XDG_CONFIG_HOME", &self.config)
    .env("USER", &self.user)
```

Use isolated runtime/config directories and the compiled binary exactly as the
current integration tests do. Do not invoke the user's installed `plux`.

## Commands You Will Need

| Purpose | Command | Expected on success |
|---|---|---|
| Targeted test | `cargo test --locked --test client_ui -- --nocapture` | all `client_ui` tests pass |
| Full test | `cargo test --locked --all-targets` | all tests pass |
| Lint | `cargo clippy --locked --all-targets --all-features -- -D warnings` | exit 0 |

## Scope

**In scope**:

- `tests/client_ui.rs` (create)
- `tests/support/mod.rs` (create only if two integration test files need the
  same helper during this plan)
- `plans/001-real-client-pty-harness.md`
- `plans/README.md`

**Out of scope**:

- Every file under `src/`
- Existing behavior or timeout changes
- SSH network tests
- New crates or dev-dependencies

## Git Workflow

- Suggested branch: `hardening/001-client-pty-harness`
- Commit message style: imperative sentence, for example
  `Add real client PTY regression harness`.
- Do not push unless the operator explicitly requests it.

## Task Status

| Task | Description | Status |
|---:|---|---|
| 1 | Build an isolated daemon and PTY client harness | DONE |
| 2 | Add baseline attach, input, render and detach coverage | DONE |
| 3 | Add deterministic resize and process-liveness helpers | DONE |
| 4 | Run full validation and document harness usage | DONE |

## Steps

### Step 1: Build The Isolated Harness

Create `tests/client_ui.rs`. Add a small `ClientHarness` that:

1. Creates a unique temporary root without external crates, following the
   process-id/timestamp pattern already used by `TestDaemon`.
2. Sets `XDG_RUNTIME_DIR`, `XDG_CONFIG_HOME`, `USER` and a deterministic
   `SHELL=/bin/sh` for every Plux process.
3. Opens a PTY with `portable_pty::native_pty_system()` and starts
   `env!("CARGO_BIN_EXE_plux") work` as the PTY child.
4. Keeps the PTY master, reader, writer and child handle so a test can write
   keystrokes, read rendered bytes, resize the outer terminal and check whether
   the client is still running.
5. On `Drop`, terminates the client if needed, runs the isolated `plux stop`,
   waits for children and removes only the harness temporary root.

All reads must use bounded timeouts or a reader thread plus channel. No test may
wait forever for terminal output.

**Verify**: `cargo test --locked --test client_ui --no-run` -> exit 0.

### Step 2: Cover The Baseline Client Path

Add a test named `real_client_attaches_renders_input_and_detaches`:

1. Wait for the shell prompt or another deterministic readiness marker.
2. Write `printf 'PLUX_CLIENT_READY\n'` followed by carriage return.
3. Read until the rendered PTY output contains `PLUX_CLIENT_READY`.
4. Send the default detach sequence `Ctrl-A d` (`0x01`, `d`).
5. Assert the client exits successfully and a separate isolated `plux list`
   still lists `work`.

The assertion is on observable terminal bytes and process state, not sleeps
alone.

**Verify**: `cargo test --locked --test client_ui real_client_attaches_renders_input_and_detaches -- --nocapture` -> pass.

### Step 3: Add Reusable Resize And Backpressure Controls

Add harness methods, with no product assertions yet:

- `resize(rows, cols)` calls `MasterPty::resize`.
- `wait_for_output(marker, timeout)` accumulates bytes up to a bounded maximum.
- `pause_output()` and `resume_output()` let later plans intentionally stop
  consuming the outer PTY without leaking a reader thread.
- `client_is_running()` uses non-blocking child status inspection.
- `run_cli(args)` invokes the isolated binary and environment.

Add a passing test named `real_client_survives_one_resize` that resizes once,
runs `stty size`, observes the final dimensions and then detaches. Use a relaxed
deadline suitable for Linux and macOS CI; do not assert an exact repaint count.

**Verify**: `cargo test --locked --test client_ui real_client_survives_one_resize -- --nocapture` -> pass on the current implementation.

### Step 4: Validate And Record

Run the shared gates. Add a short module comment in `tests/client_ui.rs` stating
that later plans must put real terminal resize/render/reconnect regressions here
instead of adding protocol-only substitutes.

**Verify**:

```bash
cargo fmt --check
cargo test --locked --all-targets
cargo clippy --locked --all-targets --all-features -- -D warnings
```

All commands exit 0.

## Test Plan

- New integration file: `tests/client_ui.rs`.
- Required tests:
  - `real_client_attaches_renders_input_and_detaches`
  - `real_client_survives_one_resize`
- Structural reference: `TestDaemon` in `tests/daemon_lifecycle.rs`.
- No test in this plan should reproduce the known five-second slow-output
  failure; that red test belongs to Plan 003.

## Done Criteria

- [x] The real compiled client runs under a PTY in an automated test.
- [x] Input, rendered output, detach and session survival are asserted.
- [x] A PTY resize is exercised without modifying production code.
- [x] Every test read and child-status assertion has a deadline.
- [x] `cargo test --locked --test client_ui -- --nocapture` passes.
- [x] Shared debug tests and strict Clippy pass.
- [x] No source file or dependency manifest is modified.
- [x] Task statuses and `plans/README.md` are updated.

## STOP Conditions

Stop and report instead of improvising if:

- `portable-pty` cannot start the Plux client with a controllable outer PTY on
  either Linux or macOS.
- The harness requires a new dependency.
- The current client cannot pass the basic attach/input/detach test without a
  product fix. Record the failure output; do not fix it in this plan.
- Cleanup would require killing non-isolated user processes.

## Maintenance Notes

Keep the harness intentionally small. It is not a terminal emulator and should
only provide process, PTY, byte capture, resize and cleanup controls. Reviewers
should reject arbitrary sleep-based assertions and any use of the user's real
runtime directory.
