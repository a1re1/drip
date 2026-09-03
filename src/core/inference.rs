// 495-512, 656-755, 1277-1334, 1368-1507): resolveInferenceConfig and the
// helpers it composes. The settings parsers live in core/config.rs.
//
// The "cmd:" credential resolver is the port of src/web/command-credentials.ts
// in tools/command_policy.rs; the TS registers it through
// setCommandCredentialResolver so the browser bundle never shells out, and
// drip has no browser, so the resolver is wired directly.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{anyhow, Result};
use indexmap::IndexMap;

use crate::core::config::{
    default_setting_values, parse_inference_model_profiles, parse_stored_api_key_entries,
    parse_system_prompt_profiles, InferenceModelProfile, SystemPromptProfile,
    ACTIVE_INFERENCE_PROFILE_SETTING_ID, ACTIVE_SYSTEM_PROMPT_PROFILE_SETTING_ID,
    ACTIVE_TOOL_PROFILE_SETTING_ID, CEREBRAS_API_KEY_SETTING_ID, DEFAULT_MAX_CONTEXT_TOKENS,
};
use crate::harness::model_call::ModelRoute;

pub const OPENROUTER_BASE_URL: &str = "https://openrouter.ai/api/v1";
pub const OPENROUTER_API_KEY_ENV: &str = "OPENROUTER_API_KEY";

/// Header list (insertion-ordered, TS `Record<string, string>`).
pub type Headers = Vec<(String, String)>;

/// settings.ts:122 — `ResolvedModelRoute`.
#[derive(Clone)]
pub struct ResolvedModelRoute {
    pub fallback_route: Option<Box<ResolvedModelRoute>>,
    pub headers: Headers,
    pub model: String,
    pub profile_id: String,
    pub provider: String,
    pub reasoning_effort: Option<String>,
    pub refresh_headers: Option<Arc<dyn Fn() -> Result<Headers, String> + Send + Sync>>,
    pub url: String,
}

impl std::fmt::Debug for ResolvedModelRoute {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResolvedModelRoute")
            .field("fallback_route", &self.fallback_route)
            .field("headers", &self.headers)
            .field("model", &self.model)
            .field("profile_id", &self.profile_id)
            .field("provider", &self.provider)
            .field("reasoning_effort", &self.reasoning_effort)
            .field("refresh_headers", &self.refresh_headers.is_some())
            .field("url", &self.url)
            .finish()
    }
}

impl ResolvedModelRoute {
    /// The harness-facing route (harness/model_call.rs `ModelRoute`) — the
    /// same fields minus the profile id, chained fallback included.
    pub fn to_model_route(&self) -> ModelRoute {
        ModelRoute {
            fallback_route: self.fallback_route.as_ref().map(|route| Box::new(route.to_model_route())),
            headers: Some(self.headers.clone()),
            model: self.model.clone(),
            provider: Some(self.provider.clone()),
            reasoning_effort: self.reasoning_effort.clone(),
            refresh_headers: self.refresh_headers.clone(),
            url: self.url.clone(),
        }
    }
}

/// settings.ts:143 — `ResolvedInferenceConfig = ResolvedModelRoute & {...}`.
#[derive(Clone, Debug)]
pub struct ResolvedInferenceConfig {
    pub route: ResolvedModelRoute,
    pub system_prompt: String,
    pub tool_route: Option<ResolvedModelRoute>,
    pub tool_route_warning: Option<String>,
}

impl std::ops::Deref for ResolvedInferenceConfig {
    type Target = ResolvedModelRoute;

    fn deref(&self) -> &ResolvedModelRoute {
        &self.route
    }
}

/// settings.ts:1368 — `ResolveInferenceOptions { env? }`: the env source for
/// "env:NAME" references; the CLI passes its env.vars file merged over the
/// process environment. `None` reads the process environment.
pub type EnvSource<'a> = Option<&'a HashMap<String, String>>;

fn env_lookup(env: EnvSource<'_>, name: &str) -> Option<String> {
    match env {
        Some(map) => map.get(name).cloned(),
        None => std::env::var(name).ok(),
    }
}

fn trim_trailing_slash(value: &str) -> String {
    value.trim_end_matches('/').to_string()
}

