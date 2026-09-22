// SGR (1006) mouse-event parsing for the dripw watch TUI.
//
// Once mouse reporting is on, the terminal writes pointer activity to stdin as
// `ESC [ < Cb ; Cx ; Cy M`: Cb 64 = wheel up and 65 = wheel down (the 64 flag
// marks a wheel event, the low bit picks the direction), Cb 0 = left-button
// press. Cx/Cy are the 1-based column and row the pointer hovered. dripw acts
// on the wheel and on a left click; motion, releases and the other buttons are
// ignored so the selection only moves where the user aimed it.

/// Prefix of every SGR mouse report (`CSI <`).
pub const SGR_PREFIX: &str = "\x1b[<";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseWheel {
    Up,
    Down,
}

/// One wheel report: the direction plus the 1-based cell under the pointer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MouseScroll {
    pub wheel: MouseWheel,
    pub col: usize,
    pub row: usize,
}

impl MouseScroll {
    /// Scroll delta for a `scroll_transcript`-style call: negative moves
    /// toward older lines (wheel up), positive toward newer (wheel down).
    pub fn delta(&self, lines: i64) -> i64 {
        match self.wheel {
            MouseWheel::Up => -lines,
            MouseWheel::Down => lines,
        }
    }

    /// Whether the pointer sits inside a 1-based inclusive rectangle
    /// `(row_start, row_end, col_start, col_end)`.
    pub fn inside(&self, rect: (usize, usize, usize, usize)) -> bool {
        let (row_start, row_end, col_start, col_end) = rect;
        self.row >= row_start && self.row <= row_end && self.col >= col_start && self.col <= col_end
    }
}

/// One pointer report: a wheel notch, or a left-button press on a cell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseEvent {
    Wheel(MouseScroll),
    /// A left-button press with the 1-based cell under the pointer. Releases,
    /// drags, motion and the other buttons are not clicks.
    Click {
        col: usize,
        row: usize,
    },
}

/// Parse one SGR mouse report at the first byte of `chunk`: a wheel notch or a
/// left-button press. `None` for anything else (a key, a truncated report, a
/// right/middle button, a release, a drag, or an extended report). Bytes after
/// the first report are ignored.
pub fn parse_mouse_event(chunk: &str) -> Option<MouseEvent> {
    let (cb, col, row, terminator) = parse_sgr_body(chunk)?;
    // 64 marks a wheel event; bits 0-1 carry the direction (0 up, 1 down).
    if cb & 64 != 0 {
        let wheel = if cb & 1 == 0 {
            MouseWheel::Up
        } else {
            MouseWheel::Down
        };
        return Some(MouseEvent::Wheel(MouseScroll { wheel, col, row }));
    }
    // Bit 5 marks pointer motion (a drag) and the 'm' terminator a release:
    // only a fresh left-button press navigates. Modifier bits are ignored.
    if cb & 3 == 0 && cb & 32 == 0 && terminator == 'M' {
        return Some(MouseEvent::Click { col, row });
    }
    None
}

/// The wheel-only view of a report, kept for callers that never navigate.
pub fn parse_sgr_mouse(chunk: &str) -> Option<MouseScroll> {
    match parse_mouse_event(chunk)? {
        MouseEvent::Wheel(scroll) => Some(scroll),
        MouseEvent::Click { .. } => None,
    }
}

