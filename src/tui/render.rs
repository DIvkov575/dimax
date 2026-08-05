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
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::Frame;
use std::collections::{HashMap, HashSet};

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
    focused: Option<crate::protocol::ClientPaneId>,
) {
    let area = frame.area();
    match &workspace.tree {
        Some(tree) => draw_tree(frame, tree, area, grids, focused),
        None => {
            let placeholder = Paragraph::new("(empty workspace — press cmd-d to spawn a pane)")
                .block(Block::bordered().title("dimux"));
            frame.render_widget(placeholder, area);
        }
    }
}

fn draw_tree(
    frame: &mut Frame,
    tree: &SplitTree,
    area: Rect,
    grids: &HashMap<ServerPaneId, GridSnapshot>,
    focused: Option<crate::protocol::ClientPaneId>,
) {
    match tree {
        SplitTree::Leaf(pane) => draw_leaf(frame, pane, area, grids, focused),
        SplitTree::Split { dir, ratio, a, b, .. } => {
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
                            Constraint::Length(1),
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
                        [Constraint::Percentage(percent_a), Constraint::Percentage(percent_b)],
                    )
                    .split(area);
                    (rects[0], rects[1])
                }
            };
            draw_tree(frame, a, rect_a, grids, focused);
            draw_tree(frame, b, rect_b, grids, focused);
        }
    }
}

/// Paint a single-column `│` divider (the shared edge between two
/// side-by-side panes) into `area`, which `draw_tree` has already
/// reserved as exactly one column wide.
fn draw_vertical_divider(frame: &mut Frame, area: Rect) {
    let line = Line::from("│").style(Style::new());
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
    if let SplitTree::Split { id, dir, ratio, a, b } = tree {
        let direction = ratatui_direction(*dir);
        let percent_a = (ratio.clamp(0.0, 1.0) * 100.0).round() as u16;
        let percent_b = 100u16.saturating_sub(percent_a);

        let (rect_a, rect_b, divider) = match direction {
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
                (rects[0], rects[2], rects[1])
            }
            Direction::Vertical => {
                let rects = Layout::new(
                    direction,
                    [Constraint::Percentage(percent_a), Constraint::Percentage(percent_b)],
                )
                .split(area);
                // The lower child's title-bar row (its topmost row) is
                // the grab zone -- no reserved row exists to point at
                // directly (module doc "Bezels").
                let title_row = Rect { height: 1, ..rects[1] };
                (rects[0], rects[1], title_row)
            }
        };
        out.push(DividerHit { split: *id, dir: *dir, grab_zone: divider, parent_area: area });
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

/// Short id prefix used as a fallback title/label when a pane/server-pane
/// has no human-assigned name.
fn short_id(id: uuid::Uuid) -> String {
    id.to_string()[..8].to_string()
}

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

/// Convert a `GridSnapshot`'s row-major cell grid into a `ratatui::Text`,
/// one `Line` per row and one styled `Span` per `Cell`.
fn grid_to_text(snapshot: &GridSnapshot) -> Text<'static> {
    let lines: Vec<Line<'static>> = snapshot
        .lines
        .iter()
        .map(|row| {
            let spans: Vec<Span<'static>> =
                row.iter().map(|cell| cell_to_span(cell)).collect();
            Line::from(spans)
        })
        .collect();
    Text::from(lines)
}

fn cell_to_span(cell: &Cell) -> Span<'static> {
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
    Span::styled(cell.text.clone(), style)
}