/// settings.ts:354 — `getDefaultBaseUrl(provider)`.
pub fn get_default_base_url(provider: &str) -> String {
    match provider {
        "openai" => "https://api.openai.com/v1",
        "claude" => "https://api.anthropic.com/v1",
        "gemini" => "https://generativelanguage.googleapis.com/v1beta/openai",
        "ollama" => "http://localhost:11434/v1",
        "cerebras" => "https://api.cerebras.ai/v1",
        "xai" => "https://api.x.ai/v1",
        "openrouter" => OPENROUTER_BASE_URL,
        _ => "http://localhost:4100/v1",
    }
    .to_string()
}

/// settings.ts:495 — `buildChatCompletionsUrl(baseUrl)`.
pub fn build_chat_completions_url(base_url: &str) -> String {
    let normalized = trim_trailing_slash(base_url);

    if normalized.ends_with("/chat/completions") {
        return normalized;
    }

    format!("{normalized}/chat/completions")
}

/// settings.ts:505 — `supportsOpenAIReasoningEffort(profile)`.
pub fn supports_openai_reasoning_effort(provider: &str, model: &str) -> bool {
    let model = model.trim();

    // OpenRouter passes reasoning_effort through to OpenAI unchanged, so the
    // gate is the model family, on either the direct or the aggregated route.
    (provider == "openai" && model.starts_with("gpt-5.4"))
        || (provider == "openrouter" && model.starts_with("openai/gpt-5.4"))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReferenceKind {
    Cmd,
    Env,
    Stored,
}

/// settings.ts:656 — `parseReferenceTarget(apiKeyRef, profileId)`.
pub fn parse_reference_target(api_key_ref: &str, profile_id: &str) -> Result<(ReferenceKind, String)> {
    let trimmed_ref = api_key_ref.trim();

    if let Some(rest) = trimmed_ref.strip_prefix("env:") {
        let env_name = rest.trim();

        if env_name.is_empty() {
            return Err(anyhow!("Inference profile \"{profile_id}\" uses an empty env reference."));
        }

        return Ok((ReferenceKind::Env, env_name.to_string()));
    }

    if let Some(rest) = trimmed_ref.strip_prefix("cmd:") {
        let command = rest.trim();

        if command.is_empty() {
            return Err(anyhow!("Inference profile \"{profile_id}\" uses an empty cmd reference."));
        }

        return Ok((ReferenceKind::Cmd, command.to_string()));
    }

    let stored_key_id = trimmed_ref.strip_prefix("stored:").map(str::trim).unwrap_or(trimmed_ref);

    if stored_key_id.is_empty() {
        return Err(anyhow!("Inference profile \"{profile_id}\" uses an empty stored key reference."));
    }

    Ok((ReferenceKind::Stored, stored_key_id.to_string()))
}

/// settings.ts:697 — `resolveStoredApiKey(settings, keyId, profileId)`.
fn resolve_stored_api_key(settings: &IndexMap<String, String>, key_id: &str, profile_id: &str) -> Result<String> {
    let stored_keys = parse_stored_api_key_entries(settings)?;
    let matching_key = stored_keys
        .iter()
        .find(|entry| entry.id == key_id)
        .ok_or_else(|| anyhow!("Inference profile \"{profile_id}\" references stored key \"{key_id}\" but it is not configured."))?;
    let key_value = matching_key.value.trim();

    if key_value.is_empty() {
        return Err(anyhow!("Stored key \"{key_id}\" is empty."));
    }

    Ok(key_value.to_string())
}

/// settings.ts:714 — `resolveProfileApiKey(profile, settings, env)`.
pub fn resolve_profile_api_key(
    profile: &InferenceModelProfile,
    settings: &IndexMap<String, String>,
    env: EnvSource<'_>,
) -> Result<Option<String>> {
    if let Some(api_key) = profile.api_key.as_deref().filter(|key| !key.is_empty()) {
        return Ok(Some(api_key.to_string()));
    }

    if let Some(api_key_ref) = profile.api_key_ref.as_deref().filter(|reference| !reference.is_empty()) {
        let (kind, value) = parse_reference_target(api_key_ref, &profile.id)?;

        return match kind {
            ReferenceKind::Env => {
                let env_value = env_lookup(env, &value).map(|text| text.trim().to_string()).filter(|text| !text.is_empty());

                match env_value {
                    Some(text) => Ok(Some(text)),
                    None => Err(anyhow!(
                        "Inference profile \"{}\" references env var \"{value}\" but it is not set.",
                        profile.id
                    )),
                }
            }
            ReferenceKind::Cmd => Ok(Some(crate::tools::command_policy::resolve_command_credential(
                &value,
                chrono::Utc::now().timestamp_millis(),
            )?)),
            ReferenceKind::Stored => Ok(Some(resolve_stored_api_key(settings, &value, &profile.id)?)),
        };
    }

    if profile.provider != "cerebras" {
        return Ok(None);
    }

    Ok(settings
        .get(CEREBRAS_API_KEY_SETTING_ID)
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty()))
}

