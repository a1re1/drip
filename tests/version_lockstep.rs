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