/// Overlay for `cmd-shift-z`'s attach menu: lists every server-pane
/// (grouped under selectable per-cwd headers) plus a trailing "spawn
/// new" entry, opened after the focused client-pane has already been
/// detached from whatever it was previously bound to. `collapsed` names
/// which groups (by `group_key`) should have their member rows hidden --
/// lives on `App`, not `AttachMenu`, so it's threaded in as its own
/// parameter rather than a field read off `menu`.
pub(super) fn draw_attach_menu(frame: &mut Frame, menu: &super::AttachMenu, collapsed: &HashSet<String>) {
    // Wider than the previous 60% -- each row now packs four columns
    // (name/process/id/status, see `attach_menu_line`) rather than the
    // original two, and needs more horizontal room to avoid every field
    // being clipped down to near-nothing on an ordinary terminal width.
    let area = centered_rect(85, 60, frame.area());
    frame.render_widget(Clear, area);

    let rows = super::visible_attach_menu_rows(&menu.servers, collapsed);
    let mut lines: Vec<Line<'static>> = Vec::with_capacity(rows.len() * 2);
    for (row_index, row) in rows.iter().enumerate() {
        match *row {
            super::AttachMenuRow::GroupHeader(server_index) => {
                let group = &menu.servers[server_index].0;
                lines.push(group_header_line(group, collapsed.contains(group), row_index == menu.selected));
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
                    lines.push(Line::styled(format!("    {error}"), Style::new().fg(Color::Red)));
                }
            }
            super::AttachMenuRow::SpawnNewInGroup(server_index) => {
                let group = &menu.servers[server_index].0;
                lines.push(spawn_new_in_group_line(group, row_index == menu.selected));
            }
            super::AttachMenuRow::SpawnNew => {
                lines.push(spawn_new_line(row_index == menu.selected));
            }
        }
    }

    let block = Block::bordered().title("Attach server-pane");
    frame.render_widget(Paragraph::new(lines).block(block), area);
}

/// One directory-group header row: `group` (bold, as before headers were
/// selectable) prefixed with a disclosure marker (`▾` expanded, `▸`
/// collapsed) so the group's current state is visible without having to
/// select it first, and reverse-video highlighted when it's the current
/// selection -- matching how a real server-pane row highlights.
fn group_header_line(group: &str, collapsed: bool, selected: bool) -> Line<'static> {
    let marker = if collapsed { "▸" } else { "▾" };
    let mut style = Style::new().add_modifier(Modifier::BOLD);
    if selected {
        style = style.add_modifier(Modifier::REVERSED);
    }
    Line::styled(format!("{marker} {group}"), style)
}

