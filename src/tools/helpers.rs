// The TS helpers are fs/promises-based; the Rust port uses std::fs (blocking),
// which is equivalent for a single-threaded-per-call tool execution model.

use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};
use std::sync::OnceLock;

use anyhow::{anyhow, Result};

const BINARY_PROBE_SIZE: usize = 1024; // 1KB — mirror of grep-tool.ts isBinaryBuffer

/// Returns true if the buffer contains a NUL byte in the first 1 KB,
/// indicating a binary file. Mirrors the same check in tools/grep-tool.ts.
pub fn is_binary_buffer(buffer: &[u8]) -> bool {
    let probe_len = buffer.len().min(BINARY_PROBE_SIZE);
    buffer[..probe_len].contains(&0)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolPathKind {
    Directory,
    File,
    Missing,
    Other,
}

pub const DEFAULT_IGNORED_DIRS: &[&str] = &[
    // Directory names every workspace-walking tool skips (debt audit C5: three
    // copies had already diverged). The retired harness dir names
    // (.solid-state, .local-coding-app) stay ignored indefinitely — repos
    // touched by old versions may still carry them, and walking that garbage
    // helps nothing. Rename rule: the harness dir is .drip here.
    ".git",
    ".drip",
    ".local-coding-app",
    ".solid-state",
    "dist",
    "node_modules",
];

pub fn default_ignored_dirs() -> &'static HashSet<&'static str> {
    static IGNORED: OnceLock<HashSet<&'static str>> = OnceLock::new();
    IGNORED.get_or_init(|| DEFAULT_IGNORED_DIRS.iter().copied().collect())
}

const CWD_ALIASES: &[&str] = &[
    ".",
    "./",
    "cwd",
    "current directory",
    "current working directory",
    "project root",
    "workspace",
];

/// Port of parseToolArguments: parses a JSON object out of the raw model
/// string, rejecting non-objects with the model-facing error text.
pub fn parse_tool_arguments(raw_input: &str) -> Result<serde_json::Map<String, serde_json::Value>> {
    let trimmed_input = raw_input.trim();

    if trimmed_input.is_empty() {
        return Ok(serde_json::Map::new());
    }

    let parsed_value: serde_json::Value = serde_json::from_str(trimmed_input)
        .map_err(|_| anyhow!("Tool arguments must be valid JSON."))?;

    match parsed_value {
        serde_json::Value::Object(map) => Ok(map),
        _ => Err(anyhow!("Tool arguments must be a JSON object.")),
    }
}

/// Port of getRequiredStringArgument.
pub fn get_required_string_argument(
    args: &serde_json::Map<String, serde_json::Value>,
    key: &str,
) -> Result<String> {
    let value = args.get(key);

    let text = match value {
        Some(serde_json::Value::String(s)) if !s.trim().is_empty() => s.trim().to_string(),
        _ => {
            return Err(anyhow!("Missing required string argument \"{}\".", key));
        }
    };

    Ok(text)
}

/// Port of getOptionalNumberArgument.
pub fn get_optional_number_argument(
    args: &serde_json::Map<String, serde_json::Value>,
    key: &str,
) -> Result<Option<f64>> {
    let value = args.get(key);

    match value {
        // Models frequently send explicit nulls for optional params; null means
        // "absent", not "invalid".
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(serde_json::Value::Number(n)) => match n.as_f64() {
            Some(f) => Ok(Some(f)),
            None => Err(anyhow!("Expected \"{}\" to be a number.", key)),
        },
        Some(serde_json::Value::String(s)) if !s.trim().is_empty() => match js_number(s.trim()) {
            Some(parsed) => Ok(Some(parsed)),
            None => Err(anyhow!("Expected \"{}\" to be a number.", key)),
        },
        _ => Err(anyhow!("Expected \"{}\" to be a number.", key)),
    }
}

/// `Number(text)` for an already-trimmed, non-empty string: the decimal
/// literal forms, `Infinity` with an optional sign, and the unsigned `0x`/`0o`/
/// `0b` prefixes. Rust's `f64::from_str` differs on both sides — it takes
/// `nan`/`inf`/`infinity` (JS: NaN) and rejects the hex/octal/binary forms.
pub fn js_number(text: &str) -> Option<f64> {
    static DECIMAL: OnceLock<regex::Regex> = OnceLock::new();
    let decimal = DECIMAL.get_or_init(|| regex::Regex::new(r"^[+-]?(?:\d+\.?\d*|\.\d+)(?:[eE][+-]?\d+)?$").unwrap());

    match text {
        "Infinity" | "+Infinity" => return Some(f64::INFINITY),
        "-Infinity" => return Some(f64::NEG_INFINITY),
        _ => {}
    }

    let radix = match text.get(..2) {
        Some("0x") | Some("0X") => Some(16),
        Some("0o") | Some("0O") => Some(8),
        Some("0b") | Some("0B") => Some(2),
        _ => None,
    };

    if let Some(radix) = radix {
        // Accumulate in f64 so a literal past u64::MAX stays a large finite
        // float, as Number() gives, instead of an overflow error.
        let digits = &text[2..];

        if digits.is_empty() {
            return None;
        }

        return digits
            .chars()
            .try_fold(0.0_f64, |acc, ch| ch.to_digit(radix).map(|digit| acc * radix as f64 + digit as f64));
    }

    if decimal.is_match(text) {
        text.parse::<f64>().ok()
    } else {
        None
    }
}

/// Port of resolveToolPath: alias strings resolve to the cwd itself; relative
/// paths resolve against the cwd; absolute paths pass through unchanged.
pub fn resolve_tool_path(cwd: &str, path_value: &str) -> PathBuf {
    let normalized_path = path_value.trim();

    if CWD_ALIASES.iter().any(|alias| alias.eq_ignore_ascii_case(normalized_path)) {
        return PathBuf::from(cwd);
    }

    let candidate = Path::new(normalized_path);
    if candidate.is_absolute() {
        // node: isAbsolute(p) ? p : resolve(cwd, p) — absolute input passes
        // through unnormalized; only the relative branch goes through
        // path.resolve's lexical normalization.
        candidate.to_path_buf()
    } else {
        absolute_normalize(&Path::new(cwd).join(candidate))
    }
}

/// Port of path.relative for the cases the tool paths hit: both paths on the
/// same root; produces `../`-prefixed components when the target escapes cwd.
/// Returns an absolute path string when a relative form would be meaningless
/// (diverging roots on Windows).
fn path_relative(from: &Path, to: &Path) -> PathBuf {
    let from_abs = absolute_normalize(from);
    let to_abs = absolute_normalize(to);

    let from_components: Vec<_> = components_after_root(&from_abs);
    let to_components: Vec<_> = components_after_root(&to_abs);

    let mut common = 0;
    while common < from_components.len()
        && common < to_components.len()
        && from_components[common] == to_components[common]
    {
        common += 1;
    }

    let mut result = PathBuf::new();
    for _ in common..from_components.len() {
        result.push("..");
    }
    for component in &to_components[common..] {
        result.push(component);
    }

    result
}

fn components_after_root(path: &Path) -> Vec<String> {
    path.components()
        .skip(1) // drop RootDir / Prefix
        .filter(|c| !matches!(c, Component::CurDir))
        .map(|c| c.as_os_str().to_string_lossy().to_string())
        .collect()
}

/// Lexically normalize a path against the process cwd (mirror of node
/// path.resolve's normalization: collapses . and .. without touching the fs).
fn absolute_normalize(path: &Path) -> PathBuf {
    let base = if path.is_absolute() {
        PathBuf::new()
    } else {
        std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
    };

    let mut result = base;
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                result.pop();
            }
            other => result.push(other.as_os_str()),
        }
    }
    result
}

