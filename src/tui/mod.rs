//! The `dimux attach` frontend: connects to the daemon, subscribes to a
//! workspace, and renders it with `ratatui`. See design doc "Default
//! keybinds (TUI)" and "Protocol / Subscription model".
//!
//! Module split:
//! - `keys` — parses raw input bytes (including Kitty-forwarded Cmd-chord
//!   escape sequences) into an [`Action`], independent of rendering.
//! - `render` — turns the current workspace snapshot + grids into a
//!   `ratatui::Frame`, independent of networking.
//! - `mod.rs` (here) — the event loop tying input, network, and render
//!   together.

pub mod keys;
pub mod render;

use crate::cli::Client;

/// User-facing actions a keypress can resolve to, decoupled from the
/// specific chord that produced it so `render`/the event loop don't need
/// to know about Kitty escape sequences.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    SwitchWorkspace(u8),
    SplitVertical,
    SplitHorizontal,
    OpenPicker,
    CloseFocusedPane,
    KillFocusedServerPane,
    FocusLeft,
    FocusRight,
    FocusUp,
    FocusDown,
    /// Not a dimux chord — forward these raw bytes to the focused
    /// client-pane's bound server-pane as keyboard input.
    PassThrough,
}

/// Drives one attached frontend: connect, subscribe to the initial
/// workspace, then loop handling terminal input events and pushed daemon
/// `Event`s until the user quits.
pub async fn run() -> anyhow::Result<()> {
    let _client = Client::connect().await?;
    todo!("ratatui terminal setup, initial Subscribe, select! loop over input + daemon events")
}
