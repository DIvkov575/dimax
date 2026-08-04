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
//! button, and scroll events, are recognized but discarded (see
//! [`ParsedInput::Mouse`] below) — never `PassThrough`, since mouse bytes
//! are never meaningful keyboard input to forward to a focused pane.
//!
//! # Defense in depth: any SGR mouse sequence is swallowed, not just the
//! ones dimux acts on
//!
//! dimux only requests button-event mouse tracking from the terminal
//! (`?1000h` + `?1002h` + `?1006h` in `tui/mod.rs`'s
//! `enable_button_event_mouse_tracking`), deliberately *not* any-event
//! tracking (`?1003h`, which reports every mouse movement with no button
//! held). That's the root-cause fix for a bug where bare mouse movement
//! generated an SGR sequence (`Cb=35`, "button 3" in this encoding, no
//! button dimux recognizes) that `parse` used to reject outright — and a
//! rejected mouse byte used to fall through to `keys::parse`, which also
//! didn't recognize it, resolved to `Action::PassThrough`, and wrote the
//! raw escape sequence into the focused pane as literal keystrokes. That
//! was the garbage-character symptom; high mouse-movement volume flooding
//! `Request::Input` round-trips was the "random hangup" symptom.
//!
//! Fixing what the terminal is asked to report closes the bug for a
//! correctly-behaving terminal. This module additionally recognizes *any*
//! well-formed SGR mouse sequence (any button, any event kind) as
//! "definitely mouse input, not keyboard input" and swallows it via
//! [`ParsedInput::Mouse`] — so even a stray sequence from a terminal that
//! doesn't fully honor `?1003h` being left off, a race during mode
//! switching, or some other future source, still can never leak into a
//! pane as text.

/// A parsed left-button mouse event: press, drag (move while held), or
/// release, at a given terminal cell position (0-indexed, matching
/// `ratatui::layout::Rect` coordinates).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseEvent {
    Down { col: u16, row: u16 },
    Drag { col: u16, row: u16 },
    Up { col: u16, row: u16 },
    /// One wheel-tick scrolling back into a pane's history.
    ScrollUp { col: u16, row: u16 },
    /// One wheel-tick scrolling toward a pane's live tail.
    ScrollDown { col: u16, row: u16 },
}

/// Result of parsing one input chunk against the SGR mouse format. See
/// module doc "Defense in depth" for why `Ignored` exists as a distinct
/// case from `NotMouse` — both categories.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParsedInput {
    /// A left-button press/drag/release dimux acts on.
    Mouse(MouseEvent),
    /// A well-formed SGR mouse sequence for something dimux has no use
    /// for (right/middle button, scroll, bare movement) — recognized as
    /// mouse input and discarded, never passed through as keystrokes.
    Ignored,
    /// Not an SGR mouse sequence at all; the caller should try parsing it
    /// as something else (a dimux chord, or plain keyboard input).
    NotMouse,
}

