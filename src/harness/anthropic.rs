// Native Anthropic Messages API transport for the "claude" provider.
//
// Anthropic's OpenAI-compatible endpoint (the /chat/completions URL every
// other provider uses) explicitly does not support prompt caching, and its
// usage.prompt_tokens_details is documented as always empty — so a Claude run
// through that endpoint re-pays full price for the entire growing transcript
// on every cycle. This module translates the harness's OpenAI-shaped requests
// to /v1/messages (where cache_control is supported) and translates responses
// back, so the rest of the transport layer stays provider-agnostic:
//   - system messages hoist into the top-level `system` array, with a
//     cache_control breakpoint on the last block (caches tools + system);
//   - tool-bearing requests set top-level cache_control, Anthropic's
//     automatic conversation caching (the breakpoint follows the transcript
//     as it grows, so each cycle re-reads the previous cycle's prefix);
//   - cache_read_input_tokens / cache_creation_input_tokens surface on the
//     translated usage so the run ledger can price the run honestly.
use crate::harness::chat_types::ChatRoleTag;
use crate::harness::transport::{
    OpenAICompatibleRequestTool, OpenAICompatibleToolCall, OpenAICompatibleToolCallFunction,
    TransportContent, TransportContentPart, TransportRequestMessage, transport_content_to_text,
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

pub const ANTHROPIC_VERSION: &str = "2023-06-01";

// /v1/messages requires max_tokens. Harness turns are tool-call sized; 16384
// leaves room for long PATCH bodies while staying inside the non-streaming
// comfort zone of every current Claude model.
pub const DEFAULT_ANTHROPIC_MAX_TOKENS: u64 = 16384;

/// Providers whose requests are translated to the native Anthropic API.
pub fn is_anthropic_native_provider(provider: Option<&str>) -> bool {
    provider == Some("claude")
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AnthropicCacheControl {
    #[serde(rename = "type")]
    pub cache_type: String, // "ephemeral"
}

impl AnthropicCacheControl {
    pub fn ephemeral() -> Self {
        Self {
            cache_type: "ephemeral".to_string(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AnthropicTextBlock {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_control: Option<AnthropicCacheControl>,
    pub text: String,
    #[serde(rename = "type")]
    pub block_type: String, // "text"
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AnthropicImageBase64Source {
    pub data: String,
    pub media_type: String,
    #[serde(rename = "type")]
    pub source_type: String, // "base64"
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AnthropicImageUrlSource {
    #[serde(rename = "type")]
    pub source_type: String, // "url"
    pub url: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum AnthropicImageSource {
    Base64(AnthropicImageBase64Source),
    Url(AnthropicImageUrlSource),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AnthropicImageBlock {
    pub source: AnthropicImageSource,
    #[serde(rename = "type")]
    pub block_type: String, // "image"
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AnthropicToolUseBlock {
    pub id: String,
    pub input: Value,
    pub name: String,
    #[serde(rename = "type")]
    pub block_type: String, // "tool_use"
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AnthropicToolResultBlock {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    pub tool_use_id: String,
    #[serde(rename = "type")]
    pub block_type: String, // "tool_result"
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum AnthropicContentBlock {
    Text(AnthropicTextBlock),
    Image(AnthropicImageBlock),
    ToolUse(AnthropicToolUseBlock),
    ToolResult(AnthropicToolResultBlock),
    /// Native blocks the harness never constructs (e.g. thinking blocks)
    /// replay verbatim as raw JSON, exactly like the TS cast does.
    Raw(Value),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum AnthropicRole {
    #[serde(rename = "assistant")]
    Assistant,
    #[serde(rename = "user")]
    User,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AnthropicMessage {
    pub content: Vec<AnthropicContentBlock>,
    pub role: AnthropicRole,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AnthropicToolDefinition {
    pub description: String,
    pub input_schema: Value,
    pub name: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AnthropicToolChoice {
    #[serde(rename = "type")]
    pub choice_type: String, // "auto"
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct AnthropicRequestPayload {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_control: Option<AnthropicCacheControl>,
    pub max_tokens: u64,
    pub messages: Vec<AnthropicMessage>,
    pub model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system: Option<Vec<AnthropicTextBlock>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<AnthropicToolChoice>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<AnthropicToolDefinition>>,
}

fn text_block(text: String) -> AnthropicContentBlock {
    AnthropicContentBlock::Text(AnthropicTextBlock {
        cache_control: None,
        text,
        block_type: "text".to_string(),
    })
}

/// Routes the resolved chat-completions URL to its native /v1/messages sibling.
pub fn build_anthropic_messages_url(chat_completions_url: &str) -> String {
    for suffix in ["/chat/completions/", "/chat/completions"] {
        if let Some(head) = chat_completions_url.strip_suffix(suffix) {
            return format!("{head}/messages");
        }
    }

    let trimmed_url = chat_completions_url.trim_end_matches('/');

    if trimmed_url.ends_with("/messages") {
        trimmed_url.to_string()
    } else {
        format!("{trimmed_url}/messages")
    }
}

// The shared route resolver authenticates every provider with
// "Authorization: Bearer <key>"; the native Anthropic API wants API keys on
// x-api-key plus an anthropic-version header. Explicit x-api-key or
// anthropic-version headers on a profile win over the conversion.
pub fn build_anthropic_headers(headers: &[(String, String)]) -> Vec<(String, String)> {
    fn bearer_regex() -> &'static regex::Regex {
        static BEARER: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();

        BEARER.get_or_init(|| {
            regex::Regex::new(r"(?i)^Bearer\s+(.+)$").expect("valid bearer header regex")
        })
    }

    let mut result: Vec<(String, String)> = Vec::new();
    let mut bearer_token: Option<String> = None;

    for (name, value) in headers {
        if name.to_lowercase() == "authorization" {
            if let Some(captures) = bearer_regex().captures(value.trim()) {
                bearer_token = Some(captures[1].to_string());
                continue;
            }
        }

        result.push((name.clone(), value.clone()));
    }

    let has_header = |result: &Vec<(String, String)>, header_name: &str| {
        result
            .iter()
            .any(|(name, _)| name.to_lowercase() == header_name)
    };

    if let Some(token) = bearer_token {
        if !has_header(&result, "x-api-key") {
            result.push(("x-api-key".to_string(), token));
        }
    }

    if !has_header(&result, "anthropic-version") {
        result.push(("anthropic-version".to_string(), ANTHROPIC_VERSION.to_string()));
    }

    result
}

// Tool-call arguments we serialize ourselves always round-trip through
// JSON.parse; a foreign or truncated history degrades to an empty input
// instead of failing the whole request.
fn parse_tool_arguments(raw_arguments: Option<&str>) -> Value {
    let Some(raw_arguments) = raw_arguments else {
        return Value::Object(Map::new());
    };

    if raw_arguments.trim().is_empty() {
        return Value::Object(Map::new());
    }

    match serde_json::from_str::<Value>(raw_arguments) {
        Ok(parsed @ Value::Object(_)) => parsed,
        _ => Value::Object(Map::new()),
    }
}

fn image_block_from_url(url: &str) -> AnthropicContentBlock {
    fn data_url_regex() -> &'static regex::Regex {
        static DATA_URL: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();

        DATA_URL.get_or_init(|| {
            regex::Regex::new(r"(?s)^data:([^;,]+);base64,(.*)$")
                .expect("valid data URL regex")
        })
    }

    if let Some(captures) = data_url_regex().captures(url) {
        return AnthropicContentBlock::Image(AnthropicImageBlock {
            source: AnthropicImageSource::Base64(AnthropicImageBase64Source {
                data: captures[2].to_string(),
                media_type: captures[1].to_string(),
                source_type: "base64".to_string(),
            }),
            block_type: "image".to_string(),
        });
    }

    let lowered = url.to_lowercase();

    if lowered.starts_with("http://") || lowered.starts_with("https://") {
        return AnthropicContentBlock::Image(AnthropicImageBlock {
            source: AnthropicImageSource::Url(AnthropicImageUrlSource {
                source_type: "url".to_string(),
                url: url.to_string(),
            }),
            block_type: "image".to_string(),
        });
    }

    // Unrepresentable image reference (e.g. a non-base64 data URL): keep a
    // textual trace so the model knows something was elided.
    let head: String = url.chars().take(40).collect();

    AnthropicContentBlock::Text(AnthropicTextBlock {
        cache_control: None,
        text: format!("[image omitted: {head}...]"),
        block_type: "text".to_string(),
    })
}

fn user_content_blocks(content: Option<&TransportContent>) -> Vec<AnthropicContentBlock> {
    let Some(content) = content else {
        return Vec::new();
    };

    match content {
        TransportContent::Text(text) => {
            if text.is_empty() {
                Vec::new()
            } else {
                vec![text_block(text.clone())]
            }
        }
        TransportContent::Parts(parts) => {
            let mut blocks: Vec<AnthropicContentBlock> = Vec::new();

            for part in parts {
                match part {
                    TransportContentPart::Text { text } => {
                        if !text.is_empty() {
                            blocks.push(text_block(text.clone()));
                        }
                    }
                    TransportContentPart::ImageUrl { image_url } => {
                        blocks.push(image_block_from_url(&image_url.url));
                    }
                }
            }

            blocks
        }
    }
}

// The native API requires strictly alternating user/assistant turns; merging
// keeps consecutive same-role outputs (parallel tool results, a tool result
// followed by user steering) legal.
fn append_blocks(
    messages: &mut Vec<AnthropicMessage>,
    role: AnthropicRole,
    blocks: Vec<AnthropicContentBlock>,
) {
    if blocks.is_empty() {
        return;
    }

    if let Some(last_message) = messages.last_mut() {
        if last_message.role == role {
            last_message.content.extend(blocks);
            return;
        }
    }

    messages.push(AnthropicMessage { content: blocks, role });
}

pub struct BuildAnthropicRequestPayloadArgs {
    /// Set false for one-shot calls (run summaries) where a cache write is a pure premium with no later read. Defaults to true.
    pub cache: Option<bool>,
    pub max_tokens: Option<u64>,
    pub messages: Vec<TransportRequestMessage>,
    pub model: String,
    pub tools: Option<Vec<OpenAICompatibleRequestTool>>,
}

pub fn build_anthropic_request_payload(
    args: BuildAnthropicRequestPayloadArgs,
) -> AnthropicRequestPayload {
    let cache = args.cache != Some(false);
    let mut system_blocks: Vec<AnthropicTextBlock> = Vec::new();
    let mut messages: Vec<AnthropicMessage> = Vec::new();

    for message in args.messages {
        match message.role {
            ChatRoleTag::System => {
                let text = transport_content_to_text(message.content.as_ref());

                if !text.trim().is_empty() {
                    system_blocks.push(AnthropicTextBlock {
                        cache_control: None,
                        text,
                        block_type: "text".to_string(),
                    });
                }
            }
            ChatRoleTag::Tool => {
                let text = transport_content_to_text(message.content.as_ref());

                append_blocks(
                    &mut messages,
                    AnthropicRole::User,
                    vec![AnthropicContentBlock::ToolResult(AnthropicToolResultBlock {
                        content: (!text.is_empty()).then_some(text),
                        tool_use_id: message.tool_call_id.clone().unwrap_or_default(),
                        block_type: "tool_result".to_string(),
                    })],
                );
            }
            ChatRoleTag::Assistant => {
                // A turn that came from this API replays verbatim: models with thinking
                // enabled reject a tool-use turn whose thinking blocks were stripped,
                // and byte-identical replay is also what keeps the cached prefix warm.
                if let Some(blocks) = &message.anthropic_content {
                    if !blocks.is_empty() {
                        let blocks = blocks
                            .iter()
                            .cloned()
                            .map(AnthropicContentBlock::Raw)
                            .collect();

                        append_blocks(&mut messages, AnthropicRole::Assistant, blocks);
                        continue;
                    }
                }

                let mut blocks: Vec<AnthropicContentBlock> = Vec::new();
                let text = transport_content_to_text(message.content.as_ref());

                if !text.is_empty() {
                    blocks.push(text_block(text));
                }

                for (index, tool_call) in message.tool_calls.unwrap_or_default().iter().enumerate() {
                    blocks.push(AnthropicContentBlock::ToolUse(AnthropicToolUseBlock {
                        id: tool_call
                            .id
                            .clone()
                            .unwrap_or_else(|| format!("call_{}_{}", messages.len(), index)),
                        input: parse_tool_arguments(
                            tool_call
                                .function
                                .as_ref()
                                .and_then(|function| function.arguments.as_deref()),
                        ),
                        name: tool_call
                            .function
                            .as_ref()
                            .and_then(|function| function.name.clone())
                            .unwrap_or_default(),
                        block_type: "tool_use".to_string(),
                    }));
                }

                append_blocks(&mut messages, AnthropicRole::Assistant, blocks);
            }
            ChatRoleTag::User => {
                append_blocks(
                    &mut messages,
                    AnthropicRole::User,
                    user_content_blocks(message.content.as_ref()),
                );
            }
        }
    }

    if cache && !system_blocks.is_empty() {
        if let Some(last_block) = system_blocks.last_mut() {
            last_block.cache_control = Some(AnthropicCacheControl::ephemeral());
        }
    }

    // Top-level cache_control is Anthropic's automatic conversation caching:
    // the breakpoint tracks the end of the transcript, so each cycle reads
    // the previous cycle's prefix instead of re-paying for it.
    let cache_control = cache.then(AnthropicCacheControl::ephemeral);
    let tools = args.tools.filter(|tools| !tools.is_empty());

    AnthropicRequestPayload {
        cache_control,
        max_tokens: args.max_tokens.unwrap_or(DEFAULT_ANTHROPIC_MAX_TOKENS),
        messages,
        model: args.model,
        system: (!system_blocks.is_empty()).then_some(system_blocks),
        tool_choice: tools
            .as_ref()
            .map(|_| AnthropicToolChoice {
                choice_type: "auto".to_string(),
            }),
        tools: tools.map(|tools| {
            tools
                .into_iter()
                .map(|tool| AnthropicToolDefinition {
                    description: tool.function.description,
                    input_schema: tool.function.parameters,
                    name: tool.function.name,
                })
                .collect()
        }),
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct AnthropicTranslatedError {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "type")]
    pub error_type: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct AnthropicTranslatedUsage {
    #[serde(rename = "cache_creation_input_tokens")]
    pub cache_creation_input_tokens: u64,
    #[serde(rename = "cache_read_input_tokens")]
    pub cache_read_input_tokens: u64,
    #[serde(rename = "completion_tokens")]
    pub completion_tokens: u64,
    #[serde(rename = "prompt_tokens")]
    pub prompt_tokens: u64,
    #[serde(rename = "total_tokens")]
    pub total_tokens: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct AnthropicTranslatedMessage {
    /// Raw native content blocks for verbatim replay (see TransportRequestMessage.anthropicContent).
    #[serde(skip_serializing_if = "Option::is_none", rename = "anthropicContent")]
    pub anthropic_content: Option<Vec<Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<OpenAICompatibleToolCall>>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct AnthropicTranslatedChoice {
    /// "length" when Anthropic reports `stop_reason: "max_tokens"`, the
    /// OpenAI spelling the loop keys its truncation nudge on.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finish_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<AnthropicTranslatedMessage>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct AnthropicTranslatedResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub choices: Option<Vec<AnthropicTranslatedChoice>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<AnthropicTranslatedError>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<AnthropicTranslatedUsage>,
}

// Maps a native response back onto the OpenAI-compatible shape the retry
// ladder and the loop already understand. Anthropic's input_tokens counts
// only the uncached remainder, so prompt_tokens re-adds the cache read/write
// counts — the ledger's promptTokens keeps meaning "tokens the model saw".
pub fn translate_anthropic_response(data: &Value) -> AnthropicTranslatedResponse {
    // TS reads `data ?? {}`: null/undefined degrades to an empty body.
    if data.is_null() {
        return AnthropicTranslatedResponse::default();
    }

    if let Some(error) = data.get("error") {
        if error.is_object() {
            let mut translated_error = AnthropicTranslatedError::default();

            if let Some(Value::String(message)) = error.get("message") {
                translated_error.message = Some(message.clone());
            }

            if let Some(Value::String(error_type)) = error.get("type") {
                translated_error.error_type = Some(error_type.clone());
            }

            return AnthropicTranslatedResponse {
                error: Some(translated_error),
                ..AnthropicTranslatedResponse::default()
            };
        }
    }

    let Some(content) = data.get("content").filter(|content| content.is_array()) else {
        // Not a message payload; the caller's no-choices guard reports it loudly.
        return AnthropicTranslatedResponse::default();
    };

    let content_blocks = content.as_array().cloned().unwrap_or_default();
    let mut text_parts: Vec<String> = Vec::new();
    let mut tool_calls: Vec<OpenAICompatibleToolCall> = Vec::new();

    for block in &content_blocks {
        match block.get("type").and_then(Value::as_str) {
            Some("text") => {
                if let Some(Value::String(text)) = block.get("text") {
                    if !text.is_empty() {
                        text_parts.push(text.clone());
                    }
                }
            }
            Some("tool_use") => {
                let raw_input = block.get("input").cloned().unwrap_or(Value::Null);
                // `input ?? {}`: a missing/null input serializes as "{}".
                let input = if raw_input.is_null() {
                    Value::Object(Map::new())
                } else {
                    raw_input
                };
                let arguments = serde_json::to_string(&input).unwrap_or_else(|_| "{}".to_string());
                let name = match block.get("name") {
                    Some(Value::String(name)) => name.clone(),
                    _ => String::new(),
                };

                tool_calls.push(OpenAICompatibleToolCall {
                    function: Some(OpenAICompatibleToolCallFunction {
                        arguments: Some(arguments),
                        name: Some(name),
                    }),
                    id: match block.get("id") {
                        Some(Value::String(id)) => Some(id.clone()),
                        _ => None,
                    },
                    tool_type: Some("function".to_string()),
                });
            }
            _ => {}
        }
    }

    let cache_creation_tokens = data
        .get("usage")
        .and_then(|usage| usage.get("cache_creation_input_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let cache_read_tokens = data
        .get("usage")
        .and_then(|usage| usage.get("cache_read_input_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let input_tokens = data
        .get("usage")
        .and_then(|usage| usage.get("input_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let output_tokens = data
        .get("usage")
        .and_then(|usage| usage.get("output_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let prompt_tokens = input_tokens + cache_creation_tokens + cache_read_tokens;
    let completion_tokens = output_tokens;

    AnthropicTranslatedResponse {
        choices: Some(vec![AnthropicTranslatedChoice {
            finish_reason: (data.get("stop_reason").and_then(Value::as_str) == Some("max_tokens"))
                .then(|| "length".to_string()),
            message: Some(AnthropicTranslatedMessage {
                // The raw blocks ride along so the loop can echo this turn back
                // verbatim (thinking blocks must survive a tool round-trip).
                anthropic_content: Some(content_blocks),
                content: Some(Value::String(text_parts.join("\n\n"))),
                tool_calls: (!tool_calls.is_empty()).then_some(tool_calls),
            }),
        }]),
        error: None,
        usage: data.get("usage").filter(|usage| usage.is_object()).map(|_| {
            AnthropicTranslatedUsage {
                cache_creation_input_tokens: cache_creation_tokens,
                cache_read_input_tokens: cache_read_tokens,
                completion_tokens: completion_tokens,
                prompt_tokens,
                total_tokens: prompt_tokens + completion_tokens,
            }
        }),
    }
}

#[cfg(test)]
mod tests {
    // Ports of the pure (non-network) tests in test/anthropic-transport.test.ts:
    // the "isAnthropicNativeProvider", "buildAnthropicMessagesUrl",
    // "buildAnthropicHeaders", "buildAnthropicRequestPayload" and
    // "translateAnthropicResponse" describes. The "shipped Claude profiles on
    // OpenRouter" describe needs the web/settings route resolver (another
    // lane's module) and the "createModelCaller with the claude provider"
    // describe needs the reqwest caller — both are deferred to task-10.
    use super::*;
    use crate::harness::transport::{
        OpenAICompatibleFunctionDefinition, OpenAICompatibleRequestTool,
        OpenAICompatibleToolCall, OpenAICompatibleToolCallFunction, TransportContent,
        TransportContentPart, TransportImageUrl, TransportRequestMessage,
    };
    use crate::harness::chat_types::ChatRoleTag;
    use serde_json::json;

    fn sample_tools() -> Vec<OpenAICompatibleRequestTool> {
        vec![OpenAICompatibleRequestTool {
            function: OpenAICompatibleFunctionDefinition {
                description: "Read a file.".to_string(),
                name: "READ".to_string(),
                parameters: json!({
                    "properties": { "path": { "type": "string" } },
                    "required": ["path"],
                    "type": "object"
                }),
            },
            tool_type: "function".to_string(),
        }]
    }

    fn tool_call(id: &str, arguments: &str) -> OpenAICompatibleToolCall {
        OpenAICompatibleToolCall {
            function: Some(OpenAICompatibleToolCallFunction {
                arguments: Some(arguments.to_string()),
                name: Some("READ".to_string()),
            }),
            id: Some(id.to_string()),
            tool_type: Some("function".to_string()),
        }
    }

    // it("routes only the claude provider natively")
    #[test]
    fn routes_only_the_claude_provider_natively() {
        assert!(is_anthropic_native_provider(Some("claude")));
        assert!(!is_anthropic_native_provider(Some("openai")));
        assert!(!is_anthropic_native_provider(None));
    }

    // it("swaps the chat-completions suffix for /messages")
    #[test]
    fn swaps_the_chat_completions_suffix_for_messages() {
        assert_eq!(
            build_anthropic_messages_url("https://api.anthropic.com/v1/chat/completions"),
            "https://api.anthropic.com/v1/messages"
        );
    }

    // it("appends /messages to a bare base URL and leaves an existing /messages URL alone")
    #[test]
    fn appends_messages_to_a_bare_base_url_and_leaves_an_existing_messages_url_alone() {
        assert_eq!(
            build_anthropic_messages_url("https://proxy.example/v1/"),
            "https://proxy.example/v1/messages"
        );
        assert_eq!(
            build_anthropic_messages_url("https://proxy.example/v1/messages"),
            "https://proxy.example/v1/messages"
        );
    }

    // it("converts the shared Bearer credential to x-api-key and stamps the API version")
    #[test]
    fn converts_the_shared_bearer_credential_to_x_api_key_and_stamps_the_api_version() {
        let headers = build_anthropic_headers(&[
            ("Authorization".to_string(), "Bearer sk-ant-test123".to_string()),
            ("content-type".to_string(), "application/json".to_string()),
        ]);

        // TS asserts with toEqual on an object (order-insensitive); the
        // builder's insertion order is content-type, then x-api-key, then
        // anthropic-version — same keys, same values.
        assert_eq!(
            headers,
            vec![
                ("content-type".to_string(), "application/json".to_string()),
                ("x-api-key".to_string(), "sk-ant-test123".to_string()),
                ("anthropic-version".to_string(), ANTHROPIC_VERSION.to_string()),
            ]
        );
    }

    // it("never clobbers an explicit x-api-key or anthropic-version from the profile")
    #[test]
    fn never_clobbers_an_explicit_x_api_key_or_anthropic_version_from_the_profile() {
        let headers = build_anthropic_headers(&[
            ("Authorization".to_string(), "Bearer ignored".to_string()),
            ("anthropic-version".to_string(), "2024-01-01".to_string()),
            ("x-api-key".to_string(), "sk-ant-explicit".to_string()),
        ]);

        assert_eq!(
            headers
                .iter()
                .find(|(name, _)| name == "x-api-key")
                .map(|(_, value)| value.as_str()),
            Some("sk-ant-explicit")
        );
        assert_eq!(
            headers
                .iter()
                .find(|(name, _)| name == "anthropic-version")
                .map(|(_, value)| value.as_str()),
            Some("2024-01-01")
        );
        assert!(headers.iter().all(|(name, _)| name != "Authorization"));
    }

    // it("passes a non-bearer authorization header through untouched")
    #[test]
    fn passes_a_non_bearer_authorization_header_through_untouched() {
        let headers = build_anthropic_headers(&[(
            "Authorization".to_string(),
            "Basic abc".to_string(),
        )]);

        assert_eq!(
            headers
                .iter()
                .find(|(name, _)| name == "Authorization")
                .map(|(_, value)| value.as_str()),
            Some("Basic abc")
        );
    }

    // it("hoists system messages with a cache breakpoint on the last block and enables automatic caching")
    #[test]
    fn hoists_system_messages_with_a_cache_breakpoint_on_the_last_block() {
        let messages = vec![
            TransportRequestMessage {
                content: Some(TransportContent::Text("You are the harness.".to_string())),
                role: ChatRoleTag::System,
                ..TransportRequestMessage::default()
            },
            TransportRequestMessage {
                content: Some(TransportContent::Text("cwd: /repo".to_string())),
                role: ChatRoleTag::System,
                ..TransportRequestMessage::default()
            },
            TransportRequestMessage {
                content: Some(TransportContent::Text("do the goal".to_string())),
                role: ChatRoleTag::User,
                ..TransportRequestMessage::default()
            },
        ];

        let payload = build_anthropic_request_payload(BuildAnthropicRequestPayloadArgs {
            cache: None,
            max_tokens: None,
            messages,
            model: "claude-sonnet-4-6".to_string(),
            tools: Some(sample_tools()),
        });

        assert_eq!(payload.cache_control, Some(AnthropicCacheControl::ephemeral()));
        assert_eq!(payload.max_tokens, DEFAULT_ANTHROPIC_MAX_TOKENS);
        assert_eq!(
            payload.system,
            Some(vec![
                AnthropicTextBlock {
                    cache_control: None,
                    text: "You are the harness.".to_string(),
                    block_type: "text".to_string(),
                },
                AnthropicTextBlock {
                    cache_control: Some(AnthropicCacheControl::ephemeral()),
                    text: "cwd: /repo".to_string(),
                    block_type: "text".to_string(),
                },
            ])
        );
        assert_eq!(
            payload.messages,
            vec![AnthropicMessage {
                content: vec![AnthropicContentBlock::Text(AnthropicTextBlock {
                    cache_control: None,
                    text: "do the goal".to_string(),
                    block_type: "text".to_string(),
                })],
                role: AnthropicRole::User,
            }]
        );
        assert_eq!(
            payload.tool_choice,
            Some(AnthropicToolChoice { choice_type: "auto".to_string() })
        );
        assert_eq!(
            payload.tools,
            Some(vec![AnthropicToolDefinition {
                description: "Read a file.".to_string(),
                input_schema: json!({
                    "properties": { "path": { "type": "string" } },
                    "required": ["path"],
                    "type": "object"
                }),
                name: "READ".to_string(),
            }])
        );

        // The whole payload must serialize with the native wire field names.
        let wire = serde_json::to_value(&payload).expect("payload serializes");
        assert_eq!(wire["cache_control"], json!({ "type": "ephemeral" }));
        assert_eq!(wire["max_tokens"], json!(DEFAULT_ANTHROPIC_MAX_TOKENS));
        assert_eq!(wire["tool_choice"], json!({ "type": "auto" }));
        assert_eq!(
            wire["tools"],
            json!([{
                "description": "Read a file.",
                "input_schema": {
                    "properties": { "path": { "type": "string" } },
                    "required": ["path"],
                    "type": "object"
                },
                "name": "READ"
            }])
        );
    }

    // it("omits every cache_control marker when caching is off (one-shot summary calls)")
    #[test]
    fn omits_every_cache_control_marker_when_caching_is_off() {
        let payload = build_anthropic_request_payload(BuildAnthropicRequestPayloadArgs {
            cache: Some(false),
            max_tokens: None,
            messages: vec![
                TransportRequestMessage {
                    content: Some(TransportContent::Text("summarize".to_string())),
                    role: ChatRoleTag::System,
                    ..TransportRequestMessage::default()
                },
                TransportRequestMessage {
                    content: Some(TransportContent::Text("the run".to_string())),
                    role: ChatRoleTag::User,
                    ..TransportRequestMessage::default()
                },
            ],
            model: "claude-sonnet-4-6".to_string(),
            tools: None,
        });

        assert_eq!(payload.cache_control, None);
        let system = payload.system.expect("system hoisted");
        assert!(system.iter().all(|block| block.cache_control.is_none()));
        assert_eq!(payload.tools, None);
    }

    // it("translates assistant tool calls to tool_use blocks and degrades unparseable arguments to an empty input")
    #[test]
    fn translates_assistant_tool_calls_to_tool_use_blocks() {
        let payload = build_anthropic_request_payload(BuildAnthropicRequestPayloadArgs {
            cache: None,
            max_tokens: None,
            messages: vec![
                TransportRequestMessage {
                    content: Some(TransportContent::Text("goal".to_string())),
                    role: ChatRoleTag::User,
                    ..TransportRequestMessage::default()
                },
                TransportRequestMessage {
                    content: Some(TransportContent::Text("Working on it.".to_string())),
                    role: ChatRoleTag::Assistant,
                    tool_calls: Some(vec![
                        tool_call("call-1", r#"{"path":"src/a.ts"}"#),
                        tool_call("call-2", "{not json"),
                    ]),
                    ..TransportRequestMessage::default()
                },
            ],
            model: "claude-sonnet-4-6".to_string(),
            tools: None,
        });

        assert_eq!(
            payload.messages[1],
            AnthropicMessage {
                content: vec![
                    AnthropicContentBlock::Text(AnthropicTextBlock {
                        cache_control: None,
                        text: "Working on it.".to_string(),
                        block_type: "text".to_string(),
                    }),
                    AnthropicContentBlock::ToolUse(AnthropicToolUseBlock {
                        id: "call-1".to_string(),
                        input: json!({ "path": "src/a.ts" }),
                        name: "READ".to_string(),
                        block_type: "tool_use".to_string(),
                    }),
                    AnthropicContentBlock::ToolUse(AnthropicToolUseBlock {
                        id: "call-2".to_string(),
                        input: json!({}),
                        name: "READ".to_string(),
                        block_type: "tool_use".to_string(),
                    }),
                ],
                role: AnthropicRole::Assistant,
            }
        );
    }

    // it("merges consecutive tool results and trailing user text into a single alternating user turn")
    #[test]
    fn merges_consecutive_tool_results_and_trailing_user_text() {
        let payload = build_anthropic_request_payload(BuildAnthropicRequestPayloadArgs {
            cache: None,
            max_tokens: None,
            messages: vec![
                TransportRequestMessage {
                    content: Some(TransportContent::Text("goal".to_string())),
                    role: ChatRoleTag::User,
                    ..TransportRequestMessage::default()
                },
                TransportRequestMessage {
                    content: None,
                    role: ChatRoleTag::Assistant,
                    tool_calls: Some(vec![tool_call("call-1", "{}")]),
                    ..TransportRequestMessage::default()
                },
                TransportRequestMessage {
                    content: Some(TransportContent::Text("result one".to_string())),
                    role: ChatRoleTag::Tool,
                    tool_call_id: Some("call-1".to_string()),
                    ..TransportRequestMessage::default()
                },
                TransportRequestMessage {
                    content: Some(TransportContent::Text(String::new())),
                    role: ChatRoleTag::Tool,
                    tool_call_id: Some("call-2".to_string()),
                    ..TransportRequestMessage::default()
                },
                TransportRequestMessage {
                    content: Some(TransportContent::Text("now continue".to_string())),
                    role: ChatRoleTag::User,
                    ..TransportRequestMessage::default()
                },
            ],
            model: "claude-sonnet-4-6".to_string(),
            tools: None,
        });

        let roles: Vec<AnthropicRole> = payload.messages.iter().map(|m| m.role).collect();
        assert_eq!(roles, vec![AnthropicRole::User, AnthropicRole::Assistant, AnthropicRole::User]);
        assert_eq!(
            payload.messages[2].content,
            vec![
                AnthropicContentBlock::ToolResult(AnthropicToolResultBlock {
                    content: Some("result one".to_string()),
                    tool_use_id: "call-1".to_string(),
                    block_type: "tool_result".to_string(),
                }),
                AnthropicContentBlock::ToolResult(AnthropicToolResultBlock {
                    content: None,
                    tool_use_id: "call-2".to_string(),
                    block_type: "tool_result".to_string(),
                }),
                AnthropicContentBlock::Text(AnthropicTextBlock {
                    cache_control: None,
                    text: "now continue".to_string(),
                    block_type: "text".to_string(),
                }),
            ]
        );
    }

    // it("replays anthropicContent blocks verbatim instead of re-deriving from text + tool_calls")
    #[test]
    fn replays_anthropic_content_blocks_verbatim() {
        let native_blocks = vec![
            json!({ "signature": "sig", "thinking": "let me look", "type": "thinking" }),
            json!({ "id": "toolu_01", "input": { "path": "a.ts" }, "name": "READ", "type": "tool_use" }),
        ];
        let payload = build_anthropic_request_payload(BuildAnthropicRequestPayloadArgs {
            cache: None,
            max_tokens: None,
            messages: vec![
                TransportRequestMessage {
                    content: Some(TransportContent::Text("goal".to_string())),
                    role: ChatRoleTag::User,
                    ..TransportRequestMessage::default()
                },
                TransportRequestMessage {
                    anthropic_content: Some(native_blocks.clone()),
                    content: None,
                    role: ChatRoleTag::Assistant,
                    tool_calls: Some(vec![tool_call("toolu_01", r#"{"path":"a.ts"}"#)]),
                    ..TransportRequestMessage::default()
                },
                TransportRequestMessage {
                    content: Some(TransportContent::Text("file contents".to_string())),
                    role: ChatRoleTag::Tool,
                    tool_call_id: Some("toolu_01".to_string()),
                    ..TransportRequestMessage::default()
                },
            ],
            model: "claude-sonnet-4-6".to_string(),
            tools: None,
        });

        // TS: expect(payload.messages[1]).toEqual({ content: nativeBlocks, role: "assistant" })
        assert_eq!(payload.messages[1], AnthropicMessage {
            content: native_blocks.iter().cloned().map(AnthropicContentBlock::Raw).collect(),
            role: AnthropicRole::Assistant,
        });
        assert_eq!(payload.messages[2].content, vec![
            AnthropicContentBlock::ToolResult(AnthropicToolResultBlock {
                content: Some("file contents".to_string()),
                tool_use_id: "toolu_01".to_string(),
                block_type: "tool_result".to_string(),
            }),
        ]);
    }

    // it("converts image parts: data URLs to base64 sources, http URLs to url sources")
    #[test]
    fn converts_image_parts_data_urls_to_base64_sources() {
        let payload = build_anthropic_request_payload(BuildAnthropicRequestPayloadArgs {
            cache: None,
            max_tokens: None,
            messages: vec![TransportRequestMessage {
                content: Some(TransportContent::Parts(vec![
                    TransportContentPart::Text { text: "look at this".to_string() },
                    TransportContentPart::ImageUrl {
                        image_url: TransportImageUrl {
                            url: "data:image/png;base64,AAAA".to_string(),
                        },
                    },
                    TransportContentPart::ImageUrl {
                        image_url: TransportImageUrl {
                            url: "https://example.com/shot.png".to_string(),
                        },
                    },
                ])),
                role: ChatRoleTag::User,
                ..TransportRequestMessage::default()
            }],
            model: "claude-sonnet-4-6".to_string(),
            tools: None,
        });

        assert_eq!(
            payload.messages[0].content,
            vec![
                AnthropicContentBlock::Text(AnthropicTextBlock {
                    cache_control: None,
                    text: "look at this".to_string(),
                    block_type: "text".to_string(),
                }),
                AnthropicContentBlock::Image(AnthropicImageBlock {
                    source: AnthropicImageSource::Base64(AnthropicImageBase64Source {
                        data: "AAAA".to_string(),
                        media_type: "image/png".to_string(),
                        source_type: "base64".to_string(),
                    }),
                    block_type: "image".to_string(),
                }),
                AnthropicContentBlock::Image(AnthropicImageBlock {
                    source: AnthropicImageSource::Url(AnthropicImageUrlSource {
                        source_type: "url".to_string(),
                        url: "https://example.com/shot.png".to_string(),
                    }),
                    block_type: "image".to_string(),
                }),
            ]
        );
    }

    // it("maps content blocks to an OpenAI-shaped choice and re-adds cache tokens to prompt usage")
    #[test]
    fn maps_content_blocks_to_an_openai_shaped_choice() {
        let translated = translate_anthropic_response(&json!({
            "content": [
                { "text": "Reading the file.", "type": "text" },
                { "id": "toolu_1", "input": { "path": "src/a.ts" }, "name": "READ", "type": "tool_use" }
            ],
            "usage": {
                "cache_creation_input_tokens": 200,
                "cache_read_input_tokens": 1000,
                "input_tokens": 50,
                "output_tokens": 30
            }
        }));

        let message = translated.choices.as_ref().expect("choices")[0]
            .message
            .as_ref()
            .expect("message");
        assert_eq!(message.content, Some(json!("Reading the file.")));
        assert_eq!(
            message.tool_calls,
            Some(vec![OpenAICompatibleToolCall {
                function: Some(OpenAICompatibleToolCallFunction {
                    arguments: Some(r#"{"path":"src/a.ts"}"#.to_string()),
                    name: Some("READ".to_string()),
                }),
                id: Some("toolu_1".to_string()),
                tool_type: Some("function".to_string()),
            }])
        );
        assert_eq!(
            translated.usage,
            Some(AnthropicTranslatedUsage {
                cache_creation_input_tokens: 200,
                cache_read_input_tokens: 1000,
                completion_tokens: 30,
                prompt_tokens: 1250,
                total_tokens: 1280,
            })
        );
        let usage = translated.usage.expect("usage");
        assert_eq!(
            serde_json::to_value(&usage).expect("usage serializes"),
            json!({
                "cache_creation_input_tokens": 200,
                "cache_read_input_tokens": 1000,
                "completion_tokens": 30,
                "prompt_tokens": 1250,
                "total_tokens": 1280
            })
        );
    }

    // it("attaches the raw native blocks for verbatim replay")
    #[test]
    fn attaches_the_raw_native_blocks_for_verbatim_replay() {
        let content = vec![
            json!({ "signature": "sig", "thinking": "hmm", "type": "thinking" }),
            json!({ "id": "toolu_1", "input": {}, "name": "READ", "type": "tool_use" }),
        ];
        let translated = translate_anthropic_response(&json!({
            "content": content,
            "usage": { "input_tokens": 1, "output_tokens": 1 }
        }));

        assert_eq!(
            translated
                .choices
                .as_ref()
                .expect("choices")[0]
                .message
                .as_ref()
                .expect("message")
                .anthropic_content
                .as_ref(),
            Some(&content)
        );
    }

    // it("maps the native error envelope onto the shared error shape")
    #[test]
    fn maps_the_native_error_envelope_onto_the_shared_error_shape() {
        let translated = translate_anthropic_response(&json!({
            "error": {
                "message": "Your credit balance is too low to access the Anthropic API.",
                "type": "invalid_request_error"
            },
            "type": "error"
        }));

        let error = translated.error.expect("error");
        assert!(error
            .message
            .as_ref()
            .expect("message")
            .contains("credit balance"));
        assert!(translated.choices.is_none());
    }

    // it("returns no choices for a non-message body so the protocol guard fires")
    #[test]
    fn returns_no_choices_for_a_non_message_body() {
        assert!(translate_anthropic_response(&json!({})).choices.is_none());
    }
}
