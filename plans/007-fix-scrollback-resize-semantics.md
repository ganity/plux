# Plan 007: Enforce Scrollback Limits And Preserve Content Across Resize

> **Executor instructions**: This plan changes the vendored terminal grid and
> has the highest data-loss risk. Add invariant tests first and run every
> vendored test after each algorithm step. Mark tasks only after verification
> and update Plan 007 in `plans/README.md` when complete.
>
> **Drift check (run first)**:
> `git diff --stat 83689a7..HEAD -- src/terminal.rs vendor/vt100/src/grid.rs vendor/vt100/src/row.rs vendor/vt100/src/screen.rs fuzz/fuzz_targets/terminal_bytes.rs`
> Stop if the vendor fork or terminal state representation changed materially.

## Status

- **Priority**: P1
- **Effort**: L
- **Risk**: HIGH
- **Depends on**: `plans/002-coalesce-terminal-resize.md`,
  `plans/005-render-pane-bounds-safely.md`
- **Category**: bug, perf
- **Planned at**: commit `83689a7`, 2026-07-25
- **Status**: DONE

## Why This Matters

The configured byte limit is calculated as eight bytes per terminal cell, but
the vendored `Cell` is exactly 32 bytes before row and allocation overhead. A
wide terminal can therefore retain several times the advertised memory. Resize
also truncates visible rows and columns directly, discarding text instead of
rewrapping logical lines. Since resize is a basic operation and scrollback is a
core product promise, both the memory ceiling and content preservation must be
defined and tested rather than approximated.

## Current State

`src/terminal.rs:13-20` estimates eight bytes per cell:

```rust
let estimated_bytes_per_line = usize::from(cols).max(1).saturating_mul(8);
let byte_limited_lines = (scrollback_bytes / estimated_bytes_per_line).max(1);
```

`vendor/vt100/src/cell.rs:17` asserts:

```rust
const _: () = assert!(std::mem::size_of::<Cell>() == 32);
```

`vendor/vt100/src/grid.rs:64-91` resizes every visible row and then resizes the
visible-row Vec:

```rust
for row in &mut self.rows {
    row.resize(size.cols, crate::Cell::new());
}
self.rows.resize(usize::from(size.rows), self.new_row());
```

This truncates columns and bottom rows, does not reflow wrapped lines, does not
resize history capacity after a width change and leaves historical rows at
their previous width.

Design requirements from `DESIGN.md:352-376` include long-line wrapping,
combining/wide characters and rewrapping after resize.

## Commands You Will Need

| Purpose | Command | Expected on success |
|---|---|---|
| Terminal tests | `cargo test --locked --lib terminal::tests -- --nocapture` | all pass |
| History integration | `cargo test --locked --test daemon_lifecycle scrollback -- --nocapture` | all matching tests pass |
| Fuzz build | `cargo check --locked --manifest-path fuzz/Cargo.toml` | exit 0 |
| Release tests | `cargo test --locked --release --all-targets` | all pass |

## Scope

**In scope**:

- `src/terminal.rs`
- `vendor/vt100/src/grid.rs`
- `vendor/vt100/src/row.rs`
- `vendor/vt100/src/screen.rs`
- root `terminal` tests that exercise the changed vendored behavior
- `tests/daemon_lifecycle.rs`
- `fuzz/fuzz_targets/terminal_bytes.rs`
- `plans/007-fix-scrollback-resize-semantics.md`
- `plans/README.md`

**Out of scope**:

- Alternate-screen scrollback capture.
- Persisting scrollback to disk.
- Search indexing or compression.
- Configurable East Asian ambiguous width.
- Replacing vt100 with another terminal emulator crate.

## Git Workflow

- Suggested branch: `hardening/007-scrollback-reflow`
- Suggested commit: `Preserve scrollback content across terminal resize`
- Do not push unless requested.

## Task Status

| Task | Description | Status |
|---:|---|---|
| 1 | Add memory-budget and resize data-loss regressions | DONE |
| 2 | Recalculate and trim history capacity by actual cell cost | DONE |
| 3 | Reflow primary-grid logical lines across width changes | DONE |
| 4 | Preserve cursor and scrolled-view anchors | DONE |
| 5 | Extend resize fuzzing and large-history integration tests | DONE |

## Required Invariants

The implementation must preserve these invariants:

1. Primary-grid scrollback plus visible rows represent the same logical text
   before and after shrink/grow, except content evicted by the configured limit.
2. A wide cell and its continuation are never split across physical rows.
3. The cursor maps to the same logical character position when possible and is
   otherwise clamped to a valid cell.
4. A user at live bottom remains at live bottom. A user scrolled into history
   remains anchored near the same logical content.
5. The alternate grid has no scrollback and may use simple bounded resize; it
   must still remain valid and panic-free.
6. The configured byte ceiling uses actual cell size plus conservative row
   overhead and is re-evaluated when width changes.

## Steps

### Step 1: Add Characterization And Failure Tests

Add root terminal tests for:

1. A wrapped ASCII logical line shrunk from 20 to 8 columns and grown back;
   `contents()` preserves text order.
2. Chinese wide characters and emoji near a wrap boundary are never split or
   replaced by invalid continuation cells.
3. Shrinking visible rows moves recoverable primary-grid content into history
   instead of deleting it.
4. A terminal scrolled to a known marker remains anchored at that marker after
   resize.
5. Default 64 MiB history at 120 and 1000 columns computes a capacity based on
   at least `size_of::<Cell>()`, not eight bytes.
6. Repeated narrow/wide resize does not grow history beyond the recalculated
   capacity.

The data-preservation and budget tests must fail on commit `83689a7`.

