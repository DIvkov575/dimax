//! Minimal SGR mouse-protocol parser.
//!
//! `mod.rs` reads raw stdin bytes directly rather than using crossterm's
//! event abstraction (see that module's doc comment "Raw stdin reads,
//! not crossterm's `KeyEvent`s" for why — the short version: `keys::parse`
//! needs the exact escape-sequence bytes Kitty sends, and crossterm's
//! parser would consume and discard them). Mouse input arrives on the
//! same stdin stream as an escape sequence too, so the same constraint
//! applies: this module hand-parses just enough of the SGR mouse protocol
//! (`ESC [ < Cb ; Cx ; Cy M` for a press, `...m` for a release) to drive
//! divider dragging, rather than pulling in crossterm's mouse events.
//!
//! Reference: <http://www.xfree86.org/current/ctlseqs.html#Mouse%20Tracking>.
//! Only the left mouse button matters for dragging a divider; every other
//! button, and scroll events, resolve to `None` (ignored, not
//! `PassThrough` — mouse bytes are never meaningful keyboard input to
//! forward to a focused pane).

/// A parsed left-button mouse event: press, drag (move while held), or
/// release, at a given terminal cell position (0-indexed, matching
/// `ratatui::layout::Rect` coordinates).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseEvent {
    Down { col: u16, row: u16 },
    Drag { col: u16, row: u16 },
    Up { col: u16, row: u16 },
}

/// Parse one input chunk as an SGR mouse escape sequence. Returns `None`
/// for anything that isn't a left-button press/drag/release in this
/// format — including well-formed SGR sequences for other buttons/scroll,
/// which this module has no use for.
pub fn parse(bytes: &[u8]) -> Option<MouseEvent> {
    let rest = bytes.strip_prefix(b"\x1b[<")?;
    let (rest, released) = match rest.strip_suffix(b"M") {
        Some(r) => (r, false),
        None => (rest.strip_suffix(b"m")?, true),
    };
    let s = std::str::from_utf8(rest).ok()?;
    let mut parts = s.split(';');
    let cb: u32 = parts.next()?.parse().ok()?;
    // 1-indexed on the wire; ratatui rects are 0-indexed (see the
    // crossterm reference parser this mirrors).
    let col: u16 = parts.next()?.parse::<u16>().ok()?.checked_sub(1)?;
    let row: u16 = parts.next()?.parse::<u16>().ok()?.checked_sub(1)?;
    // A trailing `;` before the final M/m is optional and carries no
    // extra field (see module doc reference's `\x1B[<0;20;10;M` test
    // vector) -- `split` turns it into one trailing empty string, which
    // is fine; anything else after row is a genuinely malformed sequence.
    if parts.next().is_some_and(|extra| !extra.is_empty()) {
        return None;
    }

    // Bit layout (low to high): button lo, button hi, shift, alt,
    // control, dragging, button lo, button hi -- see module doc
    // reference. Only button number 0 (left) with dragging=0 is a
    // press/release; dragging=1 with button 0 is a drag.
    let button_number = (cb & 0b0000_0011) | ((cb & 0b1100_0000) >> 4);
    let dragging = cb & 0b0010_0000 != 0;
    if button_number != 0 {
        return None; // not the left button
    }

    Some(if released {
        MouseEvent::Up { col, row }
    } else if dragging {
        MouseEvent::Drag { col, row }
    } else {
        MouseEvent::Down { col, row }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn left_button_down_with_trailing_semicolon() {
        assert_eq!(parse(b"\x1b[<0;20;10;M"), Some(MouseEvent::Down { col: 19, row: 9 }));
    }

    #[test]
    fn left_button_down_without_trailing_semicolon() {
        assert_eq!(parse(b"\x1b[<0;20;10M"), Some(MouseEvent::Down { col: 19, row: 9 }));
    }

    #[test]
    fn left_button_up() {
        assert_eq!(parse(b"\x1b[<0;20;10m"), Some(MouseEvent::Up { col: 19, row: 9 }));
        assert_eq!(parse(b"\x1b[<0;20;10;m"), Some(MouseEvent::Up { col: 19, row: 9 }));
    }

    #[test]
    fn left_button_drag() {
        // dragging bit (0b0010_0000 = 32) set alongside button 0.
        assert_eq!(parse(b"\x1b[<32;5;5M"), Some(MouseEvent::Drag { col: 4, row: 4 }));
    }

    #[test]
    fn right_and_middle_buttons_are_ignored() {
        assert_eq!(parse(b"\x1b[<1;5;5M"), None); // middle
        assert_eq!(parse(b"\x1b[<2;5;5M"), None); // right
    }

    #[test]
    fn scroll_events_are_ignored() {
        assert_eq!(parse(b"\x1b[<64;5;5M"), None); // scroll up
        assert_eq!(parse(b"\x1b[<65;5;5M"), None); // scroll down
    }

    #[test]
    fn non_mouse_bytes_are_ignored() {
        assert_eq!(parse(b""), None);
        assert_eq!(parse(b"hello"), None);
        assert_eq!(parse(b"\x1b_Dd\x1b\\"), None); // a dimux chord, not a mouse event
    }

    #[test]
    fn malformed_sequences_are_ignored() {
        assert_eq!(parse(b"\x1b[<0;20M"), None); // missing row
        assert_eq!(parse(b"\x1b[<abc;20;10M"), None); // non-numeric
    }
}
