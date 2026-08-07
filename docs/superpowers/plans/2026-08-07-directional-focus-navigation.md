# Directional Focus Navigation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `cmd-h/j/k/l` move focus to the leaf that's actually positioned left/down/up/right on screen (no wraparound), replacing today's tree-order leaf-cycling. Clicking inside a pane's body also focuses it.

**Architecture:** A new pure function `nearest_leaf_in_direction` computes real screen-adjacency from `render::leaf_rects` (already built for mouse hit-testing/resize-reporting), replacing `cycle_focus`. `move_focus` takes a new `Direction` enum instead of a `bool`. `handle_mouse`'s `Down` arm gains a fallback: if the click didn't hit a divider, hit-test `leaf_rects` and focus whatever leaf it landed in.

**Tech Stack:** Rust, ratatui, tokio

---

### Task 1: Replace `cycle_focus` with geometric `nearest_leaf_in_direction`

**Files:**
- Modify: `src/tui/mod.rs:57-65` (module doc comment)
- Modify: `src/tui/mod.rs:187-207` (`Action` enum)
- Modify: `src/tui/mod.rs:1277-1281` (`move_focus`)
- Modify: `src/tui/mod.rs:1379-1399` (`handle_action`)
- Modify: `src/tui/mod.rs:1411-1436` (`cycle_focus` → `nearest_leaf_in_direction`)
- Test: `src/tui/mod.rs` (existing `cycle_focus_*` tests, replaced)

- [ ] **Step 1: Write the failing tests for `nearest_leaf_in_direction`**

Replace the five existing `cycle_focus_*` tests (`src/tui/mod.rs:2033-2073`, i.e. `cycle_focus_single_leaf_stays_put`, `cycle_focus_no_current_focus_lands_on_first_leaf`, `cycle_focus_forward_moves_to_next_leaf_and_wraps`, `cycle_focus_backward_moves_to_previous_leaf_and_wraps`, `cycle_focus_unknown_current_id_lands_on_first_leaf`) with:

```rust
fn rect(x: u16, y: u16, width: u16, height: u16) -> ratatui::layout::Rect {
    ratatui::layout::Rect { x, y, width, height }
}

#[test]
fn nearest_leaf_single_leaf_has_no_neighbors() {
    let id = Uuid::new_v4();
    let tree = leaf(id);
    let area = rect(0, 0, 80, 24);
    for dir in [Direction::Left, Direction::Right, Direction::Up, Direction::Down] {
        assert_eq!(nearest_leaf_in_direction(&tree, area, Some(id), dir), None);
    }
}

#[test]
fn nearest_leaf_no_current_focus_lands_on_first_leaf() {
    let a = Uuid::new_v4();
    let b = Uuid::new_v4();
    let tree = split(SplitDir::Vertical, leaf(a), leaf(b));
    let area = rect(0, 0, 80, 24);
    assert_eq!(nearest_leaf_in_direction(&tree, area, None, Direction::Right), Some(a));
}

#[test]
fn nearest_leaf_unknown_current_id_lands_on_first_leaf() {
    let a = Uuid::new_v4();
    let b = Uuid::new_v4();
    let tree = split(SplitDir::Vertical, leaf(a), leaf(b));
    let area = rect(0, 0, 80, 24);
    let stale = Uuid::new_v4();
    assert_eq!(nearest_leaf_in_direction(&tree, area, Some(stale), Direction::Left), Some(a));
}

#[test]
fn nearest_leaf_left_right_split_moves_horizontally_only() {
    let a = Uuid::new_v4();
    let b = Uuid::new_v4();
    // Vertical split = side-by-side panes (see render.rs module doc
    // "SplitDir -> ratatui::Direction mapping").
    let tree = split(SplitDir::Vertical, leaf(a), leaf(b));
    let area = rect(0, 0, 80, 24);
    assert_eq!(nearest_leaf_in_direction(&tree, area, Some(a), Direction::Right), Some(b));
    assert_eq!(nearest_leaf_in_direction(&tree, area, Some(b), Direction::Left), Some(a));
    assert_eq!(nearest_leaf_in_direction(&tree, area, Some(a), Direction::Up), None);
    assert_eq!(nearest_leaf_in_direction(&tree, area, Some(a), Direction::Down), None);
    assert_eq!(nearest_leaf_in_direction(&tree, area, Some(b), Direction::Right), None, "no wraparound");
}

#[test]
fn nearest_leaf_top_bottom_split_moves_vertically_only() {
    let a = Uuid::new_v4();
    let b = Uuid::new_v4();
    // Horizontal split = stacked panes.
    let tree = split(SplitDir::Horizontal, leaf(a), leaf(b));
    let area = rect(0, 0, 80, 24);
    assert_eq!(nearest_leaf_in_direction(&tree, area, Some(a), Direction::Down), Some(b));
    assert_eq!(nearest_leaf_in_direction(&tree, area, Some(b), Direction::Up), Some(a));
    assert_eq!(nearest_leaf_in_direction(&tree, area, Some(a), Direction::Left), None);
    assert_eq!(nearest_leaf_in_direction(&tree, area, Some(a), Direction::Right), None);
}

#[test]
fn nearest_leaf_picks_the_directly_adjacent_pane_over_a_diagonal_one() {
    // Three panes: `top` spans the full width across the top half;
    // `bottom_left`/`bottom_right` split the bottom half side by side.
    // From `bottom_left`, Up must land on `top` (directly above),
    // not skip past it -- and there is only one candidate "above" here
    // by construction, so this also confirms the direction filter
    // itself (not just tie-breaking) is doing the right thing.
    let top = Uuid::new_v4();
    let bottom_left = Uuid::new_v4();
    let bottom_right = Uuid::new_v4();
    let tree = split(
        SplitDir::Horizontal,
        leaf(top),
        split(SplitDir::Vertical, leaf(bottom_left), leaf(bottom_right)),
    );
    let area = rect(0, 0, 80, 24);
    assert_eq!(nearest_leaf_in_direction(&tree, area, Some(bottom_left), Direction::Up), Some(top));
    assert_eq!(nearest_leaf_in_direction(&tree, area, Some(bottom_right), Direction::Up), Some(top));
    assert_eq!(nearest_leaf_in_direction(&tree, area, Some(bottom_left), Direction::Right), Some(bottom_right));
}
```

