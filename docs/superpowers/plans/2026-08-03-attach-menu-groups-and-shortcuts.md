# Attach Menu: cwd Grouping, Delete, and Rename Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Extend the existing `cmd-shift-z` attach menu so its server-pane
rows are grouped by working directory under header lines, and add an
arm/confirm delete (`x`, `x`) and an inline full-line-editing rename (`r`)
— all within the menu's existing selection model, no new keybind.

**Architecture:** `tui::render::draw_attach_menu` groups an already-sorted
`&[ServerPaneInfo]` into header + indented-row output; the sort itself
lives in `tui::mod` (a pure helper, unit-tested independent of rendering)
and runs every time `AttachMenu.servers` is (re)fetched. `AttachMenu` gains
two new optional sub-states (`pending_delete`, `rename`) that
`handle_attach_menu_input` checks before falling through to the existing
browse-mode dispatch table, so browsing/deleting/renaming are three
mutually exclusive modes layered on one struct rather than three separate
menus.

**Tech Stack:** Rust, tokio, ratatui — no new dependencies.

---

## File structure

- **Modify `src/tui/mod.rs`**: `AttachMenu` struct gains `pending_delete`/
  `rename` fields + a new `RenameState` struct; a new pure
  `group_servers_by_cwd` sort helper; `handle_attach_menu_input` grows
  branching for the two new modes; new `AttachMenuAction::Delete`/
  `StartRename` variants + `parse_attach_menu_input` cases; new
  `apply_rename_edit` byte-level editing function; `detach_and_open_menu`
  and every place that (re)populates `AttachMenu.servers` route through
  the sort helper and reset `pending_delete`/`rename` to `None`.
- **Modify `src/tui/render.rs`**: `draw_attach_menu` takes the sort
  helper's grouped output instead of a flat slice, emits header lines,
  drops the per-row `cwd` column, and renders the two new per-row visual
  states (delete-armed, rename-active). `attach_menu_line` narrows its
  signature accordingly.

No new files — both existing modules already own exactly the
responsibilities this feature extends (menu state/input in `mod.rs`,
menu drawing in `render.rs`).

---

### Task 1: `group_servers_by_cwd` sort helper

**Files:**
- Modify: `src/tui/mod.rs` (add near the bottom, alongside other pure
  helpers like `cycle_focus`/`hit_test_dividers`)
- Test: `src/tui/mod.rs` `mod tests` block

- [ ] **Step 1: Write the failing tests**

Add to the `mod tests` block at the bottom of `src/tui/mod.rs` (after the
existing `first_leaf_of_populated_workspace_is_leftmost_leaf` test):

```rust
fn server_with_cwd(name: &str, cwd: Option<&str>) -> ServerPaneInfo {
    ServerPaneInfo {
        id: Uuid::new_v4(),
        name: Some(name.to_string()),
        size: crate::protocol::Size { rows: 24, cols: 80 },
        status: crate::protocol::ServerPaneStatus::Running,
        foreground: cwd.map(|c| crate::protocol::ForegroundProcessInfo {
            process_name: "bash".to_string(),
            cwd: Some(c.to_string()),
        }),
    }
}

#[test]
fn group_servers_by_cwd_groups_matching_dirs_together() {
    let servers = vec![
        server_with_cwd("a", Some("/home/dev/api")),
        server_with_cwd("b", Some("/home/dev/web")),
        server_with_cwd("c", Some("/home/dev/api")),
    ];
    let grouped = group_servers_by_cwd(servers);
    let names: Vec<&str> = grouped.iter().map(|(_, s)| s.name.as_deref().unwrap()).collect();
    // Ascending by cwd: api's two panes (in original relative order),
    // then web's one pane.
    assert_eq!(names, vec!["a", "c", "b"]);
    let keys: Vec<&str> = grouped.iter().map(|(k, _)| k.as_str()).collect();
    assert_eq!(keys, vec!["/home/dev/api", "/home/dev/api", "/home/dev/web"]);
}

#[test]
fn group_servers_by_cwd_sorts_unknown_group_last() {
    let servers = vec![
        server_with_cwd("no-cwd", None),
        server_with_cwd("has-cwd", Some("/zzz/last-alphabetically")),
    ];
    let grouped = group_servers_by_cwd(servers);
    let names: Vec<&str> = grouped.iter().map(|(_, s)| s.name.as_deref().unwrap()).collect();
    // "/zzz/..." sorts after "Unknown" alphabetically, but Unknown is
    // forced last regardless.
    assert_eq!(names, vec!["has-cwd", "no-cwd"]);
    assert_eq!(grouped[1].0, "Unknown");
}

#[test]
fn group_servers_by_cwd_empty_list_is_empty() {
    // `ServerPaneInfo` doesn't derive `PartialEq`, so compare lengths
    // rather than the whole `Vec` via `assert_eq!`.
    assert_eq!(group_servers_by_cwd(vec![]).len(), 0);
}
```

Add `use crate::protocol::ServerPaneInfo;` if not already imported in the
test module's `use super::*;` scope (check — `super::*` already re-exports
it via the parent module's `use crate::protocol::{...ServerPaneInfo...}`
at the top of `tui/mod.rs`, so no new import line should be needed; if the
compiler disagrees, add it).

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test group_servers_by_cwd`
Expected: FAIL with `cannot find function `group_servers_by_cwd` in this scope`

- [ ] **Step 3: Write the implementation**

Add this function in `src/tui/mod.rs`, near `cycle_focus`/`hit_test_dividers`
(after `hit_test_dividers`, before the `AttachMenuAction` enum):

```rust
/// Sort `servers` into cwd-bucket order for the attach menu's grouped
/// display: ascending by cwd string, with a synthetic `"Unknown"` bucket
/// (no resolvable foreground `cwd` — a `Dead` pane, or a live one whose
/// lookup failed) always sorted last regardless of where it would fall
/// alphabetically. Within a bucket, panes keep their relative input
/// order (stable sort, no secondary key) — whatever order `ServerList`/
/// `ServerPaneList` returned. Returns `(group_key, server)` pairs rather
/// than a nested `Vec<Vec<_>>` so callers can walk it once and detect
/// group boundaries by comparing consecutive keys, which is exactly what
/// `render::draw_attach_menu` needs to decide where to emit a header.
fn group_servers_by_cwd(mut servers: Vec<ServerPaneInfo>) -> Vec<(String, ServerPaneInfo)> {
    const UNKNOWN: &str = "Unknown";
    servers.sort_by(|a, b| {
        let key_a = a.foreground.as_ref().and_then(|f| f.cwd.as_deref()).unwrap_or(UNKNOWN);
        let key_b = b.foreground.as_ref().and_then(|f| f.cwd.as_deref()).unwrap_or(UNKNOWN);
        match (key_a == UNKNOWN, key_b == UNKNOWN) {
            (true, true) => std::cmp::Ordering::Equal,
            (true, false) => std::cmp::Ordering::Greater,
            (false, true) => std::cmp::Ordering::Less,
            (false, false) => key_a.cmp(key_b),
        }
    });
    servers
        .into_iter()
        .map(|s| {
            let key = s.foreground.as_ref().and_then(|f| f.cwd.clone()).unwrap_or_else(|| UNKNOWN.to_string());
            (key, s)
        })
        .collect()
}
```

Note: `Vec::sort_by` is stable, which is what preserves each bucket's
relative input order (required by the "within a group, panes keep their
existing relative order" spec requirement).

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test group_servers_by_cwd`
Expected: PASS (3 tests)

- [ ] **Step 5: Commit**

```bash
git add src/tui/mod.rs
git commit -m "feat: add cwd-bucket sort helper for the attach menu"
```

---

### Task 2: Wire the sort helper into `AttachMenu` state + render grouped output

**Files:**
- Modify: `src/tui/mod.rs:210-213` (`AttachMenu` struct), `src/tui/mod.rs:457-471`
  (`detach_and_open_menu`)
- Modify: `src/tui/render.rs:324-378` (`draw_attach_menu`, `attach_menu_line`)
- Test: `src/tui/render.rs` `mod tests` block

This task changes `AttachMenu.servers`'s type from `Vec<ServerPaneInfo>`
to the sorted-and-grouped `Vec<(String, ServerPaneInfo)>` produced by
Task 1's helper, threading that new shape through render. Every other
place in `mod.rs` that reads `menu.servers[i]` or `menu.servers.len()`
still works unchanged against the new shape as long as it indexes/lens
the `Vec` itself (tuple contents are read via `.1`), which is what the
next few tasks' code does — this task only touches the two call sites
that currently pattern-match `ServerPaneInfo` fields directly off
`servers[i]` (there are none outside `confirm_attach_menu`, handled in
Task 4, and `attach_menu_line`/`draw_attach_menu`, handled here).

- [ ] **Step 1: Update `AttachMenu`'s struct definition**

