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
//! percentage is ever invented. While the harness is blocked on an ask_user
//! survey the prefix is the waiting marker `?` instead of a spinner (a
//! spinner would claim progress the blocked run is not making). Updates are
//! deduplicated (identical titles
//! are never re-emitted) and throttled (spinner advances at most once per
//! `SPINNER_INTERVAL_MS`). The idle state is the bare label without a
//! spinner.

use std::time::{Duration, Instant};

use crate::tui::term::write_out;

/// Braille spinner frames, cycled time-driven while busy.
pub const SPINNER_FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// Braille Patterns block (U+2800..=U+28FF): the spinner frames. Allowed
/// explicitly because these code points are Unicode symbols, not letters.
const BRAILLE_BLOCK: std::ops::RangeInclusive<char> = '\u{2800}'..='\u{28ff}';

/// Combining-mark ranges whose script has precomposed forms: a decomposed
/// `e` + U+0301 renders as two cells in terminals with imperfect grapheme
/// handling, so those marks are dropped and only the precomposed letter
/// (`é`, U+00E9) is allowed through the allowlist.
fn is_combining_mark(c: char) -> bool {
    matches!(
        c,
        '\u{0300}'..='\u{036f}'
            | '\u{0483}'..='\u{0489}'
            | '\u{0591}'..='\u{05bd}'
            | '\u{0610}'..='\u{061a}'
            | '\u{064b}'..='\u{065f}'
            | '\u{0e31}'..='\u{0e3a}'
            | '\u{1ab0}'..='\u{1aff}'
            | '\u{1dc0}'..='\u{1dff}'
            | '\u{20d0}'..='\u{20f0}'
            | '\u{fe20}'..='\u{fe2f}'
    )
}

/// Scripts written with combining marks that carry no precomposed equivalent
/// (Indic, Thai, Lao, Tibetan, Khmer, Myanmar). Dropping one of those marks
/// leaves a consonant skeleton that still *looks* like real text — silent title
/// corruption — so every character of these blocks is rejected outright.
const ABUGIDA_BLOCKS: [std::ops::RangeInclusive<char>; 13] = [
    '\u{0900}'..='\u{097f}', // Devanagari
    '\u{0980}'..='\u{09ff}', // Bengali
    '\u{0a00}'..='\u{0a7f}', // Gurmukhi
    '\u{0a80}'..='\u{0aff}', // Gujarati
    '\u{0b00}'..='\u{0b7f}', // Oriya
    '\u{0b80}'..='\u{0bff}', // Tamil
    '\u{0c00}'..='\u{0c7f}', // Telugu
    '\u{0c80}'..='\u{0cff}', // Kannada
    '\u{0d00}'..='\u{0d7f}', // Malayalam
    '\u{0d80}'..='\u{0dff}', // Sinhala
    '\u{0e00}'..='\u{0eff}', // Thai + Lao
    '\u{0f00}'..='\u{0fff}', // Tibetan
    '\u{1000}'..='\u{109f}', // Myanmar
];

/// True when `c` belongs to a script whose letters cannot survive mark
/// stripping intact.
fn is_abugida_char(c: char) -> bool {
    ABUGIDA_BLOCKS.iter().any(|block| block.contains(&c))
}

/// Non-ASCII punctuation and symbols that are safe in a pane title: no emoji,
/// no pictographs, no format or zero-width characters.
fn is_safe_punctuation(c: char) -> bool {
    matches!(
        c,
        '\u{00a7}' // section sign
            | '\u{00b0}' // degree sign
            | '\u{00b7}' // middle dot
            | '\u{2013}' // en dash
            | '\u{2014}' // em dash
            | '\u{2022}' // bullet
            | '\u{2026}' // ellipsis
            | '\u{3001}' // ideographic comma
            | '\u{3002}' // ideographic full stop
            | '\u{30fb}' // katakana middle dot
    )
}

