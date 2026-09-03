// Faithful port: the TS helper names are kept in snake_case so the two files
// can be diffed side by side. TransportRequestMessage carries the same wire
// fields (camelCase JSON) as the TS type; `anthropic_content` mirrors the
// internal `anthropicContent` bookkeeping field and is stripped from payload
// messages exactly like the TS buildTransportRequestPayload does.
use crate::harness::chat_types::{
    ChatMessage, ChatRoleTag, ChatRuntimeContext, WorkingFileScope,
    serialize_chat_message_for_context,
};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TransportImageUrl {
    pub url: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum TransportContentPart {
    #[serde(rename = "image_url")]
    ImageUrl { image_url: TransportImageUrl },
    #[serde(rename = "text")]
    Text { text: String },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum TransportContent {
    Text(String),
    Parts(Vec<TransportContentPart>),
}

// Wire field names stay exactly as the TS type spells them: tool_call_id and
// tool_calls are snake_case on the OpenAI-compatible wire (no camelCase rename).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TransportRequestMessage {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<TransportContent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub role: ChatRoleTag,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<OpenAICompatibleToolCall>>,
    /// Raw content blocks of an assistant turn as the native Anthropic API
    /// returned them (thinking + text + tool_use). Replayed verbatim on the
    /// native route: models with thinking enabled reject a tool-use turn whose
    /// thinking blocks were stripped. Internal only — stripped from
    /// OpenAI-compatible request payloads.
    #[serde(skip)]
    pub anthropic_content: Option<Vec<serde_json::Value>>,
}

// Rust-only convenience for test struct-update syntax (`..Default::default()`).
impl Default for TransportRequestMessage {
    fn default() -> Self {
        Self {
            content: None,
            name: None,
            role: ChatRoleTag::User,
            tool_call_id: None,
            tool_calls: None,
            anthropic_content: None,
        }
    }
}

pub fn build_multimodal_user_content(
    text: &str,
    image_data_urls: &[String],
) -> TransportContent {
    if image_data_urls.is_empty() {
        return TransportContent::Text(text.to_string());
    }

    let mut parts = vec![TransportContentPart::Text {
        text: text.to_string(),
    }];

    for url in image_data_urls {
        parts.push(TransportContentPart::ImageUrl {
            image_url: TransportImageUrl { url: url.clone() },
        });
    }

    TransportContent::Parts(parts)
}

pub fn transport_content_to_text(content: Option<&TransportContent>) -> String {
    let Some(content) = content else {
        return String::new();
    };

    match content {
        TransportContent::Text(text) => text.clone(),
        TransportContent::Parts(parts) => parts
            .iter()
            .map(|part| match part {
                TransportContentPart::Text { text } => text.clone(),
                TransportContentPart::ImageUrl { image_url } => {
                    let head: String =
                        image_url.url.chars().take(40).collect();
                    format!("[image: {head}...]")
                }
            })
            .collect::<Vec<_>>()
            .join("\n"),
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct OpenAICompatibleFunctionDefinition {
    pub description: String,
    pub name: String,
    pub parameters: serde_json::Value,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct OpenAICompatibleRequestTool {
    pub function: OpenAICompatibleFunctionDefinition,
    #[serde(rename = "type")]
    pub tool_type: String, // always "function"
}

pub fn create_request_tool(
    name: &str,
    description: &str,
    parameters: serde_json::Value,
) -> OpenAICompatibleRequestTool {
    OpenAICompatibleRequestTool {
        function: OpenAICompatibleFunctionDefinition {
            description: description.to_string(),
            name: name.to_string(),
            parameters,
        },
        tool_type: "function".to_string(),
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OpenAICompatibleToolCallFunction {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub arguments: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OpenAICompatibleToolCall {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub function: Option<OpenAICompatibleToolCallFunction>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(rename = "type")]
    pub tool_type: Option<String>, // "function"
}

// TS wire field names stay snake_case here (tool_choice, prompt_cache_key,
// reasoning_effort) — no camelCase rename, unlike the ChatMessage types.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TransportRequestPayload {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<String>, // "auto"
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<OpenAICompatibleRequestTool>>,
    pub messages: Vec<TransportRequestMessage>,
    pub model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_cache_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    pub stream: bool, // always false
}

pub const DEFAULT_CHAT_COMPLETIONS_URL: &str =
    "http://localhost:4100/v1/chat/completions";
pub const DEFAULT_CHAT_MODEL: &str = "llama3.1-8B";

// Tuned for small local models (llama 3.1 8B): decision rule first, few-shot
// examples instead of abstract instructions, numbered rules, and the no-tool
// rule repeated last (small models weight the start and end of the prompt).
pub const DEFAULT_SYSTEM_PROMPT: &str = concat!(
    "You are a coding assistant in a terminal chat editor.\n",
    "\n",
    "First decide: is the user chatting, or asking for work on the workspace?\n",
    "- Chatting (greetings, thanks, opinions, general questions you already know): answer in plain text. Do not call tools.\n",
    "- Work (inspect files, run commands, change code): call ONE tool, wait for its result, then answer in plain text.\n",
    "\n",
    "Rules:\n",
    "1. Never call a tool for a message you can answer directly.\n",
    "2. One tool call at a time. Stop as soon as you can answer.\n",
    "3. After a tool result, answer the user. Do not repeat the same tool call.\n",
    "4. Keep answers short and concrete.\n",
    "\n",
    "Examples:\n",
    "User: \"hi\" -> reply \"Hi! What are we working on?\" with no tool calls.\n",
    "User: \"what is a mutex?\" -> explain it directly, no tool calls.\n",
    "User: \"what does Array.map do in js?\" -> explain it directly, no tool calls. Questions about languages or concepts never need workspace tools.\n",
    "User: \"what does src/dev.ts do?\" -> call READ on src/dev.ts, then summarize it.\n",
    "User: \"run the tests\" -> call BASH with the test command, then report the result.\n",
    "\n",
    "If the message needs no workspace access, reply in plain text without tools."
);

fn truncate_text(value: &str, max_length: usize) -> String {
    if value.chars().count() <= max_length {
        return value.to_string();
    }

    let keep = max_length.saturating_sub(3);
    let head: String = value.chars().take(keep).collect();

    format!("{head}...")
}

fn serialize_message_for_transport(message: &ChatMessage) -> String {
    serialize_chat_message_for_context(message)
}

pub fn normalize_openai_compatible_tool_call(
    tool_call: OpenAICompatibleToolCall,
) -> OpenAICompatibleToolCall {
    OpenAICompatibleToolCall {
        function: tool_call.function.map(|function| {
            OpenAICompatibleToolCallFunction {
                arguments: function.arguments,
                name: function.name,
            }
        }),
        id: tool_call.id,
        tool_type: Some("function".to_string()),
    }
}

fn build_history_transport_message(message: &ChatMessage) -> TransportRequestMessage {
    let transport_state = message.transport_state.as_ref();

    if message.role == ChatRoleTag::Assistant {
        let assistant_tool_calls = transport_state
            .and_then(|state| state.assistant_tool_calls.as_ref())
            .filter(|tool_calls| !tool_calls.is_empty());

        if let Some(assistant_tool_calls) = assistant_tool_calls {
            let serialized = serialize_message_for_transport(message);

            return TransportRequestMessage {
                content: Some(if serialized.is_empty() {
                    TransportContent::Parts(Vec::new())
                } else {
                    TransportContent::Text(serialized)
                }),
                name: None,
                role: ChatRoleTag::Assistant,
                tool_call_id: None,
                tool_calls: Some(assistant_tool_calls
                    .iter()
                    .map(|tool_call| {
                        normalize_openai_compatible_tool_call(
                            OpenAICompatibleToolCall {
                                function: Some(OpenAICompatibleToolCallFunction {
                                    arguments: tool_call.input.clone(),
                                    name: Some(tool_call.tool_name.clone()),
                                }),
                                id: Some(tool_call.id.clone()),
                                tool_type: None,
                            },
                        )
                    })
                    .collect()),
                anthropic_content: None,
            };
        }
    }

    if message.role == ChatRoleTag::Tool {
        return TransportRequestMessage {
            content: Some(TransportContent::Text(serialize_message_for_transport(
                message,
            ))),
            name: transport_state.and_then(|state| state.tool_name.clone()),
            role: ChatRoleTag::Tool,
            tool_call_id: transport_state.and_then(|state| state.tool_call_id.clone()),
            tool_calls: None,
            anthropic_content: None,
        };
    }

    TransportRequestMessage {
        content: Some(TransportContent::Text(serialize_message_for_transport(
            message,
        ))),
        name: None,
        role: message.role,
        tool_call_id: None,
        tool_calls: None,
        anthropic_content: None,
    }
}

fn build_context_message(context: &ChatRuntimeContext) -> String {
    let mut parts = vec![format!("cwd: {}", context.cwd)];

    if context.working_file.scope == WorkingFileScope::File {
        parts.push(format!("working_file: {}", context.working_file.path));
        parts.push(format!(
            "working_file_exists: {}",
            if context.working_file.exists { "true" } else { "false" }
        ));

        if let Some(text) = &context.working_file.text {
            parts.push(format!(
                "working_file_text:\n{}",
                truncate_text(text, 4000)
            ));
        }
    } else {
        parts.push("workspace_scope: cwd".to_string());
        parts.push("working_file: none".to_string());
    }

    parts.join("\n")
}

pub fn build_transport_messages(
    history: &[ChatMessage],
    system_prompt: &str,
    context: &ChatRuntimeContext,
) -> Vec<TransportRequestMessage> {
    let mut messages = vec![
        TransportRequestMessage {
            content: Some(TransportContent::Text(system_prompt.to_string())),
            name: None,
            role: ChatRoleTag::System,
            tool_call_id: None,
            tool_calls: None,
            anthropic_content: None,
        },
        TransportRequestMessage {
            content: Some(TransportContent::Text(build_context_message(context))),
            name: None,
            role: ChatRoleTag::System,
            tool_call_id: None,
            tool_calls: None,
            anthropic_content: None,
        },
    ];

    for message in history {
        if message.pending == Some(true) {
            continue;
        }

        messages.push(build_history_transport_message(message));
    }

    messages
}

pub struct BuildTransportRequestPayloadArgs {
    pub messages: Vec<TransportRequestMessage>,
    pub model: String,
    pub prompt_cache_key: Option<String>,
    pub reasoning_effort: Option<String>,
    pub tools: Option<Vec<OpenAICompatibleRequestTool>>,
}

pub fn build_transport_request_payload(
    args: BuildTransportRequestPayloadArgs,
) -> TransportRequestPayload {
    let (tool_choice, tools) = match &args.tools {
        Some(tools) if !tools.is_empty() => {
            (Some("auto".to_string()), Some(tools.clone()))
        }
        _ => (None, None),
    };

    // anthropicContent is native-route bookkeeping, not wire format; strict
    // OpenAI-compatible endpoints can reject unknown message fields.
    let messages = args
        .messages
        .into_iter()
        .map(|mut message| {
            if message.anthropic_content.is_some() {
                message.anthropic_content = None;
            }

            message
        })
        .collect();

    TransportRequestPayload {
        tool_choice,
        tools,
        messages,
        model: args.model,
        prompt_cache_key: args.prompt_cache_key.filter(|key| !key.is_empty()),
        reasoning_effort: args
            .reasoning_effort
            .map(|effort| effort.trim().to_string())
            .filter(|effort| !effort.is_empty()),
        stream: false,
    }
}

pub struct CreateTransportRequestPreviewArgs {
    pub context: ChatRuntimeContext,
    pub history: Vec<ChatMessage>,
    pub model: Option<String>,
    pub reasoning_effort: Option<String>,
    pub system_prompt: Option<String>,
    pub tools: Option<Vec<OpenAICompatibleRequestTool>>,
}

pub fn create_transport_request_preview(
    args: CreateTransportRequestPreviewArgs,
) -> TransportRequestPayload {
    let system_prompt =
        args.system_prompt.unwrap_or_else(|| DEFAULT_SYSTEM_PROMPT.to_string());
    let model = args.model.unwrap_or_else(|| DEFAULT_CHAT_MODEL.to_string());

    build_transport_request_payload(BuildTransportRequestPayloadArgs {
        messages: build_transport_messages(
            &args.history,
            &system_prompt,
            &args.context,
        ),
        model,
        prompt_cache_key: None,
        reasoning_effort: args.reasoning_effort,
        tools: args.tools,
    })
}

pub fn estimate_transport_payload_tokens(payload: &TransportRequestPayload) -> u64 {
    let serialized_payload =
        serde_json::to_string(payload).expect("payload serializes");

    std::cmp::max(1, (serialized_payload.chars().count() as f64 / 4.0).ceil() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::chat_types::{
        ChatMessage, ChatRoleTag, TextBlock, WorkingFileContext, WorkingFileScope,
    };

    fn text_message(role: ChatRoleTag, text: &str) -> ChatMessage {
        ChatMessage {
            blocks: vec![ChatMessageBlock::Text(TextBlock {
                context_state: None,
                tags: None,
                text: text.to_string(),
            })],
            context_files: None,
            context_state: None,
            created_at: None,
            failed: None,
            id: "m1".to_string(),
            pending: None,
            reply_to_message_id: None,
            role,
            tags: None,
            transport_state: None,
        }
    }

    use crate::harness::chat_types::ChatMessageBlock;

    #[test]
    fn build_multimodal_user_content_plain_text() {
        assert_eq!(
            build_multimodal_user_content("hello", &[]),
            TransportContent::Text("hello".to_string())
        );
    }

    #[test]
    fn build_multimodal_user_content_with_images() {
        let content = build_multimodal_user_content(
            "see this",
            &["data:image/png;base64,AAA".to_string()],
        );

        assert_eq!(
            content,
            TransportContent::Parts(vec![
                TransportContentPart::Text {
                    text: "see this".to_string(),
                },
                TransportContentPart::ImageUrl {
                    image_url: TransportImageUrl {
                        url: "data:image/png;base64,AAA".to_string(),
                    },
                },
            ])
        );
    }

    #[test]
    fn transport_content_to_text_renders_image_placeholders() {
        let content = build_multimodal_user_content(
            "look",
            &["data:image/png;base64,0123456789abcdefghijklmnopqrstuvwxyz".to_string()],
        );

        let text = transport_content_to_text(Some(&content));

        // TS: `url.slice(0, 40)` — exactly 40 characters of the data URL.
        assert_eq!(
            text,
            "look\n[image: data:image/png;base64,0123456789abcdefgh...]"
        );
        assert_eq!(transport_content_to_text(None), "");
    }

    #[test]
    fn normalize_tool_call_always_sets_type_function() {
        let normalized = normalize_openai_compatible_tool_call(
            OpenAICompatibleToolCall {
                function: Some(OpenAICompatibleToolCallFunction {
                    arguments: Some("{\"a\":1}".to_string()),
                    name: Some("READ".to_string()),
                }),
                id: Some("call_1".to_string()),
                tool_type: None,
            },
        );

        assert_eq!(normalized.tool_type.as_deref(), Some("function"));
        assert_eq!(normalized.id.as_deref(), Some("call_1"));
        assert_eq!(
            normalized.function.as_ref().unwrap().arguments.as_deref(),
            Some("{\"a\":1}")
        );

        let bare = normalize_openai_compatible_tool_call(OpenAICompatibleToolCall {
            function: None,
            id: None,
            tool_type: None,
        });

        assert_eq!(bare.tool_type.as_deref(), Some("function"));
        assert_eq!(bare.function, None);
        assert_eq!(bare.id, None);
    }

    #[test]
    fn payload_omits_optional_fields_and_strips_anthropic_content() {
        let message = TransportRequestMessage {
            anthropic_content: Some(vec![serde_json::json!({"type": "text"})]),
            content: Some(TransportContent::Text("hi".to_string())),
            name: None,
            role: ChatRoleTag::User,
            tool_call_id: None,
            tool_calls: None,
        };

        let payload = build_transport_request_payload(BuildTransportRequestPayloadArgs {
            messages: vec![message],
            model: "llama3.1-8B".to_string(),
            prompt_cache_key: Some("key".to_string()),
            reasoning_effort: Some("  high  ".to_string()),
            tools: None,
        });

        let json = serde_json::to_value(&payload).unwrap();

        assert_eq!(json["model"], "llama3.1-8B");
        assert_eq!(json["prompt_cache_key"], "key");
        assert_eq!(json["reasoning_effort"], "high");
        assert_eq!(json["stream"], false);
        assert!(json.get("tool_choice").is_none());
        assert!(json.get("tools").is_none());
        assert!(json["messages"][0].get("anthropicContent").is_none());
        assert!(json["messages"][0].get("anthropic_content").is_none());

        // Empty prompt_cache_key / blank reasoning effort are omitted, like the
        // TS truthiness checks.
        let payload = build_transport_request_payload(BuildTransportRequestPayloadArgs {
            messages: Vec::new(),
            model: "m".to_string(),
            prompt_cache_key: Some(String::new()),
            reasoning_effort: Some("   ".to_string()),
            tools: None,
        });

        let json = serde_json::to_value(&payload).unwrap();

        assert!(json.get("prompt_cache_key").is_none());
        assert!(json.get("reasoning_effort").is_none());
    }

    #[test]
    fn payload_includes_tools_with_auto_choice() {
        let payload = build_transport_request_payload(BuildTransportRequestPayloadArgs {
            messages: Vec::new(),
            model: "m".to_string(),
            prompt_cache_key: None,
            reasoning_effort: None,
            tools: Some(vec![create_request_tool(
                "READ",
                "read a file",
                serde_json::json!({"type": "object"}),
            )]),
        });

        let json = serde_json::to_value(&payload).unwrap();

        assert_eq!(json["tool_choice"], "auto");
        assert_eq!(json["tools"][0]["type"], "function");
        assert_eq!(json["tools"][0]["function"]["name"], "READ");
    }

    #[test]
    fn token_estimate_is_chars_over_four_at_least_one() {
        let payload = build_transport_request_payload(BuildTransportRequestPayloadArgs {
            messages: Vec::new(),
            model: "ab".to_string(),
            prompt_cache_key: None,
            reasoning_effort: None,
            tools: None,
        });

        let serialized = serde_json::to_string(&payload).unwrap();
        let expected =
            std::cmp::max(1, (serialized.chars().count() as f64 / 4.0).ceil() as u64);

        assert_eq!(estimate_transport_payload_tokens(&payload), expected);
    }

    #[test]
    fn system_prompt_matches_ts_constant() {
        assert!(DEFAULT_SYSTEM_PROMPT
            .starts_with("You are a coding assistant in a terminal chat editor."));
        assert!(DEFAULT_SYSTEM_PROMPT
            .ends_with("If the message needs no workspace access, reply in plain text without tools."));
    }

    #[test]
    fn build_transport_messages_leads_with_system_and_skips_pending() {
        let context = ChatRuntimeContext {
            cwd: "/repo".to_string(),
            working_file: WorkingFileContext {
                exists: true,
                path: "/repo/src/a.ts".to_string(),
                scope: WorkingFileScope::File,
                text: Some("fn main() {}".to_string()),
            },
        };

        let mut pending = text_message(ChatRoleTag::User, "ghost");
        pending.pending = Some(true);

        let history = vec![text_message(ChatRoleTag::User, "hello"), pending];

        let messages = build_transport_messages(&history, "sys", &context);

        assert_eq!(messages.len(), 3);
        assert_eq!(messages[0].role, ChatRoleTag::System);
        assert_eq!(
            transport_content_to_text(messages[0].content.as_ref()),
            "sys"
        );
        assert!(messages[1]
            .content
            .as_ref()
            .map(|content| transport_content_to_text(Some(content)))
            .unwrap()
            .contains("cwd: /repo"));
        assert!(messages[1]
            .content
            .as_ref()
            .map(|content| transport_content_to_text(Some(content)))
            .unwrap()
            .contains("working_file_text:\nfn main() {}"));
        assert_eq!(
            transport_content_to_text(messages[2].content.as_ref()),
            "hello"
        );
    }

    #[test]
    fn context_message_workspace_scope() {
        let context = ChatRuntimeContext {
            cwd: "/repo".to_string(),
            working_file: WorkingFileContext {
                exists: false,
                path: String::new(),
                scope: WorkingFileScope::Cwd,
                text: None,
            },
        };

        let message = build_context_message(&context);

        assert_eq!(message, "cwd: /repo\nworkspace_scope: cwd\nworking_file: none");
    }
}

#[cfg(test)]
mod wire_field_names {
    use super::*;

    #[test]
    fn tool_messages_keep_snake_case_wire_keys() {
        let message = TransportRequestMessage {
            role: ChatRoleTag::Tool,
            content: Some(TransportContent::Text("ok".to_string())),
            name: Some("READ".to_string()),
            tool_call_id: Some("call_1".to_string()),
            tool_calls: Some(vec![OpenAICompatibleToolCall {
                id: Some("call_1".to_string()),
                tool_type: Some("function".to_string()),
                function: Some(OpenAICompatibleToolCallFunction {
                    name: Some("READ".to_string()),
                    arguments: Some("{}".to_string()),
                }),
            }]),
            ..Default::default()
        };
        let json = serde_json::to_value(&message).unwrap();
        assert!(json.get("tool_call_id").is_some(), "{json}");
        assert!(json.get("tool_calls").is_some(), "{json}");
        assert!(json.get("toolCallId").is_none() && json.get("toolCalls").is_none(), "{json}");
        assert_eq!(json["tool_calls"][0]["type"], "function");
    }
}
