//! The run report: a markdown file the harness keeps for the length of a
//! session, plus the clickable link the operator can follow to read it.
//!
//! The agent adds a short write-up per task or cycle through the `report`
//! harness tool; the end-of-run summary step synthesizes those entries into the
//! executive summary printed at the end of the run. The file is the durable
//! detail behind that summary, so the summary never has to be the only record
//! of what happened.

use std::path::{Path, PathBuf};

use crate::core::types::HarnessTaskReport;

/// File name of the running report inside the session directory.
pub const RUN_REPORT_FILE_NAME: &str = "report.md";

/// Append one write-up to the running report, creating the file (and its
/// parent directory) when needed. The header is written only when the file is
/// missing, so a resumed run keeps appending to the same report.
pub fn append_report_entry(
    path: &Path,
    goal: &str,
    entry: &HarnessTaskReport,
) -> std::io::Result<()> {
    use std::io::Write;

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let mut text = String::new();
    if !path.exists() {
        text.push_str(&format!("# Run report\n\nGoal: {goal}\n"));
    }
    text.push_str(&format!(
        "\n## {}\n\n- at: iteration {}, loop {}\n- task: {}\n\n{}\n",
        entry.headline.trim(),
        entry.at_iteration,
        entry.at_loop,
        entry.task_id.as_deref().unwrap_or("run"),
        entry.body.trim()
    ));

    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    file.write_all(text.as_bytes())
}

/// Whether this terminal is one whose hyperlinks we know how to render. The
/// editor/terminal integration advertises itself through the environment; an
/// unknown terminal gets the plain path instead of escape bytes it would show
/// as garbage. `DRIP_NO_HYPERLINKS=1` forces the plain form.
pub fn hyperlinks_supported() -> bool {
    if std::env::var("DRIP_NO_HYPERLINKS").is_ok_and(|value| !value.is_empty()) {
        return false;
    }
    let term_program = std::env::var("TERM_PROGRAM")
        .unwrap_or_default()
        .to_ascii_lowercase();
    let term = std::env::var("TERM")
        .unwrap_or_default()
        .to_ascii_lowercase();
    matches!(
        term_program.as_str(),
        "vscode" | "iterm.app" | "wezterm" | "ghostty"
    ) || term.contains("kitty")
        || std::env::var_os("VTE_VERSION").is_some()
}

/// An OSC 8 hyperlink to a local file (`file://` URL, absolute path) carrying
/// `label` as the visible text.
pub fn osc8_link(path: &Path, label: &str) -> String {
    let absolute = std::fs::canonicalize(path).unwrap_or_else(|_| absolute_path(path));
    format!(
        "\x1b]8;;file://{}\x1b\\{}\x1b]8;;\x1b\\",
        absolute.display(),
        label
    )
}

fn absolute_path(path: &Path) -> PathBuf {
    if path.is_absolute() {
        return path.to_path_buf();
    }
    match std::env::current_dir() {
        Ok(cwd) => cwd.join(path),
        Err(_) => path.to_path_buf(),
    }
}

/// The line printed after the run summary: the report as a clickable link when
/// the terminal supports hyperlinks, the plain path otherwise.
pub fn report_link_line(path: &Path) -> String {
    if hyperlinks_supported() {
        format!("Full run report: {}", osc8_link(path, "report.md"))
    } else {
        format!("Full run report: {}", path.display())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(headline: &str, body: &str, iteration: i64, r#loop: i64) -> HarnessTaskReport {
        HarnessTaskReport {
            at_iteration: iteration,
            at_loop: r#loop,
            task_id: Some("task-1".to_string()),
            headline: headline.to_string(),
            body: body.to_string(),
        }
    }

    fn scratch_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "drip-report-{}-{}-{}",
            std::process::id(),
            name,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn the_first_entry_writes_the_header_and_every_later_entry_appends() {
        let dir = scratch_dir("append");
        let path = dir.join(RUN_REPORT_FILE_NAME);

        append_report_entry(
            &path,
            "ship the report",
            &entry("first", "did a thing", 2, 1),
        )
        .unwrap();
        append_report_entry(
            &path,
            "ship the report",
            &entry("second", "did another", 5, 2),
        )
        .unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text.matches("# Run report").count(), 1, "{text}");
        assert_eq!(text.matches("Goal: ship the report").count(), 1, "{text}");
        assert!(text.contains("## first"), "{text}");
        assert!(text.contains("## second"), "{text}");
        assert!(text.contains("- at: iteration 2, loop 1"), "{text}");
        assert!(text.contains("- task: task-1"), "{text}");
        assert!(text.contains("did a thing"), "{text}");
        assert!(text.contains("did another"), "{text}");
        // The second entry must come after the first.
        let first_at = text.find("## first").unwrap();
        let second_at = text.find("## second").unwrap();
        assert!(first_at < second_at, "{text}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_run_level_entry_omits_the_task_id() {
        let dir = scratch_dir("run-level");
        let path = dir.join(RUN_REPORT_FILE_NAME);
        let mut run_entry = entry("cycle", "no task active", 7, 3);
        run_entry.task_id = None;

        append_report_entry(&path, "goal", &run_entry).unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("- task: run"), "{text}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_link_line_names_the_report_path() {
        let path = Path::new("/tmp/some-session/report.md");
        let line = report_link_line(path);
        assert!(line.starts_with("Full run report: "), "{line}");
        assert!(line.contains("report.md"), "{line}");
    }

    #[test]
    fn an_osc8_link_wraps_the_label_in_the_hyperlink_escape() {
        let link = osc8_link(Path::new("/tmp/x/report.md"), "report.md");
        assert!(link.starts_with("\x1b]8;;file://"), "{link:?}");
        assert!(link.contains("report.md\x1b]8;;\x1b\\"), "{link:?}");
        assert!(link.ends_with("\x1b]8;;\x1b\\"), "{link:?}");
    }
}