In `src/tui/mod.rs`, replace:

```rust
struct AttachMenu {
    servers: Vec<ServerPaneInfo>,
    selected: usize,
}
```

with:

```rust
/// State for the `cmd-shift-z` attach menu: pick an existing server-pane,
/// or spawn a new one, to bind into the client-pane that was just
/// detached — or, since this menu doubles as a lightweight server-pane
/// manager, delete/rename one instead. `servers.len()` is always the
/// trailing "spawn new" row's index (mirrors the removed picker's
/// convention — see `render::draw_attach_menu`).
struct AttachMenu {
    /// `(cwd_group_key, server)` pairs, pre-sorted into cwd-bucket order
    /// by `group_servers_by_cwd` every time this field is populated —
    /// see that function's doc comment for the exact ordering rules.
    /// `selected` indexes this `Vec` directly; grouping is a rendering
    /// concern layered on top by `render::draw_attach_menu`, not a
    /// change to how selection/nav math works.
    servers: Vec<(String, ServerPaneInfo)>,
    selected: usize,
    /// `Some(index into servers)` while that row's delete is armed
    /// (first `x` pressed, awaiting a confirming `x`/Enter or a
    /// cancelling any-other-key). Mutually exclusive with `rename` —
    /// opening one clears the other.
    pending_delete: Option<usize>,
    /// `Some` while the inline rename field is focused for the row at
    /// `.index`. See `RenameState`'s own doc comment.
    rename: Option<RenameState>,
}

/// Live edit state for the attach menu's inline rename field (`r` on a
/// row). `text`/`cursor` are the field's edit buffer and cursor
/// position — a byte offset into `text`, always kept on a UTF-8 char
/// boundary by every editing operation in `apply_rename_edit`. `error`
/// holds the daemon's last rejection message (e.g. a name collision) to
/// render under the field; cleared on the next edit so a stale error
/// doesn't linger once the user starts fixing it.
struct RenameState {
    index: usize,
    text: String,
    cursor: usize,
    error: Option<String>,
}
```

- [ ] **Step 2: Update `detach_and_open_menu` to populate the new shape**

In `src/tui/mod.rs`, replace the body of `detach_and_open_menu`:

```rust
    async fn detach_and_open_menu(
        &mut self,
        write_half: &mut OwnedWriteHalf,
        reader: &mut FrameReader,
    ) -> anyhow::Result<()> {
        let Some(pane) = self.focused else { return Ok(()) };
        let req = Request::ClientUnbind { workspace: self.workspace.id.to_string(), pane };
        let _ = self.request(write_half, reader, req).await?;
        if let Response::ServerPaneList(servers) =
            self.request(write_half, reader, Request::ServerList).await?
        {
            self.attach_menu = Some(AttachMenu { servers, selected: 0 });
        }
        Ok(())
    }
```

with:

```rust
    async fn detach_and_open_menu(
        &mut self,
        write_half: &mut OwnedWriteHalf,
        reader: &mut FrameReader,
    ) -> anyhow::Result<()> {
        let Some(pane) = self.focused else { return Ok(()) };
        let req = Request::ClientUnbind { workspace: self.workspace.id.to_string(), pane };
        let _ = self.request(write_half, reader, req).await?;
        if let Response::ServerPaneList(servers) =
            self.request(write_half, reader, Request::ServerList).await?
        {
            self.attach_menu = Some(AttachMenu {
                servers: group_servers_by_cwd(servers),
                selected: 0,
                pending_delete: None,
                rename: None,
            });
        }
        Ok(())
    }
```

- [ ] **Step 3: Update `draw_attach_menu`/`attach_menu_line` to consume grouped input**

In `src/tui/render.rs`, replace:

```rust
pub fn draw_attach_menu(frame: &mut Frame, servers: &[ServerPaneInfo], selected: usize) {
    // Wider than the previous 60% -- each row now packs five columns
    // (name/cwd/process/id/status, see `attach_menu_line`) rather than
    // the original two, and needs more horizontal room to avoid every
    // field being clipped down to near-nothing on an ordinary terminal
    // width.
    let area = centered_rect(85, 60, frame.area());
    frame.render_widget(Clear, area);

    let mut lines: Vec<Line<'static>> = servers
        .iter()
        .enumerate()
        .map(|(i, server)| attach_menu_line(server, i == selected))
        .collect();

    let spawn_index = servers.len();
    lines.push(spawn_new_line(selected == spawn_index));

    let block = Block::bordered().title("Attach server-pane");
    frame.render_widget(Paragraph::new(lines).block(block), area);
}

/// Column widths for the attach menu's server-pane rows: `name | cwd |
/// process | id | status`. `cwd` gets the most space since full paths
/// are usually the longest field; `id` is fixed at 8 (the same
/// `short_id` prefix used elsewhere) since a full UUID would dominate
/// the row for no benefit — the attach menu is for picking a pane by
/// eye, not by exact id.
const NAME_COL_WIDTH: usize = 12;
const CWD_COL_WIDTH: usize = 24;
const PROCESS_COL_WIDTH: usize = 10;

fn attach_menu_line(server: &ServerPaneInfo, selected: bool) -> Line<'static> {
    let name = server.name.clone().unwrap_or_else(|| short_id(server.id));
    let status = match server.status {
        ServerPaneStatus::Running => "Running",
        ServerPaneStatus::Dead => "Dead",
    };
    let process = server.foreground.as_ref().map_or("-", |f| f.process_name.as_str());
    let cwd = server.foreground.as_ref().and_then(|f| f.cwd.as_deref()).unwrap_or("-");
    let text = format!(
        "{} {:<name_w$} {:<cwd_w$} {:<process_w$} {} {}",
        if selected { ">" } else { " " },
        truncate_end(&name, NAME_COL_WIDTH),
        truncate_start(cwd, CWD_COL_WIDTH),
        truncate_end(process, PROCESS_COL_WIDTH),
        short_id(server.id),
        status,
        name_w = NAME_COL_WIDTH,
        cwd_w = CWD_COL_WIDTH,
        process_w = PROCESS_COL_WIDTH,
    );
    let style = if selected { Style::new().add_modifier(Modifier::REVERSED) } else { Style::new() };
    Line::styled(text, style)
}
```

with:

```rust
pub fn draw_attach_menu(frame: &mut Frame, servers: &[(String, ServerPaneInfo)], selected: usize) {
    // Wider than the previous 60% -- each row now packs four columns
    // (name/process/id/status, see `attach_menu_line`) rather than the
    // original two, and needs more horizontal room to avoid every field
    // being clipped down to near-nothing on an ordinary terminal width.
    let area = centered_rect(85, 60, frame.area());
    frame.render_widget(Clear, area);

    let mut lines: Vec<Line<'static>> = Vec::with_capacity(servers.len() * 2 + 1);
    let mut last_group: Option<&str> = None;
    for (i, (group, server)) in servers.iter().enumerate() {
        if last_group != Some(group.as_str()) {
            lines.push(Line::styled(group.clone(), Style::new().add_modifier(Modifier::BOLD)));
            last_group = Some(group.as_str());
        }
        lines.push(attach_menu_line(server, i == selected));
    }

    let spawn_index = servers.len();
    lines.push(spawn_new_line(selected == spawn_index));

    let block = Block::bordered().title("Attach server-pane");
    frame.render_widget(Paragraph::new(lines).block(block), area);
}

/// Column widths for the attach menu's server-pane rows: `name | process
/// | id | status` (a `cwd` column existed here before rows were grouped
/// under per-cwd header lines — see `draw_attach_menu` — at which point
/// showing it a second time per row became redundant). `id` is fixed at
/// 8 (the same `short_id` prefix used elsewhere) since a full UUID would
/// dominate the row for no benefit — the attach menu is for picking a
/// pane by eye, not by exact id.
const NAME_COL_WIDTH: usize = 12;
const PROCESS_COL_WIDTH: usize = 10;