/// Parse one input chunk against the SGR mouse escape-sequence format
/// (`ESC [ < Cb ; Cx ; Cy M` for press/drag, `...m` for release).
pub fn parse(bytes: &[u8]) -> ParsedInput {
    let Some(rest) = bytes.strip_prefix(b"\x1b[<") else { return ParsedInput::NotMouse };
    let (rest, released) = match rest.strip_suffix(b"M") {
        Some(r) => (r, false),
        None => match rest.strip_suffix(b"m") {
            Some(r) => (r, true),
            None => return ParsedInput::NotMouse,
        },
    };
    let Ok(s) = std::str::from_utf8(rest) else { return ParsedInput::NotMouse };
    let mut parts = s.split(';');
    let Some(Ok(cb)) = parts.next().map(str::parse::<u32>) else { return ParsedInput::NotMouse };
    // 1-indexed on the wire; ratatui rects are 0-indexed (see the
    // crossterm reference parser this mirrors).
    let Some(Ok(col)) = parts.next().map(str::parse::<u16>) else { return ParsedInput::NotMouse };
    let Some(col) = col.checked_sub(1) else { return ParsedInput::NotMouse };
    let Some(Ok(row)) = parts.next().map(str::parse::<u16>) else { return ParsedInput::NotMouse };
    let Some(row) = row.checked_sub(1) else { return ParsedInput::NotMouse };
    // A trailing `;` before the final M/m is optional and carries no
    // extra field (see module doc reference's `\x1B[<0;20;10;M` test
    // vector) -- `split` turns it into one trailing empty string, which
    // is fine; anything else after row is a genuinely malformed sequence
    // (not a mouse event dimux recognizes, so `NotMouse` rather than
    // `Ignored` -- this matches the pre-existing test expectation that a
    // truncated/malformed sequence still falls through, e.g. to be typed
    // literally if that's ever genuinely desired, rather than being
    // silently eaten as if it were valid mouse protocol).
    if parts.next().is_some_and(|extra| !extra.is_empty()) {
        return ParsedInput::NotMouse;
    }

    // Bit layout (low to high): button lo, button hi, shift, alt,
    // control, dragging, button lo, button hi -- see module doc
    // reference. Only button number 0 (left) with dragging=0 is a
    // press/release; dragging=1 with button 0 is a drag. Any other
    // button number (middle, right, or the higher values used for
    // scroll/bare-movement encoding) is a recognized SGR mouse sequence
    // dimux simply doesn't act on.
    let button_number = (cb & 0b0000_0011) | ((cb & 0b1100_0000) >> 4);
    let dragging = cb & 0b0010_0000 != 0;

    if button_number == 4 {
        return ParsedInput::Mouse(MouseEvent::ScrollUp { col, row });
    }
    if button_number == 5 {
        return ParsedInput::Mouse(MouseEvent::ScrollDown { col, row });
    }
    if button_number != 0 {
        return ParsedInput::Ignored;
    }

    ParsedInput::Mouse(if released {
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
        assert_eq!(
            parse(b"\x1b[<0;20;10;M"),
            ParsedInput::Mouse(MouseEvent::Down { col: 19, row: 9 })
        );
    }

    #[test]
    fn left_button_down_without_trailing_semicolon() {
        assert_eq!(
            parse(b"\x1b[<0;20;10M"),
            ParsedInput::Mouse(MouseEvent::Down { col: 19, row: 9 })
        );
    }

    #[test]
    fn left_button_up() {
        assert_eq!(parse(b"\x1b[<0;20;10m"), ParsedInput::Mouse(MouseEvent::Up { col: 19, row: 9 }));
        assert_eq!(
            parse(b"\x1b[<0;20;10;m"),
            ParsedInput::Mouse(MouseEvent::Up { col: 19, row: 9 })
        );
    }

    #[test]
    fn left_button_drag() {
        // dragging bit (0b0010_0000 = 32) set alongside button 0.
        assert_eq!(parse(b"\x1b[<32;5;5M"), ParsedInput::Mouse(MouseEvent::Drag { col: 4, row: 4 }));
    }

    #[test]
    fn right_and_middle_buttons_are_recognized_and_ignored() {
        assert_eq!(parse(b"\x1b[<1;5;5M"), ParsedInput::Ignored); // middle
        assert_eq!(parse(b"\x1b[<2;5;5M"), ParsedInput::Ignored); // right
    }

    #[test]
    fn scroll_up_and_down_are_recognized_as_mouse_events() {
        assert_eq!(
            parse(b"\x1b[<64;5;5M"),
            ParsedInput::Mouse(MouseEvent::ScrollUp { col: 4, row: 4 })
        );
        assert_eq!(
            parse(b"\x1b[<65;5;5M"),
            ParsedInput::Mouse(MouseEvent::ScrollDown { col: 4, row: 4 })
        );
    }

    #[test]
    fn bare_movement_with_no_button_is_recognized_and_ignored() {
        // The exact sequence class that used to leak into the pane as
        // garbage text before the ?1003h any-event-tracking root-cause
        // fix: bare mouse movement encodes as button_number=3 (Cb=35 with
        // the dragging bit set), which dimux never acts on but must
        // still recognize as mouse input, not keyboard input.
        assert_eq!(parse(b"\x1b[<35;10;5M"), ParsedInput::Ignored);
    }

    #[test]
    fn non_mouse_bytes_fall_through() {
        assert_eq!(parse(b""), ParsedInput::NotMouse);
        assert_eq!(parse(b"hello"), ParsedInput::NotMouse);
        assert_eq!(parse(b"\x1b_Dd\x1b\\"), ParsedInput::NotMouse); // a dimux chord, not a mouse event
    }

    #[test]
    fn malformed_sequences_fall_through() {
        assert_eq!(parse(b"\x1b[<0;20M"), ParsedInput::NotMouse); // missing row
        assert_eq!(parse(b"\x1b[<abc;20;10M"), ParsedInput::NotMouse); // non-numeric
    }
}
