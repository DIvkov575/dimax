# Pane resize wiring + mouse-wheel scrollback — design

## Summary

Two related fixes to how a client-pane's on-screen size and viewport
relate to its bound server-pane's PTY:

1. **Resize bug fix.** `Request::ResizeClientPane` — and the daemon's
   full "smallest-viewer-wins" sizing logic behind it — already exist
   and are already tested, but the TUI frontend never sends the
   request. Every PTY is permanently pinned at the hardcoded
   `DEFAULT_PTY_SIZE` (24×80): a pane rendered taller or wider than that
   shows the grid's content only in the top-left 24×80 corner of its
   actual on-screen area, with the rest of the pane blank — the "bash
   renders in the top half" symptom. The fix reports each leaf's actual
   on-screen size to the daemon every time it changes.
2. **Mouse-wheel scrollback.** Adds the ability to scroll back through a
   pane's PTY output history via the mouse wheel — currently recognized
   by `mouse::parse` (`Cb=64`/`65`, "scroll up"/"scroll down" in SGR
   encoding) but always discarded (`ParsedInput::Ignored`).

## Non-goals

- No keybind-driven scrolling (mouse wheel only, per explicit
  direction) — a keybind can be added later without protocol changes if
  wanted.
- No "jump to bottom" affordance beyond scrolling all the way down
  yourself (clamped at offset 0) — no dedicated keybind/click target for
  this in v1.
- No SIGWINCH/terminal-resize-signal handling — resize reporting piggybacks
  on the render loop's existing per-frame cadence (see "Resize" below for
  why this is sufficient without a signal handler).
- No change to `dimux server ls`/CLI surface — both fixes are
  TUI-attach/daemon-protocol only.
- No scrollback-position persistence across workspace switches or
  detach/reattach — switching away from a pane and back resets its
  effective view to whatever offset the daemon still has recorded for
  that client-pane (server-side state, see "Scroll offset ownership"),
  which is not reset by a mere frontend redraw, but no persistence
  guarantee is being *added* here beyond that; this spec does not change
  existing detach/reattach/close semantics.

## Part 1: Resize wiring

### Root cause (confirmed)

- `src/protocol.rs`: `Request::ResizeClientPane { pane, size }` exists.
- `src/daemon/mod.rs`: the `ResizeClientPane` dispatch arm exists, calls
  `state.resize_client_pane(pane, size)`, then broadcasts a `GridDelta`
  for the affected server-pane.
