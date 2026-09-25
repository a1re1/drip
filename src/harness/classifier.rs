//! The optional per-loop skill classifier (Typesafe "jev" Decisions API).
//!
//! When a model profile is configured (`runtime.classifier_profile_id`, or the
//! `--classifier <profile-id>` flag), drip asks the Decisions API which
//! discovered skills to compose into a loop's system prompt, gated by a cached
//! per-skill capability-requirements record (`crate::core::skill_requirements`).
//!
//! Nothing here is ever fatal: every failure path returns an `Err`/warning and
//! the run proceeds with only the explicitly activated skills. Requests never
//! carry repository file contents or a transcript — only the goal, task fields,
//! role, tool names, and (for the requirements pass) the skill markdown itself.

use std::collections::BTreeMap;
use std::time::Duration;

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use tokio::task::JoinSet;

use crate::core::config::{
    baseline_setting_values, CLASSIFIER_PROFILE_SETTING_ID, CLASSIFIER_TIMEOUT_MS_SETTING_ID,
};
use crate::core::inference::{resolve_model_profile_route, EnvSource};

/// Wall-clock cap for one classifier request, in milliseconds.
pub const DEFAULT_CLASSIFIER_TIMEOUT_MS: u64 = 8_000;
/// Unauthored skills are included at or above this relevance probability.
pub const UNAUTHORED_RELEVANCE_THRESHOLD: f64 = 0.6;
/// How many unauthored skills one selection may compose.
pub const UNAUTHORED_SELECTION_CAP: usize = 4;
/// The inclusion threshold an authored skill gets when it declares none.
pub const DEFAULT_AUTHORED_THRESHOLD: f64 = 0.6;

// ---------------------------------------------------------------------------
// Settings
// ---------------------------------------------------------------------------

/// The configured classifier profile id — `None` when the classifier is
/// disabled (an empty or missing setting), which is the shipped default.
pub fn classifier_profile_id(settings: &IndexMap<String, String>) -> Option<String> {
    let raw = settings
        .get(CLASSIFIER_PROFILE_SETTING_ID)
        .cloned()
        .or_else(|| {
            baseline_setting_values()
                .get(CLASSIFIER_PROFILE_SETTING_ID)
                .cloned()
        })
        .unwrap_or_default();
    let trimmed = raw.trim();

    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// The configured wall-clock cap for one classifier request, in milliseconds;
/// non-numeric or sub-millisecond values fall back to the default.
pub fn classifier_timeout_ms(settings: &IndexMap<String, String>) -> u64 {
    settings
        .get(CLASSIFIER_TIMEOUT_MS_SETTING_ID)
        .and_then(|raw| raw.trim().parse::<u64>().ok())
        .filter(|ms| *ms > 0)
        .unwrap_or(DEFAULT_CLASSIFIER_TIMEOUT_MS)
}

// ---------------------------------------------------------------------------
// Route resolution
// ---------------------------------------------------------------------------

/// A resolved Decisions-API route: endpoint, model, headers (credentials
/// included — never log them), and the request timeout.
#[derive(Clone)]
pub struct ClassifierRoute {
    pub url: String,
    pub model: String,
    pub headers: Vec<(String, String)>,
    pub timeout_ms: u64,
}

// Hand-written so a stray `{:?}` can never print an Authorization header.
impl std::fmt::Debug for ClassifierRoute {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClassifierRoute")
            .field("url", &self.url)
            .field("model", &self.model)
            .field("headers", &format!("<{} header(s)>", self.headers.len()))
            .field("timeout_ms", &self.timeout_ms)
            .finish()
    }
}

/// Resolves the classifier route from settings.
///
/// `Ok(None)` means the classifier is disabled (no profile id configured).
/// `override_profile` is the `--classifier <id>` flag; `--no-classifier` is
/// applied by the caller before this is reached. Credentials and headers come
/// from the same profile resolution every other provider uses.
///
/// The resolved route's URL ends in `/chat/completions` (built from the
/// profile's base URL); the decisions endpoint is derived from it per provider:
/// openrouter `https://openrouter.ai/api/v1` → `.../api/alpha/decisions`
/// (the `/v1` is dropped). `typesafe` derives nothing: the profile's `baseUrl`
/// *is* the endpoint, so a Decisions profile spells its own path out
/// (`https://api.typesafe.ai/v1/systemone`).
pub fn resolve_classifier_route(
    settings: &IndexMap<String, String>,
    env: EnvSource<'_>,
    override_profile: Option<&str>,
) -> Result<Option<ClassifierRoute>, String> {
    let profile_id = override_profile
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .or_else(|| classifier_profile_id(settings));

    let Some(profile_id) = profile_id else {
        return Ok(None);
    };

    let route = resolve_model_profile_route(settings, &profile_id, env).map_err(|error| {
        format!("classifier: profile \"{profile_id}\" could not be resolved: {error}")
    })?;

    // `/chat/completions` is the chat shape; the decisions endpoint is a
    // sibling path on the same host.
    let base = route
        .url
        .trim_end_matches('/')
        .strip_suffix("/chat/completions")
        .unwrap_or(route.url.trim_end_matches('/'))
        .trim_end_matches('/')
        .to_string();

    let url = match route.provider.as_str() {
        "openrouter" => format!(
            "{}/alpha/decisions",
            base.strip_suffix("/v1").unwrap_or(&base).trim_end_matches('/')
        ),
        // Nothing is appended: the profile's `baseUrl` is the endpoint
        // (`https://api.typesafe.ai/v1/systemone` for the host's Decisions
        // API). A bare `https://api.typesafe.ai/v1` posts to `/v1` itself.
        "typesafe" => base,
        other => {
            return Err(format!(
                "classifier: provider \"{other}\" is not supported for the skill classifier (use \"openrouter\" or \"typesafe\")"
            ))
        }
    };

    Ok(Some(ClassifierRoute {
        url,
        model: route.model.clone(),
        headers: route.headers.clone(),
        timeout_ms: classifier_timeout_ms(settings),
    }))
}

// ---------------------------------------------------------------------------
// Wire types
// ---------------------------------------------------------------------------

/// One answer in a decisions response. Question payloads are authored by hand
/// in native jev JSON, so requests carry `serde_json::Value` questions.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum DecisionAnswer {
    Noul {
        noul: f64,
    },
    Choice {
        choice: String,
        #[serde(default)]
        probabilities: BTreeMap<String, f64>,
        #[serde(default)]
        confidence: f64,
    },
    Score {
        score: f64,
        #[serde(default)]
        legend: BTreeMap<String, String>,
        #[serde(default)]
        probabilities: BTreeMap<String, f64>,
        #[serde(default)]
        confidence: f64,
    },
}