**Verify before implementation**: targeted tests fail with missing content or
an excessive calculated line capacity.

### Step 2: Make The History Budget Resize-Aware

Refactor `TerminalState`:

1. Store configured `scrollback_lines` and `scrollback_bytes` in the state.
2. Calculate per-line cost with
   `size_of::<vt100::Cell>() * max(cols, 1)` plus a conservative fixed row/Vec
   overhead. Use checked/saturating arithmetic.
3. Add an internal vendored Screen/Grid method to update `scrollback_len` and
   immediately evict the oldest rows above the new limit.
4. Recalculate the allowed line count on every width change before or as part
   of reflow.
5. Keep at least one history row only when the configured line and byte limits
   both allow it; do not claim a byte limit can hold a row when it cannot.
6. Expose a test-only history-capacity accessor so tests assert the calculated
   bound without estimating process RSS.

Do not hardcode 32 in Plux. Use `size_of` so vendor layout changes remain safe.

**Verify**: memory-budget tests pass at default, narrow and very wide sizes.

### Step 3: Reflow The Primary Grid By Logical Line

Implement reflow inside the vendored Grid, where rows, wrap flags, cursor and
scrollback are available together.

Algorithm:

1. Combine primary scrollback rows followed by visible primary rows in display
   order. Do not include the alternate grid.
2. Group physical rows into logical lines. A row with `wrapped=true` continues
   into the next physical row; an unwrapped row terminates the logical line.
3. Preserve cell objects and attributes. On the final physical row of a logical
   line, trim only trailing default cells that are beyond both the last
   meaningful cell and any mapped cursor/anchor position.
4. Rechunk each logical line to the new width. Never place a wide cell in the
   final column without its continuation; pad the current row and move the pair
   to the next row.
5. Mark every non-final physical row of a logical line as wrapped. Preserve an
   explicit empty logical line as one empty physical row.
6. Split the resulting physical rows into bounded scrollback and exactly the
   requested visible-row count. Pad missing visible rows with defaults and
   evict only the oldest rows above `scrollback_len`.
7. Re-establish scroll region and saved positions using the existing clamp
   rules after the row transformation.

Keep helper functions private to the vendored crate. Do not serialize rows to
plain text and parse ANSI back; that would lose attributes and wide-cell state.

**Verify**: `cargo test --locked --lib terminal::tests -- --nocapture` passes,
including ASCII, styled-cell, combining-character and wide-character reflow in
both shrink and grow directions.

### Step 4: Map Cursor And Scrollback Anchors

Before reflow, derive logical locations:

1. Map cursor row/column to `{logical_line_index, cell_offset}` within the
   combined primary rows.
2. If `scrollback_offset == 0`, record a live-bottom anchor.
3. Otherwise map the top visible physical row to a logical line and offset.
4. After reflow, map these locations back to physical rows/columns.
5. Keep live bottom at offset zero. For a history anchor, choose the closest
   physical row containing the same logical offset and clamp to available
   history if old content was evicted.
6. Apply the same mapping to saved cursor position where possible.

Add focused tests for cursor on a wide cell, cursor at end of a wrapped line,
live-bottom output and a scrolled history marker.

**Verify**: all anchor tests pass and no test uses private field mutation from
outside the vendored crate.

### Step 5: Extend Fuzz And Integration Coverage

Update `fuzz/fuzz_targets/terminal_bytes.rs` so input bytes drive both parser
output and bounded resize operations:

1. Derive rows in `2..=80` and columns in `1..=240` from deterministic input
   bytes.
2. Alternate processing chunks and resizing; never allocate based directly on
   an unbounded fuzz integer.
3. Exercise scroll-to-top/bottom around selected resize operations if public
   methods permit it.

Extend daemon lifecycle history coverage with a finite long output containing
markers before and after width changes. Verify all markers remain searchable or
visible until the configured capacity legitimately evicts them.

**Verify**:

```bash
cargo check --locked --manifest-path fuzz/Cargo.toml
cargo test --locked --lib terminal::tests -- --nocapture
cargo test --locked --test daemon_lifecycle scrollback -- --nocapture
cargo test --locked --release --all-targets
```

All pass.

## Test Plan

Required cases:

- actual byte-budget calculation and trimming;
- ASCII and styled logical-line reflow;
- Chinese, emoji and wide continuation boundaries;
- visible-row shrink preservation;
- cursor and saved-cursor mapping;
- live-bottom and scrolled-history anchors;
- repeated resize without memory-cap drift;
- fuzzed bytes interleaved with bounded resize.

## Done Criteria

- [x] Scrollback capacity uses actual `Cell` size and row overhead.
- [x] Capacity is recalculated and trimmed after width changes.
- [x] Primary-grid content reflows instead of being truncated.
- [x] Wide cells and attributes survive shrink/grow.
- [x] Cursor and viewport anchors remain valid.
- [x] Alternate-grid resize remains valid and history-free.
- [x] Terminal, integration, fuzz-build and release tests pass.
- [x] Task statuses and `plans/README.md` are updated.

## STOP Conditions

Stop and report if:

- Row internals do not expose enough information to preserve wide-cell and
  attribute invariants without a broad public API change.
- Reflow requires changing terminal semantics unrelated to resize.
- Cursor or viewport mapping cannot be made deterministic from existing grid
  state.
- A supported test case requires retaining more rows than the configured byte
  ceiling permits. Report the exact dimensions and calculated cost.

## Maintenance Notes

This vendor fork becomes part of Plux's persistence semantics. Reviewers should
scrutinize data preservation and anchor mapping more than micro-optimizations.
Any future change to `Cell` size, Row storage or ambiguous-width policy must
rerun the budget and reflow tests.
