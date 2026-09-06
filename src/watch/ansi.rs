// Raw-ANSI helpers for the dripw watch TUI.
// Golden rule: size/clip PLAIN text FIRST, then colorize — never truncate a
// string that already contains escape codes.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;

use regex::Regex;

// Global color switch. `--no-color` / NO_COLOR env flips this off.
static COLOR: AtomicBool = AtomicBool::new(true);

pub fn set_color_enabled(on: bool) {
    COLOR.store(on, Ordering::SeqCst);
}

/// Tests that flip the global colour switch serialize on this lock so a
/// parallel test does not observe the other's setting.
#[cfg(test)]
pub fn color_test_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

pub fn color_enabled() -> bool {
    COLOR.load(Ordering::SeqCst)
}

const ESC: &str = "\x1b";

// Matches CSI sequences (colors, cursor moves) so we can measure visible width.
fn ansi_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new("\x1b\\[[0-9;?]*[ -/]*[@-~]").unwrap())
}

pub fn strip_ansi(s: &str) -> String {
    ansi_re().replace_all(s, "").into_owned()
}

// Width of a single Unicode code point. Zero-width (combining) -> 0, wide -> 2.
pub fn char_width(cp: u32) -> usize {
    if cp == 0 {
        return 0;
    }
    // C0/C1 control chars render unpredictably; treat as zero so they don't skew.
    if cp < 32 || (0x7f..0xa0).contains(&cp) {
        return 0;
    }
    // Combining marks and zero-width joiners/spaces.
    if (0x0300..=0x036f).contains(&cp)
        || (0x1ab0..=0x1aff).contains(&cp)
        || (0x1dc0..=0x1dff).contains(&cp)
        || (0x20d0..=0x20ff).contains(&cp)
        || (0xfe00..=0xfe0f).contains(&cp) // variation selectors
        || (0xfe20..=0xfe2f).contains(&cp)
        || cp == 0x200b
        || cp == 0x200d
    {
        return 0;
    }
    // Common wide ranges: CJK, Hangul, Kana, wide punctuation, most emoji.
    if (0x1100..=0x115f).contains(&cp)
        || (0x2e80..=0x303e).contains(&cp)
        || (0x3041..=0x33ff).contains(&cp)
        || (0x3400..=0x4dbf).contains(&cp)
        || (0x4e00..=0x9fff).contains(&cp)
        || (0xa000..=0xa4cf).contains(&cp)
        || (0xac00..=0xd7a3).contains(&cp)
        || (0xf900..=0xfaff).contains(&cp)
        || (0xfe30..=0xfe4f).contains(&cp)
        || (0xff00..=0xff60).contains(&cp)
        || (0xffe0..=0xffe6).contains(&cp)
        || (0x1f300..=0x1faff).contains(&cp)
        || (0x20000..=0x3fffd).contains(&cp)
    {
        return 2;
    }
    1
}

// Visible width of a string (measures after stripping any escape codes).
pub fn string_width(s: &str) -> usize {
    let mut w = 0;
    for ch in strip_ansi(s).chars() {
        w += char_width(ch as u32);
    }
    w
}

// Clip/pad PLAIN text to exactly `width` visible columns. When truncation is
// needed and width >= 1, the last visible column becomes an ellipsis so the
// user can tell a value was cut. Guarantees string_width(result) === width.
pub fn fit(plain: &str, width: usize, ellipsis: bool) -> String {
    if width == 0 {
        return String::new();
    }
    let text = strip_ansi(plain).replace('\t', " ").replace(['\r', '\n'], " ");
    let mut out = String::new();
    let mut w = 0usize;
    for ch in text.chars() {
        let cw = char_width(ch as u32);
        if w + cw > width {
            if ellipsis {
                // Back off until there's room for a single '…' cell.
                while w + 1 > width && !out.is_empty() {
                    let last = out.chars().last().unwrap();
                    out.truncate(out.len() - last.len_utf8());
                    w -= char_width(last as u32);
                }
                out.push('…');
                w += 1;
            }
            break;
        }
        out.push(ch);
        w += cw;
    }
    while w < width {
        out.push(' ');
        w += 1;
    }
    out
}

fn sgr_split_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new("(\x1b\\[[0-9;?]*[ -/]*[@-~])").unwrap())
}

