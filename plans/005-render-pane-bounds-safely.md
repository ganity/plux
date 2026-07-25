# Plan 005: Render Pane Deltas Without Erasing Adjacent Panes

> **Executor instructions**: Treat terminal escape semantics as correctness,
> not appearance. Add parser-backed regressions first. Mark each task after its
> verification succeeds and update Plan 005 in `plans/README.md` when complete.
>
> **Drift check (run first)**:
> `git diff --stat 83689a7..HEAD -- src/session.rs src/layout.rs src/client.rs vendor/vt100/src/row.rs vendor/vt100/src/term.rs`
> Stop if Session rendering has already changed from pane-row deltas to a
> physical-screen compositor.

## Status

- **Priority**: P0
- **Effort**: M
- **Risk**: MED
- **Depends on**: `plans/001-real-client-pty-harness.md`,
  `plans/003-deliver-snapshots-in-order.md`
- **Category**: bug
- **Planned at**: commit `83689a7`, 2026-07-25
- **Status**: DONE

## Why This Matters

Plux renders a pane-local row into the outer terminal and then clears to the
outer physical line end. In a vertical split, changing the left pane can erase
an unchanged right pane, which is then omitted from the delta. Row formatting
also assumes default attributes while Session does not reset attributes before
each pane segment, allowing colors or inverse video to bleed across panes and
borders. This plan keeps the existing small row-diff design but makes every
operation bounded to its pane.

## Current State

`src/session.rs:353-385` renders each pane independently:

```rust
write!(output, "\x1b[{};{}H", rect.y + row as u16 + 1, rect.x + 1)?;
output.write_all(contents)?;
output.write_all(b"\x1b[K")?;
```

`vendor/vt100/src/row.rs:244-254` may already include another erase-to-line-end:

```rust
crate::term::ClearRowForward.write_buf(contents);
```

`rows_formatted` documents that the caller positions each row, but it starts
attribute tracking from default. Session resets attributes only once at the end
of the whole frame (`src/session.rs:443`).

`src/layout.rs:160-190` forces split availability to at least two cells even
when the outer terminal is smaller, so pane rectangles plus a border can exceed
the real viewport.

## Commands You Will Need

| Purpose | Command | Expected on success |
|---|---|---|
| Session renderer | `cargo test --locked --lib session::tests -- --nocapture` | all pass |
| Real client split | `cargo test --locked --test client_ui split -- --nocapture` | all matching tests pass |
| Full validation | `cargo test --locked --all-targets` | all pass |

## Scope

**In scope**:

- `src/session.rs`
- `src/layout.rs`
- `vendor/vt100/src/row.rs`
- `vendor/vt100/src/term.rs` only if a bounded erase helper is required
- `tests/client_ui.rs`
- root `session`/`terminal` tests that exercise the changed vendored behavior
- `plans/005-render-pane-bounds-safely.md`
- `plans/README.md`

**Out of scope**:

- Snapshot delivery and queueing.
- A new retained-mode renderer or graphics library.
- Pane titles, status bars or decorative borders.
- Full terminal reflow; Plan 007 owns it.
- Selection UX changes except regressions proving it still renders correctly.

## Git Workflow

- Suggested branch: `hardening/005-pane-render-bounds`
- Suggested commit: `Keep pane delta rendering inside pane bounds`
- Do not push unless requested.

## Task Status

| Task | Description | Status |
|---:|---|---|
| 1 | Reproduce adjacent-pane erasure and attribute bleed | DONE |
| 2 | Make row erasure width-bounded | DONE |
| 3 | Reset rendering state around every pane segment and border | DONE |
| 4 | Handle terminals too small for the saved split layout | DONE |
| 5 | Validate split output and selection in a real client | DONE |

## Steps

### Step 1: Add Parser-Backed Renderer Regressions

Extend `src/session.rs` tests. Use a separate outer `vt100::Parser` as the
terminal receiving Session frames:

1. Create a vertical split and put distinct text in left and right panes.
2. Apply the initial full Session render to the outer parser.
3. Change only the left pane and apply the next delta.
4. Assert the right pane's visible cells remain unchanged.
5. Add a colored/inverse left row followed by default right-pane text and
   assert right-pane cell attributes remain default.
6. Add a border assertion so style does not bleed into `|` or `-`.

Also add a test for a split Session resized below the dimensions needed for two
panes and a border. The render must stay within the declared rows and columns
and must not panic.

**Verify before implementation**: adjacent-pane and attribute tests fail on
commit `83689a7` for the expected cell differences.

