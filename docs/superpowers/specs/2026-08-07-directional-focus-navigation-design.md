# Directional Focus Navigation — Design

## Goal

Replace `cmd-h/j/k/l`'s current "previous/next leaf in tree order" behavior (an intentional v1 stand-in, per this module's own doc comment) with real geometric direction: `cmd-h` moves focus to whichever leaf is actually positioned to the left on screen, `cmd-l` right, `cmd-k` up, `cmd-j` down. No leaf in that direction is a no-op (no wraparound). Add click-to-focus as part of the same change, since it needs the identical rect-hit-testing this fix already requires.

## Why this is now feasible

`SplitTree` itself carries no rect information (only `dir`/`ratio` per split), which is why `cycle_focus` fell back to tree-order cycling. But `render::leaf_rects(tree, area)` already computes every leaf's actual on-screen `Rect` — built for mouse hit-testing (`scroll_pane_under`, divider drag) and already called every frame for resize-reporting. `App` already carries `self.frame_area`, refreshed every frame. Nothing new needs to be computed or plumbed through; this is a pure client-side reshuffling of data that already exists.

## Adjacency Algorithm

New function, replacing `cycle_focus`:

```rust
fn nearest_leaf_in_direction(
    tree: &SplitTree,
    area: Rect,
    current: Option<ClientPaneId>,
    direction: Direction,
) -> Option<ClientPaneId>
```

where `Direction` is a new small enum (`Left`, `Right`, `Up`, `Down`) — not reusing `SplitDir` or `ratatui::layout::Direction`, both of which mean something else in this codebase already.

1. Compute `render::leaf_rects(tree, area)`.
2. If `current` is `None` or not found among the current leaves (e.g. it was just closed elsewhere — same edge case `cycle_focus` handled by falling back to the first leaf), return the first leaf in `leaf_rects`' own order. This matches today's existing fallback behavior for a stale/absent focus, which is orthogonal to directional movement itself.
3. Otherwise, find the focused leaf's own rect, then filter every *other* leaf's rect to only those positioned in the requested direction:
   - `Left`: `other.x + other.width <= focused.x`
   - `Right`: `other.x >= focused.x + focused.width`
   - `Up`: `other.y + other.height <= focused.y`
   - `Down`: `other.y >= focused.y + focused.height`
4. Among the filtered candidates, pick the nearest one: smallest gap along the movement axis (`focused.x - (other.x + other.width)` for `Left`, etc.), breaking ties by the largest overlap along the perpendicular axis (favors a pane that's actually beside the focused one over a diagonal neighbor that happens to be marginally closer). If there are no candidates, return `None` — the caller leaves `self.focused` untouched.

This is pure, synchronous, and takes only already-available data — no new `Request`, no daemon involvement, no async.

## `App` Changes

`move_focus` (currently `fn move_focus(&mut self, forward: bool)`) becomes `fn move_focus(&mut self, direction: Direction)`, calling `nearest_leaf_in_direction` and updating `self.focused` only if it returns `Some`. `Action::FocusLeft/Right/Up/Down` each map to their own `Direction` variant in `handle_action` (today two of the four already collapse into the same `move_focus(bool)` call; this un-collapses them).

## Click-to-Focus

Today, `MouseEvent::Down` only checks for a divider grab; a click inside a pane's body does nothing. Fixed by extending the existing handler: if the click didn't hit a divider, hit-test `render::leaf_rects` the same way `scroll_pane_under` already does, and if it landed inside some leaf's rect, set `self.focused` to that leaf's id. A click on empty space (shouldn't normally happen — the tree always covers the full frame area — but defensively) or on a divider (existing behavior, unchanged) doesn't change focus.

## Non-Goals

- No change to `cmd-shift-z`, tab cycling (`cmd-]`/`cmd-[`), or any other existing chord.
- No diagonal movement or any new chord — still exactly `h`/`j`/`k`/`l`, now meaning real directions instead of cycle-prev/next.
- No change to focus-reconciliation-after-close (`reconcile_focus`) — that's a separate, already-correct mechanism for keeping `self.focused` valid when the tree shrinks; this only changes what an *intentional* h/j/k/l press does while focus is already valid.

## Testing

- Pure unit tests for `nearest_leaf_in_direction` against hand-built `SplitTree`s + rects, covering: simple 2-pane left/right split (h/l work, j/k no-op), 2-pane top/bottom split (j/k work, h/l no-op), a 3-way layout where the nearest-vs-tie-break logic actually matters (e.g. two candidates both "above" but one directly above vs. one diagonally above — the direct one should win), no-leaf-in-that-direction returns `None`, and the stale/absent-focus fallback to the first leaf.
- `handle_action` test confirming each of `FocusLeft/Right/Up/Down` calls `move_focus` with the correct `Direction` (or an integration-style test at the `App` level, matching how `move_focus`'s current behavior is tested today).
- A click-to-focus test: build an `App` with a real split tree and a nonzero `frame_area`, dispatch a `MouseEvent::Down` inside one leaf's rect, assert `self.focused` updated to that leaf; another asserting a click on a divider's grab zone does NOT change focus (falls through to the existing drag-start behavior).
