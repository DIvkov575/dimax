use crate::protocol::{ClientPaneId, GridSnapshot, ServerPaneId};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use ratatui::layout::Rect;

/// A terminal-grid coordinate inside a pane's visible snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct GridPosition {
    pub row: usize,
    pub col: usize,
}

/// One pane-local mouse selection. Selection never crosses pane
/// boundaries: drags outside the originating pane clamp to its nearest
/// visible cell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextSelection {
    pane: ClientPaneId,
    server_pane: ServerPaneId,
    start: GridPosition,
    end: GridPosition,
    dragging: bool,
}

impl TextSelection {
    pub fn new(pane: ClientPaneId, server_pane: ServerPaneId, start: GridPosition) -> Self {
        Self {
            pane,
            server_pane,
            start,
            end: start,
            dragging: true,
        }
    }

    pub fn pane(&self) -> ClientPaneId {
        self.pane
    }

    pub fn server_pane(&self) -> ServerPaneId {
        self.server_pane
    }

    pub fn is_dragging(&self) -> bool {
        self.dragging
    }

    pub fn update(&mut self, end: GridPosition) {
        self.end = end;
    }

    /// Finish the drag and report whether it spans more than one cell.
    /// A plain click should focus a pane, not copy one character.
    pub fn finish(&mut self, end: GridPosition) -> bool {
        self.end = end;
        self.dragging = false;
        self.start != self.end
    }

    pub fn contains(&self, row: usize, col: usize) -> bool {
        let point = GridPosition { row, col };
        let (start, end) = self.ordered();
        point >= start && point <= end
    }

    fn ordered(&self) -> (GridPosition, GridPosition) {
        if self.start <= self.end {
            (self.start, self.end)
        } else {
            (self.end, self.start)
        }
    }
}

/// Convert a screen coordinate to a snapshot cell. The leaf's first row
/// is its title border, so selectable content begins one row below it.
pub fn position_at(
    leaf: Rect,
    snapshot: &GridSnapshot,
    col: u16,
    row: u16,
) -> Option<GridPosition> {
    let content = content_rect(leaf)?;
    if !contains(content, col, row) {
        return None;
    }
    let grid_row = usize::from(row - content.y);
    let grid_col = usize::from(col - content.x);
    let snapshot_row = snapshot.lines.get(grid_row)?;
    (grid_col < snapshot_row.len()).then_some(GridPosition {
        row: grid_row,
        col: grid_col,
    })
}

/// Map a drag coordinate to the closest selectable cell in `leaf`.
pub fn clamped_position(
    leaf: Rect,
    snapshot: &GridSnapshot,
    col: u16,
    row: u16,
) -> Option<GridPosition> {
    let content = content_rect(leaf)?;
    let visible_rows = usize::from(content.height).min(snapshot.lines.len());
    if visible_rows == 0 {
        return None;
    }

    let grid_row = usize::from(row.saturating_sub(content.y)).min(visible_rows.saturating_sub(1));
    let visible_cols = usize::from(content.width).min(snapshot.lines[grid_row].len());
    if visible_cols == 0 {
        return None;
    }
    let grid_col = usize::from(col.saturating_sub(content.x)).min(visible_cols.saturating_sub(1));
    Some(GridPosition {
        row: grid_row,
        col: grid_col,
    })
}

/// Extract the selected visible cells as plain text. Right-side terminal
/// padding is removed per row; explicit row boundaries become newlines.
pub fn selected_text(snapshot: &GridSnapshot, selection: &TextSelection) -> Option<String> {
    if snapshot.server_pane != selection.server_pane {
        return None;
    }
    let start = clamp_grid_position(snapshot, selection.ordered().0)?;
    let end = clamp_grid_position(snapshot, selection.ordered().1)?;
    let (start, end) = if start <= end {
        (start, end)
    } else {
        (end, start)
    };

    let mut selected_lines = Vec::with_capacity(end.row - start.row + 1);
    for row_index in start.row..=end.row {
        let row = snapshot.lines.get(row_index)?;
        if row.is_empty() {
            selected_lines.push(String::new());
            continue;
        }
        let first_col = if row_index == start.row { start.col } else { 0 };
        let last_col = if row_index == end.row {
            end.col.min(row.len() - 1)
        } else {
            row.len() - 1
        };
        let line = row[first_col.min(row.len() - 1)..=last_col]
            .iter()
            .map(|cell| cell.text.as_str())
            .collect::<String>()
            .trim_end()
            .to_string();
        selected_lines.push(line);
    }
    Some(selected_lines.join("\n"))
}

