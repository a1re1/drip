//! Terminal pane-title rendering primitives (OSC 2 window-title updates).
//!
//! This module is intentionally split in two layers:
//!
//! - **Pure state/formatting** (`sanitize`, `fallback_title`, `format_title`,
//!   `frame_for`, and the `PaneTitle` state machine): no I/O, fully
//!   unit-testable. Every string that could ever reach a terminal passes
//!   through `sanitize`, which strips ESC/CSI/OSC sequences, BEL, all C0/C1
//!   control characters, quote characters, collapses whitespace and enforces
//!   the 5-word / character budget. Malicious model output or a hostile goal
//!   string can therefore never inject escape sequences into the title.
//! - **Emission** (`emit`): the only place that writes to the terminal, via
//!   the shared `crate::tui::term::write_out` convention.
//!
//! Behavior: a short stable label (derived deterministically from the initial
//! goal, optionally replaced later by a generated 3-5 word title) plus a
//! time-driven braille loading spinner while the harness is busy. No
//! percentage is ever invented. Updates are deduplicated (identical titles
//! are never re-emitted) and throttled (spinner advances at most once per
//! `SPINNER_INTERVAL_MS`). The idle state is the bare label without a
//! spinner.

use std::time::{Duration, Instant};

use crate::tui::term::write_out;

/// Braille spinner frames, cycled time-driven while busy.
pub const SPINNER_FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// Minimum real-time spacing between two spinner frames.
pub const SPINNER_INTERVAL_MS: u64 = 100;

/// Hard word budget for any title (generated or fallback).
pub const MAX_TITLE_WORDS: usize = 5;

/// Hard character budget for any title, applied after the word budget so a
/// few very long words still cannot produce an unbounded title.
pub const MAX_TITLE_CHARS: usize = 48;

/// Title used when the goal yields nothing usable.
pub const FALLBACK_LABEL: &str = "drip";

// ---------------------------------------------------------------------------
// Sanitization (pure)
// ---------------------------------------------------------------------------

/// True for C1 control characters (U+0080..=U+009F) beyond ASCII controls.
fn is_c1(c: char) -> bool {
    ('\u{80}'..='\u{9f}').contains(&c)
}

/// Quote-like characters are stripped from titles; they carry no meaning in a
/// pane label and keep hostile input from looking like escape terminators.
fn is_quote(c: char) -> bool {
    matches!(
        c,
        '"' | '\'' | '`' | '\u{2018}' | '\u{2019}' | '\u{201c}' | '\u{201d}' | '\u{ab}' | '\u{bb}'
    )
}

/// Removes every escape sequence and control character from `raw`.
///
/// Handles CSI (`ESC [ ... final`), OSC (`ESC ] ... BEL` or `ESC ] ... ESC \`),
/// and any other two-byte `ESC x` form, then drops remaining C0/C1 controls.
/// The result is guaranteed ESC-free and control-free.
pub fn strip_control(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars().peekable();

    while let Some(c) = chars.next() {
        if c == '\x1b' {
            match chars.peek() {
                Some('[') => {
                    chars.next();
                    // CSI: consume through the final byte (0x40..=0x7E), with
                    // a bounded length so a malformed sequence cannot eat the
                    // whole string.
                    let mut taken = 0usize;
                    while let Some(&next) = chars.peek() {
                        if ('\u{40}'..='\u{7e}').contains(&next) || taken >= 64 {
                            chars.next();
                            break;
                        }
                        chars.next();
                        taken += 1;
                    }
                }
                Some(']') | Some('P') | Some('X') | Some('^') | Some('_') => {
                    chars.next();
                    // OSC/DCS/SOS/PM/APC: consume through BEL or ESC \.
                    let mut last_was_esc = false;
                    let mut taken = 0usize;
                    for next in chars.by_ref() {
                        if next == '\x07' || (last_was_esc && next == '\\') || taken >= 512 {
                            break;
                        }
                        last_was_esc = next == '\x1b';
                        taken += 1;
                    }
                }
                _ => {
                    // Lone ESC or two-character escape: drop both bytes.
                    chars.next();
                }
            }
            continue;
        }
        if c == '\n' || c == '\r' || c == '\t' {
            // Line/indent boundaries become spaces so multi-line goals and
            // model output stay readable in a single-line title.
            out.push(' ');
            continue;
        }
        if c.is_control() || is_c1(c) {
            continue;
        }
        out.push(c);
    }

    out
}