/// Port of formatToolPath: display `target_path` relative to `cwd` when it
/// lives underneath, `.` when it is the cwd itself, and the absolute path
/// when it escapes upward.
pub fn format_tool_path(cwd: &str, target_path: &Path) -> String {
    let relative_path = path_relative(Path::new(cwd), target_path);
    let relative_str = relative_path.to_string_lossy().to_string();

    if relative_str.is_empty() {
        return ".".to_string();
    }

    if relative_str.starts_with("..") {
        target_path.to_string_lossy().to_string()
    } else {
        relative_str
    }
}

/// Port of countLines: an empty string still counts as one (trailing) line.
pub fn count_lines(text: &str) -> usize {
    if text.is_empty() {
        1
    } else {
        text.split('\n').count()
    }
}

/// Port of getToolPathKind (the TS version is async over fs/promises; std::fs
/// is used here — same syscall surface).
pub fn get_tool_path_kind(target_path: &Path) -> Result<ToolPathKind> {
    match std::fs::metadata(target_path) {
        Ok(metadata) => {
            if metadata.is_dir() {
                Ok(ToolPathKind::Directory)
            } else if metadata.is_file() {
                Ok(ToolPathKind::File)
            } else {
                Ok(ToolPathKind::Other)
            }
        }
        Err(error) => {
            if error.kind() == std::io::ErrorKind::NotFound {
                Ok(ToolPathKind::Missing)
            } else {
                Err(anyhow::Error::from(error))
            }
        }
    }
}

