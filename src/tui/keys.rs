//! Parses raw input bytes into [`super::Action`]s.
//!
//! Two input sources feed this: normal keystrokes (passed through to the
//! focused pane) and Kitty-forwarded Cmd-chords, which arrive as custom
//! escape sequences configured via `kitty.conf` `map ... send_text`
//! (design doc "Default keybinds (TUI)" — exact sequences documented in
//! the setup docs this module's implementer should also write).

use super::Action;

/// Parse one input event's raw bytes into an [`Action`]. Returns
/// `Action::PassThrough` for anything not recognized as a dimux chord, so
/// the caller forwards it to the focused server-pane unchanged.
pub fn parse(bytes: &[u8]) -> Action {
    let _ = bytes;
    todo!("recognize the Kitty-forwarded escape sequences for cmd-1..9/d/shift-d/p/w/shift-w/hjkl")
}
