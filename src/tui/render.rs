//! Renders a workspace's split tree + bound server-pane grids into a
//! `ratatui::Frame`. Pure w.r.t. networking — takes already-fetched
//! state, never talks to the daemon itself.
//!
//! # `SplitDir` -> `ratatui::Direction` mapping
//!
//! The design doc's `SplitDir::Horizontal`/`Vertical` describes a split
//! the way tmux does: "split vertically" (`cmd-d`, see design doc "Default
//! keybinds") produces two panes side by side, stacked left/right, with a
//! *vertical* divider line between them; "split horizontally"
//! (`cmd-shift-d`) produces two panes stacked top/bottom, with a
//! *horizontal* divider line. That is, the `SplitDir` name refers to the
//! divider's orientation, not the axis the space gets cut along.
//!
//! `ratatui::layout::Direction` names the axis panes are laid out along
//! instead: `Direction::Horizontal` arranges children left-to-right
//! (side by side), `Direction::Vertical` arranges children top-to-bottom.
//!
//! So the mapping here is:
//! - `SplitDir::Vertical` (tmux "split vertically", divider is a vertical
//!   line, panes side by side) -> `Direction::Horizontal` (children laid
//!   out left/right).
//! - `SplitDir::Horizontal` (tmux "split horizontally", divider is a
//!   horizontal line, panes stacked) -> `Direction::Vertical` (children
//!   laid out top/bottom).
//!
//! This is the inverse-looking but correct pairing: `SplitDir` names the
//! divider's orientation, `Direction` names the layout axis, and a
//! vertical divider requires a horizontal (left/right) layout axis. Get
//! this backwards and every split renders transposed.
//!
//! # Bezels: shared dividers instead of per-pane boxes
//!
//! Every leaf draws only a top border (a 1-row title bar), not a full box
//! — the previous `Borders::ALL` meant two side-by-side panes each drew
//! their own left/right border, doubling into a 2-character-wide dead
//! seam at every split. Two cases now:
//! - `Direction::Horizontal` (side-by-side panes, vertical divider line):
//!   there is no natural shared edge to reuse, so `draw_tree` explicitly
//!   reserves a single column between the two children and paints a `│`
//!   divider into it.
//! - `Direction::Vertical` (stacked panes, horizontal divider line): the
//!   *lower* child's own top-border title bar already sits exactly on the
//!   boundary, so no extra row is reserved — one fewer thing to draw, one
//!   less row of dead space.

use crate::protocol::{
    Cell, ClientPane, GridSnapshot, ServerPaneId, ServerPaneInfo, ServerPaneStatus, SplitDir,
    SplitId, SplitTree, WorkspaceInfo,
};
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use std::collections::{HashMap, HashSet};

/// Reserved width, in columns, of the gap between two side-by-side panes:
/// one blank column of margin on each side of the `│` divider glyph
/// itself (see `draw_vertical_divider`). Stacked panes have no matching
/// constant -- module doc "Bezels" -- their boundary stays exactly the
/// lower pane's own title-bar row, unchanged.
const PANE_GAP: u16 = 3;

/// See module doc comment "`SplitDir` -> `ratatui::Direction` mapping".
fn ratatui_direction(dir: SplitDir) -> Direction {
    match dir {
        SplitDir::Vertical => Direction::Horizontal,
        SplitDir::Horizontal => Direction::Vertical,
    }
}

/// Draw `workspace`'s current split tree into `frame`, blitting each
/// bound client-pane's grid (looked up in `grids` by server-pane id) into
/// its computed rect. Panes with no binding, or whose server-pane isn't
/// in `grids`, render as an "unbound"/placeholder box (design doc "Error
/// handling").
pub fn draw(
    frame: &mut Frame,
    workspace: &WorkspaceInfo,
    grids: &HashMap<ServerPaneId, GridSnapshot>,
    names: &HashMap<ServerPaneId, ServerPaneInfo>,
    focused: Option<crate::protocol::ClientPaneId>,
) {
    draw_with_selection(frame, workspace, grids, names, focused, None);
}

pub(super) fn draw_with_selection(
    frame: &mut Frame,
    workspace: &WorkspaceInfo,
    grids: &HashMap<ServerPaneId, GridSnapshot>,
    names: &HashMap<ServerPaneId, ServerPaneInfo>,
    focused: Option<crate::protocol::ClientPaneId>,
    selection: Option<&super::selection::TextSelection>,
) {
    let area = frame.area();
    match &workspace.tree {
        Some(tree) => draw_tree(frame, tree, area, grids, names, focused, selection),
        None => {
            let placeholder = Paragraph::new("(empty workspace — press cmd-d to spawn a pane)")
                .block(Block::bordered().title("dimax"));
            frame.render_widget(placeholder, area);
        }
    }
}

fn draw_tree(
    frame: &mut Frame,
    tree: &SplitTree,
    area: Rect,
    grids: &HashMap<ServerPaneId, GridSnapshot>,
    names: &HashMap<ServerPaneId, ServerPaneInfo>,
    focused: Option<crate::protocol::ClientPaneId>,
    selection: Option<&super::selection::TextSelection>,
) {
    match tree {
        SplitTree::Leaf(pane) => draw_leaf(frame, pane, area, grids, names, focused, selection),
        SplitTree::Split {
            dir, ratio, a, b, ..
        } => {
            let direction = ratatui_direction(*dir);
            let percent_a = (ratio.clamp(0.0, 1.0) * 100.0).round() as u16;
            let percent_b = 100u16.saturating_sub(percent_a);

            // See module doc "Bezels: shared dividers instead of per-pane
            // boxes" — side-by-side panes need an explicit reserved
            // column for the divider; stacked panes don't (the lower
            // child's own title-bar row sits on the boundary for free).
            let (rect_a, rect_b) = match direction {
                Direction::Horizontal => {
                    let rects = Layout::new(
                        direction,
                        [
                            Constraint::Percentage(percent_a),
                            Constraint::Length(PANE_GAP),
                            Constraint::Percentage(percent_b),
                        ],
                    )
                    .split(area);
                    draw_vertical_divider(frame, rects[1]);
                    (rects[0], rects[2])
                }
                Direction::Vertical => {
                    let rects = Layout::new(
                        direction,
                        [
                            Constraint::Percentage(percent_a),
                            Constraint::Percentage(percent_b),
                        ],
                    )
                    .split(area);
                    (rects[0], rects[1])
                }
            };
            draw_tree(frame, a, rect_a, grids, names, focused, selection);
            draw_tree(frame, b, rect_b, grids, names, focused, selection);
        }
    }
}

/// Paint a `│` divider (the shared edge between two side-by-side panes)
/// centered into `area`, which `draw_tree` has already reserved as
/// `PANE_GAP` columns wide -- one blank column of margin on each side of
/// the glyph itself.
fn draw_vertical_divider(frame: &mut Frame, area: Rect) {
    let line = Line::from("│")
        .alignment(Alignment::Center)
        .style(Style::new());
    let text = Text::from(vec![line; area.height as usize]);
    frame.render_widget(Paragraph::new(text), area);
}

/// One divider's on-screen position: `grab_zone` is the thin rect a mouse
/// click must land in to grab it; `parent_area` is the full rect the
/// split divides (both children plus the divider), needed to convert a
/// later drag position into a new ratio along the split's axis.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DividerHit {
    pub split: SplitId,
    pub dir: SplitDir,
    pub grab_zone: Rect,
    pub parent_area: Rect,
}

/// Mirror of `draw_tree`'s layout math, but computing where every
/// divider's grab zone landed on screen instead of drawing anything —
/// pure and networking/frame-free so the TUI's mouse-drag hit-testing can
/// call it on every `MouseDown` without touching `ratatui::Frame` at all.
///
/// For a side-by-side split, the grab zone is the reserved 1-column
/// divider rect itself. For a stacked split, there is no reserved row
/// (module doc "Bezels") — the grab zone is instead the lower child's
/// title-bar row, which is exactly the boundary the drag visually moves.
pub fn divider_rects(tree: &SplitTree, area: Rect) -> Vec<DividerHit> {
    let mut out = Vec::new();
    collect_divider_rects(tree, area, &mut out);
    out
}

fn collect_divider_rects(tree: &SplitTree, area: Rect, out: &mut Vec<DividerHit>) {
    if let SplitTree::Split {
        id,
        dir,
        ratio,
        a,
        b,
    } = tree
    {
        let direction = ratatui_direction(*dir);
        let percent_a = (ratio.clamp(0.0, 1.0) * 100.0).round() as u16;
        let percent_b = 100u16.saturating_sub(percent_a);

        let (rect_a, rect_b, divider) = match direction {
            Direction::Horizontal => {
                let rects = Layout::new(
                    direction,
                    [
                        Constraint::Percentage(percent_a),
                        Constraint::Length(PANE_GAP),
                        Constraint::Percentage(percent_b),
                    ],
                )
                .split(area);
                (rects[0], rects[2], rects[1])
            }
            Direction::Vertical => {
                let rects = Layout::new(
                    direction,
                    [
                        Constraint::Percentage(percent_a),
                        Constraint::Percentage(percent_b),
                    ],
                )
                .split(area);
                // The lower child's title-bar row (its topmost row) is
                // the grab zone -- no reserved row exists to point at
                // directly (module doc "Bezels").
                let title_row = Rect {
                    height: 1,
                    ..rects[1]
                };
                (rects[0], rects[1], title_row)
            }
        };
        out.push(DividerHit {
            split: *id,
            dir: *dir,
            grab_zone: divider,
            parent_area: area,
        });
        collect_divider_rects(a, rect_a, out);
        collect_divider_rects(b, rect_b, out);
    }
}

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