fn attach_menu_line(server: &ServerPaneInfo, selected: bool) -> Line<'static> {
    let name = server.name.clone().unwrap_or_else(|| short_id(server.id));
    let status = match server.status {
        ServerPaneStatus::Running => "Running",
        ServerPaneStatus::Dead => "Dead",
    };
    let process = server.foreground.as_ref().map_or("-", |f| f.process_name.as_str());
    let text = format!(
        "  {} {:<name_w$} {:<process_w$} {} {}",
        if selected { ">" } else { " " },
        truncate_end(&name, NAME_COL_WIDTH),
        truncate_end(process, PROCESS_COL_WIDTH),
        short_id(server.id),
        status,
        name_w = NAME_COL_WIDTH,
        process_w = PROCESS_COL_WIDTH,
    );
    let style = if selected { Style::new().add_modifier(Modifier::REVERSED) } else { Style::new() };
    Line::styled(text, style)
}
```

(The leading two-space literal in the row's `text` is the "indented two
spaces underneath its header" from the spec — the `>`/` ` selection
marker still leads within that indent, matching the original row's
leading-column convention.)

Delete the now-unused `truncate_start` function only if nothing else in
the file calls it — check first:

```bash
grep -n "truncate_start" src/tui/render.rs
```

If the only remaining hits are its own definition and its own test
(`truncate_start_keeps_the_tail_with_a_leading_ellipsis`), delete both
the function and that test — it existed solely to truncate the `cwd`
column this task removes. If anything else still calls it, leave it.

- [ ] **Step 4: Update the two render tests that construct `servers` directly**

In `src/tui/render.rs`'s `mod tests`, `draw_attach_menu_shows_selected_item`
and `draw_attach_menu_shows_dash_for_unknown_foreground` currently build
`let servers = vec![server];` and pass it straight to `draw_attach_menu`.
Update both call sites to wrap in the new tuple shape. For
`draw_attach_menu_shows_selected_item`, change:

```rust
        let servers = vec![server];
```

to:

```rust
        let servers = vec![("/home/dev/project".to_string(), server)];
```

For `draw_attach_menu_shows_dash_for_unknown_foreground`, change:

```rust
        let servers = vec![server];
```

to:

```rust
        let servers = vec![("Unknown".to_string(), server)];
```

`draw_attach_menu_spawn_new_selected_does_not_panic` already passes an
empty `Vec` — its declared type `Vec<ServerPaneInfo>` needs updating to
`Vec<(String, ServerPaneInfo)>` but the empty literal itself is
unaffected:

```rust
        let servers: Vec<(String, ServerPaneInfo)> = vec![];
```

- [ ] **Step 5: Run the full test suite to verify everything compiles and passes**

Run: `cargo build 2>&1 | grep -A3 "error\["`
Expected: a compile error confined to `confirm_attach_menu`'s indexing
of `menu.servers[menu.selected]` (a `(String, ServerPaneInfo)` tuple
where a bare `ServerPaneInfo` is now expected). This is expected —
Task 3 fixes it next; do not attempt to fix it in this task.

Do not run `cargo test` yet — the build error above means it would
fail to compile.

- [ ] **Step 6: Commit**

```bash
git add src/tui/mod.rs src/tui/render.rs
git commit -m "feat: group attach menu rows by cwd under header lines"
```

---

### Task 3: Fix `confirm_attach_menu` and `move_attach_menu_selection` for the new tuple shape

**Files:**
- Modify: `src/tui/mod.rs:492-525` (`move_attach_menu_selection`, `confirm_attach_menu`)

Task 2 changed `AttachMenu.servers`'s element type from `ServerPaneInfo`
to `(String, ServerPaneInfo)`. `move_attach_menu_selection` only calls
`.len()`, which is unaffected — no change needed there, confirm by
inspection. `confirm_attach_menu` indexes `menu.servers[menu.selected].id`
directly, which now needs `.1.id`.

- [ ] **Step 1: Update `confirm_attach_menu`**

In `src/tui/mod.rs`, replace:

```rust
        } else {
            menu.servers[menu.selected].id.to_string()
        };
```

with:

```rust
        } else {
            menu.servers[menu.selected].1.id.to_string()
        };
```

- [ ] **Step 2: Verify `move_attach_menu_selection` needs no change**

Read the function body — it only calls `menu.servers.len()`, which
returns the same count whether elements are `ServerPaneInfo` or
`(String, ServerPaneInfo)`. Confirm this by reading
`src/tui/mod.rs`'s `move_attach_menu_selection` and note in your own
head that no edit is needed; do not modify it.

- [ ] **Step 3: Build and run the full test suite**

Run: `cargo build 2>&1 | grep -A3 "error\["`
Expected: no output (clean build)

Run: `cargo test`
Expected: all existing tests PASS, including the render tests updated in
Task 2.

- [ ] **Step 4: Commit**

```bash
git add src/tui/mod.rs
git commit -m "fix: index the tuple-wrapped attach menu servers list correctly"
```

---

### Task 4: Delete (`x`) — arm/confirm state machine

**Files:**
- Modify: `src/tui/mod.rs:476-490` (`handle_attach_menu_input`), `:671-694`
  (`AttachMenuAction` enum + `parse_attach_menu_input`)
- Test: `src/tui/mod.rs` `mod tests` block

- [ ] **Step 1: Write the failing tests**

Add to `src/tui/mod.rs`'s `mod tests` block:

```rust
#[test]
fn parse_attach_menu_input_x_is_delete() {
    assert_eq!(parse_attach_menu_input(b"x"), AttachMenuAction::Delete);
}

