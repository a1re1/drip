use anyhow::Result;
use serde_json::{json, Value};
use std::time::Duration;

use super::{tool_arguments, ToolCtx, ToolOutcome};
use crate::tools::helpers::get_required_string_argument;

const DEFAULT_MAX_BYTES: i64 = 50_000;
const MAX_MAX_BYTES: i64 = 200_000;
const FETCH_TIMEOUT_MS: u64 = 15_000;
const ELISION_MARKER: &str = "\n...[content elided — size cap reached]...\n";

// Tags to strip entirely (including content between open/close tags)
const BLOCK_TAGS: [&str; 5] = ["script", "style", "nav", "header", "footer"];

// FetchToolInput: the parsed arguments for the fetch tool.
#[derive(Debug)]
pub struct FetchToolInput {
    pub max_bytes: i64,
    pub url: String,
}

// FetchToolResult: the fetched body plus response metadata.
#[derive(Debug)]
pub struct FetchToolResult {
    pub body: String,
    pub content_type: String,
    pub final_url: String,
    pub status: u16,
}

/// What the prepare stage returns: the parsed input and its display string.
#[derive(Debug)]
pub struct FetchToolPrepared {
    pub input: FetchToolInput,
    pub display_input: String,
}

/// What the execute stage returns: the result data plus the output text.
#[derive(Debug)]
pub struct FetchToolExecution {
    pub data: FetchToolResult,
    pub output_text: String,
}

// ---------------------------------------------------------------------------
// HTML stripping
// ---------------------------------------------------------------------------