/// `Cb ; Cx ; Cy` plus the terminating `M` (press) / `m` (release).
fn parse_sgr_body(chunk: &str) -> Option<(u32, usize, usize, char)> {
    let rest = chunk.strip_prefix(SGR_PREFIX)?;
    let end = rest.find(['M', 'm'])?;
    let terminator = rest[end..].chars().next()?;
    let body = &rest[..end];
    let mut parts = body.split(';');
    let cb: u32 = parts.next()?.parse().ok()?;
    let col: usize = parts.next()?.parse().ok()?;
    let row: usize = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some((cb, col, row, terminator))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_wheel_reports_with_hover_cell() {
        assert_eq!(
            parse_sgr_mouse("\x1b[<64;12;7M"),
            Some(MouseScroll {
                wheel: MouseWheel::Up,
                col: 12,
                row: 7
            })
        );
        assert_eq!(
            parse_sgr_mouse("\x1b[<65;100;41M"),
            Some(MouseScroll {
                wheel: MouseWheel::Down,
                col: 100,
                row: 41
            })
        );
    }

    #[test]
    fn parses_wheel_reports_with_modifiers_and_release_terminator() {
        // Cb 68 = wheel up + shift, 81 = wheel down + ctrl; 'm' is the release form.
        assert_eq!(
            parse_sgr_mouse("\x1b[<68;3;4M").map(|s| s.wheel),
            Some(MouseWheel::Up)
        );
        assert_eq!(
            parse_sgr_mouse("\x1b[<81;3;4m").map(|s| s.wheel),
            Some(MouseWheel::Down)
        );
    }

    #[test]
    fn ignores_keys_clicks_and_junk() {
        assert_eq!(parse_sgr_mouse("q"), None);
        assert_eq!(parse_sgr_mouse("\x1b[B"), None); // arrow key, not a mouse report
        assert_eq!(parse_sgr_mouse("\x1b[<0;10;5M"), None); // left button press
        assert_eq!(parse_sgr_mouse("\x1b[<64;10M"), None); // truncated: no row
        assert_eq!(parse_sgr_mouse("\x1b[<64;10;5;9M"), None); // extra field
        assert_eq!(
            parse_sgr_mouse("\x1b[<64;10;5M\x1b[<65;10;5M"),
            Some(MouseScroll {
                wheel: MouseWheel::Up,
                col: 10,
                row: 5
            })
        );
    }

    #[test]
    fn delta_and_hit_testing_follow_the_pointer() {
        let up = parse_sgr_mouse("\x1b[<64;12;7M").unwrap();
        let down = parse_sgr_mouse("\x1b[<65;12;7M").unwrap();
        assert_eq!(up.delta(3), -3);
        assert_eq!(down.delta(3), 3);

        let log = (1, 10, 1, 120);
        assert!(up.inside(log));
        let below = parse_sgr_mouse("\x1b[<64;12;11M").unwrap();
        let right_of = parse_sgr_mouse("\x1b[<64;121;7M").unwrap();
        assert!(!below.inside(log));
        assert!(!right_of.inside(log));
    }

    fn wheel_of(event: MouseEvent) -> MouseWheel {
        match event {
            MouseEvent::Wheel(scroll) => scroll.wheel,
            MouseEvent::Click { .. } => panic!("expected a wheel report"),
        }
    }

    #[test]
    fn left_press_is_a_click_and_the_other_buttons_are_ignored() {
        assert_eq!(
            parse_mouse_event("\x1b[<0;12;7M"),
            Some(MouseEvent::Click { col: 12, row: 7 })
        );
        // Modifier bits ride along with the button and still mean the left one.
        assert_eq!(
            parse_mouse_event("\x1b[<4;3;4M"),
            Some(MouseEvent::Click { col: 3, row: 4 })
        );
        assert_eq!(
            parse_mouse_event("\x1b[<16;3;4M"),
            Some(MouseEvent::Click { col: 3, row: 4 })
        );
        // Middle (1), right (2), release (3), a drag (32) and the 'm' release
        // terminator never navigate.
        assert_eq!(parse_mouse_event("\x1b[<1;10;5M"), None);
        assert_eq!(parse_mouse_event("\x1b[<2;10;5M"), None);
        assert_eq!(parse_mouse_event("\x1b[<3;10;5M"), None);
        assert_eq!(parse_mouse_event("\x1b[<32;10;5M"), None);
        assert_eq!(parse_mouse_event("\x1b[<0;10;5m"), None);
        assert_eq!(parse_mouse_event("q"), None);
        assert_eq!(parse_mouse_event("\x1b[B"), None);
    }

    #[test]
    fn wheel_reports_survive_the_event_layer() {
        assert_eq!(
            parse_mouse_event("\x1b[<64;12;7M").map(wheel_of),
            Some(MouseWheel::Up)
        );
        assert_eq!(
            parse_mouse_event("\x1b[<65;3;4M").map(wheel_of),
            Some(MouseWheel::Down)
        );
        // The wheel-only helper still rejects a click.
        assert_eq!(parse_sgr_mouse("\x1b[<0;12;7M"), None);
    }
}