/// Sanitizes an arbitrary string into a single-line, escape-free title body:
/// escape sequences and controls stripped, quotes removed, whitespace
/// (including newlines) collapsed to single ASCII spaces, trimmed.
pub fn sanitize(raw: &str) -> String {
    let stripped = strip_control(raw);
    let mut out = String::with_capacity(stripped.len());

    for word in stripped.split_whitespace() {
        if !out.is_empty() {
            out.push(' ');
        }
        for c in word.chars() {
            if !is_quote(c) {
                out.push(c);
            }
        }
    }
    out
}

/// Keeps at most `max_words` words and at most `max_chars` characters
/// (never splitting a word mid-character; whole words are dropped until the
/// remainder fits).
pub fn bound_words(raw: &str, max_words: usize, max_chars: usize) -> String {
    let mut out = String::new();
    for word in raw.split_whitespace().take(max_words) {
        let candidate = if out.is_empty() {
            word.to_string()
        } else {
            format!("{out} {word}")
        };
        if candidate.chars().count() > max_chars {
            break;
        }
        out = candidate;
    }
    out
}

/// Deterministic title derived from the initial user goal: sanitized, at most
/// `MAX_TITLE_WORDS` words and `MAX_TITLE_CHARS` characters, with a stable
/// fallback when nothing usable remains.
pub fn fallback_title(goal: &str) -> String {
    let cleaned = bound_words(&sanitize(goal), MAX_TITLE_WORDS, MAX_TITLE_CHARS);
    if cleaned.is_empty() {
        FALLBACK_LABEL.to_string()
    } else {
        cleaned
    }
}

/// Renders the full title text: an optional spinner frame prefix plus the
/// stable label. Pure formatting; no percentage is ever fabricated.
pub fn format_title(label: &str, frame: Option<&str>) -> String {
    match frame {
        Some(frame) => format!("{frame} {label}"),
        None => label.to_string(),
    }
}

/// Time-driven spinner frame for a busy interval (no harness events needed).
pub fn frame_for(elapsed: Duration) -> &'static str {
    let step = elapsed.as_millis() / SPINNER_INTERVAL_MS.max(1) as u128;
    SPINNER_FRAMES[(step % SPINNER_FRAMES.len() as u128) as usize]
}

/// Wraps a sanitized title in an OSC 2 (window title) escape sequence.
pub fn osc2(title: &str) -> String {
    format!("\x1b]2;{title}\x07")
}

// ---------------------------------------------------------------------------
// Emission (the only terminal-writing path)
// ---------------------------------------------------------------------------

/// Writes one title escape to the terminal, if any. Never called by tests.
pub fn emit(escape: Option<&str>) {
    if let Some(escape) = escape {
        write_out(escape);
    }
}

// ---------------------------------------------------------------------------
// State machine
// ---------------------------------------------------------------------------

/// Mutable pane-title state: the stable label plus busy/idle tracking with
/// deduplication and throttling. `tick`/`set_*` return the escape sequence to
/// emit (`None` when nothing changed), so callers stay in charge of writing.
pub struct PaneTitle {
    label: String,
    busy: bool,
    busy_since: Option<Instant>,
    last_emitted: Option<String>,
    last_emit_at: Option<Instant>,
}

impl PaneTitle {
    /// Starts from the initial goal, emitting nothing yet; the first call to
    /// `set_*`, `tick`, or `update` produces the first escape.
    pub fn new(goal: &str) -> Self {
        Self {
            label: fallback_title(goal),
            busy: false,
            busy_since: None,
            last_emitted: None,
            last_emit_at: None,
        }
    }

    pub fn label(&self) -> &str {
        &self.label
    }

    pub fn is_busy(&self) -> bool {
        self.busy
    }

    /// The escape for the current state, deduplicated against what was last
    /// emitted. Does not advance the throttle clock.
    fn candidate(&self, now: Instant) -> Option<String> {
        let frame = if self.busy {
            Some(frame_for(now - self.busy_since.unwrap_or(now)))
        } else {
            None
        };
        let title = format_title(&self.label, frame);
        if self.last_emitted.as_deref() == Some(title.as_str()) {
            return None;
        }
        Some(osc2(&title))
    }

    /// Recomputes the current title, deduplicates, and records emission.
    fn update(&mut self, now: Instant) -> Option<String> {
        let escape = self.candidate(now)?;
        self.last_emitted = Some(
            escape
                .strip_prefix("\x1b]2;")
                .and_then(|rest| rest.strip_suffix('\x07'))
                .unwrap_or(&escape)
                .to_string(),
        );
        self.last_emit_at = Some(now);
        Some(escape)
    }

