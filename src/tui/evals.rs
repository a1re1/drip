//! The eval-case browser: the overlay `drip --tui` opens with `/evals`.
//!
//! It is deliberately render-only. [`EvalBrowser`] keeps the list cursor, the
//! live search filter, the detail frame and the operator's judgment, and every
//! screen is a pure function of that state plus the row it shows. Running a
//! case through the real classifier is the caller's job (see
//! `crate::cli::eval_runner`), so the browser itself never touches the network
//! and a unit test can drive the whole overlay.
//!
//! The judgment is the point of the overlay: the operator says which candidates
//! were applicable for the case's scenario, and that list is what
//! `crate::cli::evals::eval_agreement` scores the classifier's `matched` names
//! against, so a verdict written here is exactly the verdict the CLI records.

/// One case as the browser lists it: the identity the list frame shows, the
/// scenario context the detail frame prints, and the names the case declares.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct EvalRow {
    /// The case's name (the directory name unless `case.json` overrides it).
    pub name: String,
    /// `prompt`, `task` or `plan`.
    pub kind: String,
    /// `project` or `user` — the scope the verdict is written at.
    pub scope: String,
    /// The case directory, shown in the detail frame and written beside.
    pub dir: String,
    pub description: String,
    /// The scenario's goal text, the classifier's input.
    pub goal: String,
    /// The case's `expected` names: what should be composed.
    pub expected: Vec<String>,
    /// The candidates the case fixes, when it fixes them.
    pub candidates: Vec<String>,
}

/// What the last run of this case matched, as the detail frame reports it.
/// Built from the runner's outcome so the browser holds no runner types.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct EvalRunSummary {
    pub matched: Vec<String>,
    pub missed: Vec<String>,
    pub spurious: Vec<String>,
    pub warnings: Vec<String>,
}

/// The overlay's whole state.
#[derive(Debug, Default)]
pub struct EvalBrowser {
    pub rows: Vec<EvalRow>,
    pub selected: usize,
    /// Live search text, matched against each row's name and description.
    pub filter: String,
    /// The row whose detail frame is open, as an index into `rows`.
    pub detail: Option<usize>,
    /// True while a classifier run for `detail` is in flight.
    pub running: bool,
    /// Names the operator has judged applicable for the open case.
    pub applicable: Vec<String>,
    /// Names the operator has judged not applicable.
    pub not_applicable: Vec<String>,
    /// The last run's matches for the open case.
    pub summary: Option<EvalRunSummary>,
    /// The candidate cursor inside the detail frame.
    pub candidate_index: usize,
}

impl EvalBrowser {
    pub fn new(rows: Vec<EvalRow>) -> Self {
        Self {
            rows,
            ..Self::default()
        }
    }

    /// The rows surviving the filter, as indices into `rows`, in list order.
    /// An empty filter keeps everything; matching is case-insensitive over the
    /// name and the description, exactly like the skills picker's search.
    pub fn visible(&self) -> Vec<usize> {
        let needle = self.filter.trim().to_lowercase();
        self.rows
            .iter()
            .enumerate()
            .filter(|(_index, row)| {
                needle.is_empty()
                    || row.name.to_lowercase().contains(&needle)
                    || row.description.to_lowercase().contains(&needle)
            })
            .map(|(index, _row)| index)
            .collect()
    }

    /// Moves the highlight by `delta` within the filtered list, clamping at
    /// both ends so a held arrow key never wraps past a case.
    pub fn move_selection(&mut self, delta: isize) {
        let visible = self.visible();
        if visible.is_empty() {
            self.selected = 0;
            return;
        }
        let position = visible
            .iter()
            .position(|index| *index == self.selected)
            .unwrap_or(0) as isize;
        let next = (position + delta).clamp(0, visible.len() as isize - 1);
        self.selected = visible[next as usize];
    }

    /// Opens the detail frame of the highlighted case, clearing the judgment
    /// of the case that was open before it.
    pub fn open_detail(&mut self) {
        if self.rows.get(self.selected).is_none() {
            return;
        }
        self.detail = Some(self.selected);
        self.running = false;
        self.summary = None;
        self.applicable.clear();
        self.not_applicable.clear();
        self.candidate_index = 0;
    }

    /// Closes the detail frame, stepping back to the list.
    pub fn close_detail(&mut self) {
        self.detail = None;
        self.running = false;
        self.candidate_index = 0;
    }

    /// The case the detail frame shows, when one is open.
    pub fn detail_row(&self) -> Option<&EvalRow> {
        self.rows.get(self.detail?)
    }

