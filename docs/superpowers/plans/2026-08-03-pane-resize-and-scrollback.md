# Pane Resize Wiring + Mouse-Wheel Scrollback Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Fix the "bash renders in the top half" bug by actually wiring
up pane-size reporting to the daemon (the protocol/daemon side already
exists and is tested — only the TUI frontend was missing), then add
mouse-wheel scrollback reusing `wezterm_term`'s existing scrollback
buffer.

**Architecture:** Part 1 (Tasks 1-3) adds a `render::leaf_rects` helper
(mirrors the existing `divider_rects` pattern) and has `run`'s loop diff
each leaf's rect against a new `App.pane_sizes` map every frame, sending
`Request::ResizeClientPane` on change. Part 2 (Tasks 4-8) adds a
`scroll_offset` field to `GridSnapshot`, a new `Request::ScrollClientPane`,
server-side offset storage keyed by `(SubscriberId, ServerPaneId)` (not
`ClientPaneId` — see design spec's "Scroll offset ownership" for why),
and wires two new mouse-wheel `MouseEvent` variants through to it.

**Tech Stack:** Rust, tokio, ratatui, wezterm_term (vendored as
`tattoy-wezterm-term`) — no new dependencies. Full design rationale,
including two corrections made during spec self-review (an initial
`ClientPaneId`-keyed offset design was found to be incoherent with the
wire protocol's actual shape, and `wezterm_term::Screen::scrollback_rows()`'s
true semantics), is in
`docs/superpowers/specs/2026-08-03-pane-resize-and-scrollback-design.md`
— read it before starting if anything below is unclear on the "why".

---

## File structure

- **Modify `src/tui/render.rs`**: new `leaf_rects` pure helper (mirrors
  `divider_rects`); `draw_leaf`'s title gets a `[scrollback]` suffix.
- **Modify `src/tui/mod.rs`**: `App` gains `pane_sizes: HashMap<ClientPaneId,
  Size>`; `run`'s loop diffs leaf sizes every frame and sends
  `ResizeClientPane`; `mouse::MouseEvent`'s two new variants get handled
  in `handle_mouse`.
- **Modify `src/tui/mouse.rs`**: `MouseEvent` gains `ScrollUp`/`ScrollDown`;
  `parse` recognizes scroll-wheel button numbers before the catch-all
  `Ignored`.
- **Modify `src/protocol.rs`**: `GridSnapshot` gains `scroll_offset:
  usize`; new `Request::ScrollClientPane { pane, delta }`.
- **Modify `src/term/mod.rs`**: `ServerPane` gains `scrollback_rows()`;
  `snapshot()` gains an `offset: usize` parameter.
- **Modify `src/daemon/state.rs`**: `State` gains `scroll_offsets:
  HashMap<(SubscriberId, ServerPaneId), usize>`; new
  `scroll_server_pane`/`clear_scroll_offsets_for_subscriber` methods;
  `server_kill` additionally clears offsets for the killed server-pane.
- **Modify `src/daemon/mod.rs`**: new `ScrollClientPane` dispatch arm;
  `Subscribe` arm's snapshot calls pass the subscriber's recorded
  offset; `GridBroadcast`/`broadcast_grid_prepare` restructured to
  support multiple distinct-offset groups per broadcast;
  `handle_connection`'s teardown calls the new cleanup method.

No new files — every change extends an existing module's established
responsibility (state/dispatch logic stays in `daemon/{state,mod}.rs`,
terminal-emulation glue stays in `term/mod.rs`, rendering stays in
`tui/render.rs`, event-loop/input glue stays in `tui/mod.rs`).

---

### Task 1: `leaf_rects` render helper

**Files:**
- Modify: `src/tui/render.rs` (add near `divider_rects`, which starts at
  line 165)
- Test: `src/tui/render.rs` `mod tests` block

- [ ] **Step 1: Write the failing tests**

Add to `src/tui/render.rs`'s `mod tests` block (near the existing
`divider_rects_*` tests):

```rust
#[test]
fn leaf_rects_single_leaf_returns_the_whole_area() {
    let id = Uuid::new_v4();
    let tree = SplitTree::Leaf(ClientPane { id, name: None, bound: None });
    let area = Rect { x: 0, y: 0, width: 80, height: 24 };
    let rects = leaf_rects(&tree, area);
    assert_eq!(rects, vec![(id, area)]);
}

#[test]
fn leaf_rects_side_by_side_split_divides_width() {
    let a = Uuid::new_v4();
    let b = Uuid::new_v4();
    let tree = SplitTree::Split {
        id: Uuid::new_v4(),
        dir: SplitDir::Vertical,
        ratio: 0.5,
        a: Box::new(SplitTree::Leaf(ClientPane { id: a, name: None, bound: None })),
        b: Box::new(SplitTree::Leaf(ClientPane { id: b, name: None, bound: None })),
    };
    let area = Rect { x: 0, y: 0, width: 81, height: 24 };
    let rects = leaf_rects(&tree, area);
    assert_eq!(rects.len(), 2);
    let rect_a = rects.iter().find(|(id, _)| *id == a).unwrap().1;
    let rect_b = rects.iter().find(|(id, _)| *id == b).unwrap().1;
    // 81-wide area, 50/50 split, minus the 1-column reserved divider --
    // same math draw_tree already uses for SplitDir::Vertical.
    assert_eq!(rect_a.width + rect_b.width + 1, 81);
    assert_eq!(rect_a.height, 24);
    assert_eq!(rect_b.height, 24);
}

#[test]
fn leaf_rects_stacked_split_divides_height() {
    let a = Uuid::new_v4();
    let b = Uuid::new_v4();
    let tree = SplitTree::Split {
        id: Uuid::new_v4(),
        dir: SplitDir::Horizontal,
        ratio: 0.5,
        a: Box::new(SplitTree::Leaf(ClientPane { id: a, name: None, bound: None })),
        b: Box::new(SplitTree::Leaf(ClientPane { id: b, name: None, bound: None })),
    };
    let area = Rect { x: 0, y: 0, width: 80, height: 24 };
    let rects = leaf_rects(&tree, area);
    assert_eq!(rects.len(), 2);
    let rect_a = rects.iter().find(|(id, _)| *id == a).unwrap().1;
    let rect_b = rects.iter().find(|(id, _)| *id == b).unwrap().1;
    // SplitDir::Horizontal reserves no extra row (module doc "Bezels") --
    // heights sum exactly to the parent, unlike the vertical-split case.
    assert_eq!(rect_a.height + rect_b.height, 24);
    assert_eq!(rect_a.width, 80);
    assert_eq!(rect_b.width, 80);
}
```

Check the test module's existing imports at the top of `mod tests` — it
already has `use super::*;` plus `use crate::protocol::{...};` and
`use uuid::Uuid;` (confirm by reading the file; these tests need
`ClientPane`, `SplitDir`, `SplitTree`, `Uuid`, `Rect` in scope, all of
which prior tests in this same module already use).

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test leaf_rects`
Expected: FAIL with `cannot find function 'leaf_rects' in this scope`

- [ ] **Step 3: Write the implementation**

Add to `src/tui/render.rs`, right after `divider_rects`/`collect_divider_rects`
(after line 207, before `ratio_at`):

```rust
/// Every leaf's on-screen `Rect`, keyed by `ClientPaneId` -- mirrors
/// `divider_rects`'s pattern exactly (same recursive walk, same
/// `Layout`/`Constraint` math `draw_tree` uses to lay out real frames),
/// but collects leaf rects instead of divider grab zones. Used by
/// `App`'s render loop to know each pane's actual on-screen size (for
/// `Request::ResizeClientPane` reporting) and by `App::handle_mouse` to
/// hit-test which pane a mouse-wheel event landed over.
pub fn leaf_rects(tree: &SplitTree, area: Rect) -> Vec<(crate::protocol::ClientPaneId, Rect)> {
    let mut out = Vec::new();
    collect_leaf_rects(tree, area, &mut out);
    out
}

