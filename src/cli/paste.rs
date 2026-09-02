// port of src/cli/paste.ts
//
// Terminals wrap pastes in bracketed-paste markers when mode 2004 is on. The
// markers may arrive with or without their ESC depending on how the terminal
// chunks the paste, so both spellings are stripped.

pub const ENABLE_BRACKETED_PASTE: &str = "\u{1b}[?2004h";
pub const DISABLE_BRACKETED_PASTE: &str = "\u{1b}[?2004l";

fn paste_markers() -> &'static regex::Regex {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| regex::Regex::new("\u{1b}?\\[20[01]~").expect("valid regex"))
}

fn csi_sequences() -> &'static regex::Regex {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| regex::Regex::new("\u{1b}\\[[0-9;?]*[@-~]").expect("valid regex"))
}

/// Pasted text may carry bracketed-paste markers, stray escape sequences, and
/// control bytes — none of which belong in composer text. Losing them silently
/// beats inserting them: a credential pasted into /env must come out byte-equal
/// or visibly broken, never plausibly wrong. Tab, newline, and carriage return
/// survive (the composer's own normalization handles them).
pub fn sanitize_pasted_input(raw: &str) -> String {
    let without_markers = paste_markers().replace_all(raw, "");
    let without_csi = csi_sequences().replace_all(&without_markers, "");
    without_csi
        .chars()
        .filter(|c| {
            let code = *c as u32;
            code != 0x1b
                && !(code <= 0x08 || code == 0x0b || code == 0x0c || (0x0e..=0x1f).contains(&code) || code == 0x7f)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_markers_escapes_and_control_bytes() {
        assert_eq!(sanitize_pasted_input("\u{1b}[200~hello\u{1b}[201~"), "hello");
        assert_eq!(sanitize_pasted_input("[200~tok\u{1b}[31men[201~"), "token");
        assert_eq!(sanitize_pasted_input("a\u{0}b\u{7f}c\td\ne\r"), "abc\td\ne\r");
        assert_eq!(sanitize_pasted_input("plain text"), "plain text");
    }
}
