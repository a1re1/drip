// Settings are a flat map of string -> string (IndexMap keeps JSON object key
// order stable across load/save). Profile lists are stored as JSON-string-
// encoded settings values. The shipped defaults are embedded byte-exact.
#![allow(non_snake_case)]

use anyhow::{anyhow, bail, Result};
use std::collections::HashSet;
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::{Path, PathBuf};

// ---------------------------------------------------------------------------
// Setting ids
// ---------------------------------------------------------------------------

pub const ACTIVE_INFERENCE_PROFILE_SETTING_ID: &str = "runtime.active_profile_id";
pub const ACTIVE_TOOL_PROFILE_SETTING_ID: &str = "runtime.active_tool_profile_id";
pub const ACTIVE_SYSTEM_PROMPT_PROFILE_SETTING_ID: &str = "runtime.active_system_prompt_id";
pub const MODEL_PROFILES_SETTING_ID: &str = "runtime.model_profiles";
pub const SYSTEM_PROMPT_PROFILES_SETTING_ID: &str = "runtime.system_prompt_profiles";
pub const STORED_API_KEYS_SETTING_ID: &str = "credentials.stored_api_keys";
pub const CEREBRAS_API_KEY_SETTING_ID: &str = "runtime.cerebras_api_key";
pub const ROLE_PROFILES_SETTING_ID: &str = "runtime.role_profiles";
pub const ROLE_BINDINGS_SETTING_ID: &str = "runtime.role_bindings";
pub const DEFAULT_MAX_CONTEXT_TOKENS: i64 = 64000;
pub const OPENAI_REASONING_EFFORT_VALUES: [&str; 5] = ["none", "low", "medium", "high", "xhigh"];

// Byte-exact shipped defaults.
pub const MODEL_PROFILES_DEFAULT_JSON: &str = include_str!("defaults/model_profiles.json");
pub const SYSTEM_PROMPT_PROFILES_DEFAULT_JSON: &str =
    include_str!("defaults/system_prompt_profiles.json");
pub const STORED_API_KEYS_DEFAULT_JSON: &str = include_str!("defaults/stored_api_keys.json");
pub const OTHER_SETTINGS_DEFAULT_JSON: &str = include_str!("defaults/other_settings.json");

// ---------------------------------------------------------------------------
// InferenceProviderId
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum InferenceProviderId {
    #[serde(rename = "cerebras")]
    Cerebras,
    #[serde(rename = "claude")]
    Claude,
    #[serde(rename = "codex")]
    Codex,
    #[serde(rename = "gemini")]
    Gemini,
    #[serde(rename = "ollama")]
    Ollama,
    #[serde(rename = "openai")]
    OpenAi,
    #[serde(rename = "openai-compatible")]
    OpenAiCompatible,
    #[serde(rename = "openrouter")]
    OpenRouter,
    #[serde(rename = "xai")]
    Xai,
}

impl InferenceProviderId {
    pub fn as_str(&self) -> &'static str {
        match self {
            InferenceProviderId::Cerebras => "cerebras",
            InferenceProviderId::Claude => "claude",
            InferenceProviderId::Codex => "codex",
            InferenceProviderId::Gemini => "gemini",
            InferenceProviderId::Ollama => "ollama",
            InferenceProviderId::OpenAi => "openai",
            InferenceProviderId::OpenAiCompatible => "openai-compatible",
            InferenceProviderId::OpenRouter => "openrouter",
            InferenceProviderId::Xai => "xai",
        }
    }

    pub fn parse(raw: &str) -> Option<InferenceProviderId> {
        Some(match raw {
            "cerebras" => InferenceProviderId::Cerebras,
            "claude" => InferenceProviderId::Claude,
            "codex" => InferenceProviderId::Codex,
            "gemini" => InferenceProviderId::Gemini,
            "ollama" => InferenceProviderId::Ollama,
            "openai" => InferenceProviderId::OpenAi,
            "openai-compatible" => InferenceProviderId::OpenAiCompatible,
            "openrouter" => InferenceProviderId::OpenRouter,
            "xai" => InferenceProviderId::Xai,
            _ => return None,
        })
    }
}

// ---------------------------------------------------------------------------
// default_setting_values()
// ---------------------------------------------------------------------------

/// The shipped defaults as a settings map, with the profile lists
/// JSON-string-encoded.
pub fn default_setting_values() -> IndexMap<String, String> {
    let mut settings: IndexMap<String, String> =
        serde_json::from_str(OTHER_SETTINGS_DEFAULT_JSON).expect("embedded other_settings parses");
    settings.insert(
        MODEL_PROFILES_SETTING_ID.to_string(),
        MODEL_PROFILES_DEFAULT_JSON.to_string(),
    );
    settings.insert(
        SYSTEM_PROMPT_PROFILES_SETTING_ID.to_string(),
        SYSTEM_PROMPT_PROFILES_DEFAULT_JSON.to_string(),
    );
    settings.insert(
        STORED_API_KEYS_SETTING_ID.to_string(),
        STORED_API_KEYS_DEFAULT_JSON.to_string(),
    );
    settings
}

// ---------------------------------------------------------------------------
// Profile types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InferenceModelProfile {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub provider: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    // Stored as a number or a numeric string ("64000"); accept both on read.
    #[serde(default, deserialize_with = "de_max_context_tokens", skip_serializing_if = "Option::is_none")]
    pub max_context_tokens: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    // Id of another profile to retry against when this profile's gateway fails.
    // Deliberately a loose reference: the named profile may not exist (a user
    // can delete it), and that must degrade to "no fallback" rather than break
    // parsing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback_profile_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headers: Option<IndexMap<String, String>>,
}

fn de_max_context_tokens<'de, D>(deserializer: D) -> Result<Option<i64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value: Option<Value> = Option::deserialize(deserializer)?;
    Ok(match value {
        None | Some(Value::Null) => None,
        Some(Value::Number(n)) => n.as_i64(),
        Some(Value::String(s)) => s.trim().parse::<i64>().ok(),
        Some(_) => None,
    })
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StoredApiKeyEntry {
    #[serde(default)]
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default)]
    pub value: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SystemPromptProfile {
    #[serde(default)]
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default)]
    pub prompt: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_access: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_names: Option<Vec<String>>,
}

// ---------------------------------------------------------------------------
// normalize helpers
// ---------------------------------------------------------------------------

fn str_field<'a>(obj: &'a serde_json::Map<String, Value>, key: &str) -> Option<&'a str> {
    obj.get(key).and_then(Value::as_str)
}