/// Port of assertReadableFilePath.
pub fn assert_readable_file_path(target_path: &Path, display_path: &str) -> Result<()> {
    match get_tool_path_kind(target_path)? {
        ToolPathKind::File => Ok(()),
        ToolPathKind::Directory => Err(anyhow!(
            "\"{}\" is a directory. Use DIR for directories or choose a file path for READ.",
            display_path
        )),
        ToolPathKind::Missing => Err(anyhow!("\"{}\" does not exist.", display_path)),
        ToolPathKind::Other => Err(anyhow!("\"{}\" is not a readable file.", display_path)),
    }
}

/// Port of assertPatchTargetPath.
pub fn assert_patch_target_path(target_path: &Path, display_path: &str) -> Result<ToolPathKind> {
    match get_tool_path_kind(target_path)? {
        kind @ (ToolPathKind::File | ToolPathKind::Missing) => Ok(kind),
        ToolPathKind::Directory => Err(anyhow!(
            "\"{}\" is a directory. PATCH expects a file path.",
            display_path
        )),
        ToolPathKind::Other => Err(anyhow!(
            "\"{}\" is not a patchable file target.",
            display_path
        )),
    }
}

/// Port of assertDirectoryPath.
pub fn assert_directory_path(target_path: &Path, display_path: &str) -> Result<()> {
    match get_tool_path_kind(target_path)? {
        ToolPathKind::Directory => Ok(()),
        ToolPathKind::File => Err(anyhow!(
            "\"{}\" is a file. Use READ for files or point DIR at a directory.",
            display_path
        )),
        ToolPathKind::Missing => Err(anyhow!("\"{}\" does not exist.", display_path)),
        ToolPathKind::Other => Err(anyhow!("\"{}\" is not a directory.", display_path)),
    }
}

// --- localeCompare -----------------------------------------------------------
//
// JS `String.prototype.localeCompare` under Bun is ICU root collation. The TS
// tools sort directory listings, grep walks, @-mention suggestions and
// canonical JSON keys with it, and a model sees that order (`DIR` puts
// `hello.txt` before `NOTES.md`; byte order would not). This is the subset of
// the root collation the workspace names actually exercise: three levels —
// primary (whitespace < ICU punctuation order < digits < letters, case and
// accents ignored), secondary (accents), tertiary (lowercase first) — then
// byte order for anything still tied.