/// OSC 52 asks the containing terminal to place the payload on its
/// clipboard. It avoids platform-specific clipboard binaries and works
/// over remote shells when the terminal permits clipboard writes.
pub fn osc52_sequence(text: &str) -> String {
    format!("\x1b]52;c;{}\x1b\\", STANDARD.encode(text.as_bytes()))
}

fn content_rect(leaf: Rect) -> Option<Rect> {
    (leaf.width > 0 && leaf.height > 1).then_some(Rect {
        x: leaf.x,
        y: leaf.y + 1,
        width: leaf.width,
        height: leaf.height - 1,
    })
}

fn contains(rect: Rect, col: u16, row: u16) -> bool {
    col >= rect.x
        && col < rect.x.saturating_add(rect.width)
        && row >= rect.y
        && row < rect.y.saturating_add(rect.height)
}

fn clamp_grid_position(snapshot: &GridSnapshot, position: GridPosition) -> Option<GridPosition> {
    let row = position.row.min(snapshot.lines.len().checked_sub(1)?);
    let col = position.col.min(snapshot.lines[row].len().checked_sub(1)?);
    Some(GridPosition { row, col })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{Cell, Size};
    use uuid::Uuid;

    fn cell(text: &str) -> Cell {
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

    fn snapshot(lines: &[&str]) -> GridSnapshot {
        let server_pane = Uuid::new_v4();
        let width = lines
            .iter()
            .map(|line| line.chars().count())
            .max()
            .unwrap_or(0);
        GridSnapshot {
            server_pane,
            size: Size {
                rows: lines.len() as u16,
                cols: width as u16,
            },
            cursor: (0, 0),
            lines: lines
                .iter()
                .map(|line| line.chars().map(|ch| cell(&ch.to_string())).collect())
                .collect(),
            scroll_offset: 0,
        }
    }

    fn selection(snapshot: &GridSnapshot, start: GridPosition, end: GridPosition) -> TextSelection {
        let mut selection = TextSelection::new(Uuid::new_v4(), snapshot.server_pane, start);
        selection.finish(end);
        selection
    }

    #[test]
    fn screen_position_skips_the_title_row() {
        let snapshot = snapshot(&["abcd", "efgh"]);
        let leaf = Rect {
            x: 10,
            y: 5,
            width: 4,
            height: 3,
        };
        assert_eq!(position_at(leaf, &snapshot, 10, 5), None);
        assert_eq!(
            position_at(leaf, &snapshot, 12, 6),
            Some(GridPosition { row: 0, col: 2 })
        );
        assert_eq!(
            position_at(leaf, &snapshot, 13, 7),
            Some(GridPosition { row: 1, col: 3 })
        );
    }

    #[test]
    fn drag_position_clamps_to_the_snapshot_edges() {
        let snapshot = snapshot(&["abcd", "efgh"]);
        let leaf = Rect {
            x: 10,
            y: 5,
            width: 4,
            height: 3,
        };
        assert_eq!(
            clamped_position(leaf, &snapshot, 0, 0),
            Some(GridPosition { row: 0, col: 0 })
        );
        assert_eq!(
            clamped_position(leaf, &snapshot, u16::MAX, u16::MAX),
            Some(GridPosition { row: 1, col: 3 })
        );
    }

    #[test]
    fn extracts_forward_and_reverse_single_line_selections() {
        let snapshot = snapshot(&["abcdef"]);
        let forward = selection(
            &snapshot,
            GridPosition { row: 0, col: 1 },
            GridPosition { row: 0, col: 4 },
        );
        let reverse = selection(
            &snapshot,
            GridPosition { row: 0, col: 4 },
            GridPosition { row: 0, col: 1 },
        );
        assert_eq!(selected_text(&snapshot, &forward).as_deref(), Some("bcde"));
        assert_eq!(selected_text(&snapshot, &reverse).as_deref(), Some("bcde"));
    }

    #[test]
    fn extracts_multiline_selection_without_terminal_padding() {
        let snapshot = snapshot(&["abc ", "def ", "ghi "]);
        let selection = selection(
            &snapshot,
            GridPosition { row: 0, col: 1 },
            GridPosition { row: 2, col: 2 },
        );
        assert_eq!(
            selected_text(&snapshot, &selection).as_deref(),
            Some("bc\ndef\nghi")
        );
    }

    #[test]
    fn plain_click_does_not_count_as_a_drag() {
        let point = GridPosition { row: 1, col: 2 };
        let mut selection = TextSelection::new(Uuid::new_v4(), Uuid::new_v4(), point);
        assert!(!selection.finish(point));
        assert!(!selection.is_dragging());
    }

    #[test]
    fn osc52_encodes_utf8_for_the_clipboard_selector() {
        assert_eq!(osc52_sequence("hello"), "\x1b]52;c;aGVsbG8=\x1b\\");
    }
}