fn collect_leaf_rects(
    tree: &SplitTree,
    area: Rect,
    out: &mut Vec<(crate::protocol::ClientPaneId, Rect)>,
) {
    match tree {
        SplitTree::Leaf(pane) => out.push((pane.id, area)),
        SplitTree::Split {
            dir, ratio, a, b, ..
        } => {
            let direction = ratatui_direction(*dir);
            let percent_a = (ratio.clamp(0.0, 1.0) * 100.0).round() as u16;
            let percent_b = 100u16.saturating_sub(percent_a);
            let (rect_a, rect_b) = match direction {
                Direction::Horizontal => {
                    let rects = Layout::new(
                        direction,
                        [
                            Constraint::Percentage(percent_a),
                            Constraint::Length(PANE_GAP),
                            Constraint::Percentage(percent_b),
                        ],
                    )
                    .split(area);
                    (rects[0], rects[2])
                }
                Direction::Vertical => {
                    let rects = Layout::new(
                        direction,
                        [
                            Constraint::Percentage(percent_a),
                            Constraint::Percentage(percent_b),
                        ],
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

/// Given a divider's `hit` (from [`divider_rects`]) and the current mouse
/// position, compute the new ratio a drag to that position implies —
/// the fraction of `hit.parent_area`'s span (along the split's axis)
/// that falls before the mouse position. Pure arithmetic; clamping to a
/// sane range happens server-side in `SplitTree::resize_split`, not here,
/// so the TUI can send the raw intended ratio and let the daemon be the
/// single source of truth for the clamp.
pub fn ratio_at(hit: &DividerHit, col: u16, row: u16) -> f32 {
    let direction = ratatui_direction(hit.dir);
    match direction {
        Direction::Horizontal => {
            let span = hit.parent_area.width.max(1) as f32;
            let offset = col.saturating_sub(hit.parent_area.x) as f32;
            offset / span
        }
        Direction::Vertical => {
            let span = hit.parent_area.height.max(1) as f32;
            let offset = row.saturating_sub(hit.parent_area.y) as f32;
            offset / span
        }
    }
}

fn draw_leaf(
    frame: &mut Frame,
    pane: &ClientPane,
    area: Rect,
    grids: &HashMap<ServerPaneId, GridSnapshot>,
    names: &HashMap<ServerPaneId, ServerPaneInfo>,
    focused: Option<crate::protocol::ClientPaneId>,
    selection: Option<&super::selection::TextSelection>,
) {
    let active = pane.active_bound();
    let snapshot = active.and_then(|server_pane_id| grids.get(&server_pane_id));
    // The title bar shows the *bound server-pane's* id/name (both --
    // `[short_id] name`, or just `[short_id]` when unnamed), not the
    // client-pane wrapper's own -- `ClientPane.name`/`.short_id` are a
    // separate identity for the grid leaf itself, which nothing ever
    // renames in practice, so showing those left every pane's title
    // permanently uninformative. Falls back to the client-pane's own
    // short id only when unbound, or when `names` hasn't caught up yet
    // (a few hundred ms after spawn/rebind -- see `App::refresh_server_names`).
    let mut title = match active.and_then(|id| names.get(&id)) {
        Some(server) => match &server.name {
            Some(name) => format!("[{}] {name}", server.short_id),
            None => format!("[{}]", server.short_id),
        },
        None => pane.short_id.clone(),
    };
    if pane.tabs.len() > 1 {
        title.push_str(&format!(" ({}/{})", pane.active_tab + 1, pane.tabs.len()));
    }
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

    match active {
        None => {
            let placeholder =
                Paragraph::new("(unbound — bind via `dimax client bind`)").block(block);
            frame.render_widget(placeholder, area);
        }
        Some(_) => match snapshot {
            Some(snapshot) => {
                let pane_selection = selection.filter(|selection| {
                    selection.pane() == pane.id && selection.server_pane() == snapshot.server_pane
                });
                let text = grid_to_text(snapshot, pane_selection);
                let inner = block.inner(area);
                frame.render_widget(Paragraph::new(text).block(block), area);
                if is_focused {
                    place_cursor(frame, snapshot, inner);
                }
            }
            None => {
                let placeholder = Paragraph::new("(server-pane closed)").block(block);
                frame.render_widget(placeholder, area);
            }
        },
    }
}

/// Position the terminal's real cursor at `snapshot`'s reported
/// `(col, row)`, translated into screen coordinates within `inner`
/// (the leaf's content rect, i.e. `area` minus its title-bar border) --
/// without this, `ratatui` never calls `Frame::set_cursor_position` for
/// anything, so the cursor stays wherever it was last drawn system-wide
/// (or hidden), regardless of which cell the foreground process
/// actually considers "current". This is what makes e.g. nvim's cursor
/// visible at all, distinct from and in addition to whatever cell-level
/// `reverse`/highlight attributes the app itself paints around it (a
/// `cursorline` full-row highlight is real cell content already handled
/// by `cell_to_span`; this is the separate blinking caret glyph).
///
/// Skipped (falls through to whatever `Frame::render_widget` already
/// left, i.e. hidden) when the pane is scrolled back -- the PTY's
/// cursor position is only meaningful against the *live* screen, and
/// would otherwise point at an arbitrary cell in history -- or when the
/// reported position has scrolled outside `inner`'s bounds (a
/// momentarily stale snapshot from just before a resize).
fn place_cursor(frame: &mut Frame, snapshot: &GridSnapshot, inner: Rect) {
    if snapshot.scroll_offset > 0 {
        return;
    }
    let (col, row) = snapshot.cursor;
    if col >= inner.width || row >= inner.height {
        return;
    }
    frame.set_cursor_position((inner.x + col, inner.y + row));
}

/// Convert a `GridSnapshot`'s row-major cell grid into a `ratatui::Text`,
/// one `Line` per row. Adjacent cells that resolve to the exact same
/// `Style` are merged into a single `Span` covering their combined text
/// rather than emitting one `Span` per `Cell` -- ratatui applies a
/// `Span`'s style to every character it contains regardless of how many
/// there are, so this changes nothing about what ends up on screen, only
/// how many `Span`/`String` allocations it costs to get there. Typical
/// terminal output is runs of identically-styled plain text (a whole
/// line of default-color shell output collapses to one `Span` instead
/// of one per character), which is where most of a large pane's render
/// cost was going -- measured at ~600µs for an 85x246 grid's worth of
/// individually-allocated single-character `Span`s before this change,
/// on every `terminal.draw()` call for every leaf currently visible,
/// not just the one whose content actually changed since the last frame
/// (see `draw_tree`'s doc comment -- there's no per-leaf dirty-tracking,
/// so this cost is paid by every visible pane on every frame regardless
/// of which pane's `GridDelta` triggered it).
fn grid_to_text(
    snapshot: &GridSnapshot,
    selection: Option<&super::selection::TextSelection>,
) -> Text<'static> {
    let lines: Vec<Line<'static>> = snapshot
        .lines
        .iter()
        .enumerate()
        .map(|(row_index, row)| row_to_line(row, row_index, selection))
        .collect();
    Text::from(lines)
}

fn row_to_line(
    row: &[Cell],
    row_index: usize,
    selection: Option<&super::selection::TextSelection>,
) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut run_text = String::new();
    let mut run_style: Option<Style> = None;
    for (col_index, cell) in row.iter().enumerate() {
        let selected = selection.is_some_and(|selection| selection.contains(row_index, col_index));
        let style = cell_style(cell, selected);
        match run_style {
            Some(current) if current == style => run_text.push_str(&cell.text),
            _ => {
                if let Some(current) = run_style.replace(style) {
                    spans.push(Span::styled(std::mem::take(&mut run_text), current));
                }
                run_text.push_str(&cell.text);
            }
        }
    }
    if let Some(style) = run_style {
        spans.push(Span::styled(run_text, style));
    }
    Line::from(spans)
}

fn cell_style(cell: &Cell, selected: bool) -> Style {
    let mut style = Style::new();
    if let Some((r, g, b)) = cell.fg {
        style = style.fg(Color::Rgb(r, g, b));
    }
    if let Some((r, g, b)) = cell.bg {
        style = style.bg(Color::Rgb(r, g, b));
    }
    let mut modifiers = Modifier::empty();
    if cell.bold {
        modifiers |= Modifier::BOLD;
    }
    if cell.italic {
        modifiers |= Modifier::ITALIC;
    }
    if cell.underline {
        modifiers |= Modifier::UNDERLINED;
    }
    if cell.reverse {
        modifiers |= Modifier::REVERSED;
    }
    style = style.add_modifier(modifiers);
    if selected {
        style = style
            .remove_modifier(Modifier::REVERSED)
            .fg(Color::White)
            .bg(Color::Rgb(70, 110, 170));
    }
    style
}

/// Fixed row count of the attach menu's preview panel (see
/// `draw_attach_menu`'s doc comment for why this is a constant, not
/// content-dependent) -- 2 for the panel's own bordered title bar +
/// bottom border, plus enough interior rows to show a handful of a
/// pane's most recent lines at a glance without the popup growing tall
/// enough to crowd out the row list above it.
const PREVIEW_PANEL_HEIGHT: u16 = 12;

/// Rendered along the row-list block's bottom border -- must stay in sync
/// with `parse_attach_menu_input`'s actual byte matches (`tui/mod.rs`),
/// which is the source of truth this is only a display of.
const ATTACH_MENU_KEY_HINTS: &str = "↑↓ move · Enter attach · x del · r rename · p pin · f agents · g group · d detach · q quit · Esc cancel";

/// Overlay for `cmd-shift-z`'s attach menu: lists every server-pane
/// (grouped under selectable per-cwd headers) plus a trailing "spawn
/// new" entry, opened after the focused client-pane has already been
/// detached from whatever it was previously bound to. `collapsed` names
/// which groups (by `group_key`) should have their member rows hidden --
/// lives on `App`, not `AttachMenu`, so it's threaded in as its own
/// parameter rather than a field read off `menu`. `preview` is the
/// currently cached `(server_pane, text)` pair from `App
/// ::refresh_attach_menu_preview`, rendered in a fixed-height panel
/// below the row list -- fixed so the row list's own layout never
/// shifts based on whether there's anything to preview right now (a
/// hard UI requirement: a resizing/reflowing list while navigating it
/// is disorienting). The panel exists and is exactly
/// `PREVIEW_PANEL_HEIGHT` rows whether or not `preview` matches the
/// current selection; it just renders blank when it doesn't.
pub(super) fn draw_attach_menu(
    frame: &mut Frame,
    menu: &super::AttachMenu,
    collapsed: &HashSet<String>,
    grouped: bool,
    preview: Option<&(ServerPaneId, String)>,
    pinned: &[String],
) {
    // Wider than the previous 60% -- each row now packs five columns
    // (name/tag/process/id/status, see `attach_menu_line`) rather than
    // the original two, and needs more horizontal room to avoid every
    // field being clipped down to near-nothing on an ordinary terminal
    // width.
    let popup_area = centered_rect(85, 60, frame.area());
    frame.render_widget(Clear, popup_area);

    // Fixed top/bottom split -- see this function's doc comment for why
    // the preview panel's presence and height never depend on content.
    // `Constraint::Min(0)` (rather than a computed remainder) is what
    // makes this genuinely fixed: the row list gets "whatever's left",
    // so it's the preview panel's height that's authoritative, not the
    // other way around.
    let [rows_area, preview_area] = Layout::new(
        Direction::Vertical,
        [Constraint::Min(0), Constraint::Length(PREVIEW_PANEL_HEIGHT)],
    )
    .areas(popup_area);

    let rows = super::visible_attach_menu_rows(&menu.servers, collapsed, grouped);
    let mut lines: Vec<Line<'static>> = Vec::with_capacity(rows.len() * 2);
    for (row_index, row) in rows.iter().enumerate() {
        match *row {
            super::AttachMenuRow::GroupHeader(server_index) => {
                let group = &menu.servers[server_index].0;
                let pinned_here = pinned.iter().any(|p| p == group);
                lines.push(group_header_line(
                    group,
                    collapsed.contains(group),
                    pinned_here,
                    row_index == menu.selected,
                ));
            }
            super::AttachMenuRow::Server(server_index) => {
                let (_, server) = &menu.servers[server_index];
                let armed = menu.pending_delete == Some(server_index);
                let renaming = menu.rename.as_ref().filter(|r| r.index == server_index);
                let just_detached = menu.previously_bound == Some(server.id);
                lines.push(attach_menu_line(
                    server,
                    row_index == menu.selected,
                    armed,
                    renaming,
                    just_detached,
                ));
                if let Some(rename) = renaming
                    && let Some(error) = &rename.error
                {
                    lines.push(Line::styled(
                        format!("    {error}"),
                        Style::new().fg(Color::Red),
                    ));
                }
            }
            super::AttachMenuRow::SpawnNewInGroup(server_index) => {
                let group = &menu.servers[server_index].0;
                let spawning = menu
                    .spawn_in_group
                    .as_ref()
                    .filter(|s| s.group_server_index == server_index);
                lines.push(spawn_new_in_group_line(
                    group,
                    row_index == menu.selected,
                    spawning,
                ));
                if let Some(spawn) = spawning
                    && let Some(error) = &spawn.error
                {
                    lines.push(Line::styled(
                        format!("    {error}"),
                        Style::new().fg(Color::Red),
                    ));
                }
            }
            super::AttachMenuRow::SpawnNew => {
                lines.push(spawn_new_line(row_index == menu.selected));
            }
        }
    }

    let block = Block::bordered()
        .title("Attach server-pane")
        .title_bottom(Line::from(ATTACH_MENU_KEY_HINTS).right_aligned());
    frame.render_widget(Paragraph::new(lines).block(block), rows_area);

    let selected_server = match rows.get(menu.selected) {
        Some(super::AttachMenuRow::Server(server_index)) => Some(&menu.servers[*server_index].1),
        _ => None,
    };
    let selected_server_id = selected_server.map(|s| s.id);
    let selected_name = selected_server.map(|s| s.name.as_deref().unwrap_or(s.short_id.as_str()));
    draw_attach_menu_preview(
        frame,
        selected_server_id,
        selected_name,
        preview,
        preview_area,
    );
}

/// The attach menu's fixed-height preview panel (see `draw_attach_menu`
/// for the layout rationale). Blank body -- title only -- in every case
/// where there's nothing meaningful to show: `selected` is `None` (the
/// selection isn't on a `Server` row), or `preview`'s cached pane doesn't
/// match `selected` (a stale fetch from just before the selection moved;
/// see `App::refresh_attach_menu_preview`'s doc comment on why this can
/// briefly happen and why showing nothing is correct rather than showing
/// the wrong pane's content for one frame). `selected_name` is that same
/// selected pane's custom name (or short id fallback, see
/// `draw_attach_menu`'s caller) -- shown as the panel's own title in
/// place of the generic "Preview" so it's clear at a glance whose output
/// is on screen, without having to look back up at the row list.
fn draw_attach_menu_preview(
    frame: &mut Frame,
    selected: Option<ServerPaneId>,
    selected_name: Option<&str>,
    preview: Option<&(ServerPaneId, String)>,
    area: Rect,
) {
    let full_text = selected
        .zip(preview)
        .filter(|(selected, (cached, _))| selected == cached)
        .map(|(_, (_, text))| text.as_str())
        .unwrap_or("");

    // The panel is only a handful of rows tall (see `PREVIEW_PANEL_HEIGHT`)
    // but `ServerRead` returns the pane's *entire* current screen, which
    // is routinely taller -- rendering it as-is via `Paragraph` (which
    // draws from the top, with no scroll) would only ever show the
    // pane's oldest visible rows, not its most recent output. Interior
    // height is `area.height` minus the block's own top+bottom border.
    let interior_rows = area.height.saturating_sub(2) as usize;
    let lines: Vec<&str> = full_text.lines().collect();
    let visible_text = lines[lines.len().saturating_sub(interior_rows)..].join("\n");

    let block = Block::bordered().title(selected_name.unwrap_or("Preview"));
    frame.render_widget(Paragraph::new(visible_text).block(block), area);
}

/// One directory-group header row: `group` (bold, as before headers were
/// selectable) prefixed with a disclosure marker (`▾` expanded, `▸`
/// collapsed) so the group's current state is visible without having to
/// select it first, and reverse-video highlighted when it's the current
/// selection -- matching how a real server-pane row highlights. `pinned`
/// prepends a 📌 marker ahead of the disclosure marker -- pinned groups
/// already sort first (see `App::group_servers_by_cwd`), so this is
/// purely a visual confirmation of that state, not what causes it.
fn group_header_line(group: &str, collapsed: bool, pinned: bool, selected: bool) -> Line<'static> {
    let disclosure = if collapsed { "▸" } else { "▾" };
    let pin_marker = if pinned { "📌" } else { "  " };
    let mut style = Style::new().add_modifier(Modifier::BOLD);
    if selected {
        style = style.add_modifier(Modifier::REVERSED);
    }
    Line::styled(format!("{pin_marker}{disclosure} {group}"), style)
}

/// A group's own "+ spawn new here" row, indented like a `Server` row
/// (matches its visual nesting under the group's header) rather than
/// the global `spawn_new_line`'s unindented top-level look. When
/// `spawning` is `Some` (its inline field is open), shows the field's
/// live text in brackets instead of the static label -- same visual
/// convention `attach_menu_line` uses for an active rename.
fn spawn_new_in_group_line(
    group: &str,
    selected: bool,
    spawning: Option<&super::SpawnInGroupState>,
) -> Line<'static> {
    let marker = if selected { ">" } else { " " };
    let text = match spawning {
        Some(spawn) => format!("  {marker} + [{}] in {group}", spawn.text),
        None => format!("  {marker} + spawn new in {group}"),
    };
    let style = if selected || spawning.is_some() {
        Style::new().add_modifier(Modifier::REVERSED)
    } else {
        Style::new()
    };
    Line::styled(text, style)
}

/// Column widths for the attach menu's server-pane rows: `name |
/// attached | tag | process | id | status` (a `cwd` column existed here
/// before rows were grouped under per-cwd header lines — see
/// `draw_attach_menu` — at which point showing it a second time per
/// row became redundant). `id` here is `ServerPaneInfo::short_id`
/// (`"aa"`, `"ab"`, ...), a sequential two-plus-character label
/// assigned once at spawn time — not the full UUID, which would
/// dominate the row for no benefit; the attach menu is for picking a
/// pane by eye, not by exact id.
const NAME_COL_WIDTH: usize = 28;
/// Fits `[opencode]` (10 chars), the longest of `session_tag`'s
/// possible outputs, with no truncation.
const TAG_COL_WIDTH: usize = 10;
/// Fits one binding formatted as `<ws>/<client-short>` (typically 4
/// characters: `1/aa`) plus a `+N` overflow marker for the multi-tab
/// case (e.g. `1/aa +2`). A `+` marks that the binding is a background
/// tab, not the client-pane's currently displayed one; a lone `-` means
/// unattached.
const ATTACHED_COL_WIDTH: usize = 10;
const PROCESS_COL_WIDTH: usize = 10;

/// `[claude]`/`[codex]`/etc. when `server`'s foreground process is a
/// recognized AI-coding CLI tool (see `protocol::SessionKind`), blank
/// otherwise -- a visible tag for the same classification `dimax
/// server ls`'s `kind` column exposes on the CLI side, so a recognized
/// session stands out at a glance in the row list too, not just in
/// scripted output.
fn session_tag(server: &ServerPaneInfo) -> String {
    match server.foreground.as_ref().and_then(|f| f.session_kind) {
        Some(kind) => format!("[{}]", kind.as_str()),
        None => String::new(),
    }
}

/// Rendered `attached` column value for the attach menu -- see
/// `ATTACHED_COL_WIDTH`. `-` for an unattached pane, `<ws>/<short>` for
/// a single-binding pane (`+` suffix if that one binding is a background
/// tab, not the currently displayed one -- so `1/aa` reads as "you can
/// see this pane right now on workspace 1's client-pane aa"; `1/aa+`
/// reads as "workspace 1's client-pane aa has it, but as a hidden
/// tab"). Multiple bindings render the first as above plus a compact
/// `+N` overflow marker for the rest, since a longer list would
/// overflow the column anyway.
fn attached_column(server: &ServerPaneInfo) -> String {
    let bindings = &server.attached_to;
    let Some(first) = bindings.first() else {
        return "-".to_string();
    };
    let bg_marker = if first.active { "" } else { "+" };
    let head = format!(
        "{}/{}{}",
        first.workspace_number, first.client_short_id, bg_marker
    );
    if bindings.len() == 1 {
        head
    } else {
        format!("{head} +{}", bindings.len() - 1)
    }
}

fn attach_menu_line(
    server: &ServerPaneInfo,
    selected: bool,
    delete_armed: bool,
    renaming: Option<&super::RenameState>,
    just_detached: bool,
) -> Line<'static> {
    let status = match server.status {
        ServerPaneStatus::Running => "Running",
        ServerPaneStatus::Dead => "Dead",
    };
    let process = server
        .foreground
        .as_ref()
        .map_or("-", |f| f.process_name.as_str());
    let tag = session_tag(server);
    let attached = attached_column(server);
    let marker = if selected { ">" } else { " " };
    // Marks the row this client-pane was bound to right before this
    // menu opened (see `AttachMenu.previously_bound`'s doc comment) --
    // rendered as its own leading character ahead of the `>`/` `
    // selection marker so it stays visible whether or not that row also
    // happens to be selected right now.
    let just_detached_marker = if just_detached { "*" } else { " " };

    if let Some(rename) = renaming {
        let text = format!(
            "  {just_detached_marker}{marker} [{}] {:<attached_w$} {:<tag_w$} {:<process_w$} {} {}",
            rename.text,
            truncate_end(&attached, ATTACHED_COL_WIDTH),
            tag,
            truncate_end(process, PROCESS_COL_WIDTH),
            server.short_id,
            status,
            attached_w = ATTACHED_COL_WIDTH,
            tag_w = TAG_COL_WIDTH,
            process_w = PROCESS_COL_WIDTH,
        );
        return Line::styled(text, Style::new().add_modifier(Modifier::REVERSED));
    }

    let name = server
        .name
        .clone()
        .unwrap_or_else(|| server.short_id.clone());
    let text = format!(
        "  {just_detached_marker}{marker} {:<name_w$} {:<attached_w$} {:<tag_w$} {:<process_w$} {} {}{}",
        truncate_end(&name, NAME_COL_WIDTH),
        truncate_end(&attached, ATTACHED_COL_WIDTH),
        tag,
        truncate_end(process, PROCESS_COL_WIDTH),
        server.short_id,
        status,
        if delete_armed {
            "  [x/Enter: confirm delete]"
        } else {
            ""
        },
        name_w = NAME_COL_WIDTH,
        attached_w = ATTACHED_COL_WIDTH,
        tag_w = TAG_COL_WIDTH,
        process_w = PROCESS_COL_WIDTH,
    );
    let style = match (selected, delete_armed) {
        (_, true) => Style::new().add_modifier(Modifier::REVERSED).fg(Color::Red),
        (true, false) => Style::new().add_modifier(Modifier::REVERSED),
        (false, false) => Style::new(),
    };
    Line::styled(text, style)
}

/// Truncate `s` to at most `width` characters, keeping the *start* (a
/// trailing ellipsis, e.g. `some-long-n...`) — used for `name`/`process`,
/// where the beginning is usually the more identifying part.
fn truncate_end(s: &str, width: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= width {
        return s.to_string();
    }
    if width <= 3 {
        return ".".repeat(width);
    }
    let head: String = chars[..width - 3].iter().collect();
    format!("{head}...")
}

fn spawn_new_line(selected: bool) -> Line<'static> {
    let text = format!("{} spawn new...", if selected { ">" } else { " " });
    let style = if selected {
        Style::new().add_modifier(Modifier::REVERSED)
    } else {
        Style::new()
    };
    Line::styled(text, style)
}