- `src/daemon/state.rs`: `resize_client_pane` records the size in
  `client_pane_sizes: HashMap<ClientPaneId, Size>` and recomputes
  "smallest on-screen size across every viewer" (`viewed_size`) to apply
  to the bound server-pane's PTY (`apply_pty_size`) — this is the
  existing, tested "smallest-viewer-wins" logic (design doc "PTY
  sizing").
- **The gap:** `src/tui/mod.rs` never constructs or sends a
  `Request::ResizeClientPane`. `client_pane_sizes` never gets an entry
  for any pane opened via `dimux attach`, so `viewed_size` always
  returns `None`, and every server-pane's PTY stays at whatever it was
  created with (`term::ServerPane::spawn`'s caller passes
  `DEFAULT_PTY_SIZE`, per `daemon/state.rs`'s `server_spawn`) —
  permanently 24×80 regardless of the pane's real rendered size.

### Fix

**New pure helper, `render::leaf_rects`** (mirrors the existing
`divider_rects` pattern exactly — same recursive walk over `SplitTree`,
same `Layout`/`Constraint` math already used by `draw_tree`, just
collecting each `Leaf`'s final `Rect` instead of computing divider grab
zones):

```rust
pub fn leaf_rects(tree: &SplitTree, area: Rect) -> Vec<(ClientPaneId, Rect)>
```

**`App` gains one field**, `pane_sizes: HashMap<ClientPaneId, Size>`
(mirrors the daemon's own `client_pane_sizes` map, one-directionally —
this is the frontend's memory of what it last *told* the daemon, so it
only sends a request when something actually changed).

**After every `terminal.draw()` call in `run`'s loop** (right after the
existing `app.frame_area = frame.area()` assignment, still inside the
draw closure or immediately after it — outside is simpler since sending
a request is async and the draw closure itself is sync), walk
`leaf_rects(tree, app.frame_area)` for the current workspace's tree.
For each `(pane_id, rect)`: compute `Size { rows: rect.height, cols:
rect.width }` (accounting for the 1-row title-bar border every leaf
already reserves — subtract 1 from `rect.height` for the usable grid
area, matching what `draw_leaf` actually gives the PTY's rendered
`Paragraph`), compare against `pane_sizes.get(&pane_id)`, and if
different (including "not present yet" — a pane's first frame), send
`Request::ResizeClientPane { pane: pane_id, size }` and update
`pane_sizes`.

This deliberately piggybacks on the render loop's existing cadence
rather than adding a `SIGWINCH` handler or any new event source:
- **Initial size**: reported on the very first `terminal.draw()` after
  `App::bootstrap` or any `switch_workspace` — both already run before
  the loop's first iteration / immediately trigger a redraw next
  iteration, so a freshly-visible pane always gets its size reported
  before the user could plausibly type into it.
- **Terminal window resize**: `ratatui`'s `CrosstermBackend` recomputes
  `frame.area()` on every `draw()` call by construction (it queries the
  real terminal size each time), so a real terminal resize changes
  `frame.area()`, which changes every leaf's `Rect` on the very next
  frame — no signal needed, the render loop is already running
  continuously (`tokio::select!` between stdin and socket reads, both of
  which fire constantly enough in an interactive session — worst case, a
  resize with zero subsequent input/network activity wouldn't redraw
  until the next keystroke, an acceptable v1 tradeoff given the
  alternative is adding real signal-handling infrastructure this crate
  doesn't have yet).
- **Split/close changes**: any action that adds/removes/resizes a leaf
  already triggers a redraw (either locally via the returned
  `LayoutDelta`/`GridDelta`, or via the next loop iteration), so this
  needs no special-casing either.

### Testing

- Unit test `leaf_rects` directly (pure function, no daemon needed):
  single leaf returns exactly `frame.area()`; two leaves side-by-side
  split the width correctly (accounting for the 1-column divider);
  two leaves stacked split the height correctly.
- Unit test the size-diffing logic (extract into a small pure helper,
  e.g. `fn changed_pane_sizes(current: &HashMap<...>, tree: &SplitTree,
  area: Rect) -> Vec<(ClientPaneId, Size)>`, so it's testable without a
  live `App`/socket) — confirms an unchanged pane produces no entries,
  a newly-seen pane produces one, a genuinely resized pane produces one
  with the new size.
- Integration test (daemon `TestConn` pattern, matching
  `resize_split_updates_ratio_visible_to_subscribers`'s style): send
  `Request::ResizeClientPane` directly, confirm a subsequent `Subscribe`
  snapshot's grid reflects the new size.

## Part 2: Mouse-wheel scrollback

### Protocol changes

**`GridSnapshot` gains one field:**

```rust
pub struct GridSnapshot {
    pub server_pane: ServerPaneId,
    pub size: Size,
    pub cursor: (u16, u16),
    pub lines: Vec<Vec<Cell>>,
    /// How many rows back from the live tail this snapshot's `lines`
    /// starts at (0 = the live/current viewport, matching every
    /// existing snapshot before this field existed). Lets a frontend
    /// distinguish "this pane is showing history" from "this pane is
    /// live" without needing to separately track offset state itself.
    pub scroll_offset: usize,
}
```

**New request:**

```rust
Request::ScrollClientPane {
    pane: ClientPaneId,
    /// Positive scrolls back into history (more negative = further
    /// back is NOT the sign convention here -- positive delta always
    /// means "show older content", matching one discrete wheel-tick per
    /// call); negative scrolls toward the live tail. The daemon clamps
    /// the resulting offset to `0 ..= scrollback_rows()` server-side --
    /// the frontend never computes or owns the authoritative offset.
    /// (`scrollback_rows()` here means "rows of actual history," see
    /// the `ServerPane::scrollback_rows` doc comment below for why this
    /// is NOT the same number `wezterm_term::Screen::scrollback_rows()`
    /// itself returns.)
    delta: i32,
},
```

Response: `Response::Ack` (same convention as `Request::Input` and every
other fire-and-forget mutation in this protocol — the resulting
`GridDelta` push is what actually delivers the new content, not the
response itself, matching how `ResizeClientPane` already works).

### Scroll offset ownership

**Server-side, per client-pane** (not per server-pane — the same
server-pane can be bound into multiple client-panes across workspaces,
per the design doc's "a single server-pane can be bound into many
client-panes at once" — each viewer scrolls independently). `State`
gains `client_pane_scroll: HashMap<ClientPaneId, usize>`, directly
alongside the existing `client_pane_sizes: HashMap<ClientPaneId, Size>`
field it's modeled on. Absence in the map means offset 0 (live), same
"absent = default" convention `client_pane_sizes` already uses.

**Clamping and clearing:**
- `State::scroll_client_pane(pane, delta)`: resolves the client-pane's
  bound server-pane, reads that server-pane's current
  `scrollback_rows()` (new small accessor on `ServerPane`, see below),
  computes `new_offset = (current_offset + delta).clamp(0,
  scrollback_len)`, stores it (or removes the map entry if the result is
  0, keeping the map's "absent = 0" invariant tidy rather than
  accumulating dead zero-entries), and returns the affected
  `ServerPaneId` so the caller can broadcast a fresh `GridDelta` — same
  shape as every other mutating dispatch arm in `daemon/mod.rs`.
- **Rebinding/closing a client-pane** (`client_unbind`, `client_close`,
  `server_kill`'s unbind-on-kill path): each of these should also remove
  that client-pane's `client_pane_scroll` entry, the same way
  `client_pane_sizes` conceptually goes stale for a pane that no longer
  points at anything meaningful — verify at implementation time whether
  `client_pane_sizes` itself is actually cleared on unbind today (if it
  isn't, match whatever the existing convention is exactly, rather than
  inventing a new cleanup discipline just for the new map — consistency
  with the established pattern matters more here than which specific
  behavior is "more correct").
- **No explicit "jump to bottom" action** exists yet (non-goal) — the
  only way back to offset 0 is scrolling down far enough that the clamp
  reaches it.

### `ServerPane`/`term` changes

`ServerPane` gains:

```rust
/// Rows of actual history available to scroll back into (excludes the
/// live on-screen rows) -- the upper bound `State::scroll_client_pane`
/// clamps against. NOT the same number as
/// `wezterm_term::Screen::scrollback_rows()`, despite the matching
/// name and this being a thin wrapper around it: that method returns
/// the *total* buffered line count (visible rows + history -- a fresh
/// pane with zero history still reports `physical_rows`, not 0), so
/// `physical_rows` must be subtracted to get the actual scrollable
/// depth. Verified against the vendored `tattoy-wezterm-term` source
/// (`Screen::new` seeds `lines` with exactly `physical_rows` entries
/// before any output arrives; `scroll_up` only ever grows `lines`
/// beyond that count, never below it).
pub fn scrollback_rows(&self) -> usize {
    let guard = self.inner.lock().unwrap();
    let screen = guard.terminal.screen();
    screen.scrollback_rows().saturating_sub(screen.physical_rows)
}
```

`ServerPane::snapshot` gains an `offset: usize` parameter (0 preserves
today's exact behavior — every existing call site not yet updated for
scrolling passes `0`):

```rust
pub fn snapshot(&self, offset: usize) -> GridSnapshot {
    let guard = self.inner.lock().unwrap();
    let screen = guard.terminal.screen();
    let rows = screen.physical_rows;
    let cols = screen.physical_cols;
    // Shift the window `offset` rows back from the live tail --
    // `scrollback_or_visible_range` (already provided by
    // `wezterm_term::Screen`) accepts a signed row range that can dip
    // into the scrollback buffer directly, so no new row-indexing math
    // is needed here beyond picking the right start/end.
    let start = -(offset as i32);
    let end = start + rows as i32;
    let phys_range = screen.scrollback_or_visible_range(&(start..end));
    let lines = screen.lines_in_phys_range(phys_range);

    let mut grid_lines: Vec<Vec<Cell>> = lines.iter().map(|l| line_to_cells(l, cols)).collect();
    while grid_lines.len() < rows {
        grid_lines.push(vec![blank_cell(); cols]);
    }

    let cursor = guard.terminal.cursor_pos();
    GridSnapshot {
        server_pane: self.id,
        size: guard.size,
        cursor: (
            cursor.x.min(u16::MAX as usize) as u16,
            cursor.y.max(0).min(u16::MAX as i64) as u16,
        ),
        lines: grid_lines,
        scroll_offset: offset,
    }
}
```

Every existing caller of `ServerPane::snapshot()` (the `Subscribe`
dispatch arm, `broadcast_grid_prepare`) needs updating to pass the
correct offset — `broadcast_grid_prepare` in particular needs to know
*which* offset to snapshot at when a `Changed`/`Died` event fires for a
server-pane that has one or more scrolled-back viewers: it must
broadcast a *per-subscriber* snapshot at each subscriber's own client-pane's
offset, not one shared snapshot — this is the one place the existing
"snapshot once, push to everyone" broadcast shape needs to become
"snapshot once per distinct offset among current subscribers, push each
group its own copy" (in practice, almost always 1-2 distinct offsets:
the common case of everyone live, plus however many viewers are
actively scrolled back at that moment).

### Frontend changes

**`mouse::MouseEvent` gains two variants:**

```rust
pub enum MouseEvent {
    Down { col: u16, row: u16 },
    Drag { col: u16, row: u16 },
    Up { col: u16, row: u16 },
    ScrollUp { col: u16, row: u16 },
    ScrollDown { col: u16, row: u16 },
}
```

`mouse::parse`'s button-number check currently treats any non-zero
button as `ParsedInput::Ignored` unconditionally. Scroll wheel encodes as
button numbers 4/5 (`Cb=64`/`65` before the low-bits decode, matching the
module's existing `scroll_events_are_recognized_and_ignored` test
vectors) — add a check for these two specific decoded button numbers
*before* the catch-all `Ignored` fallback, resolving to
`ParsedInput::Mouse(MouseEvent::ScrollUp/ScrollDown { col, row })`
instead. Every other non-zero button number (middle/right/bare-movement)
keeps resolving to `Ignored`, unchanged.

**`App::handle_mouse`** gains two new match arms for
`ScrollUp`/`ScrollDown`. Each hit-tests `(col, row)` against
`render::leaf_rects(tree, self.frame_area)` (the same helper Part 1
adds) to find which client-pane the wheel event landed over — **not**
necessarily the focused pane, matching ordinary terminal-multiplexer
feel (scroll whatever's under the cursor). If the hit lands on a bound
leaf, send `Request::ScrollClientPane { pane, delta: 3 }` for
`ScrollUp` / `delta: -3` for `ScrollDown` (3 rows per wheel tick — an
arbitrary but conventional-feeling default; tune later if it feels
wrong in practice, no protocol implications either way since `delta` is
already an arbitrary signed int). A wheel event over empty space
(divider, unbound pane, no pane at all) is a no-op.

**`App::apply_event`'s `GridDelta` handling** needs no change — it
already just does `self.grids.insert(snapshot.server_pane, snapshot)`
unconditionally; the new `scroll_offset` field rides along for free.

**Rendering** (`render::draw_leaf`/`grid_to_text`): no change needed to
draw the grid itself (it's still just rows of cells) — but per the
design's "acknowledged simplification" precedent (attach-menu rename's
missing cursor caret), there is currently no visual indicator that a
pane is scrolled back at all. Add one: `draw_leaf`'s title-bar
`Block::title` gets a `" [scrollback]"` suffix when the bound
server-pane's current `GridSnapshot.scroll_offset > 0`, so a scrolled
pane is visually distinguishable from a live one without needing a
scrollbar widget (out of scope — `ratatui`'s `Scrollbar` widget could be
added later as a follow-up, not required for the core feature to work).

### Testing

- Unit tests for `mouse::parse`'s two new scroll-wheel-to-`MouseEvent`
  mappings (replacing/extending the existing
  `scroll_events_are_recognized_and_ignored` test, which currently
  asserts `Ignored` for these exact byte sequences — that assertion
  necessarily changes).
- Unit tests for `State::scroll_client_pane`'s clamping (clamps at 0,
  clamps at `scrollback_rows()`, absent-entry defaults to 0,
  zero-result removes the map entry rather than leaving a stale `0`).
- Unit test for `ServerPane::snapshot(offset)` at a non-zero offset
  against a pane with known scrollback content (spawn, write enough
  lines to scroll some off the top, snapshot at an offset, confirm the
  returned lines match the expected historical content, not the live
  tail).
- Integration test (daemon `TestConn` pattern): two connections
  subscribed to the same workspace/server-pane, one issues
  `ScrollClientPane`, confirm only that connection's own subsequent
  `GridDelta`s reflect the scrolled offset while the other's stay live —
  this is the test that actually exercises the per-subscriber-offset
  broadcast-splitting behavior called out above, which is the trickiest
  part of this whole spec to get right.

## Open questions / risks

- **Broadcast-splitting complexity** (per-subscriber offset grouping in
  `broadcast_grid_prepare`) is the single riskiest piece of this spec —
  everything else is close to mechanical. Flag this at planning time as
  the task that most needs careful review, not a "should be
  straightforward" task.
- **Wheel-tick delta of 3** is a guess, not derived from anything — call
  it out as a tunable constant with an obvious name
  (`SCROLL_ROWS_PER_TICK` or similar) so it's trivial to adjust without
  hunting for a magic number later.