pub fn normalize_model_profile(value: &Value, index: usize) -> Result<InferenceModelProfile> {
    let obj = value
        .as_object()
        .ok_or_else(|| anyhow!("Model profile #{} is not an object.", index + 1))?;
    let id = str_field(obj, "id").unwrap_or("");
    if id.is_empty() {
        return Err(anyhow!("Model profile #{} is missing an id.", index + 1));
    }
    let model = str_field(obj, "model").unwrap_or("");
    if model.is_empty() {
        return Err(anyhow!("Inference profile \"{}\" is missing a model.", id));
    }
    let provider = str_field(obj, "provider").unwrap_or("");
    if InferenceProviderId::parse(provider).is_none() {
        return Err(anyhow!(
            "Inference profile \"{}\" has an unsupported provider \"{}\".",
            id,
            provider
        ));
    }
    // A number or a numeric string, positive integer only; anything else
    // is `Inference profile "<id>" must use a positive integer for max context tokens.`
    let max_context_tokens = match obj.get("maxContextTokens") {
        None | Some(Value::Null) => None,
        Some(Value::Number(n)) => match n.as_i64() {
            Some(v) if v > 0 && n.as_f64().map_or(true, |f| f.fract() == 0.0) => Some(v),
            _ => return Err(anyhow!(
                "Inference profile \"{}\" must use a positive integer for max context tokens.",
                id
            )),
        },
        Some(Value::String(s)) => {
            let trimmed = s.trim();
            if trimmed.is_empty() {
                None
            } else {
                match trimmed.parse::<i64>() {
                    Ok(v) if v > 0 && !trimmed.starts_with('+') => Some(v),
                    _ => return Err(anyhow!(
                        "Inference profile \"{}\" must use a positive integer for max context tokens.",
                        id
                    )),
                }
            }
        }
        Some(_) => {
            return Err(anyhow!(
                "Inference profile \"{}\" must use a positive integer for max context tokens.",
                id
            ))
        }
    };
    let reasoning_effort = str_field(obj, "reasoningEffort")
        .filter(|value| OPENAI_REASONING_EFFORT_VALUES.contains(value))
        .map(|value| value.to_string());
    let headers = match obj.get("headers") {
        None => None,
        Some(Value::Object(map)) => {
            let mut headers = IndexMap::new();
            for (key, value) in map {
                // Blank values are dropped, not sent.
                if let Some(value) = value.as_str().filter(|value| !value.trim().is_empty()) {
                    headers.insert(key.clone(), value.to_string());
                }
            }
            if headers.is_empty() {
                None
            } else {
                Some(headers)
            }
        }
        Some(_) => bail!("Inference profile \"{id}\" must use an object for headers."),
    };
    let base_url = str_field(obj, "baseUrl")
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(|v| v.to_string());
    let api_key = str_field(obj, "apiKey")
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(|v| v.to_string());
    let api_key_ref = str_field(obj, "apiKeyRef")
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(|v| v.to_string());
    if provider == "codex" {
        // Codex never talks HTTP: the profile routes through the local
        // `codex app-server` bridge and the ChatGPT login is the credential.
        // HTTP billing inputs are rejected loudly instead of being silently
        // charged to the OpenAI API.
        let conflicts: Vec<&str> = [
            ("apiKey", api_key.is_some()),
            ("apiKeyRef", api_key_ref.is_some()),
            ("baseUrl", base_url.is_some()),
            ("headers", headers.is_some()),
        ]
        .into_iter()
        .filter(|(_, present)| *present)
        .map(|(field, _)| field)
        .collect();
        if !conflicts.is_empty() {
            return Err(anyhow!(
                "Inference profile \"{}\" uses provider \"codex\", which runs the local codex \
                 app-server (ChatGPT login); remove {} — HTTP credentials and endpoints would \
                 bill the OpenAI API instead.",
                id,
                conflicts.join(", ")
            ));
        }
    }
    Ok(InferenceModelProfile {
        id: id.to_string(),
        model: model.to_string(),
        provider: provider.to_string(),
        base_url,
        label: str_field(obj, "label")
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(|v| v.to_string()),
        max_context_tokens,
        api_key,
        api_key_ref,
        reasoning_effort,
        fallback_profile_id: str_field(obj, "fallbackProfileId")
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(|v| v.to_string()),
        headers,
    })
}

pub fn normalize_system_prompt_profile(
    value: &Value,
    index: usize,
) -> Result<SystemPromptProfile> {
    let obj = value
        .as_object()
        .ok_or_else(|| anyhow!("System prompt profile #{} is not an object.", index + 1))?;
    let id = str_field(obj, "id").unwrap_or("");
    if id.is_empty() {
        return Err(anyhow!("System prompt profile #{} is missing an id.", index + 1));
    }
    let prompt = str_field(obj, "prompt").unwrap_or("");
    if prompt.is_empty() {
        return Err(anyhow!("System prompt profile \"{}\" is missing a prompt.", id));
    }
    let tool_access = str_field(obj, "toolAccess")
        .filter(|value| *value == "all" || *value == "selected")
        .map(|value| value.to_string());
    let tool_names = match obj.get("toolNames") {
        Some(Value::Array(items)) if !items.is_empty() => Some(
            items
                .iter()
                .filter_map(Value::as_str)
                .map(|v| v.to_string())
                .collect(),
        ),
        _ => None,
    };
    Ok(SystemPromptProfile {
        id: id.to_string(),
        label: str_field(obj, "label").map(|v| v.to_string()),
        prompt: prompt.to_string(),
        tool_access,
        tool_names,
    })
}

/// Decodes a JSON-string-encoded settings value into an array; the error
/// messages are part of the contract.
pub fn parse_settings_json_array(label: &str, raw: &str) -> Result<Vec<Value>> {
    let parsed: Value = serde_json::from_str(raw)
        .map_err(|_| anyhow!("{} must be a valid JSON array.", label))?;
    match parsed {
        Value::Array(items) => Ok(items),
        _ => Err(anyhow!("{} must be a JSON array.", label)),
    }
}

fn check_duplicate_ids(ids: &[String], label: &str) -> Result<()> {
    let mut seen = HashSet::new();
    for id in ids {
        if !seen.insert(id.clone()) {
            // Same message shape for every profile kind (model, system prompt,
            // stored API key).
            return Err(anyhow!("{} \"{}\" is duplicated.", label, id));
        }
    }
    Ok(())
}

pub fn parse_inference_model_profiles(
    settings: &IndexMap<String, String>,
) -> Result<Vec<InferenceModelProfile>> {
    let raw = settings
        .get(MODEL_PROFILES_SETTING_ID)
        .map(String::as_str)
        .unwrap_or(MODEL_PROFILES_DEFAULT_JSON);
    let raw = if raw.trim().is_empty() {
        MODEL_PROFILES_DEFAULT_JSON
    } else {
        raw
    };
    let items = parse_settings_json_array("Model Profiles", raw)?;
    let mut profiles = Vec::new();
    let mut ids = Vec::new();
    for (index, item) in items.iter().enumerate() {
        let profile = normalize_model_profile(item, index)?;
        ids.push(profile.id.clone());
        profiles.push(profile);
    }
    check_duplicate_ids(&ids, "Inference profile")?;
    Ok(profiles)
}

pub fn parse_system_prompt_profiles(
    settings: &IndexMap<String, String>,
) -> Result<Vec<SystemPromptProfile>> {
    let raw = settings
        .get(SYSTEM_PROMPT_PROFILES_SETTING_ID)
        .map(String::as_str)
        .unwrap_or(SYSTEM_PROMPT_PROFILES_DEFAULT_JSON);
    let raw = if raw.trim().is_empty() {
        SYSTEM_PROMPT_PROFILES_DEFAULT_JSON
    } else {
        raw
    };
    let items = parse_settings_json_array("System Prompt Profiles", raw)?;
    let mut profiles = Vec::new();
    let mut ids = Vec::new();
    for (index, item) in items.iter().enumerate() {
        let profile = normalize_system_prompt_profile(item, index)?;
        ids.push(profile.id.clone());
        profiles.push(profile);
    }
    check_duplicate_ids(&ids, "System prompt profile")?;
    Ok(profiles)
}