/// A rect covering `percent_x`% width and `percent_y`% height of `area`,
/// centered within it. Standard ratatui centering pattern: split into
/// thirds along each axis (with the requested percentage as the middle
/// share) and take the middle segment of each split.
pub(super) fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let vertical_margin = (100u16.saturating_sub(percent_y)) / 2;
    let rows = Layout::new(
        Direction::Vertical,
        [
            Constraint::Percentage(vertical_margin),
            Constraint::Percentage(percent_y),
            Constraint::Percentage(vertical_margin),
        ],
    )
    .split(area);

    let horizontal_margin = (100u16.saturating_sub(percent_x)) / 2;
    Layout::new(
        Direction::Horizontal,
        [
            Constraint::Percentage(horizontal_margin),
            Constraint::Percentage(percent_x),
            Constraint::Percentage(horizontal_margin),
        ],
    )
    .split(rows[1])[1]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{ForegroundProcessInfo, Size};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use uuid::Uuid;

    fn buffer_contains(terminal: &Terminal<TestBackend>, needle: &str) -> bool {
        let buffer = terminal.backend().buffer();
        let content: String = buffer.content().iter().map(|c| c.symbol()).collect();
        content.contains(needle)
    }

    /// `grid_to_text` merges adjacent identically-styled cells into one
    /// `Span` rather than emitting one per `Cell` (see its doc comment
    /// for why -- ratatui applies a `Span`'s style to every character it
    /// contains, so this is purely a render-cost optimization with no
    /// visible effect). Three cells, two styles: "ab" plain, "c" colored
    /// -- must produce exactly two spans, not three, with "ab" merged
    /// into a single `Span` rather than staying as two.
    #[test]
    fn grid_to_text_merges_adjacent_cells_with_identical_style() {
        let snapshot = GridSnapshot {
            server_pane: Uuid::new_v4(),
            size: Size { rows: 1, cols: 3 },
            cursor: (0, 0),
            lines: vec![vec![
                simple_cell("a"),
                simple_cell("b"),
                Cell {
                    text: "c".to_string(),
                    fg: Some((80, 200, 120)),
                    bg: None,
                    bold: false,
                    italic: false,
                    underline: false,
                    reverse: false,
                },
            ]],
            scroll_offset: 0,
        };

        let text = grid_to_text(&snapshot, None);
        assert_eq!(text.lines.len(), 1);
        let spans = &text.lines[0].spans;
        assert_eq!(
            spans.len(),
            2,
            "expected \"ab\" merged into one span and \"c\" as a second, distinctly-styled span: {spans:?}"
        );
        assert_eq!(spans[0].content, "ab");
        assert_eq!(spans[1].content, "c");
    }

    /// A style change caused purely by text selection (not by the cells'
    /// own styling) must still break the merge -- otherwise a selected
    /// run touching identically-styled unselected text on either side
    /// would incorrectly merge its highlight into neighboring cells.
    #[test]
    fn grid_to_text_breaks_merge_at_a_selection_boundary() {
        let snapshot = GridSnapshot {
            server_pane: Uuid::new_v4(),
            size: Size { rows: 1, cols: 3 },
            cursor: (0, 0),
            lines: vec![vec![simple_cell("a"), simple_cell("b"), simple_cell("c")]],
            scroll_offset: 0,
        };
        let pane_id = Uuid::new_v4();
        let mut selection = super::super::selection::TextSelection::new(
            pane_id,
            snapshot.server_pane,
            super::super::selection::GridPosition { row: 0, col: 1 },
        );
        selection.finish(super::super::selection::GridPosition { row: 0, col: 1 });

        let text = grid_to_text(&snapshot, Some(&selection));
        let spans = &text.lines[0].spans;
        assert_eq!(
            spans.len(),
            3,
            "\"a\" | \"b\" (selected) | \"c\" must stay three distinct spans: {spans:?}"
        );
        assert_eq!(spans[0].content, "a");
        assert_eq!(spans[1].content, "b");
        assert_eq!(spans[2].content, "c");
    }

    /// Reconstruct one row of `terminal`'s backend buffer as a plain
    /// `String`, picking the (first) row that contains `needle` --
    /// unlike `buffer_contains`, `buffer.content()` is a flat cell array
    /// with no row separators, so a substring search alone can't tell
    /// you which *row* a match landed in; this splits by `buffer.area
    /// .width` to recover row boundaries before searching.
    fn buffer_row_containing(terminal: &Terminal<TestBackend>, needle: &str) -> String {
        let buffer = terminal.backend().buffer();
        let width = buffer.area.width as usize;
        let symbols: Vec<&str> = buffer.content().iter().map(|c| c.symbol()).collect();
        symbols
            .chunks(width)
            .map(|row| row.concat())
            .find(|row| row.contains(needle))
            .unwrap_or_else(|| panic!("no row in the buffer contained {needle:?}"))
    }

    /// Like `buffer_row_containing`, but returns the row's index rather
    /// than its content -- for asserting a row's *position* stayed put
    /// across two renders, which comparing content alone can't do (the
    /// same text would match regardless of which row it landed on).
    fn buffer_row_index_containing(terminal: &Terminal<TestBackend>, needle: &str) -> usize {
        let buffer = terminal.backend().buffer();
        let width = buffer.area.width as usize;
        let symbols: Vec<&str> = buffer.content().iter().map(|c| c.symbol()).collect();
        symbols
            .chunks(width)
            .map(|row| row.concat())
            .position(|row| row.contains(needle))
            .unwrap_or_else(|| panic!("no row in the buffer contained {needle:?}"))
    }

    fn empty_workspace() -> WorkspaceInfo {
        WorkspaceInfo {
            id: Uuid::new_v4(),
            number: 1,
            name: None,
            tree: None,
        }
    }

    fn leaf_pane(bound: Option<ServerPaneId>) -> ClientPane {
        ClientPane {
            id: Uuid::new_v4(),
            name: None,
            tabs: bound.into_iter().collect(),
            active_tab: 0,
            short_id: "aa".to_string(),
        }
    }

    fn workspace_with_tree(tree: SplitTree) -> WorkspaceInfo {
        WorkspaceInfo {
            id: Uuid::new_v4(),
            number: 1,
            name: None,
            tree: Some(tree),
        }
    }

    fn simple_cell(text: &str) -> Cell {
        Cell {
            text: text.to_string(),
            fg: None,
            bg: None,
            bold: false,
            italic: false,
            underline: false,
            reverse: false,
        }
    }

    #[test]
    fn draw_empty_workspace_shows_placeholder() {
        let workspace = empty_workspace();
        let grids = HashMap::new();
        let backend = TestBackend::new(40, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| draw(frame, &workspace, &grids, &HashMap::new(), None))
            .unwrap();
        assert!(buffer_contains(&terminal, "empty workspace"));
    }

    #[test]
    fn draw_unbound_leaf_shows_placeholder() {
        let pane = leaf_pane(None);
        let workspace = workspace_with_tree(SplitTree::Leaf(pane));
        let grids = HashMap::new();
        let backend = TestBackend::new(40, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| draw(frame, &workspace, &grids, &HashMap::new(), None))
            .unwrap();
        assert!(buffer_contains(&terminal, "unbound"));
    }

    #[test]
    fn draw_leaf_falls_back_to_short_id_when_unnamed() {
        let pane = leaf_pane(None);
        let backend = TestBackend::new(40, 6);
        let mut terminal = Terminal::new(backend).unwrap();
        let grids = HashMap::new();
        terminal
            .draw(|frame| {
                draw_leaf(
                    frame,
                    &pane,
                    frame.area(),
                    &grids,
                    &HashMap::new(),
                    None,
                    None,
                )
            })
            .unwrap();
        assert!(buffer_contains(&terminal, &pane.short_id));
    }

    #[test]
    fn draw_bound_leaf_with_missing_snapshot_shows_closed_placeholder() {
        let server_pane_id = Uuid::new_v4();
        let pane = leaf_pane(Some(server_pane_id));
        let workspace = workspace_with_tree(SplitTree::Leaf(pane));
        let grids = HashMap::new();
        let backend = TestBackend::new(40, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| draw(frame, &workspace, &grids, &HashMap::new(), None))
            .unwrap();
        assert!(buffer_contains(&terminal, "server-pane closed"));
    }

    #[test]
    fn draw_bound_leaf_with_snapshot_renders_grid_text() {
        let server_pane_id = Uuid::new_v4();
        let pane = leaf_pane(Some(server_pane_id));
        let workspace = workspace_with_tree(SplitTree::Leaf(pane));
        let mut grids = HashMap::new();
        grids.insert(
            server_pane_id,
            GridSnapshot {
                server_pane: server_pane_id,
                size: Size { rows: 1, cols: 2 },
                cursor: (0, 0),
                lines: vec![vec![simple_cell("h"), simple_cell("i")]],
                scroll_offset: 0,
            },
        );
        let backend = TestBackend::new(40, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| draw(frame, &workspace, &grids, &HashMap::new(), None))
            .unwrap();
        assert!(buffer_contains(&terminal, "hi"));
    }

    #[test]
    fn draw_places_the_real_cursor_at_the_snapshots_reported_position_when_focused() {
        let pane_id = Uuid::new_v4();
        let server_pane_id = Uuid::new_v4();
        let pane = ClientPane {
            id: pane_id,
            name: None,
            tabs: vec![server_pane_id],
            active_tab: 0,
            short_id: "aa".to_string(),
        };
        let workspace = workspace_with_tree(SplitTree::Leaf(pane));
        let mut grids = HashMap::new();
        grids.insert(
            server_pane_id,
            GridSnapshot {
                server_pane: server_pane_id,
                size: Size { rows: 3, cols: 10 },
                cursor: (4, 1),
                lines: vec![vec![simple_cell(" "); 10]; 3],
                scroll_offset: 0,
            },
        );
        let backend = TestBackend::new(40, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| draw(frame, &workspace, &grids, &HashMap::new(), Some(pane_id)))
            .unwrap();
        // area starts at (0, 0); the leaf's top-border title bar occupies
        // row 0, so the content rect's origin is row 1 -- cursor (4, 1)
        // within it lands at absolute (4, 2).
        assert_eq!(
            terminal.get_cursor_position().unwrap(),
            ratatui::layout::Position { x: 4, y: 2 }
        );
        assert!(terminal.backend().cursor_visible());
    }

    #[test]
    fn draw_selection_highlights_only_selected_cells() {
        let pane_id = Uuid::new_v4();
        let server_pane_id = Uuid::new_v4();
        let pane = ClientPane {
            id: pane_id,
            name: Some("shell".to_string()),
            tabs: vec![server_pane_id],
            active_tab: 0,
            short_id: "aa".to_string(),
        };
        let workspace = workspace_with_tree(SplitTree::Leaf(pane));
        let mut grids = HashMap::new();
        grids.insert(
            server_pane_id,
            GridSnapshot {
                server_pane: server_pane_id,
                size: Size { rows: 1, cols: 3 },
                cursor: (0, 0),
                lines: vec![vec![simple_cell("a"), simple_cell("b"), simple_cell("c")]],
                scroll_offset: 0,
            },
        );
        let mut selection = super::super::selection::TextSelection::new(
            pane_id,
            server_pane_id,
            super::super::selection::GridPosition { row: 0, col: 1 },
        );
        selection.finish(super::super::selection::GridPosition { row: 0, col: 2 });

        let backend = TestBackend::new(10, 3);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                draw_with_selection(
                    frame,
                    &workspace,
                    &grids,
                    &HashMap::new(),
                    None,
                    Some(&selection),
                );
            })
            .unwrap();

        let buffer = terminal.backend().buffer();
        assert_eq!(buffer.cell((0, 1)).unwrap().bg, Color::Reset);
        assert_eq!(buffer.cell((1, 1)).unwrap().bg, Color::Rgb(70, 110, 170));
        assert_eq!(buffer.cell((2, 1)).unwrap().bg, Color::Rgb(70, 110, 170));
    }

    #[test]
    fn draw_does_not_place_the_cursor_when_the_leaf_is_not_focused() {
        let pane_id = Uuid::new_v4();
        let server_pane_id = Uuid::new_v4();
        let pane = ClientPane {
            id: pane_id,
            name: None,
            tabs: vec![server_pane_id],
            active_tab: 0,
            short_id: "aa".to_string(),
        };
        let workspace = workspace_with_tree(SplitTree::Leaf(pane));
        let mut grids = HashMap::new();
        grids.insert(
            server_pane_id,
            GridSnapshot {
                server_pane: server_pane_id,
                size: Size { rows: 3, cols: 10 },
                cursor: (4, 1),
                lines: vec![vec![simple_cell(" "); 10]; 3],
                scroll_offset: 0,
            },
        );
        let backend = TestBackend::new(40, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| draw(frame, &workspace, &grids, &HashMap::new(), None))
            .unwrap();
        assert!(
            !terminal.backend().cursor_visible(),
            "cursor should stay hidden when unfocused"
        );
    }

    #[test]
    fn draw_does_not_place_the_cursor_when_the_leaf_is_scrolled_back() {
        let pane_id = Uuid::new_v4();
        let server_pane_id = Uuid::new_v4();
        let pane = ClientPane {
            id: pane_id,
            name: None,
            tabs: vec![server_pane_id],
            active_tab: 0,
            short_id: "aa".to_string(),
        };
        let workspace = workspace_with_tree(SplitTree::Leaf(pane));
        let mut grids = HashMap::new();
        grids.insert(
            server_pane_id,
            GridSnapshot {
                server_pane: server_pane_id,
                size: Size { rows: 3, cols: 10 },
                cursor: (4, 1),
                lines: vec![vec![simple_cell(" "); 10]; 3],
                scroll_offset: 2,
            },
        );
        let backend = TestBackend::new(40, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| draw(frame, &workspace, &grids, &HashMap::new(), Some(pane_id)))
            .unwrap();
        assert!(
            !terminal.backend().cursor_visible(),
            "cursor should stay hidden while scrolled back"
        );
    }

    #[test]
    fn draw_leaf_shows_scrollback_indicator_when_scrolled() {
        let pane_id = Uuid::new_v4();
        let server_id = Uuid::new_v4();
        let pane = ClientPane {
            id: pane_id,
            name: Some("shell".to_string()),
            tabs: vec![server_id],
            active_tab: 0,
            short_id: "aa".to_string(),
        };
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
                draw_leaf(
                    frame,
                    &pane,
                    frame.area(),
                    &grids,
                    &HashMap::new(),
                    None,
                    None,
                );
            })
            .unwrap();
        assert!(buffer_contains(&terminal, "scrollback"));
    }

    #[test]
    fn draw_leaf_shows_no_scrollback_indicator_when_live() {
        let pane_id = Uuid::new_v4();
        let server_id = Uuid::new_v4();
        let pane = ClientPane {
            id: pane_id,
            name: Some("shell".to_string()),
            tabs: vec![server_id],
            active_tab: 0,
            short_id: "aa".to_string(),
        };
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
                draw_leaf(
                    frame,
                    &pane,
                    frame.area(),
                    &grids,
                    &HashMap::new(),
                    None,
                    None,
                );
            })
            .unwrap();
        assert!(!buffer_contains(&terminal, "scrollback"));
    }

    #[test]
    fn draw_leaf_shows_tab_count_only_when_multiple_tabs() {
        let pane_id = Uuid::new_v4();
        let sp1 = Uuid::new_v4();
        let sp2 = Uuid::new_v4();
        let pane = ClientPane {
            id: pane_id,
            name: Some("editor".to_string()),
            tabs: vec![sp1, sp2],
            active_tab: 0,
            short_id: "aa".to_string(),
        };
        let grids = HashMap::new();
        let backend = TestBackend::new(40, 6);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                draw_leaf(
                    frame,
                    &pane,
                    frame.area(),
                    &grids,
                    &HashMap::new(),
                    None,
                    None,
                )
            })
            .unwrap();
        assert!(buffer_contains(&terminal, "(1/2)"));
    }

    #[test]
    fn draw_leaf_hides_tab_count_for_single_tab() {
        let pane_id = Uuid::new_v4();
        let sp = Uuid::new_v4();
        let pane = ClientPane {
            id: pane_id,
            name: None,
            tabs: vec![sp],
            active_tab: 0,
            short_id: "aa".to_string(),
        };
        let grids = HashMap::new();
        let mut names = HashMap::new();
        names.insert(
            sp,
            ServerPaneInfo {
                id: sp,
                name: Some("shell".to_string()),
                size: Size { rows: 24, cols: 80 },
                status: ServerPaneStatus::Running,
                foreground: None,
                short_id: "aa".to_string(),
                attached_to: Vec::new(),
            },
        );
        let backend = TestBackend::new(40, 6);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| draw_leaf(frame, &pane, frame.area(), &grids, &names, None, None))
            .unwrap();
        assert!(!buffer_contains(&terminal, "(1/1)"));
        assert!(buffer_contains(&terminal, "shell"));
    }

    #[test]
    fn draw_split_produces_no_panic_and_both_leaves_render() {
        let left_id = Uuid::new_v4();
        let right_id = Uuid::new_v4();
        let left = ClientPane {
            id: Uuid::new_v4(),
            name: None,
            tabs: vec![left_id],
            active_tab: 0,
            short_id: "aa".to_string(),
        };
        let right = ClientPane {
            id: Uuid::new_v4(),
            name: None,
            tabs: vec![right_id],
            active_tab: 0,
            short_id: "aa".to_string(),
        };
        let mut names = HashMap::new();
        names.insert(
            left_id,
            ServerPaneInfo {
                id: left_id,
                name: Some("left".to_string()),
                size: Size { rows: 1, cols: 4 },
                status: ServerPaneStatus::Running,
                foreground: None,
                short_id: "aa".to_string(),
                attached_to: Vec::new(),
            },
        );
        names.insert(
            right_id,
            ServerPaneInfo {
                id: right_id,
                name: Some("right".to_string()),
                size: Size { rows: 1, cols: 5 },
                status: ServerPaneStatus::Running,
                foreground: None,
                short_id: "ab".to_string(),
                attached_to: Vec::new(),
            },
        );
        let tree = SplitTree::Split {
            id: Uuid::new_v4(),
            dir: SplitDir::Vertical,
            ratio: 0.5,
            a: Box::new(SplitTree::Leaf(left)),
            b: Box::new(SplitTree::Leaf(right)),
        };
        let workspace = workspace_with_tree(tree);
        let mut grids = HashMap::new();
        grids.insert(
            left_id,
            GridSnapshot {
                server_pane: left_id,
                size: Size { rows: 1, cols: 4 },
                cursor: (0, 0),
                lines: vec![vec![
                    simple_cell("L"),
                    simple_cell("E"),
                    simple_cell("F"),
                    simple_cell("T"),
                ]],
                scroll_offset: 0,
            },
        );
        grids.insert(
            right_id,
            GridSnapshot {
                server_pane: right_id,
                size: Size { rows: 1, cols: 5 },
                cursor: (0, 0),
                lines: vec![vec![
                    simple_cell("R"),
                    simple_cell("I"),
                    simple_cell("G"),
                    simple_cell("H"),
                    simple_cell("T"),
                ]],
                scroll_offset: 0,
            },
        );
        let backend = TestBackend::new(40, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| draw(frame, &workspace, &grids, &names, None))
            .unwrap();
        assert!(buffer_contains(&terminal, "LEFT"));
        assert!(buffer_contains(&terminal, "RIGHT"));
        // Also confirm the two leaves' titles ended up on opposite halves
        // (left half contains "left", right half contains "right").
        let buffer = terminal.backend().buffer();
        let width = buffer.area().width;
        let mut left_half = String::new();
        let mut right_half = String::new();
        for y in 0..buffer.area().height {
            for x in 0..width {
                let symbol = buffer.cell((x, y)).unwrap().symbol();
                if x < width / 2 {
                    left_half.push_str(symbol);
                } else {
                    right_half.push_str(symbol);
                }
            }
        }
        assert!(left_half.contains("left"));
        assert!(right_half.contains("right"));
    }

    #[test]
    fn vertical_split_draws_single_shared_divider_column() {
        // A vertical split (side-by-side panes) reserves exactly one
        // divider column between the two children -- not two (one from
        // each pane's own border), per module doc "Bezels".
        let left = ClientPane {
            id: Uuid::new_v4(),
            name: None,
            tabs: vec![],
            active_tab: 0,
            short_id: "aa".to_string(),
        };
        let right = ClientPane {
            id: Uuid::new_v4(),
            name: None,
            tabs: vec![],
            active_tab: 0,
            short_id: "aa".to_string(),
        };
        let tree = SplitTree::Split {
            id: Uuid::new_v4(),
            dir: SplitDir::Vertical,
            ratio: 0.5,
            a: Box::new(SplitTree::Leaf(left)),
            b: Box::new(SplitTree::Leaf(right)),
        };
        let workspace = workspace_with_tree(tree);
        let grids = HashMap::new();
        let backend = TestBackend::new(41, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| draw(frame, &workspace, &grids, &HashMap::new(), None))
            .unwrap();

        let buffer = terminal.backend().buffer();
        // Row 1 (below the title-bar row) is pure pane body on both sides
        // of the divider -- count how many columns in that row are the
        // divider glyph. Exactly one column should be "│".
        let divider_columns = (0..buffer.area().width)
            .filter(|&x| buffer.cell((x, 1)).unwrap().symbol() == "│")
            .count();
        assert_eq!(
            divider_columns, 1,
            "expected exactly one shared divider column, not a doubled seam"
        );
    }

    #[test]
    fn leaf_uses_top_only_border_not_a_full_box() {
        // A single leaf should not draw side/bottom border glyphs -- only
        // the top title-bar row, per module doc "Bezels".
        let pane = ClientPane {
            id: Uuid::new_v4(),
            name: Some("solo".into()),
            tabs: vec![],
            active_tab: 0,
            short_id: "aa".to_string(),
        };
        let workspace = workspace_with_tree(SplitTree::Leaf(pane));
        let grids = HashMap::new();
        let backend = TestBackend::new(20, 6);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| draw(frame, &workspace, &grids, &HashMap::new(), None))
            .unwrap();

        let buffer = terminal.backend().buffer();
        let last_row = buffer.area().height - 1;
        for x in 0..buffer.area().width {
            let symbol = buffer.cell((x, last_row)).unwrap().symbol();
            assert_ne!(symbol, "└", "bottom border should not be drawn");
            assert_ne!(symbol, "┘", "bottom border should not be drawn");
            assert_ne!(symbol, "─", "bottom border should not be drawn");
        }
    }

    #[test]
    fn divider_rects_finds_vertical_split_at_reserved_column() {
        let left = ClientPane {
            id: Uuid::new_v4(),
            name: None,
            tabs: vec![],
            active_tab: 0,
            short_id: "aa".to_string(),
        };
        let right = ClientPane {
            id: Uuid::new_v4(),
            name: None,
            tabs: vec![],
            active_tab: 0,
            short_id: "aa".to_string(),
        };
        let split_id = Uuid::new_v4();
        let tree = SplitTree::Split {
            id: split_id,
            dir: SplitDir::Vertical,
            ratio: 0.5,
            a: Box::new(SplitTree::Leaf(left)),
            b: Box::new(SplitTree::Leaf(right)),
        };
        let area = Rect {
            x: 0,
            y: 0,
            width: 41,
            height: 10,
        };
        let hits = divider_rects(&tree, area);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].split, split_id);
        assert_eq!(hits[0].dir, SplitDir::Vertical);
        // 41 wide, 50/50 split -> percent_a=50% of 41 rounds to 20 or 21,
        // then a PANE_GAP-column divider; the exact column depends on
        // ratatui's rounding, but it must land inside the area and be
        // PANE_GAP columns wide.
        assert_eq!(hits[0].grab_zone.width, PANE_GAP);
        assert_eq!(hits[0].grab_zone.height, 10);
        assert_eq!(hits[0].parent_area, area);
    }

    #[test]
    fn divider_rects_finds_horizontal_split_at_lower_titlebar_row() {
        let top = ClientPane {
            id: Uuid::new_v4(),
            name: None,
            tabs: vec![],
            active_tab: 0,
            short_id: "aa".to_string(),
        };
        let bottom = ClientPane {
            id: Uuid::new_v4(),
            name: None,
            tabs: vec![],
            active_tab: 0,
            short_id: "aa".to_string(),
        };
        let split_id = Uuid::new_v4();
        let tree = SplitTree::Split {
            id: split_id,
            dir: SplitDir::Horizontal,
            ratio: 0.5,
            a: Box::new(SplitTree::Leaf(top)),
            b: Box::new(SplitTree::Leaf(bottom)),
        };
        let area = Rect {
            x: 0,
            y: 0,
            width: 20,
            height: 11,
        };
        let hits = divider_rects(&tree, area);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].dir, SplitDir::Horizontal);
        // No reserved row for a stacked split -- the grab zone is exactly
        // one row tall (the lower pane's title bar).
        assert_eq!(hits[0].grab_zone.height, 1);
        assert_eq!(hits[0].grab_zone.width, 20);
    }

    #[test]
    fn ratio_at_maps_position_across_the_parent_area() {
        let hit = DividerHit {
            split: Uuid::new_v4(),
            dir: SplitDir::Vertical,
            grab_zone: Rect {
                x: 50,
                y: 0,
                width: 1,
                height: 10,
            },
            parent_area: Rect {
                x: 0,
                y: 0,
                width: 100,
                height: 10,
            },
        };
        assert!((ratio_at(&hit, 0, 0) - 0.0).abs() < 0.01);
        assert!((ratio_at(&hit, 50, 0) - 0.5).abs() < 0.01);
        assert!((ratio_at(&hit, 99, 0) - 0.99).abs() < 0.02);
    }

    #[test]
    fn ratio_at_uses_row_for_horizontal_split_dir() {
        let hit = DividerHit {
            split: Uuid::new_v4(),
            dir: SplitDir::Horizontal,
            grab_zone: Rect {
                x: 0,
                y: 5,
                width: 10,
                height: 1,
            },
            parent_area: Rect {
                x: 0,
                y: 0,
                width: 10,
                height: 10,
            },
        };
        assert!((ratio_at(&hit, 0, 5) - 0.5).abs() < 0.01);
    }

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
                session_kind: None,
            }),
            short_id: "aa".to_string(),
            attached_to: Vec::new(),
        };
        let servers = vec![("/home/dev/project".to_string(), server)];
        let menu = super::super::AttachMenu {
            servers,
            selected: 0,
            pending_delete: None,
            rename: None,
            previously_bound: None,
            spawn_in_group: None,
            adding_tab: false,
        };
        // Wide enough that the popup (85% of frame width, see
        // draw_attach_menu) comfortably fits every column's full width
        // rather than clipping mid-row -- a narrower backend was exactly
        // what caused this test to flake when the columns were added.
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| draw_attach_menu(frame, &menu, &HashSet::new(), true, None, &[]))
            .unwrap();
        assert!(buffer_contains(&terminal, "editor"));
        assert!(buffer_contains(&terminal, "vim"));
        assert!(buffer_contains(&terminal, "/home/dev/project"));
        assert!(buffer_contains(&terminal, "spawn new"));
        assert!(buffer_contains(&terminal, "Attach server-pane"));
    }

    #[test]
    fn session_tag_is_bracketed_kind_or_blank() {
        let mut server = ServerPaneInfo {
            id: Uuid::new_v4(),
            name: None,
            size: Size { rows: 24, cols: 80 },
            status: ServerPaneStatus::Running,
            foreground: Some(ForegroundProcessInfo {
                process_name: "claude".to_string(),
                cwd: None,
                session_kind: Some(crate::protocol::SessionKind::Claude),
            }),
            short_id: "aa".to_string(),
            attached_to: Vec::new(),
        };
        assert_eq!(session_tag(&server), "[claude]");

        server.foreground = Some(ForegroundProcessInfo {
            process_name: "bash".to_string(),
            cwd: None,
            session_kind: None,
        });
        assert_eq!(session_tag(&server), "");

        server.foreground = None;
        assert_eq!(session_tag(&server), "");
    }

    #[test]
    fn draw_attach_menu_shows_recognized_session_tag() {
        let server = ServerPaneInfo {
            id: Uuid::new_v4(),
            name: Some("my-session".to_string()),
            size: Size { rows: 24, cols: 80 },
            status: ServerPaneStatus::Running,
            foreground: Some(ForegroundProcessInfo {
                process_name: "claude".to_string(),
                cwd: Some("/home/dev/project".to_string()),
                session_kind: Some(crate::protocol::SessionKind::Claude),
            }),
            short_id: "aa".to_string(),
            attached_to: Vec::new(),
        };
        let servers = vec![("/home/dev/project".to_string(), server)];
        let menu = super::super::AttachMenu {
            servers,
            selected: 0,
            pending_delete: None,
            rename: None,
            previously_bound: None,
            spawn_in_group: None,
            adding_tab: false,
        };
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| draw_attach_menu(frame, &menu, &HashSet::new(), true, None, &[]))
            .unwrap();
        assert!(buffer_contains(&terminal, "my-session"));
        assert!(buffer_contains(&terminal, "[claude]"));
    }

    #[test]
    fn draw_attach_menu_shows_no_tag_for_an_unrecognized_process() {
        let server = ServerPaneInfo {
            id: Uuid::new_v4(),
            name: Some("plain-shell".to_string()),
            size: Size { rows: 24, cols: 80 },
            status: ServerPaneStatus::Running,
            foreground: Some(ForegroundProcessInfo {
                process_name: "bash".to_string(),
                cwd: Some("/home/dev".to_string()),
                session_kind: None,
            }),
            short_id: "aa".to_string(),
            attached_to: Vec::new(),
        };
        let servers = vec![("/home/dev".to_string(), server)];
        let menu = super::super::AttachMenu {
            servers,
            selected: 0,
            pending_delete: None,
            rename: None,
            previously_bound: None,
            spawn_in_group: None,
            adding_tab: false,
        };
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| draw_attach_menu(frame, &menu, &HashSet::new(), true, None, &[]))
            .unwrap();
        assert!(buffer_contains(&terminal, "plain-shell"));
        assert!(!buffer_contains(&terminal, "["));
    }

    #[test]
    fn attached_column_renders_dash_for_no_bindings() {
        let server = ServerPaneInfo {
            id: Uuid::new_v4(),
            name: None,
            size: Size { rows: 24, cols: 80 },
            status: ServerPaneStatus::Running,
            foreground: None,
            short_id: "aa".to_string(),
            attached_to: Vec::new(),
        };
        assert_eq!(super::attached_column(&server), "-");
    }

    #[test]
    fn attached_column_renders_active_binding() {
        let server = ServerPaneInfo {
            id: Uuid::new_v4(),
            name: None,
            size: Size { rows: 24, cols: 80 },
            status: ServerPaneStatus::Running,
            foreground: None,
            short_id: "aa".to_string(),
            attached_to: vec![crate::protocol::AttachedBinding {
                workspace_number: 1,
                client_short_id: "aa".to_string(),
                active: true,
            }],
        };
        assert_eq!(super::attached_column(&server), "1/aa");
    }

    #[test]
    fn attached_column_marks_background_tab_with_plus() {
        let server = ServerPaneInfo {
            id: Uuid::new_v4(),
            name: None,
            size: Size { rows: 24, cols: 80 },
            status: ServerPaneStatus::Running,
            foreground: None,
            short_id: "aa".to_string(),
            attached_to: vec![crate::protocol::AttachedBinding {
                workspace_number: 2,
                client_short_id: "ab".to_string(),
                active: false,
            }],
        };
        assert_eq!(super::attached_column(&server), "2/ab+");
    }

    #[test]
    fn attached_column_shows_overflow_count_for_multiple_bindings() {
        let server = ServerPaneInfo {
            id: Uuid::new_v4(),
            name: None,
            size: Size { rows: 24, cols: 80 },
            status: ServerPaneStatus::Running,
            foreground: None,
            short_id: "aa".to_string(),
            attached_to: vec![
                crate::protocol::AttachedBinding {
                    workspace_number: 1,
                    client_short_id: "aa".to_string(),
                    active: true,
                },
                crate::protocol::AttachedBinding {
                    workspace_number: 2,
                    client_short_id: "ab".to_string(),
                    active: false,
                },
                crate::protocol::AttachedBinding {
                    workspace_number: 3,
                    client_short_id: "ac".to_string(),
                    active: false,
                },
            ],
        };
        assert_eq!(super::attached_column(&server), "1/aa +2");
    }

    #[test]
    fn draw_attach_menu_shows_the_attached_column() {
        let server = ServerPaneInfo {
            id: Uuid::new_v4(),
            name: Some("attached-session".to_string()),
            size: Size { rows: 24, cols: 80 },
            status: ServerPaneStatus::Running,
            foreground: None,
            short_id: "aa".to_string(),
            attached_to: vec![crate::protocol::AttachedBinding {
                workspace_number: 1,
                client_short_id: "aa".to_string(),
                active: true,
            }],
        };
        let menu = super::super::AttachMenu {
            servers: vec![("Unknown".to_string(), server)],
            selected: 0,
            pending_delete: None,
            rename: None,
            previously_bound: None,
            spawn_in_group: None,
            adding_tab: false,
        };
        let backend = TestBackend::new(120, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| draw_attach_menu(frame, &menu, &HashSet::new(), true, None, &[]))
            .unwrap();
        assert!(buffer_contains(&terminal, "1/aa"));
    }

    #[test]
    fn draw_attach_menu_shows_key_hints() {
        let menu = super::super::AttachMenu {
            servers: vec![],
            selected: 0,
            pending_delete: None,
            rename: None,
            previously_bound: None,
            spawn_in_group: None,
            adding_tab: false,
        };
        // Wide enough that the full (now longer, with "g group"/"q quit"
        // added) hint string fits within the popup's 85%-width interior.
        let backend = TestBackend::new(140, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| draw_attach_menu(frame, &menu, &HashSet::new(), true, None, &[]))
            .unwrap();
        assert!(buffer_contains(&terminal, "move"));
        assert!(buffer_contains(&terminal, "attach"));
        assert!(buffer_contains(&terminal, "cancel"));
    }

    #[test]
    fn draw_attach_menu_key_hints_do_not_panic_on_a_narrow_popup() {
        let menu = super::super::AttachMenu {
            servers: vec![],
            selected: 0,
            pending_delete: None,
            rename: None,
            previously_bound: None,
            spawn_in_group: None,
            adding_tab: false,
        };
        let backend = TestBackend::new(40, 15);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| draw_attach_menu(frame, &menu, &HashSet::new(), true, None, &[]))
            .unwrap();
    }

    #[test]
    fn draw_attach_menu_preview_shows_text_when_it_matches_the_selected_pane() {
        let server = ServerPaneInfo {
            id: Uuid::new_v4(),
            name: Some("editor".to_string()),
            size: Size { rows: 24, cols: 80 },
            status: ServerPaneStatus::Running,
            foreground: None,
            short_id: "aa".to_string(),
            attached_to: Vec::new(),
        };
        let selected_id = server.id;
        // Row 0 is the "Unknown" group header; row 1 is the server row.
        let menu = super::super::AttachMenu {
            servers: vec![("Unknown".to_string(), server)],
            selected: 1,
            pending_delete: None,
            rename: None,
            previously_bound: None,
            spawn_in_group: None,
            adding_tab: false,
        };
        let preview = (selected_id, "some live pane output".to_string());
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                draw_attach_menu(frame, &menu, &HashSet::new(), true, Some(&preview), &[])
            })
            .unwrap();
        assert!(buffer_contains(&terminal, "some live pane output"));
        // The panel's own title is the selected pane's custom name, not
        // the generic "Preview" label -- see `draw_attach_menu_preview`.
        assert!(buffer_contains(&terminal, "editor"));
    }

    #[test]
    fn draw_attach_menu_preview_blank_when_cached_pane_does_not_match_selection() {
        let server = ServerPaneInfo {
            id: Uuid::new_v4(),
            name: Some("editor".to_string()),
            size: Size { rows: 24, cols: 80 },
            status: ServerPaneStatus::Running,
            foreground: None,
            short_id: "aa".to_string(),
            attached_to: Vec::new(),
        };
        // Row 0 is the "Unknown" group header; row 1 is the server row
        // -- selecting the server row itself is what makes this a real
        // test of "selection is on a Server row, but the cached
        // preview is for a *different* pane," not just "nothing is
        // selected at all."
        let menu = super::super::AttachMenu {
            servers: vec![("Unknown".to_string(), server)],
            selected: 1,
            pending_delete: None,
            rename: None,
            previously_bound: None,
            spawn_in_group: None,
            adding_tab: false,
        };
        // Stale preview from some other (now-unselected) pane -- must
        // not be shown for a pane it wasn't fetched for.
        let stale_preview = (
            Uuid::new_v4(),
            "stale output from a different pane".to_string(),
        );
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                draw_attach_menu(
                    frame,
                    &menu,
                    &HashSet::new(),
                    true,
                    Some(&stale_preview),
                    &[],
                )
            })
            .unwrap();
        assert!(!buffer_contains(
            &terminal,
            "stale output from a different pane"
        ));
    }

    #[test]
    fn draw_attach_menu_preview_blank_when_selection_is_not_a_server_row() {
        let server = ServerPaneInfo {
            id: Uuid::new_v4(),
            name: Some("editor".to_string()),
            size: Size { rows: 24, cols: 80 },
            status: ServerPaneStatus::Running,
            foreground: Some(ForegroundProcessInfo {
                process_name: "vim".to_string(),
                cwd: Some("/home/dev/project".to_string()),
                session_kind: None,
            }),
            short_id: "aa".to_string(),
            attached_to: Vec::new(),
        };
        let server_id = server.id;
        let menu = super::super::AttachMenu {
            servers: vec![("/home/dev/project".to_string(), server)],
            // Row 0 is the group header, not the server row.
            selected: 0,
            pending_delete: None,
            rename: None,
            previously_bound: None,
            spawn_in_group: None,
            adding_tab: false,
        };
        let preview = (
            server_id,
            "should not appear while a header is selected".to_string(),
        );
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                draw_attach_menu(frame, &menu, &HashSet::new(), true, Some(&preview), &[])
            })
            .unwrap();
        assert!(!buffer_contains(
            &terminal,
            "should not appear while a header is selected"
        ));
    }

    #[test]
    fn draw_attach_menu_preview_panel_height_is_the_same_with_or_without_content() {
        // The hard UI requirement this enforces: the row list's own
        // area must not shift depending on whether there's anything to
        // preview -- verified here by confirming the popup's top-left
        // corner (where its border is drawn) lands on the identical
        // screen row in both cases.
        let server = ServerPaneInfo {
            id: Uuid::new_v4(),
            name: Some("editor".to_string()),
            size: Size { rows: 24, cols: 80 },
            status: ServerPaneStatus::Running,
            foreground: None,
            short_id: "aa".to_string(),
            attached_to: Vec::new(),
        };
        let server_id = server.id;
        let menu = super::super::AttachMenu {
            servers: vec![("Unknown".to_string(), server)],
            selected: 0,
            pending_delete: None,
            rename: None,
            previously_bound: None,
            spawn_in_group: None,
            adding_tab: false,
        };

        let backend_empty = TestBackend::new(100, 30);
        let mut terminal_empty = Terminal::new(backend_empty).unwrap();
        terminal_empty
            .draw(|frame| draw_attach_menu(frame, &menu, &HashSet::new(), true, None, &[]))
            .unwrap();
        let row_index_empty = buffer_row_index_containing(&terminal_empty, "editor");

        let preview = (server_id, "some content".to_string());
        let backend_filled = TestBackend::new(100, 30);
        let mut terminal_filled = Terminal::new(backend_filled).unwrap();
        terminal_filled
            .draw(|frame| {
                draw_attach_menu(frame, &menu, &HashSet::new(), true, Some(&preview), &[])
            })
            .unwrap();
        let row_index_filled = buffer_row_index_containing(&terminal_filled, "editor");

        assert_eq!(
            row_index_empty, row_index_filled,
            "the server row must land on the identical screen row whether or not the preview panel has content"
        );
    }

    #[test]
    fn draw_attach_menu_preview_shows_the_last_lines_not_the_first_when_content_overflows() {
        let server = ServerPaneInfo {
            id: Uuid::new_v4(),
            name: Some("editor".to_string()),
            size: Size { rows: 24, cols: 80 },
            status: ServerPaneStatus::Running,
            foreground: None,
            short_id: "aa".to_string(),
            attached_to: Vec::new(),
        };
        let selected_id = server.id;
        let menu = super::super::AttachMenu {
            servers: vec![("Unknown".to_string(), server)],
            selected: 1,
            pending_delete: None,
            rename: None,
            previously_bound: None,
            spawn_in_group: None,
            adding_tab: false,
        };
        // More lines than the preview panel's interior can hold
        // (PREVIEW_PANEL_HEIGHT is 10, i.e. 8 interior rows) -- a naive
        // top-down render would show "line-0" and cut off before ever
        // reaching the pane's actual current/most-recent output.
        let full_text = (0..20)
            .map(|i| format!("line-{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let preview = (selected_id, full_text);
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                draw_attach_menu(frame, &menu, &HashSet::new(), true, Some(&preview), &[])
            })
            .unwrap();
        assert!(
            buffer_contains(&terminal, "line-19"),
            "the most recent line must be visible"
        );
        assert!(
            !buffer_row_matches_exactly(&terminal, "line-0"),
            "the earliest line should have scrolled off, not just be a substring of a later one"
        );
    }

    /// Like `buffer_contains`, but requires an exact (trimmed) row match
    /// rather than a substring anywhere in the flattened buffer --
    /// needed to tell "line-0" is genuinely absent as its own row rather
    /// than merely not being a substring of "line-10"/"line-0"-adjacent
    /// text (a plain substring check for "line-0" would actually still
    /// find it inside "line-01"..."line-09" if those existed, though
    /// they don't here -- this is the more precise tool regardless).
    fn buffer_row_matches_exactly(terminal: &Terminal<TestBackend>, needle: &str) -> bool {
        let buffer = terminal.backend().buffer();
        let width = buffer.area.width as usize;
        let symbols: Vec<&str> = buffer.content().iter().map(|c| c.symbol()).collect();
        symbols
            .chunks(width)
            .map(|row| row.concat())
            .any(|row| row.trim() == needle)
    }

    #[test]
    fn draw_attach_menu_shows_dash_for_unknown_foreground() {
        let server = ServerPaneInfo {
            id: Uuid::new_v4(),
            name: Some("editor".to_string()),
            size: Size { rows: 24, cols: 80 },
            status: ServerPaneStatus::Dead,
            foreground: None,
            short_id: "aa".to_string(),
            attached_to: Vec::new(),
        };
        let servers = vec![("Unknown".to_string(), server)];
        let menu = super::super::AttachMenu {
            servers,
            selected: 0,
            pending_delete: None,
            rename: None,
            previously_bound: None,
            spawn_in_group: None,
            adding_tab: false,
        };
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| draw_attach_menu(frame, &menu, &HashSet::new(), true, None, &[]))
            .unwrap();
        assert!(buffer_contains(&terminal, "editor"));
        assert!(buffer_contains(&terminal, "-"));
    }

    #[test]
    fn truncate_end_keeps_the_head_with_a_trailing_ellipsis() {
        assert_eq!(truncate_end("a-very-long-process-name", 10), "a-very-...");
        assert_eq!(truncate_end("vim", 10), "vim");
        assert_eq!(truncate_end("exact-widt", 10), "exact-widt");
    }

    #[test]
    fn draw_attach_menu_spawn_new_selected_does_not_panic() {
        let menu = super::super::AttachMenu {
            servers: vec![],
            selected: 0,
            pending_delete: None,
            rename: None,
            previously_bound: None,
            spawn_in_group: None,
            adding_tab: false,
        };
        let backend = TestBackend::new(60, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| draw_attach_menu(frame, &menu, &HashSet::new(), true, None, &[]))
            .unwrap();
        assert!(buffer_contains(&terminal, "spawn new"));
    }

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
                session_kind: None,
            }),
            short_id: "aa".to_string(),
            attached_to: Vec::new(),
        };
        let b = ServerPaneInfo {
            id: Uuid::new_v4(),
            name: Some("web-shell".to_string()),
            size: Size { rows: 24, cols: 80 },
            status: ServerPaneStatus::Running,
            foreground: Some(ForegroundProcessInfo {
                process_name: "bash".to_string(),
                cwd: Some("/home/dev/web".to_string()),
                session_kind: None,
            }),
            short_id: "aa".to_string(),
            attached_to: Vec::new(),
        };
        let servers = vec![
            ("/home/dev/api".to_string(), a),
            ("/home/dev/web".to_string(), b),
        ];
        let menu = super::super::AttachMenu {
            servers,
            selected: 0,
            pending_delete: None,
            rename: None,
            previously_bound: None,
            spawn_in_group: None,
            adding_tab: false,
        };
        // Tall enough that the row list still has room for all 7 rows
        // (2 headers + 2 servers + 2 per-group spawn rows + the global
        // spawn-new) below the now-taller PREVIEW_PANEL_HEIGHT panel.
        let backend = TestBackend::new(100, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| draw_attach_menu(frame, &menu, &HashSet::new(), true, None, &[]))
            .unwrap();
        assert!(buffer_contains(&terminal, "/home/dev/api"));
        assert!(buffer_contains(&terminal, "/home/dev/web"));
        assert!(buffer_contains(&terminal, "api-shell"));
        assert!(buffer_contains(&terminal, "web-shell"));
    }

    #[test]
    fn draw_attach_menu_shows_a_spawn_row_per_real_group_but_not_for_unknown() {
        let a = ServerPaneInfo {
            id: Uuid::new_v4(),
            name: Some("api-shell".to_string()),
            size: Size { rows: 24, cols: 80 },
            status: ServerPaneStatus::Running,
            foreground: Some(ForegroundProcessInfo {
                process_name: "bash".to_string(),
                cwd: Some("/home/dev/api".to_string()),
                session_kind: None,
            }),
            short_id: "aa".to_string(),
            attached_to: Vec::new(),
        };
        let dead = ServerPaneInfo {
            id: Uuid::new_v4(),
            name: Some("dead-pane".to_string()),
            size: Size { rows: 24, cols: 80 },
            status: ServerPaneStatus::Dead,
            foreground: None,
            short_id: "aa".to_string(),
            attached_to: Vec::new(),
        };
        let servers = vec![
            ("/home/dev/api".to_string(), a),
            ("Unknown".to_string(), dead),
        ];
        let menu = super::super::AttachMenu {
            servers,
            selected: 0,
            pending_delete: None,
            rename: None,
            previously_bound: None,
            spawn_in_group: None,
            adding_tab: false,
        };
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| draw_attach_menu(frame, &menu, &HashSet::new(), true, None, &[]))
            .unwrap();
        assert!(buffer_contains(&terminal, "+ spawn new in /home/dev/api"));
        assert!(!buffer_contains(&terminal, "+ spawn new in Unknown"));
    }

    #[test]
    fn draw_attach_menu_collapsed_group_hides_its_rows_but_keeps_its_header() {
        let a = ServerPaneInfo {
            id: Uuid::new_v4(),
            name: Some("api-shell".to_string()),
            size: Size { rows: 24, cols: 80 },
            status: ServerPaneStatus::Running,
            foreground: Some(ForegroundProcessInfo {
                process_name: "bash".to_string(),
                cwd: Some("/home/dev/api".to_string()),
                session_kind: None,
            }),
            short_id: "aa".to_string(),
            attached_to: Vec::new(),
        };
        let b = ServerPaneInfo {
            id: Uuid::new_v4(),
            name: Some("web-shell".to_string()),
            size: Size { rows: 24, cols: 80 },
            status: ServerPaneStatus::Running,
            foreground: Some(ForegroundProcessInfo {
                process_name: "bash".to_string(),
                cwd: Some("/home/dev/web".to_string()),
                session_kind: None,
            }),
            short_id: "aa".to_string(),
            attached_to: Vec::new(),
        };
        let servers = vec![
            ("/home/dev/api".to_string(), a),
            ("/home/dev/web".to_string(), b),
        ];
        let menu = super::super::AttachMenu {
            servers,
            selected: 0,
            pending_delete: None,
            rename: None,
            previously_bound: None,
            spawn_in_group: None,
            adding_tab: false,
        };
        let mut collapsed = HashSet::new();
        collapsed.insert("/home/dev/api".to_string());
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| draw_attach_menu(frame, &menu, &collapsed, true, None, &[]))
            .unwrap();
        // The collapsed group's header stays visible (so it can be
        // re-expanded), but its member row AND its own "spawn new
        // here" row are both gone.
        assert!(buffer_contains(&terminal, "/home/dev/api"));
        assert!(!buffer_contains(&terminal, "api-shell"));
        assert!(!buffer_contains(&terminal, "spawn new in /home/dev/api"));
        // The other, uncollapsed group is unaffected.
        assert!(buffer_contains(&terminal, "/home/dev/web"));
        assert!(buffer_contains(&terminal, "web-shell"));
        assert!(buffer_contains(&terminal, "spawn new in /home/dev/web"));
    }

    #[test]
    fn draw_attach_menu_group_header_disclosure_marker_reflects_collapsed_state() {
        let a = ServerPaneInfo {
            id: Uuid::new_v4(),
            name: Some("api-shell".to_string()),
            size: Size { rows: 24, cols: 80 },
            status: ServerPaneStatus::Running,
            foreground: Some(ForegroundProcessInfo {
                process_name: "bash".to_string(),
                cwd: Some("/home/dev/api".to_string()),
                session_kind: None,
            }),
            short_id: "aa".to_string(),
            attached_to: Vec::new(),
        };
        let menu = super::super::AttachMenu {
            servers: vec![("/home/dev/api".to_string(), a)],
            selected: 0,
            pending_delete: None,
            rename: None,
            previously_bound: None,
            spawn_in_group: None,
            adding_tab: false,
        };

        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| draw_attach_menu(frame, &menu, &HashSet::new(), true, None, &[]))
            .unwrap();
        assert!(
            buffer_contains(&terminal, "▾"),
            "expanded group should show the expanded marker"
        );

        let mut collapsed = HashSet::new();
        collapsed.insert("/home/dev/api".to_string());
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| draw_attach_menu(frame, &menu, &collapsed, true, None, &[]))
            .unwrap();
        assert!(
            buffer_contains(&terminal, "▸"),
            "collapsed group should show the collapsed marker"
        );
    }

    #[test]
    fn draw_attach_menu_group_header_shows_pin_marker_only_when_pinned() {
        let a = ServerPaneInfo {
            id: Uuid::new_v4(),
            name: Some("api-shell".to_string()),
            size: Size { rows: 24, cols: 80 },
            status: ServerPaneStatus::Running,
            foreground: Some(ForegroundProcessInfo {
                process_name: "bash".to_string(),
                cwd: Some("/home/dev/api".to_string()),
                session_kind: None,
            }),
            short_id: "aa".to_string(),
            attached_to: Vec::new(),
        };
        let menu = super::super::AttachMenu {
            servers: vec![("/home/dev/api".to_string(), a)],
            selected: 0,
            pending_delete: None,
            rename: None,
            previously_bound: None,
            spawn_in_group: None,
            adding_tab: false,
        };

        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| draw_attach_menu(frame, &menu, &HashSet::new(), true, None, &[]))
            .unwrap();
        assert!(
            !buffer_contains(&terminal, "📌"),
            "unpinned group should show no pin marker"
        );

        let pinned = vec!["/home/dev/api".to_string()];
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| draw_attach_menu(frame, &menu, &HashSet::new(), true, None, &pinned))
            .unwrap();
        assert!(
            buffer_contains(&terminal, "📌"),
            "pinned group should show the pin marker"
        );
    }

    #[test]
    fn draw_attach_menu_shows_delete_confirm_hint_on_armed_row() {
        let server = ServerPaneInfo {
            id: Uuid::new_v4(),
            name: Some("editor".to_string()),
            size: Size { rows: 24, cols: 80 },
            status: ServerPaneStatus::Running,
            foreground: None,
            short_id: "aa".to_string(),
            attached_to: Vec::new(),
        };
        let menu = super::super::AttachMenu {
            servers: vec![("Unknown".to_string(), server)],
            selected: 0,
            pending_delete: Some(0),
            rename: None,
            previously_bound: None,
            spawn_in_group: None,
            adding_tab: false,
        };
        // Wide enough that the row (now with tag + attached columns)
        // plus the delete-confirm suffix all fit within the popup's
        // 85%-width interior without truncating.
        let backend = TestBackend::new(140, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| draw_attach_menu(frame, &menu, &HashSet::new(), true, None, &[]))
            .unwrap();
        assert!(buffer_contains(&terminal, "confirm delete"));
    }

    #[test]
    fn draw_attach_menu_marks_the_just_detached_row() {
        let detached_from = Uuid::new_v4();
        let other = Uuid::new_v4();
        let server_a = ServerPaneInfo {
            id: detached_from,
            name: Some("detached".to_string()),
            size: Size { rows: 24, cols: 80 },
            status: ServerPaneStatus::Running,
            foreground: None,
            short_id: "aa".to_string(),
            attached_to: Vec::new(),
        };
        let server_b = ServerPaneInfo {
            id: other,
            name: Some("other-pane".to_string()),
            size: Size { rows: 24, cols: 80 },
            status: ServerPaneStatus::Running,
            foreground: None,
            short_id: "aa".to_string(),
            attached_to: Vec::new(),
        };
        let menu = super::super::AttachMenu {
            servers: vec![
                ("Unknown".to_string(), server_a),
                ("Unknown".to_string(), server_b),
            ],
            selected: 1,
            pending_delete: None,
            rename: None,
            previously_bound: Some(detached_from),
            spawn_in_group: None,
            adding_tab: false,
        };
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| draw_attach_menu(frame, &menu, &HashSet::new(), true, None, &[]))
            .unwrap();
        let marked_row = buffer_row_containing(&terminal, "detached");
        let unmarked_row = buffer_row_containing(&terminal, "other-pane");
        assert!(
            marked_row.contains('*'),
            "row previously bound should carry the * marker: {marked_row:?}"
        );
        assert!(
            !unmarked_row.contains('*'),
            "an unrelated row must not carry the marker: {unmarked_row:?}"
        );
    }

    #[test]
    fn draw_attach_menu_shows_no_marker_when_nothing_was_previously_bound() {
        let server = ServerPaneInfo {
            id: Uuid::new_v4(),
            name: Some("fresh".to_string()),
            size: Size { rows: 24, cols: 80 },
            status: ServerPaneStatus::Running,
            foreground: None,
            short_id: "aa".to_string(),
            attached_to: Vec::new(),
        };
        let menu = super::super::AttachMenu {
            servers: vec![("Unknown".to_string(), server)],
            selected: 0,
            pending_delete: None,
            rename: None,
            previously_bound: None,
            spawn_in_group: None,
            adding_tab: false,
        };
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| draw_attach_menu(frame, &menu, &HashSet::new(), true, None, &[]))
            .unwrap();
        let row = buffer_row_containing(&terminal, "fresh");
        assert!(
            !row.contains('*'),
            "no row should be marked when previously_bound is None: {row:?}"
        );
    }

    #[test]
    fn draw_attach_menu_shows_live_rename_text_and_error() {
        let server = ServerPaneInfo {
            id: Uuid::new_v4(),
            name: Some("old-name".to_string()),
            size: Size { rows: 24, cols: 80 },
            status: ServerPaneStatus::Running,
            foreground: None,
            short_id: "aa".to_string(),
            attached_to: Vec::new(),
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
            previously_bound: None,
            spawn_in_group: None,
            adding_tab: false,
        };
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| draw_attach_menu(frame, &menu, &HashSet::new(), true, None, &[]))
            .unwrap();
        assert!(buffer_contains(&terminal, "new-name"));
        assert!(buffer_contains(&terminal, "name taken"));
    }

    #[test]
    fn leaf_rects_single_leaf_returns_the_whole_area() {
        let id = Uuid::new_v4();
        let tree = SplitTree::Leaf(ClientPane {
            id,
            name: None,
            tabs: vec![],
            active_tab: 0,
            short_id: "aa".to_string(),
        });
        let area = Rect {
            x: 0,
            y: 0,
            width: 80,
            height: 24,
        };
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
            a: Box::new(SplitTree::Leaf(ClientPane {
                id: a,
                name: None,
                tabs: vec![],
                active_tab: 0,
                short_id: "aa".to_string(),
            })),
            b: Box::new(SplitTree::Leaf(ClientPane {
                id: b,
                name: None,
                tabs: vec![],
                active_tab: 0,
                short_id: "aa".to_string(),
            })),
        };
        let area = Rect {
            x: 0,
            y: 0,
            width: 81,
            height: 24,
        };
        let rects = leaf_rects(&tree, area);
        assert_eq!(rects.len(), 2);
        let rect_a = rects.iter().find(|(id, _)| *id == a).unwrap().1;
        let rect_b = rects.iter().find(|(id, _)| *id == b).unwrap().1;
        // 81-wide area, 50/50 split, minus the PANE_GAP-column reserved
        // divider -- same math draw_tree already uses for SplitDir::Vertical.
        assert_eq!(rect_a.width + rect_b.width + PANE_GAP, 81);
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
            a: Box::new(SplitTree::Leaf(ClientPane {
                id: a,
                name: None,
                tabs: vec![],
                active_tab: 0,
                short_id: "aa".to_string(),
            })),
            b: Box::new(SplitTree::Leaf(ClientPane {
                id: b,
                name: None,
                tabs: vec![],
                active_tab: 0,
                short_id: "aa".to_string(),
            })),
        };
        let area = Rect {
            x: 0,
            y: 0,
            width: 80,
            height: 24,
        };
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
}