// ICU root order of the ASCII "variable" characters, primary level.
const ICU_ASCII_SYMBOL_ORDER: &str = "\t\n _-,;:!?.'\"()[]{}@*/\\&#%`^+<=>|~$";

/// (primary weight, has accent, is upper) per collation element; a char may
/// expand to two elements (æ → a e, ß → s s).
fn collation_elements(ch: char, out: &mut Vec<(u32, bool, bool)>) {
    let letter = |base: char, accent: bool, upper: bool| (200 + (base as u32 - 'a' as u32), accent, upper);
    match ch {
        'a'..='z' => out.push(letter(ch, false, false)),
        'A'..='Z' => out.push(letter(ch.to_ascii_lowercase(), false, true)),
        '0'..='9' => out.push((100 + (ch as u32 - '0' as u32), false, false)),
        _ if ch.is_ascii() => match ICU_ASCII_SYMBOL_ORDER.find(ch) {
            Some(index) => out.push((index as u32, false, false)),
            None => out.push((1000 + ch as u32, false, false)),
        },
        'æ' => {
            out.push(letter('a', true, false));
            out.push(letter('e', false, false));
        }
        'Æ' => {
            out.push(letter('a', true, true));
            out.push(letter('e', false, true));
        }
        'ß' => {
            out.push(letter('s', true, false));
            out.push(letter('s', false, false));
        }
        _ => {
            let upper = ch.is_uppercase();
            let base = match ch.to_lowercase().next().unwrap_or(ch) {
                'à' | 'á' | 'â' | 'ã' | 'ä' | 'å' => Some('a'),
                'ç' => Some('c'),
                'è' | 'é' | 'ê' | 'ë' => Some('e'),
                'ì' | 'í' | 'î' | 'ï' => Some('i'),
                'ñ' => Some('n'),
                'ò' | 'ó' | 'ô' | 'õ' | 'ö' | 'ø' => Some('o'),
                'ù' | 'ú' | 'û' | 'ü' => Some('u'),
                'ý' | 'ÿ' => Some('y'),
                _ => None,
            };
            match base {
                Some(base) => out.push(letter(base, true, upper)),
                None => out.push((100_000 + ch as u32, false, upper)),
            }
        }
    }
}

