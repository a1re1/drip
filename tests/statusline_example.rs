//! Validates the executable status-line example
//! (`examples/statusline/statusline.sh`) against payloads produced by the
//! real implementation, and cross-checks the JSON keys documented in
//! README.md ("Custom status line"). Unix-only: the example is a POSIX script.

#![cfg(unix)]

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use drip::tui::status_line::{StatusLinePayload, StatusLineRequest};

fn representative_request() -> StatusLineRequest {
    StatusLineRequest {
        session_id: Some("sess-example-1".to_string()),
        cwd: Some("/tmp/drip-demo".to_string()),
        model_id: Some("example-model".to_string()),
        model_display_name: Some("Opus 5 (1M context)".to_string()),
        version: Some("2.1.261".to_string()),
        render_width_chars: 120,
        context_usage: Some(0.35),
    }
}

/// Spawns the real example script with `payload` on stdin and returns its
/// stdout as a single line (trailing newline stripped). Every caller gets
/// the shared assertions for free: successful exit and single-line output.
fn run_script(payload: &str) -> String {
    let script = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("examples")
        .join("statusline")
        .join("statusline.sh");
    let mut child = Command::new("sh")
        .arg(script)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn example script");
    child
        .stdin
        .take()
        .expect("script stdin")
        .write_all(payload.as_bytes())
        .expect("write payload");
    let out = child.wait_with_output().expect("run example script");
    assert!(out.status.success(), "example script failed: {:?}", out.status);

    let line = String::from_utf8_lossy(&out.stdout);
    let line = line.trim_end_matches('\n');
    assert!(!line.contains('\n'), "expected a single line, got {line:?}");
    line.to_string()
}

#[test]
fn documented_payload_keys_match_implementation() {
    let json = StatusLinePayload::from_request(&representative_request()).to_json();
    let documented_keys = [
        r#""session_id":"sess-example-1""#,
        r#""workspace":{"current_dir":"/tmp/drip-demo""#,
        r#""model":{"id":"example-model""#,
        r#""display_name":"Opus 5 (1M context)""#,
        r#""version":"2.1.261""#,
        r#""render_width_chars":120"#,
        r#""context_usage":0.35"#,
    ];
    for key in documented_keys {
        assert!(json.contains(key), "payload missing documented key {key}: {json}");
    }
}

#[test]
fn example_script_renders_representative_payload() {
    let payload = StatusLinePayload::from_request(&representative_request()).to_json();
    let line = run_script(&payload);

    assert!(!line.contains("\x1b]"), "OSC sequences must be absent: {line:?}");
    assert!(line.contains("Opus 5 (1M context)"), "missing model name: {line:?}");
    assert!(line.contains("drip-demo"), "missing cwd basename: {line:?}");
    assert!(line.contains("\x1b[36m"), "SGR color was dropped: {line:?}");
    assert!(line.ends_with("\x1b[0m"), "missing trailing SGR reset: {line:?}");
}

/// A payload with no recognizable fields (the script's documented graceful
/// degradation) must render the plain unstyled `drip` literal.
#[test]
fn empty_payload_falls_back_to_literal_drip() {
    let line = run_script("{}");
    assert_eq!(line, "drip");
    assert!(!line.contains('\x1b'), "plain fallback must be unstyled: {line:?}");
}

/// The script's model-only branch: a styled model name with no separator
/// and no cwd basename. Real production shape — `from_request` emits the
/// workspace object with no fields when cwd is unavailable.
#[test]
fn model_only_payload_renders_styled_model_without_cwd() {
    let request = StatusLineRequest {
        session_id: Some("sess-example-2".to_string()),
        cwd: None,
        model_id: Some("example-model".to_string()),
        model_display_name: Some("Opus 5 (1M context)".to_string()),
        version: None,
        render_width_chars: 120,
        context_usage: None,
    };
    let payload = StatusLinePayload::from_request(&request).to_json();
    let line = run_script(&payload);

    assert!(line.contains("Opus 5 (1M context)"), "missing model name: {line:?}");
    assert!(
        !line.contains('·'),
        "cwd-less row must omit the separator: {line:?}"
    );
    assert!(line.contains("\x1b[36m"), "SGR color was dropped: {line:?}");
    assert!(line.ends_with("\x1b[0m"), "missing trailing SGR reset: {line:?}");
}

/// The script styles a row only when BOTH model and cwd are present; a
/// workspace-only payload therefore degrades to the plain `drip` literal
/// (there is no dir-only styled branch).
#[test]
fn workspace_only_payload_falls_back_to_literal_drip() {
    let request = StatusLineRequest {
        session_id: None,
        cwd: Some("/tmp/drip-demo".to_string()),
        model_id: None,
        model_display_name: None,
        version: None,
        render_width_chars: 120,
        context_usage: None,
    };
    let payload = StatusLinePayload::from_request(&request).to_json();
    let line = run_script(&payload);

    assert_eq!(line, "drip");
    assert!(!line.contains('\x1b'), "plain fallback must be unstyled: {line:?}");
}