fn sgr_closers() -> &'static [(&'static str, Regex)] {
    static TABLE: OnceLock<Vec<(&'static str, Regex)>> = OnceLock::new();
    TABLE.get_or_init(|| {
        vec![
            ("22", Regex::new("^\\x1b\\[(?:1|2)m$").unwrap()),
            ("23", Regex::new("^\\x1b\\[3m$").unwrap()),
            ("24", Regex::new("^\\x1b\\[4m$").unwrap()),
            ("27", Regex::new("^\\x1b\\[7m$").unwrap()),
            ("29", Regex::new("^\\x1b\\[9m$").unwrap()),
            ("39", Regex::new("^\\x1b\\[(?:3[0-7]|9[0-7]|38;[0-9;]+)m$").unwrap()),
            ("49", Regex::new("^\\x1b\\[(?:4[0-7]|10[0-7]|48;[0-9;]+)m$").unwrap()),
        ]
    })
}

// Which openers a partial reset retires: 22 closes bold/faint, 23 italic,
// 24 underline, 27 reverse, 29 strike, 39 foreground, 49 background. Keeping
// the active set minimal means a wrapped line re-opens only live styles —
// chalk (marked-terminal) emits these partial resets, not \x1b[0m.
fn update_activity(activity: &mut Vec<String>, seg: &str) {
    let params = &seg[2..seg.len() - 1];
    if params.is_empty() || params == "0" {
        activity.clear();
        return;
    }
    if let Some((_, re)) = sgr_closers().iter().find(|(code, _)| *code == params) {
        activity.retain(|open| !re.is_match(open));
        return;
    }
    activity.push(seg.to_string());
}

fn sgr_tail_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new("(?:\x1b\\[[0-9;?]*[ -/]*[@-~])+$").unwrap())
}

struct Space {
    // Byte offset into `cur` of the space itself, the visible width before it,
    // and the styles live at that point — the preferred break for prose.
    at: usize,
    w: usize,
    activity: Vec<String>,
}

struct State {
    cur: String,
    w: usize,
    activity: Vec<String>,
    space: Option<Space>,
}

impl State {
    fn emit(&mut self, out: &mut Vec<String>, line: String, tail: String, tail_w: usize) {
        // Drop escapes trailing the last visible cell, then close any open style
        // so it cannot bleed into the pane padding that follows the row.
        let mut line = sgr_tail_re().replace(&line, "").into_owned();
        if line.contains(ESC) {
            line.push_str("\x1b[0m");
        }
        out.push(line);
        self.cur = self.activity.join("") + &tail;
        self.w = tail_w;
        self.space = None;
    }
}

// Wrap text that may contain SGR escape sequences by VISIBLE width (escapes
// occupy zero columns; embedded \n hard-breaks; tabs become two spaces). A
// line that breaks mid-style is closed with \x1b[0m and the currently active
// SGR sequences are re-opened on the continuation line so styling survives
// the wrap. \x1b[0m clears the activity set. Every returned line satisfies
// stringWidth(line) <= width for inputs whose widest char fits in `width`.
pub fn wrap_ansi(text: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![text.to_string()];
    }
    let mut out: Vec<String> = Vec::new();
    let mut st = State {
        cur: String::new(),
        w: 0,
        activity: Vec::new(),
        space: None,
    };
    for para in text.replace('\t', "  ").split('\n') {
        for seg in sgr_iter(para) {
            if seg.is_empty() {
                continue;
            }
            if seg.starts_with(ESC) {
                st.cur.push_str(&seg);
                if seg.ends_with('m') {
                    update_activity(&mut st.activity, &seg);
                }
                continue;
            }
            for ch in seg.chars() {
                let cw = char_width(ch as u32);
                if st.w > 0 && st.w + cw > width {
                    let use_space = st
                        .space
                        .as_ref()
                        .is_some_and(|s| s.w > 0)
                        && ch != ' ';
                    if use_space {
                        let space = st.space.take().unwrap();
                        // Break at the last space: the word in progress moves down whole.
                        let line = st.cur[..space.at].to_string();
                        let tail = st.cur[space.at + 1..].to_string();
                        let tail_w = st.w - space.w - 1;
                        let live_now = std::mem::take(&mut st.activity);
                        st.activity = space.activity;
                        st.emit(&mut out, line, tail, tail_w);
                        st.activity = live_now;
                        // A wide char can still overflow the new line: force it out.
                        if st.w + cw > width {
                            let line = std::mem::take(&mut st.cur);
                            st.emit(&mut out, line, String::new(), 0);
                        }
                    } else {
                        let line = std::mem::take(&mut st.cur);
                        st.emit(&mut out, line, String::new(), 0);
                    }
                    if ch == ' ' && st.w == 0 {
                        continue; // never start a line with the break space
                    }
                }
                if ch == ' ' {
                    st.space = Some(Space {
                        at: st.cur.len(),
                        w: st.w,
                        activity: st.activity.clone(),
                    });
                }
                st.cur.push(ch);
                st.w += cw;
            }
        }
        let line = std::mem::take(&mut st.cur);
        st.emit(&mut out, line, String::new(), 0);
    }
    out
}

fn sgr_iter(text: &str) -> Vec<String> {
    let re = sgr_re();
    let mut parts = Vec::new();
    let mut last = 0;
    for m in re.find_iter(text) {
        if m.start() > last {
            parts.push(text[last..m.start()].to_string());
        }
        parts.push(m.as_str().to_string());
        last = m.end();
    }
    if last < text.len() {
        parts.push(text[last..].to_string());
    }
    parts
}

fn sgr_re() -> &'static Regex {
    sgr_split_re()
}