/// settings.ts:1277 — `resolveActiveInferenceProfile(settings)`.
pub fn resolve_active_inference_profile(settings: &IndexMap<String, String>) -> Result<InferenceModelProfile> {
    let defaults = default_setting_values();
    let active_profile_id = settings
        .get(ACTIVE_INFERENCE_PROFILE_SETTING_ID)
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .or_else(|| defaults.get(ACTIVE_INFERENCE_PROFILE_SETTING_ID).cloned())
        .unwrap_or_default();
    let profiles = parse_inference_model_profiles(settings)?;

    profiles
        .into_iter()
        .find(|candidate| candidate.id == active_profile_id)
        .ok_or_else(|| anyhow!(
            "Unknown active model id \"{active_profile_id}\". Add it to Model Profiles or switch Active Model ID."
        ))
}

/// settings.ts:1290 — `resolveActiveSystemPromptProfile(settings)`.
pub fn resolve_active_system_prompt_profile(settings: &IndexMap<String, String>) -> Result<SystemPromptProfile> {
    let defaults = default_setting_values();
    let active_prompt_id = settings
        .get(ACTIVE_SYSTEM_PROMPT_PROFILE_SETTING_ID)
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .or_else(|| defaults.get(ACTIVE_SYSTEM_PROMPT_PROFILE_SETTING_ID).cloned())
        .unwrap_or_default();
    let profiles = parse_system_prompt_profiles(settings)?;

    profiles
        .into_iter()
        .find(|candidate| candidate.id == active_prompt_id)
        .ok_or_else(|| anyhow!(
            "Unknown active system prompt id \"{active_prompt_id}\". Add it to System Prompt Profiles or switch Active System Prompt ID."
        ))
}

// The tool-calling profile is optional routing. An explicitly cleared setting
// disables the split, an explicitly configured id that is missing from the
// profile catalog fails loudly, and a stale catalog that lacks the built-in
// default tool profile falls back to the general model so settings saved
// before routing existed keep working.
/// settings.ts:1308 — `resolveActiveToolProfile(settings)`.
pub fn resolve_active_tool_profile(settings: &IndexMap<String, String>) -> Result<Option<InferenceModelProfile>> {
    let raw_value = settings.get(ACTIVE_TOOL_PROFILE_SETTING_ID);

    if let Some(raw) = raw_value {
        if raw.trim().is_empty() {
            return Ok(None);
        }
    }

    let explicit_id = raw_value.map(|value| value.trim().to_string()).unwrap_or_default();
    let tool_profile_id = if explicit_id.is_empty() {
        default_setting_values().get(ACTIVE_TOOL_PROFILE_SETTING_ID).cloned().unwrap_or_default()
    } else {
        explicit_id.clone()
    };
    let profiles = parse_inference_model_profiles(settings)?;

    match profiles.into_iter().find(|candidate| candidate.id == tool_profile_id) {
        Some(profile) => Ok(Some(profile)),
        None if !explicit_id.is_empty() => Err(anyhow!(
            "Unknown tool-calling model id \"{tool_profile_id}\". Add it to Model Profiles or switch Tool-Calling Model ID."
        )),
        None => Ok(None),
    }
}

/// settings.ts:1331 — `resolveActiveProfileMaxContextTokens(settings)`.
pub fn resolve_active_profile_max_context_tokens(settings: &IndexMap<String, String>) -> Result<i64> {
    Ok(resolve_active_inference_profile(settings)?
        .max_context_tokens
        .unwrap_or(DEFAULT_MAX_CONTEXT_TOKENS))
}

