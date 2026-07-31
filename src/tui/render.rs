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

use crate::protocol::{Cell, ClientPane, GridSnapshot, ServerPaneId, SplitDir, SplitTree, WorkspaceInfo};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Paragraph};
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
            let percent_a = (ratio.clamp(0.0, 1.0) * 100.0).round() as u16;
            let percent_b = 100u16.saturating_sub(percent_a);
            let rects = Layout::new(
                ratatui_direction(*dir),
                [Constraint::Percentage(percent_a), Constraint::Percentage(percent_b)],
            )
            .split(area);
            draw_tree(frame, a, rects[0], grids, focused);
            draw_tree(frame, b, rects[1], grids, focused);
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
    let block = Block::default()
        .borders(Borders::ALL)
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
}