pub fn parse_stored_api_key_entries(
    settings: &IndexMap<String, String>,
) -> Result<Vec<StoredApiKeyEntry>> {
    let raw = settings
        .get(STORED_API_KEYS_SETTING_ID)
        .map(String::as_str)
        .unwrap_or(STORED_API_KEYS_DEFAULT_JSON);
    let raw = if raw.trim().is_empty() {
        STORED_API_KEYS_DEFAULT_JSON
    } else {
        raw
    };
    let items = parse_settings_json_array("Stored API Keys", raw)?;
    let mut entries = Vec::new();
    let mut ids = Vec::new();
    for (index, item) in items.iter().enumerate() {
        let obj = item.as_object().ok_or_else(|| {
            anyhow!("Stored API key entry #{} is not an object.", index + 1)
        })?;
        let id = str_field(obj, "id").unwrap_or("").to_string();
        if id.is_empty() {
            return Err(anyhow!("Stored API key entry #{} is missing an id.", index + 1));
        }
        let value = str_field(obj, "value").unwrap_or("").to_string();
        ids.push(id.clone());
        entries.push(StoredApiKeyEntry {
            id,
            label: str_field(obj, "label").map(|v| v.to_string()),
            value,
        });
    }
    check_duplicate_ids(&ids, "Stored API key")?;
    Ok(entries)
}

// ---------------------------------------------------------------------------
// Editable profiles
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, PartialEq)]
pub struct EditableInferenceModelProfile {
    pub id: String,
    pub model: String,
    pub provider: String,
    pub base_url: String,
    pub label: String,
    pub max_context_tokens: String,
    pub reasoning_effort: String,
    pub credential_mode: String,
    pub credential_value: String,
    pub fallback_profile_id: Option<String>,
    pub headers: Option<IndexMap<String, String>>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct EditableSystemPromptProfile {
    pub id: String,
    pub label: String,
    pub prompt: String,
    pub tool_access: String,
    pub tool_names: Option<Vec<String>>,
}

pub fn parse_editable_inference_model_profiles(
    raw_value: Option<&str>,
) -> Result<Vec<EditableInferenceModelProfile>> {
    let value = raw_value.unwrap_or("").trim();
    if value.is_empty() {
        return Ok(Vec::new());
    }
    let items = parse_settings_json_array("Model Profiles", value)?;
    Ok(items
        .iter()
        .map(|item| {
            let obj = item.as_object().cloned().unwrap_or_default();
            let provider = str_field(&obj, "provider")
                .filter(|p| InferenceProviderId::parse(p).is_some())
                .unwrap_or("openai-compatible")
                .to_string();
            let api_key = str_field(&obj, "apiKey")
                .map(str::trim)
                .filter(|v| !v.is_empty());
            let api_key_ref = str_field(&obj, "apiKeyRef")
                .map(str::trim)
                .filter(|v| !v.is_empty());
            // inferEditableCredential(): inline -> credentialValue, env -> "env:NAME".
            let (credential_mode, credential_value) = if let Some(key) = api_key {
                ("inline", key.to_string())
            } else if let Some(reference) = api_key_ref {
                if let Some(name) = reference.strip_prefix("env:") {
                    ("env", name.to_string())
                } else {
                    ("none", String::new())
                }
            } else {
                ("none", String::new())
            };
            EditableInferenceModelProfile {
                id: str_field(&obj, "id").unwrap_or("").to_string(),
                model: str_field(&obj, "model").unwrap_or("").to_string(),
                provider,
                base_url: str_field(&obj, "baseUrl").unwrap_or("").to_string(),
                label: str_field(&obj, "label").unwrap_or("").to_string(),
                max_context_tokens: match obj.get("maxContextTokens") {
                    Some(Value::Number(n)) => n.to_string(),
                    Some(Value::String(s)) => s.clone(),
                    _ => String::new(),
                },
                reasoning_effort: str_field(&obj, "reasoningEffort")
                    .unwrap_or("")
                    .to_string(),
                credential_mode: credential_mode.to_string(),
                credential_value,
                fallback_profile_id: str_field(&obj, "fallbackProfileId")
                    .map(str::trim)
                    .filter(|v| !v.is_empty())
                    .map(|v| v.to_string()),
                headers: None,
            }
        })
        .collect())
}

pub fn serialize_editable_inference_model_profiles(
    profiles: &[EditableInferenceModelProfile],
) -> String {
    let items: Vec<Value> = profiles
        .iter()
        .map(|profile| {
            let mut map = serde_json::Map::new();
            map.insert("id".to_string(), Value::String(profile.id.clone()));
            map.insert("model".to_string(), Value::String(profile.model.clone()));
            map.insert("provider".to_string(), Value::String(profile.provider.clone()));
            if !profile.base_url.trim().is_empty() {
                map.insert(
                    "baseUrl".to_string(),
                    Value::String(profile.base_url.trim().to_string()),
                );
            }
            if !profile.label.trim().is_empty() {
                map.insert(
                    "label".to_string(),
                    Value::String(profile.label.trim().to_string()),
                );
            }
            if !profile.max_context_tokens.trim().is_empty() {
                map.insert(
                    "maxContextTokens".to_string(),
                    Value::String(profile.max_context_tokens.trim().to_string()),
                );
            }
            if !profile.reasoning_effort.trim().is_empty() {
                map.insert(
                    "reasoningEffort".to_string(),
                    Value::String(profile.reasoning_effort.trim().to_string()),
                );
            }
            if let Some(fallback) = profile
                .fallback_profile_id
                .as_ref()
                .map(|s| s.trim())
                .filter(|v| !v.is_empty())
            {
                map.insert(
                    "fallbackProfileId".to_string(),
                    Value::String(fallback.to_string()),
                );
            }
            let trimmed = profile.credential_value.trim();
            if profile.credential_mode == "inline" && !trimmed.is_empty() {
                map.insert("apiKey".to_string(), Value::String(trimmed.to_string()));
            }
            if profile.credential_mode == "env" && !trimmed.is_empty() {
                map.insert(
                    "apiKeyRef".to_string(),
                    Value::String(format!("env:{}", trimmed)),
                );
            }
            Value::Object(map)
        })
        .collect();
    serde_json::to_string_pretty(&items).unwrap_or_else(|_| "[]".to_string())
}

pub fn parse_editable_system_prompt_profiles(
    raw_value: Option<&str>,
) -> Result<Vec<EditableSystemPromptProfile>> {
    let value = raw_value.unwrap_or("").trim();
    if value.is_empty() {
        return Ok(Vec::new());
    }
    let items = parse_settings_json_array("System Prompt Profiles", value)?;
    Ok(items
        .iter()
        .map(|item| {
            let obj = item.as_object().cloned().unwrap_or_default();
            EditableSystemPromptProfile {
                id: str_field(&obj, "id").unwrap_or("").to_string(),
                label: str_field(&obj, "label").unwrap_or("").to_string(),
                prompt: str_field(&obj, "prompt").unwrap_or("").to_string(),
                tool_access: str_field(&obj, "toolAccess").unwrap_or("").to_string(),
                tool_names: match obj.get("toolNames") {
                    Some(Value::Array(items)) => Some(
                        items
                            .iter()
                            .filter_map(Value::as_str)
                            .map(|v| v.to_string())
                            .collect(),
                    ),
                    _ => None,
                },
            }
        })
        .collect())
}

pub fn serialize_editable_system_prompt_profiles(
    profiles: &[EditableSystemPromptProfile],
) -> String {
    let items: Vec<Value> = profiles
        .iter()
        .map(|profile| {
            let mut map = serde_json::Map::new();
            map.insert("id".to_string(), Value::String(profile.id.clone()));
            map.insert("label".to_string(), Value::String(profile.label.clone()));
            map.insert("prompt".to_string(), Value::String(profile.prompt.clone()));
            if !profile.tool_access.is_empty() {
                map.insert(
                    "toolAccess".to_string(),
                    Value::String(profile.tool_access.clone()),
                );
            }
            if let Some(tool_names) = profile.tool_names.as_ref().filter(|v| !v.is_empty()) {
                map.insert(
                    "toolNames".to_string(),
                    serde_json::to_value(tool_names).unwrap_or(Value::Array(Vec::new())),
                );
            }
            Value::Object(map)
        })
        .collect();
    serde_json::to_string_pretty(&items).unwrap_or_else(|_| "[]".to_string())
}

// ---------------------------------------------------------------------------
// merge_missing_default_* helpers
// ---------------------------------------------------------------------------

pub fn merge_missing_default_inference_profiles(raw_value: Option<&str>) -> String {
    let value = raw_value.unwrap_or("").trim();
    if value.is_empty() {
        return MODEL_PROFILES_DEFAULT_JSON.to_string();
    }
    let items = match serde_json::from_str::<Vec<Value>>(value) {
        Ok(items) => items,
        Err(_) => return value.to_string(),
    };
    let existing: Vec<String> = items
        .iter()
        .filter_map(|item| item.get("id").and_then(Value::as_str).map(String::from))
        .collect();
    let defaults: Vec<Value> =
        serde_json::from_str(MODEL_PROFILES_DEFAULT_JSON).unwrap_or_default();
    let mut merged = items;
    for default in defaults {
        let id = default.get("id").and_then(Value::as_str).unwrap_or("");
        if !existing.contains(&id.to_string()) {
            merged.push(default);
        }
    }
    serde_json::to_string(&merged).unwrap_or_else(|_| value.to_string())
}

pub fn merge_missing_default_system_prompt_profiles(raw_value: Option<&str>) -> String {
    let value = raw_value.unwrap_or("").trim();
    if value.is_empty() {
        return SYSTEM_PROMPT_PROFILES_DEFAULT_JSON.to_string();
    }
    let items = match serde_json::from_str::<Vec<Value>>(value) {
        Ok(items) => items,
        Err(_) => return value.to_string(),
    };
    let existing: Vec<String> = items
        .iter()
        .filter_map(|item| item.get("id").and_then(Value::as_str).map(String::from))
        .collect();
    let defaults: Vec<Value> =
        serde_json::from_str(SYSTEM_PROMPT_PROFILES_DEFAULT_JSON).unwrap_or_default();
    let mut merged = items;
    for default in defaults {
        let id = default.get("id").and_then(Value::as_str).unwrap_or("");
        if !existing.contains(&id.to_string()) {
            merged.push(default);
        }
    }
    serde_json::to_string(&merged).unwrap_or_else(|_| value.to_string())
}

// ---------------------------------------------------------------------------
// Custom status line (opt-in)
// ---------------------------------------------------------------------------

// The optional `statusLine` object in ~/.drip/config.json — shaped after
// Claude Code's status line so an existing script's shape carries over, but
// drip only ever reads its own config file: ~/.claude settings are never
// imported or executed. Absent or null keeps the built-in status bar, and a
// bad statusLine produces a nonfatal diagnostic — nothing here executes.
pub const STATUS_LINE_DEFAULT_UPDATE_INTERVAL_MS: u64 = 300;
pub const STATUS_LINE_MIN_UPDATE_INTERVAL_MS: u64 = 100;
pub const STATUS_LINE_MAX_UPDATE_INTERVAL_MS: u64 = 60_000;
pub const STATUS_LINE_DEFAULT_TIMEOUT_MS: u64 = 5_000;
pub const STATUS_LINE_MIN_TIMEOUT_MS: u64 = 100;
pub const STATUS_LINE_MAX_TIMEOUT_MS: u64 = 30_000;
pub const STATUS_LINE_MIN_PADDING: u16 = 0;
pub const STATUS_LINE_MAX_PADDING: u16 = 4;
pub const STATUS_LINE_MAX_COMMAND_CHARS: usize = 4096;

fn default_status_line_type() -> String {
    "command".to_string()
}

fn default_status_line_padding() -> u16 {
    0
}

fn default_status_line_update_interval_ms() -> u64 {
    STATUS_LINE_DEFAULT_UPDATE_INTERVAL_MS
}

fn default_status_line_timeout_ms() -> u64 {
    STATUS_LINE_DEFAULT_TIMEOUT_MS
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StatusLineSetting {
    #[serde(rename = "type", default = "default_status_line_type")]
    pub kind: String,
    pub command: String,
    #[serde(default = "default_status_line_padding")]
    pub padding: u16,
    #[serde(
        rename = "updateIntervalMs",
        default = "default_status_line_update_interval_ms"
    )]
    pub update_interval_ms: u64,
    #[serde(rename = "timeoutMs", default = "default_status_line_timeout_ms")]
    pub timeout_ms: u64,
}