// Resolves a profile's declared fallback chain, if any. Deliberately total: a
// fallbackProfileId that is blank, names a deleted profile, or would revisit a
// profile already on the chain (self-reference, cycle) yields undefined rather
// than throwing, because losing a fallback must never take down the primary
// route. A hop whose own route fails to resolve (missing credential on this
// machine) is skipped in favour of ITS fallback, so a chain like
// friendli → baseten → z.ai still reaches z.ai when only the z.ai key is set.
/// settings.ts:1381 — `resolveFallbackRoute(...)`.
fn resolve_fallback_route(
    profile: &InferenceModelProfile,
    settings: &IndexMap<String, String>,
    env: EnvSource<'_>,
    profiles: Option<&[InferenceModelProfile]>,
    visited: &[String],
) -> Option<ResolvedModelRoute> {
    let fallback_profile_id = profile.fallback_profile_id.as_deref().unwrap_or("").trim().to_string();
    let profiles = profiles?;

    if fallback_profile_id.is_empty() || visited.iter().any(|id| *id == fallback_profile_id) {
        return None;
    }

    let fallback_profile = profiles.iter().find(|candidate| candidate.id == fallback_profile_id)?;
    let mut next_visited = visited.to_vec();
    next_visited.push(fallback_profile_id);

    match resolve_profile_route_visited(fallback_profile, settings, env, Some(profiles), &next_visited) {
        Ok(route) => Some(route),
        Err(_) => resolve_fallback_route(fallback_profile, settings, env, Some(profiles), &next_visited),
    }
}

fn build_headers(profile: &InferenceModelProfile, token: Option<&str>) -> Headers {
    let mut headers: Headers = profile
        .headers
        .as_ref()
        .map(|map| map.iter().map(|(key, value)| (key.clone(), value.clone())).collect())
        .unwrap_or_default();

    if let Some(token) = token {
        // TS object spread: an existing Authorization key keeps its position
        // and takes the new value.
        if let Some(slot) = headers.iter_mut().find(|(key, _)| key == "Authorization") {
            slot.1 = format!("Bearer {token}");
        } else {
            headers.push(("Authorization".to_string(), format!("Bearer {token}")));
        }
    }

    headers
}

/// settings.ts:1409 — `resolveProfileRoute(profile, settings, env, profiles?, visited)`.
fn resolve_profile_route_visited(
    profile: &InferenceModelProfile,
    settings: &IndexMap<String, String>,
    env: EnvSource<'_>,
    profiles: Option<&[InferenceModelProfile]>,
    visited: &[String],
) -> Result<ResolvedModelRoute> {
    let base_url = trim_trailing_slash(
        profile
            .base_url
            .as_deref()
            .unwrap_or(&get_default_base_url(&profile.provider)),
    );

    if reqwest::Url::parse(&base_url).is_err() {
        return Err(anyhow!("Inference profile \"{}\" has an invalid baseUrl.", profile.id));
    }

    let api_key = resolve_profile_api_key(profile, settings, env)?;
    // A "cmd:" credential is re-resolved per request (hitting the TTL cache) so a
    // token that expires mid-run is transparently re-minted. Static credentials
    // keep their headers baked once, exactly as before.
    let uses_dynamic_credential = match profile.api_key_ref.as_deref().filter(|reference| !reference.is_empty()) {
        Some(reference) => parse_reference_target(reference, &profile.id)?.0 == ReferenceKind::Cmd,
        None => false,
    };

    let fallback_route = resolve_fallback_route(profile, settings, env, profiles, visited);

    let refresh_headers: Option<Arc<dyn Fn() -> Result<Headers, String> + Send + Sync>> = if uses_dynamic_credential {
        let profile = profile.clone();
        let settings = settings.clone();
        let env = env.cloned();

        // A failing command is the call's error, exactly as the TS closure throws:
        // a request silently sent without Authorization would surface as a 401
        // with no hint of the real cause.
        Some(Arc::new(move || {
            let token = resolve_profile_api_key(&profile, &settings, env.as_ref()).map_err(|error| error.to_string())?;

            Ok(build_headers(&profile, token.as_deref()))
        }))
    } else {
        None
    };

    Ok(ResolvedModelRoute {
        fallback_route: fallback_route.map(Box::new),
        headers: build_headers(profile, api_key.as_deref()),
        model: profile.model.clone(),
        profile_id: profile.id.clone(),
        provider: profile.provider.clone(),
        refresh_headers,
        reasoning_effort: if supports_openai_reasoning_effort(&profile.provider, &profile.model) {
            profile.reasoning_effort.clone().filter(|effort| !effort.is_empty())
        } else {
            None
        },
        url: build_chat_completions_url(&base_url),
    })
}