/// Removes block-level tags and their content, removes the remaining tags,
/// decodes common HTML entities, then collapses whitespace.
pub fn strip_html(html: &str) -> String {
    let mut text = html.to_string();

    for tag in BLOCK_TAGS {
        // Remove block tags and everything between them (non-greedy, case-insensitive, dotall)
        text = replace_all(
            &text,
            &regex::Regex::new(&format!(r#"(?is)<{tag}[^>]*>[\s\S]*?</{tag}>"#)).unwrap(),
            "",
        );
        // Remove self-closing or unclosed opening tags for these blocks
        text = replace_all(
            &text,
            &regex::Regex::new(&format!(r#"(?is)<{tag}[^>]*/?>"#)).unwrap(),
            "",
        );
    }

    // Remove all remaining HTML tags
    text = replace_all(&text, &regex::Regex::new(r#"<[^>]+>"#).unwrap(), "");

    // Decode common HTML entities
    text = text
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&nbsp;", " ")
        .replace("&apos;", "'");

    // Collapse whitespace runs (spaces, tabs, newlines) to single spaces/newlines
    text = replace_all(&text, &regex::Regex::new(r"[ \t]+").unwrap(), " ");
    text = replace_all(&text, &regex::Regex::new(r"\n{3,}").unwrap(), "\n\n");
    text.trim().to_string()
}

/// Regex::replace_all with a literal replacement (no `$group` expansion).
fn replace_all(text: &str, re: &regex::Regex, replacement: &str) -> String {
    re.replace_all(text, regex::NoExpand(replacement)).into_owned()
}

// ---------------------------------------------------------------------------
// Size cap (ends-kept: keep the end, elide from the front)
// ---------------------------------------------------------------------------

/// Keep the last maxBytes bytes of the text (ends-kept), prefixing the elision
/// marker when truncation happened. A cut landing inside a multi-byte sequence
/// is replaced with U+FFFD, matching lossy UTF-8 decoding.
pub fn cap_bytes(text: &str, max_bytes: usize) -> String {
    let encoded = text.as_bytes();

    if encoded.len() <= max_bytes {
        return text.to_string();
    }

    let kept = &encoded[encoded.len() - max_bytes..];
    format!("{}{}", ELISION_MARKER, String::from_utf8_lossy(kept))
}

// ---------------------------------------------------------------------------
// Content-type helpers
// ---------------------------------------------------------------------------

fn is_html_content_type(content_type: &str) -> bool {
    content_type.contains("text/html")
}

fn is_binary_content_type(content_type: &str) -> bool {
    // Consider binary: image/*, audio/*, video/*, application/* (except json, xml, javascript)
    let lower = content_type.to_lowercase();
    if lower.starts_with("image/") {
        return true;
    }
    if lower.starts_with("audio/") {
        return true;
    }
    if lower.starts_with("video/") {
        return true;
    }
    if lower.starts_with("application/") {
        // Allow text-like application types
        if lower.contains("json")
            || lower.contains("xml")
            || lower.contains("javascript")
            || lower.contains("x-www-form-urlencoded")
            || lower.contains("graphql")
        {
            return false;
        }
        return true;
    }
    false
}

// ---------------------------------------------------------------------------
// Tool definition
// ---------------------------------------------------------------------------

/// The OpenAI function definition drip sends for this tool (the
/// {type: "function", function: {...}} envelope).
pub fn definition() -> Value {
    json!({
        "type": "function",
        "function": {
            "description": "Fetch a URL via HTTP GET and return its text content. HTML is stripped of scripts/styles and collapsed. Requires DRIP_ALLOW_NET=1. Input: {url, maxBytes?}.",
            "name": "FETCH",
            "parameters": {
                "additionalProperties": false,
                "properties": {
                    "maxBytes": {
                        "description": "Maximum bytes of response body to return. Default 50000, max 200000. Content is ends-kept with an elision marker when truncated.",
                        "type": "number"
                    },
                    "url": {
                        "description": "The URL to fetch via HTTP GET. Must use http:// or https:// scheme.",
                        "type": "string"
                    }
                },
                "required": ["url"],
                "type": "object"
            }
        }
    })
}

// ---------------------------------------------------------------------------
// URL parsing for the cases prepare hits (absolute URLs with a scheme)
// ---------------------------------------------------------------------------

/// Parse the scheme from a URL for the scheme guard. Returns the protocol
/// without the trailing colon ("http", "https", "ftp", "file", "data", ...)
/// or None when the URL does not parse at all.
fn parse_url_scheme(url: &str) -> Option<String> {
    let colon = url.find(':')?;

    // WHATWG URL: scheme = ALPHA *( ALPHA / DIGIT / "+" / "-" / "." ),
    // and the scheme must start with a letter.
    let scheme = &url[..colon];
    let mut chars = scheme.chars();
    match chars.next() {
        Some(first) if first.is_ascii_alphabetic() => {}
        _ => return None,
    }
    for ch in chars {
        if !(ch.is_ascii_alphanumeric() || ch == '+' || ch == '-' || ch == '.') {
            return None;
        }
    }

    // A URL with no scheme at all (e.g. "not a url") must not parse as a
    // relative-path URL — WHATWG baseless parsing rejects those.
    if url.len() == colon + 1 {
        return None;
    }

    Some(scheme.to_lowercase())
}

/// Validate arguments and build the prepared input for the fetch call.
pub fn prepare(args: &Value, ctx: &ToolCtx) -> Result<FetchToolPrepared> {
    let args = tool_arguments(args)?;

    if !args.get("url").is_some_and(|v| v.is_string()) {
        return Err(anyhow::anyhow!("Missing required string argument \"url\"."));
    }

    let url = get_required_string_argument(&args, "url")?;

    // Scheme guard: only http and https allowed
    let Some(scheme) = parse_url_scheme(&url) else {
        return Err(anyhow::anyhow!("FETCH refused: invalid URL \"{url}\"."));
    };

    if scheme != "http" && scheme != "https" {
        return Err(anyhow::anyhow!(
            "FETCH refused: scheme \"{scheme}\" is not allowed. Only http:// and https:// URLs are supported."
        ));
    }

    // Network gating: must opt in via environment variable
    if !ctx.allow_net {
        return Err(anyhow::anyhow!(
            "FETCH refused: network access is disabled. To enable it, set DRIP_ALLOW_NET=1 (use the --allow-net flag when starting drip)."
        ));
    }

    // Validate maxBytes if provided
    let mut max_bytes = DEFAULT_MAX_BYTES;
    if let Some(raw_max) = args.get("maxBytes") {
        if raw_max.is_null() {
            // Models frequently send explicit nulls for optional params; null
            // means "absent", not "invalid" (getOptionalNumberArgument).
        } else {
            let parsed = match raw_max.as_f64() {
                Some(f) => f,
                None => {
                    return Err(anyhow::anyhow!("Argument \"maxBytes\" must be a positive number."));
                }
            };
            if !parsed.is_finite() || parsed <= 0.0 {
                return Err(anyhow::anyhow!("Argument \"maxBytes\" must be a positive number."));
            }
            max_bytes = (parsed.round() as i64).min(MAX_MAX_BYTES);
        }
    }

    let display_input = match args.get("maxBytes") {
        Some(v) if !v.is_null() => format!("GET {url} (max {max_bytes} bytes)"),
        _ => format!("GET {url}"),
    };

    Ok(FetchToolPrepared {
        input: FetchToolInput {
            max_bytes,
            url,
        },
        display_input,
    })
}

// ---------------------------------------------------------------------------
// Execution
// ---------------------------------------------------------------------------

/// GET with timeout and redirect following, binary content-type refusal,
/// HTML stripping, ends-kept size cap.
pub fn execute_prepared(prepared: &FetchToolPrepared) -> Result<FetchToolExecution> {
    let url = &prepared.input.url;
    let capped_max = prepared.input.max_bytes.min(MAX_MAX_BYTES) as usize;

    // Fetch with timeout and redirect following
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_millis(FETCH_TIMEOUT_MS))
        .redirect(reqwest::redirect::Policy::limited(10))
        .build()?;
    let response = client.get(url).send()?;

    let final_url = response.url().as_str().to_string();
    let raw_content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("application/octet-stream")
        .to_string();
    // Normalize: strip parameters for classification
    let base_content_type = raw_content_type.split(';').next().unwrap_or("").trim().to_string();

    // Refuse binary content types
    if is_binary_content_type(&base_content_type) {
        return Err(anyhow::anyhow!(
            "FETCH refused: binary content-type \"{base_content_type}\" is not supported. Only text content types are allowed."
        ));
    }

    // text() consumes the response, so grab everything else off it first.
    let status = response.status().as_u16();
    let raw_text = response.text()?;
    let body = cap_bytes(
        &if is_html_content_type(&base_content_type) {
            strip_html(&raw_text)
        } else {
            raw_text
        },
        capped_max,
    );
    let header_line = format!("FETCH {status} {raw_content_type} {final_url} ({} chars)", body.chars().count());

    Ok(FetchToolExecution {
        data: FetchToolResult {
            body: body.clone(),
            content_type: raw_content_type,
            final_url,
            status,
        },
        output_text: header_line,
    })
}

/// Build the final tool completion. The header is the description AND the
/// first line of the tool content.
pub fn complete(prepared: &FetchToolPrepared, result: &FetchToolResult) -> super::ToolCompletion {
    let header_line = format!(
        "FETCH {} {} {} ({} chars)",
        result.status,
        result.content_type,
        result.final_url,
        result.body.chars().count()
    );
    let tool_content = format!("{}\n\n{}", header_line, result.body);

    let _ = prepared;

    super::ToolCompletion {
        blocks: vec![super::ToolCompletionBlock {
            code: tool_content.clone(),
            description: header_line,
            language: "text".to_string(),
            path: result.final_url.clone().into(),
        }],
        tool_content,
    }
}

/// The transcript's display string for this call — the prepare stage's
/// display string — or None when the arguments do not parse (the execute path
/// reports that error).
pub fn display_input(args: &Value, ctx: &ToolCtx) -> Option<String> {
    prepare(args, ctx).ok().map(|prepared| prepared.display_input)
}

/// Whole-pipeline entry point: prepare → execute → complete, mapping errors
/// to the model-facing failure text (buildFailureResult shape).
pub fn execute(args: &Value, ctx: &ToolCtx) -> ToolOutcome {
    let outcome = prepare(args, ctx).and_then(|prepared| {
        let execution = execute_prepared(&prepared)?;
        let completion = complete(&prepared, &execution.data);
        Ok(completion.tool_content)
    });

    match outcome {
        Ok(text) => ToolOutcome::success(text),
        Err(error) => ToolOutcome::error(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // prepare() — scheme refusal (the DRIP_ALLOW_NET gate is opened via the
    // ToolCtx.allow_net flag in these tests).

    fn stage_ctx(allow_net: bool) -> ToolCtx {
        ToolCtx {
            cwd: std::env::temp_dir(),
            allow_net,
        }
    }

    fn prepare_json(raw: Value) -> Result<FetchToolPrepared> {
        prepare(&raw, &stage_ctx(true))
    }

    #[test]
    fn prepare_refuses_ftp_scheme() {
        let error = prepare_json(json!({ "url": "ftp://example.com/file.txt" }))
            .unwrap_err()
            .to_string();
        assert!(error.contains("scheme \"ftp\" is not allowed"), "{error}");
    }

    #[test]
    fn prepare_refuses_file_scheme() {
        let error = prepare_json(json!({ "url": "file:///etc/passwd" }))
            .unwrap_err()
            .to_string();
        assert!(error.contains("scheme \"file\" is not allowed"), "{error}");
    }

    #[test]
    fn prepare_refuses_data_scheme() {
        let error = prepare_json(json!({ "url": "data:text/plain,hello" }))
            .unwrap_err()
            .to_string();
        assert!(error.contains("scheme \"data\" is not allowed"), "{error}");
    }

    #[test]
    fn prepare_refuses_completely_invalid_url() {
        let error = prepare_json(json!({ "url": "not a url" }))
            .unwrap_err()
            .to_string();
        assert!(error.contains("invalid URL"), "{error}");
    }

    #[test]
    fn prepare_accepts_http_and_https() {
        assert!(prepare_json(json!({ "url": "http://example.com/" })).is_ok());
        assert!(prepare_json(json!({ "url": "https://example.com/" })).is_ok());
    }

    // prepare() — network gating

    #[test]
    fn prepare_refuses_without_allow_net() {
        let error = prepare(&json!({ "url": "https://example.com/" }), &stage_ctx(false))
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("network access is disabled"),
            "{error}"
        );
    }

    // The refusal must name the env var and the CLI flag that enables network.
    #[test]
    fn allow_net_refusal_message_mentions_how_to_enable() {
        let error = prepare(&json!({ "url": "https://example.com/" }), &stage_ctx(false))
            .unwrap_err()
            .to_string();
        assert!(error.contains("DRIP_ALLOW_NET=1"), "{error}");
        assert!(error.contains("--allow-net"), "{error}");
    }

    // The gate is the boolean ctx.allow_net, so anything but enabled is refused.
    #[test]
    fn prepare_refuses_when_allow_net_not_exactly_enabled() {
        assert!(prepare(&json!({ "url": "https://example.com/" }), &stage_ctx(false)).is_err());
        assert!(prepare(&json!({ "url": "https://example.com/" }), &stage_ctx(true)).is_ok());
    }
}