/// A decisions response body.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DecisionsResponse {
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub answers: BTreeMap<String, DecisionAnswer>,
}

/// Posts one decisions request and parses the reply. Every failure (timeout,
/// transport, non-2xx, malformed body) is an `Err(String)`; nothing panics and
/// no response body or credential is ever echoed into the message.
pub async fn ask(
    route: &ClassifierRoute,
    state: Value,
    questions: Map<String, Value>,
) -> Result<DecisionsResponse, String> {
    let bound = Duration::from_millis(route.timeout_ms.max(1));
    let body = serde_json::json!({
        "model": route.model,
        "state": state,
        "questions": questions,
    });
    // Both awaits below are bounded by an explicit `tokio::time::timeout`,
    // so the client carries no timeout of its own: a request-level timeout
    // here would race the outer one and make the error text nondeterministic.
    // One shared client: a loop can fan out one request per authored skill,
    // and each fresh client would otherwise carry its own connection pool.
    static CLIENT: std::sync::OnceLock<Result<reqwest::Client, String>> =
        std::sync::OnceLock::new();
    let client = CLIENT
        .get_or_init(|| {
            reqwest::Client::builder()
                .build()
                .map_err(|error| format!("classifier: could not build the HTTP client: {error}"))
        })
        .clone()?;

    let mut request = client.post(&route.url).json(&body);
    for (key, value) in &route.headers {
        request = request.header(key.as_str(), value.as_str());
    }

    let response = match tokio::time::timeout(bound, request.send()).await {
        Err(_) => {
            return Err(format!(
                "classifier: request timed out after {}ms",
                route.timeout_ms
            ))
        }
        Ok(Err(error)) => return Err(format!("classifier: request failed: {error}")),
        Ok(Ok(response)) => response,
    };
    let status = response.status();

    if !status.is_success() {
        // Status only: a body can echo credentials back.
        return Err(format!(
            "classifier: endpoint returned HTTP {}",
            status.as_u16()
        ));
    }

    let text = match tokio::time::timeout(bound, response.text()).await {
        Err(_) => {
            return Err(format!(
                "classifier: response timed out after {}ms",
                route.timeout_ms
            ))
        }
        Ok(Err(error)) => return Err(format!("classifier: response could not be read: {error}")),
        Ok(Ok(text)) => text,
    };

    serde_json::from_str::<DecisionsResponse>(&text)
        .map_err(|error| format!("classifier: response was not valid decisions JSON: {error}"))
}

// ---------------------------------------------------------------------------
// Answer normalisation
// ---------------------------------------------------------------------------

/// Normalizes an answer to 0..1: a noul is already a probability of yes, a
/// choice is the probability of its top option, and a score is its position
/// among the legend levels (0 when there are fewer than two levels).
pub fn answer_value(answer: &DecisionAnswer) -> f64 {
    match answer {
        DecisionAnswer::Noul { noul } => *noul,
        DecisionAnswer::Choice {
            choice,
            probabilities,
            ..
        } => probabilities.get(choice).copied().unwrap_or(0.0),
        DecisionAnswer::Score { score, legend, .. } => {
            let levels = legend.len() as f64;

            if levels < 2.0 {
                0.0
            } else {
                score / (levels - 1.0)
            }
        }
    }
}

/// One option's (choice) or level's (score) probability. `None` for a noul,
/// which carries no per-member probabilities.
pub fn answer_member(answer: &DecisionAnswer, member: &str) -> Option<f64> {
    match answer {
        DecisionAnswer::Noul { .. } => None,
        DecisionAnswer::Choice { probabilities, .. } => probabilities.get(member).copied(),
        DecisionAnswer::Score { probabilities, .. } => probabilities.get(member).copied(),
    }
}

// ---------------------------------------------------------------------------
// Formula evaluation
// ---------------------------------------------------------------------------

/// Evaluates an author-written relevance formula. Grammar:
///
/// ```text
/// expr    := term (('+' | '-') term)*
/// term    := unary (('*' | '/') unary)*
/// unary   := '-' unary | primary
/// primary := number | ident ('.' member)? | func '(' expr (',' expr)* ')' | '(' expr ')'
/// member  := ident | digits
/// func    := max | min | clamp | abs
/// ```
///
/// An unknown variable, an unknown function, or a syntax error is an `Err`;
/// division by zero yields 0. Dependency-free (a hand-written recursive-descent
/// parser) so no crate is added for it.
pub fn eval_formula(formula: &str, vars: &dyn Fn(&str) -> Option<f64>) -> Result<f64, String> {
    if formula.trim().is_empty() {
        return Err("formula is empty".to_string());
    }

    FormulaParser::new(formula, vars).parse()
}

struct FormulaParser<'a> {
    chars: Vec<char>,
    pos: usize,
    vars: &'a dyn Fn(&str) -> Option<f64>,
}

impl<'a> FormulaParser<'a> {
    fn new(formula: &str, vars: &'a dyn Fn(&str) -> Option<f64>) -> Self {
        Self {
            chars: formula.chars().collect(),
            pos: 0,
            vars,
        }
    }

    fn skip_ws(&mut self) {
        while self.pos < self.chars.len() && self.chars[self.pos].is_whitespace() {
            self.pos += 1;
        }
    }

    fn peek(&mut self) -> Option<char> {
        self.skip_ws();
        self.chars.get(self.pos).copied()
    }