/// The allowlist every emitted title character must satisfy.
///
/// Kept deliberately narrow so the payload survives terminals and multiplexers
/// that mangle wide or emoji code points: ASCII graphic characters and space,
/// Unicode letters and numbers (precomposed accents, CJK, Greek, Cyrillic),
/// the braille spinner block, and a short punctuation list. Everything else is
/// dropped — notably ESC, C0/C1 controls, DEL, emoji and other pictographs,
/// zero-width joiners/non-joiners, variation selectors, bidi/format characters,
/// line and paragraph separators, and private-use code points.
pub fn is_allowed_title_char(c: char) -> bool {
    if is_abugida_char(c) {
        return false;
    }
    // ASCII graphic characters and the space; DEL and every C0 control are
    // excluded by construction.
    if (' '..='~').contains(&c) {
        return true;
    }
    if BRAILLE_BLOCK.contains(&c) {
        return true;
    }
    if c.is_alphanumeric() && !is_combining_mark(c) {
        return true;
    }
    is_safe_punctuation(c)
}

/// Minimum real-time spacing between two spinner frames.
pub const SPINNER_INTERVAL_MS: u64 = 100;

/// Prefix shown while the harness is blocked on an ask_user survey: the run
/// waits on the operator, so the title must not pretend to be working.
pub const WAITING_FRAME: &str = "?";

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
        // A word made entirely of dropped characters (an emoji-only word, a
        // word of format characters) must not leave a double space behind.
        let word: String = word
            .chars()
            .filter(|&c| !is_quote(c) && is_allowed_title_char(c))
            .collect();
        // The collection above already applied the quote filter and the
        // allowlist once per character, so the word is emitted verbatim: a pane
        // title can never carry an emoji, a zero-width joiner or a variation
        // selector.
        if word.is_empty() {
            continue;
        }
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(&word);
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

/// The standard string terminator (ESC backslash) for OSC sequences.
///
/// ST is the documented terminator; BEL (0x07) is the legacy alternative some
/// terminals still prefer, but ST parses identically everywhere, so both
/// escapes drip emits use it.
pub const ST: &str = "\x1b\\";

/// Wraps a sanitized title in BOTH window-title escapes, each ST-terminated:
///
/// - `ESC ] 0 ; <title> ESC \` — icon name *and* window title. Several
///   terminals (and tmux's `set-titles on`) key the visible title off OSC 0,
///   which is why a drip pane could keep showing the bare process name
///   ("drip") while the OSC 2 update alone went unnoticed.
/// - `ESC ] 2 ; <title> ESC \` — window title, the sequence drip has always
///   emitted.
///
/// The payload is passed through untouched: callers sanitize first, and
/// [`PaneTitle`] deduplicates on the title text, not on the escape bytes.
pub fn osc(title: &str) -> String {
    format!("\x1b]0;{title}{ST}\x1b]2;{title}{ST}")
}