fn resolve_profile_route(
    profile: &InferenceModelProfile,
    settings: &IndexMap<String, String>,
    env: EnvSource<'_>,
    profiles: Option<&[InferenceModelProfile]>,
) -> Result<ResolvedModelRoute> {
    resolve_profile_route_visited(profile, settings, env, profiles, &[profile.id.clone()])
}

// Resolves an arbitrary model profile id (e.g. a harness role's model) to a
// callable route, honoring stored/env credential references like the active
// and tool-calling profiles do.
/// settings.ts:1455 — `resolveModelProfileRoute(settings, profileId, options?)`.
pub fn resolve_model_profile_route(
    settings: &IndexMap<String, String>,
    profile_id: &str,
    env: EnvSource<'_>,
) -> Result<ResolvedModelRoute> {
    let profiles = parse_inference_model_profiles(settings)?;
    let profile = profiles
        .iter()
        .find(|candidate| candidate.id == profile_id)
        .ok_or_else(|| anyhow!("Unknown model profile \"{profile_id}\". Add it to Model Profiles."))?;

    resolve_profile_route(profile, settings, env, Some(&profiles))
}

/// settings.ts:1470 — `resolveInferenceConfig(settings, options?)`.
pub fn resolve_inference_config(settings: &IndexMap<String, String>, env: EnvSource<'_>) -> Result<ResolvedInferenceConfig> {
    let profile = resolve_active_inference_profile(settings)?;
    let system_prompt_profile = resolve_active_system_prompt_profile(settings)?;
    let tool_profile = resolve_active_tool_profile(settings)?;
    let profiles = parse_inference_model_profiles(settings)?;
    let mut tool_route: Option<ResolvedModelRoute> = None;
    let mut tool_route_warning: Option<String> = None;

    if let Some(tool_profile) = tool_profile.filter(|tool_profile| tool_profile.id != profile.id) {
        // A tool profile the user never picked (the built-in default) must not
        // break runs on other providers — e.g. switching to a local model without
        // a Cerebras key. Explicitly configured non-default profiles fail loudly.
        let configured_tool_id = settings
            .get(ACTIVE_TOOL_PROFILE_SETTING_ID)
            .map(|value| value.trim().to_string())
            .unwrap_or_default();
        let is_default_tool_selection = configured_tool_id.is_empty()
            || Some(&configured_tool_id) == default_setting_values().get(ACTIVE_TOOL_PROFILE_SETTING_ID);

        match resolve_profile_route(&tool_profile, settings, env, Some(&profiles)) {
            Ok(route) => tool_route = Some(route),
            Err(error) => {
                if !is_default_tool_selection {
                    return Err(error);
                }

                tool_route_warning = Some(format!(
                    "Tool-calling model \"{}\" is unavailable ({}); every request uses \"{}\".",
                    tool_profile.id, error, profile.id
                ));
            }
        }
    }

    Ok(ResolvedInferenceConfig {
        route: resolve_profile_route(&profile, settings, env, Some(&profiles))?,
        system_prompt: system_prompt_profile.prompt,
        tool_route,
        tool_route_warning,
    })
}

