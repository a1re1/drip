// Shared raw-terminal plumbing for dripw and drip --tui: raw mode with
// output post-processing kept, the TIOCGWINSZ size probe, and a flushing
// stdout writer.

use std::io::Write;

pub struct RawMode {
    original: Option<libc::termios>,
}

impl RawMode {
    pub fn enable() -> Self {
        // SAFETY: tcgetattr/tcsetattr on fd 0 with a zeroed termios out-param.
        unsafe {
            let mut termios: libc::termios = std::mem::zeroed();

            if libc::isatty(0) == 0 || libc::tcgetattr(0, &mut termios) != 0 {
                return Self { original: None }; // not a TTY
            }

            let original = termios;

            libc::cfmakeraw(&mut termios);
            // Keep output post-processing (\n → \r\n) so the frame's newlines land.
            termios.c_oflag = original.c_oflag;
            libc::tcsetattr(0, libc::TCSANOW, &termios);

            Self { original: Some(original) }
        }
    }

    pub fn restore(&mut self) {
        if let Some(original) = self.original.take() {
            // SAFETY: restoring the termios captured by enable().
            unsafe {
                libc::tcsetattr(0, libc::TCSANOW, &original);
            }
        }
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        self.restore();
    }
}

/// `process.stdout.columns || 80`, `rows || 24` — a size-less PTY reports
/// 0×0, which must also fall back.
pub fn terminal_size() -> (usize, usize) {
    // SAFETY: TIOCGWINSZ fills a winsize struct; failure leaves it zeroed.
    unsafe {
        let mut size: libc::winsize = std::mem::zeroed();

        if libc::ioctl(1, libc::TIOCGWINSZ, &mut size) != 0 {
            return (80, 24);
        }

        let cols = if size.ws_col == 0 { 80 } else { size.ws_col as usize };
        let rows = if size.ws_row == 0 { 24 } else { size.ws_row as usize };

        (cols, rows)
    }
}

pub fn write_out(text: &str) {
    let mut stdout = std::io::stdout().lock();

    let _ = stdout.write_all(text.as_bytes());
    let _ = stdout.flush();
}