    /// Records one candidate as applicable (or not), keeping the two lists
    /// exclusive so a name can never be judged both ways.
    pub fn judge(&mut self, name: &str, applicable: bool) {
        self.applicable.retain(|entry| entry != name);
        self.not_applicable.retain(|entry| entry != name);
        if applicable {
            self.applicable.push(name.to_string());
        } else {
            self.not_applicable.push(name.to_string());
        }
    }

    /// True when the highlighted case carries a judgment to persist.
    pub fn has_judgment(&self) -> bool {
        !self.applicable.is_empty() || !self.not_applicable.is_empty()
    }

    /// The candidates this case judges over: the pool its scenario fixes when
    /// it fixes one, else its `expected` names followed by whatever a run
    /// matched — so a case that declares no pool still has rows to judge, and
    /// a name that only the classifier found is judgeable too.
    pub fn candidates(&self, row: &EvalRow) -> Vec<String> {
        if !row.candidates.is_empty() {
            return row.candidates.clone();
        }

        let mut names = row.expected.clone();
        if let Some(summary) = self.summary.as_ref() {
            for name in &summary.matched {
                if !names.contains(name) {
                    names.push(name.clone());
                }
            }
        }
        names
    }

    /// The candidate under the detail frame's cursor, if there is one.
    pub fn selected_candidate(&self) -> Option<String> {
        let row = self.detail_row()?.clone();
        self.candidates(&row).get(self.candidate_index).cloned()
    }

    /// Moves the candidate cursor, clamping at both ends so a held arrow key
    /// never runs off the pool.
    pub fn move_candidate(&mut self, delta: isize) {
        let Some(row) = self.detail_row().cloned() else {
            return;
        };
        let count = self.candidates(&row).len();
        if count == 0 {
            self.candidate_index = 0;
            return;
        }
        self.candidate_index =
            (self.candidate_index as isize + delta).clamp(0, count as isize - 1) as usize;
    }

    /// Judges the candidate under the cursor; returns its name when there was
    /// one. `a` in the detail frame is this with `true`, `n` with `false`.
    pub fn judge_selected(&mut self, applicable: bool) -> Option<String> {
        let name = self.selected_candidate()?;
        self.judge(&name, applicable);
        Some(name)
    }
}

/// The list frame: a header with the filtered count and one row per visible
/// case — name, kind and scope on the line, description dimmed under it.
/// `width` is the terminal width; rows are clipped to it so a long description
/// can never wrap the frame.
pub fn render_evals_list(browser: &EvalBrowser, width: usize) -> Vec<String> {
    let visible = browser.visible();
    let mut rows = vec![format!(
        "eval cases ({}/{})",
        visible.len(),
        browser.rows.len()
    )];

    if visible.is_empty() {
        rows.push("  no case matches the filter".to_string());
    }

    for index in visible {
        let row = &browser.rows[index];
        rows.push(clip(
            &format!(
                "{} {:<24} {:<6} {}",
                if index == browser.selected {
                    "▸"
                } else {
                    " "
                },
                row.name,
                row.kind,
                row.scope
            ),
            width,
        ));
        if !row.description.is_empty() {
            rows.push(clip(&format!("    {}", row.description), width));
        }
    }

    rows.push(String::new());
    rows.push(clip(
        "enter open a case · type to filter · esc close",
        width,
    ));
    rows
}

/// The detail frame: the case's scenario, its expected names, what the last run
/// matched and the judgment as it stands. The classifier's misses and its
/// spurious matches are named explicitly — they are the two counts the
/// agreement score is built from, so the overlay never hides a disagreement.
pub fn render_eval_detail(
    row: &EvalRow,
    summary: Option<&EvalRunSummary>,
    applicable: &[String],
    not_applicable: &[String],
    running: bool,
    width: usize,
) -> Vec<String> {
    let mut rows = vec![
        format!("{} · {} · {}", row.name, row.kind, row.scope),
        clip(&format!("dir: {}", row.dir), width),
    ];

    if !row.description.is_empty() {
        rows.push(clip(&row.description, width));
    }
    if !row.goal.is_empty() {
        rows.push(String::new());
        rows.push("scenario goal".to_string());
        rows.push(clip(&format!("  {}", row.goal), width));
    }

    rows.push(String::new());
    rows.push(format!(
        "expected: {}",
        if row.expected.is_empty() {
            "(none declared)".to_string()
        } else {
            row.expected.join(", ")
        }
    ));

    match summary {
        Some(summary) if !summary.matched.is_empty() => {
            rows.push(format!("matched: {}", summary.matched.join(", ")));
        }
        Some(_) => rows.push("matched: (nothing matched)".to_string()),
        None if running => rows.push("matched: running…".to_string()),
        None => rows.push("matched: (not run yet)".to_string()),
    }

    if let Some(summary) = summary {
        if !summary.missed.is_empty() {
            rows.push(clip(
                &format!("missed: {}", summary.missed.join(", ")),
                width,
            ));
        }
        if !summary.spurious.is_empty() {
            rows.push(clip(
                &format!("spurious: {}", summary.spurious.join(", ")),
                width,
            ));
        }
        for warning in &summary.warnings {
            rows.push(clip(&format!("warning: {warning}"), width));
        }
    }

    rows.push(String::new());
    rows.push(format!(
        "judgment — applicable: {} · not applicable: {}",
        if applicable.is_empty() {
            "(none)".to_string()
        } else {
            applicable.join(", ")
        },
        if not_applicable.is_empty() {
            "(none)".to_string()
        } else {
            not_applicable.join(", ")
        }
    ));

    rows.push(String::new());
    if running {
        rows.push("running the classifier…".to_string());
    } else {
        rows.push(clip(
            "enter run through the classifier · a/n judge the expected names · esc back",
            width,
        ));
    }
    rows
}

