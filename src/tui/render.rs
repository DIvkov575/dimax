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
    SplitTree, WorkspaceInfo,
};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::Frame;
use std::collections::HashMap;

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
        SplitTree::Split { dir, ratio, a, b } => {
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

/// Overlay for `cmd-shift-z`'s attach menu: lists every server-pane plus a
/// trailing "spawn new" entry, opened after the focused client-pane has
/// already been detached from whatever it was previously bound to.
pub fn draw_attach_menu(frame: &mut Frame, servers: &[ServerPaneInfo], selected: usize) {
    let area = centered_rect(60, 60, frame.area());
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

fn attach_menu_line(server: &ServerPaneInfo, selected: bool) -> Line<'static> {
    let label = server.name.clone().unwrap_or_else(|| short_id(server.id));
    let status = match server.status {
        ServerPaneStatus::Running => "Running",
        ServerPaneStatus::Dead => "Dead",
    };
    let text = format!(
        "{} {} [{}] {}x{}",
        if selected { ">" } else { " " },
        label,
        status,
        server.size.cols,
        server.size.rows,
    );
    let style = if selected { Style::new().add_modifier(Modifier::REVERSED) } else { Style::new() };
    Line::styled(text, style)
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
    use crate::protocol::Size;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    use uuid::Uuid;

    fn buffer_contains(terminal: &Terminal<TestBackend>, needle: &str) -> bool {
        let buffer = terminal.backend().buffer();
        let content: String = buffer.content().iter().map(|c| c.symbol()).collect();
        content.contains(needle)
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
    fn draw_split_produces_no_panic_and_both_leaves_render() {
        let left_id = Uuid::new_v4();
        let right_id = Uuid::new_v4();
        let left = ClientPane { id: Uuid::new_v4(), name: Some("left".into()), bound: Some(left_id) };
        let right = ClientPane { id: Uuid::new_v4(), name: Some("right".into()), bound: Some(right_id) };
        let tree = SplitTree::Split {
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
    fn draw_attach_menu_shows_selected_item() {
        let server = ServerPaneInfo {
            id: Uuid::new_v4(),
            name: Some("editor".to_string()),
            size: Size { rows: 24, cols: 80 },
            status: ServerPaneStatus::Running,
        };
        let servers = vec![server];
        let backend = TestBackend::new(60, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw_attach_menu(frame, &servers, 0)).unwrap();
        assert!(buffer_contains(&terminal, "editor"));
        assert!(buffer_contains(&terminal, "spawn new"));
        assert!(buffer_contains(&terminal, "Attach server-pane"));
    }

    #[test]
    fn draw_attach_menu_spawn_new_selected_does_not_panic() {
        let servers: Vec<ServerPaneInfo> = vec![];
        let backend = TestBackend::new(60, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw_attach_menu(frame, &servers, 0)).unwrap();
        assert!(buffer_contains(&terminal, "spawn new"));
    }
}