fn collect_leaf_rects(tree: &SplitTree, area: Rect, out: &mut Vec<(crate::protocol::ClientPaneId, Rect)>) {
    match tree {
        SplitTree::Leaf(pane) => out.push((pane.id, area)),
        SplitTree::Split { dir, ratio, a, b, .. } => {
            let direction = ratatui_direction(*dir);
            let percent_a = (ratio.clamp(0.0, 1.0) * 100.0).round() as u16;
            let percent_b = 100u16.saturating_sub(percent_a);
            let (rect_a, rect_b) = match direction {
                Direction::Horizontal => {
                    let rects = Layout::new(
                        direction,
                        [
                            Constraint::Percentage(percent_a),
                            Constraint::Length(1),
                            Constraint::Percentage(percent_b),
                        ],
                    )
                    .split(area);
                    (rects[0], rects[2])
                }
                Direction::Vertical => {
                    let rects = Layout::new(
                        direction,
                        [Constraint::Percentage(percent_a), Constraint::Percentage(percent_b)],
                    )
                    .split(area);
                    (rects[0], rects[1])
                }
            };
            collect_leaf_rects(a, rect_a, out);
            collect_leaf_rects(b, rect_b, out);
        }
    }
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test leaf_rects`
Expected: PASS (3 tests)

- [ ] **Step 5: Commit**

```bash
git add src/tui/render.rs
git commit -m "feat: add leaf_rects render helper for pane size/hit-testing"
```

---

### Task 2: Wire `leaf_rects` into the render loop to fix the resize bug

**Files:**
- Modify: `src/tui/mod.rs` (`App` struct around line 257, `run`'s loop
  around line 1093)
- Test: `src/tui/mod.rs` `mod tests` block

- [ ] **Step 1: Write the failing tests for the pure size-diffing helper**

Add to `src/tui/mod.rs`'s `mod tests` block. This reuses the module's
existing `leaf(id) -> SplitTree` test helper (already defined near the
top of `mod tests`, used by the `cycle_focus_*` tests) — do not redefine
it:

```rust
#[test]
fn changed_pane_sizes_reports_a_never_seen_pane() {
    let id = Uuid::new_v4();
    let tree = leaf(id);
    let area = ratatui::layout::Rect { x: 0, y: 0, width: 80, height: 24 };
    let current: HashMap<ClientPaneId, crate::protocol::Size> = HashMap::new();
    let changed = changed_pane_sizes(&current, &tree, area);
    assert_eq!(changed.len(), 1);
    assert_eq!(changed[0].0, id);
    // -1 row for the title-bar border every leaf reserves (see
    // `render::draw_leaf`) -- the usable grid area is one row shorter
    // than the leaf's full on-screen rect.
    assert_eq!(changed[0].1, crate::protocol::Size { rows: 23, cols: 80 });
}

#[test]
fn changed_pane_sizes_reports_nothing_when_unchanged() {
    let id = Uuid::new_v4();
    let tree = leaf(id);
    let area = ratatui::layout::Rect { x: 0, y: 0, width: 80, height: 24 };
    let mut current = HashMap::new();
    current.insert(id, crate::protocol::Size { rows: 23, cols: 80 });
    let changed = changed_pane_sizes(&current, &tree, area);
    assert_eq!(changed.len(), 0);
}

#[test]
fn changed_pane_sizes_reports_a_genuinely_resized_pane() {
    let id = Uuid::new_v4();
    let tree = leaf(id);
    let area = ratatui::layout::Rect { x: 0, y: 0, width: 100, height: 30 };
    let mut current = HashMap::new();
    current.insert(id, crate::protocol::Size { rows: 23, cols: 80 });
    let changed = changed_pane_sizes(&current, &tree, area);
    assert_eq!(changed.len(), 1);
    assert_eq!(changed[0], (id, crate::protocol::Size { rows: 29, cols: 100 }));
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test changed_pane_sizes`
Expected: FAIL — `changed_pane_sizes` doesn't exist yet.

- [ ] **Step 3: Add `App.pane_sizes` field**

In `src/tui/mod.rs`, find the `App` struct (starts at line 257):

```rust
struct App {
    workspace: WorkspaceInfo,
    grids: HashMap<ServerPaneId, GridSnapshot>,
    focused: Option<ClientPaneId>,
```

Add a new field right after `grids`:

```rust
struct App {
    workspace: WorkspaceInfo,
    grids: HashMap<ServerPaneId, GridSnapshot>,
    /// Last on-screen size this frontend told the daemon about for each
    /// client-pane (`Request::ResizeClientPane`), so `run`'s loop only
    /// sends a new report when something actually changed instead of
    /// re-sending every pane's size on every single frame. Mirrors the
    /// daemon's own `State::client_pane_sizes`, one-directionally: this
    /// is purely this frontend's memory of what it last *told* the
    /// daemon, not a second source of truth.
    pane_sizes: HashMap<ClientPaneId, Size>,
    focused: Option<ClientPaneId>,
```

You'll need `Size` in scope — check the existing `use crate::protocol::{...}` import list at the top of the file and add `Size` to it if it's not already there.

Every place that constructs an `App` literal (there are several: `App::bootstrap`,
and multiple test helpers in `mod tests`) needs `pane_sizes: HashMap::new()`
added to its field list — find them with:

```bash
grep -n "App {" src/tui/mod.rs
```

Update every one.

- [ ] **Step 4: Write `changed_pane_sizes`**

Add this pure function in `src/tui/mod.rs`, near `hit_test_dividers`:

```rust
/// Diff each leaf's current on-screen `Rect` (from `leaf_rects`) against
/// `current` (a frontend's last-reported sizes), returning only the
/// panes whose size actually changed -- including a pane not yet
/// present in `current` at all (its first frame). Pure and
/// `App`-free so it's directly unit-testable; `run`'s loop calls this
/// every frame and only sends `Request::ResizeClientPane` for whatever
/// this returns, rather than unconditionally re-reporting every pane on
/// every frame.
///
/// Subtracts 1 from each leaf's rect height before comparing/returning
/// -- every leaf reserves its top row as a title-bar border (see
/// `render::draw_leaf`), so the *usable* grid area the PTY should
/// actually be sized to is one row shorter than the leaf's full
/// on-screen rect.
fn changed_pane_sizes(
    current: &HashMap<ClientPaneId, crate::protocol::Size>,
    tree: &SplitTree,
    area: ratatui::layout::Rect,
) -> Vec<(ClientPaneId, crate::protocol::Size)> {
    render::leaf_rects(tree, area)
        .into_iter()
        .filter_map(|(pane_id, rect)| {
            let size = crate::protocol::Size {
                rows: rect.height.saturating_sub(1),
                cols: rect.width,
            };
            if current.get(&pane_id) == Some(&size) {
                None
            } else {
                Some((pane_id, size))
            }
        })
        .collect()
}
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test changed_pane_sizes`
Expected: PASS (3 tests)

- [ ] **Step 6: Wire it into `run`'s loop**

In `src/tui/mod.rs`, find `run`'s loop (starts around line 1093):

```rust
    loop {
        terminal.draw(|frame| {
            app.frame_area = frame.area();
```

The draw closure is synchronous (can't `.await` inside it), so the
actual `Request::ResizeClientPane` sends need to happen *after* the
closure returns, using the diffs computed *inside* it. Change the loop
body's structure: capture the changed sizes as a `Vec` from inside the
closure (via a `let mut` local declared just before `terminal.draw`),
then send requests for each one right after the `terminal.draw(...)?;`
line and before the `tokio::select!`.

Find this exact block:

```rust
    loop {
        terminal.draw(|frame| {
            app.frame_area = frame.area();
            // While a divider drag is in progress, render a locally
            // patched clone with the live ratio applied -- this is what
            // makes the drag look smooth (drawn every frame with no
            // round-trip) even though the daemon isn't told about it
            // until `Up` (see `dragging_split`'s field doc for why).
            match app.dragging_split {
                Some((split, ratio)) => {
                    let mut preview = app.workspace.clone();
                    if let Some(tree) = &mut preview.tree {
                        let _ = tree.resize_split(split, ratio);
                    }
                    render::draw(frame, &preview, &app.grids, app.focused);
                }
                None => render::draw(frame, &app.workspace, &app.grids, app.focused),
            }
            if let Some(menu) = &app.attach_menu {
                render::draw_attach_menu(frame, menu);
            }
        })?;

        tokio::select! {
```

Replace with:

```rust
    loop {
        terminal.draw(|frame| {
            app.frame_area = frame.area();
            // While a divider drag is in progress, render a locally
            // patched clone with the live ratio applied -- this is what
            // makes the drag look smooth (drawn every frame with no
            // round-trip) even though the daemon isn't told about it
            // until `Up` (see `dragging_split`'s field doc for why).
            match app.dragging_split {
                Some((split, ratio)) => {
                    let mut preview = app.workspace.clone();
                    if let Some(tree) = &mut preview.tree {
                        let _ = tree.resize_split(split, ratio);
                    }
                    render::draw(frame, &preview, &app.grids, app.focused);
                }
                None => render::draw(frame, &app.workspace, &app.grids, app.focused),
            }
            if let Some(menu) = &app.attach_menu {
                render::draw_attach_menu(frame, menu);
            }
        })?;

        // Report any leaf whose on-screen size changed since the last
        // frame -- see `changed_pane_sizes`'s doc comment for why this
        // piggybacks on the render loop's own cadence instead of a
        // SIGWINCH handler. This is the fix for the "bash renders in the
        // top half" bug: without this, `Request::ResizeClientPane` is
        // never sent at all, and every PTY stays pinned at the daemon's
        // 24x80 default regardless of the pane's real size.
        if let Some(tree) = &app.workspace.tree {
            let changed = changed_pane_sizes(&app.pane_sizes, tree, app.frame_area);
            for (pane, size) in changed {
                app.pane_sizes.insert(pane, size);
                let req = Request::ResizeClientPane { pane, size };
                let _ = app.request(&mut write_half, &mut reader, req).await?;
            }
        }

        tokio::select! {
```

- [ ] **Step 7: Run the full test suite**

Run: `cargo build 2>&1 | grep -A3 "error\["`
Expected: no output (clean build)

Run: `cargo test`
Expected: all tests PASS.

- [ ] **Step 8: Commit**

```bash
git add src/tui/mod.rs
git commit -m "fix: report client-pane sizes to the daemon every frame

Fixes the daemon's PTY sizing being permanently stuck at the hardcoded
24x80 default -- Request::ResizeClientPane always existed on the
protocol/daemon side (with tested smallest-viewer-wins logic behind it)
but the TUI frontend never sent it, so a pane rendered larger than
24x80 only ever showed content in its top-left 24x80 corner."
```

---

### Task 3: Integration test for the resize fix

**Files:**
- Modify: `src/daemon/mod.rs` `mod tests` block

- [ ] **Step 1: Write the test**

Add to `src/daemon/mod.rs`'s `mod tests` block, after
`resize_split_updates_ratio_visible_to_subscribers`:

```rust
    /// Regression test for the "bash renders in the top half" bug: a
    /// client-pane's on-screen size, once reported via
    /// `Request::ResizeClientPane`, must actually change the bound
    /// server-pane's PTY/grid size that a subsequent `Subscribe`
    /// snapshot returns -- this exercises the exact wire-level path the
    /// TUI's per-frame resize reporting (added in Task 2) now drives.
    #[tokio::test]
    async fn resize_client_pane_changes_the_subscribed_grid_size() {
        let guard = start_daemon().await;
        let mut conn = TestConn::connect(&guard.0).await;

        let (workspace, pane) = match conn
            .request(Request::ClientSpawn {
                workspace: "1".to_string(),
                split_of: None,
                dir: None,
                bind: None,
            })
            .await
        {
            Response::ClientPaneCreated { workspace, pane } => (workspace, pane),
            other => panic!("expected ClientPaneCreated, got {other:?}"),
        };

        match conn
            .request(Request::ResizeClientPane {
                pane,
                size: Size { rows: 40, cols: 120 },
            })
            .await
        {
            Response::Ack => {}
            other => panic!("expected Ack, got {other:?}"),
        }

        match conn.request(Request::Subscribe { workspace: workspace.to_string() }).await {
            Response::Snapshot { grids, .. } => {
                // No server-pane is bound yet in this test, so `grids`
                // is empty -- this test only needs to confirm the
                // request itself is accepted and doesn't error; the
                // actual size-application-to-a-bound-pane path is
                // already covered by `daemon::state`'s
                // `pty_size_is_smallest_viewer_dimension_wise` test.
                // Bind a server-pane now and re-subscribe to see the
                // resize actually reflected in a grid.
                let _ = grids;
            }
            other => panic!("expected Snapshot, got {other:?}"),
        }

        let server_pane = match conn
            .request(Request::ServerSpawn { name: None, cmd: Some("cat".to_string()) })
            .await
        {
            Response::ServerPane(info) => info.id,
            other => panic!("expected ServerPane, got {other:?}"),
        };
        match conn
            .request(Request::ClientBind {
                workspace: workspace.to_string(),
                pane,
                target: server_pane.to_string(),
            })
            .await
        {
            Response::Ack => {}
            other => panic!("expected Ack, got {other:?}"),
        }

        match conn.request(Request::Subscribe { workspace: workspace.to_string() }).await {
            Response::Snapshot { grids, .. } => {
                assert_eq!(grids.len(), 1, "expected exactly one grid for the bound server-pane");
                assert_eq!(
                    grids[0].size,
                    Size { rows: 40, cols: 120 },
                    "server-pane's grid size should reflect the earlier ResizeClientPane call"
                );
            }
            other => panic!("expected Snapshot, got {other:?}"),
        }
    }
```

- [ ] **Step 2: Run the test**

Run: `cargo test resize_client_pane_changes_the_subscribed_grid_size`
Expected: PASS (this exercises only existing, already-implemented
daemon logic — `Request::ResizeClientPane`'s dispatch arm and
`State::resize_client_pane`/`apply_pty_size` already work; this test is
new coverage, not new functionality).

- [ ] **Step 3: Run the full suite**

Run: `cargo test`
Expected: all tests PASS.

- [ ] **Step 4: Commit**

```bash
git add src/daemon/mod.rs
git commit -m "test: add integration coverage for ResizeClientPane's effect on grid size"
```

---

### Task 4: Protocol changes — `GridSnapshot.scroll_offset` + `Request::ScrollClientPane`

**Files:**
- Modify: `src/protocol.rs`

- [ ] **Step 1: Add `scroll_offset` to `GridSnapshot`**

In `src/protocol.rs`, find:

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GridSnapshot {
    pub server_pane: ServerPaneId,
    pub size: Size,
    pub cursor: (u16, u16),
    /// Row-major: `lines[row][col]`.
    pub lines: Vec<Vec<Cell>>,
}
```

Replace with:

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GridSnapshot {
    pub server_pane: ServerPaneId,
    pub size: Size,
    pub cursor: (u16, u16),
    /// Row-major: `lines[row][col]`.
    pub lines: Vec<Vec<Cell>>,
    /// How many rows back from the live tail this snapshot's `lines`
    /// starts at (0 = the live/current viewport, matching every
    /// snapshot before this field existed). Lets a frontend distinguish
    /// "this pane is showing history" from "this pane is live" without
    /// separately tracking offset state itself.
    pub scroll_offset: usize,
}
```

- [ ] **Step 2: Add `Request::ScrollClientPane`**

In `src/protocol.rs`, find the `ResizeClientPane` variant inside `enum Request`:

```rust
    /// Report the current on-screen size of a client-pane (e.g. after a
    /// frontend terminal resize or a split-ratio change), so the daemon
    /// can recompute its bound server-pane's PTY size.
    ResizeClientPane {
        pane: ClientPaneId,
        size: Size,
    },
```

Add a new variant right after it:

```rust
    /// Report the current on-screen size of a client-pane (e.g. after a
    /// frontend terminal resize or a split-ratio change), so the daemon
    /// can recompute its bound server-pane's PTY size.
    ResizeClientPane {
        pane: ClientPaneId,
        size: Size,
    },
    /// Scroll `pane`'s bound server-pane's view back into (positive
    /// `delta`) or forward out of (negative `delta`) scrollback
    /// history, from this connection's own point of view. Addressed by
    /// client-pane, matching `Input`/`ResizeClientPane`'s convention,
    /// but the daemon resolves it to the bound server-pane and stores
    /// the resulting offset keyed by `(this connection, that
    /// server-pane)` -- NOT by `pane` itself. See design doc "Scroll
    /// offset ownership" for why: `GridSnapshot` carries only a
    /// `server_pane` id, so a connection can only ever see one grid per
    /// server-pane at a time regardless of how many client-panes it has
    /// bound -- offset has to live at that same granularity to be
    /// deliverable at all. The daemon clamps the resulting offset to
    /// `0..=` that server-pane's available scrollback server-side; the
    /// frontend never computes or owns the authoritative value. A
    /// `pane` that's currently unbound is a silent no-op (`Ack`, not
    /// `Error` -- an accidental mouse-wheel event over an unbound
    /// placeholder is a very plausible occurrence, not a client bug
    /// worth surfacing as an error).
    ScrollClientPane {
        pane: ClientPaneId,
        delta: i32,
    },
```

- [ ] **Step 3: Update every existing `GridSnapshot` construction site to compile**

This will not compile yet — `GridSnapshot` is constructed in
`src/term/mod.rs`'s `ServerPane::snapshot` and in test code. Run:

```bash
cargo build 2>&1 | grep -B2 "missing field \`scroll_offset\`"
```

Expected: errors in `src/term/mod.rs` (the real `snapshot()` method,
fixed in Task 5) and possibly test helper code in `src/tui/render.rs`'s
or `src/tui/mod.rs`'s test modules that construct a bare `GridSnapshot`
literal directly (search for `GridSnapshot {` across the codebase to
find every site: `grep -rn "GridSnapshot {" src/`). For any test-only
construction site, add `scroll_offset: 0` to its field list — this
preserves that test's existing behavior exactly (0 = live, matching
what every snapshot implicitly was before this field existed).

Do NOT attempt to fix `src/term/mod.rs`'s real `snapshot()` method in
this task — that's Task 5's job, which needs the `offset` parameter
threaded through properly, not just a hardcoded `0`.

- [ ] **Step 4: Commit**

```bash
git add src/protocol.rs
```

Also stage whatever test files you touched in Step 3 for `scroll_offset: 0`
literals (but NOT `src/term/mod.rs`, which Task 5 owns):

```bash
git status
# add any test files with scroll_offset: 0 additions, e.g.:
# git add src/tui/render.rs src/tui/mod.rs
git commit -m "feat: add GridSnapshot.scroll_offset and Request::ScrollClientPane to the wire protocol"
```

The crate will NOT compile cleanly after this commit
(`src/term/mod.rs`'s real `snapshot()` still needs the parameter added)
— this is expected, matching the same "commit an intentionally
mid-refactor state, next task fixes the one remaining error" pattern
used in the previous attach-menu feature's plan. Confirm via:

```bash
cargo build 2>&1 | grep -A3 "error\["
```

Expected: exactly the missing-field/wrong-arity errors around
`src/term/mod.rs`'s `snapshot()` definition and its callers in
`src/daemon/mod.rs` — nothing else.

---

### Task 5: `ServerPane::snapshot(offset)` + `scrollback_rows()`

**Files:**
- Modify: `src/term/mod.rs`
- Test: `src/term/mod.rs` `mod tests` block

- [ ] **Step 1: Write the failing test for `scrollback_rows`**

Add to `src/term/mod.rs`'s `mod tests` block, near `resize_updates_size`:

```rust
    #[test]
    fn scrollback_rows_is_zero_for_a_fresh_pane() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let id = Uuid::new_v4();
        let pane =
            ServerPane::spawn(id, None, Some("cat".to_string()), Size { rows: 24, cols: 80 }, tx).unwrap();
        assert_eq!(pane.scrollback_rows(), 0);
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test scrollback_rows_is_zero_for_a_fresh_pane`
Expected: FAIL — `scrollback_rows` doesn't exist on `ServerPane` yet.

- [ ] **Step 3: Add `ServerPane::scrollback_rows`**

In `src/term/mod.rs`, add this method to `impl ServerPane`, right after
`size()` (which currently reads):

```rust
    pub fn size(&self) -> Size {
        self.inner.lock().unwrap().size
    }
```

Add immediately after:

```rust
    /// Rows of actual history available to scroll back into (excludes
    /// the live on-screen rows) -- the upper bound
    /// `daemon::state::State::scroll_server_pane` clamps against. NOT
    /// the same number as `wezterm_term::Screen::scrollback_rows()`,
    /// despite the matching name and this being a thin wrapper around
    /// it: that method returns the *total* buffered line count (visible
    /// rows + history -- a fresh pane with zero history still reports
    /// `physical_rows`, not 0), so `physical_rows` must be subtracted to
    /// get the actual scrollable depth.
    pub fn scrollback_rows(&self) -> usize {
        let guard = self.inner.lock().unwrap();
        let screen = guard.terminal.screen();
        screen.scrollback_rows().saturating_sub(screen.physical_rows)
    }
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test scrollback_rows_is_zero_for_a_fresh_pane`
Expected: PASS

- [ ] **Step 5: Write the failing test for `snapshot(offset)`**

Add to `src/term/mod.rs`'s `mod tests` block:

```rust
    #[test]
    fn snapshot_at_nonzero_offset_shows_scrolled_off_content() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let id = Uuid::new_v4();
        // A small 5-row pane makes it easy to scroll content off the
        // top with a modest number of printed lines.
        let pane =
            ServerPane::spawn(id, None, Some("cat".to_string()), Size { rows: 5, cols: 80 }, tx).unwrap();

        // Write enough lines to scroll "first-line" off the top of a
        // 5-row screen and into scrollback.
        for i in 0..20 {
            pane.write_input(format!("line-{i}\n").as_bytes()).unwrap();
        }
        let found = wait_until(&mut rx, || {
            pane.snapshot(0)
                .lines
                .iter()
                .map(|row| row.iter().map(|c| c.text.as_str()).collect::<String>())
                .collect::<Vec<_>>()
                .join("\n")
                .contains("line-19")
        });
        assert!(found, "expected the live view to eventually show the last written line");

        assert!(pane.scrollback_rows() > 0, "expected some scrollback to have accumulated");

        // At offset 0 (live), the most recent content should be
        // visible; at a nonzero offset, it should NOT be (we've
        // scrolled back away from it).
        let live_text: String = pane
            .snapshot(0)
            .lines
            .iter()
            .flat_map(|row| row.iter().map(|c| c.text.as_str()))
            .collect();
        assert!(live_text.contains("line-19"));

        let scrolled = pane.snapshot(pane.scrollback_rows());
        assert_eq!(scrolled.scroll_offset, pane.scrollback_rows());
        let scrolled_text: String = scrolled
            .lines
            .iter()
            .flat_map(|row| row.iter().map(|c| c.text.as_str()))
            .collect();
        assert!(
            !scrolled_text.contains("line-19"),
            "scrolled all the way back should show older content, not the latest line"
        );
    }
```

This reuses the existing `wait_until` test helper already defined in
this same `mod tests` block (used by `spawn_prints_output_and_dies` and
others) — no new helper needed.

- [ ] **Step 6: Run test to verify it fails**

Run: `cargo test snapshot_at_nonzero_offset_shows_scrolled_off_content`
Expected: FAIL — `snapshot` doesn't take an argument yet.

- [ ] **Step 7: Update `snapshot` to take an `offset` parameter**

In `src/term/mod.rs`, find the current `snapshot` method:

```rust
    pub fn snapshot(&self) -> GridSnapshot {
        let guard = self.inner.lock().unwrap();
        let screen = guard.terminal.screen();
        let rows = screen.physical_rows;
        let cols = screen.physical_cols;
        let phys_range = screen.phys_range(&(0..rows as i64));
        let lines = screen.lines_in_phys_range(phys_range);

        let mut grid_lines: Vec<Vec<Cell>> = lines.iter().map(|l| line_to_cells(l, cols)).collect();
        // Pad with blank rows if the screen somehow reports fewer
        // physical lines than its declared height, so `lines.len()`
        // always matches `size.rows` for callers.
        while grid_lines.len() < rows {
            grid_lines.push(vec![blank_cell(); cols]);
        }

        let cursor = guard.terminal.cursor_pos();
        GridSnapshot {
            server_pane: self.id,
            size: guard.size,
            // `(col, row)`, matching `CursorPosition`'s own `(x, y)`.
            cursor: (
                cursor.x.min(u16::MAX as usize) as u16,
                cursor.y.max(0).min(u16::MAX as i64) as u16,
            ),
            lines: grid_lines,
        }
    }
```

Replace with:

```rust
    /// `offset` shifts the returned window back from the live tail by
    /// that many rows (0 = today's exact live-view behavior, preserved
    /// unchanged for every caller not yet updated for scrolling).
    /// Clamping `offset` to a sane range is the *caller*'s
    /// responsibility (`daemon::state::State::scroll_server_pane`
    /// clamps against `scrollback_rows()` before this is ever called
    /// with a value from user input) -- this method trusts its input.
    pub fn snapshot(&self, offset: usize) -> GridSnapshot {
        let guard = self.inner.lock().unwrap();
        let screen = guard.terminal.screen();
        let rows = screen.physical_rows;
        let cols = screen.physical_cols;
        // Shift the window `offset` rows back from the live tail --
        // `scrollback_or_visible_range` (already provided by
        // `wezterm_term::Screen`) accepts a signed row range that can
        // dip into the scrollback buffer directly, so no new
        // row-indexing math is needed here beyond picking the right
        // start/end. At `offset == 0` this produces exactly the same
        // physical range `phys_range(&(0..rows))` did before this
        // parameter existed.
        let start = -(offset as i32);
        let end = start + rows as i32;
        let phys_range = screen.scrollback_or_visible_range(&(start..end));
        let lines = screen.lines_in_phys_range(phys_range);

        let mut grid_lines: Vec<Vec<Cell>> = lines.iter().map(|l| line_to_cells(l, cols)).collect();
        // Pad with blank rows if the screen somehow reports fewer
        // physical lines than its declared height, so `lines.len()`
        // always matches `size.rows` for callers.
        while grid_lines.len() < rows {
            grid_lines.push(vec![blank_cell(); cols]);
        }

        let cursor = guard.terminal.cursor_pos();
        GridSnapshot {
            server_pane: self.id,
            size: guard.size,
            // `(col, row)`, matching `CursorPosition`'s own `(x, y)`.
            cursor: (
                cursor.x.min(u16::MAX as usize) as u16,
                cursor.y.max(0).min(u16::MAX as i64) as u16,
            ),
            lines: grid_lines,
            scroll_offset: offset,
        }
    }
```

- [ ] **Step 8: Fix every existing call site in `src/term/mod.rs`'s own tests**

`grep -n "\.snapshot()" src/term/mod.rs` — every hit needs `.snapshot()`
changed to `.snapshot(0)` (preserves existing live-view behavior in
every pre-existing test). Do this now for every hit in this file.

- [ ] **Step 9: Run tests to verify they pass**

Run: `cargo test --lib term::`
Expected: all `term` module tests PASS, including the two new ones from
this task.

Run: `cargo build 2>&1 | grep -A3 "error\["`
Expected: errors now confined to `src/daemon/mod.rs` (its two
`.snapshot()` call sites, fixed in Task 7) — nothing in `src/term/mod.rs`
or `src/protocol.rs` anymore.

- [ ] **Step 10: Commit**

```bash
git add src/term/mod.rs
git commit -m "feat: add ServerPane::scrollback_rows and thread an offset through snapshot()"
```

---

### Task 6: `State::scroll_offsets` + `scroll_server_pane` + cleanup

**Files:**
- Modify: `src/daemon/state.rs`
- Test: `src/daemon/state.rs` `mod tests` block

- [ ] **Step 1: Write the failing tests**

Add to `src/daemon/state.rs`'s `mod tests` block:

```rust
    #[test]
    fn scroll_server_pane_clamps_at_zero() {
        let mut state = State::new();
        let info = state.server_spawn(None, Some("cat".to_string())).unwrap();
        // No scrollback yet on a freshly spawned pane -- scrolling back
        // (positive delta) has nothing to clamp against but 0.
        let offset = state.scroll_server_pane(1, info.id, 5);
        assert_eq!(offset, 0);
        assert_eq!(state.scroll_offsets.get(&(1, info.id)), None, "a zero offset should not be stored");
    }

    #[test]
    fn scroll_server_pane_absent_entry_defaults_to_zero_before_first_call() {
        let mut state = State::new();
        let info = state.server_spawn(None, Some("cat".to_string())).unwrap();
        // Scrolling *forward* (negative delta) from an implicit starting
        // offset of 0 must clamp at 0, not go negative.
        let offset = state.scroll_server_pane(1, info.id, -5);
        assert_eq!(offset, 0);
    }
```

Note: these two tests only exercise the zero-scrollback clamp cases,
since driving real scrollback accumulation requires actually writing
enough PTY output and waiting for the background reader thread to
process it (async timing `State`'s own synchronous unit tests aren't
set up for) — the "clamps against real accumulated scrollback" case is
covered by Task 5's `snapshot_at_nonzero_offset_shows_scrolled_off_content`
test (which does wait for real output) combined with Task 8's
integration test (which exercises the full request round-trip). This
task's tests only need to prove the *zero* boundary and the
absent-defaults-to-zero behavior, which are exactly the parts
`scroll_server_pane` itself is responsible for, independent of timing.

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test scroll_server_pane`
Expected: FAIL — `scroll_server_pane`/`scroll_offsets` don't exist yet.

- [ ] **Step 3: Add `scroll_offsets` field to `State`**

In `src/daemon/state.rs`, find the `State` struct:

```rust
pub struct State {
    server_panes: HashMap<ServerPaneId, ServerPane>,
    workspaces: HashMap<WorkspaceId, Workspace>,
    /// workspace -> set of subscribers currently viewing it. Drives both
    /// event broadcast and (via each workspace's bound server-panes)
    /// smallest-viewer-wins PTY sizing.
    subscribers: HashMap<WorkspaceId, Vec<SubscriberId>>,
    /// Last on-screen size reported for each client-pane
    /// (`Request::ResizeClientPane`). A pane absent from this map has no
    /// known size yet and is skipped when computing smallest-viewer-wins.
    client_pane_sizes: HashMap<ClientPaneId, Size>,
    /// Cloned into every [`ServerPane::spawn`] so its reader thread can
    /// report output/exit.
    pane_events: UnboundedSender<ServerPaneEvent>,
    pane_events_rx: Option<UnboundedReceiver<ServerPaneEvent>>,
}
```

Add a new field after `client_pane_sizes`:

```rust
pub struct State {
    server_panes: HashMap<ServerPaneId, ServerPane>,
    workspaces: HashMap<WorkspaceId, Workspace>,
    /// workspace -> set of subscribers currently viewing it. Drives both
    /// event broadcast and (via each workspace's bound server-panes)
    /// smallest-viewer-wins PTY sizing.
    subscribers: HashMap<WorkspaceId, Vec<SubscriberId>>,
    /// Last on-screen size reported for each client-pane
    /// (`Request::ResizeClientPane`). A pane absent from this map has no
    /// known size yet and is skipped when computing smallest-viewer-wins.
    client_pane_sizes: HashMap<ClientPaneId, Size>,
    /// Per-connection scroll position into a server-pane's history.
    /// Keyed by `(SubscriberId, ServerPaneId)`, NOT `ClientPaneId` --
    /// `GridSnapshot` carries only a `server_pane` id, so a connection
    /// can only ever see one grid per server-pane per broadcast
    /// regardless of how many client-panes it has bound to it; offset
    /// has to live at that same granularity to be deliverable over the
    /// wire at all (design doc "Scroll offset ownership" has the full
    /// rationale, including the one narrow case this can't represent:
    /// the same server-pane bound twice within one workspace shares a
    /// scroll position from that viewer's perspective). Absent means 0
    /// (live) -- same "absent = default" convention `client_pane_sizes`
    /// already uses, just keyed differently. A zero-result store is
    /// removed rather than kept as an explicit `0` entry, keeping that
    /// convention consistent both ways.
    scroll_offsets: HashMap<(SubscriberId, ServerPaneId), usize>,
    /// Cloned into every [`ServerPane::spawn`] so its reader thread can
    /// report output/exit.
    pane_events: UnboundedSender<ServerPaneEvent>,
    pane_events_rx: Option<UnboundedReceiver<ServerPaneEvent>>,
}
```

Update `State::new()` to initialize it — find:

```rust
    pub fn new() -> Self {
        let (pane_events, pane_events_rx) = tokio::sync::mpsc::unbounded_channel();
        Self {
            server_panes: HashMap::new(),
            workspaces: HashMap::new(),
```

Read further to find the rest of the literal (the tests you just wrote
call `State::new()` so this must compile before Step 2 can even fail
correctly, but that's fine, it fails on the missing method, not this) —
add `scroll_offsets: HashMap::new(),` to the same struct literal,
positioned to match the field order above (after `client_pane_sizes:
HashMap::new(),`).

- [ ] **Step 4: Add `scroll_server_pane`**

Add this method to `impl State`, near `resize_client_pane`:

```rust
    /// Adjust `subscriber`'s scroll position into `server_pane`'s
    /// history by `delta` rows (positive = further back into
    /// scrollback, negative = toward the live tail), clamped to
    /// `0..=server_pane.scrollback_rows()`. Returns the resulting
    /// (already-clamped) offset so the caller can build a `GridSnapshot`
    /// at exactly that offset without a second lookup. A `server_pane`
    /// that no longer exists (e.g. killed between the frontend sending
    /// the request and it arriving) clamps to 0 and stores nothing --
    /// nothing to scroll if there's no pane.
    pub fn scroll_server_pane(&mut self, subscriber: SubscriberId, server_pane: ServerPaneId, delta: i32) -> usize {
        let Some(pane) = self.server_panes.get(&server_pane) else {
            return 0;
        };
        let max = pane.scrollback_rows();
        let current = self
            .scroll_offsets
            .get(&(subscriber, server_pane))
            .copied()
            .unwrap_or(0);
        let new_offset = (current as i64 + delta as i64).clamp(0, max as i64) as usize;
        if new_offset == 0 {
            self.scroll_offsets.remove(&(subscriber, server_pane));
        } else {
            self.scroll_offsets.insert((subscriber, server_pane), new_offset);
        }
        new_offset
    }

    /// The offset a fresh `GridSnapshot` for `server_pane` should be
    /// built at for `subscriber` -- 0 if they've never scrolled it (or
    /// their prior scroll position was already cleaned up). Called by
    /// the `Subscribe` dispatch arm and by the broadcast-splitting logic
    /// in `daemon::mod`'s pane-event drain task.
    pub fn scroll_offset_for(&self, subscriber: SubscriberId, server_pane: ServerPaneId) -> usize {
        self.scroll_offsets.get(&(subscriber, server_pane)).copied().unwrap_or(0)
    }

    /// Remove every scroll-offset entry belonging to `subscriber` --
    /// called from `daemon::mod`'s `handle_connection` teardown path
    /// (alongside its existing `unsubscribe` call) once a connection
    /// closes, so a disconnected client's offsets don't linger in the
    /// map forever addressed by a `SubscriberId` nothing will ever reuse.
    pub fn clear_scroll_offsets_for_subscriber(&mut self, subscriber: SubscriberId) {
        self.scroll_offsets.retain(|(sub, _), _| *sub != subscriber);
    }
```

- [ ] **Step 5: Clear offsets on `server_kill`**

In `src/daemon/state.rs`, find `server_kill`:

```rust
    pub fn server_kill(&mut self, target: &str) -> anyhow::Result<()> {
        let id = self.resolve_server_pane(target)?;
        let mut pane = self
            .server_panes
            .remove(&id)
            .expect("resolve_server_pane only yields ids present in the pool");
        // The pane leaves the pool either way, so a kill error (typically
        // "process already exited") isn't worth failing the request over.
        let _ = pane.kill();
        // Design doc "Error handling": a killed server-pane leaves every
        // client-pane bound to it as an unbound placeholder — not closed,
        // not rebound.
        for ws in self.workspaces.values_mut() {
            if let Some(tree) = &mut ws.tree {
                unbind_all(tree, id);
            }
        }
        Ok(())
    }
```

Add one line right after the `unbind_all` loop, before `Ok(())`:

```rust
    pub fn server_kill(&mut self, target: &str) -> anyhow::Result<()> {
        let id = self.resolve_server_pane(target)?;
        let mut pane = self
            .server_panes
            .remove(&id)
            .expect("resolve_server_pane only yields ids present in the pool");
        // The pane leaves the pool either way, so a kill error (typically
        // "process already exited") isn't worth failing the request over.
        let _ = pane.kill();
        // Design doc "Error handling": a killed server-pane leaves every
        // client-pane bound to it as an unbound placeholder — not closed,
        // not rebound.
        for ws in self.workspaces.values_mut() {
            if let Some(tree) = &mut ws.tree {
                unbind_all(tree, id);
            }
        }
        // No point remembering a scroll position into a server-pane
        // that no longer exists.
        self.scroll_offsets.retain(|(_, sp), _| *sp != id);
        Ok(())
    }
```

- [ ] **Step 6: Run tests to verify they pass**

Run: `cargo test scroll_server_pane`
Expected: PASS (2 tests)

Run: `cargo build 2>&1 | grep -A3 "error\["`
Expected: errors confined to `src/daemon/mod.rs` (Task 7's job) — no
errors remaining in `state.rs`.

- [ ] **Step 7: Commit**

```bash
git add src/daemon/state.rs
git commit -m "feat: add per-subscriber scroll offset storage to State"
```

---

### Task 7: Daemon dispatch — `ScrollClientPane` arm + broadcast-splitting + `Subscribe`/teardown updates

**Files:**
- Modify: `src/daemon/mod.rs`
- Test: `src/daemon/mod.rs` `mod tests` block

This is the task the design spec flags as the riskiest — take care here,
and re-read the design doc's "Scroll offset ownership" and the
`broadcast_grid_prepare` section before starting if anything below is
unclear.

- [ ] **Step 1: Fix `Subscribe`'s snapshot calls to pass the subscriber's recorded offset**

In `src/daemon/mod.rs`, find the `Subscribe` dispatch arm:

```rust
        Request::Subscribe { workspace } => {
            let mut state = state.lock().await;
            let ws_id = match state.resolve_or_create_workspace(&workspace) {
                Ok(id) => id,
                Err(err) => return Response::Error { message: err.to_string() },
            };
            state.subscribe(subscriber_id, ws_id);
            *subscribed_workspace = Some(ws_id);
            let info = match state.workspace_info(ws_id) {
                Ok(info) => info,
                Err(err) => return Response::Error { message: err.to_string() },
            };
            let grids = info
                .tree
                .as_ref()
                .map(|tree| {
                    tree.leaves()
                        .into_iter()
                        .filter_map(|leaf| leaf.bound)
                        .filter_map(|bound| state.server_pane(bound))
                        .map(|pane| pane.snapshot())
                        .collect()
                })
                .unwrap_or_default();
            Response::Snapshot { workspace: info, grids }
        }
```

Replace the `grids` computation:

```rust
        Request::Subscribe { workspace } => {
            let mut state = state.lock().await;
            let ws_id = match state.resolve_or_create_workspace(&workspace) {
                Ok(id) => id,
                Err(err) => return Response::Error { message: err.to_string() },
            };
            state.subscribe(subscriber_id, ws_id);
            *subscribed_workspace = Some(ws_id);
            let info = match state.workspace_info(ws_id) {
                Ok(info) => info,
                Err(err) => return Response::Error { message: err.to_string() },
            };
            let grids = info
                .tree
                .as_ref()
                .map(|tree| {
                    tree.leaves()
                        .into_iter()
                        .filter_map(|leaf| leaf.bound)
                        .filter_map(|bound| {
                            let pane = state.server_pane(bound)?;
                            let offset = state.scroll_offset_for(subscriber_id, bound);
                            Some(pane.snapshot(offset))
                        })
                        .collect()
                })
                .unwrap_or_default();
            Response::Snapshot { workspace: info, grids }
        }
```

Note: `filter_map`'s closure now needs two sequential `state` reads
(`server_pane` then `scroll_offset_for`) instead of one — both are
plain `&self` methods on the already-locked `state` `MutexGuard`
in scope, so this is just two method calls in sequence, no new locking
or borrow-checker conflict.

- [ ] **Step 2: Update `handle_connection`'s teardown to clear scroll offsets**

In `src/daemon/mod.rs`, find `handle_connection`'s teardown (near the
end of the function):

```rust
    push_task.abort();
    registry.lock().await.remove(&subscriber_id);
    if let Some(ws) = subscribed_workspace {
        state.lock().await.unsubscribe(subscriber_id, ws);
    }
}
```

Replace with:

```rust
    push_task.abort();
    registry.lock().await.remove(&subscriber_id);
    if let Some(ws) = subscribed_workspace {
        state.lock().await.unsubscribe(subscriber_id, ws);
    }
    state.lock().await.clear_scroll_offsets_for_subscriber(subscriber_id);
}
```

- [ ] **Step 3: Add the `ScrollClientPane` dispatch arm**

In `src/daemon/mod.rs`, find the `ResizeClientPane` dispatch arm:

```rust
        Request::ResizeClientPane { pane, size } => {
            let mut guard = state.lock().await;
            let affected = guard.bound_server_pane(pane);
            guard.resize_client_pane(pane, size);
            let prepared = affected.and_then(|server_pane| broadcast_grid_prepare(&guard, server_pane));
            // Drop the lock before the expensive serialize+push -- see
            // `broadcast_grid_prepare`'s doc comment.
            drop(guard);
            if let Some(broadcast) = prepared {
                broadcast_grid_send(registry, broadcast).await;
            }
            Response::Ack
        }
```

Add a new arm right after it (leave `ResizeClientPane` itself
unmodified for now — `broadcast_grid_prepare`'s signature changes in
Step 4, and this arm's call site gets fixed together with that step,
not here):

```rust
        Request::ScrollClientPane { pane, delta } => {
            let mut guard = state.lock().await;
            let Some(server_pane) = guard.bound_server_pane(pane) else {
                // Unbound target -- see the Request::ScrollClientPane
                // doc comment in protocol.rs for why this is a silent
                // Ack, not an Error.
                return Response::Ack;
            };
            guard.scroll_server_pane(subscriber_id, server_pane, delta);
            let prepared = broadcast_grid_prepare(&guard, server_pane);
            drop(guard);
            if let Some(broadcast) = prepared {
                broadcast_grid_send(registry, broadcast).await;
            }
            Response::Ack
        }
```

- [ ] **Step 4: Restructure `GridBroadcast`/`broadcast_grid_prepare` to support multiple offset groups**

This is the core of the task. In `src/daemon/mod.rs`, find:

```rust
struct GridBroadcast {
    event: ServerMessage,
    subscribers: Vec<SubscriberId>,
}

/// The cheap half of a grid broadcast: check whether anyone is even
/// subscribed to `server_pane`, and if so, copy its current grid.
///
/// Must be called with the state lock held (it needs `state.server_pane`/
/// `subscribers_for_server_pane`), but is itself fast — `ServerPane::
/// snapshot()` is a plain cell-grid walk-and-clone, measured at ~2ms for
/// a 50x200 pane. Returns `None` immediately, without touching the grid
/// at all, if nobody is subscribed — a pane nobody is currently viewing
/// (e.g. producing background scroll in an unwatched workspace) should
/// cost nothing, not even a wasted snapshot.
///
/// Deliberately does NOT serialize the snapshot to `ServerMessage`'s
/// wire bytes here — that's `broadcast_grid_send`'s job, called only
/// after the caller has released the state lock. Serializing a full grid
/// to JSON is the actually expensive step (~22ms measured for the same
/// 50x200 pane, roughly 10x the snapshot itself) — doing it while every
/// other request in the daemon is blocked on the same global lock is
/// what caused dimux to visibly freeze (stop responding to keystrokes in
/// any other pane) whenever a watched pane produced output rapidly
/// enough (e.g. an animated startup banner).
fn broadcast_grid_prepare(state: &State, server_pane: ServerPaneId) -> Option<GridBroadcast> {
    let subscribers = state.subscribers_for_server_pane(server_pane);
    if subscribers.is_empty() {
        return None;
    }
    let pane = state.server_pane(server_pane)?;
    let event = ServerMessage::Event(Event::GridDelta { snapshot: pane.snapshot() });
    Some(GridBroadcast { event, subscribers })
}

/// The expensive half: serialize and push. Call this with the state lock
/// already released — see [`broadcast_grid_prepare`]'s doc comment for
/// why the split exists.
async fn broadcast_grid_send(registry: &SubscriberRegistry, broadcast: GridBroadcast) {
    push_to_subscribers(registry, &broadcast.subscribers, &broadcast.event).await;
}
```

Replace with:

```rust
struct GridBroadcast {
    /// One entry per distinct scroll offset currently in use among
    /// `server_pane`'s subscribers -- in the overwhelmingly common case
    /// (nobody scrolled back) this has exactly one entry, matching the
    /// pre-scrollback broadcast's cost exactly. See module doc "PTY
    /// sizing"/this struct's origin in `broadcast_grid_prepare`'s doc
    /// comment for why grouping happens here rather than serializing
    /// once per subscriber (would multiply the expensive JSON-serialize
    /// step by subscriber count instead of by distinct-offset count).
    groups: Vec<(ServerMessage, Vec<SubscriberId>)>,
}

/// The cheap half of a grid broadcast: check whether anyone is even
/// subscribed to `server_pane`, and if so, copy its current grid --
/// once per *distinct scroll offset* among its subscribers, not once
/// per subscriber (see [`GridBroadcast`]'s doc comment).
///
/// Must be called with the state lock held (it needs `state.server_pane`/
/// `subscribers_for_server_pane`/`scroll_offset_for`), but is itself
/// fast — `ServerPane::snapshot()` is a plain cell-grid walk-and-clone,
/// measured at ~2ms for a 50x200 pane. Returns `None` immediately,
/// without touching the grid at all, if nobody is subscribed — a pane
/// nobody is currently viewing (e.g. producing background scroll in an
/// unwatched workspace) should cost nothing, not even a wasted snapshot.
///
/// Deliberately does NOT serialize the snapshot to `ServerMessage`'s
/// wire bytes here — that's `broadcast_grid_send`'s job, called only
/// after the caller has released the state lock. Serializing a full grid
/// to JSON is the actually expensive step (~22ms measured for the same
/// 50x200 pane, roughly 10x the snapshot itself) — doing it while every
/// other request in the daemon is blocked on the same global lock is
/// what caused dimux to visibly freeze (stop responding to keystrokes in
/// any other pane) whenever a watched pane produced output rapidly
/// enough (e.g. an animated startup banner).
fn broadcast_grid_prepare(state: &State, server_pane: ServerPaneId) -> Option<GridBroadcast> {
    let subscribers = state.subscribers_for_server_pane(server_pane);
    if subscribers.is_empty() {
        return None;
    }
    let pane = state.server_pane(server_pane)?;

    // Group subscribers by their current offset into this server-pane.
    let mut by_offset: HashMap<usize, Vec<SubscriberId>> = HashMap::new();
    for sub in subscribers {
        let offset = state.scroll_offset_for(sub, server_pane);
        by_offset.entry(offset).or_default().push(sub);
    }

    let groups = by_offset
        .into_iter()
        .map(|(offset, subs)| {
            let event = ServerMessage::Event(Event::GridDelta { snapshot: pane.snapshot(offset) });
            (event, subs)
        })
        .collect();
    Some(GridBroadcast { groups })
}

/// The expensive half: serialize and push each group. Call this with the
/// state lock already released — see [`broadcast_grid_prepare`]'s doc
/// comment for why the split exists.
async fn broadcast_grid_send(registry: &SubscriberRegistry, broadcast: GridBroadcast) {
    for (event, subscribers) in &broadcast.groups {
        push_to_subscribers(registry, subscribers, event).await;
    }
}
```

`HashMap` is already imported at the top of `src/daemon/mod.rs` (used
by `SubscriberRegistry`'s type alias) — no new import needed.

- [ ] **Step 5: Run the full test suite**

Run: `cargo build 2>&1 | grep -A3 "error\["`
Expected: no output (clean build) — every `.snapshot()` call site in
the whole crate should now be fixed (Task 5 fixed `term/mod.rs`'s own
tests; this task fixed `daemon/mod.rs`'s two real call sites).

Run: `cargo test`
Expected: all tests PASS.

- [ ] **Step 6: Commit**

```bash
git add src/daemon/mod.rs
git commit -m "feat: add ScrollClientPane dispatch + split grid broadcasts by scroll offset"
```

---

### Task 8: Integration test for the full scroll round-trip

**Files:**
- Modify: `src/daemon/mod.rs` `mod tests` block

- [ ] **Step 1: Write the test**

Add to `src/daemon/mod.rs`'s `mod tests` block, after the test added in
Task 3:

```rust
    /// Two connections subscribed to the same workspace/server-pane;
    /// one scrolls back, the other doesn't. This is the test that
    /// actually exercises `broadcast_grid_prepare`'s offset-grouping
    /// logic (Task 7) -- each connection's own subsequent `GridDelta`s
    /// must reflect its own scroll position, independent of the other's.
    #[tokio::test]
    async fn scroll_offset_is_independent_per_subscriber() {
        let guard = start_daemon().await;

        let mut owner = TestConn::connect(&guard.0).await;
        let server_pane = match owner
            .request(Request::ServerSpawn { name: None, cmd: Some("cat".to_string()) })
            .await
        {
            Response::ServerPane(info) => info.id,
            other => panic!("expected ServerPane, got {other:?}"),
        };
        // Small pane so a modest number of writes produces real
        // scrollback quickly.
        let (workspace, pane) = match owner
            .request(Request::ClientSpawn {
                workspace: "1".to_string(),
                split_of: None,
                dir: None,
                bind: Some(server_pane.to_string()),
            })
            .await
        {
            Response::ClientPaneCreated { workspace, pane } => (workspace, pane),
            other => panic!("expected ClientPaneCreated, got {other:?}"),
        };
        match owner
            .request(Request::ResizeClientPane { pane, size: Size { rows: 5, cols: 80 } })
            .await
        {
            Response::Ack => {}
            other => panic!("expected Ack, got {other:?}"),
        }

        let mut scroller = TestConn::connect(&guard.0).await;
        let mut watcher = TestConn::connect(&guard.0).await;
        for conn in [&mut scroller, &mut watcher] {
            match conn.request(Request::Subscribe { workspace: workspace.to_string() }).await {
                Response::Snapshot { .. } => {}
                other => panic!("expected Snapshot, got {other:?}"),
            }
        }

        // Produce enough output to build real scrollback on a 5-row pane.
        for i in 0..20 {
            match owner
                .request(Request::Input { pane, bytes: format!("line-{i}\n").into_bytes() })
                .await
            {
                Response::Ack => {}
                other => panic!("expected Ack, got {other:?}"),
            }
        }
        // Drain whatever GridDelta events have already landed for both
        // subscribers so the assertions below only see events produced
        // *after* the scroll request, not stale ones from the writes
        // above racing this drain.
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while std::time::Instant::now() < deadline {
            if scroller.read_event(Duration::from_millis(50)).await.is_none() {
                break;
            }
        }
        while std::time::Instant::now() < deadline {
            if watcher.read_event(Duration::from_millis(50)).await.is_none() {
                break;
            }
        }

        // `scroller` scrolls back; `watcher` does nothing.
        match scroller.request(Request::ScrollClientPane { pane, delta: 3 }).await {
            Response::Ack => {}
            other => panic!("expected Ack, got {other:?}"),
        }

        // Trigger a fresh broadcast for both by writing more output.
        match owner.request(Request::Input { pane, bytes: b"more\n".to_vec() }).await {
            Response::Ack => {}
            other => panic!("expected Ack, got {other:?}"),
        }

        let scroller_event = scroller
            .read_event(Duration::from_secs(2))
            .await
            .expect("expected a GridDelta for the scrolled-back connection");
        let watcher_event = watcher
            .read_event(Duration::from_secs(2))
            .await
            .expect("expected a GridDelta for the live connection");

        match scroller_event {
            Event::GridDelta { snapshot } => {
                assert!(snapshot.scroll_offset > 0, "scroller's snapshot should reflect its scrolled offset");
            }
            other => panic!("expected GridDelta, got {other:?}"),
        }
        match watcher_event {
            Event::GridDelta { snapshot } => {
                assert_eq!(snapshot.scroll_offset, 0, "watcher never scrolled -- should stay live");
            }
            other => panic!("expected GridDelta, got {other:?}"),
        }
    }
```

This test relies on `TestConn::read_event(&mut self, timeout: Duration)
-> Option<Event>` (already exists, used by
`client_spawn_broadcasts_layout_delta_to_subscribers` and others in
this same file — verified its exact behavior: returns `None` on
timeout, `Some(event)` on an `Event` frame, and panics if a `Response`
frame arrives instead — the drain loop above's `is_none() { break; }`
check is exactly correct against this).

- [ ] **Step 2: Run the test**

Run: `cargo test scroll_offset_is_independent_per_subscriber`
Expected: PASS. If it's flaky (timing-sensitive drain logic), increase
the drain deadline or read `TestConn::read_event`'s exact semantics more
carefully and adjust — do not just retry blindly; understand why it's
flaking first (systematic debugging, not guessing).

- [ ] **Step 3: Run the full suite**

Run: `cargo test`
Expected: all tests PASS.

- [ ] **Step 4: Commit**

```bash
git add src/daemon/mod.rs
git commit -m "test: add integration coverage for per-subscriber scroll offset independence"
```

---

### Task 9: Frontend — mouse-wheel `MouseEvent` variants

**Files:**
- Modify: `src/tui/mouse.rs`
- Test: `src/tui/mouse.rs` `mod tests` block

- [ ] **Step 1: Update the existing test that will now fail on purpose**

`src/tui/mouse.rs`'s `mod tests` currently has:

```rust
    #[test]
    fn scroll_events_are_recognized_and_ignored() {
        assert_eq!(parse(b"\x1b[<64;5;5M"), ParsedInput::Ignored); // scroll up
        assert_eq!(parse(b"\x1b[<65;5;5M"), ParsedInput::Ignored); // scroll down
    }
```

This assertion is about to become wrong on purpose — scroll events stop
being `Ignored` and become real `Mouse` events. Replace this test with:

```rust
    #[test]
    fn scroll_up_and_down_are_recognized_as_mouse_events() {
        assert_eq!(
            parse(b"\x1b[<64;5;5M"),
            ParsedInput::Mouse(MouseEvent::ScrollUp { col: 4, row: 4 })
        );
        assert_eq!(
            parse(b"\x1b[<65;5;5M"),
            ParsedInput::Mouse(MouseEvent::ScrollDown { col: 4, row: 4 })
        );
    }
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test scroll_up_and_down_are_recognized_as_mouse_events`
Expected: FAIL — `MouseEvent::ScrollUp`/`ScrollDown` don't exist yet.

- [ ] **Step 3: Add the two new `MouseEvent` variants**

In `src/tui/mouse.rs`, find:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseEvent {
    Down { col: u16, row: u16 },
    Drag { col: u16, row: u16 },
    Up { col: u16, row: u16 },
}
```

Replace with:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseEvent {
    Down { col: u16, row: u16 },
    Drag { col: u16, row: u16 },
    Up { col: u16, row: u16 },
    /// One wheel-tick scrolling back into a pane's history.
    ScrollUp { col: u16, row: u16 },
    /// One wheel-tick scrolling toward a pane's live tail.
    ScrollDown { col: u16, row: u16 },
}
```

- [ ] **Step 4: Update `parse`'s button-number handling**

Find the button-decode block in `parse`:

```rust
    // Bit layout (low to high): button lo, button hi, shift, alt,
    // control, dragging, button lo, button hi -- see module doc
    // reference. Only button number 0 (left) with dragging=0 is a
    // press/release; dragging=1 with button 0 is a drag. Any other
    // button number (middle, right, or the higher values used for
    // scroll/bare-movement encoding) is a recognized SGR mouse sequence
    // dimux simply doesn't act on.
    let button_number = (cb & 0b0000_0011) | ((cb & 0b1100_0000) >> 4);
    let dragging = cb & 0b0010_0000 != 0;
    if button_number != 0 {
        return ParsedInput::Ignored;
    }

    ParsedInput::Mouse(if released {
        MouseEvent::Up { col, row }
    } else if dragging {
        MouseEvent::Drag { col, row }
    } else {
        MouseEvent::Down { col, row }
    })
}
```

Replace with:

```rust
    // Bit layout (low to high): button lo, button hi, shift, alt,
    // control, dragging, button lo, button hi -- see module doc
    // reference. Button number 0 (left) with dragging=0 is a
    // press/release; dragging=1 with button 0 is a drag. Button numbers
    // 4/5 are the scroll wheel (up/down) -- `Cb=64`/`65` on the wire
    // decode to button_number 4/5 via this same bit layout, always
    // reported with dragging=0 (the wheel has no "held" state the way a
    // mouse button does, so `released`/`dragging` are meaningless for
    // these two button numbers and simply ignored). Any other button
    // number (middle, right, or the higher values used for
    // bare-movement encoding) is a recognized SGR mouse sequence dimux
    // simply doesn't act on.
    let button_number = (cb & 0b0000_0011) | ((cb & 0b1100_0000) >> 4);
    let dragging = cb & 0b0010_0000 != 0;

    if button_number == 4 {
        return ParsedInput::Mouse(MouseEvent::ScrollUp { col, row });
    }
    if button_number == 5 {
        return ParsedInput::Mouse(MouseEvent::ScrollDown { col, row });
    }
    if button_number != 0 {
        return ParsedInput::Ignored;
    }

    ParsedInput::Mouse(if released {
        MouseEvent::Up { col, row }
    } else if dragging {
        MouseEvent::Drag { col, row }
    } else {
        MouseEvent::Down { col, row }
    })
}
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test scroll_up_and_down_are_recognized_as_mouse_events`
Expected: PASS

Run: `cargo test --lib tui::mouse::`
Expected: all mouse module tests PASS (confirm
`right_and_middle_buttons_are_recognized_and_ignored` and
`bare_movement_with_no_button_is_recognized_and_ignored` still pass
unchanged — button numbers 1/2/3 are untouched by this change).

- [ ] **Step 6: Run the full test suite**

Run: `cargo build 2>&1 | grep -A3 "error\["`
Expected: an exhaustiveness error somewhere `MouseEvent` is matched
without a wildcard arm — this is expected, Task 10 fixes
`App::handle_mouse`'s match. Confirm the error is confined to
`src/tui/mod.rs`.

- [ ] **Step 7: Commit**

```bash
git add src/tui/mouse.rs
git commit -m "feat: recognize mouse-wheel scroll as real MouseEvents instead of discarding them"
```

---

### Task 10: Frontend — wire scroll events into `App::handle_mouse`

**Files:**
- Modify: `src/tui/mod.rs`

- [ ] **Step 1: Add the `SCROLL_ROWS_PER_TICK` constant and the two new match arms**

In `src/tui/mod.rs`, find `handle_mouse`:

```rust
    async fn handle_mouse(
        &mut self,
        event: mouse::MouseEvent,
        write_half: &mut OwnedWriteHalf,
        reader: &mut FrameReader,
    ) -> anyhow::Result<()> {
        match event {
            mouse::MouseEvent::Down { col, row } => {
```

Add a module-level constant right before the `App` `impl` block (or
near `QUIT_BYTE`, matching the existing convention of small named
constants living near the top of the file):

```rust
/// Rows scrolled per mouse-wheel tick. Arbitrary but
/// conventional-feeling default -- tune freely, no protocol
/// implications either way since `Request::ScrollClientPane::delta` is
/// already an arbitrary signed `i32`.
const SCROLL_ROWS_PER_TICK: i32 = 3;
```

Then update `handle_mouse`'s match to add the two new arms. Find the
full current match (ends with the `Up { .. }` arm) and add two more
arms right after `Down`, before `Drag`:

```rust
        match event {
            mouse::MouseEvent::Down { col, row } => {
                let Some(tree) = &self.workspace.tree else { return Ok(()) };
                let hits = render::divider_rects(tree, self.frame_area);
                self.dragging_split = hit_test_dividers(&hits, col, row).map(|split| {
                    let hit = hits.iter().find(|h| h.split == split).expect(
                        "hit_test_dividers only returns a split id that came from this exact `hits` list",
                    );
                    (split, render::ratio_at(hit, col, row))
                });
            }
            mouse::MouseEvent::ScrollUp { col, row } => {
                self.scroll_pane_under(col, row, SCROLL_ROWS_PER_TICK, write_half, reader).await?;
            }
            mouse::MouseEvent::ScrollDown { col, row } => {
                self.scroll_pane_under(col, row, -SCROLL_ROWS_PER_TICK, write_half, reader).await?;
            }
            mouse::MouseEvent::Drag { col, row } => {
```

(Leave `Drag` and everything after it exactly as it already is — only
inserting the two new arms above it.)

- [ ] **Step 2: Add the `scroll_pane_under` helper method**

Add this method to `impl App`, right after `handle_mouse` (which ends
with its closing `Ok(())` and `}`):

```rust
    /// Hit-test `(col, row)` against the current workspace's leaves and
    /// send `Request::ScrollClientPane` for whichever one it landed
    /// over, if any -- **not** necessarily the focused pane, matching
    /// ordinary terminal-multiplexer feel (the wheel scrolls whatever's
    /// under the cursor, regardless of focus). A hit over empty space
    /// (a divider, or no leaf at all) is a no-op; a hit over an unbound
    /// leaf still sends the request (the daemon itself no-ops on an
    /// unbound target -- see `Request::ScrollClientPane`'s doc comment
    /// in `protocol.rs` for why that's a silent `Ack`, not something
    /// this frontend needs to pre-filter).
    async fn scroll_pane_under(
        &mut self,
        col: u16,
        row: u16,
        delta: i32,
        write_half: &mut OwnedWriteHalf,
        reader: &mut FrameReader,
    ) -> anyhow::Result<()> {
        let Some(tree) = &self.workspace.tree else { return Ok(()) };
        let hit = render::leaf_rects(tree, self.frame_area)
            .into_iter()
            .find(|(_, rect)| {
                col >= rect.x && col < rect.x + rect.width && row >= rect.y && row < rect.y + rect.height
            });
        let Some((pane, _)) = hit else { return Ok(()) };
        let req = Request::ScrollClientPane { pane, delta };
        let _ = self.request(write_half, reader, req).await?;
        Ok(())
    }
```

- [ ] **Step 2: Run the full test suite**

Run: `cargo build 2>&1 | grep -A3 "error\["`
Expected: no output (clean build).

Run: `cargo test`
Expected: all tests PASS.

- [ ] **Step 3: Commit**

```bash
git add src/tui/mod.rs
git commit -m "feat: wire mouse-wheel scroll events to Request::ScrollClientPane"
```

---

### Task 11: Frontend — render the `[scrollback]` indicator

**Files:**
- Modify: `src/tui/render.rs`
- Test: `src/tui/render.rs` `mod tests` block

- [ ] **Step 1: Write the failing test**

Add to `src/tui/render.rs`'s `mod tests` block, near the other
`draw_*`/`grid_to_text` tests:

```rust
    #[test]
    fn draw_leaf_shows_scrollback_indicator_when_scrolled() {
        let pane_id = Uuid::new_v4();
        let server_id = Uuid::new_v4();
        let pane = ClientPane { id: pane_id, name: Some("shell".to_string()), bound: Some(server_id) };
        let mut grids = HashMap::new();
        grids.insert(
            server_id,
            GridSnapshot {
                server_pane: server_id,
                size: Size { rows: 5, cols: 20 },
                cursor: (0, 0),
                lines: vec![vec![]; 5],
                scroll_offset: 3,
            },
        );
        let backend = TestBackend::new(20, 6);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                draw_leaf(frame, &pane, frame.area(), &grids, None);
            })
            .unwrap();
        assert!(buffer_contains(&terminal, "scrollback"));
    }

    #[test]
    fn draw_leaf_shows_no_scrollback_indicator_when_live() {
        let pane_id = Uuid::new_v4();
        let server_id = Uuid::new_v4();
        let pane = ClientPane { id: pane_id, name: Some("shell".to_string()), bound: Some(server_id) };
        let mut grids = HashMap::new();
        grids.insert(
            server_id,
            GridSnapshot {
                server_pane: server_id,
                size: Size { rows: 5, cols: 20 },
                cursor: (0, 0),
                lines: vec![vec![]; 5],
                scroll_offset: 0,
            },
        );
        let backend = TestBackend::new(20, 6);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                draw_leaf(frame, &pane, frame.area(), &grids, None);
            })
            .unwrap();
        assert!(!buffer_contains(&terminal, "scrollback"));
    }
```

Check whether `draw_leaf` is currently `fn` (private) or `pub(...)` —
since this test lives in the same file's `mod tests`, private
visibility already suffices (same reasoning as the attach-menu
feature's `pub(super)` investigation) — no visibility change needed
either way, just confirm by reading the function's current signature
before assuming.

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test draw_leaf_shows`
Expected: FAIL — `"scrollback"` never appears (the indicator doesn't
exist yet), so the first test fails; the second should already pass
trivially (nothing to find). Confirm this is indeed the failure mode
before proceeding (if BOTH fail to compile instead, check your
`GridSnapshot` literal's field list matches Task 4's updated struct
shape exactly — it needs `scroll_offset` now).

- [ ] **Step 3: Add the indicator to `draw_leaf`**

In `src/tui/render.rs`, find `draw_leaf`:

```rust
fn draw_leaf(
    frame: &mut Frame,
    pane: &ClientPane,
    area: Rect,
    grids: &HashMap<ServerPaneId, GridSnapshot>,
    focused: Option<crate::protocol::ClientPaneId>,
) {
    let title = pane
        .name
        .clone()
        .unwrap_or_else(|| short_id(pane.id));
    let is_focused = focused == Some(pane.id);
    let border_style = if is_focused {
        Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD)
    } else {
        Style::new()
    };
    // Top-only border (a title bar), not a full box -- see module doc
    // "Bezels: shared dividers instead of per-pane boxes".
    let block = Block::default()
        .borders(Borders::TOP)
        .title(title)
        .border_style(border_style);

    match pane.bound {
        None => {
            let placeholder =
                Paragraph::new("(unbound — bind via `dimux client bind`)").block(block);
            frame.render_widget(placeholder, area);
        }
        Some(server_pane_id) => match grids.get(&server_pane_id) {
            Some(snapshot) => {
                let text = grid_to_text(snapshot);
                frame.render_widget(Paragraph::new(text).block(block), area);
            }
            None => {
                let placeholder = Paragraph::new("(server-pane closed)").block(block);
                frame.render_widget(placeholder, area);
            }
        },
    }
}
```

The scroll indicator depends on the *bound server-pane's snapshot*,
which isn't known yet at the point the `title`/`block` are built (they
come first). Restructure so the snapshot lookup happens before the
title is finalized:

```rust
fn draw_leaf(
    frame: &mut Frame,
    pane: &ClientPane,
    area: Rect,
    grids: &HashMap<ServerPaneId, GridSnapshot>,
    focused: Option<crate::protocol::ClientPaneId>,
) {
    let snapshot = pane.bound.and_then(|server_pane_id| grids.get(&server_pane_id));
    let mut title = pane
        .name
        .clone()
        .unwrap_or_else(|| short_id(pane.id));
    // A pane currently showing history rather than the live tail gets a
    // visible marker -- there is otherwise no indication a pane is
    // scrolled back at all (no scrollbar widget in v1, see design doc
    // "Frontend changes" for why that's an accepted, deliberate gap).
    if snapshot.is_some_and(|s| s.scroll_offset > 0) {
        title.push_str(" [scrollback]");
    }
    let is_focused = focused == Some(pane.id);
    let border_style = if is_focused {
        Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD)
    } else {
        Style::new()
    };
    // Top-only border (a title bar), not a full box -- see module doc
    // "Bezels: shared dividers instead of per-pane boxes".
    let block = Block::default()
        .borders(Borders::TOP)
        .title(title)
        .border_style(border_style);

    match pane.bound {
        None => {
            let placeholder =
                Paragraph::new("(unbound — bind via `dimux client bind`)").block(block);
            frame.render_widget(placeholder, area);
        }
        Some(_) => match snapshot {
            Some(snapshot) => {
                let text = grid_to_text(snapshot);
                frame.render_widget(Paragraph::new(text).block(block), area);
            }
            None => {
                let placeholder = Paragraph::new("(server-pane closed)").block(block);
                frame.render_widget(placeholder, area);
            }
        },
    }
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test draw_leaf_shows`
Expected: PASS (2 tests)

- [ ] **Step 5: Run the full test suite**

Run: `cargo build 2>&1 | grep -A3 "error\["`
Expected: no output (clean build).

Run: `cargo test`
Expected: all tests PASS.

Run: `cargo clippy --all-targets 2>&1 | tail -30`
Expected: no new warnings.

- [ ] **Step 6: Commit**

```bash
git add src/tui/render.rs
git commit -m "feat: show a [scrollback] indicator on panes viewing history"
```

---

### Task 12: Full-suite verification + manual test checklist

**Files:** none (verification only)

- [ ] **Step 1: Full clean build and test run**

Run: `cargo build --release 2>&1 | tail -10`
Expected: clean build.

Run: `cargo test 2>&1 | grep "test result"`
Expected: all three test binaries report `ok`, 0 failed.

Run: `cargo clippy --all-targets 2>&1 | tail -30`
Expected: zero warnings.

- [ ] **Step 2: Manual verification (requires a real Kitty terminal — cannot be automated)**

```bash
cargo install --path .
```

Then in Kitty:
1. `dimux attach`, split into 2+ panes of different sizes (some tall,
   some wide, at least one larger than 24 rows / 80 cols).
2. Run a command that fills the pane's height in each (e.g. `seq 1 100`
   in a shell) — confirm output fills the ENTIRE visible pane area, not
   just a 24-row corner with blank space below/beside it. This is the
   direct verification of the resize fix.
3. Resize the actual Kitty window — confirm panes visibly reflow AND
   their content re-wraps/re-fills the new size within a frame or two
   (not stuck at the old size).
4. In a pane running something with lots of scrollable output (e.g.
   `seq 1 200`), scroll the mouse wheel up over that pane — confirm
   older content appears and the pane's title bar shows `[scrollback]`.
   Scroll down — confirm it returns toward live output and the
   indicator disappears once fully back at the tail.
5. While scrolled back in one pane, type into a *different* focused
   pane — confirm the scrolled pane's history view doesn't get
   disturbed by unrelated activity (matches "stay scrolled, new output
   queues below").
6. Scroll the mouse wheel over a pane that is NOT the focused one —
   confirm it scrolls that pane, not the focused one (matches "scroll
   whatever's under the cursor").

- [ ] **Step 3: Report results**

If all manual checks pass, this plan is complete. If any fail, use
`superpowers:systematic-debugging` before attempting a fix — do not
guess at a patch without first reproducing and understanding exactly
which step in the design (resize wiring, offset storage, broadcast
splitting, or frontend hit-testing) is behaving differently from the
spec's description.