#[test]
fn parse_attach_menu_input_r_is_start_rename() {
    assert_eq!(parse_attach_menu_input(b"r"), AttachMenuAction::StartRename);
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test parse_attach_menu_input_x_is_delete parse_attach_menu_input_r_is_start_rename`
Expected: FAIL — `AttachMenuAction::Delete`/`StartRename` don't exist yet.

- [ ] **Step 3: Extend `AttachMenuAction` and `parse_attach_menu_input`**

In `src/tui/mod.rs`, replace:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AttachMenuAction {
    Up,
    Down,
    Confirm,
    Cancel,
    Ignore,
}

/// Map one raw input chunk to an `AttachMenuAction`. Direct byte matching:
/// arrow-key escape sequences and vi-style `j`/`k` move the selection,
/// Enter (`\r` or `\n`) confirms, a bare `Esc` cancels (leaving the pane
/// unbound — it was already detached by `detach_and_open_menu` before the
/// menu opened). Anything else is ignored rather than passed through — the
/// menu is modal and has no server-pane to forward keystrokes to yet.
fn parse_attach_menu_input(bytes: &[u8]) -> AttachMenuAction {
    match bytes {
        b"\r" | b"\n" => AttachMenuAction::Confirm,
        b"\x1b" => AttachMenuAction::Cancel,
        b"\x1b[A" | b"k" => AttachMenuAction::Up,
        b"\x1b[B" | b"j" => AttachMenuAction::Down,
        _ => AttachMenuAction::Ignore,
    }
}
```

with:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AttachMenuAction {
    Up,
    Down,
    Confirm,
    Cancel,
    /// `x` on a real server-pane row: arm (first press) or confirm
    /// (second press) that row's deletion — see
    /// `App::handle_attach_menu_input`'s `pending_delete` branch for the
    /// actual arm/confirm/cancel state machine; this variant alone
    /// doesn't distinguish arm from confirm, since that distinction
    /// depends on `AttachMenu.pending_delete`'s current state, not on
    /// the byte itself.
    Delete,
    /// `r` on a real server-pane row: open the inline rename field.
    StartRename,
    Ignore,
}

/// Map one raw input chunk to an `AttachMenuAction`. Direct byte matching:
/// arrow-key escape sequences and vi-style `j`/`k` move the selection,
/// Enter (`\r` or `\n`) confirms, a bare `Esc` cancels (leaving the pane
/// unbound — it was already detached by `detach_and_open_menu` before the
/// menu opened), `x` arms/confirms delete, `r` opens rename. Anything else
/// is ignored rather than passed through — the menu is modal and has no
/// server-pane to forward keystrokes to yet. Note: this table is only
/// consulted for *browsing* mode — `App::handle_attach_menu_input` routes
/// to entirely separate byte-handling while `pending_delete`/`rename` are
/// active, so `x`/Enter's *confirm* behavior when a delete is already
/// armed is handled there, not by this function returning some third
/// "confirm delete" variant.
fn parse_attach_menu_input(bytes: &[u8]) -> AttachMenuAction {
    match bytes {
        b"\r" | b"\n" => AttachMenuAction::Confirm,
        b"\x1b" => AttachMenuAction::Cancel,
        b"\x1b[A" | b"k" => AttachMenuAction::Up,
        b"\x1b[B" | b"j" => AttachMenuAction::Down,
        b"x" => AttachMenuAction::Delete,
        b"r" => AttachMenuAction::StartRename,
        _ => AttachMenuAction::Ignore,
    }
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test parse_attach_menu_input_x_is_delete parse_attach_menu_input_r_is_start_rename`
Expected: PASS

Run: `cargo build 2>&1 | grep -A3 "error\["`
Expected: an exhaustiveness error in `handle_attach_menu_input`'s
`match parse_attach_menu_input(bytes) { ... }` — the two new variants
aren't handled yet. This is expected; Step 5 fixes it.

- [ ] **Step 5: Write the delete arm/confirm/cancel logic**

Add these methods to `impl App` in `src/tui/mod.rs`, right after
`move_attach_menu_selection`:

```rust
    /// Arm the selected row's deletion (first `x`), a no-op on the
    /// trailing "spawn new" row (nothing to delete there).
    fn arm_delete(&mut self) {
        let Some(menu) = &mut self.attach_menu else { return };
        if menu.selected < menu.servers.len() {
            menu.pending_delete = Some(menu.selected);
        }
    }

    /// Confirm a previously-armed deletion: kill the server-pane, then
    /// re-fetch and re-group the server list so the menu reflects its
    /// removal. Clamps `selected` into the shrunk list -- if the deleted
    /// row was the last one, selection moves to the new last row;
    /// otherwise the numeric index is left as-is (which now names
    /// whatever slid up into that slot, an acceptable "selection moved
    /// on" side effect of any list shrinking under the cursor).
    async fn confirm_delete(
        &mut self,
        write_half: &mut OwnedWriteHalf,
        reader: &mut FrameReader,
    ) -> anyhow::Result<()> {
        let Some(menu) = &mut self.attach_menu else { return Ok(()) };
        let Some(index) = menu.pending_delete.take() else { return Ok(()) };
        let target = menu.servers[index].1.id.to_string();
        let _ = self.request(write_half, reader, Request::ServerKill { target }).await?;
        if let Response::ServerPaneList(servers) =
            self.request(write_half, reader, Request::ServerList).await?
        {
            let grouped = group_servers_by_cwd(servers);
            let Some(menu) = &mut self.attach_menu else { return Ok(()) };
            menu.servers = grouped;
            let spawn_index = menu.servers.len();
            if menu.selected > spawn_index {
                menu.selected = spawn_index;
            }
        }
        Ok(())
    }
```

Then replace `handle_attach_menu_input`'s body:

```rust
    async fn handle_attach_menu_input(
        &mut self,
        bytes: &[u8],
        write_half: &mut OwnedWriteHalf,
        reader: &mut FrameReader,
    ) -> anyhow::Result<()> {
        match parse_attach_menu_input(bytes) {
            AttachMenuAction::Up => self.move_attach_menu_selection(false),
            AttachMenuAction::Down => self.move_attach_menu_selection(true),
            AttachMenuAction::Cancel => self.attach_menu = None,
            AttachMenuAction::Confirm => self.confirm_attach_menu(write_half, reader).await?,
            AttachMenuAction::Ignore => {}
        }
        Ok(())
    }
```

with:

```rust
    async fn handle_attach_menu_input(
        &mut self,
        bytes: &[u8],
        write_half: &mut OwnedWriteHalf,
        reader: &mut FrameReader,
    ) -> anyhow::Result<()> {
        let has_pending_delete =
            self.attach_menu.as_ref().is_some_and(|m| m.pending_delete.is_some());
        if has_pending_delete {
            match bytes {
                b"x" | b"\r" | b"\n" => self.confirm_delete(write_half, reader).await?,
                _ => {
                    if let Some(menu) = &mut self.attach_menu {
                        menu.pending_delete = None;
                    }
                    // A cancelling keystroke isn't swallowed -- e.g. `j`
                    // both cancels the pending delete *and* moves the
                    // selection down, in one keypress. Fall through to
                    // normal dispatch for this same byte.
                    self.dispatch_attach_menu_action(
                        parse_attach_menu_input(bytes),
                        write_half,
                        reader,
                    )
                    .await?;
                }
            }
            return Ok(());
        }
        self.dispatch_attach_menu_action(parse_attach_menu_input(bytes), write_half, reader).await
    }

    /// Browse-mode dispatch: everything `handle_attach_menu_input` routes
    /// to once neither a pending delete nor an active rename is
    /// intercepting input first. Split out so the pending-delete
    /// cancel-and-fall-through path (above) can re-dispatch the same
    /// byte's `AttachMenuAction` without duplicating this match.
    async fn dispatch_attach_menu_action(
        &mut self,
        action: AttachMenuAction,
        write_half: &mut OwnedWriteHalf,
        reader: &mut FrameReader,
    ) -> anyhow::Result<()> {
        match action {
            AttachMenuAction::Up => self.move_attach_menu_selection(false),
            AttachMenuAction::Down => self.move_attach_menu_selection(true),
            AttachMenuAction::Cancel => self.attach_menu = None,
            AttachMenuAction::Confirm => self.confirm_attach_menu(write_half, reader).await?,
            AttachMenuAction::Delete => self.arm_delete(),
            AttachMenuAction::StartRename => self.start_rename(),
            AttachMenuAction::Ignore => {}
        }
        Ok(())
    }
```

`start_rename` doesn't exist yet — add a stub for now so this compiles,
Task 5 replaces it with the real implementation:

```rust
    fn start_rename(&mut self) {
        // Implemented in Task 5.
    }
```

- [ ] **Step 6: Run the full test suite**

Run: `cargo build 2>&1 | grep -A3 "error\["`
Expected: no output (clean build)

Run: `cargo test attach_menu`
Expected: all attach-menu-related tests PASS (existing +
`parse_attach_menu_input_x_is_delete`/`_r_is_start_rename` from Step 1).

- [ ] **Step 7: Write and run tests for the arm/confirm/cancel state machine**

Add to `src/tui/mod.rs`'s `mod tests`:

```rust
#[test]
fn arm_delete_sets_pending_delete_on_a_real_row() {
    let servers = vec![("Unknown".to_string(), server_with_cwd("a", None))];
    let mut app = App {
        workspace: WorkspaceInfo { id: Uuid::new_v4(), number: 1, name: None, tree: None },
        grids: HashMap::new(),
        focused: None,
        attach_menu: Some(AttachMenu { servers, selected: 0, pending_delete: None, rename: None }),
        frame_area: ratatui::layout::Rect::default(),
        dragging_split: None,
    };
    app.arm_delete();
    assert_eq!(app.attach_menu.unwrap().pending_delete, Some(0));
}

#[test]
fn arm_delete_on_spawn_new_row_is_a_no_op() {
    let servers = vec![("Unknown".to_string(), server_with_cwd("a", None))];
    let spawn_index = servers.len();
    let mut app = App {
        workspace: WorkspaceInfo { id: Uuid::new_v4(), number: 1, name: None, tree: None },
        grids: HashMap::new(),
        focused: None,
        attach_menu: Some(AttachMenu {
            servers,
            selected: spawn_index,
            pending_delete: None,
            rename: None,
        }),
        frame_area: ratatui::layout::Rect::default(),
        dragging_split: None,
    };
    app.arm_delete();
    assert_eq!(app.attach_menu.unwrap().pending_delete, None);
}
```

Run: `cargo test arm_delete`
Expected: PASS (2 tests)

Note: `confirm_delete`'s and the pending-delete-cancel-falls-through
behavior in `handle_attach_menu_input` both require a live daemon
connection (they call `self.request`), so they're covered at the
integration level in Task 6, not here — these two unit tests cover only
the synchronous `arm_delete` half of the state machine, which is all
that's testable without a socket.

- [ ] **Step 8: Commit**

```bash
git add src/tui/mod.rs
git commit -m "feat: add attach menu delete (x) with arm/confirm/cancel"
```

---

### Task 5: Rename (`r`) — inline full-line-editing text field

**Files:**
- Modify: `src/tui/mod.rs` (add `apply_rename_edit`, replace `start_rename`
  stub, extend `handle_attach_menu_input`)
- Test: `src/tui/mod.rs` `mod tests` block

- [ ] **Step 1: Write the failing tests for `apply_rename_edit`**

Add to `src/tui/mod.rs`'s `mod tests`:

```rust
fn rename_state(text: &str, cursor: usize) -> RenameState {
    RenameState { index: 0, text: text.to_string(), cursor, error: None }
}

#[test]
fn rename_edit_inserts_printable_bytes_at_cursor() {
    let mut state = rename_state("ab", 1);
    apply_rename_edit(&mut state, b"X");
    assert_eq!(state.text, "aXb");
    assert_eq!(state.cursor, 2);
}

#[test]
fn rename_edit_backspace_removes_char_before_cursor() {
    let mut state = rename_state("abc", 2);
    apply_rename_edit(&mut state, b"\x7f");
    assert_eq!(state.text, "ac");
    assert_eq!(state.cursor, 1);
}

#[test]
fn rename_edit_backspace_at_start_is_a_no_op() {
    let mut state = rename_state("abc", 0);
    apply_rename_edit(&mut state, b"\x7f");
    assert_eq!(state.text, "abc");
    assert_eq!(state.cursor, 0);
}

#[test]
fn rename_edit_delete_removes_char_at_cursor() {
    let mut state = rename_state("abc", 1);
    apply_rename_edit(&mut state, b"\x1b[3~");
    assert_eq!(state.text, "ac");
    assert_eq!(state.cursor, 1);
}

#[test]
fn rename_edit_left_and_right_move_cursor() {
    let mut state = rename_state("abc", 1);
    apply_rename_edit(&mut state, b"\x1b[C");
    assert_eq!(state.cursor, 2);
    apply_rename_edit(&mut state, b"\x1b[D");
    assert_eq!(state.cursor, 1);
}

#[test]
fn rename_edit_left_at_start_and_right_at_end_are_no_ops() {
    let mut state = rename_state("abc", 0);
    apply_rename_edit(&mut state, b"\x1b[D");
    assert_eq!(state.cursor, 0);
    let mut state = rename_state("abc", 3);
    apply_rename_edit(&mut state, b"\x1b[C");
    assert_eq!(state.cursor, 3);
}

#[test]
fn rename_edit_home_and_end_jump_cursor() {
    let mut state = rename_state("abc", 1);
    apply_rename_edit(&mut state, b"\x1b[H");
    assert_eq!(state.cursor, 0);
    apply_rename_edit(&mut state, b"\x1b[F");
    assert_eq!(state.cursor, 3);
}

#[test]
fn rename_edit_clears_stale_error_on_any_edit() {
    let mut state = rename_state("abc", 1);
    state.error = Some("name taken".to_string());
    apply_rename_edit(&mut state, b"X");
    assert_eq!(state.error, None);
}

#[test]
fn rename_edit_unrecognized_escape_is_ignored() {
    let mut state = rename_state("abc", 1);
    apply_rename_edit(&mut state, b"\x1b[99~");
    assert_eq!(state.text, "abc");
    assert_eq!(state.cursor, 1);
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test rename_edit`
Expected: FAIL — `apply_rename_edit`/`RenameState` field access errors
(compile failure, since `apply_rename_edit` doesn't exist).

- [ ] **Step 3: Implement `apply_rename_edit`**

Add this function in `src/tui/mod.rs`, near `parse_attach_menu_input`:

```rust
/// Apply one raw input chunk to a live rename field's edit buffer.
/// Byte-level matching in the same minimal style as
/// `parse_attach_menu_input`/`keys::parse` rather than pulling in a
/// text-input crate — this field only ever needs insert/delete/cursor
/// movement, not a general-purpose editor. Every branch that mutates
/// `text`/`cursor` also clears `state.error`, so a fresh edit dismisses
/// the last rejection message rather than leaving stale text stuck under
/// the field. Enter/Esc are handled by the caller (`App::
/// handle_attach_menu_input`'s rename branch), not here, since they
/// trigger network requests or close rename mode entirely -- concerns
/// outside this pure buffer-editing function.
fn apply_rename_edit(state: &mut RenameState, bytes: &[u8]) {
    match bytes {
        b"\x7f" | b"\x08" => {
            if state.cursor > 0 {
                let mut chars: Vec<char> = state.text.chars().collect();
                let char_idx = state.text[..state.cursor].chars().count() - 1;
                chars.remove(char_idx);
                state.text = chars.into_iter().collect();
                state.cursor = state.text[..].chars().take(char_idx).map(|c| c.len_utf8()).sum();
            } else {
                return;
            }
        }
        b"\x1b[3~" => {
            let char_idx = state.text[..state.cursor].chars().count();
            let mut chars: Vec<char> = state.text.chars().collect();
            if char_idx < chars.len() {
                chars.remove(char_idx);
                state.text = chars.into_iter().collect();
            } else {
                return;
            }
        }
        b"\x1b[D" => {
            if state.cursor > 0 {
                let char_idx = state.text[..state.cursor].chars().count() - 1;
                state.cursor = state.text.chars().take(char_idx).map(|c| c.len_utf8()).sum();
            } else {
                return;
            }
        }
        b"\x1b[C" => {
            if state.cursor < state.text.len() {
                let char_idx = state.text[..state.cursor].chars().count() + 1;
                state.cursor = state.text.chars().take(char_idx).map(|c| c.len_utf8()).sum();
            } else {
                return;
            }
        }
        b"\x1b[H" | b"\x1b[1~" => {
            if state.cursor == 0 {
                return;
            }
            state.cursor = 0;
        }
        b"\x1b[F" | b"\x1b[4~" => {
            if state.cursor == state.text.len() {
                return;
            }
            state.cursor = state.text.len();
        }
        _ if bytes.first() == Some(&0x1b) => {
            // Any other escape sequence this field doesn't recognize --
            // defense in depth, same rationale as `mouse::parse`'s
            // "Ignored" case: don't let a stray sequence get inserted as
            // literal garbage text.
            return;
        }
        _ => match std::str::from_utf8(bytes) {
            Ok(text) => {
                state.text.insert_str(state.cursor, text);
                state.cursor += text.len();
            }
            Err(_) => return,
        },
    }
    state.error = None;
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test rename_edit`
Expected: PASS (9 tests)

- [ ] **Step 5: Wire `start_rename` and the rename branch into `handle_attach_menu_input`**

Replace the `start_rename` stub added in Task 4:

```rust
    fn start_rename(&mut self) {
        // Implemented in Task 5.
    }
```

with:

```rust
    /// `r` on a real server-pane row: open the inline rename field,
    /// pre-filled with its current custom name (empty if it's currently
    /// falling back to the id — see `render::attach_menu_line`'s
    /// `unwrap_or_else(|| short_id(...))`), cursor starting at the end
    /// of the pre-filled text. A no-op on the trailing "spawn new" row.
    fn start_rename(&mut self) {
        let Some(menu) = &mut self.attach_menu else { return };
        if menu.selected >= menu.servers.len() {
            return;
        }
        let text = menu.servers[menu.selected].1.name.clone().unwrap_or_default();
        let cursor = text.len();
        menu.rename = Some(RenameState { index: menu.selected, text, cursor, error: None });
    }

    /// Submit the active rename field's current text: a no-op if empty
    /// (stays in rename mode rather than sending an empty name), sends
    /// `Request::ServerRename` otherwise. On success, re-fetches and
    /// re-groups the server list and closes rename mode. On
    /// `Response::Error` (e.g. a name collision), records the message in
    /// `rename.error` and stays open for another attempt.
    async fn confirm_rename(
        &mut self,
        write_half: &mut OwnedWriteHalf,
        reader: &mut FrameReader,
    ) -> anyhow::Result<()> {
        let Some(menu) = &self.attach_menu else { return Ok(()) };
        let Some(rename) = &menu.rename else { return Ok(()) };
        if rename.text.is_empty() {
            return Ok(());
        }
        let target = menu.servers[rename.index].1.id.to_string();
        let new_name = rename.text.clone();
        let req = Request::ServerRename { target, new_name };
        match self.request(write_half, reader, req).await? {
            Response::Ack => {
                if let Response::ServerPaneList(servers) =
                    self.request(write_half, reader, Request::ServerList).await?
                {
                    let Some(menu) = &mut self.attach_menu else { return Ok(()) };
                    menu.servers = group_servers_by_cwd(servers);
                    menu.rename = None;
                }
            }
            Response::Error { message } => {
                if let Some(menu) = &mut self.attach_menu {
                    if let Some(rename) = &mut menu.rename {
                        rename.error = Some(message);
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }
```

Then update `handle_attach_menu_input` (from Task 4's version) to check
`rename` first, before the `pending_delete` check:

```rust
    async fn handle_attach_menu_input(
        &mut self,
        bytes: &[u8],
        write_half: &mut OwnedWriteHalf,
        reader: &mut FrameReader,
    ) -> anyhow::Result<()> {
        let is_renaming = self.attach_menu.as_ref().is_some_and(|m| m.rename.is_some());
        if is_renaming {
            match bytes {
                b"\r" | b"\n" => self.confirm_rename(write_half, reader).await?,
                b"\x1b" => {
                    if let Some(menu) = &mut self.attach_menu {
                        menu.rename = None;
                    }
                }
                _ => {
                    if let Some(menu) = &mut self.attach_menu {
                        if let Some(rename) = &mut menu.rename {
                            apply_rename_edit(rename, bytes);
                        }
                    }
                }
            }
            return Ok(());
        }

        let has_pending_delete =
            self.attach_menu.as_ref().is_some_and(|m| m.pending_delete.is_some());
        if has_pending_delete {
            match bytes {
                b"x" | b"\r" | b"\n" => self.confirm_delete(write_half, reader).await?,
                _ => {
                    if let Some(menu) = &mut self.attach_menu {
                        menu.pending_delete = None;
                    }
                    self.dispatch_attach_menu_action(
                        parse_attach_menu_input(bytes),
                        write_half,
                        reader,
                    )
                    .await?;
                }
            }
            return Ok(());
        }
        self.dispatch_attach_menu_action(parse_attach_menu_input(bytes), write_half, reader).await
    }
```

- [ ] **Step 6: Run the full test suite**

Run: `cargo build 2>&1 | grep -A3 "error\["`
Expected: no output (clean build)

Run: `cargo test`
Expected: all tests PASS.

- [ ] **Step 7: Write and run tests for `start_rename`**

Add to `src/tui/mod.rs`'s `mod tests`:

```rust
#[test]
fn start_rename_prefills_current_name_with_cursor_at_end() {
    let servers = vec![("Unknown".to_string(), server_with_cwd("my-name", None))];
    let mut app = App {
        workspace: WorkspaceInfo { id: Uuid::new_v4(), number: 1, name: None, tree: None },
        grids: HashMap::new(),
        focused: None,
        attach_menu: Some(AttachMenu { servers, selected: 0, pending_delete: None, rename: None }),
        frame_area: ratatui::layout::Rect::default(),
        dragging_split: None,
    };
    app.start_rename();
    let rename = app.attach_menu.unwrap().rename.unwrap();
    assert_eq!(rename.text, "my-name");
    assert_eq!(rename.cursor, "my-name".len());
    assert_eq!(rename.index, 0);
}

#[test]
fn start_rename_on_spawn_new_row_is_a_no_op() {
    let servers = vec![("Unknown".to_string(), server_with_cwd("a", None))];
    let spawn_index = servers.len();
    let mut app = App {
        workspace: WorkspaceInfo { id: Uuid::new_v4(), number: 1, name: None, tree: None },
        grids: HashMap::new(),
        focused: None,
        attach_menu: Some(AttachMenu {
            servers,
            selected: spawn_index,
            pending_delete: None,
            rename: None,
        }),
        frame_area: ratatui::layout::Rect::default(),
        dragging_split: None,
    };
    app.start_rename();
    assert!(app.attach_menu.unwrap().rename.is_none());
}
```

Run: `cargo test start_rename`
Expected: PASS (2 tests)

- [ ] **Step 8: Commit**

```bash
git add src/tui/mod.rs
git commit -m "feat: add attach menu inline rename (r) with full line editing"
```

---

### Task 6: Render the delete-armed and rename-active row states

**Files:**
- Modify: `src/tui/mod.rs` (`run`'s draw closure, where
  `render::draw_attach_menu` is called)
- Modify: `src/tui/render.rs` (`draw_attach_menu`'s signature and body)
- Test: `src/tui/render.rs` `mod tests` block

`draw_attach_menu` currently takes `(&mut Frame, &[(String,
ServerPaneInfo)], selected: usize)`. It needs to also know: which index
(if any) has a pending delete, and the live rename state (if any), to
render those two per-row visual overrides. Rather than adding two more
positional parameters (error-prone at call sites), pass the whole
`&AttachMenu` — render already depends on `tui::mod`'s types
transitively (it's called from there), so this isn't a new coupling
direction, just a wider one.

- [ ] **Step 1: Update `run`'s call site**

In `src/tui/mod.rs`, find:

```rust
            if let Some(menu) = &app.attach_menu {
                render::draw_attach_menu(frame, &menu.servers, menu.selected);
            }
```

Replace with:

```rust
            if let Some(menu) = &app.attach_menu {
                render::draw_attach_menu(frame, menu);
            }
```

- [ ] **Step 2: Update `draw_attach_menu`'s signature and body**

`render` is a *child* module of `tui` (declared via `pub mod render;` in
`tui/mod.rs`), and Rust's default (private) item visibility already
extends to descendant modules — a private item defined in `tui/mod.rs`
is visible to `tui::render` and `tui::render::tests` alike, with no
`pub(super)`/`pub(crate)` annotation needed. `AttachMenu`/`RenameState`
and all their fields can therefore be referenced from `render.rs` as
`super::AttachMenu { .. }` / `super::RenameState { .. }` exactly as
currently declared — do not add any visibility modifiers to either
struct.

One consequence: `draw_attach_menu`'s new parameter type (`&AttachMenu`)
is private, so its signature must drop the `pub` it currently has —
`pub fn` exposing a private type in its own signature is a compiler
warning (`private_interfaces`), and `draw_attach_menu` is in fact only
ever called from `tui::mod` (a parent module, which sees non-`pub`
items in its child modules with no visibility change needed — same
reasoning as the `AttachMenu`/`RenameState` note above, just applied to
a function instead of a struct). `draw` (the sibling function that
still is `pub`) is unaffected — its own parameters are all types from
`protocol.rs`, which are `pub` because they cross the wire, so it never
had this issue.

In `src/tui/render.rs`, replace:

```rust
pub fn draw_attach_menu(frame: &mut Frame, servers: &[(String, ServerPaneInfo)], selected: usize) {
    // Wider than the previous 60% -- each row now packs four columns
    // (name/process/id/status, see `attach_menu_line`) rather than the
    // original two, and needs more horizontal room to avoid every field
    // being clipped down to near-nothing on an ordinary terminal width.
    let area = centered_rect(85, 60, frame.area());
    frame.render_widget(Clear, area);

    let mut lines: Vec<Line<'static>> = Vec::with_capacity(servers.len() * 2 + 1);
    let mut last_group: Option<&str> = None;
    for (i, (group, server)) in servers.iter().enumerate() {
        if last_group != Some(group.as_str()) {
            lines.push(Line::styled(group.clone(), Style::new().add_modifier(Modifier::BOLD)));
            last_group = Some(group.as_str());
        }
        lines.push(attach_menu_line(server, i == selected));
    }

    let spawn_index = servers.len();
    lines.push(spawn_new_line(selected == spawn_index));

    let block = Block::bordered().title("Attach server-pane");
    frame.render_widget(Paragraph::new(lines).block(block), area);
}
```

with:

```rust
fn draw_attach_menu(frame: &mut Frame, menu: &super::AttachMenu) {
    // Wider than the previous 60% -- each row now packs four columns
    // (name/process/id/status, see `attach_menu_line`) rather than the
    // original two, and needs more horizontal room to avoid every field
    // being clipped down to near-nothing on an ordinary terminal width.
    let area = centered_rect(85, 60, frame.area());
    frame.render_widget(Clear, area);

    let servers = &menu.servers;
    let mut lines: Vec<Line<'static>> = Vec::with_capacity(servers.len() * 2 + 2);
    let mut last_group: Option<&str> = None;
    for (i, (group, server)) in servers.iter().enumerate() {
        if last_group != Some(group.as_str()) {
            lines.push(Line::styled(group.clone(), Style::new().add_modifier(Modifier::BOLD)));
            last_group = Some(group.as_str());
        }
        let armed = menu.pending_delete == Some(i);
        let renaming = menu.rename.as_ref().filter(|r| r.index == i);
        lines.push(attach_menu_line(server, i == menu.selected, armed, renaming));
        if let Some(rename) = renaming {
            if let Some(error) = &rename.error {
                lines.push(Line::styled(
                    format!("    {error}"),
                    Style::new().fg(Color::Red),
                ));
            }
        }
    }

    let spawn_index = servers.len();
    lines.push(spawn_new_line(menu.selected == spawn_index));

    let block = Block::bordered().title("Attach server-pane");
    frame.render_widget(Paragraph::new(lines).block(block), area);
}
```

- [ ] **Step 3: Update `attach_menu_line` for the two new visual states**

Replace:

```rust
fn attach_menu_line(server: &ServerPaneInfo, selected: bool) -> Line<'static> {
    let name = server.name.clone().unwrap_or_else(|| short_id(server.id));
    let status = match server.status {
        ServerPaneStatus::Running => "Running",
        ServerPaneStatus::Dead => "Dead",
    };
    let process = server.foreground.as_ref().map_or("-", |f| f.process_name.as_str());
    let text = format!(
        "  {} {:<name_w$} {:<process_w$} {} {}",
        if selected { ">" } else { " " },
        truncate_end(&name, NAME_COL_WIDTH),
        truncate_end(process, PROCESS_COL_WIDTH),
        short_id(server.id),
        status,
        name_w = NAME_COL_WIDTH,
        process_w = PROCESS_COL_WIDTH,
    );
    let style = if selected { Style::new().add_modifier(Modifier::REVERSED) } else { Style::new() };
    Line::styled(text, style)
}
```

with:

```rust
fn attach_menu_line(
    server: &ServerPaneInfo,
    selected: bool,
    delete_armed: bool,
    renaming: Option<&super::RenameState>,
) -> Line<'static> {
    let status = match server.status {
        ServerPaneStatus::Running => "Running",
        ServerPaneStatus::Dead => "Dead",
    };
    let process = server.foreground.as_ref().map_or("-", |f| f.process_name.as_str());
    let marker = if selected { ">" } else { " " };

    if let Some(rename) = renaming {
        let text = format!(
            "  {marker} [{}] {:<process_w$} {} {}",
            rename.text,
            truncate_end(process, PROCESS_COL_WIDTH),
            short_id(server.id),
            status,
            process_w = PROCESS_COL_WIDTH,
        );
        return Line::styled(text, Style::new().add_modifier(Modifier::REVERSED));
    }

    let name = server.name.clone().unwrap_or_else(|| short_id(server.id));
    let text = format!(
        "  {marker} {:<name_w$} {:<process_w$} {} {}{}",
        truncate_end(&name, NAME_COL_WIDTH),
        truncate_end(process, PROCESS_COL_WIDTH),
        short_id(server.id),
        status,
        if delete_armed { "  [x/Enter: confirm delete]" } else { "" },
        name_w = NAME_COL_WIDTH,
        process_w = PROCESS_COL_WIDTH,
    );
    let style = match (selected, delete_armed) {
        (_, true) => Style::new().add_modifier(Modifier::REVERSED).fg(Color::Red),
        (true, false) => Style::new().add_modifier(Modifier::REVERSED),
        (false, false) => Style::new(),
    };
    Line::styled(text, style)
}
```

Note: this drops the cursor-position rendering (`rename.cursor`) called
for in the spec's "visible cursor" wording — ratatui's `Line`/`Span`
text model doesn't support an inline cursor glyph without manual
character-splitting into styled spans, which is more complexity than
this row-level view currently has elsewhere. The `[text]` bracket
convention makes the field visually distinct as an edit box; if a
visible in-text cursor caret is later wanted, that's a follow-up, not a
blocker for the arm/confirm/insert/delete/move functionality tests
above already fully cover. Flag this in the PR description as a known
simplification.

- [ ] **Step 4: Update the two call sites in `mod tests` that still use the old two-arg shape**

The `draw_attach_menu_shows_selected_item` and
`draw_attach_menu_shows_dash_for_unknown_foreground` tests from Task 2
call `draw_attach_menu(frame, &servers, 0)` — this signature no longer
exists. Update both tests to build a real `super::super::AttachMenu`
instead (per Step 2's visibility note, its private fields are already
reachable from `render.rs`'s nested `tests` module with no annotation
changes).

Update both tests. Replace:

```rust
    #[test]
    fn draw_attach_menu_shows_selected_item() {
        let server = ServerPaneInfo {
            id: Uuid::new_v4(),
            name: Some("editor".to_string()),
            size: Size { rows: 24, cols: 80 },
            status: ServerPaneStatus::Running,
            foreground: Some(ForegroundProcessInfo {
                process_name: "vim".to_string(),
                cwd: Some("/home/dev/project".to_string()),
            }),
        };
        let servers = vec![("/home/dev/project".to_string(), server)];
        // Wide enough that the popup (85% of frame width, see
        // draw_attach_menu) comfortably fits every column's full width
        // rather than clipping mid-row -- a narrower backend was exactly
        // what caused this test to flake when the columns were added.
        let backend = TestBackend::new(100, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw_attach_menu(frame, &servers, 0)).unwrap();
        assert!(buffer_contains(&terminal, "editor"));
        assert!(buffer_contains(&terminal, "vim"));
        assert!(buffer_contains(&terminal, "project"));
        assert!(buffer_contains(&terminal, "spawn new"));
        assert!(buffer_contains(&terminal, "Attach server-pane"));
    }

    #[test]
    fn draw_attach_menu_shows_dash_for_unknown_foreground() {
        let server = ServerPaneInfo {
            id: Uuid::new_v4(),
            name: Some("editor".to_string()),
            size: Size { rows: 24, cols: 80 },
            status: ServerPaneStatus::Dead,
            foreground: None,
        };
        let servers = vec![("Unknown".to_string(), server)];
        let backend = TestBackend::new(100, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw_attach_menu(frame, &servers, 0)).unwrap();
        assert!(buffer_contains(&terminal, "editor"));
        assert!(buffer_contains(&terminal, "-"));
    }
```

with:

```rust
    #[test]
    fn draw_attach_menu_shows_selected_item() {
        let server = ServerPaneInfo {
            id: Uuid::new_v4(),
            name: Some("editor".to_string()),
            size: Size { rows: 24, cols: 80 },
            status: ServerPaneStatus::Running,
            foreground: Some(ForegroundProcessInfo {
                process_name: "vim".to_string(),
                cwd: Some("/home/dev/project".to_string()),
            }),
        };
        let servers = vec![("/home/dev/project".to_string(), server)];
        let menu = super::super::AttachMenu {
            servers,
            selected: 0,
            pending_delete: None,
            rename: None,
        };
        // Wide enough that the popup (85% of frame width, see
        // draw_attach_menu) comfortably fits every column's full width
        // rather than clipping mid-row -- a narrower backend was exactly
        // what caused this test to flake when the columns were added.
        let backend = TestBackend::new(100, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw_attach_menu(frame, &menu)).unwrap();
        assert!(buffer_contains(&terminal, "editor"));
        assert!(buffer_contains(&terminal, "vim"));
        assert!(buffer_contains(&terminal, "/home/dev/project"));
        assert!(buffer_contains(&terminal, "spawn new"));
        assert!(buffer_contains(&terminal, "Attach server-pane"));
    }

    #[test]
    fn draw_attach_menu_shows_dash_for_unknown_foreground() {
        let server = ServerPaneInfo {
            id: Uuid::new_v4(),
            name: Some("editor".to_string()),
            size: Size { rows: 24, cols: 80 },
            status: ServerPaneStatus::Dead,
            foreground: None,
        };
        let servers = vec![("Unknown".to_string(), server)];
        let menu = super::super::AttachMenu {
            servers,
            selected: 0,
            pending_delete: None,
            rename: None,
        };
        let backend = TestBackend::new(100, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw_attach_menu(frame, &menu)).unwrap();
        assert!(buffer_contains(&terminal, "editor"));
        assert!(buffer_contains(&terminal, "-"));
    }
```

(Note the assertion in the first test changed from the substring
`"project"` to the full `"/home/dev/project"` — now that `cwd` only
appears once, as the group header, matching on the full path is more
precise and equally valid since the header renders the complete cwd
string, not a truncated one.)

Also update `draw_attach_menu_spawn_new_selected_does_not_panic`.
Replace:

```rust
    #[test]
    fn draw_attach_menu_spawn_new_selected_does_not_panic() {
        let servers: Vec<(String, ServerPaneInfo)> = vec![];
        let backend = TestBackend::new(60, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw_attach_menu(frame, &servers, 0)).unwrap();
        assert!(buffer_contains(&terminal, "spawn new"));
    }
```

with:

```rust
    #[test]
    fn draw_attach_menu_spawn_new_selected_does_not_panic() {
        let menu = super::super::AttachMenu {
            servers: vec![],
            selected: 0,
            pending_delete: None,
            rename: None,
        };
        let backend = TestBackend::new(60, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw_attach_menu(frame, &menu)).unwrap();
        assert!(buffer_contains(&terminal, "spawn new"));
    }
```

- [ ] **Step 5: Write new render tests for grouping, delete-armed, and rename-active rows**

Add to `src/tui/render.rs`'s `mod tests`:

```rust
    #[test]
    fn draw_attach_menu_shows_group_headers_for_each_distinct_cwd() {
        let a = ServerPaneInfo {
            id: Uuid::new_v4(),
            name: Some("api-shell".to_string()),
            size: Size { rows: 24, cols: 80 },
            status: ServerPaneStatus::Running,
            foreground: Some(ForegroundProcessInfo {
                process_name: "bash".to_string(),
                cwd: Some("/home/dev/api".to_string()),
            }),
        };
        let b = ServerPaneInfo {
            id: Uuid::new_v4(),
            name: Some("web-shell".to_string()),
            size: Size { rows: 24, cols: 80 },
            status: ServerPaneStatus::Running,
            foreground: Some(ForegroundProcessInfo {
                process_name: "bash".to_string(),
                cwd: Some("/home/dev/web".to_string()),
            }),
        };
        let servers =
            vec![("/home/dev/api".to_string(), a), ("/home/dev/web".to_string(), b)];
        let menu = super::super::AttachMenu {
            servers,
            selected: 0,
            pending_delete: None,
            rename: None,
        };
        let backend = TestBackend::new(100, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw_attach_menu(frame, &menu)).unwrap();
        assert!(buffer_contains(&terminal, "/home/dev/api"));
        assert!(buffer_contains(&terminal, "/home/dev/web"));
        assert!(buffer_contains(&terminal, "api-shell"));
        assert!(buffer_contains(&terminal, "web-shell"));
    }

    #[test]
    fn draw_attach_menu_shows_delete_confirm_hint_on_armed_row() {
        let server = ServerPaneInfo {
            id: Uuid::new_v4(),
            name: Some("editor".to_string()),
            size: Size { rows: 24, cols: 80 },
            status: ServerPaneStatus::Running,
            foreground: None,
        };
        let menu = super::super::AttachMenu {
            servers: vec![("Unknown".to_string(), server)],
            selected: 0,
            pending_delete: Some(0),
            rename: None,
        };
        let backend = TestBackend::new(100, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw_attach_menu(frame, &menu)).unwrap();
        assert!(buffer_contains(&terminal, "confirm delete"));
    }

    #[test]
    fn draw_attach_menu_shows_live_rename_text_and_error() {
        let server = ServerPaneInfo {
            id: Uuid::new_v4(),
            name: Some("old-name".to_string()),
            size: Size { rows: 24, cols: 80 },
            status: ServerPaneStatus::Running,
            foreground: None,
        };
        let menu = super::super::AttachMenu {
            servers: vec![("Unknown".to_string(), server)],
            selected: 0,
            pending_delete: None,
            rename: Some(super::super::RenameState {
                index: 0,
                text: "new-name".to_string(),
                cursor: 8,
                error: Some("name taken".to_string()),
            }),
        };
        let backend = TestBackend::new(100, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw_attach_menu(frame, &menu)).unwrap();
        assert!(buffer_contains(&terminal, "new-name"));
        assert!(buffer_contains(&terminal, "name taken"));
    }
```

- [ ] **Step 6: Run the full test suite**

Run: `cargo build 2>&1 | grep -A3 "error\["`
Expected: no output (clean build)

Run: `cargo test`
Expected: all tests PASS (existing + all new tests from Tasks 1-6).

- [ ] **Step 7: Commit**

```bash
git add src/tui/mod.rs src/tui/render.rs
git commit -m "feat: render attach menu delete-armed and rename-active row states"
```

---

### Task 7: Integration test — delete and rename round-trip through a real daemon

**Files:**
- Modify: `src/daemon/mod.rs` `mod tests` block (existing daemon
  integration test infrastructure — `TestConn`, `start_daemon`)

Per the spec's Non-goals ("this spec only wires the existing menu to
already-tested requests, not adds new daemon behavior"), this task adds
one regression test confirming the wire-level shape the menu now relies
on (`Request::ServerKill`/`ServerRename` → `Response::Ack`/`Error`)
still holds — not new daemon logic tests, which already exist (see
`server_rename_to_existing_name_errors_and_keeps_old_name` in
`src/daemon/state.rs` and `server_kill_unbinds_client_panes_across_workspaces`
in `src/daemon/mod.rs`).

- [ ] **Step 1: Write the failing test**

Add to `src/daemon/mod.rs`'s `mod tests` block, after
`server_kill_unbinds_client_panes_across_workspaces`:

```rust
    /// Regression coverage for the attach menu's new delete/rename
    /// actions (see docs/superpowers/specs/2026-08-03-attach-menu-groups-
    /// and-shortcuts-design.md): both requests the menu now issues
    /// directly must still round-trip through the wire protocol exactly
    /// as `dimax server kill`/`rename` already do. This is regression
    /// coverage for the wiring, not new daemon logic -- ServerKill and
    /// ServerRename's actual behavior is already covered by
    /// `server_kill_unbinds_client_panes_across_workspaces` above and by
    /// `daemon::state`'s `server_rename_*` tests.
    #[tokio::test]
    async fn server_rename_then_kill_round_trip_for_the_attach_menu() {
        let guard = start_daemon().await;
        let mut conn = TestConn::connect(&guard.0).await;

        let server_pane = match conn
            .request(Request::ServerSpawn { name: None, cmd: Some("cat".to_string()) })
            .await
        {
            Response::ServerPane(info) => info.id,
            other => panic!("expected ServerPane, got {other:?}"),
        };

        match conn
            .request(Request::ServerRename {
                target: server_pane.to_string(),
                new_name: "renamed-from-menu".to_string(),
            })
            .await
        {
            Response::Ack => {}
            other => panic!("expected Ack, got {other:?}"),
        }

        match conn.request(Request::ServerList).await {
            Response::ServerPaneList(list) => {
                let pane = list.iter().find(|p| p.id == server_pane).expect("pane should still exist");
                assert_eq!(pane.name.as_deref(), Some("renamed-from-menu"));
            }
            other => panic!("expected ServerPaneList, got {other:?}"),
        }

        match conn
            .request(Request::ServerKill { target: server_pane.to_string() })
            .await
        {
            Response::Ack => {}
            other => panic!("expected Ack, got {other:?}"),
        }

        match conn.request(Request::ServerList).await {
            Response::ServerPaneList(list) => {
                assert!(
                    !list.iter().any(|p| p.id == server_pane),
                    "killed server-pane should no longer be listed"
                );
            }
            other => panic!("expected ServerPaneList, got {other:?}"),
        }
    }
```

- [ ] **Step 2: Run the test to verify it fails first (sanity check the test itself)**

Run: `cargo test server_rename_then_kill_round_trip_for_the_attach_menu`
Expected: this should actually PASS immediately, since `ServerRename`/
`ServerKill`/`ServerList` all already exist and work — there is no new
daemon code to write for this task. If it fails, that's a real
regression signal worth stopping on, not an "expected fail before
implementing" step like earlier tasks.

- [ ] **Step 3: Run the full test suite one final time**

Run: `cargo test`
Expected: all tests PASS across every module.

Run: `cargo clippy --all-targets 2>&1 | tail -30`
Expected: possibly a few `collapsible_if` warnings in the new code
(nested `if let Some(menu) = &mut self.attach_menu { if let Some(...) =
... { ... } }` patterns from Tasks 4-5) — if so, collapse them with `&&`
per clippy's own suggestion (e.g. `if let Some(menu) = &mut
self.attach_menu && let Some(rename) = &mut menu.rename { ... }`, using
Rust's `let`-chains). Otherwise, no new warnings beyond that; pre-existing
warnings elsewhere in the codebase, if any, are out of scope.

- [ ] **Step 4: Commit**

```bash
git add src/daemon/mod.rs
git commit -m "test: add attach menu delete/rename wire-protocol regression coverage"
```

---

### Task 8: Manual verification + update the design doc's keybind table

**Files:**
- Modify: `docs/superpowers/specs/2026-07-30-dimax-design.md` (the
  "Default keybinds (TUI)" table doesn't mention `x`/`r` since those are
  menu-local, not global chords — but the "Attach menu identification
  columns" section describes the now-outdated 5-column
  `name | cwd | process | id | status` layout and needs updating to
  match the 4-column grouped layout this feature ships)

- [ ] **Step 1: Update the design doc's "Attach menu identification columns" section**

In `docs/superpowers/specs/2026-07-30-dimax-design.md`, find the
paragraph starting "The attach menu (and `dimax server ls`) list every
server-pane with five columns: `name | cwd | process | id | status`."
Replace that sentence with:

```
The attach menu groups server-panes by working directory under
non-selectable header lines (one per distinct `cwd`, with a synthetic
"Unknown" group sorted last for panes with no resolvable `cwd` —
see docs/superpowers/specs/2026-08-03-attach-menu-groups-and-shortcuts-design.md).
Each row then shows four columns: `name | process | id | status` (`cwd`
moved to the group header, no longer repeated per row). `dimax server
ls`'s CLI output is unaffected — it still lists all five fields per row;
only the TUI attach menu groups/drops columns.
```

Leave the rest of that paragraph (about `name`'s id-prefix fallback,
`cwd`/`process` sourcing via `ServerPaneInfo.foreground`, and the
`process_group_leader`/`sysinfo` lookup mechanics) as-is — none of that
changed.

- [ ] **Step 2: Build the release binary and manually verify in a real terminal**

Run: `cargo build --release`
Expected: clean build.

Manually verify (requires Kitty, per the design doc's platform
constraint):
1. `cargo install --path .`
2. Run `dimax attach` inside Kitty.
3. Spawn 2-3 server-panes with different working directories (e.g.
   `cd /tmp && dimax server spawn` in one terminal, then `cd ~ &&
   dimax server spawn` in another, from outside the TUI via the CLI).
4. Press `cmd-shift-z` on a focused pane to open the attach menu.
5. Confirm: rows are grouped under cwd header lines, matching the
   working directories used above.
6. Press `x` on a row — confirm the "confirm delete" hint appears.
7. Press `j` (not `x`/Enter) — confirm the hint disappears AND selection
   moved down (both effects from one keypress).
8. Press `x` twice on a row — confirm that server-pane disappears from
   the list and the menu stays open.
9. Press `r` on a row — confirm an editable field appears pre-filled
   with the current name, cursor at the end.
10. Type a few characters, press `Left`/`Right`/`Home`/`End` — confirm
    the text edits correctly at each cursor position.
11. Press `Enter` — confirm the row's name updates and rename mode
    closes.
12. Press `r` again, clear the field, press `Enter` — confirm nothing
    happens (empty-name no-op) and the field stays open.
13. Press `Esc` — confirm rename mode closes without changing the name.

- [ ] **Step 3: Commit the doc update**

```bash
git add docs/superpowers/specs/2026-07-30-dimax-design.md
git commit -m "docs: update attach menu column description for cwd grouping"
```
