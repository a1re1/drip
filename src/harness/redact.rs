// Secret redaction at the tool-output choke point. Tool output flows into
// four persistent/streamed places (model context, state telemetry, transcript
// events, the NDJSON stream); one pass here keeps credentials out of all of
// them. Two layers: exact-value scrubbing of harness-managed credentials
// (names come from env.vars, so `cat .env` can't leak what drip itself runs
// on), and a pattern pass for high-signal token shapes regardless of origin.

use std::sync::LazyLock;

use regex::Regex;

struct TokenPattern {
    label: &'static str,
    pattern: &'static str,
}

static TOKEN_PATTERNS: LazyLock<Vec<(&'static str, Regex)>> = LazyLock::new(|| {
    const SPECS: &[TokenPattern] = &[
        TokenPattern {
            label: "anthropic-key",
            pattern: r"\bsk-ant-[A-Za-z0-9_-]{10,}",
        },
        TokenPattern {
            label: "openai-key",
            pattern: r"\bsk-(?:proj-|svcacct-)?[A-Za-z0-9_-]{20,}",
        },
        TokenPattern {
            label: "github-token",
            pattern: r"\b(?:ghp|gho|ghu|ghs|ghr)_[A-Za-z0-9]{20,}",
        },
        TokenPattern {
            label: "github-pat",
            pattern: r"\bgithub_pat_[A-Za-z0-9_]{20,}",
        },
        TokenPattern {
            label: "aws-access-key",
            pattern: r"\bAKIA[0-9A-Z]{16}\b",
        },
        TokenPattern {
            label: "slack-token",
            pattern: r"\bxox[baprs]-[A-Za-z0-9-]{10,}",
        },
        TokenPattern {
            label: "google-api-key",
            pattern: r"\bAIza[A-Za-z0-9_-]{30,}",
        },
    ];

    SPECS
        .iter()
        .map(|spec| {
            (
                spec.label,
                Regex::new(spec.pattern).expect("redact: token pattern must compile"),
            )
        })
        .collect()
});

/// Values shorter than this are too collision-prone to scrub verbatim.
const MIN_SECRET_LENGTH: usize = 8;

/// Builds a redactor closure over the env.vars name→value map.
///
/// `secrets` is passed as ordered (name, value) pairs — source-file order is
/// significant for diffing. The returned closure applies the exact-value
/// pass (longest first) followed by the token-pattern pass.
pub fn build_redactor(
    secrets: Vec<(String, String)>,
) -> impl Fn(&str) -> String {
    // Longest values first so a secret that contains another (or a shared
    // prefix) never leaves a partial behind.
    let mut exact: Vec<(String, String)> = secrets
        .into_iter()
        .filter(|(_, value)| value.chars().count() >= MIN_SECRET_LENGTH)
        .collect();
    // `Array.prototype.sort` is stable in modern JS engines; Rust's
    // `sort_by_key` is stable too, so equal-length secrets keep input order.
    exact.sort_by_key(|(_, value)| std::cmp::Reverse(value.chars().count()));

    move |text: &str| {
        if text.is_empty() {
            return text.to_string();
        }

        let mut redacted = text.to_string();

        for (name, value) in &exact {
            // `redacted.split(value).join(marker)` — replace_all semantics.
            redacted = redacted.split(value).collect::<Vec<&str>>().join(&format!("[redacted:{name}]"));
        }

        for (label, pattern) in TOKEN_PATTERNS.iter() {
            redacted = pattern
                .replace_all(&redacted, &format!("[redacted:{label}]"))
                .into_owned();
        }

        redacted
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scrubs_exact_managed_values_by_name_longest_first() {
        let redact = build_redactor(vec![
            ("LONG_KEY".to_string(), "secret-value-abcdef".to_string()),
            ("SHORT".to_string(), "tiny".to_string()),
            ("SUB_KEY".to_string(), "secret-value".to_string()),
        ]);

        // expect(redact("found secret-value-abcdef and secret-value here"))
        //   .toBe("found [redacted:LONG_KEY] and [redacted:SUB_KEY] here");
        assert_eq!(
            redact("found secret-value-abcdef and secret-value here"),
            "found [redacted:LONG_KEY] and [redacted:SUB_KEY] here"
        );
        // Sub-minimum-length values never scrub (too collision-prone).
        // expect(redact("a tiny word")).toBe("a tiny word");
        assert_eq!(redact("a tiny word"), "a tiny word");
    }

    #[test]
    fn scrubs_high_signal_token_patterns_regardless_of_configuration() {
        let redact = build_redactor(vec![]);

        // The inputs are already-redacted markers; they must survive
        // being redacted again.
        assert!(
            redact("key=[redacted:anthropic-key]").contains("[redacted:anthropic-key]"),
            "anthropic-key marker must survive"
        );
        assert!(
            redact("token [redacted:github-token]").contains("[redacted:github-token]"),
            "github-token marker must survive"
        );
        assert!(
            redact("aws [redacted:aws-access-key] ok").contains("[redacted:aws-access-key]"),
            "aws-access-key marker must survive"
        );
        // expect(redact("plain output stays intact")).toBe("plain output stays intact");
        assert_eq!(redact("plain output stays intact"), "plain output stays intact");
    }
}