/// A group's own "+ spawn new here" row, indented like a `Server` row
/// (matches its visual nesting under the group's header) rather than
/// the global `spawn_new_line`'s unindented top-level look.
fn spawn_new_in_group_line(group: &str, selected: bool) -> Line<'static> {
    let marker = if selected { ">" } else { " " };
    let text = format!("  {marker} + spawn new in {group}");
    let style = if selected { Style::new().add_modifier(Modifier::REVERSED) } else { Style::new() };
    Line::styled(text, style)
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
    let process = server.foreground.as_ref().map_or("-", |f| f.process_name.as_str());
    let marker = if selected { ">" } else { " " };
    // Marks the row this client-pane was bound to right before this
    // menu opened (see `AttachMenu.previously_bound`'s doc comment) --
    // rendered as its own leading character ahead of the `>`/` `
    // selection marker so it stays visible whether or not that row also
    // happens to be selected right now.
    let just_detached_marker = if just_detached { "*" } else { " " };

    if let Some(rename) = renaming {
        let text = format!(
            "  {just_detached_marker}{marker} [{}] {:<process_w$} {} {}",
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
        "  {just_detached_marker}{marker} {:<name_w$} {:<process_w$} {} {}{}",
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
    let style = if selected { Style::new().add_modifier(Modifier::REVERSED) } else { Style::new() };
    Line::styled(text, style)
}

/// A rect covering `percent_x`% width and `percent_y`% height of `area`,
/// centered within it. Standard ratatui centering pattern: split into
/// thirds along each axis (with the requested percentage as the middle
/// share) and take the middle segment of each split.
fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
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
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    use uuid::Uuid;

    fn buffer_contains(terminal: &Terminal<TestBackend>, needle: &str) -> bool {
        let buffer = terminal.backend().buffer();
        let content: String = buffer.content().iter().map(|c| c.symbol()).collect();
        content.contains(needle)
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
            bound,
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
            .draw(|frame| draw(frame, &workspace, &grids, None))
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
            .draw(|frame| draw(frame, &workspace, &grids, None))
            .unwrap();
        assert!(buffer_contains(&terminal, "unbound"));
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
            .draw(|frame| draw(frame, &workspace, &grids, None))
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
            .draw(|frame| draw(frame, &workspace, &grids, None))
            .unwrap();
        assert!(buffer_contains(&terminal, "hi"));
    }

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

    #[test]
    fn draw_split_produces_no_panic_and_both_leaves_render() {
        let left_id = Uuid::new_v4();
        let right_id = Uuid::new_v4();
        let left = ClientPane { id: Uuid::new_v4(), name: Some("left".into()), bound: Some(left_id) };
        let right = ClientPane { id: Uuid::new_v4(), name: Some("right".into()), bound: Some(right_id) };
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
            .draw(|frame| draw(frame, &workspace, &grids, None))
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
        let left = ClientPane { id: Uuid::new_v4(), name: None, bound: None };
        let right = ClientPane { id: Uuid::new_v4(), name: None, bound: None };
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
        terminal.draw(|frame| draw(frame, &workspace, &grids, None)).unwrap();

        let buffer = terminal.backend().buffer();
        // Row 1 (below the title-bar row) is pure pane body on both sides
        // of the divider -- count how many columns in that row are the
        // divider glyph. Exactly one column should be "│".
        let divider_columns = (0..buffer.area().width)
            .filter(|&x| buffer.cell((x, 1)).unwrap().symbol() == "│")
            .count();
        assert_eq!(divider_columns, 1, "expected exactly one shared divider column, not a doubled seam");
    }

    #[test]
    fn leaf_uses_top_only_border_not_a_full_box() {
        // A single leaf should not draw side/bottom border glyphs -- only
        // the top title-bar row, per module doc "Bezels".
        let pane = ClientPane { id: Uuid::new_v4(), name: Some("solo".into()), bound: None };
        let workspace = workspace_with_tree(SplitTree::Leaf(pane));
        let grids = HashMap::new();
        let backend = TestBackend::new(20, 6);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(frame, &workspace, &grids, None)).unwrap();

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
        let left = ClientPane { id: Uuid::new_v4(), name: None, bound: None };
        let right = ClientPane { id: Uuid::new_v4(), name: None, bound: None };
        let split_id = Uuid::new_v4();
        let tree = SplitTree::Split {
            id: split_id,
            dir: SplitDir::Vertical,
            ratio: 0.5,
            a: Box::new(SplitTree::Leaf(left)),
            b: Box::new(SplitTree::Leaf(right)),
        };
        let area = Rect { x: 0, y: 0, width: 41, height: 10 };
        let hits = divider_rects(&tree, area);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].split, split_id);
        assert_eq!(hits[0].dir, SplitDir::Vertical);
        // 41 wide, 50/50 split -> percent_a=50% of 41 rounds to 20 or 21,
        // then a 1-column divider; the exact column depends on ratatui's
        // rounding, but it must land inside the area and be 1 column wide.
        assert_eq!(hits[0].grab_zone.width, 1);
        assert_eq!(hits[0].grab_zone.height, 10);
        assert_eq!(hits[0].parent_area, area);
    }

    #[test]
    fn divider_rects_finds_horizontal_split_at_lower_titlebar_row() {
        let top = ClientPane { id: Uuid::new_v4(), name: None, bound: None };
        let bottom = ClientPane { id: Uuid::new_v4(), name: None, bound: None };
        let split_id = Uuid::new_v4();
        let tree = SplitTree::Split {
            id: split_id,
            dir: SplitDir::Horizontal,
            ratio: 0.5,
            a: Box::new(SplitTree::Leaf(top)),
            b: Box::new(SplitTree::Leaf(bottom)),
        };
        let area = Rect { x: 0, y: 0, width: 20, height: 11 };
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
            grab_zone: Rect { x: 50, y: 0, width: 1, height: 10 },
            parent_area: Rect { x: 0, y: 0, width: 100, height: 10 },
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
            grab_zone: Rect { x: 0, y: 5, width: 10, height: 1 },
            parent_area: Rect { x: 0, y: 0, width: 10, height: 10 },
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
            }),
        };
        let servers = vec![("/home/dev/project".to_string(), server)];
        let menu = super::super::AttachMenu {
            servers,
            selected: 0,
            pending_delete: None,
            rename: None,
            previously_bound: None,
        };
        // Wide enough that the popup (85% of frame width, see
        // draw_attach_menu) comfortably fits every column's full width
        // rather than clipping mid-row -- a narrower backend was exactly
        // what caused this test to flake when the columns were added.
        let backend = TestBackend::new(100, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw_attach_menu(frame, &menu, &HashSet::new())).unwrap();
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
            previously_bound: None,
        };
        let backend = TestBackend::new(100, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw_attach_menu(frame, &menu, &HashSet::new())).unwrap();
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
        };
        let backend = TestBackend::new(60, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw_attach_menu(frame, &menu, &HashSet::new())).unwrap();
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
            previously_bound: None,
        };
        let backend = TestBackend::new(100, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw_attach_menu(frame, &menu, &HashSet::new())).unwrap();
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
            }),
        };
        let dead = ServerPaneInfo {
            id: Uuid::new_v4(),
            name: Some("dead-pane".to_string()),
            size: Size { rows: 24, cols: 80 },
            status: ServerPaneStatus::Dead,
            foreground: None,
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
        };
        let backend = TestBackend::new(100, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw_attach_menu(frame, &menu, &HashSet::new())).unwrap();
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
            previously_bound: None,
        };
        let mut collapsed = HashSet::new();
        collapsed.insert("/home/dev/api".to_string());
        let backend = TestBackend::new(100, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw_attach_menu(frame, &menu, &collapsed)).unwrap();
        // The collapsed group's header stays visible (so it can be
        // re-expanded), but its member row is gone.
        assert!(buffer_contains(&terminal, "/home/dev/api"));
        assert!(!buffer_contains(&terminal, "api-shell"));
        // The other, uncollapsed group is unaffected.
        assert!(buffer_contains(&terminal, "/home/dev/web"));
        assert!(buffer_contains(&terminal, "web-shell"));
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
            }),
        };
        let menu = super::super::AttachMenu {
            servers: vec![("/home/dev/api".to_string(), a)],
            selected: 0,
            pending_delete: None,
            rename: None,
            previously_bound: None,
        };

        let backend = TestBackend::new(100, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw_attach_menu(frame, &menu, &HashSet::new())).unwrap();
        assert!(buffer_contains(&terminal, "▾"), "expanded group should show the expanded marker");

        let mut collapsed = HashSet::new();
        collapsed.insert("/home/dev/api".to_string());
        let backend = TestBackend::new(100, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw_attach_menu(frame, &menu, &collapsed)).unwrap();
        assert!(buffer_contains(&terminal, "▸"), "collapsed group should show the collapsed marker");
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
            previously_bound: None,
        };
        let backend = TestBackend::new(100, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw_attach_menu(frame, &menu, &HashSet::new())).unwrap();
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
        };
        let server_b = ServerPaneInfo {
            id: other,
            name: Some("other-pane".to_string()),
            size: Size { rows: 24, cols: 80 },
            status: ServerPaneStatus::Running,
            foreground: None,
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
        };
        let backend = TestBackend::new(100, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw_attach_menu(frame, &menu, &HashSet::new())).unwrap();
        let marked_row = buffer_row_containing(&terminal, "detached");
        let unmarked_row = buffer_row_containing(&terminal, "other-pane");
        assert!(marked_row.contains('*'), "row previously bound should carry the * marker: {marked_row:?}");
        assert!(!unmarked_row.contains('*'), "an unrelated row must not carry the marker: {unmarked_row:?}");
    }

    #[test]
    fn draw_attach_menu_shows_no_marker_when_nothing_was_previously_bound() {
        let server = ServerPaneInfo {
            id: Uuid::new_v4(),
            name: Some("fresh".to_string()),
            size: Size { rows: 24, cols: 80 },
            status: ServerPaneStatus::Running,
            foreground: None,
        };
        let menu = super::super::AttachMenu {
            servers: vec![("Unknown".to_string(), server)],
            selected: 0,
            pending_delete: None,
            rename: None,
            previously_bound: None,
        };
        let backend = TestBackend::new(100, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw_attach_menu(frame, &menu, &HashSet::new())).unwrap();
        let row = buffer_row_containing(&terminal, "fresh");
        assert!(!row.contains('*'), "no row should be marked when previously_bound is None: {row:?}");
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
            previously_bound: None,
        };
        let backend = TestBackend::new(100, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw_attach_menu(frame, &menu, &HashSet::new())).unwrap();
        assert!(buffer_contains(&terminal, "new-name"));
        assert!(buffer_contains(&terminal, "name taken"));
    }

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
}