/// Clips one rendered row to `width` characters, counting characters rather
/// than bytes so a description with non-ASCII text is never cut mid-character.
fn clip(text: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    if text.chars().count() <= width {
        return text.to_string();
    }
    text.chars().take(width).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(name: &str, kind: &str, scope: &str, description: &str) -> EvalRow {
        EvalRow {
            name: name.to_string(),
            kind: kind.to_string(),
            scope: scope.to_string(),
            dir: format!("/home/evals/{name}"),
            description: description.to_string(),
            goal: format!("fix the {name} flake"),
            expected: vec!["tdd".to_string()],
            candidates: vec!["tdd".to_string(), "review-independently".to_string()],
        }
    }

    fn browser() -> EvalBrowser {
        EvalBrowser::new(vec![
            row("flaky-test-fixup", "task", "user", "a failing test to fix"),
            row("ship-this-branch", "plan", "project", "open a pull request"),
        ])
    }

    #[test]
    fn the_filter_keeps_every_row_until_it_is_typed_into() {
        let mut browser = browser();
        assert_eq!(browser.visible(), vec![0, 1]);
        browser.filter = "SHIP".to_string();
        assert_eq!(browser.visible(), vec![1], "filtering is case-insensitive");
    }

    #[test]
    fn the_filter_searches_the_description_as_well_as_the_name() {
        let mut browser = browser();
        browser.filter = "pull request".to_string();
        assert_eq!(browser.visible(), vec![1]);
        browser.filter = "nothing here".to_string();
        assert!(browser.visible().is_empty());
    }

    #[test]
    fn the_selection_clamps_within_the_filtered_list() {
        let mut browser = browser();
        browser.move_selection(1);
        assert_eq!(browser.selected, 1);
        browser.move_selection(1);
        assert_eq!(browser.selected, 1, "the highlight stops at the last row");
        browser.move_selection(-5);
        assert_eq!(browser.selected, 0);
    }

    #[test]
    fn a_judgment_is_exclusive_and_survives_a_second_verdict() {
        let mut browser = browser();
        browser.judge("tdd", true);
        assert_eq!(browser.applicable, vec!["tdd".to_string()]);
        browser.judge("tdd", false);
        assert!(browser.applicable.is_empty());
        assert_eq!(browser.not_applicable, vec!["tdd".to_string()]);
        assert!(browser.has_judgment());
    }

    #[test]
    fn opening_a_detail_frame_clears_the_judgment_of_the_last_case() {
        let mut browser = browser();
        browser.judge("tdd", true);
        browser.selected = 1;
        browser.open_detail();
        assert_eq!(browser.detail, Some(1));
        assert!(browser.applicable.is_empty());
        assert!(!browser.has_judgment());
        browser.close_detail();
        assert_eq!(browser.detail, None);
    }

    #[test]
    fn the_empty_browser_opens_no_detail_frame() {
        let mut browser = EvalBrowser::default();
        browser.open_detail();
        assert_eq!(browser.detail, None);
        assert!(browser.detail_row().is_none());
    }

    #[test]
    fn the_list_frame_marks_the_selection_and_clips_every_row() {
        let mut browser = browser();
        browser.selected = 1;
        let rows = render_evals_list(&browser, 30);
        assert!(rows[0].contains("eval cases (2/2)"), "{}", rows[0]);
        assert!(
            rows.iter().any(|row| row.contains("▸ ship-this-branch")),
            "{rows:?}"
        );
        assert!(rows.iter().all(|row| row.chars().count() <= 30), "{rows:?}");
        assert!(rows.last().unwrap().contains("enter open a case"));
    }

    #[test]
    fn an_empty_filter_result_says_so_instead_of_showing_nothing() {
        let mut browser = browser();
        browser.filter = "zzz".to_string();
        let rows = render_evals_list(&browser, 40);
        assert!(rows.iter().any(|row| row.contains("no case matches")));
        assert!(!rows.iter().any(|row| row.contains("flaky-test-fixup")));
    }

    #[test]
    fn the_detail_frame_reports_misses_and_spurious_matches_by_name() {
        let row = row("flaky-test-fixup", "task", "user", "a failing test to fix");
        let summary = EvalRunSummary {
            matched: vec!["tdd".to_string(), "hooks-setup".to_string()],
            missed: vec!["verify-before-done".to_string()],
            spurious: vec!["hooks-setup".to_string()],
            warnings: vec!["one candidate is not installed".to_string()],
        };
        let rows = render_eval_detail(
            &row,
            Some(&summary),
            &["tdd".to_string()],
            &["hooks-setup".to_string()],
            false,
            200,
        );
        let joined = rows.join("\n");
        assert!(joined.contains("expected: tdd"), "{joined}");
        assert!(joined.contains("matched: tdd, hooks-setup"), "{joined}");
        assert!(joined.contains("missed: verify-before-done"), "{joined}");
        assert!(joined.contains("spurious: hooks-setup"), "{joined}");
        assert!(joined.contains("warning: one candidate is not installed"));
        assert!(joined.contains("applicable: tdd"), "{joined}");
        assert!(joined.contains("not applicable: hooks-setup"), "{joined}");
    }

    #[test]
    fn the_candidate_cursor_walks_the_pool_and_judges_what_it_sits_on() {
        let mut browser = browser();
        browser.open_detail();
        assert_eq!(browser.selected_candidate(), Some("tdd".to_string()));
        browser.move_candidate(-1);
        assert_eq!(browser.selected_candidate(), Some("tdd".to_string()));
        browser.move_candidate(5);
        assert_eq!(
            browser.selected_candidate(),
            Some("review-independently".to_string()),
            "the cursor stops at the last candidate"
        );
        assert_eq!(
            browser.judge_selected(true),
            Some("review-independently".to_string())
        );
        assert_eq!(browser.applicable, vec!["review-independently".to_string()]);
        browser.judge_selected(false);
        assert!(browser.applicable.is_empty());
        assert_eq!(
            browser.not_applicable,
            vec!["review-independently".to_string()]
        );
    }

    #[test]
    fn a_case_with_nothing_to_judge_has_no_candidate() {
        let mut browser = browser();
        browser.rows[0].candidates.clear();
        browser.rows[0].expected.clear();
        browser.open_detail();
        assert_eq!(browser.selected_candidate(), None);
        assert_eq!(browser.judge_selected(true), None);
        browser.move_candidate(1);
        assert_eq!(browser.candidate_index, 0);
    }

    #[test]
    fn the_detail_frame_distinguishes_unrun_from_nothing_matched() {
        let row = row("flaky-test-fixup", "task", "user", "a failing test to fix");
        let unrun = render_eval_detail(&row, None, &[], &[], false, 200).join("\n");
        assert!(unrun.contains("matched: (not run yet)"), "{unrun}");
        let empty = EvalRunSummary::default();
        let ran = render_eval_detail(&row, Some(&empty), &[], &[], false, 200).join("\n");
        assert!(ran.contains("matched: (nothing matched)"), "{ran}");
        let running = render_eval_detail(&row, None, &[], &[], true, 200).join("\n");
        assert!(running.contains("running the classifier…"), "{running}");
    }

    #[test]
    fn a_case_with_no_declared_pool_judges_expected_then_matched_names() {
        let mut browser = browser();
        assert_eq!(
            browser.candidates(&browser.rows[0]),
            vec!["tdd".to_string(), "review-independently".to_string()]
        );

        // No declared pool: the expected names are judged first, and a name
        // only the classifier matched is appended — never duplicated.
        let mut bare = row("bare", "prompt", "user", "no declared pool");
        bare.candidates.clear();
        browser.rows[0] = bare.clone();
        assert_eq!(browser.candidates(&bare), vec!["tdd".to_string()]);
        browser.summary = Some(EvalRunSummary {
            matched: vec!["tdd".to_string(), "hooks-setup".to_string()],
            ..Default::default()
        });
        assert_eq!(
            browser.candidates(&bare),
            vec!["tdd".to_string(), "hooks-setup".to_string()]
        );
    }
}