impl Default for StatusLineSetting {
    fn default() -> Self {
        StatusLineSetting {
            kind: default_status_line_type(),
            command: String::new(),
            padding: default_status_line_padding(),
            update_interval_ms: default_status_line_update_interval_ms(),
            timeout_ms: default_status_line_timeout_ms(),
        }
    }
}

// validate_status_line(): the shared checks over a deserialized statusLine.
// Returns the cleaned setting (trimmed command, numeric bounds clamped to the
// documented ranges) plus human-readable warnings for every clamped value.
// Errors name the offending key so the caller can surface a useful,
// nonfatal diagnostic without executing anything.
pub fn validate_status_line(
    setting: &StatusLineSetting,
) -> Result<(StatusLineSetting, Vec<String>)> {
    let mut warnings = Vec::new();

    if setting.kind.trim() != "command" {
        bail!(
            "statusLine.type must be \"command\" (the only supported kind); got {:?}.",
            setting.kind
        );
    }

    let command = setting.command.trim();
    if command.is_empty() {
        bail!("statusLine.command must be a non-empty shell command.");
    }
    if command.chars().count() > STATUS_LINE_MAX_COMMAND_CHARS {
        bail!(
            "statusLine.command is longer than the supported {STATUS_LINE_MAX_COMMAND_CHARS} characters."
        );
    }

    let update_interval_ms = setting.update_interval_ms.clamp(
        STATUS_LINE_MIN_UPDATE_INTERVAL_MS,
        STATUS_LINE_MAX_UPDATE_INTERVAL_MS,
    );
    if update_interval_ms != setting.update_interval_ms {
        warnings.push(format!(
            "statusLine.updateIntervalMs clamped to {update_interval_ms} ms (supported range {}-{} ms).",
            STATUS_LINE_MIN_UPDATE_INTERVAL_MS, STATUS_LINE_MAX_UPDATE_INTERVAL_MS
        ));
    }

    let timeout_ms = setting
        .timeout_ms
        .clamp(STATUS_LINE_MIN_TIMEOUT_MS, STATUS_LINE_MAX_TIMEOUT_MS);
    if timeout_ms != setting.timeout_ms {
        warnings.push(format!(
            "statusLine.timeoutMs clamped to {timeout_ms} ms (supported range {}-{} ms).",
            STATUS_LINE_MIN_TIMEOUT_MS, STATUS_LINE_MAX_TIMEOUT_MS
        ));
    }

    let padding = setting.padding.clamp(STATUS_LINE_MIN_PADDING, STATUS_LINE_MAX_PADDING);
    if padding != setting.padding {
        warnings.push(format!(
            "statusLine.padding clamped to {padding} (supported range {}-{}).",
            STATUS_LINE_MIN_PADDING, STATUS_LINE_MAX_PADDING
        ));
    }

    Ok((
        StatusLineSetting {
            kind: "command".to_string(),
            command: command.to_string(),
            padding,
            update_interval_ms,
            timeout_ms,
        },
        warnings,
    ))
}