/// Recovers the title payload from an escape produced by [`osc`]. Returns
/// `None` for anything that is not one of our title escapes, so callers
/// degrade to "record the whole string" rather than to garbage.
pub fn title_payload(escape: &str) -> Option<&str> {
    let rest = escape.strip_prefix("\x1b]0;")?;
    let end = rest.find(ST)?;
    Some(&rest[..end])
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
    awaiting: bool,
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
            awaiting: false,
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

    /// True while the title waits on an operator answer (ask_user survey).
    pub fn is_waiting(&self) -> bool {
        self.awaiting
    }

    /// The spinner animates only while working; a waiting title stays static.
    fn allows_spinner(&self) -> bool {
        !self.awaiting
    }

    /// The escape for the current state, deduplicated against what was last
    /// emitted. Does not advance the throttle clock.
    fn candidate(&self, now: Instant) -> Option<String> {
        let frame = if self.busy && self.allows_spinner() {
            Some(frame_for(now - self.busy_since.unwrap_or(now)))
        } else if self.busy {
            // Waiting on the operator: a spinner would claim progress.
            Some(WAITING_FRAME)
        } else {
            None
        };
        let title = format_title(&self.label, frame);
        if self.last_emitted.as_deref() == Some(title.as_str()) {
            return None;
        }
        Some(osc(&title))
    }

    /// Recomputes the current title, deduplicates, and records emission.
    fn update(&mut self, now: Instant) -> Option<String> {
        let escape = self.candidate(now)?;
        self.last_emitted = Some(title_payload(&escape).unwrap_or(&escape).to_string());
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

    /// Marks the title as waiting on an operator answer (ask_user survey):
    /// the spinner prefix becomes `?` and stays static. Returns the escape to
    /// emit when the rendered title changes.
    pub fn set_waiting(&mut self, waiting: bool, now: Instant) -> Option<String> {
        if self.awaiting == waiting {
            return None;
        }
        self.awaiting = waiting;
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
        assert_eq!(
            fallback_title("  refactor   the\t parser\n "),
            "refactor the parser"
        );
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
        // Emoji are NOT title-safe characters: an all-emoji goal falls back to
        // the bare label instead of emitting pictographs a terminal may mangle.
        let emoji = "🦀🦀🦀🦀🦀🦀🦀🦀";
        let bounded = fallback_title(emoji);
        assert_eq!(bounded, FALLBACK_LABEL);
        assert!(bounded.chars().count() <= MAX_TITLE_CHARS);
        // Emoji mixed with real words keep only the words.
        assert_eq!(fallback_title("fix 🦀 login bug"), "fix login bug");
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
        assert_eq!(first, osc("⠋ fix login bug"));

        // tick at +150ms -> second frame, purely from the clock.
        let second = title
            .tick(start + Duration::from_millis(150))
            .expect("frame advances");
        assert_eq!(second, osc("⠙ fix login bug"));

        // tick at +180ms -> throttled to None.
        assert_eq!(title.tick(start + Duration::from_millis(180)), None);

        // Much later the cycle wraps deterministically.
        let later = title
            .tick(start + Duration::from_millis(1000))
            .expect("frame advances");
        assert_eq!(later, osc("⠋ fix login bug"));
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
        assert_eq!(idle, osc("fix login bug"));

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
        assert_eq!(title.update(start), Some(osc("fix login bug")));
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
        assert_eq!(second, osc("⠙ fix login bug"));
        // tick at +180ms is throttled again.
        assert_eq!(title.tick(start + Duration::from_millis(180)), None);
        // Setting the same label emits nothing (frame and label unchanged).
        assert_eq!(
            title.set_label("fix login bug", start + Duration::from_millis(160)),
            None
        );
        // A different label emits again.
        assert!(title
            .set_label("refactor parser", start + Duration::from_millis(240))
            .is_some());
        // Idle: bare label emitted, then the same words in an unsanitized
        // variant sanitize to the identical label and dedupe to None.
        assert!(title
            .set_busy(false, start + Duration::from_millis(250))
            .is_some());
        assert_eq!(
            title.set_label(
                "  refactor\t\"parser\" ",
                start + Duration::from_millis(260)
            ),
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
        let inner = title_payload(&escape).expect("an OSC 0 title escape");
        // The injected OSC header (through BEL) is consumed, so "hacked" is gone.
        assert_eq!(inner, "⠙ write tests for parser now");
        assert!(!inner.contains("hacked"));
        assert!(!inner.contains('\x1b'));
        // 5 label words plus the one spinner frame.
        assert!(inner.split_whitespace().count() <= MAX_TITLE_WORDS + 1);
    }

    #[test]
    fn waiting_replaces_the_spinner_and_resumes_it_after_answer() {
        let start = Instant::now();
        let mut title = PaneTitle::new("fix login bug");
        let busy = title.set_busy(true, start).expect("busy emits first frame");
        assert_eq!(busy, osc("⠋ fix login bug"));

        // The run blocks on an ask_user survey: the spinner becomes `?`.
        assert_eq!(
            title.set_waiting(true, start + Duration::from_millis(10)),
            Some(osc("? fix login bug"))
        );
        // A redundant set dedupes, and waiting is static: ticks emit nothing.
        assert_eq!(
            title.set_waiting(true, start + Duration::from_millis(20)),
            None
        );
        assert_eq!(title.tick(start + Duration::from_millis(150)), None);
        assert_eq!(title.tick(start + Duration::from_secs(1)), None);

        // Answers recorded: the spinner resumes with the current frame.
        let resumed = title
            .set_waiting(false, start + Duration::from_millis(1000))
            .expect("resume emits the spinner again");
        assert_eq!(resumed, osc("⠋ fix login bug"));
        let advanced = title
            .tick(start + Duration::from_millis(1100))
            .expect("frames advance again");
        assert_ne!(advanced, resumed);

        // Waiting follows the run, not the title: a title that never went
        // busy shows the bare label, and going busy while waiting shows `?`.
        let mut idle = PaneTitle::new("fix login bug");
        assert_eq!(idle.set_waiting(true, start), Some(osc("fix login bug")));
        assert_eq!(
            idle.set_busy(true, start + Duration::from_millis(1)),
            Some(osc("? fix login bug"))
        );
        assert_eq!(
            idle.set_busy(false, start + Duration::from_millis(2)),
            Some(osc("fix login bug"))
        );
    }
    #[test]
    fn emitted_escape_carries_both_title_codes_st_terminated() {
        let escape = osc("fix login bug");
        // Exactly two OSC sequences, in order: OSC 0 then OSC 2.
        assert_eq!(
            escape,
            "\x1b]0;fix login bug\x1b\\\x1b]2;fix login bug\x1b\\"
        );
        assert!(escape.starts_with("\x1b]0;"));
        assert!(escape.ends_with(ST));
        assert_eq!(
            escape.matches('\x1b').count(),
            4,
            "two ESC in, two ESC \\ out"
        );
        assert_eq!(
            escape.matches('\x1b').count(),
            escape.matches(ST).count() * 2,
            "every ESC belongs to an escape introducer or an ST terminator"
        );
        // The BEL legacy terminator is never emitted.
        assert!(!escape.contains('\x07'), "{escape:?}");
        // Both halves carry the identical payload.
        assert_eq!(title_payload(&escape), Some("fix login bug"));
        let second = escape
            .split(ST)
            .find(|part| part.starts_with("\x1b]2;"))
            .expect("the OSC 2 half");
        assert_eq!(second, "\x1b]2;fix login bug");
    }

    #[test]
    fn pane_title_emissions_use_the_st_terminated_pair() {
        let start = Instant::now();
        let mut title = PaneTitle::new("fix login bug");
        for escape in [
            title.set_busy(true, start).expect("busy emits"),
            title
                .tick(start + Duration::from_millis(150))
                .expect("frame advances"),
            title
                .set_waiting(true, start + Duration::from_millis(200))
                .expect("waiting emits"),
            title
                .set_busy(false, start + Duration::from_millis(250))
                .expect("idle emits"),
        ] {
            assert!(escape.starts_with("\x1b]0;"), "{escape:?}");
            assert!(escape.ends_with(ST), "{escape:?}");
            assert!(!escape.contains('\x07'), "{escape:?}");
            // The payload round-trips out of the escape unchanged.
            let payload = title_payload(&escape).expect("payload");
            assert!(!payload.is_empty());
            assert_eq!(title_payload(&escape), Some(payload));
        }
    }

    #[test]
    fn a_tmux_style_title_round_trip_recovers_the_payload() {
        // tmux rewrites the pane title from OSC 0 and OSC 2 and re-emits it
        // to the outer terminal; anything the multiplexer cannot carry shows
        // up here as a payload that differs from what drip intended.
        let emitted = osc("⠙ fix login bug");
        let oscs: Vec<&str> = emitted
            .split(ST)
            .filter(|p| p.starts_with('\x1b'))
            .collect();
        assert_eq!(oscs.len(), 2, "OSC 0 and OSC 2, nothing else: {emitted:?}");

        // What a terminal left after consuming the stream.
        let mut icon = None;
        let mut window = None;
        for body in &oscs {
            let payload = body.trim_start_matches('\x1b').trim_start_matches(']');
            let (code, text) = payload.split_once(';').expect("code;payload");
            match code {
                "0" => icon = Some(text.to_string()),
                "2" => window = Some(text.to_string()),
                other => panic!("unexpected OSC code {other:?} in {body:?}"),
            }
        }
        assert_eq!(icon.as_deref(), Some("⠙ fix login bug"));
        assert_eq!(
            window, icon,
            "both codes must agree or tmux shows one of them"
        );
        // And the recovered text is exactly what drip meant to show.
        assert_eq!(window.as_deref(), Some(title_payload(&emitted).unwrap()));
    }

    #[test]
    fn the_title_allowlist_rejects_emoji_and_invisible_characters() {
        // Kept: the braille spinner, precomposed accented letters, CJK,
        // Cyrillic, Greek, digits and the punctuation drip emits itself.
        for kept in ['⠋', 'é', 'ß', '修', 'Д', 'Ω', '7', '?', ' ', '~', '·', '…'] {
            assert!(is_allowed_title_char(kept), "{kept:?} must be allowed");
        }
        // Rejected: emoji and other pictographs, flags, ZWJ/ZWNJ, variation
        // selectors, bidi and zero-width format controls, line/paragraph
        // separators, private-use characters, and every control.
        for dropped in [
            '\u{1f980}',                                  // crab emoji
            '\u{2764}',                                   // heavy black heart
            "\u{1f1fa}\u{1f1f8}".chars().next().unwrap(), // regional indicator
            '\u{200d}',                                   // zero-width joiner
            '\u{200c}',                                   // zero-width non-joiner
            '\u{200b}',                                   // zero-width space
            '\u{fe0f}',                                   // variation selector-16
            '\u{fe0e}',                                   // variation selector-15
            '\u{feff}',                                   // zero-width no-break space / BOM
            '\u{200e}',                                   // left-to-right mark
            '\u{202a}',                                   // left-to-right embedding
            '\u{202e}',                                   // right-to-left override
            '\u{2066}',                                   // directional isolate
            '\u{2028}',                                   // line separator
            '\u{2029}',                                   // paragraph separator
            '\u{e000}',                                   // private use
            '\u{1b}',                                     // ESC
            '\u{7f}',                                     // DEL
            '\u{85}',                                     // C1 NEL
            '\u{9c}',                                     // C1 ST
        ] {
            assert!(
                !is_allowed_title_char(dropped),
                "U+{:04X} must be rejected",
                dropped as u32
            );
        }
        // A decomposed accent loses the combining mark but keeps the letter.
        assert!(!is_allowed_title_char('\u{301}'));
    }

    #[test]
    fn sanitize_strips_emoji_and_invisible_characters_from_titles() {
        assert_eq!(sanitize("fix \u{1f980} the bug"), "fix the bug");
        assert_eq!(sanitize("ship \u{200d}\u{fe0f}it"), "ship it");
        assert_eq!(sanitize("caf\u{e9} \u{301}run"), "caf\u{e9} run");
        // The braille spinner survives: the busy prefix still renders.
        for frame in SPINNER_FRAMES {
            assert_eq!(sanitize(frame), frame);
        }
        assert_eq!(sanitize("one\u{200b}two"), "onetwo");
        // An emoji-only word disappears without leaving a gap behind.
        assert_eq!(sanitize("fix \u{1f980} the bug"), "fix the bug");
        // A title made only of dropped characters falls back cleanly.
        assert_eq!(fallback_title("\u{1f980}\u{1f980}\u{fe0f}"), FALLBACK_LABEL);
    }

    #[test]
    fn abugida_scripts_are_dropped_whole_rather_than_mangled() {
        // Devanagari virama (U+094D) and vowel sign I (U+093F) are not in the
        // Latin/Greek/Cyrillic drop-list, but a title must never render a bare
        // consonant skeleton: the whole word goes instead.
        assert!(!is_allowed_title_char('\u{94d}'));
        assert!(!is_allowed_title_char('\u{93f}'));
        for dropped in ['स', 'त', 'य', 'अ', 'ก', 'ไ', 'ཀ', 'မ', 'ക'] {
            assert!(!is_allowed_title_char(dropped), "U+{:04X}", dropped as u32);
        }
        assert_eq!(sanitize("सत्य bug"), "bug");
        assert_eq!(fallback_title("सत्य"), FALLBACK_LABEL);
        // A mixed-script title keeps the parts that are safe.
        assert_eq!(sanitize("fix सत्य login"), "fix login");
    }
}
