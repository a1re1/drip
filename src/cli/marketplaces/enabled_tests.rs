// Tests for the enable/disable key functions: is_marketplace_key_enabled
// precedence, set_marketplace_key_enabled
// round-trip through a temp home, and the empty-registry skill listing.
use super::*;
use crate::core::home::open_drip_home;
use std::path::Path;

#[test]
fn is_marketplace_key_enabled_precedence() {
    let file = MarketplacesFile {
        disabled: vec!["pkg/a".to_string()],
        enabled: vec!["pkg/b".to_string()],
        ..Default::default()
    };

    // Project override disabled beats the registry's enabled entry.
    let overrides = ProjectPluginOverrides {
        disabled: vec!["pkg/b".to_string()],
        enabled: Vec::new(),
    };
    assert!(!is_marketplace_key_enabled("pkg/b", "pkg/b", &file, &overrides));

    // Project override enabled beats the registry's disabled entry.
    let overrides = ProjectPluginOverrides {
        disabled: Vec::new(),
        enabled: vec!["pkg/a".to_string()],
    };
    assert!(is_marketplace_key_enabled("pkg/a", "pkg/a", &file, &overrides));

    // The item key wins over the plugin key when they disagree.
    let overrides = ProjectPluginOverrides {
        disabled: Vec::new(),
        enabled: Vec::new(),
    };
    assert!(is_marketplace_key_enabled("pkg/a", "pkg/b", &file, &overrides));
    assert!(!is_marketplace_key_enabled("pkg/b", "pkg/a", &file, &overrides));

    // The plugin key is used as a fallback when the item key is unknown.
    assert!(!is_marketplace_key_enabled("pkg/a", "pkg/a/unknown-item", &file, &overrides));
    assert!(is_marketplace_key_enabled("pkg/b", "pkg/b/unknown-item", &file, &overrides));

    // Neither key known anywhere -> false.
    assert!(!is_marketplace_key_enabled("pkg/zzz", "pkg/zzz/item", &file, &overrides));
}

#[test]
fn set_marketplace_key_enabled_round_trips_and_is_idempotent() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let home = open_drip_home(tmp.path().to_string_lossy().as_ref());
    let path = Path::new(&home.marketplaces_path);

    // Disable: the key lands in `disabled`, `enabled` stays empty.
    let file = set_marketplace_key_enabled(&home, "acme/kit", false).expect("disable");
    assert_eq!(file.disabled, vec!["acme/kit"]);
    assert!(file.enabled.is_empty());

    // Re-disabling is idempotent (no duplicate entry).
    let again = set_marketplace_key_enabled(&home, "acme/kit", false).expect("disable again");
    assert_eq!(again.disabled, vec!["acme/kit"]);

    // Enable: the key moves to `enabled`.
    let file = set_marketplace_key_enabled(&home, "acme/kit", true).expect("enable");
    assert_eq!(file.enabled, vec!["acme/kit"]);
    assert!(file.disabled.is_empty());

    // Re-enabling is idempotent too.
    let again = set_marketplace_key_enabled(&home, "acme/kit", true).expect("enable again");
    assert_eq!(again.enabled, vec!["acme/kit"]);

    // The registry file on disk reflects the final state.
    let file = load_marketplaces_file(path).expect("load");
    assert_eq!(file.enabled, vec!["acme/kit"]);
    assert!(file.disabled.is_empty());
    assert_eq!(file.version, 1);
}

#[test]
fn list_enabled_marketplace_skills_returns_empty_for_an_empty_registry() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let home = open_drip_home(tmp.path().to_string_lossy().as_ref());

    let skills = list_enabled_marketplace_skills(tmp.path(), &home).expect("list");

    assert!(skills.is_empty());
}