fn describe_status_line_value(value: &Value) -> &'static str {
    if value.is_string() {
        "a string"
    } else if value.is_array() {
        "an array"
    } else if value.is_number() || value.is_boolean() {
        "a scalar"
    } else {
        "an unexpected value"
    }
}

// parse_status_line_setting(raw): Ok(None) when statusLine is absent or null
// — the built-in status bar stays exactly as it is. Err carries a
// human-readable diagnostic; callers show it nonfatally and continue without
// a custom status line. Nothing here spawns a process.
pub fn parse_status_line_setting(
    raw: Option<&Value>,
) -> Result<(Option<StatusLineSetting>, Vec<String>)> {
    let Some(value) = raw else {
        return Ok((None, Vec::new()));
    };
    if value.is_null() {
        return Ok((None, Vec::new()));
    }
    if !value.is_object() {
        bail!(
            "statusLine must be an object like {{\"type\":\"command\",\"command\":\"...\"}}; found {}.",
            describe_status_line_value(value)
        );
    }
    if !value.get("command").map(Value::is_string).unwrap_or(false) {
        bail!("statusLine.command must be a non-empty shell command string.");
    }
    let setting: StatusLineSetting = serde_json::from_value(value.clone())
        .map_err(|error| anyhow!("statusLine is not a valid status-line configuration: {error}"))?;
    let (setting, warnings) = validate_status_line(&setting)?;
    Ok((Some(setting), warnings))
}

// ---------------------------------------------------------------------------
// CLI layer
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CliConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<PathBuf>,
    pub settings: IndexMap<String, String>,
    #[serde(rename = "statusLine", default, skip_serializing_if = "Option::is_none")]
    pub status_line: Option<StatusLineSetting>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<u32>,
}

pub fn create_default_cli_config() -> CliConfig {
    CliConfig {
        path: None,
        settings: default_setting_values(),
        status_line: None,
        version: Some(1),
    }
}

// upgradeCerebrasProfiles(): early builds stored the key inline on the profile
// (or not at all); once runtime.cerebras_api_key existed, profiles with no
// credential at all are upgraded to read the env key — but only when the user
// has not configured the legacy key setting themselves.
fn upgrade_cerebras_profiles(settings: &mut IndexMap<String, String>) {
    let mut profiles = match parse_editable_inference_model_profiles(
        settings.get(MODEL_PROFILES_SETTING_ID).map(String::as_str),
    ) {
        Ok(profiles) => profiles,
        Err(_) => return,
    };

    let legacy_key_configured = settings
        .get(CEREBRAS_API_KEY_SETTING_ID)
        .map(|value| !value.trim().is_empty())
        .unwrap_or(false);
    let mut changed = false;

    for profile in &mut profiles {
        if profile.provider == "cerebras"
            && profile.credential_mode == "none"
            && !legacy_key_configured
        {
            profile.credential_mode = "env".to_string();
            profile.credential_value = "CEREBRAS_API_KEY".to_string();
            changed = true;
        }
    }

    if changed {
        settings.insert(
            MODEL_PROFILES_SETTING_ID.to_string(),
            serialize_editable_inference_model_profiles(&profiles),
        );
    }
}

// v0.73–0.74 shipped `glm-5-3-flash` / `glm-5-3` / `kimi-k3` on FriendliAI and
// Baseten with hand-built fallback chains; v0.75 re-points the same ids at
// OpenRouter. mergeMissingDefaultInferenceProfiles only fills in ids a saved
// catalog lacks, so an upgraded config would keep the retired vendor route
// under the very ids every preset and --review pin — and the one-key setup
// would fail on a missing FRIENDLI_TOKEN. Re-point entries that still
// sit on one of those two hosts to the shipped profile of the same id. Only
// hosts a shipped default ever pointed those ids at qualify — Z.AI is not
// one (its routes shipped under `zai-*` ids), so a user who deliberately
// re-pointed a lane at api.z.ai keeps it, as does anyone on any other host.
const RETIRED_VENDOR_HOSTS: [&str; 2] = ["api.friendli.ai", "inference.baseten.co"];

// new URL(...).hostname semantics: scheme required, authority ends at the
// first "/", "?" or "#"; userinfo and port are stripped; the host is
// lowercased. Unparseable input yields None (isRetiredVendorHost → false).
fn url_hostname(url: &str) -> Option<String> {
    let after_scheme = url.split_once("://")?.1;
    // Authority runs until the first "/", "?" or "#".
    let authority = {
        let end = after_scheme
            .find(['/', '?', '#'])
            .unwrap_or(after_scheme.len());
        &after_scheme[..end]
    };
    // Userinfo ends at the last "@" in the authority; the port is the last
    // ":" after that (IPv6 hosts keep their brackets, which never contain a
    // bare ":" in these host strings).
    let host = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    let host = host.rsplit_once(':').map_or(host, |(h, _)| h);
    Some(host.to_ascii_lowercase())
}

fn is_retired_vendor_host(base_url: &str) -> bool {
    match url_hostname(base_url) {
        Some(host) => RETIRED_VENDOR_HOSTS.contains(&host.as_str()),
        None => false,
    }
}

fn upgrade_retired_vendor_profiles(settings: &mut IndexMap<String, String>) {
    let profiles = match parse_editable_inference_model_profiles(
        settings.get(MODEL_PROFILES_SETTING_ID).map(String::as_str),
    ) {
        Ok(profiles) => profiles,
        Err(_) => return,
    };

    let mut changed = false;
    let upgraded: Vec<EditableInferenceModelProfile> = profiles
        .into_iter()
        .map(|profile| {
            let shipped = default_model_profile_by_id(profile.id.trim());

            let shipped = shipped.filter(|_| is_retired_vendor_host(&profile.base_url));
            match shipped {
                Some(shipped) => {
                    changed = true;
                    shipped
                }
                None => profile,
            }
        })
        .collect();

    if changed {
        settings.insert(
            MODEL_PROFILES_SETTING_ID.to_string(),
            serialize_editable_inference_model_profiles(&upgraded),
        );
    }
}

// defaultModelProfiles.find((candidate) => candidate.id === profile.id.trim()),
// re-materialized as the EditableInferenceModelProfile the shipper would emit
// (createEditableInferenceModelProfile) by running the default entry back
// through the shared parse path.
fn default_model_profile_by_id(id: &str) -> Option<EditableInferenceModelProfile> {
    let defaults: Vec<Value> = serde_json::from_str(MODEL_PROFILES_DEFAULT_JSON).ok()?;
    let item = defaults
        .iter()
        .find(|item| item.get("id").and_then(Value::as_str) == Some(id))?;
    let text = serde_json::to_string(&Value::Array(vec![item.clone()])).ok()?;
    parse_editable_inference_model_profiles(Some(&text))
        .ok()?
        .into_iter()
        .next()
}