/// Port of `left.localeCompare(right)` (ICU root collation, see above).
pub fn locale_compare(left: &str, right: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    if left == right {
        return Ordering::Equal;
    }
    let mut l = Vec::with_capacity(left.len());
    let mut r = Vec::with_capacity(right.len());
    left.chars().for_each(|ch| collation_elements(ch, &mut l));
    right.chars().for_each(|ch| collation_elements(ch, &mut r));
    let primary = l.iter().map(|e| e.0).cmp(r.iter().map(|e| e.0));
    if primary != Ordering::Equal {
        return primary;
    }
    let secondary = l.iter().map(|e| e.1).cmp(r.iter().map(|e| e.1));
    if secondary != Ordering::Equal {
        return secondary;
    }
    let tertiary = l.iter().map(|e| e.2).cmp(r.iter().map(|e| e.2));
    if tertiary != Ordering::Equal {
        return tertiary;
    }
    left.cmp(right)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // --- parseToolArguments ---

    #[test]
    fn parse_tool_arguments_empty_input_returns_empty_map() {
        assert!(parse_tool_arguments("").unwrap().is_empty());
        assert!(parse_tool_arguments("   \n  ").unwrap().is_empty());
    }

    #[test]
    fn parse_tool_arguments_parses_object() {
        let args = parse_tool_arguments(r#"{"path": "a.rs", "offset": 3}"#).unwrap();
        assert_eq!(args.get("path").unwrap().as_str().unwrap(), "a.rs");
        assert_eq!(args.get("offset").unwrap().as_f64().unwrap(), 3.0);
    }

    #[test]
    fn parse_tool_arguments_invalid_json_model_facing_error() {
        let err = parse_tool_arguments("{not json}").unwrap_err();
        assert_eq!(err.to_string(), "Tool arguments must be valid JSON.");
    }

    #[test]
    fn parse_tool_arguments_non_object_rejected() {
        for raw in ["[1,2,3]", "\"text\"", "42", "null", "true"] {
            let err = parse_tool_arguments(raw).unwrap_err();
            assert_eq!(err.to_string(), "Tool arguments must be a JSON object.", "raw={raw}");
        }
    }

    // --- getRequiredStringArgument ---

    #[test]
    fn get_required_string_argument_returns_trimmed_value() {
        let args = json!({"path": "  src/main.rs  "});
        let map = args.as_object().unwrap();
        assert_eq!(
            get_required_string_argument(map, "path").unwrap(),
            "src/main.rs"
        );
    }

    #[test]
    fn get_required_string_argument_missing_blank_or_wrong_type() {
        let empty = serde_json::Map::new();
        assert_eq!(
            get_required_string_argument(&empty, "path").unwrap_err().to_string(),
            "Missing required string argument \"path\"."
        );

        let blank = json!({"path": "   "}).as_object().unwrap().clone();
        assert_eq!(
            get_required_string_argument(&blank, "path").unwrap_err().to_string(),
            "Missing required string argument \"path\"."
        );

        let numeric = json!({"path": 7}).as_object().unwrap().clone();
        assert_eq!(
            get_required_string_argument(&numeric, "path").unwrap_err().to_string(),
            "Missing required string argument \"path\"."
        );
    }

    // --- getOptionalNumberArgument ---

    #[test]
    fn get_optional_number_argument_accepts_number_and_numeric_string() {
        let args = json!({"a": 5, "b": "12", "c": " 3.5 ", "d": null}).as_object().unwrap().clone();
        assert_eq!(get_optional_number_argument(&args, "a").unwrap(), Some(5.0));
        assert_eq!(get_optional_number_argument(&args, "b").unwrap(), Some(12.0));
        assert_eq!(get_optional_number_argument(&args, "c").unwrap(), Some(3.5));
        // null means "absent", not "invalid".
        assert_eq!(get_optional_number_argument(&args, "d").unwrap(), None);
        assert_eq!(get_optional_number_argument(&args, "missing").unwrap(), None);
    }

    #[test]
    fn js_number_matches_the_js_number_constructor() {
        assert_eq!(js_number("12"), Some(12.0));
        assert_eq!(js_number("+1.5e2"), Some(150.0));
        assert_eq!(js_number(".5"), Some(0.5));
        assert_eq!(js_number("0x10"), Some(16.0));
        assert_eq!(js_number("0b101"), Some(5.0));
        assert_eq!(js_number("0o17"), Some(15.0));
        assert_eq!(js_number("Infinity"), Some(f64::INFINITY));
        assert_eq!(js_number("-Infinity"), Some(f64::NEG_INFINITY));
        // Rust's parser would take these; Number() gives NaN.
        assert_eq!(js_number("nan"), None);
        assert_eq!(js_number("inf"), None);
        assert_eq!(js_number("infinity"), None);
        assert_eq!(js_number("1_000"), None);
        assert_eq!(js_number("-0x10"), None);
        assert_eq!(js_number("0x"), None);
        assert_eq!(js_number("0xFFFFFFFFFFFFFFFFFF"), Some(4722366482869645213695.0));
        assert_eq!(js_number("12abc"), None);
    }

    #[test]
    fn get_optional_number_argument_rejects_non_numeric() {
        let args = json!({"a": "abc", "b": true}).as_object().unwrap().clone();
        assert_eq!(
            get_optional_number_argument(&args, "a").unwrap_err().to_string(),
            "Expected \"a\" to be a number."
        );
        assert_eq!(
            get_optional_number_argument(&args, "b").unwrap_err().to_string(),
            "Expected \"b\" to be a number."
        );
    }

    // --- resolveToolPath ---

    #[test]
    fn resolve_tool_path_aliases_and_relative_paths() {
        let cwd = "/repo/drip";
        assert_eq!(resolve_tool_path(cwd, "cwd"), PathBuf::from(cwd));
        assert_eq!(resolve_tool_path(cwd, "Workspace"), PathBuf::from(cwd));
        assert_eq!(resolve_tool_path(cwd, "  ./  "), PathBuf::from(cwd));
        assert_eq!(
            resolve_tool_path(cwd, "src/tools.rs"),
            PathBuf::from("/repo/drip/src/tools.rs")
        );
        assert_eq!(
            resolve_tool_path(cwd, "/abs/path.txt"),
            PathBuf::from("/abs/path.txt")
        );
    }

    // --- formatToolPath ---

    #[test]
    fn format_tool_path_relative_inside_escaping_and_self() {
        assert_eq!(
            format_tool_path("/repo", Path::new("/repo/src/main.rs")),
            "src/main.rs"
        );
        assert_eq!(format_tool_path("/repo", Path::new("/repo")), ".");
        assert_eq!(
            format_tool_path("/repo/drip", Path::new("/elsewhere/x.txt")),
            "/elsewhere/x.txt"
        );
    }

    // --- countLines ---

    #[test]
    fn count_lines_empty_string_counts_one() {
        assert_eq!(count_lines(""), 1);
        assert_eq!(count_lines("a"), 1);
        assert_eq!(count_lines("a\nb"), 2);
        assert_eq!(count_lines("a\nb\n"), 3);
        assert_eq!(count_lines("\n\n"), 3);
    }

    // --- isBinaryBuffer ---

    #[test]
    fn is_binary_buffer_probes_first_kb_for_nul() {
        assert!(!is_binary_buffer(b"plain text"));
        assert!(is_binary_buffer(b"text\0with nul"));
        // NUL beyond the 1KB probe window is ignored.
        let mut big = vec![b'a'; 1024];
        big.push(0);
        assert!(!is_binary_buffer(&big));
        let mut early = vec![b'a'; 100];
        early.push(0);
        assert!(is_binary_buffer(&early));
    }

    // --- getToolPathKind + assert helpers (temp-dir based) ---

    #[test]
    fn get_tool_path_kind_classifies_file_dir_missing() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("file.txt");
        std::fs::write(&file_path, "hello").unwrap();
        let missing = dir.path().join("nope.txt");

        assert_eq!(get_tool_path_kind(&file_path).unwrap(), ToolPathKind::File);
        assert_eq!(get_tool_path_kind(dir.path()).unwrap(), ToolPathKind::Directory);
        assert_eq!(get_tool_path_kind(&missing).unwrap(), ToolPathKind::Missing);
    }

    #[test]
    fn assert_readable_file_path_error_texts() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("file.txt");
        std::fs::write(&file_path, "hello").unwrap();
        let missing = dir.path().join("nope.txt");

        assert!(assert_readable_file_path(&file_path, "file.txt").is_ok());

        let err = assert_readable_file_path(dir.path(), "some-dir").unwrap_err();
        assert_eq!(
            err.to_string(),
            "\"some-dir\" is a directory. Use DIR for directories or choose a file path for READ."
        );

        let err = assert_readable_file_path(&missing, "nope.txt").unwrap_err();
        assert_eq!(err.to_string(), "\"nope.txt\" does not exist.");
    }

    #[test]
    fn assert_patch_target_path_error_texts() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("file.txt");
        std::fs::write(&file_path, "hello").unwrap();
        let missing = dir.path().join("nope.txt");

        assert_eq!(
            assert_patch_target_path(&file_path, "file.txt").unwrap(),
            ToolPathKind::File
        );
        assert_eq!(
            assert_patch_target_path(&missing, "nope.txt").unwrap(),
            ToolPathKind::Missing
        );

        let err = assert_patch_target_path(dir.path(), "some-dir").unwrap_err();
        assert_eq!(
            err.to_string(),
            "\"some-dir\" is a directory. PATCH expects a file path."
        );
    }

    #[test]
    fn assert_directory_path_error_texts() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("file.txt");
        std::fs::write(&file_path, "hello").unwrap();
        let missing = dir.path().join("nope.txt");

        assert!(assert_directory_path(dir.path(), "some-dir").is_ok());

        let err = assert_directory_path(&file_path, "file.txt").unwrap_err();
        assert_eq!(
            err.to_string(),
            "\"file.txt\" is a file. Use READ for files or point DIR at a directory."
        );

        let err = assert_directory_path(&missing, "nope.txt").unwrap_err();
        assert_eq!(err.to_string(), "\"nope.txt\" does not exist.");
    }

    // --- DEFAULT_IGNORED_DIRS ---

    #[test]
    fn default_ignored_dirs_matches_ts_set() {
        assert_eq!(
            default_ignored_dirs(),
            &HashSet::from([".git", ".drip", ".local-coding-app", ".solid-state", "dist", "node_modules"])
        );
    }

    // The fixture is `[...names].sort((a, b) => a.localeCompare(b))` under Bun
    // (ICU root); every adjacent pair must compare the same way here.
    #[test]
    fn locale_compare_matches_bun_icu_root_order() {
        let expected: Vec<&str> = vec![
            "_x",
            "-a",
            ".drip",
            ".git",
            "[a",
            "@a",
            "~a",
            "$x",
            "10",
            "1a",
            "9",
            "a",
            "A",
            "ä",
            "a b",
            "a_b",
            "a-b",
            "a,",
            "a;",
            "a:",
            "a!",
            "a?",
            "a.b",
            "a'",
            "a\"",
            "a(",
            "a)",
            "a[",
            "a]",
            "a{",
            "a}",
            "a@",
            "a*",
            "a/",
            "a\\",
            "a&",
            "a#",
            "a%",
            "a`",
            "a^",
            "a+",
            "a<",
            "a=",
            "a>",
            "a|",
            "a~",
            "a$",
            "a1",
            "a10",
            "a2",
            "aa",
            "Aa",
            "ab",
            "aB",
            "Ab",
            "ae",
            "æ",
            "b",
            "B",
            "c",
            "ç",
            "Cargo.toml",
            "e",
            "é",
            "f",
            "hello.txt",
            "index.ts",
            "Index.ts",
            "lib.rs",
            "Lib.rs",
            "license",
            "LICENSE",
            "main.rs",
            "Makefile",
            "mod.rs",
            "MOD.rs",
            "n",
            "ñ",
            "NOTES.md",
            "o",
            "ø",
            "package-lock.json",
            "package.json",
            "readme",
            "Readme",
            "READme",
            "README",
            "README.md",
            "src",
            "Src",
            "ss",
            "ß",
            "test",
            "test_utils.ts",
            "test-utils.ts",
            "test.ts",
            "tests",
            "x_",
            "x.rs",
            "x.RS",
            "X.rs",
            "x$",
            "z",
            "Z"
        ];
        let mut shuffled: Vec<&str> = expected.iter().rev().copied().collect();
        shuffled.sort_by(|a, b| locale_compare(a, b));
        assert_eq!(shuffled, expected);
        for pair in expected.windows(2) {
            assert_eq!(locale_compare(pair[0], pair[1]), std::cmp::Ordering::Less, "{:?} < {:?}", pair[0], pair[1]);
            assert_eq!(locale_compare(pair[1], pair[0]), std::cmp::Ordering::Greater);
        }
        assert_eq!(locale_compare("same", "same"), std::cmp::Ordering::Equal);
    }
}
