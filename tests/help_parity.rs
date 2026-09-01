// Integration test: `drip --help` must byte-match `bun run src/cli/main.tsx --help`
// with only the brand rename applied (lciw→dripw, LCI_→DRIP_, then bare lci→drip,
// which also rewrites ~/.lci→~/.drip and .lci/→.drip/).
//
// Skips gracefully when bun is unavailable or the TypeScript reference tree is
// not present next to the crate.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Rename pairs in application order — longest/most specific first, mirroring
/// the sed pipeline 's/lciw/dripw/g; s/LCI_/DRIP_/g; s/lci/drip/g'.
const RENAMES: [(&str, &str); 3] = [("lciw", "dripw"), ("LCI_", "DRIP_"), ("lci", "drip")];

fn apply_rename(input: &str) -> String {
    let mut out = input.to_string();

    for (from, to) in RENAMES {
        out = out.replace(from, to);
    }

    out
}

/// The crate lives at <repo>/drip, so the TypeScript sources are one level up.
fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("manifest dir has a parent")
        .to_path_buf()
}

fn bun_available() -> bool {
    Command::new("bun")
        .arg("--version")
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false)
}

#[test]
fn drip_help_matches_renamed_lci_help() {
    let root = repo_root();
    let entry = root.join("src/cli/main.tsx");

    if !entry.is_file() {
        eprintln!("skipping: reference entrypoint {} not found", entry.display());
        return;
    }

    if !bun_available() {
        eprintln!("skipping: bun is not installed");
        return;
    }

    let lci_help = Command::new("bun")
        .arg("run")
        .arg("src/cli/main.tsx")
        .arg("--help")
        .current_dir(&root)
        .output()
        .expect("failed to run bun");
    assert!(
        lci_help.status.success(),
        "bun run src/cli/main.tsx --help exited {:?}",
        lci_help.status.code()
    );

    let drip_help = Command::new(env!("CARGO_BIN_EXE_drip"))
        .arg("--help")
        .output()
        .expect("failed to run the drip binary");
    assert!(
        drip_help.status.success(),
        "drip --help exited {:?}",
        drip_help.status.code()
    );

    let expected = apply_rename(&String::from_utf8(lci_help.stdout).expect("utf8 help"));
    let actual = String::from_utf8(drip_help.stdout).expect("utf8 help");

    // The migration OPTIONS block is drip-only: `--migrate-from-lci` has no
    // lci counterpart, so the renamed TS output has no matching lines. Strip
    // it from the drip output before comparing so the rest stays byte-equal.
    let actual_stripped = drip::cli::help::strip_migration_block(&actual);

    assert_eq!(
        actual_stripped, expected,
        "drip --help (migration block stripped) must byte-match the renamed lci --help output"
    );
}
