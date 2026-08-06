//! Parses raw input bytes into [`super::Action`]s.
//!
//! Two input sources feed this: normal keystrokes (passed through to the
//! focused pane) and Kitty-forwarded Cmd-chords, which arrive as custom
//! escape sequences configured via `kitty.conf` `map ... send_text`
//! (design doc "Default keybinds (TUI)" — exact sequences documented in
//! the setup docs this module's implementer should also write).
//!
//! # Chord encoding
//!
//! There is no pre-existing "Cmd chord to escape sequence" standard to
//! match, so this module defines its own. Every dimux chord is encoded as
//! an APC-style private escape sequence:
//!
//! ```text
//! ESC _ D <tag> ESC \
//! 0x1B 0x5F 0x44 <tag byte(s)> 0x1B 0x5C
//! ```
//!
//! - `ESC _` (`0x1B 0x5F`) opens an Application Program Command (APC)
//!   string — a control sequence real terminal programs essentially never
//!   emit on their own, and Kitty never generates from normal keyboard
//!   input. Using it as a private prefix means we will never misinterpret
//!   an ordinary keystroke or a program's own output as a dimux chord.
//! - `D` identifies this APC string as a "dimux chord" (as opposed to some
//!   other private APC use some other tool might pick).
//! - `<tag>` is one or more ASCII bytes identifying which chord: a decimal
//!   digit `'1'..='9'` for workspace switches, or a single letter for the
//!   named chords below.
//! - `ESC \` (`0x1B 0x5C`, the standard "String Terminator", `ST`)
//!   terminates the APC string.
//!
//! Full table of `kitty.conf` `send_text` payloads (each written here as
//! the literal bytes to configure, using `\x1b` for `ESC`):
//!
//! | Chord         | Tag  | Bytes                        |
//! |---------------|------|-------------------------------|
//! | `cmd-1`       | `1`  | `\x1b_D1\x1b\\`               |
//! | `cmd-2`       | `2`  | `\x1b_D2\x1b\\`               |
//! | `cmd-3`       | `3`  | `\x1b_D3\x1b\\`               |
//! | `cmd-4`       | `4`  | `\x1b_D4\x1b\\`               |
//! | `cmd-5`       | `5`  | `\x1b_D5\x1b\\`               |
//! | `cmd-6`       | `6`  | `\x1b_D6\x1b\\`               |
//! | `cmd-7`       | `7`  | `\x1b_D7\x1b\\`               |
//! | `cmd-8`       | `8`  | `\x1b_D8\x1b\\`               |
//! | `cmd-9`       | `9`  | `\x1b_D9\x1b\\`               |
//! | `cmd-d`       | `d`  | `\x1b_Dd\x1b\\`               |
//! | `cmd-shift-d` | `D`  | `\x1b_DD\x1b\\`               |
//! | `cmd-w`       | `w`  | `\x1b_Dw\x1b\\`               |
//! | `cmd-shift-w` | `W`  | `\x1b_DW\x1b\\`               |
//! | `cmd-shift-z` | `Z`  | `\x1b_DZ\x1b\\`               |
//! | `cmd-h`       | `h`  | `\x1b_Dh\x1b\\`               |
//! | `cmd-j`       | `j`  | `\x1b_Dj\x1b\\`               |
//! | `cmd-k`       | `k`  | `\x1b_Dk\x1b\\`               |
//! | `cmd-l`       | `l`  | `\x1b_Dl\x1b\\`               |
//! | `shift-enter` | `s`  | `\x1b_Ds\x1b\\`               |
//! | `cmd-t`       | `t`  | `\x1b_Dt\x1b\\`               |
//! | `cmd-]`       | `]`  | `\x1b_D]\x1b\\`               |
//! | `cmd-[`       | `[`  | `\x1b_D[\x1b\\`               |
//!
//! (Named-chord tags are case-sensitive letters, deliberately mirroring
//! the shift relationship: `cmd-shift-d` uses the uppercase of `cmd-d`'s
//! tag, etc. — easy to eyeball in a `kitty.conf` diff.)
//!
//! Anything that does not match this exact prefix/tag/terminator shape
//! (including a bare `ESC`, plain text, or an unrecognized tag) resolves
//! to [`Action::PassThrough`], so the caller forwards the original bytes
//! to the focused server-pane untouched.
//!
//! `shift-enter` is the one entry in this table with no [`Chord`]/
//! [`Action`] variant of its own: plain Enter and Shift+Enter are
//! otherwise indistinguishable (both just `\r`) without a terminal-level
//! remap, but nothing in the *main* keymap currently needs to tell them
//! apart -- only the attach menu's per-group spawn field does (spawn+
//! bind+send vs. spawn+send-but-leave-unbound). So this chord is
//! recognized as a raw byte sequence directly by `super::mod`'s
//! attach-menu input handling, bypassing [`parse`]/[`Action`] entirely,
//! via the [`SHIFT_ENTER_CHORD`] constant below.

