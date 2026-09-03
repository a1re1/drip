// Markdown → ANSI for line-oriented surfaces (the TUI timeline, dripw's
// transcript pane). The TS renders through marked + marked-terminal; drip
// carries a small renderer of its own covering what transcripts actually
// contain — headings, emphasis, inline code, fenced code, lists, quotes,
// rules — so no markdown engine is pulled into the process. Known deviation:
// the exact bytes differ from marked-terminal's; the structure and the SGR
// styling (bold headings, yellow inline code, dim fences) match in spirit.
//
// reflow stays off — callers wrap to their own width — and the output is
// always coloured: callers strip the escapes when colour is disabled, the
// way markdownRows in the watch view does.

const RESET: &str = "\x1b[0m";
const BOLD: &str = "\x1b[1m";
const DIM: &str = "\x1b[2m";
const ITALIC: &str = "\x1b[3m";
const UNDERLINE: &str = "\x1b[4m";
const YELLOW: &str = "\x1b[33m";
const GREEN: &str = "\x1b[32m";

/// Inline emphasis: `**bold**`, `*italic*` / `_italic_`, `` `code` ``.
fn render_inline(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::new();
    let mut i = 0;

    while i < chars.len() {
        let rest = &chars[i..];

        if rest[0] == '`' {
            if let Some(end) = rest[1..].iter().position(|&ch| ch == '`') {
                let code: String = rest[1..=end].iter().collect();

                out.push_str(YELLOW);
                out.push_str(&code);
                out.push_str(RESET);
                i += end + 2;
                continue;
            }
        }

        if rest.len() >= 2 && rest[0] == '*' && rest[1] == '*' {
            if let Some(end) = find_closing(&rest[2..], "**") {
                let inner: String = rest[2..2 + end].iter().collect();

                out.push_str(BOLD);
                out.push_str(&render_inline(&inner));
                out.push_str(RESET);
                i += end + 4;
                continue;
            }
        }

        if (rest[0] == '*' || rest[0] == '_') && rest.len() >= 3 {
            let marker = rest[0];
            let boundary_ok = rest[1] != ' ' && (i == 0 || !chars[i - 1].is_alphanumeric() || marker == '*');

            if boundary_ok {
                if let Some(end) = rest[1..].iter().position(|&ch| ch == marker) {
                    if end > 0 && rest[end] != ' ' {
                        let inner: String = rest[1..=end].iter().collect();

                        out.push_str(ITALIC);
                        out.push_str(&render_inline(&inner));
                        out.push_str(RESET);
                        i += end + 2;
                        continue;
                    }
                }
            }
        }

        out.push(rest[0]);
        i += 1;
    }

    out
}

fn find_closing(chars: &[char], marker: &str) -> Option<usize> {
    let marker: Vec<char> = marker.chars().collect();

    (0..chars.len().saturating_sub(marker.len() - 1)).find(|&at| chars[at..at + marker.len()] == marker[..])
}

pub fn render_markdown_ansi(source: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut in_fence = false;

    for raw in source.lines() {
        let line = raw.trim_end();

        if line.trim_start().starts_with("```") {
            in_fence = !in_fence;
            continue;
        }

        if in_fence {
            out.push(format!("{DIM}  {line}{RESET}"));
            continue;
        }

        let trimmed = line.trim_start();
        let indent = line.len() - trimmed.len();

        if trimmed.is_empty() {
            out.push(String::new());
        } else if let Some(heading) = trimmed.strip_prefix('#') {
            let level = 1 + heading.chars().take_while(|&ch| ch == '#').count();
            let text = heading.trim_start_matches('#').trim();
            let style = if level == 1 { format!("{BOLD}{UNDERLINE}{GREEN}") } else { format!("{BOLD}{GREEN}") };

            out.push(format!("{style}{}{RESET}", render_inline(text)));
        } else if trimmed == "---" || trimmed == "***" || trimmed == "___" {
            out.push(format!("{DIM}{}{RESET}", "─".repeat(40)));
        } else if let Some(quote) = trimmed.strip_prefix('>') {
            out.push(format!("{DIM}│ {}{RESET}", render_inline(quote.trim_start())));
        } else if let Some(item) = trimmed.strip_prefix("- ").or_else(|| trimmed.strip_prefix("* ")).or_else(|| trimmed.strip_prefix("+ ")) {
            out.push(format!("{}  * {}", " ".repeat(indent), render_inline(item)));
        } else if let Some((number, item)) = ordered_item(trimmed) {
            out.push(format!("{}  {number}. {}", " ".repeat(indent), render_inline(item)));
        } else {
            out.push(format!("{}{}", " ".repeat(indent), render_inline(trimmed)));
        }
    }

    out.join("\n").trim_end().to_string()
}

fn ordered_item(line: &str) -> Option<(&str, &str)> {
    let digits = line.chars().take_while(|ch| ch.is_ascii_digit()).count();

    if digits == 0 {
        return None;
    }

    let rest = &line[digits..];
    let body = rest.strip_prefix(". ").or_else(|| rest.strip_prefix(") "))?;

    Some((&line[..digits], body))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn headings_are_bold_and_body_keeps_emphasis() {
        let rendered = render_markdown_ansi("# Title\n\nSome **bold** and `code` here.\n");

        assert!(rendered.starts_with(&format!("{BOLD}{UNDERLINE}{GREEN}Title{RESET}")));
        assert!(rendered.contains(&format!("{BOLD}bold{RESET}")));
        assert!(rendered.contains(&format!("{YELLOW}code{RESET}")));
        assert!(!rendered.ends_with('\n'));
    }

    #[test]
    fn fences_lists_and_quotes_render_line_by_line() {
        let rendered = render_markdown_ansi("- one\n- **two**\n\n```\nlet x = 1;\n```\n> note\n1. first\n");
        let lines: Vec<&str> = rendered.split('\n').collect();

        assert_eq!(lines[0], "  * one");
        assert_eq!(lines[1], format!("  * {BOLD}two{RESET}"));
        assert_eq!(lines[3], format!("{DIM}  let x = 1;{RESET}"));
        assert_eq!(lines[4], format!("{DIM}│ note{RESET}"));
        assert_eq!(lines[5], "  1. first");
    }

    #[test]
    fn unbalanced_markers_pass_through() {
        assert_eq!(render_markdown_ansi("a * b ** c ` d"), "a * b ** c ` d");
        assert_eq!(render_markdown_ansi("snake_case_name stays"), "snake_case_name stays");
    }
}