// loadCliConfig(): a missing file is created with the defaults; otherwise the
// {"settings": {...}, "version": 1} wrapper is required or the load fails.
pub fn load_cli_config(path: &Path) -> Result<CliConfig> {
    if !path.exists() {
        let config = create_default_cli_config();

        save_cli_config(path, &config)?;

        return Ok(config);
    }

    let content = std::fs::read_to_string(path)?;
    let parsed_value: serde_json::Value = serde_json::from_str(&content)
        .map_err(|error| anyhow!("Failed to parse {}: {}", path.display(), error))?;

    let settings_value = parsed_value.get("settings");
    if !matches!(settings_value, Some(serde_json::Value::Object(_))) {
        return Err(anyhow!(
            "The file at {} is not a valid drip config file.",
            path.display()
        ));
    }

    let mut settings = normalize_web_setting_values(settings_value.unwrap_or(&serde_json::Value::Null));

    // New default profiles added by upgrades appear without clobbering user edits.
    let merged = merge_missing_default_inference_profiles(
        settings.get(MODEL_PROFILES_SETTING_ID).map(String::as_str),
    );
    settings.insert(MODEL_PROFILES_SETTING_ID.to_string(), merged);
    let merged = merge_missing_default_system_prompt_profiles(
        settings
            .get(SYSTEM_PROMPT_PROFILES_SETTING_ID)
            .map(String::as_str),
    );
    settings.insert(SYSTEM_PROMPT_PROFILES_SETTING_ID.to_string(), merged);
    upgrade_cerebras_profiles(&mut settings);
    upgrade_retired_vendor_profiles(&mut settings);

    // statusLine is opt-in and never fatal: a malformed entry warns and the
    // rest of the config still loads. No process is spawned here.
    let (status_line, warnings) = match parse_status_line_setting(parsed_value.get("statusLine")) {
        Ok(parsed) => parsed,
        Err(error) => {
            eprintln!("warning: {}: ignoring \"statusLine\": {error}", path.display());
            (None, Vec::new())
        }
    };
    for warning in &warnings {
        eprintln!("warning: {}: {warning}", path.display());
    }

    Ok(CliConfig {
        path: None,
        settings,
        status_line,
        version: Some(1),
    })
}

// The result carries exactly the known setting ids, each defaulting to its
// definition default and overwritten only where the input holds a string.
fn normalize_web_setting_values(input: &Value) -> IndexMap<String, String> {
    let mut normalized = default_setting_values();
    if let Some(object) = input.as_object() {
        for (key, value) in object {
            if value.is_string() {
                if let Some(slot) = normalized.get_mut(key) {
                    *slot = value.as_str().unwrap_or_default().to_string();
                }
            }
        }
    }
    normalized
}

pub fn save_cli_config(path: &Path, config: &CliConfig) -> Result<()> {
    let body = serde_json::to_string_pretty(config)?;
    crate::lib_fs::write_file_atomic(path, &format!("{body}\n"), true)?;
    Ok(())
}

// Resolves the inference config from the CLI config's settings.
pub fn resolve_cli_inference(
    config: &CliConfig,
    env: Option<&std::collections::HashMap<String, String>>,
) -> Result<crate::core::inference::ResolvedInferenceConfig> {
    crate::core::inference::resolve_inference_config(&config.settings, env)
}

pub fn list_cli_model_profiles(settings: &IndexMap<String, String>) -> Result<Vec<InferenceModelProfile>> {
    parse_inference_model_profiles(settings)
}

pub fn list_cli_system_prompt_profiles(
    settings: &IndexMap<String, String>,
) -> Result<Vec<SystemPromptProfile>> {
    parse_system_prompt_profiles(settings)
}

pub fn get_active_cli_profile_id(settings: &IndexMap<String, String>) -> String {
    // settings[id].trim(), falling back to defaults[id] — never a hardcoded literal.
    settings
        .get(ACTIVE_INFERENCE_PROFILE_SETTING_ID)
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| {
            default_setting_values()
                .get(ACTIVE_INFERENCE_PROFILE_SETTING_ID)
                .cloned()
                .unwrap_or_default()
        })
}

pub fn set_active_cli_profile(mut config: CliConfig, profile_id: &str) -> Result<CliConfig> {
    let known = list_cli_model_profiles(&config.settings)?
        .iter()
        .any(|profile| profile.id == profile_id);
    if !known {
        return Err(anyhow!("Unknown model profile \"{}\".", profile_id));
    }
    config
        .settings
        .insert(ACTIVE_INFERENCE_PROFILE_SETTING_ID.to_string(), profile_id.to_string());
    Ok(config)
}

pub fn get_active_cli_tool_profile_id(settings: &IndexMap<String, String>) -> String {
    settings
        .get(ACTIVE_TOOL_PROFILE_SETTING_ID)
        .cloned()
        .unwrap_or_default()
}

pub fn set_active_cli_tool_profile(mut config: CliConfig, profile_id: &str) -> Result<CliConfig> {
    if !profile_id.is_empty() {
        let known = list_cli_model_profiles(&config.settings)?
            .iter()
            .any(|profile| profile.id == profile_id);
        if !known {
            return Err(anyhow!("Unknown model profile \"{}\".", profile_id));
        }
    }
    config
        .settings
        .insert(ACTIVE_TOOL_PROFILE_SETTING_ID.to_string(), profile_id.to_string());
    Ok(config)
}