    fn eat(&mut self, expected: char) -> bool {
        if self.peek() == Some(expected) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn parse(mut self) -> Result<f64, String> {
        let value = self.expression()?;

        if let Some(rest) = self.peek() {
            return Err(format!("unexpected character \"{rest}\" in formula"));
        }

        if !value.is_finite() {
            return Err("formula produced a non-finite value".to_string());
        }

        Ok(value)
    }

    fn expression(&mut self) -> Result<f64, String> {
        let mut value = self.term()?;

        loop {
            match self.peek() {
                Some('+') => {
                    self.pos += 1;
                    value += self.term()?;
                }
                Some('-') => {
                    self.pos += 1;
                    value -= self.term()?;
                }
                _ => return Ok(value),
            }
        }
    }

    fn term(&mut self) -> Result<f64, String> {
        let mut value = self.unary()?;

        loop {
            match self.peek() {
                Some('*') => {
                    self.pos += 1;
                    value *= self.unary()?;
                }
                Some('/') => {
                    self.pos += 1;
                    let divisor = self.unary()?;
                    // Division by zero is defined as 0 so a malformed formula
                    // can only under-select, never poison an answer.
                    value = if divisor == 0.0 { 0.0 } else { value / divisor };
                }
                _ => return Ok(value),
            }
        }
    }

    fn unary(&mut self) -> Result<f64, String> {
        if self.eat('-') {
            Ok(-self.unary()?)
        } else {
            self.primary()
        }
    }

    fn primary(&mut self) -> Result<f64, String> {
        let current = self
            .peek()
            .ok_or_else(|| "formula ended early".to_string())?;

        if current == '(' {
            self.pos += 1;
            let value = self.expression()?;

            if !self.eat(')') {
                return Err("missing closing parenthesis in formula".to_string());
            }

            return Ok(value);
        }

        if current.is_ascii_digit() || current == '.' {
            return self.number();
        }

        if current.is_ascii_alphabetic() || current == '_' {
            let name = self.identifier();

            if self.peek() == Some('(') {
                self.pos += 1;
                let mut args: Vec<f64> = Vec::new();

                if self.peek() != Some(')') {
                    loop {
                        args.push(self.expression()?);

                        if self.eat(',') {
                            continue;
                        }

                        break;
                    }
                }

                if !self.eat(')') {
                    return Err("missing closing parenthesis in formula".to_string());
                }

                return call_function(&name, &args);
            }

            let mut path = name.clone();

            if self.peek() == Some('.') {
                self.pos += 1;
                let member = self.member().ok_or_else(|| {
                    format!("expected an option or level name after \"{name}.\" in formula")
                })?;
                path.push('.');
                path.push_str(&member);
            }

            return (self.vars)(&path)
                .ok_or_else(|| format!("unknown variable \"{path}\" in formula"));
        }

        Err(format!("unexpected character \"{current}\" in formula"))
    }

    fn number(&mut self) -> Result<f64, String> {
        let start = self.pos;

        while self.pos < self.chars.len() && self.chars[self.pos].is_ascii_digit() {
            self.pos += 1;
        }

        if self.pos < self.chars.len() && self.chars[self.pos] == '.' {
            self.pos += 1;

            while self.pos < self.chars.len() && self.chars[self.pos].is_ascii_digit() {
                self.pos += 1;
            }
        }

        let text: String = self.chars[start..self.pos].iter().collect();

        text.parse::<f64>()
            .map_err(|_| format!("invalid number \"{text}\" in formula"))
    }

    fn identifier(&mut self) -> String {
        let start = self.pos;
        self.pos += 1;

        while self.pos < self.chars.len() {
            let current = self.chars[self.pos];

            if current.is_ascii_alphanumeric() || current == '_' {
                self.pos += 1;
            } else {
                break;
            }
        }

        self.chars[start..self.pos].iter().collect()
    }

    /// An option/level member: an identifier or a bare index (`risk.0`).
    fn member(&mut self) -> Option<String> {
        let current = self.peek()?;

        if current.is_ascii_alphabetic() || current == '_' {
            return Some(self.identifier());
        }

        if current.is_ascii_digit() {
            let start = self.pos;

            while self.pos < self.chars.len() && self.chars[self.pos].is_ascii_digit() {
                self.pos += 1;
            }

            return Some(self.chars[start..self.pos].iter().collect());
        }

        None
    }
}

fn call_function(name: &str, args: &[f64]) -> Result<f64, String> {
    match name {
        "max" if !args.is_empty() => Ok(args.iter().copied().fold(f64::NEG_INFINITY, f64::max)),
        "min" if !args.is_empty() => Ok(args.iter().copied().fold(f64::INFINITY, f64::min)),
        "abs" if args.len() == 1 => Ok(args[0].abs()),
        "clamp" if args.len() == 3 => {
            let (value, low, high) = (args[0], args[1], args[2]);
            let (low, high) = if low <= high {
                (low, high)
            } else {
                (high, low)
            };
            Ok(value.max(low).min(high))
        }
        "max" | "min" | "abs" | "clamp" => Err(format!(
            "wrong number of arguments for \"{name}\" in formula"
        )),
        other => Err(format!("unknown function \"{other}\" in formula")),
    }
}

// ---------------------------------------------------------------------------
// classifiers.json
// ---------------------------------------------------------------------------

/// A skill's optional `classifiers.json` sidecar.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SkillClassifiers {
    #[serde(default)]
    pub relevance: Option<RelevanceSpec>,
    #[serde(default)]
    pub requirements: Option<DeclaredRequirements>,
}

/// An authored relevance pass: native jev questions plus an optional formula
/// over their answers.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RelevanceSpec {
    #[serde(default)]
    pub threshold: Option<f64>,
    #[serde(default)]
    pub questions: Map<String, Value>,
    #[serde(default)]
    pub formula: Option<String>,
}

/// Requirements the author states outright: no classifier call, no cache row.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeclaredRequirements {
    #[serde(default)]
    pub tools: Vec<String>,
    #[serde(default)]
    pub mcp_servers: Vec<String>,
}

/// Reads `<dir of SKILL.md>/classifiers.json`.
///
/// `None` when the file is absent (or the skill is built in — built-ins can
/// never be authored). `Some(Err)` when the file is malformed; the caller
/// warns once per skill and treats the skill as unauthored.
pub fn load_skill_classifiers(skill_md_path: &str) -> Option<Result<SkillClassifiers, String>> {
    if skill_md_path.starts_with("<builtin>") {
        return None;
    }

    let path = std::path::Path::new(skill_md_path)
        .parent()?
        .join("classifiers.json");

    if !path.exists() {
        return None;
    }

    let raw = match std::fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(error) => {
            return Some(Err(format!(
                "could not read {}: {error}",
                path.to_string_lossy()
            )))
        }
    };

    Some(
        serde_json::from_str::<SkillClassifiers>(&raw)
            .map_err(|error| format!("malformed {}: {error}", path.to_string_lossy())),
    )
}

// ---------------------------------------------------------------------------
// Selection
// ---------------------------------------------------------------------------

/// One discovered skill offered to the classifier.
#[derive(Debug, Clone, PartialEq)]
pub struct SkillCandidate {
    pub name: String,
    pub description: String,
    pub classifiers: Option<SkillClassifiers>,
}

