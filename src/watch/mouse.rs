// SGR (1006) mouse-event parsing for the dripw watch TUI.
//
// Once mouse reporting is on, the terminal writes wheel motion to stdin as
// `ESC [ < Cb ; Cx ; Cy M`: Cb 64 = wheel up and 65 = wheel down (the 64 flag
// marks a wheel event, the low bit picks the direction), Cx/Cy are the
// 1-based column and row the pointer hovered. dripw listens only for the
// wheel — clicks, motion and releases are ignored so the selection stays
// where the keyboard put it.

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

/// Parse a mouse report that starts at the first byte of `chunk`; `None` for
/// anything else (a key, a truncated report, a non-wheel button, or an
/// extended report). Bytes after the first report are ignored.
pub fn parse_sgr_mouse(chunk: &str) -> Option<MouseScroll> {
    let rest = chunk.strip_prefix(SGR_PREFIX)?;
    let end = rest.find(|c: char| c == 'M' || c == 'm')?;
    let body = &rest[..end];
    let mut parts = body.split(';');
    let cb: u32 = parts.next()?.parse().ok()?;
    let col: usize = parts.next()?.parse().ok()?;
    let row: usize = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    // 64 marks a wheel event; bits 0-1 carry the direction (0 up, 1 down).
    if cb & 64 == 0 {
        return None;
    }
    let wheel = if cb & 1 == 0 {
        MouseWheel::Up
    } else {
        MouseWheel::Down
    };
    Some(MouseScroll { wheel, col, row })
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
}
