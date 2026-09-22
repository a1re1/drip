// Sample of what the `ask_user` harness tool does: how a survey call parses,
// becomes a pending question on state, renders as the stepped overlay, and
// accepts an answers.jsonl batch. Everything here drives the shipped
// implementation (no re-derived shapes), so this file is a real, executable
// example of the tool.
use drip::core::state::answers::{append_answers, parse_answers_value};
use drip::core::state::create_harness_state;
use drip::core::types::{HarnessSurveyQuestion, QuestionSurvey};
use drip::harness::harness_tools::{
    apply_harness_op, parse_harness_op, HarnessOp, HarnessOpContext,
};
use drip::tui::widgets::{render_survey, PickerItem};

/// The foo/bar sample survey, exactly as a model would send it: three
/// questions, one with `allow_other: false` to show the escape hatches.
const FOO_BAR_SURVEY: &str = r#"{
  "questions": [
    {"header":"Foo","question":"Which foo should this run use?","options":[
      {"label":"Foo Alpha","description":"the original, well-trodden foo"},
      {"label":"Foo Beta","description":"rewritten foo, fewer surprises"}]},
    {"header":"Bar","question":"How should bar be served?","allow_other":false,"options":[
      {"label":"Bar draft","description":"serve bar as a draft"},
      {"label":"Bar chilled","description":"chill bar before serving"}]},
    {"header":"Foo Bar","question":"How should foo and bar be combined?","options":[
      {"label":"Pair them","description":"foo and bar ship together"},
      {"label":"Keep separate","description":"foo and bar stay independent"},
      {"label":"Decide later","description":"leave the combination open"}]}
  ]
}"#;

/// Strip ANSI SGR sequences so assertions read the visible text only.
fn plain(rows: &[String]) -> String {
    let joined = rows.join("\n");
    let mut out = String::new();
    let mut chars = joined.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\u{1b}' {
            for next in chars.by_ref() {
                if next.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            out.push(ch);
        }
    }
    out
}

fn sample_questions() -> Vec<HarnessSurveyQuestion> {
    match parse_harness_op("ask_user", FOO_BAR_SURVEY).expect("sample survey parses") {
        HarnessOp::AskUser { survey } => survey.questions,
        other => panic!("expected an AskUser op, got {other:?}"),
    }
}

/// Step 1: the call parses into a 3-question survey; an omitted `allow_other`
/// defaults to true, an explicit `false` is honoured.
#[test]
fn foo_bar_survey_parses_into_three_questions() {
    let questions = sample_questions();
    assert_eq!(questions.len(), 3);
    let headers: Vec<&str> = questions.iter().map(|q| q.header.as_str()).collect();
    assert_eq!(headers, vec!["Foo", "Bar", "Foo Bar"]);
    assert_eq!(questions[0].options.len(), 2);
    assert_eq!(questions[0].options[0].label, "Foo Alpha");
    assert_eq!(
        questions[0].options[1].description,
        "rewritten foo, fewer surprises"
    );
    assert!(
        questions[0].allow_other,
        "omitted allow_other defaults to true"
    );
    assert!(
        !questions[1].allow_other,
        "explicit allow_other=false honoured"
    );
    assert_eq!(questions[2].options.len(), 3);
    assert!(questions[2].allow_other);
}

/// Step 2: with `--ask`, the accepted call persists the survey as the pending
/// question set and the outcome reports the blocked/pending condition.
#[test]
fn foo_bar_survey_becomes_a_pending_question_on_state() {
    let op = parse_harness_op("ask_user", FOO_BAR_SURVEY).unwrap();
    let mut state = create_harness_state("sample the ask_user tool");
    let context = HarnessOpContext {
        ask_user_enabled: true,
        ask_window_open: true,
        ..HarnessOpContext::default()
    };
    let outcome = apply_harness_op(&mut state, op, &context);
    assert!(outcome.state_changed);
    let pending = state
        .pending_questions
        .expect("survey persisted as pending");
    assert_eq!(pending.questions.len(), 3);
    assert_eq!(pending.questions[2].header, "Foo Bar");
}

/// Step 3: what the operator sees. The stepped overlay shows the header chip
/// with progress, the question, numbered options with descriptions, and the
/// `Type something.` / `Chat about this` escape-hatch rows.
#[test]
fn foo_bar_survey_renders_the_stepped_survey_overlay() {
    let questions = sample_questions();
    let items: Vec<PickerItem> = questions[0]
        .options
        .iter()
        .map(|option| PickerItem {
            detail: Some(option.description.clone()),
            id: option.label.clone(),
            label: option.label.clone(),
        })
        .collect();

    let rows = render_survey(
        &questions[0].header,
        &questions[0].question,
        &items,
        &[],
        questions[0].allow_other,
        0,
        1,
        questions.len(),
        78,
    );
    let text = plain(&rows);
    assert!(text.contains("Foo"), "{text}");
    assert!(text.contains("question 1 of 3"), "{text}");
    assert!(text.contains("Which foo should this run use?"), "{text}");
    assert!(text.contains("1. Foo Alpha"), "{text}");
    assert!(text.contains("2. Foo Beta"), "{text}");
    assert!(text.contains("the original, well-trodden foo"), "{text}");
    assert!(text.contains("Type something."), "{text}");
    assert!(text.contains("Chat about this"), "{text}");

    // allow_other=false drops `Type something.` but keeps `Chat about this`.
    let rows = render_survey(
        &questions[1].header,
        &questions[1].question,
        &items,
        &[],
        questions[1].allow_other,
        0,
        2,
        questions.len(),
        78,
    );
    let text = plain(&rows);
    assert!(!text.contains("Type something."), "{text}");
    assert!(text.contains("Chat about this"), "{text}");
}

/// Step 4: the operator's reply round-trips — the sample batch validates
/// against the survey (one answer via free text) and lands as one JSONL line;
/// a partial batch is refused so a run never consumes a stale line.
#[test]
fn foo_bar_answers_validate_and_append_as_jsonl() {
    let survey = QuestionSurvey {
        questions: sample_questions(),
        answers_cursor: None,
    };
    let raw = serde_json::json!({"answers": [
        {"index": 0, "choice": "Foo Alpha"},
        {"index": 1, "choice": "Bar chilled"},
        {"index": 2, "other": "pair them but keep the bar interface"}
    ]});
    let record = parse_answers_value(&raw).expect("answers payload parses");
    assert!(drip::harness::harness_tools::validate_survey_answers(&survey, &record).is_ok());

    let partial =
        parse_answers_value(&serde_json::json!({"answers": [{"index": 0, "choice": "Foo Alpha"}]}))
            .unwrap();
    assert!(drip::harness::harness_tools::validate_survey_answers(&survey, &partial).is_err());

    let path = std::env::temp_dir().join(format!(
        "drip-ask-user-sample-{}-{}.jsonl",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    append_answers(&path, &record).expect("answers append");
    let body = std::fs::read_to_string(&path).expect("answers.jsonl readable");
    let _ = std::fs::remove_file(&path);
    let lines: Vec<&str> = body
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();
    assert_eq!(lines.len(), 1, "one record per answered batch: {body}");
    let replayed =
        parse_answers_value(&serde_json::from_str::<serde_json::Value>(lines[0]).unwrap())
            .expect("written record parses back");
    assert_eq!(replayed.answers.len(), 3);
    assert_eq!(replayed.answers[1].choice.as_deref(), Some("Bar chilled"));
    assert_eq!(
        replayed.answers[2].other.as_deref(),
        Some("pair them but keep the bar interface")
    );
}