/// One pooled skill the harness may compose into a loop: the loaded markdown,
/// the skill's own `classifiers.json` (when authored), and the cached
/// capability requirements that gate whether this loop can use it at all.
///
/// The CLI builds these (`--skill` activations are excluded — they are already
/// in the base prompt).
#[derive(Debug, Clone, PartialEq)]
pub struct DynamicSkill {
    pub name: String,
    pub description: String,
    /// The skill's markdown, as `load_skill_content` returned it.
    pub content: String,
    pub classifiers: Option<SkillClassifiers>,
    pub requirements: crate::core::skill_requirements::SkillRequirements,
}

/// What one selection decided: the skills to compose (name, score) and every
/// non-fatal warning the caller should surface.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SkillSelection {
    pub selected: Vec<(String, f64)>,
    pub warnings: Vec<String>,
}

enum SelectionOutcome {
    Unauthored {
        selected: Vec<(String, f64)>,
        warnings: Vec<String>,
    },
    Authored {
        name: String,
        score: Option<f64>,
        warnings: Vec<String>,
    },
}

/// Decides which candidate skills this loop should compose.
///
/// - Candidates WITHOUT an authored `relevance` share ONE request (one noul
///   question each); they are included at probability >= 0.6, ranked by
///   probability descending, and capped to the top 4.
/// - Candidates WITH an authored `relevance` get their OWN request (their
///   questions verbatim, the same state), scored by their formula (or the max
///   answer value when they declare none) and included at or above their own
///   threshold (0.6 when absent). NO cap applies to authored skills.
///
/// All requests run concurrently and the whole selection is bounded by
/// `route.timeout_ms`. Authored selections come first, then unauthored; each
/// group is ordered by score descending.
pub async fn select_skills(
    route: &ClassifierRoute,
    state: Value,
    candidates: &[SkillCandidate],
) -> SkillSelection {
    let mut unauthored: Vec<(usize, String, String)> = Vec::new();
    let mut authored: Vec<(String, RelevanceSpec)> = Vec::new();

    for (index, candidate) in candidates.iter().enumerate() {
        match candidate
            .classifiers
            .as_ref()
            .and_then(|entry| entry.relevance.as_ref())
        {
            Some(spec) => authored.push((candidate.name.clone(), spec.clone())),
            None => unauthored.push((index, candidate.name.clone(), candidate.description.clone())),
        }
    }

    if unauthored.is_empty() && authored.is_empty() {
        return SkillSelection::default();
    }

    let bound_ms = route.timeout_ms.max(1);
    let mut set: JoinSet<SelectionOutcome> = JoinSet::new();

    if !unauthored.is_empty() {
        let route = route.clone();
        let state = state.clone();
        set.spawn(async move { select_unauthored_batch(&route, state, unauthored).await });
    }

    for (name, spec) in authored {
        let route = route.clone();
        let state = state.clone();
        set.spawn(async move { select_authored_skill(&route, state, name, spec).await });
    }

    let collect = async move {
        let mut outcomes: Vec<SelectionOutcome> = Vec::new();
        let mut warnings: Vec<String> = Vec::new();

        while let Some(joined) = set.join_next().await {
            match joined {
                Ok(outcome) => outcomes.push(outcome),
                Err(error) => warnings.push(format!("classifier: selection task failed: {error}")),
            }
        }

        (outcomes, warnings)
    };

    let (outcomes, mut warnings) =
        match tokio::time::timeout(Duration::from_millis(bound_ms), collect).await {
            Ok(result) => result,
            Err(_) => (
                Vec::new(),
                vec![format!(
                    "classifier: skill selection timed out after {bound_ms}ms — composing no dynamic skills for this loop"
                )],
            ),
        };

    let mut authored_selected: Vec<(String, f64)> = Vec::new();
    let mut unauthored_selected: Vec<(String, f64)> = Vec::new();

    for outcome in outcomes {
        match outcome {
            SelectionOutcome::Unauthored {
                selected,
                warnings: extra,
            } => {
                unauthored_selected.extend(selected);
                warnings.extend(extra);
            }
            SelectionOutcome::Authored {
                name,
                score,
                warnings: extra,
            } => {
                if let Some(score) = score {
                    authored_selected.push((name, score));
                }
                warnings.extend(extra);
            }
        }
    }

    by_score_descending(&mut authored_selected);
    by_score_descending(&mut unauthored_selected);

    let mut selected = authored_selected;
    selected.extend(unauthored_selected);

    SkillSelection { selected, warnings }
}