use super::Action;

/// APC opener: `ESC _ D`.
const PREFIX: &[u8] = b"\x1b_D";
/// String terminator: `ESC \`.
const TERMINATOR: &[u8] = b"\x1b\\";

/// The `shift-enter` chord's full byte sequence -- see module doc for why
/// this is a bare constant rather than a [`Chord`]/[`Action`] variant.
pub const SHIFT_ENTER_CHORD: &[u8] = b"\x1b_Ds\x1b\\";

/// Recognized chord tag, decoded from the byte(s) between [`PREFIX`] and
/// [`TERMINATOR`]. Kept separate from [`Action`] so the escape-sequence
/// grammar (single tag byte) doesn't leak into the public action type.
enum Chord {
    Workspace(u8),
    SplitVertical,
    SplitHorizontal,
    CloseFocusedPane,
    KillFocusedServerPane,
    DetachAndAttach,
    FocusLeft,
    FocusDown,
    FocusUp,
    FocusRight,
    AddTab,
    CycleTabForward,
    CycleTabBackward,
}

impl Chord {
    fn from_tag(tag: u8) -> Option<Chord> {
        match tag {
            b'1'..=b'9' => Some(Chord::Workspace(tag - b'0')),
            b'd' => Some(Chord::SplitVertical),
            b'D' => Some(Chord::SplitHorizontal),
            b'w' => Some(Chord::CloseFocusedPane),
            b'W' => Some(Chord::KillFocusedServerPane),
            b'Z' => Some(Chord::DetachAndAttach),
            b'h' => Some(Chord::FocusLeft),
            b'j' => Some(Chord::FocusDown),
            b'k' => Some(Chord::FocusUp),
            b'l' => Some(Chord::FocusRight),
            b't' => Some(Chord::AddTab),
            b']' => Some(Chord::CycleTabForward),
            b'[' => Some(Chord::CycleTabBackward),
            _ => None,
        }
    }

    fn into_action(self) -> Action {
        match self {
            Chord::Workspace(n) => Action::SwitchWorkspace(n),
            Chord::SplitVertical => Action::SplitVertical,
            Chord::SplitHorizontal => Action::SplitHorizontal,
            Chord::CloseFocusedPane => Action::CloseFocusedPane,
            Chord::KillFocusedServerPane => Action::KillFocusedServerPane,
            Chord::DetachAndAttach => Action::DetachAndAttach,
            Chord::FocusLeft => Action::FocusLeft,
            Chord::FocusDown => Action::FocusDown,
            Chord::FocusUp => Action::FocusUp,
            Chord::FocusRight => Action::FocusRight,
            Chord::AddTab => Action::AddTab,
            Chord::CycleTabForward => Action::CycleTabForward,
            Chord::CycleTabBackward => Action::CycleTabBackward,
        }
    }
}