pub fn set_active_cli_system_prompt(mut config: CliConfig, prompt_profile_id: &str) -> Result<CliConfig> {
    let known = list_cli_system_prompt_profiles(&config.settings)?
        .iter()
        .any(|profile| profile.id == prompt_profile_id);
    if !known {
        return Err(anyhow!("Unknown system prompt profile \"{}\".", prompt_profile_id));
    }
    config
        .settings
        .insert(ACTIVE_SYSTEM_PROMPT_PROFILE_SETTING_ID.to_string(), prompt_profile_id.to_string());
    Ok(config)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // (a) default_setting_values() parses into >=10 profiles incl. glm-5-3-flash.
    #[test]
    fn test_default_setting_values_parses_profiles() {
        let settings = default_setting_values();
        let profiles = parse_inference_model_profiles(&settings).expect("parse");
        assert!(profiles.len() >= 10);
        assert!(profiles.iter().any(|p| p.id == "glm-5-3-flash"));
        let prompts = parse_system_prompt_profiles(&settings).expect("parse prompts");
        assert!(prompts.iter().any(|p| p.id == "default-coding-agent"));
    }

    // (b) Round-trip with maxContextTokens stored as string "64000".
    #[test]
    fn profile_fields_are_trimmed_and_blank_headers_dropped_like_the_ts() {
        let json = r#"{"id":"p1","model":"m","provider":"openai","baseUrl":"  ","label":" L ","headers":{"X-A":" ","X-B":"b"},"apiKeyRef":"env:X"}"#;
        let profile = normalize_model_profile(&serde_json::from_str(json).unwrap(), 0).unwrap();
        assert_eq!(profile.base_url, None);
        assert_eq!(profile.label.as_deref(), Some("L"));
        assert_eq!(profile.headers.as_ref().unwrap().len(), 1);
        assert_eq!(profile.headers.as_ref().unwrap().get("X-B").map(String::as_str), Some("b"));

        let bad = r#"{"id":"p1","model":"m","provider":"openai","headers":"nope"}"#;
        let error = normalize_model_profile(&serde_json::from_str(bad).unwrap(), 0).unwrap_err().to_string();
        assert_eq!(error, "Inference profile \"p1\" must use an object for headers.");
    }

    #[test]
    fn test_round_trip_string_max_context_tokens() {
        let profile_json = r#"[{"id":"p1","model":"m","provider":"openai","maxContextTokens":"64000","label":"L","apiKeyRef":"env:X"}]"#;
        let mut settings = IndexMap::new();
        settings.insert(MODEL_PROFILES_SETTING_ID.to_string(), profile_json.to_string());
        let profiles = parse_inference_model_profiles(&settings).expect("parse");
        assert_eq!(profiles[0].max_context_tokens, Some(64000));
        let serialized = serialize_editable_inference_model_profiles(
            &parse_editable_inference_model_profiles(Some(profile_json)).expect("editable"),
        );
        assert!(serialized.contains("\"maxContextTokens\": \"64000\""));
        assert!(serialized.contains("\"apiKeyRef\": \"env:X\""));
    }

    // (c) Duplicate id error text.
    #[test]
    fn test_duplicate_profile_id_error() {
        let json = r#"[{"id":"dup","model":"m","provider":"openai"},{"id":"dup","model":"m2","provider":"openai"}]"#;
        let mut settings = IndexMap::new();
        settings.insert(MODEL_PROFILES_SETTING_ID.to_string(), json.to_string());
        let error = parse_inference_model_profiles(&settings).unwrap_err().to_string();
        assert_eq!(error, "Inference profile \"dup\" is duplicated.");
    }

    // (d) merge appends missing defaults to a one-profile string.
    #[test]
    fn test_merge_missing_default_inference_profiles() {
        let json = r#"[{"id":"custom","model":"m","provider":"openai"}]"#;
        let merged = merge_missing_default_inference_profiles(Some(json));
        let items: Vec<Value> = serde_json::from_str(&merged).expect("merged parses");
        assert!(items.iter().any(|item| item.get("id").and_then(Value::as_str) == Some("custom")));
        assert!(items.iter().any(|item| item.get("id").and_then(Value::as_str) == Some("glm-5-3-flash")));
    }

    // upgradeCerebrasProfiles(): a cerebras profile with an inline apiKey has
    // credentialMode "none"? No — inline keys parse as "inline"; the upgraded
    // shape is a profile with NO credential at all plus the legacy key setting
    // unset, which flips to env/CEREBRAS_API_KEY. Idempotent on re-run.
    #[test]
    fn test_upgrade_cerebras_profiles_env_upgrade() {
        let json = r#"[{"id":"c1","model":"zai-glm","provider":"cerebras","baseUrl":"https://api.cerebras.ai/v1","label":"C"}]"#;
        let mut settings = IndexMap::new();
        settings.insert(MODEL_PROFILES_SETTING_ID.to_string(), json.to_string());
        settings.insert(CEREBRAS_API_KEY_SETTING_ID.to_string(), String::new());

        upgrade_cerebras_profiles(&mut settings);
        let items: Vec<Value> =
            serde_json::from_str(&settings[MODEL_PROFILES_SETTING_ID]).expect("json");
        // The editable credentialMode/credentialValue pair serializes back
        // as the apiKeyRef the runtime reads.
        assert_eq!(items[0]["apiKeyRef"], "env:CEREBRAS_API_KEY");

        // Idempotent: running twice changes nothing.
        let once = settings[MODEL_PROFILES_SETTING_ID].clone();
        upgrade_cerebras_profiles(&mut settings);
        assert_eq!(settings[MODEL_PROFILES_SETTING_ID], once);

        // A legacy key configured by the user blocks the upgrade.
        let mut settings2 = IndexMap::new();
        settings2.insert(MODEL_PROFILES_SETTING_ID.to_string(), json.to_string());
        settings2.insert(CEREBRAS_API_KEY_SETTING_ID.to_string(), "sk-legacy".to_string());
        upgrade_cerebras_profiles(&mut settings2);
        let items2: Vec<Value> =
            serde_json::from_str(&settings2[MODEL_PROFILES_SETTING_ID]).expect("json");
        assert!(items2[0].get("apiKeyRef").is_none());
    }

    // upgradeRetiredVendorProfiles(): a FriendliAI baseUrl on a shipped id is
    // re-pointed at the shipped OpenRouter profile; a non-retired host and a
    // non-shipped id are left alone. Idempotent on re-run.
    #[test]
    fn test_upgrade_retired_vendor_profiles() {
        let json = r#"[{"id":"glm-5-3-flash","model":"z-ai/glm-5.3-flash","provider":"openai-compatible","baseUrl":"https://api.friendli.ai/serverless/v1","label":"GLM","apiKeyRef":"env:FRIENDLI_TOKEN"},{"id":"custom","model":"m","provider":"openai","baseUrl":"https://api.z.ai/api/paas/v4","label":"Z"}]"#;
        let mut settings = IndexMap::new();
        settings.insert(MODEL_PROFILES_SETTING_ID.to_string(), json.to_string());

        upgrade_retired_vendor_profiles(&mut settings);
        let items: Vec<Value> =
            serde_json::from_str(&settings[MODEL_PROFILES_SETTING_ID]).expect("json");
        assert_eq!(items.len(), 2);
        assert_eq!(items[0]["baseUrl"], "https://openrouter.ai/api/v1");
        assert_eq!(items[0]["apiKeyRef"], "env:OPENROUTER_API_KEY");
        assert_eq!(items[0]["provider"], "openrouter");
        // Z.AI is not a retired host and "custom" is not a shipped id: kept.
        assert_eq!(items[1]["baseUrl"], "https://api.z.ai/api/paas/v4");

        // Idempotent: running twice changes nothing.
        let once = settings[MODEL_PROFILES_SETTING_ID].clone();
        upgrade_retired_vendor_profiles(&mut settings);
        assert_eq!(settings[MODEL_PROFILES_SETTING_ID], once);
    }

    // Both upgrades: a non-JSON model_profiles value leaves settings unchanged
    // (parsing fails and the upgrade returns early).
    #[test]
    fn test_upgrade_functions_non_json_noop() {
        for upgrade in [
            upgrade_cerebras_profiles as fn(&mut IndexMap<String, String>),
            upgrade_retired_vendor_profiles as fn(&mut IndexMap<String, String>),
        ] {
            let mut settings = IndexMap::new();
            settings.insert(
                MODEL_PROFILES_SETTING_ID.to_string(),
                "not json at all".to_string(),
            );
            upgrade(&mut settings);
            assert_eq!(settings[MODEL_PROFILES_SETTING_ID], "not json at all");
        }
    }

    // (codex-1) provider id: "codex" parses, is case-sensitive, and round-trips.
    #[test]
    fn test_codex_provider_id_parses() {
        assert_eq!(InferenceProviderId::parse("codex"), Some(InferenceProviderId::Codex));
        assert_eq!(InferenceProviderId::parse("Codex"), None);
        assert_eq!(InferenceProviderId::parse("openai"), Some(InferenceProviderId::OpenAi));
        assert_eq!(InferenceProviderId::Codex.as_str(), "codex");
        let profile = normalize_model_profile(
            &serde_json::from_str::<Value>(
                r#"{"id":"c1","model":"gpt-5.6-luna","provider":"codex","reasoningEffort":"high"}"#,
            )
            .unwrap(),
            0,
        )
        .expect("codex profile parses");
        assert_eq!(profile.provider, "codex");
        assert_eq!(profile.base_url, None);
        assert_eq!(profile.api_key, None);
        assert_eq!(profile.api_key_ref, None);
        assert_eq!(profile.reasoning_effort.as_deref(), Some("high"));
    }

    // (codex-2) nonempty HTTP credential/endpoint fields are rejected for codex.
    #[test]
    fn test_codex_rejects_http_credentials_and_endpoints() {
        let cases = [
            r#"{"id":"c2","model":"gpt-5.6-luna","provider":"codex","apiKey":"sk-x"}"#,
            r#"{"id":"c2","model":"gpt-5.6-luna","provider":"codex","apiKeyRef":"env:OPENAI_API_KEY"}"#,
            r#"{"id":"c2","model":"gpt-5.6-luna","provider":"codex","baseUrl":"https://api.openai.com/v1"}"#,
            r#"{"id":"c2","model":"gpt-5.6-luna","provider":"codex","headers":{"X-A":"b"}}"#,
        ];
        for json in cases {
            let error = normalize_model_profile(&serde_json::from_str(json).unwrap(), 0)
                .unwrap_err()
                .to_string();
            assert!(
                error.contains("codex") && error.contains("remove "),
                "diagnostic should name codex and the fix: {error}"
            );
        }
        // Empty/blank HTTP fields are treated as absent, not conflicts.
        let blank = normalize_model_profile(
            &serde_json::from_str::<Value>(
                r#"{"id":"c3","model":"gpt-5.6-luna","provider":"codex","apiKey":"","baseUrl":"  "}"#,
            )
            .unwrap(),
            0,
        )
        .expect("blank credential fields are ignored");
        assert_eq!(blank.api_key, None);
        assert_eq!(blank.base_url, None);

        // parse_inference_model_profiles surfaces the conflict loudly.
        let mut settings = IndexMap::new();
        settings.insert(
            MODEL_PROFILES_SETTING_ID.to_string(),
            r#"[{"id":"c4","model":"gpt-5.6-luna","provider":"codex","apiKey":"sk-x"}]"#.to_string(),
        );
        let error = parse_inference_model_profiles(&settings).unwrap_err().to_string();
        assert!(error.contains("codex"), "parse-level diagnostic: {error}");
    }

    // (codex-3) existing openai route behavior is unchanged: inline key,
    // env ref, and credential-less profiles all normalize as before.
    #[test]
    fn test_openai_profile_normalization_unchanged() {
        let inline = normalize_model_profile(
            &serde_json::from_str::<Value>(
                r#"{"id":"o1","model":"m","provider":"openai","apiKey":"sk-y","baseUrl":"https://api.openai.com/v1","headers":{"X-A":"a"}}"#,
            )
            .unwrap(),
            0,
        )
        .expect("openai keeps credentials");
        assert_eq!(inline.api_key.as_deref(), Some("sk-y"));
        assert_eq!(inline.base_url.as_deref(), Some("https://api.openai.com/v1"));
        assert!(inline.headers.is_some());

        let bare = normalize_model_profile(
            &serde_json::from_str::<Value>(r#"{"id":"o2","model":"m","provider":"openai"}"#).unwrap(),
            0,
        )
        .expect("openai without credentials still parses");
        assert_eq!(bare.api_key, None);
        assert_eq!(bare.base_url, None);

        let unsupported = normalize_model_profile(
            &serde_json::from_str::<Value>(r#"{"id":"o3","model":"m","provider":"nope"}"#).unwrap(),
            0,
        )
        .unwrap_err()
        .to_string();
        assert_eq!(
            unsupported,
            "Inference profile \"o3\" has an unsupported provider \"nope\"."
        );
    }

    // ---- statusLine configuration ----

    fn status_line_value(json: &str) -> Value {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn status_line_absent_or_null_is_none() {
        assert_eq!(parse_status_line_setting(None).unwrap().0, None);
        assert_eq!(
            parse_status_line_setting(Some(&Value::Null)).unwrap().0,
            None
        );
    }

    #[test]
    fn status_line_parses_with_defaults_and_bounded_values() {
        let (setting, warnings) = parse_status_line_setting(Some(&status_line_value(
            r#"{"type":"command","command":"echo hi"}"#,
        )))
        .unwrap();
        let setting = setting.unwrap();
        assert_eq!(setting.kind, "command");
        assert_eq!(setting.padding, 0);
        assert_eq!(
            setting.update_interval_ms,
            STATUS_LINE_DEFAULT_UPDATE_INTERVAL_MS
        );
        assert_eq!(setting.timeout_ms, STATUS_LINE_DEFAULT_TIMEOUT_MS);
        assert!(warnings.is_empty());

        // Out-of-range numbers clamp to the documented bounds with a warning.
        let (setting, warnings) = parse_status_line_setting(Some(&status_line_value(
            r#"{"type":"command","command":"echo hi","padding":9,"updateIntervalMs":5,"timeoutMs":999999}"#,
        )))
        .unwrap();
        let setting = setting.unwrap();
        assert_eq!(setting.padding, STATUS_LINE_MAX_PADDING);
        assert_eq!(
            setting.update_interval_ms,
            STATUS_LINE_MIN_UPDATE_INTERVAL_MS
        );
        assert_eq!(setting.timeout_ms, STATUS_LINE_MAX_TIMEOUT_MS);
        assert_eq!(warnings.len(), 3, "one warning per clamped field: {warnings:?}");

        // Command is trimmed.
        let (setting, warnings) = parse_status_line_setting(Some(&status_line_value(
            r#"{"type":"command","command":"  echo hi  "}"#,
        )))
        .unwrap();
        assert_eq!(setting.unwrap().command, "echo hi");
        assert!(warnings.is_empty());
    }

    #[test]
    fn status_line_rejects_bad_configurations_without_executing_anything() {
        for (json, fragment) in [
            (r#""echo hi""#, "must be an object"),
            (r#"42"#, "must be an object"),
            (r#"{"type":"tty","command":"x"}"#, "statusLine.type"),
            (r#"{"type":"command"}"#, "statusLine.command"),
            (r#"{"type":"command","command":"   "}"#, "statusLine.command"),
            (r#"{}"#, "statusLine.command"),
        ] {
            let error = parse_status_line_setting(Some(&status_line_value(json)))
                .unwrap_err()
                .to_string();
            assert!(error.contains(fragment), "{json} -> {error}");
        }
    }

    #[test]
    fn cli_config_round_trips_status_line_and_omits_it_when_absent() {
        let mut config = create_default_cli_config();
        assert_eq!(config.status_line, None);

        // Missing configuration must preserve existing behavior: no statusLine
        // key ever appears in the saved file.
        let saved = serde_json::to_string(&config).unwrap();
        assert!(!saved.contains("statusLine"), "{saved}");

        config.status_line = Some(StatusLineSetting {
            kind: "command".to_string(),
            command: "echo hi".to_string(),
            padding: 1,
            update_interval_ms: 500,
            timeout_ms: 2000,
        });
        let saved = serde_json::to_string(&config).unwrap();
        assert!(saved.contains("\"statusLine\""), "{saved}");
        let parsed: CliConfig = serde_json::from_str(&saved).unwrap();
        assert_eq!(parsed.status_line, config.status_line);
    }

    #[test]
    fn load_cli_config_keeps_status_line_and_degrades_gracefully() {
        let dir = std::env::temp_dir().join(format!(
            "drip-status-line-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");

        // No statusLine -> None, existing behavior preserved.
        std::fs::write(&path, "{\n  \"settings\": {},\n  \"version\": 1\n}\n").unwrap();
        let config = load_cli_config(&path).unwrap();
        assert_eq!(config.status_line, None);

        // A valid statusLine survives the load.
        std::fs::write(
            &path,
            r#"{ "settings": {}, "version": 1, "statusLine": {"type":"command","command":"echo hi"} }"#,
        )
        .unwrap();
        let config = load_cli_config(&path).unwrap();
        assert_eq!(
            config.status_line.map(|s| s.command),
            Some("echo hi".to_string())
        );

        // A malformed statusLine is nonfatal: config still loads, None result,
        // and nothing was executed.
        std::fs::write(
            &path,
            r#"{ "settings": {}, "version": 1, "statusLine": "echo hi" }"#,
        )
        .unwrap();
        let config = load_cli_config(&path).unwrap();
        assert_eq!(config.status_line, None);

        std::fs::remove_dir_all(&dir).ok();
    }
}