fn by_score_descending(entries: &mut [(String, f64)]) {
    entries.sort_by(|left, right| {
        right
            .1
            .partial_cmp(&left.1)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
}

async fn select_unauthored_batch(
    route: &ClassifierRoute,
    state: Value,
    batch: Vec<(usize, String, String)>,
) -> SelectionOutcome {
    let mut questions: Map<String, Value> = Map::new();

    for (index, name, description) in &batch {
        questions.insert(
            format!("skill_{index}"),
            // Structured instructions, per the typesafe guidance: the question
            // names the sibling `skill` field in backticks and the model reads
            // the referenced object, so the identity travels as data, not as
            // text spliced into the sentence.
            serde_json::json!({
                "type": "noul",
                "instructions": {
                    "question": "Would following the skill `skill` help complete the current task described in the state?",
                    "skill": { "name": name, "description": description },
                },
            }),
        );
    }

    let response = match ask(route, state, questions).await {
        Ok(response) => response,
        Err(error) => {
            return SelectionOutcome::Unauthored {
                selected: Vec::new(),
                warnings: vec![format!(
                    "classifier: skill relevance request failed: {error}"
                )],
            }
        }
    };

    let mut scored: Vec<(String, f64)> = batch
        .iter()
        .filter_map(|(index, name, _)| {
            let answer = response.answers.get(&format!("skill_{index}"))?;
            let value = answer_value(answer);

            if value.is_finite() && value >= UNAUTHORED_RELEVANCE_THRESHOLD {
                Some((name.clone(), value))
            } else {
                None
            }
        })
        .collect();

    by_score_descending(&mut scored);
    scored.truncate(UNAUTHORED_SELECTION_CAP);

    SelectionOutcome::Unauthored {
        selected: scored,
        warnings: Vec::new(),
    }
}

async fn select_authored_skill(
    route: &ClassifierRoute,
    state: Value,
    name: String,
    spec: RelevanceSpec,
) -> SelectionOutcome {
    let label = name.clone();
    let response = match ask(route, state, spec.questions.clone()).await {
        Ok(response) => response,
        Err(error) => {
            return SelectionOutcome::Authored {
                name,
                score: None,
                warnings: vec![format!("classifier: skill \"{label}\" dropped: {error}")],
            }
        }
    };

    let threshold = spec
        .threshold
        .filter(|value| value.is_finite())
        .unwrap_or(DEFAULT_AUTHORED_THRESHOLD);

    let raw = match spec.formula.as_deref() {
        Some(formula) => {
            let lookup = |var: &str| answer_var(&response, var);

            match eval_formula(formula, &lookup) {
                Ok(value) => value,
                Err(error) => {
                    return SelectionOutcome::Authored {
                        name,
                        score: None,
                        warnings: vec![format!("classifier: skill \"{label}\" dropped: {error}")],
                    }
                }
            }
        }
        None => {
            if response.answers.is_empty() {
                0.0
            } else {
                response
                    .answers
                    .values()
                    .map(answer_value)
                    .fold(f64::NEG_INFINITY, f64::max)
            }
        }
    };

    let score = if raw.is_finite() {
        raw.clamp(0.0, 1.0)
    } else {
        0.0
    };

    if score >= threshold {
        SelectionOutcome::Authored {
            name,
            score: Some(score),
            warnings: Vec::new(),
        }
    } else {
        SelectionOutcome::Authored {
            name,
            score: None,
            warnings: Vec::new(),
        }
    }
}

/// Resolves one formula variable: a bare question id reads its normalized
/// answer value, and `id.member` reads that option's (or level's) probability.
fn answer_var(response: &DecisionsResponse, var: &str) -> Option<f64> {
    match var.split_once('.') {
        Some((id, member)) => response
            .answers
            .get(id)
            .and_then(|answer| answer_member(answer, member)),
        None => response.answers.get(var).map(answer_value),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::config::{default_setting_values, MODEL_PROFILES_SETTING_ID};

    // --- helpers ---

    fn env_with(key: &str) -> std::collections::HashMap<String, String> {
        let mut env = std::collections::HashMap::new();
        env.insert(key.to_string(), "test-key".to_string());
        env
    }

    fn settings_with_profile(provider: &str, base_url: Option<&str>) -> IndexMap<String, String> {
        let mut settings = default_setting_values();
        let base = base_url
            .map(|url| format!(",\"baseUrl\":\"{url}\""))
            .unwrap_or_default();
        settings.insert(
            MODEL_PROFILES_SETTING_ID.to_string(),
            format!(
                "[{{\"id\":\"jev\",\"label\":\"Jev\",\"model\":\"~typesafe/jev-latest\",\"provider\":\"{provider}\",\"apiKeyRef\":\"env:TYPESAFE_API_KEY\"{base}}}]",
            ),
        );
        settings.insert(CLASSIFIER_PROFILE_SETTING_ID.to_string(), "jev".to_string());
        settings
    }

    fn hand_route(base: &str, timeout_ms: u64) -> ClassifierRoute {
        ClassifierRoute {
            url: format!("{base}/alpha/decisions"),
            model: "~typesafe/jev-latest".to_string(),
            headers: vec![("Authorization".to_string(), "Bearer test-key".to_string())],
            timeout_ms,
        }
    }

    /// HTTP/1.1 mock over a std TcpListener: accepts one connection per canned
    /// response (in order) and returns the request bodies it received.
    fn spawn_mock(responses: Vec<&'static str>) -> (String, std::thread::JoinHandle<Vec<String>>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = std::thread::spawn(move || {
            let mut bodies: Vec<String> = Vec::new();

            for body in responses {
                let (mut stream, _) = listener.accept().unwrap();
                let mut data: Vec<u8> = Vec::new();
                let mut chunk = [0u8; 4096];
                let body_start = loop {
                    let read = std::io::Read::read(&mut stream, &mut chunk).unwrap_or(0);
                    assert!(read > 0, "client closed before sending a full request");
                    data.extend_from_slice(&chunk[..read]);

                    if let Some(pos) = data.windows(4).position(|window| window == b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&data[..pos]).to_ascii_lowercase();
                        let length = head
                            .lines()
                            .find_map(|line| {
                                line.strip_prefix("content-length:")
                                    .and_then(|value| value.trim().parse::<usize>().ok())
                            })
                            .unwrap_or(0);

                        if data.len() >= pos + 4 + length {
                            break pos + 4;
                        }
                    }
                };
                bodies.push(String::from_utf8_lossy(&data[body_start..]).to_string());
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                std::io::Write::write_all(&mut stream, response.as_bytes()).unwrap();
            }

            bodies
        });

        (format!("http://127.0.0.1:{port}"), handle)
    }

    // --- settings accessors ---

    #[test]
    fn classifier_settings_default_off_and_timeout_defaults() {
        let settings = default_setting_values();
        assert_eq!(classifier_profile_id(&settings), None);
        assert_eq!(
            classifier_timeout_ms(&settings),
            DEFAULT_CLASSIFIER_TIMEOUT_MS
        );

        let mut settings = settings;
        settings.insert(CLASSIFIER_PROFILE_SETTING_ID.to_string(), "  ".to_string());
        assert_eq!(classifier_profile_id(&settings), None);
        settings.insert(
            CLASSIFIER_PROFILE_SETTING_ID.to_string(),
            " jev ".to_string(),
        );
        assert_eq!(classifier_profile_id(&settings).as_deref(), Some("jev"));

        for value in ["nope", "0", "-5"] {
            settings.insert(
                CLASSIFIER_TIMEOUT_MS_SETTING_ID.to_string(),
                value.to_string(),
            );
            assert_eq!(
                classifier_timeout_ms(&settings),
                DEFAULT_CLASSIFIER_TIMEOUT_MS
            );
        }
        settings.insert(
            CLASSIFIER_TIMEOUT_MS_SETTING_ID.to_string(),
            "1234".to_string(),
        );
        assert_eq!(classifier_timeout_ms(&settings), 1234);
    }

    // --- route resolution ---

    #[test]
    fn classifier_route_resolves_the_decisions_endpoint_per_provider() {
        let env = env_with("TYPESAFE_API_KEY");
        let override_profile = Some("jev");

        let openrouter = settings_with_profile("openrouter", None);
        let route = resolve_classifier_route(&openrouter, Some(&env), override_profile)
            .unwrap()
            .unwrap();
        assert_eq!(route.url, "https://openrouter.ai/api/alpha/decisions");
        assert_eq!(route.model, "~typesafe/jev-latest");
        assert!(route
            .headers
            .iter()
            .any(|(key, value)| key == "Authorization" && value == "Bearer test-key"));

        // typesafe derives no suffix: the profile's own baseUrl is the
        // endpoint, so nothing is appended to it.
        let typesafe = settings_with_profile("typesafe", None);
        let route = resolve_classifier_route(&typesafe, Some(&env), override_profile)
            .unwrap()
            .unwrap();
        assert_eq!(route.url, "https://api.typesafe.ai/v1");

        let decisions =
            settings_with_profile("typesafe", Some("https://api.typesafe.ai/v1/systemone"));
        let route = resolve_classifier_route(&decisions, Some(&env), override_profile)
            .unwrap()
            .unwrap();
        assert_eq!(route.url, "https://api.typesafe.ai/v1/systemone");

        // A trailing slash on the profile's endpoint is harmless.
        let trailing =
            settings_with_profile("typesafe", Some("https://api.typesafe.ai/v1/systemone/"));
        let route = resolve_classifier_route(&trailing, Some(&env), override_profile)
            .unwrap()
            .unwrap();
        assert_eq!(route.url, "https://api.typesafe.ai/v1/systemone");

        // The override wins over the setting; an unset override falls back to it.
        let mut settings = settings_with_profile("typesafe", None);
        settings.insert(CLASSIFIER_PROFILE_SETTING_ID.to_string(), String::new());
        assert!(resolve_classifier_route(&settings, Some(&env), None)
            .unwrap()
            .is_none());
        assert!(resolve_classifier_route(&settings, Some(&env), Some("  "))
            .unwrap()
            .is_none());
        assert!(resolve_classifier_route(&settings, Some(&env), Some("jev"))
            .unwrap()
            .is_some());
    }

    #[test]
    fn classifier_route_rejects_an_unsupported_provider() {
        let mut settings = settings_with_profile("claude", None);
        settings.insert(
            MODEL_PROFILES_SETTING_ID.to_string(),
            "[{\"id\":\"jev\",\"label\":\"Jev\",\"model\":\"claude-sonnet-4-5\",\"provider\":\"claude\",\"apiKeyRef\":\"env:ANTHROPIC_API_KEY\"}]".to_string(),
        );
        let env = env_with("ANTHROPIC_API_KEY");
        let error = resolve_classifier_route(&settings, Some(&env), Some("jev")).unwrap_err();
        assert!(error.contains("not supported"), "{error}");

        // An unknown profile id is an error too, never a panic.
        let error = resolve_classifier_route(&settings, Some(&env), Some("nope")).unwrap_err();
        assert!(error.contains("could not be resolved"), "{error}");
    }

    // --- ask() ---

    #[tokio::test]
    async fn ask_round_trips_all_three_answer_types() {
        let body = r#"{"model":"jev-1.13.0","answers":{"a":{"type":"noul","noul":0.95},"b":{"type":"choice","choice":"implement","probabilities":{"design":0.1,"implement":0.88},"confidence":0.81},"c":{"type":"score","score":1.05,"legend":{"0":"trivial","1":"moderate"},"probabilities":{"0":0.0,"1":0.95},"confidence":0.92}},"usage":{}}"#;
        let (base, server) = spawn_mock(vec![body]);
        let route = hand_route(&base, 10_000);
        let mut questions = Map::new();
        questions.insert(
            "a".to_string(),
            serde_json::json!({"type": "noul", "instructions": "?"}),
        );

        let response = ask(&route, serde_json::json!({"goal": "x"}), questions)
            .await
            .unwrap();

        assert_eq!(response.model, "jev-1.13.0");
        assert_eq!(response.answers.len(), 3);
        let bodies = server.join().unwrap();
        assert_eq!(bodies.len(), 1);
        let sent: Value = serde_json::from_str(&bodies[0]).unwrap();
        assert_eq!(sent["model"], serde_json::json!("~typesafe/jev-latest"));
        assert_eq!(sent["state"]["goal"], serde_json::json!("x"));
        assert!(sent["questions"]["a"].is_object());
    }

    #[tokio::test]
    async fn ask_times_out_without_panicking() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let route = hand_route(&format!("http://{addr}"), 150);

        let error = ask(&route, serde_json::json!({}), Map::new())
            .await
            .unwrap_err();

        assert!(error.contains("timed out"), "{error}");
    }

    #[tokio::test]
    async fn ask_reports_a_malformed_body_as_an_error() {
        let (base, server) = spawn_mock(vec!["not json at all"]);
        let route = hand_route(&base, 10_000);

        let error = ask(&route, serde_json::json!({}), Map::new())
            .await
            .unwrap_err();

        assert!(error.contains("not valid decisions JSON"), "{error}");
        server.join().unwrap();
    }

    // --- answer normalisation ---

    #[test]
    fn answer_value_and_member_normalize_all_three_types() {
        let noul = DecisionAnswer::Noul { noul: 0.95 };
        assert_eq!(answer_value(&noul), 0.95);
        assert_eq!(answer_member(&noul, "anything"), None);

        let choice = DecisionAnswer::Choice {
            choice: "implement".to_string(),
            probabilities: BTreeMap::from([
                ("design".to_string(), 0.1),
                ("implement".to_string(), 0.88),
            ]),
            confidence: 0.81,
        };
        assert!((answer_value(&choice) - 0.88).abs() < 1e-12);
        assert_eq!(answer_member(&choice, "design"), Some(0.1));
        assert_eq!(answer_member(&choice, "missing"), None);

        let score = DecisionAnswer::Score {
            score: 1.05,
            legend: BTreeMap::from([
                ("0".to_string(), "trivial".to_string()),
                ("1".to_string(), "moderate".to_string()),
                ("2".to_string(), "bad".to_string()),
            ]),
            probabilities: BTreeMap::from([("1".to_string(), 0.95)]),
            confidence: 0.92,
        };
        // 1.05 / (3 - 1) = 0.525
        assert!((answer_value(&score) - 0.525).abs() < 1e-12);
        assert_eq!(answer_member(&score, "1"), Some(0.95));

        let degenerate = DecisionAnswer::Score {
            score: 4.0,
            legend: BTreeMap::from([("0".to_string(), "only".to_string())]),
            probabilities: BTreeMap::new(),
            confidence: 0.5,
        };
        assert_eq!(answer_value(&degenerate), 0.0);
    }

    // --- formula ---

    fn vars(name: &str) -> Option<f64> {
        match name {
            "is_migration" => Some(1.0),
            "risk" => Some(0.5),
            "phase.implement" => Some(0.9),
            "phase.review" => Some(0.4),
            "risk.0" => Some(0.25),
            _ => None,
        }
    }

    #[test]
    fn formula_handles_precedence_parentheses_and_functions() {
        assert_eq!(eval_formula("1 + 2 * 3", &vars).unwrap(), 7.0);
        assert_eq!(eval_formula("(1 + 2) * 3", &vars).unwrap(), 9.0);
        assert_eq!(eval_formula("-2 + 10", &vars).unwrap(), 8.0);
        assert_eq!(
            eval_formula("max(1, 2, 3) - min(4, 5)", &vars).unwrap(),
            -1.0
        );
        assert_eq!(eval_formula("clamp(5, 0, 1)", &vars).unwrap(), 1.0);
        assert_eq!(eval_formula("abs(0 - 3)", &vars).unwrap(), 3.0);
        assert!(
            (eval_formula("0.5 * is_migration + 0.3 * risk", &vars).unwrap() - 0.65).abs() < 1e-9
        );
        assert!(
            (eval_formula(
                "0.5 * is_migration + 0.3 * risk + 0.2 * max(phase.implement, phase.review)",
                &vars,
            )
            .unwrap()
                - 0.83)
                .abs()
                < 1e-9
        );
    }

    #[test]
    fn formula_handles_member_access() {
        assert_eq!(eval_formula("phase.implement", &vars).unwrap(), 0.9);
        assert_eq!(eval_formula("risk.0", &vars).unwrap(), 0.25);
        assert_eq!(
            eval_formula("phase.implement + risk.0", &vars).unwrap(),
            1.15
        );
    }

    #[test]
    fn formula_reports_unknown_variables_syntax_and_division_by_zero() {
        let error = eval_formula("nope + 1", &vars).unwrap_err();
        assert!(error.contains("unknown variable"), "{error}");
        let error = eval_formula("phase.missing", &vars).unwrap_err();
        assert!(error.contains("unknown variable"), "{error}");
        let error = eval_formula("1 + * 2", &vars).unwrap_err();
        assert!(error.contains("unexpected character"), "{error}");
        let error = eval_formula("(1 + 2", &vars).unwrap_err();
        assert!(error.contains("missing closing parenthesis"), "{error}");
        let error = eval_formula("frob(1)", &vars).unwrap_err();
        assert!(error.contains("unknown function"), "{error}");
        let error = eval_formula("clamp(1, 2)", &vars).unwrap_err();
        assert!(error.contains("wrong number of arguments"), "{error}");
        assert!(eval_formula("  ", &vars).unwrap_err().contains("empty"));
        assert_eq!(eval_formula("1 / 0", &vars).unwrap(), 0.0);
        assert_eq!(eval_formula("1 / (2 - 2)", &vars).unwrap(), 0.0);
    }

    // --- classifiers.json ---

    #[test]
    fn load_skill_classifiers_reads_the_sidecar() {
        assert!(load_skill_classifiers("<builtin>/tdd/SKILL.md").is_none());

        let dir = tempfile::tempdir().unwrap();
        let skill_path = dir.path().join("SKILL.md");
        std::fs::write(&skill_path, "# Skill").unwrap();
        let skill_path = skill_path.to_string_lossy().into_owned();

        // Absent → None.
        assert!(load_skill_classifiers(&skill_path).is_none());

        // Malformed → Some(Err).
        std::fs::write(dir.path().join("classifiers.json"), "{ not json").unwrap();
        let malformed = load_skill_classifiers(&skill_path).unwrap();
        assert!(malformed.is_err());

        // Valid → parsed, including the camelCase mcpServers key.
        std::fs::write(
            dir.path().join("classifiers.json"),
            r#"{
                "relevance": {
                    "threshold": 0.65,
                    "questions": { "is_migration": { "type": "noul", "instructions": "?" } },
                    "formula": "is_migration"
                },
                "requirements": { "tools": ["BASH", "PATCH"], "mcpServers": ["fake"] }
            }"#,
        )
        .unwrap();
        let parsed = load_skill_classifiers(&skill_path).unwrap().unwrap();
        let relevance = parsed.relevance.as_ref().unwrap();
        assert_eq!(relevance.threshold, Some(0.65));
        assert_eq!(relevance.formula.as_deref(), Some("is_migration"));
        assert!(relevance.questions.contains_key("is_migration"));
        let requirements = parsed.requirements.as_ref().unwrap();
        assert_eq!(requirements.tools, vec!["BASH", "PATCH"]);
        assert_eq!(requirements.mcp_servers, vec!["fake"]);
    }

    // --- selection ---

    fn noul_response(entries: &[(&str, f64)]) -> String {
        let answers: Vec<String> = entries
            .iter()
            .map(|(id, value)| format!("\"{id}\":{{\"type\":\"noul\",\"noul\":{value}}}"))
            .collect();
        format!(
            "{{\"model\":\"jev\",\"answers\":{{{}}}}}",
            answers.join(",")
        )
    }

    // The response body must outlive the request, so it is leaked into a
    // 'static &str for the mock server (test-only, one small string).
    fn leak(body: String) -> &'static str {
        Box::leak(body.into_boxed_str())
    }

    fn unauthored(name: &str) -> SkillCandidate {
        SkillCandidate {
            name: name.to_string(),
            description: format!("{name} description"),
            classifiers: None,
        }
    }

    fn authored(name: &str, spec: Value) -> SkillCandidate {
        let classifiers: SkillClassifiers =
            serde_json::from_value(serde_json::json!({ "relevance": spec })).unwrap();
        SkillCandidate {
            name: name.to_string(),
            description: format!("{name} description"),
            classifiers: Some(classifiers),
        }
    }

    #[tokio::test]
    async fn unauthored_skills_are_thresholded_sorted_and_capped() {
        // Six candidates; values below 0.6 are dropped and only the top 4 kept.
        let body = noul_response(&[
            ("skill_0", 0.9),
            ("skill_1", 0.4),
            ("skill_2", 0.7),
            ("skill_3", 0.65),
            ("skill_4", 0.8),
            ("skill_5", 0.95),
        ]);
        let (base, server) = spawn_mock(vec![leak(body)]);
        let route = hand_route(&base, 10_000);
        let candidates = vec![
            unauthored("a"),
            unauthored("b"),
            unauthored("c"),
            unauthored("d"),
            unauthored("e"),
            unauthored("f"),
        ];

        let selection = select_skills(&route, serde_json::json!({"goal": "g"}), &candidates).await;

        assert!(selection.warnings.is_empty(), "{:?}", selection.warnings);
        let names: Vec<&str> = selection
            .selected
            .iter()
            .map(|(name, _)| name.as_str())
            .collect();
        assert_eq!(names, vec!["f", "a", "e", "c"]);
        // ONE batched request for every unauthored candidate.
        let bodies = server.join().unwrap();
        assert_eq!(bodies.len(), 1);
        let sent: Value = serde_json::from_str(&bodies[0]).unwrap();
        assert_eq!(sent["questions"].as_object().unwrap().len(), 6);
        assert_eq!(
            sent["questions"]["skill_0"]["type"],
            serde_json::json!("noul")
        );
        assert_eq!(
            sent["questions"]["skill_0"]["instructions"]["question"],
            serde_json::json!("Would following the skill `skill` help complete the current task described in the state?")
        );
    }

    #[tokio::test]
    async fn authored_skills_use_their_own_threshold_and_are_not_capped() {
        // Five authored skills, all above their own threshold: no cap applies.
        // Each gets its own request; the mock serves the same body to all.
        let body = noul_response(&[("q", 0.8)]);
        let (base, server) = spawn_mock(vec![leak(body.clone()); 5]);
        let route = hand_route(&base, 10_000);
        let spec = serde_json::json!({
            "threshold": 0.75,
            "questions": { "q": { "type": "noul", "instructions": "?" } },
            "formula": "q"
        });
        let candidates: Vec<SkillCandidate> = ["a", "b", "c", "d", "e"]
            .iter()
            .map(|name| authored(name, spec.clone()))
            .collect();

        let selection = select_skills(&route, serde_json::json!({}), &candidates).await;

        assert!(selection.warnings.is_empty(), "{:?}", selection.warnings);
        assert_eq!(selection.selected.len(), 5);
        assert!(selection
            .selected
            .iter()
            .all(|(_, score)| (*score - 0.8).abs() < 1e-12));
        let bodies = server.join().unwrap();
        assert_eq!(bodies.len(), 5);
        // The authored questions go verbatim, not the batch noul shape.
        let sent: Value = serde_json::from_str(&bodies[0]).unwrap();
        assert!(sent["questions"]["q"].is_object());
        assert!(sent["questions"].get("skill_0").is_none());

        // Below its own threshold: excluded, no warning.
        let body = noul_response(&[("q", 0.5)]);
        let (base, server) = spawn_mock(vec![leak(body)]);
        let route = hand_route(&base, 10_000);
        let selection = select_skills(&route, serde_json::json!({}), &[authored("f", spec)]).await;
        assert!(selection.selected.is_empty());
        assert!(selection.warnings.is_empty(), "{:?}", selection.warnings);
        server.join().unwrap();
    }

    #[tokio::test]
    async fn a_formula_error_drops_only_that_skill() {
        // The unformulated skill's max answer (0.8) clears the default
        // threshold; the formulated ones fail on an unknown variable. Every
        // request is served the same body: the authored pair keys on `q`,
        // while the unauthored batch candidate is candidate #2 (`skill_2`).
        let body = noul_response(&[("q", 0.8), ("skill_2", 0.8)]);
        let (base, server) = spawn_mock(vec![leak(body.clone()); 3]);
        let route = hand_route(&base, 10_000);
        let broken = serde_json::json!({
            "questions": { "q": { "type": "noul", "instructions": "?" } },
            "formula": "missing_var + 1"
        });
        let plain = serde_json::json!({
            "questions": { "q": { "type": "noul", "instructions": "?" } }
        });
        let candidates = vec![
            authored("broken", broken),
            authored("plain", plain),
            unauthored("batch"),
        ];

        let selection = select_skills(&route, serde_json::json!({}), &candidates).await;

        let names: Vec<&str> = selection
            .selected
            .iter()
            .map(|(name, _)| name.as_str())
            .collect();
        assert_eq!(names, vec!["plain", "batch"]);
        assert_eq!(selection.warnings.len(), 1);
        assert!(
            selection.warnings[0].contains("broken"),
            "{:?}",
            selection.warnings
        );
        assert!(
            selection.warnings[0].contains("unknown variable"),
            "{:?}",
            selection.warnings
        );
        server.join().unwrap();
    }

    #[tokio::test]
    async fn an_answerless_response_selects_nothing_without_a_warning() {
        let body = "{}";
        let (base, server) = spawn_mock(vec![leak(body.to_string()); 2]);
        let route = hand_route(&base, 10_000);
        let candidates = vec![
            unauthored("a"),
            authored(
                "b",
                serde_json::json!({ "questions": { "q": { "type": "noul", "instructions": "?" } } }),
            ),
        ];

        let selection = select_skills(&route, serde_json::json!({}), &candidates).await;

        assert!(selection.selected.is_empty());
        // A response with no answers is not a request failure: it just
        // selects nothing.
        assert!(selection.warnings.is_empty(), "{:?}", selection.warnings);
        server.join().unwrap();
    }

    #[tokio::test]
    async fn a_non_2xx_response_is_a_warning_not_a_panic() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0u8; 4096];
            let _ = std::io::Read::read(&mut stream, &mut buf);
            let response =
                "HTTP/1.1 422 Unprocessable\r\ncontent-length: 0\r\nconnection: close\r\n\r\n";
            std::io::Write::write_all(&mut stream, response.as_bytes()).unwrap();
        });
        let route = hand_route(&format!("http://127.0.0.1:{port}"), 10_000);
        let candidates = vec![unauthored("a")];

        let selection = select_skills(&route, serde_json::json!({}), &candidates).await;

        assert!(selection.selected.is_empty());
        assert_eq!(selection.warnings.len(), 1);
        assert!(
            selection.warnings[0].contains("422"),
            "{:?}",
            selection.warnings
        );
        server.join().unwrap();
    }

    #[test]
    fn a_route_never_prints_its_authorization_header() {
        let route = hand_route("http://127.0.0.1:1", 1000);
        let rendered = format!("{route:?}");
        assert!(!rendered.contains("Bearer test-key"), "{rendered}");
    }
}