/// Parse one input event's raw bytes into an [`Action`]. Returns
/// `Action::PassThrough` for anything not recognized as a dimux chord, so
/// the caller forwards it to the focused server-pane unchanged.
///
/// # Constraint on the caller
///
/// This function assumes `bytes` is exactly one already-delimited input
/// event — a single complete read, not an arbitrary chunk that might
/// split a multi-byte escape sequence across two calls. It does not
/// buffer or reassemble partial sequences. The event-loop implementer in
/// `mod.rs` must read input in a way that yields one complete escape
/// sequence (or one complete normal keystroke) per call — e.g. reading
/// with a short idle timeout so a fast-arriving multi-byte chord lands in
/// one read, the way Kitty emits `send_text` payloads as a single write.
pub fn parse(bytes: &[u8]) -> Action {
    let Some(rest) = bytes.strip_prefix(PREFIX) else {
        return Action::PassThrough;
    };
    let Some(tag) = rest.strip_suffix(TERMINATOR) else {
        return Action::PassThrough;
    };
    // Exactly one tag byte expected between prefix and terminator.
    let [tag_byte] = tag else {
        return Action::PassThrough;
    };
    match Chord::from_tag(*tag_byte) {
        Some(chord) => chord.into_action(),
        None => Action::PassThrough,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chord_bytes(tag: u8) -> Vec<u8> {
        let mut v = PREFIX.to_vec();
        v.push(tag);
        v.extend_from_slice(TERMINATOR);
        v
    }

    #[test]
    fn workspace_switch_digits() {
        for n in 1u8..=9 {
            let tag = b'0' + n;
            let action = parse(&chord_bytes(tag));
            assert_eq!(action, Action::SwitchWorkspace(n), "digit {n}");
        }
    }

    #[test]
    fn split_vertical() {
        assert_eq!(parse(&chord_bytes(b'd')), Action::SplitVertical);
    }

    #[test]
    fn split_horizontal() {
        assert_eq!(parse(&chord_bytes(b'D')), Action::SplitHorizontal);
    }

    #[test]
    fn close_focused_pane() {
        assert_eq!(parse(&chord_bytes(b'w')), Action::CloseFocusedPane);
    }

    #[test]
    fn kill_focused_server_pane() {
        assert_eq!(parse(&chord_bytes(b'W')), Action::KillFocusedServerPane);
    }

    #[test]
    fn detach_and_attach() {
        assert_eq!(parse(&chord_bytes(b'Z')), Action::DetachAndAttach);
    }

    #[test]
    fn focus_left() {
        assert_eq!(parse(&chord_bytes(b'h')), Action::FocusLeft);
    }

    #[test]
    fn focus_down() {
        assert_eq!(parse(&chord_bytes(b'j')), Action::FocusDown);
    }

    #[test]
    fn focus_up() {
        assert_eq!(parse(&chord_bytes(b'k')), Action::FocusUp);
    }

    #[test]
    fn focus_right() {
        assert_eq!(parse(&chord_bytes(b'l')), Action::FocusRight);
    }

    #[test]
    fn add_tab() {
        assert_eq!(parse(&chord_bytes(b't')), Action::AddTab);
    }

    #[test]
    fn cycle_tab_forward() {
        assert_eq!(parse(&chord_bytes(b']')), Action::CycleTabForward);
    }

    #[test]
    fn cycle_tab_backward() {
        assert_eq!(parse(&chord_bytes(b'[')), Action::CycleTabBackward);
    }

    #[test]
    fn empty_input_passes_through() {
        assert_eq!(parse(b""), Action::PassThrough);
    }

    #[test]
    fn plain_text_passes_through() {
        assert_eq!(parse(b"hello"), Action::PassThrough);
    }

    #[test]
    fn unrecognized_tag_passes_through() {
        assert_eq!(parse(&chord_bytes(b'z')), Action::PassThrough);
    }

    #[test]
    fn removed_picker_tag_now_passes_through() {
        // `p` was `cmd-p`/`OpenPicker` before the picker was removed;
        // confirms the tag is genuinely gone from the grammar, not just
        // unreachable from `Action`.
        assert_eq!(parse(&chord_bytes(b'p')), Action::PassThrough);
    }

    #[test]
    fn bare_escape_passes_through() {
        assert_eq!(parse(b"\x1b"), Action::PassThrough);
    }

    #[test]
    fn truncated_chord_passes_through() {
        // Missing terminator.
        assert_eq!(parse(b"\x1b_D1"), Action::PassThrough);
    }
}
