//! End-to-end checks for the copy-on-first-start default skills: the shipped
//! templates must land in `<home>/skills` on the first start of the real
//! `drip` binary, a deletion must stick across later starts, and
//! `--install-skills` must be the way back.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn temp_root(prefix: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("{}{}", prefix, uuid::Uuid::new_v4()));
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn run_drip(home: &Path, args: &[&str]) -> (bool, String) {
    let project = home.join("project");
    let output = Command::new(env!("CARGO_BIN_EXE_drip"))
        .args(args)
        .arg("--home")
        .arg(home)
        .arg("--project-dir")
        .arg(&project)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .expect("run the drip binary");
    (
        output.status.success(),
        String::from_utf8_lossy(&output.stdout).into_owned(),
    )
}

fn seeded_skill_dirs(home: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(home.join("skills"))
        .map(|entries| {
            entries
                .filter_map(|entry| entry.ok())
                .filter(|entry| entry.path().join("SKILL.md").is_file())
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

#[test]
fn first_start_seeds_the_default_skills_into_the_home() {
    let home = temp_root("drip-skills-seed-");
    let (ok, stdout) = run_drip(&home, &["--skills"]);
    assert!(ok, "drip --skills must succeed: {stdout}");

    let seeded = seeded_skill_dirs(&home);
    assert!(
        seeded.len() >= 11,
        "first start must copy the shipped pack into <home>/skills, got {seeded:?}"
    );
    for expected in [
        "tdd",
        "verify-before-done",
        "praeparare",
        "commit-discipline",
    ] {
        assert!(
            seeded.iter().any(|name| name == expected),
            "{expected} must be seeded, got {seeded:?}"
        );
        assert!(
            stdout.contains(expected),
            "{expected} must be discovered as a user skill:\n{stdout}"
        );
    }
    // They are ordinary user skills, not an embedded pack.
    assert!(
        stdout.contains("[user]"),
        "seeded skills must read as user-owned:\n{stdout}"
    );

    fs::remove_dir_all(&home).ok();
}

#[test]
fn a_deleted_default_skill_stays_gone_and_install_skills_brings_it_back() {
    let home = temp_root("drip-skills-reinstall-");
    let (ok, _) = run_drip(&home, &["--skills"]);
    assert!(ok, "first start must succeed");

    fs::remove_dir_all(home.join("skills/tdd")).unwrap();
    fs::remove_dir_all(home.join("skills/praeparare")).unwrap();

    // A later ordinary start must not resurrect what the operator deleted.
    let (ok, stdout) = run_drip(&home, &["--skills"]);
    assert!(ok, "second start must succeed");
    for gone in ["tdd", "praeparare"] {
        assert!(
            !stdout.contains(gone),
            "deleted default skill {gone} must stay gone:\n{stdout}"
        );
        assert!(!home.join("skills").join(gone).exists());
    }

    // --install-skills is the way back: every deleted default returns.
    let (ok, stdout) = run_drip(&home, &["--install-skills"]);
    assert!(ok, "--install-skills must succeed: {stdout}");
    assert!(
        stdout.contains("tdd") && stdout.contains("praeparare"),
        "--install-skills must report the restored names:\n{stdout}"
    );
    assert!(home.join("skills/tdd/SKILL.md").is_file());
    assert!(home.join("skills/praeparare/SKILL.md").is_file());

    let (ok, stdout) = run_drip(&home, &["--skills"]);
    assert!(ok, "third start must succeed");
    assert!(
        stdout.contains("tdd"),
        "restored tdd must be discovered:\n{stdout}"
    );

    fs::remove_dir_all(&home).ok();
}

#[test]
fn a_default_name_that_could_not_be_seeded_is_retried_on_a_later_start() {
    let home = temp_root("drip-skills-retry-");
    fs::create_dir_all(home.join("skills")).unwrap();
    // First start: `skills/tdd` is a plain file, so the template cannot land.
    fs::write(home.join("skills/tdd"), "not a skill dir").unwrap();

    let (ok, _) = run_drip(&home, &["--skills"]);
    assert!(ok, "first start must succeed");
    assert!(!home.join("skills/tdd/SKILL.md").exists());

    // The name must not be burned: clearing the obstruction seeds it next start.
    fs::remove_file(home.join("skills/tdd")).unwrap();
    let (ok, stdout) = run_drip(&home, &["--skills"]);
    assert!(ok, "second start must succeed");
    assert!(
        home.join("skills/tdd/SKILL.md").is_file(),
        "an unwritable default name must be retried, not recorded as installed"
    );
    assert!(
        stdout.contains("tdd"),
        "retried tdd must be discovered:\n{stdout}"
    );

    fs::remove_dir_all(&home).ok();
}
