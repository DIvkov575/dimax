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
//!
//! # Bundled sequences: one `read()` can contain more than one event
//!
//! `run`'s event loop does one `stdin.read()` per iteration with no
//! reassembly buffer. At real scroll-wheel/trackpad speeds, multiple SGR
//! sequences routinely land in the *same* `read()` before the loop gets a
//! chance to process any of them — three quick wheel ticks can arrive as
//! one buffer containing three back-to-back `ESC [ < ... M` sequences.
//! [`parse`] alone can't handle this: it requires its *entire* input to
//! be exactly one sequence, so a bundled buffer fails to match at all and
//! falls through to keyboard parsing, typing the raw escape bytes into
//! the focused pane as literal garbage — this was a real reported bug.
//! [`parse_all`] is the fix: it peels sequences off the front one at a
//! time via `parse_one`, so a bundle of N ticks is handled as N mouse
//! events instead of one big non-match. `run`'s event loop uses
//! `parse_all`, not `parse`, for exactly this reason; `parse` remains for
//! the (still real, still tested) single-sequence case and as the
//! simplest entry point for anything that only ever sees one event per
//! buffer.

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

/// Parse the SGR mouse sequence at the *start* of `bytes`, if its shape
/// is recognized at all (a `ESC [ <` prefix followed somewhere by an
/// `M`/`m` terminator) -- returning both the parsed result and exactly
/// how many leading bytes of `bytes` that one sequence occupies. `None`
/// means no recognizable sequence starts here at all (no prefix, or a
/// prefix with no terminator anywhere in `bytes`); the caller can't
/// safely skip anything in that case. A recognized shape whose content
/// fails to parse (missing/non-numeric fields) still returns
/// `Some((NotMouse, consumed))` -- distinct from the "not even
/// SGR-shaped" `None` case, and needed so [`parse_all`] and [`parse`]
/// agree on exactly how many bytes one such malformed sequence spans.
fn parse_one(bytes: &[u8]) -> Option<(ParsedInput, usize)> {
    let rest = bytes.strip_prefix(b"\x1b[<")?;
    let term_idx = rest.iter().position(|&b| b == b'M' || b == b'm')?;
    let released = rest[term_idx] == b'm';
    let body = &rest[..term_idx];
    // 3 for the `ESC [ <` prefix, 1 for the M/m terminator itself.
    let consumed = 3 + term_idx + 1;

    let Ok(s) = std::str::from_utf8(body) else { return Some((ParsedInput::NotMouse, consumed)) };
    let mut parts = s.split(';');
    let (Some(Ok(cb)), Some(Ok(col_raw)), Some(Ok(row_raw))) = (
        parts.next().map(str::parse::<u32>),
        // 1-indexed on the wire; ratatui rects are 0-indexed (see the
        // crossterm reference parser this mirrors).
        parts.next().map(str::parse::<u16>),
        parts.next().map(str::parse::<u16>),
    ) else {
        return Some((ParsedInput::NotMouse, consumed));
    };
    let Some(col) = col_raw.checked_sub(1) else { return Some((ParsedInput::NotMouse, consumed)) };
    let Some(row) = row_raw.checked_sub(1) else { return Some((ParsedInput::NotMouse, consumed)) };
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
        return Some((ParsedInput::NotMouse, consumed));
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

    let parsed = if button_number == 4 {
        ParsedInput::Mouse(MouseEvent::ScrollUp { col, row })
    } else if button_number == 5 {
        ParsedInput::Mouse(MouseEvent::ScrollDown { col, row })
    } else if button_number != 0 {
        ParsedInput::Ignored
    } else if released {
        ParsedInput::Mouse(MouseEvent::Up { col, row })
    } else if dragging {
        ParsedInput::Mouse(MouseEvent::Drag { col, row })
    } else {
        ParsedInput::Mouse(MouseEvent::Down { col, row })
    };
    Some((parsed, consumed))
}

/// Parse one input chunk against the SGR mouse escape-sequence format
/// (`ESC [ < Cb ; Cx ; Cy M` for press/drag, `...m` for release).
/// Requires the *entire* `bytes` to be exactly one sequence -- see
/// [`parse_all`] for the bundled-multiple-sequences case, which is what
/// `run`'s event loop actually needs to use.
pub fn parse(bytes: &[u8]) -> ParsedInput {
    match parse_one(bytes) {
        Some((parsed, consumed)) if consumed == bytes.len() => parsed,
        _ => ParsedInput::NotMouse,
    }
}