// SGR wrappers. Each returns plain text untouched when color is disabled.
// Accent is cyan (matching the watch TUI); selection uses onCyan.
fn sgr(open: &str, s: &str) -> String {
    if color_enabled() {
        format!("{ESC}[{open}m{s}{ESC}[0m")
    } else {
        s.to_string()
    }
}

pub mod c {
    pub fn dim(s: &str) -> String {
        super::sgr("2", s)
    }
    pub fn bold(s: &str) -> String {
        super::sgr("1", s)
    }
    pub fn reverse(s: &str) -> String {
        super::sgr("7", s)
    }
    // Cyan accent (replaces sub-zero's teal)
    pub fn cyan(s: &str) -> String {
        super::sgr("36", s)
    }
    pub fn cyan_bold(s: &str) -> String {
        super::sgr("1;36", s)
    }
    pub fn accent(s: &str) -> String {
        super::sgr("36", s)
    }
    pub fn accent_bold(s: &str) -> String {
        super::sgr("1;36", s)
    }
    pub fn gray(s: &str) -> String {
        super::sgr("38;5;244", s)
    }
    pub fn green(s: &str) -> String {
        super::sgr("32", s)
    }
    pub fn yellow(s: &str) -> String {
        super::sgr("33", s)
    }
    pub fn red(s: &str) -> String {
        super::sgr("31", s)
    }
    pub fn blue(s: &str) -> String {
        super::sgr("34", s)
    }
    pub fn magenta(s: &str) -> String {
        super::sgr("35", s)
    }
    pub fn white(s: &str) -> String {
        super::sgr("97", s)
    }
    // Selected row: cyan background, dark foreground (sub-zero's onTeal analogue)
    pub fn on_cyan(s: &str) -> String {
        super::sgr("46;30", s)
    }
}

// Terminal control (only used by the interactive runtime, never in tests).
pub mod term {
    pub const ALT_SCREEN: &str = "\x1b[?1049h";
    pub const MAIN_SCREEN: &str = "\x1b[?1049l";
    pub const HIDE_CURSOR: &str = "\x1b[?25l";
    pub const SHOW_CURSOR: &str = "\x1b[?25h";
    pub const CLEAR: &str = "\x1b[2J";
    pub const HOME: &str = "\x1b[H";
    pub fn move_to(row: usize, col: usize) -> String {
        format!("\x1b[{};{}H", row, col)
    }
    /// Alias kept for callers that use the older name.
    pub fn cursor_to(row: usize, col: usize) -> String {
        move_to(row, col)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_ansi_removes_csi_sequences() {
        let _guard = color_test_lock();
        set_color_enabled(true);
        assert_eq!(strip_ansi("\x1b[31mred\x1b[0m"), "red");
        assert_eq!(string_width("\x1b[1mbold\x1b[0m"), 4);
        assert_eq!(string_width("\x1b[2;10Hx"), 1);
        assert_eq!(c::cyan("x"), "\x1b[36mx\x1b[0m");
        assert_eq!(string_width(&c::cyan("xyz")), 3);
        assert_eq!(term::move_to(3, 7), "\x1b[3;7H");
    }

    #[test]
    fn string_width_counts_unicode() {
        assert_eq!(string_width("héllo"), 5);
        assert_eq!(string_width("日本"), 4);
        // Combining mark adds zero width.
        assert_eq!(char_width(0x0301), 0);
        assert_eq!(string_width("e\u{0301}"), 1);
        assert_eq!(string_width("a\u{0301}bc"), 3);
    }

    #[test]
    fn fit_truncates_with_ellipsis_and_pads() {
        let f = fit("abcdef", 4, true);
        assert!(f.ends_with('…'));
        assert_eq!(string_width(&f), 4);
        let short = fit("abc", 4, true);
        assert_eq!(string_width(&short), 4);
        assert_eq!(string_width(&f), 4);
        assert_eq!(f.trim_end_matches('…'), "abc");
        assert_eq!(string_width(&f), 4);
        assert_eq!(f.trim_end_matches('…'), "abc");
        assert_eq!(string_width(&f), 4);
    }

    #[test]
    fn wrap_ansi_wraps_plain_sentence() {
        let text = "one two three four five six seven eight nine ten";
        let lines = wrap_ansi(text, 12);
        assert!(lines.len() > 1);
        for line in &lines {
            assert!(string_width(line) <= 12, "line too wide: {line:?}");
        }
        assert_eq!(lines.join(" "), text);
    }

    #[test]
    fn wrap_ansi_keeps_sgr_intact_across_wrap() {
        let text = "\x1b[31mone two three four five\x1b[0m";
        let lines = wrap_ansi(text, 10);
        assert!(lines.len() > 1);
        for line in &lines {
            assert!(string_width(line) <= 10);
            // The red code is re-opened on every line and each line is closed.
            assert!(line.starts_with("\x1b[31m"), "line missing sgr: {line:?}");
            assert!(line.ends_with("\x1b[0m"), "line not closed: {line:?}");
        }
        let visible: Vec<String> = lines.iter().map(|l| strip_ansi(l)).collect();
        assert_eq!(visible.join(" "), "one two three four five");
    }
}