/// settings.ts:1505 — `resolveInferenceUrl(settings)`.
pub fn resolve_inference_url(settings: &IndexMap<String, String>) -> Result<String> {
    Ok(resolve_inference_config(settings, None)?.url.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::config::MODEL_PROFILES_SETTING_ID;

    fn settings_with(entries: &[(&str, &str)]) -> IndexMap<String, String> {
        let mut settings = default_setting_values();
        for (key, value) in entries {
            settings.insert(key.to_string(), value.to_string());
        }
        settings
    }

    #[test]
    fn default_base_urls_and_chat_completions_url() {
        assert_eq!(get_default_base_url("openai"), "https://api.openai.com/v1");
        assert_eq!(get_default_base_url("openai-compatible"), "http://localhost:4100/v1");
        assert_eq!(build_chat_completions_url("http://x/v1/"), "http://x/v1/chat/completions");
        assert_eq!(build_chat_completions_url("http://x/v1/chat/completions"), "http://x/v1/chat/completions");
    }

    #[test]
    fn reference_targets() {
        assert_eq!(parse_reference_target("env:FOO", "p").unwrap(), (ReferenceKind::Env, "FOO".to_string()));
        assert_eq!(parse_reference_target(" cmd: op read x ", "p").unwrap(), (ReferenceKind::Cmd, "op read x".to_string()));
        assert_eq!(parse_reference_target("stored:key", "p").unwrap(), (ReferenceKind::Stored, "key".to_string()));
        assert_eq!(parse_reference_target("key", "p").unwrap(), (ReferenceKind::Stored, "key".to_string()));
        assert_eq!(
            parse_reference_target("env: ", "p").unwrap_err().to_string(),
            "Inference profile \"p\" uses an empty env reference."
        );
    }

    #[test]
    fn resolves_env_credential_into_bearer_header_and_url() {
        let profiles = r#"[{"id":"mock","label":"Mock","model":"m","provider":"openai-compatible","baseUrl":"http://127.0.0.1:9/v1/","apiKeyRef":"env:MOCK_KEY","headers":{"X-Title":"drip"}}]"#;
        let settings = settings_with(&[
            (MODEL_PROFILES_SETTING_ID, profiles),
            (ACTIVE_INFERENCE_PROFILE_SETTING_ID, "mock"),
            (ACTIVE_TOOL_PROFILE_SETTING_ID, ""),
        ]);
        let mut env = HashMap::new();
        env.insert("MOCK_KEY".to_string(), " secret ".to_string());

        let resolved = resolve_inference_config(&settings, Some(&env)).unwrap();
        assert_eq!(resolved.url, "http://127.0.0.1:9/v1/chat/completions");
        assert_eq!(resolved.model, "m");
        assert_eq!(resolved.profile_id, "mock");
        assert_eq!(
            resolved.headers,
            vec![
                ("X-Title".to_string(), "drip".to_string()),
                ("Authorization".to_string(), "Bearer secret".to_string())
            ]
        );
        assert!(resolved.tool_route.is_none());
        assert!(resolved.tool_route_warning.is_none());
        assert!(resolved.refresh_headers.is_none());
        assert!(!resolved.system_prompt.is_empty());
    }

    #[test]
    fn missing_env_credential_fails_with_ts_message() {
        let profiles = r#"[{"id":"mock","label":"Mock","model":"m","provider":"openai","apiKeyRef":"env:NOPE_KEY"}]"#;
        let settings = settings_with(&[
            (MODEL_PROFILES_SETTING_ID, profiles),
            (ACTIVE_INFERENCE_PROFILE_SETTING_ID, "mock"),
            (ACTIVE_TOOL_PROFILE_SETTING_ID, ""),
        ]);
        let env = HashMap::new();
        let error = resolve_inference_config(&settings, Some(&env)).unwrap_err();
        assert_eq!(
            error.to_string(),
            "Inference profile \"mock\" references env var \"NOPE_KEY\" but it is not set."
        );
    }

    #[test]
    fn cmd_credential_refresh_propagates_the_command_failure() {
        // A unique command so the shared TTL cache holds no earlier success for it.
        let profiles = r#"[{"id":"mock","label":"Mock","model":"m","provider":"openai","apiKeyRef":"cmd:echo refresh-ok-1"}]"#;
        let settings = settings_with(&[
            (MODEL_PROFILES_SETTING_ID, profiles),
            (ACTIVE_INFERENCE_PROFILE_SETTING_ID, "mock"),
            (ACTIVE_TOOL_PROFILE_SETTING_ID, ""),
        ]);
        let resolved = resolve_inference_config(&settings, Some(&HashMap::new())).unwrap();
        let refresh = resolved.refresh_headers.clone().expect("cmd credentials refresh per request");
        assert!(refresh().unwrap().contains(&("Authorization".to_string(), "Bearer refresh-ok-1".to_string())));

        let failing = r#"[{"id":"mock","label":"Mock","model":"m","provider":"openai","apiKeyRef":"cmd:false"}]"#;
        let settings = settings_with(&[
            (MODEL_PROFILES_SETTING_ID, failing),
            (ACTIVE_INFERENCE_PROFILE_SETTING_ID, "mock"),
            (ACTIVE_TOOL_PROFILE_SETTING_ID, ""),
        ]);
        let error = resolve_inference_config(&settings, Some(&HashMap::new())).unwrap_err().to_string();
        assert!(error.contains("Failed to run credential command \"false\""), "{error}");
    }

    #[test]
    fn fallback_chain_skips_unresolvable_hops_and_cycles() {
        let profiles = r#"[
          {"id":"a","label":"A","model":"ma","provider":"openai","apiKeyRef":"env:A_KEY","fallbackProfileId":"b"},
          {"id":"b","label":"B","model":"mb","provider":"openai","apiKeyRef":"env:B_KEY","fallbackProfileId":"c"},
          {"id":"c","label":"C","model":"mc","provider":"openai","apiKeyRef":"env:C_KEY","fallbackProfileId":"a"}
        ]"#;
        let settings = settings_with(&[
            (MODEL_PROFILES_SETTING_ID, profiles),
            (ACTIVE_INFERENCE_PROFILE_SETTING_ID, "a"),
            (ACTIVE_TOOL_PROFILE_SETTING_ID, ""),
        ]);
        let mut env = HashMap::new();
        env.insert("A_KEY".to_string(), "ka".to_string());
        env.insert("C_KEY".to_string(), "kc".to_string());

        let resolved = resolve_inference_config(&settings, Some(&env)).unwrap();
        let fallback = resolved.fallback_route.as_ref().expect("fallback");
        assert_eq!(fallback.profile_id, "c");
        assert!(fallback.fallback_route.is_none(), "cycle back to a must stop");
    }

    #[test]
    fn default_tool_profile_failure_degrades_to_warning() {
        let profiles = r#"[{"id":"mock","label":"Mock","model":"m","provider":"ollama"}]"#;
        let mut settings = settings_with(&[
            (MODEL_PROFILES_SETTING_ID, profiles),
            (ACTIVE_INFERENCE_PROFILE_SETTING_ID, "mock"),
        ]);
        // No explicit tool selection: a stale catalog lacking the built-in
        // default tool profile falls back to the general model silently.
        settings.shift_remove(ACTIVE_TOOL_PROFILE_SETTING_ID);
        let env = HashMap::new();
        let resolved = resolve_inference_config(&settings, Some(&env)).unwrap();
        assert!(resolved.tool_route.is_none());
        assert!(resolved.tool_route_warning.is_none());
        assert_eq!(resolved.url, "http://localhost:11434/v1/chat/completions");
        assert!(resolved.headers.is_empty());
    }

    #[test]
    fn default_tool_profile_with_missing_credential_warns() {
        let profiles = r#"[{"id":"mock","label":"Mock","model":"m","provider":"ollama"},{"id":"glm-5-3-flash","label":"F","model":"f","provider":"openrouter","apiKeyRef":"env:NOPE_KEY"}]"#;
        let settings = settings_with(&[
            (MODEL_PROFILES_SETTING_ID, profiles),
            (ACTIVE_INFERENCE_PROFILE_SETTING_ID, "mock"),
        ]);
        let resolved = resolve_inference_config(&settings, Some(&HashMap::new())).unwrap();
        assert!(resolved.tool_route.is_none());
        assert_eq!(
            resolved.tool_route_warning.as_deref(),
            Some("Tool-calling model \"glm-5-3-flash\" is unavailable (Inference profile \"glm-5-3-flash\" references env var \"NOPE_KEY\" but it is not set.); every request uses \"mock\".")
        );
    }

    #[test]
    fn explicit_unknown_tool_profile_fails_loudly() {
        let profiles = r#"[{"id":"mock","label":"Mock","model":"m","provider":"ollama"}]"#;
        let settings = settings_with(&[
            (MODEL_PROFILES_SETTING_ID, profiles),
            (ACTIVE_INFERENCE_PROFILE_SETTING_ID, "mock"),
            (ACTIVE_TOOL_PROFILE_SETTING_ID, "ghost"),
        ]);
        let error = resolve_inference_config(&settings, Some(&HashMap::new())).unwrap_err();
        assert_eq!(
            error.to_string(),
            "Unknown tool-calling model id \"ghost\". Add it to Model Profiles or switch Tool-Calling Model ID."
        );
    }

    #[test]
    fn reasoning_effort_only_for_gpt_54_families() {
        assert!(supports_openai_reasoning_effort("openai", "gpt-5.4-mini"));
        assert!(supports_openai_reasoning_effort("openrouter", "openai/gpt-5.4"));
        assert!(!supports_openai_reasoning_effort("openai", "gpt-4o"));
        assert!(!supports_openai_reasoning_effort("claude", "gpt-5.4"));
    }
}
