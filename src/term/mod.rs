//! PTY + terminal-emulation glue: owns a spawned process's pseudo-terminal
//! and feeds its output into a `wezterm_term::Terminal` model, exposing a
//! plain [`GridSnapshot`] the daemon can hand to frontends.
//!
//! Contract for callers (the daemon):
//! - [`ServerPane::spawn`] starts the process and a background OS thread
//!   that reads PTY output as it arrives. That thread updates the pane's
//!   internal grid and, on every change, sends a [`ServerPaneEvent`] on
//!   the `events` channel passed in — the daemon does not poll.
//! - All other `ServerPane` methods are synchronous and safe to call from
//!   an async context (they only briefly lock an internal mutex, never
//!   block on I/O) except `write_input`, which does a non-blocking PTY
//!   write.

use crate::protocol::{Cell, GridSnapshot, ServerPaneId, ServerPaneStatus, Size};
use tokio::sync::mpsc::UnboundedSender;

/// Pushed by the background reader thread. `events` is an
/// `UnboundedSender` because tokio's unbounded sender can be used from a
/// plain (non-async) OS thread without a runtime handle.
#[derive(Debug, Clone, Copy)]
pub enum ServerPaneEvent {
    Changed(ServerPaneId),
    Died(ServerPaneId),
}

/// A single PTY-backed process plus its terminal model.
pub struct ServerPane {
    id: ServerPaneId,
    name: Option<String>,
}

impl ServerPane {
    /// Spawn `cmd` (or `$SHELL` if `None`) attached to a new PTY sized to
    /// `size`. `id` is chosen by the caller (the daemon) so it can be
    /// referenced before the pane finishes constructing. `events` receives
    /// a `Changed`/`Died` notification from the background reader thread
    /// whenever this pane's displayed content changes or its process
    /// exits.
    pub fn spawn(
        id: ServerPaneId,
        name: Option<String>,
        cmd: Option<String>,
        size: Size,
        events: UnboundedSender<ServerPaneEvent>,
    ) -> anyhow::Result<Self> {
        let _ = (id, &name, cmd, size, events);
        todo!("spawn PTY + command via portable-pty, start wezterm-term Terminal, spawn reader thread")
    }

    pub fn id(&self) -> ServerPaneId {
        self.id
    }

    pub fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }

    pub fn set_name(&mut self, name: Option<String>) {
        self.name = name;
    }

    /// `Running` while the child process is alive, `Dead` after it exits
    /// (the grid keeps its last contents either way).
    pub fn status(&self) -> ServerPaneStatus {
        todo!()
    }

    pub fn size(&self) -> Size {
        todo!()
    }

    /// Full current grid contents, suitable for sending to a newly
    /// subscribed frontend or after any change.
    pub fn snapshot(&self) -> GridSnapshot {
        todo!()
    }

    /// Write raw bytes (keystrokes) to the child process's stdin via the
    /// PTY master.
    pub fn write_input(&self, bytes: &[u8]) -> anyhow::Result<()> {
        let _ = bytes;
        todo!()
    }

    /// Resize both the OS-level PTY and the `wezterm_term::Terminal`
    /// model. Called by the daemon after recomputing the
    /// smallest-viewer-wins size for this pane.
    pub fn resize(&self, size: Size) -> anyhow::Result<()> {
        let _ = size;
        todo!()
    }

    /// Terminate the child process. `status()` will report `Dead`
    /// afterward; `snapshot()` continues to return the last grid.
    pub fn kill(&mut self) -> anyhow::Result<()> {
        todo!()
    }
}

/// Convert a `wezterm_term` screen line into the wire-level `Cell` row
/// used by [`GridSnapshot`]. Kept as a free function so it's testable in
/// isolation against constructed `wezterm_term` state without needing a
/// live PTY.
#[allow(dead_code)]
fn cell_placeholder() -> Cell {
    Cell {
        text: " ".to_string(),
        fg: None,
        bg: None,
        bold: false,
        italic: false,
        underline: false,
        reverse: false,
    }
}