    /// Replaces the label (e.g. with a generated 3-5 word title). Sanitized
    /// and word/char-bounded; returns the escape to emit when it differs.
    pub fn set_label(&mut self, raw: &str, now: Instant) -> Option<String> {
        self.label = fallback_title(raw);
        self.update(now)
    }

    /// Transitions busy/idle. Going idle emits the bare label (no spinner);
    /// going busy starts the spinner clock and emits the first frame.
    pub fn set_busy(&mut self, busy: bool, now: Instant) -> Option<String> {
        if self.busy == busy {
            return None;
        }
        self.busy = busy;
        self.busy_since = busy.then_some(now);
        self.update(now)
    }

    /// Advances the spinner while busy. Throttled to one frame per
    /// `SPINNER_INTERVAL_MS`; idle ticks produce nothing.
    pub fn tick(&mut self, now: Instant) -> Option<String> {
        if !self.busy {
            return None;
        }
        if let Some(last) = self.last_emit_at {
            if now.duration_since(last) < Duration::from_millis(SPINNER_INTERVAL_MS) {
                return None;
            }
        }
        self.update(now)
    }
}

// ---------------------------------------------------------------------------
// Tests (pure layer only — `emit` is never exercised here)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn fallback_bounds_goal_to_five_words_and_chars() {
        assert_eq!(fallback_title("fix the login bug"), "fix the login bug");
        assert_eq!(fallback_title("  refactor   the\t parser\n "), "refactor the parser");
        assert_eq!(
            fallback_title("one two three four five six seven"),
            "one two three four five"
        );
        // A single very long word is dropped whole, falling back cleanly.
        let long = "w".repeat(80);
        assert_eq!(fallback_title(&long), FALLBACK_LABEL);
        assert_eq!(fallback_title(""), FALLBACK_LABEL);
        assert_eq!(fallback_title("   "), FALLBACK_LABEL);
        assert!(
            fallback_title("ok this is fine but too long overall")
                .chars()
                .count()
                <= MAX_TITLE_CHARS
        );
    }

    #[test]
    fn sanitize_survives_malicious_model_and_goal_output() {
        // OSC body is consumed through BEL, so "pwned" never surfaces.
        assert_eq!(sanitize("\x1b]2;pwned\x07safe"), "safe");
        assert_eq!(sanitize("\x1b[2J\x1b[31mred\x1b[0m"), "red");
        assert_eq!(sanitize("a\x07b\x1bc"), "ab");
        assert_eq!(sanitize("x\u{9b}31my"), "x31my"); // C1 CSI single-byte form
        assert_eq!(sanitize("say \"hi\" now"), "say hi now");
        assert_eq!(sanitize("line1\r\nline2"), "line1 line2");
        assert!(!sanitize("\x1b]0;evil\x1b\\ok").contains('\x1b'));
        assert!(sanitize("tab\tsep").chars().all(|c| !c.is_control()));
    }

    #[test]
    fn unicode_labels_are_preserved_and_bounded() {
        assert_eq!(
            fallback_title("Débugger l'authentification"),
            "Débugger lauthentification"
        );
        assert_eq!(
            fallback_title("修复登录问题 增加测试 改进文档 优化性能 重构代码"),
            "修复登录问题 增加测试 改进文档 优化性能 重构代码"
        );
        let emoji = "🦀🦀🦀🦀🦀🦀🦀🦀";
        let bounded = fallback_title(emoji);
        assert!(bounded.chars().count() <= MAX_TITLE_CHARS);
        assert!(bounded.starts_with('🦀'));
    }

    #[test]
    fn spinner_frames_are_time_driven_and_cycle() {
        assert_eq!(frame_for(Duration::from_millis(0)), SPINNER_FRAMES[0]);
        assert_eq!(frame_for(Duration::from_millis(100)), SPINNER_FRAMES[1]);
        assert_eq!(frame_for(Duration::from_millis(950)), SPINNER_FRAMES[9]);
        assert_eq!(frame_for(Duration::from_millis(1000)), SPINNER_FRAMES[0]);
    }

    #[test]
    fn animation_advances_without_harness_events() {
        let start = Instant::now();
        let mut title = PaneTitle::new("fix login bug");
        let first = title.set_busy(true, start).expect("busy emits first frame");
        assert_eq!(first, osc2("⠋ fix login bug"));

        // tick at +150ms -> second frame, purely from the clock.
        let second = title
            .tick(start + Duration::from_millis(150))
            .expect("frame advances");
        assert_eq!(second, osc2("⠙ fix login bug"));

        // tick at +180ms -> throttled to None.
        assert_eq!(title.tick(start + Duration::from_millis(180)), None);

        // Much later the cycle wraps deterministically.
        let later = title
            .tick(start + Duration::from_millis(1000))
            .expect("frame advances");
        assert_eq!(later, osc2("⠋ fix login bug"));
    }

    #[test]
    fn idle_state_has_no_spinner_and_transitions_dedupe() {
        let start = Instant::now();
        let mut title = PaneTitle::new("fix login bug");

        // Idle from the start: one bare-label emission, ticks do nothing.
        assert_eq!(
            title.set_busy(false, start),
            None,
            "redundant idle set is a no-op"
        );
        assert_eq!(title.tick(start + Duration::from_secs(5)), None);

        // busy -> idle emits the bare label again.
        title.set_busy(true, start + Duration::from_secs(10));
        let idle = title
            .set_busy(false, start + Duration::from_secs(11))
            .expect("idle emits bare label");
        assert_eq!(idle, osc2("fix login bug"));

        // Repeated idle transitions are deduplicated.
        assert_eq!(title.set_busy(false, start + Duration::from_secs(12)), None);
        // Redundant busy set (already busy) is a no-op.
        title.set_busy(true, start + Duration::from_secs(13));
        assert_eq!(title.set_busy(true, start + Duration::from_secs(14)), None);
    }

    #[test]
    fn first_update_emits_bare_label_once() {
        let start = Instant::now();
        let mut title = PaneTitle::new("fix login bug");
        // The constructor emits nothing; the first update does, exactly once,
        // and a second identical update is deduplicated.
        assert_eq!(title.update(start), Some(osc2("fix login bug")));
        assert_eq!(title.update(start + Duration::from_millis(1)), None);
    }

    #[test]
    fn identical_titles_are_deduplicated() {
        let start = Instant::now();
        let mut title = PaneTitle::new("fix login bug");
        assert!(title.set_busy(true, start).is_some());
        // tick at +30ms: the spinner frame advanced (⠋ -> ⠙), so the composed
        // title differs and is emitted once; a further tick inside the same
        // 100ms window is throttled to None.
        // tick at +30ms is inside the 100ms spinner window -> throttled.
        assert_eq!(title.tick(start + Duration::from_millis(30)), None);
        // tick at +150ms advances the frame purely from the clock.
        let second = title
            .tick(start + Duration::from_millis(150))
            .expect("frame advances");
        assert_eq!(second, osc2("⠙ fix login bug"));
        // tick at +180ms is throttled again.
        assert_eq!(title.tick(start + Duration::from_millis(180)), None);
        // Setting the same label emits nothing (frame and label unchanged).
        assert_eq!(
            title
                .set_label("fix login bug", start + Duration::from_millis(160)),
            None
        );
        // A different label emits again.
        assert!(
            title
                .set_label("refactor parser", start + Duration::from_millis(240))
                .is_some()
        );
        // Idle: bare label emitted, then the same words in an unsanitized
        // variant sanitize to the identical label and dedupe to None.
        assert!(
            title
                .set_busy(false, start + Duration::from_millis(250))
                .is_some()
        );
        assert_eq!(
            title
                .set_label("  refactor\t\"parser\" ", start + Duration::from_millis(260)),
            None
        );
    }

    #[test]
    fn generated_labels_stay_bounded_and_safe() {
        let start = Instant::now();
        let mut title = PaneTitle::new("fix login bug");
        title.set_busy(true, start);

        let hostile = "\x1b]2;hacked\x07 write tests   for   parser now please extra";
        let escape = title
            .set_label(hostile, start + Duration::from_millis(150))
            .expect("new label emits");
        let inner = escape
            .strip_prefix("\x1b]2;")
            .and_then(|rest| rest.strip_suffix('\x07'))
            .unwrap();
        // The injected OSC header (through BEL) is consumed, so "hacked" is gone.
        assert_eq!(inner, "⠙ write tests for parser now");
        assert!(!inner.contains("hacked"));
        assert!(!inner.contains('\x1b'));
        // 5 label words plus the one spinner frame.
        assert!(inner.split_whitespace().count() <= MAX_TITLE_WORDS + 1);
    }
}