/// Parse every complete, well-formed SGR mouse sequence at the front of
/// `bytes`, returning one [`MouseEvent`] per *actionable* sequence found
/// (a recognized-but-unhandled button, e.g. middle/right-click, is
/// consumed and silently dropped here, same as [`ParsedInput::Ignored`]
/// always was -- it never reaches the returned `Vec`), plus whatever raw
/// bytes are left over once no more sequences remain at the front. That
/// leftover tail should be handled exactly like plain [`parse`]'s
/// `NotMouse` case always was -- passed on to the chord/keyboard parser
/// -- since it's either genuinely not mouse input, or a malformed
/// mouse-shaped sequence this module doesn't try to partially recover
/// from (matches `parse`'s existing behavior for a lone malformed
/// sequence: the whole thing falls through unconsumed, not just the bad
/// part).
///
/// See module doc "Bundled sequences" for why this function exists at
/// all: at real scroll-wheel/trackpad speeds, more than one SGR
/// sequence routinely lands in a single `stdin.read()` before the event
/// loop gets a chance to process any of them. `parse` alone can't
/// handle that -- it requires the whole buffer to be exactly one
/// sequence, so a bundle of two or three ticks fails to match at all
/// and used to fall through to keyboard parsing, typing the raw escape
/// bytes into the focused pane as literal garbage.
pub fn parse_all(bytes: &[u8]) -> (Vec<MouseEvent>, &[u8]) {
    let mut events = Vec::new();
    let mut rest = bytes;
    loop {
        match parse_one(rest) {
            Some((ParsedInput::Mouse(event), consumed)) => {
                events.push(event);
                rest = &rest[consumed..];
            }
            Some((ParsedInput::Ignored, consumed)) => {
                // Recognized mouse input dimux has no use for (e.g.
                // middle/right-click) -- consumed so it can't leak
                // through as keyboard input, but not collected.
                rest = &rest[consumed..];
            }
            Some((ParsedInput::NotMouse, _)) | None => {
                // Either not SGR-shaped at all, or a recognized shape
                // with malformed content -- stop here and leave it
                // (plus anything after it) for the fallback path, same
                // as a single malformed sequence always has.
                break;
            }
        }
    }
    (events, rest)
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

    // -- parse_all: bundled-sequence handling (see module doc "Bundled
    //    sequences") ------------------------------------------------------

    #[test]
    fn parse_all_handles_multiple_bundled_scroll_ticks() {
        // Regression test for the reported bug: three quick scroll-up
        // ticks arriving in ONE read() used to fail `parse`'s
        // whole-buffer-is-one-sequence contract entirely and leak into
        // the pane as literal garbage. `parse_all` must peel all three.
        let (events, leftover) = parse_all(b"\x1b[<64;5;5M\x1b[<64;5;5M\x1b[<64;5;6M");
        assert_eq!(
            events,
            vec![
                MouseEvent::ScrollUp { col: 4, row: 4 },
                MouseEvent::ScrollUp { col: 4, row: 4 },
                MouseEvent::ScrollUp { col: 4, row: 5 },
            ]
        );
        assert!(leftover.is_empty());
    }

    #[test]
    fn parse_all_handles_a_single_sequence() {
        let (events, leftover) = parse_all(b"\x1b[<0;20;10M");
        assert_eq!(events, vec![MouseEvent::Down { col: 19, row: 9 }]);
        assert!(leftover.is_empty());
    }

    #[test]
    fn parse_all_consumes_ignored_sequences_without_collecting_them() {
        // A middle-click (button 1, `Ignored`) bundled between two
        // scrolls must be consumed (not leaked as keystrokes) but not
        // surface as a MouseEvent.
        let (events, leftover) = parse_all(b"\x1b[<64;5;5M\x1b[<1;5;5M\x1b[<65;5;5M");
        assert_eq!(
            events,
            vec![
                MouseEvent::ScrollUp { col: 4, row: 4 },
                MouseEvent::ScrollDown { col: 4, row: 4 },
            ]
        );
        assert!(leftover.is_empty());
    }

    #[test]
    fn parse_all_leaves_trailing_non_mouse_bytes_for_the_fallback_path() {
        // A scroll tick followed by keyboard bytes in the same read():
        // the scroll is handled, the keyboard tail is returned as
        // leftover for the chord/keyboard parser.
        let (events, leftover) = parse_all(b"\x1b[<64;5;5Mhello");
        assert_eq!(events, vec![MouseEvent::ScrollUp { col: 4, row: 4 }]);
        assert_eq!(leftover, b"hello");
    }

    #[test]
    fn parse_all_of_pure_non_mouse_input_collects_nothing_and_returns_it_all() {
        let (events, leftover) = parse_all(b"hello");
        assert!(events.is_empty());
        assert_eq!(leftover, b"hello");
    }
}
