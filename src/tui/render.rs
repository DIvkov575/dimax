//! Renders a workspace's split tree + bound server-pane grids into a
//! `ratatui::Frame`. Pure w.r.t. networking — takes already-fetched
//! state, never talks to the daemon itself.

use crate::protocol::{GridSnapshot, WorkspaceInfo};
use ratatui::Frame;
use std::collections::HashMap;

/// Draw `workspace`'s current split tree into `frame`, blitting each
/// bound client-pane's grid (looked up in `grids` by server-pane id) into
/// its computed rect. Panes with no binding, or whose server-pane isn't
/// in `grids`, render as an "unbound"/placeholder box (design doc "Error
/// handling").
pub fn draw(
    frame: &mut Frame,
    workspace: &WorkspaceInfo,
    grids: &HashMap<crate::protocol::ServerPaneId, GridSnapshot>,
    focused: Option<crate::protocol::ClientPaneId>,
) {
    let _ = (frame, workspace, grids, focused);
    todo!("ratatui::layout::Layout split matching SplitTree structure, then blit each Cell grid")
}

/// Overlay for `cmd-p`/`cmd-d` pane-picker: lists all server-panes plus a
/// "spawn new" entry.
pub fn draw_picker(frame: &mut Frame, servers: &[crate::protocol::ServerPaneInfo], selected: usize) {
    let _ = (frame, servers, selected);
    todo!()
}