Run: `cargo check --tests 2>&1 | head -40`
Expected: compile errors — `nearest_leaf_in_direction` and `Direction` don't exist yet.

- [ ] **Step 2: Add the `Direction` enum**

Add near the top of `src/tui/mod.rs`, right after the `Action` enum (after line 207's closing `}`):

```rust
/// A screen-relative direction for [`nearest_leaf_in_direction`]. Not
/// `SplitDir` (which names a divider's orientation) or
/// `ratatui::layout::Direction` (which names a layout axis) — both
/// already mean something else in this codebase; see `render.rs`
/// module doc "SplitDir -> ratatui::Direction mapping" for why a third,
/// unambiguous name is worth it here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Direction {
    Left,
    Right,
    Up,
    Down,
}
```

- [ ] **Step 3: Replace `cycle_focus` with `nearest_leaf_in_direction`**

Replace `src/tui/mod.rs:1411-1436` in full (the `first_leaf` doc comment above it, at line 1405, is unrelated and stays untouched — only replace from the `cycle_focus` doc comment at line 1411 through the end of its function body at line 1436):

```rust
/// Find whichever leaf is nearest to `current` in the given screen
/// `direction`, using each leaf's actual on-screen `Rect` (via
/// `render::leaf_rects`) rather than tree order — see design doc
/// "Directional Focus Navigation" for why this now works where the
/// old tree-order `cycle_focus` was an intentional stand-in. `None` if
/// there is no leaf in that direction at all (no wraparound) or the
/// tree has zero leaves (cannot happen for a real `SplitTree`, but
/// handled defensively). If `current` is `None` or no longer names a
/// leaf in this tree (e.g. it was just closed elsewhere), lands on the
/// first leaf in `leaf_rects`' own order, matching `cycle_focus`'s old
/// fallback behavior for the same edge case.
fn nearest_leaf_in_direction(
    tree: &SplitTree,
    area: ratatui::layout::Rect,
    current: Option<ClientPaneId>,
    direction: Direction,
) -> Option<ClientPaneId> {
    let rects = render::leaf_rects(tree, area);
    if rects.is_empty() {
        return None;
    }
    let focused_rect = current.and_then(|id| rects.iter().find(|(pane, _)| *pane == id).map(|(_, r)| *r));
    let Some(focused_rect) = focused_rect else {
        return Some(rects[0].0);
    };
    rects
        .iter()
        .filter(|(_, rect)| is_in_direction(focused_rect, *rect, direction))
        .min_by_key(|(_, rect)| {
            let gap = axis_gap(focused_rect, *rect, direction);
            let overlap = perpendicular_overlap(focused_rect, *rect, direction);
            // Smallest gap wins; among equal gaps, the largest
            // perpendicular overlap wins (favors a pane actually
            // beside/above/below the focused one over a diagonal
            // neighbor that happens to be marginally closer). Negating
            // overlap turns "largest wins" into the same ascending
            // `min_by_key` comparison as the gap.
            (gap, i32::from(overlap).saturating_neg())
        })
        .map(|(pane, _)| *pane)
}

/// Whether `other` is positioned in `direction` relative to `focused`
/// -- strictly on that side, not overlapping it on the movement axis.
fn is_in_direction(focused: ratatui::layout::Rect, other: ratatui::layout::Rect, direction: Direction) -> bool {
    match direction {
        Direction::Left => other.x + other.width <= focused.x,
        Direction::Right => other.x >= focused.x + focused.width,
        Direction::Up => other.y + other.height <= focused.y,
        Direction::Down => other.y >= focused.y + focused.height,
    }
}

/// The gap between `focused` and `other` along the movement axis --
/// smaller means nearer. Only meaningful for a pair `is_in_direction`
/// already confirmed, so this never underflows.
fn axis_gap(focused: ratatui::layout::Rect, other: ratatui::layout::Rect, direction: Direction) -> u16 {
    match direction {
        Direction::Left => focused.x - (other.x + other.width),
        Direction::Right => other.x - (focused.x + focused.width),
        Direction::Up => focused.y - (other.y + other.height),
        Direction::Down => other.y - (focused.y + focused.height),
    }
}

/// How much `focused` and `other` overlap along the axis perpendicular
/// to `direction` -- larger means "more directly across from" rather
/// than a diagonal neighbor. Zero if they don't overlap on that axis
/// at all.
fn perpendicular_overlap(focused: ratatui::layout::Rect, other: ratatui::layout::Rect, direction: Direction) -> u16 {
    let (focused_start, focused_end, other_start, other_end) = match direction {
        Direction::Left | Direction::Right => {
            (focused.y, focused.y + focused.height, other.y, other.y + other.height)
        }
        Direction::Up | Direction::Down => {
            (focused.x, focused.x + focused.width, other.x, other.x + other.width)
        }
    };
    focused_end.min(other_end).saturating_sub(focused_start.max(other_start))
}
```

- [ ] **Step 4: Update `move_focus` to take a `Direction`**

Replace `src/tui/mod.rs:1277-1281`:

```rust
    fn move_focus(&mut self, direction: Direction) {
        if let Some(tree) = &self.workspace.tree {
            if let Some(next) = nearest_leaf_in_direction(tree, self.frame_area, self.focused, direction) {
                self.focused = Some(next);
            }
        }
    }
```

- [ ] **Step 5: Update `handle_action`'s `FocusLeft/Right/Up/Down` arms**

Replace `src/tui/mod.rs:1391-1392`:

```rust
            Action::FocusLeft => self.move_focus(Direction::Left),
            Action::FocusRight => self.move_focus(Direction::Right),
            Action::FocusUp => self.move_focus(Direction::Up),
            Action::FocusDown => self.move_focus(Direction::Down),
```

- [ ] **Step 6: Update the module doc comment**

Replace `src/tui/mod.rs:57-65`:

```rust
//! - **Focus movement (`FocusLeft/Right/Up/Down`) uses real screen
//!   adjacency.** `nearest_leaf_in_direction` computes each leaf's
//!   actual on-screen `Rect` via `render::leaf_rects` (the same data
//!   mouse hit-testing already uses) and picks whichever leaf lies in
//!   the requested direction, nearest first. No wraparound: if nothing
//!   is in that direction, the chord is a no-op.
```

- [ ] **Step 7: Run the new tests**

Run: `cargo test -p dimux --lib tui:: -- nearest_leaf 2>&1 | tail -20`
Expected: all 6 new tests PASS.

- [ ] **Step 8: Run the full crate check and test suite**

Run: `cargo check --all-targets 2>&1 | tail -20`
Expected: no errors (confirms no other call site still references `cycle_focus` or the old `move_focus(bool)` signature).

Run: `cargo test 2>&1 | tail -20`
Expected: ALL PASS.

- [ ] **Step 9: Commit**

```bash
git add src/tui/mod.rs
git commit -m "$(cat <<'EOF'
feat(tui): cmd-h/j/k/l move focus by real screen direction

Replaces cycle_focus's tree-order leaf-cycling with
nearest_leaf_in_direction, which uses render::leaf_rects (the same
per-leaf on-screen rects mouse hit-testing already computes) to find
whichever leaf is actually positioned left/right/up/down of the
focused one. No wraparound: nothing in that direction is a no-op.
EOF
)"
```

---

### Task 2: Click-to-focus

**Files:**
- Modify: `src/tui/mod.rs:1302-1311` (`handle_mouse`'s `Down` arm)
- Test: `src/tui/mod.rs` (new tests near the existing mouse-related tests)

- [ ] **Step 1: Write the failing tests**

Add near `app_against_real_daemon` (the helper used by other `App`-level tests in this file, around line 3409). Note `app_against_real_daemon` returns an *empty, unfocused* workspace (per its own doc comment — it deliberately undoes the shell fallback), so both tests spawn their own two side-by-side panes directly:

```rust
#[tokio::test]
async fn click_inside_a_pane_focuses_it() {
    let (mut app, mut write_half, mut reader) = app_against_real_daemon().await;
    let Response::ClientPaneCreated { pane: left, .. } = app
        .request(
            &mut write_half,
            &mut reader,
            Request::ClientSpawn { workspace: "1".to_string(), split_of: None, dir: None, bind: None },
        )
        .await
        .unwrap()
    else {
        panic!("expected ClientPaneCreated");
    };
    let Response::ClientPaneCreated { pane: right, .. } = app
        .request(
            &mut write_half,
            &mut reader,
            Request::ClientSpawn {
                workspace: "1".to_string(),
                split_of: Some(left),
                dir: Some(SplitDir::Vertical),
                bind: None,
            },
        )
        .await
        .unwrap()
    else {
        panic!("expected ClientPaneCreated");
    };
    app.focused = Some(left);
    app.frame_area = ratatui::layout::Rect { x: 0, y: 0, width: 80, height: 24 };

    // `left`/`right` are side by side (SplitDir::Vertical); `right`
    // occupies roughly the rightmost half, so a click near the right
    // edge lands inside it regardless of the exact divider position.
    app.handle_mouse(mouse::MouseEvent::Down { col: 75, row: 5 }, &mut write_half, &mut reader)
        .await
        .unwrap();

    assert_eq!(app.focused, Some(right), "clicking inside the right pane should focus it");
}

#[tokio::test]
async fn click_on_a_divider_does_not_change_focus() {
    let (mut app, mut write_half, mut reader) = app_against_real_daemon().await;
    let Response::ClientPaneCreated { pane: left, .. } = app
        .request(
            &mut write_half,
            &mut reader,
            Request::ClientSpawn { workspace: "1".to_string(), split_of: None, dir: None, bind: None },
        )
        .await
        .unwrap()
    else {
        panic!("expected ClientPaneCreated");
    };
    let _ = app
        .request(
            &mut write_half,
            &mut reader,
            Request::ClientSpawn {
                workspace: "1".to_string(),
                split_of: Some(left),
                dir: Some(SplitDir::Vertical),
                bind: None,
            },
        )
        .await
        .unwrap();
    app.focused = Some(left);
    app.frame_area = ratatui::layout::Rect { x: 0, y: 0, width: 80, height: 24 };

    // A 50/50 vertical split of an 80-wide area puts the 1-column
    // divider at col 40 (see render.rs's percent-based layout math).
    app.handle_mouse(mouse::MouseEvent::Down { col: 40, row: 5 }, &mut write_half, &mut reader)
        .await
        .unwrap();

    assert_eq!(app.focused, Some(left), "clicking the divider should start a drag, not change focus");
    assert!(app.dragging_split.is_some(), "the click should have been recognized as a divider grab");
}
```

Run: `cargo check --tests 2>&1 | head -20`
Expected: compiles (both tests exercise only existing behavior at this point, so `click_inside_a_pane_focuses_it` FAILS at its final assertion — `handle_mouse`'s `Down` arm doesn't focus anything yet).

- [ ] **Step 2: Run the tests to confirm the expected failure**

Run: `cargo test -p dimux --lib tui:: -- click_inside_a_pane_focuses_it 2>&1 | tail -20`
Expected: FAILS at `assert_eq!(app.focused, Some(right), ...)` — `app.focused` is still `Some(left)`.

- [ ] **Step 3: Add the click-to-focus fallback in `handle_mouse`**

Replace `src/tui/mod.rs:1302-1311`:

```rust
            mouse::MouseEvent::Down { col, row } => {
                let Some(tree) = &self.workspace.tree else { return Ok(()) };
                let hits = render::divider_rects(tree, self.frame_area);
                if let Some(split) = hit_test_dividers(&hits, col, row) {
                    let hit = hits.iter().find(|h| h.split == split).expect(
                        "hit_test_dividers only returns a split id that came from this exact `hits` list",
                    );
                    self.dragging_split = Some((split, render::ratio_at(hit, col, row)));
                } else if let Some((pane, _)) = render::leaf_rects(tree, self.frame_area)
                    .into_iter()
                    .find(|(_, rect)| {
                        col >= rect.x && col < rect.x + rect.width && row >= rect.y && row < rect.y + rect.height
                    })
                {
                    self.focused = Some(pane);
                }
            }
```

- [ ] **Step 4: Run both new tests**

Run: `cargo test -p dimux --lib tui:: -- click_inside_a_pane_focuses_it click_on_a_divider_does_not_change_focus 2>&1 | tail -20`
Expected: both PASS.

- [ ] **Step 5: Update `handle_mouse`'s doc comment**

The doc comment above `handle_mouse` (right before `src/tui/mod.rs:1295`, starting "Route one parsed [`mouse::MouseEvent`] — divider drag-resizing, per user direction: no keybind, drag only.") is now inaccurate — `Down` does more than divider-drag-resizing. Update its first sentence:

```rust
    /// Route one parsed [`mouse::MouseEvent`] — divider drag-resizing
    /// and click-to-focus. `Down` hit-tests against the current
    /// layout's divider grab zones first (recomputed fresh every call
    /// via `render::divider_rects`, since the layout can change between
    /// mouse events); if that misses, it hit-tests `render::leaf_rects`
    /// instead and focuses whichever leaf the click landed in. `Drag`
    /// updates `dragging_split`'s live ratio purely locally — no
```

(Leave the rest of the existing doc comment, from "request sent" onward, unchanged — only this first paragraph's framing needs updating; the `Drag`/`Up` behavior it describes afterward is untouched by this task.)

- [ ] **Step 6: Run the full crate check and test suite**

Run: `cargo check --all-targets 2>&1 | tail -20`
Expected: no errors.

Run: `cargo test 2>&1 | tail -20`
Expected: ALL PASS.

- [ ] **Step 7: Run clippy**

Run: `cargo clippy -- -D warnings 2>&1 | tail -20`
Expected: no warnings.

- [ ] **Step 8: Commit**

```bash
git add src/tui/mod.rs
git commit -m "$(cat <<'EOF'
feat(tui): click inside a pane focuses it

MouseEvent::Down previously only ever checked for a divider grab;
clicking inside a pane's body did nothing. Now falls back to
hit-testing render::leaf_rects (the same data used for scroll-under-
cursor and resize-reporting) and focuses whichever leaf the click
landed in.
EOF
)"
```

---

### Task 3: Final verification and PR

**Files:** None new — verification only.

- [ ] **Step 1: Full test suite, repeated for stability**

Run: `for i in 1 2 3; do cargo test 2>&1 | grep "test result"; done`
Expected: ALL PASS, all 3 runs identical.

- [ ] **Step 2: Clippy**

Run: `cargo clippy -- -D warnings 2>&1 | tail -20`
Expected: no warnings.

- [ ] **Step 3: Release build**

Run: `cargo build --release 2>&1 | tail -5`
Expected: success.

- [ ] **Step 4: Manual smoke test**

```bash
cargo build --release
./target/release/dimux
```
Split into a 2x2 grid (`cmd-d` then `cmd-shift-d` on each half), then confirm: `cmd-h`/`cmd-l` move focus left/right between the correct panes, `cmd-j`/`cmd-k` move focus down/up between the correct panes, a chord with nothing in that direction does nothing (no wraparound), and clicking inside any pane focuses it.

- [ ] **Step 5: Push and open draft PR**

```bash
git push -u origin feat/directional-focus-navigation
gh pr create --draft --title "feat: geometric directional focus navigation + click-to-focus" --body "$(cat <<'EOF'
## Summary
- `cmd-h/j/k/l` now move focus to whichever leaf is actually positioned left/down/up/right on screen, using the same per-leaf `Rect`s (`render::leaf_rects`) already computed for mouse hit-testing and resize-reporting. No wraparound: a direction with nothing in it is a no-op.
- Clicking inside a pane's body now focuses it (previously a no-op; only divider clicks did anything).

See `docs/superpowers/specs/2026-08-07-directional-focus-navigation-design.md` and the matching plan doc for full design/rationale.

## Test plan
- [ ] `cargo test` passes (run 3x for stability)
- [ ] `cargo clippy -- -D warnings` clean
- [ ] Manual: 2x2 pane grid, h/j/k/l move to the geometrically correct neighbor, no wraparound at the edges
- [ ] Manual: clicking inside any pane focuses it
EOF
)"
```

- [ ] **Step 6: Report the PR URL**
