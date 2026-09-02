// drip reports the same version as the lci it is a port of: `drip --version`
// and `lci --version` must agree, so a version bump lands in package.json AND
// drip/Cargo.toml together.
use std::path::Path;

#[test]
fn cargo_version_matches_package_json() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().expect("repo root");
    let package: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(root.join("package.json")).expect("package.json")).unwrap();
    let lci_version = package["version"].as_str().expect("package.json version");

    assert_eq!(
        env!("CARGO_PKG_VERSION"),
        lci_version,
        "drip/Cargo.toml version must match package.json (bump both in the same PR)"
    );
}

// Cargo.lock records the crate's own version too; `cargo build` rewrites it
// after a Cargo.toml bump, and an uncommitted rewrite blocks `git pull` in the
// checkout that serves the global `drip` symlink. Commit the lock with the bump.
#[test]
fn cargo_lock_records_the_same_drip_version() {
    let lock = std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.lock")).expect("Cargo.lock");
    let drip_block = lock
        .split("[[package]]")
        .find(|block| block.contains("name = \"drip\""))
        .expect("drip package in Cargo.lock");
    let locked = drip_block
        .lines()
        .find_map(|line| line.trim().strip_prefix("version = ").map(|v| v.trim_matches('"').to_string()))
        .expect("drip version in Cargo.lock");

    assert_eq!(locked, env!("CARGO_PKG_VERSION"), "drip/Cargo.lock is stale — run cargo build and commit it with the version bump");
}