### Step 2: Bound Erase Operations In Vendored vt100

The existing vendor fork is the shared narrow point. Update row subset
formatting so trailing blank cells are erased only across the requested width:

1. In both formatted-row and row-diff paths, replace `ClearRowForward` with an
   erase operation whose count is `start + width - current_column`.
2. Reuse the existing `EraseChar` CSI operation where possible; do not parse or
   string-replace ANSI after formatting.
3. Preserve wide-cell handling and wrapped-row behavior.
4. For full-width callers, the bounded erase must be visually equivalent to
   clearing the remaining row.
5. Add root crate tests through Session or TerminalState for a subset beginning
   at a nonzero outer column and for a full-width row. Do not run the vendored
   crate as a separate Cargo project because that creates a nested lockfile.

**Verify**: `cargo test --locked --lib session::tests -- --nocapture` -> all
tests pass, including new bounded-erase cases.

### Step 3: Reset Attributes At Pane Boundaries

In `Session::render`:

1. Emit `SGR 0` after positioning and before each pane row's formatted bytes,
   because `rows_formatted` assumes default starting attributes.
2. Emit `SGR 0` after the pane segment so later borders, labels and panes start
   from a known state.
3. Remove the unconditional Session-level `ESC[K`; the row formatter now owns
   width-bounded trailing erase.
4. Reset attributes before drawing borders and unread-output labels.
5. At the end, preserve existing cursor visibility, mouse mode and final reset
   behavior.
6. Remove cached `rendered_rows` entries for panes no longer present so repeated
   split/close does not retain stale buffers.

Do not force a full render on every update. Keep row-level delta behavior.

**Verify**: all parser-backed Session tests pass and an unchanged second render
remains a delta rather than a full frame.

### Step 4: Define Tiny-Terminal Layout Behavior

When a saved split cannot fit at least one cell per child plus its border:

1. Preserve the logical layout; do not close panes or change split ratios.
2. Render only the focused pane across the available viewport, equivalent to a
   temporary effective zoom.
3. Apply the same effective rectangles to PTY resize so hidden panes are not
   assigned rectangles outside the viewport. Hidden panes may retain their
   last valid PTY size until the layout fits again.
4. Automatically restore the split when the terminal grows enough.
5. Keep explicit user zoom state separate from this temporary fallback.

Add a small `LayoutNode` fit/minimum-size helper only if needed by both layout
and Session. Do not build a general constraint solver.

**Verify**: tests cover `1x2`, `2x2`, nested split shrink and restoration after
growth without panic or out-of-bounds cursor movement.

### Step 5: Validate Through The Real Client

Add `real_client_preserves_right_pane_during_left_output`:

1. Start a real client, create a vertical split and place a stable marker in
   the right pane.
2. Produce repeated colored output only in the left pane.
3. Capture outer PTY output through multiple deltas and assert the stable marker
   remains visible in the outer parser.
4. Enter and leave selection mode to prove local highlight restoration still
   redraws the full physical row correctly.
5. Shrink below split minimum, grow again and assert both pane markers return.

**Verify**: `cargo test --locked --test client_ui split -- --nocapture` -> pass.

## Test Plan

Required cases:

- changed left pane preserves unchanged right pane;
- attributes and inverse video do not bleed across panes/borders;
- blank/trailing cells erase only within a pane;
- pane close removes stale cache;
- tiny terminal temporarily shows focused pane and restores layout;
- real client split and selection remain correct.

## Done Criteria

- [x] No pane-local row emits an erase beyond its width.
- [x] Session resets terminal attributes at every pane/border boundary.
- [x] A left-pane-only delta preserves right-pane cells and attributes.
- [x] Tiny terminal dimensions never produce rectangles outside the viewport.
- [x] Saved splits return after the terminal grows.
- [x] Session, terminal and real-client tests pass.
- [x] All shared verification gates pass.
- [x] Task statuses and `plans/README.md` are updated.

## STOP Conditions

Stop and report if:

- Bounded erase cannot be implemented without changing unrelated vt100 public
  output semantics.
- Wide-character continuation cells are corrupted by the bounded erase.
- Tiny-terminal handling would require destroying or persisting a different
  layout.
- Correctness requires a full-screen compositor rather than the scoped row
  formatter change. Record the failing parser state before redesigning.

## Maintenance Notes

Review terminal effects by feeding frames into a parser, not by checking that
the output String contains a marker. ANSI can contain the marker while still
erasing it later. Any future pane decoration must obey the same width and
attribute boundaries.
