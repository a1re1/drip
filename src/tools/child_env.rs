// DRIP_SCRUB_ENV is set by the CLI to the comma-separated names of credentials
// the harness manages (the keys defined in ~/.drip/env.vars). Tool subprocesses
// are model-driven, so those credentials are stripped from their environment:
// a command the model runs cannot read or echo the harness's own API keys, and
// environment-sensitive tests behave the same inside a harness run as in a
// clean shell. A command that legitimately needs a credential should receive
// it via an explicit env override rather than inheriting it.

use std::collections::BTreeMap;

/// The child environment is a plain name→value map layered over the parent
/// process environment plus explicit overrides. BTreeMap keeps the iteration
/// order deterministic for tests.
pub type ChildProcessEnv = BTreeMap<String, String>;

fn scrub_names_from_env() -> Vec<String> {
    std::env::var("DRIP_SCRUB_ENV")
        .unwrap_or_default()
        .split(',')
        .map(|name| name.trim().to_string())
        .filter(|name| !name.is_empty())
        .collect()
}

pub fn build_child_process_env(overrides: Option<&BTreeMap<String, String>>) -> ChildProcessEnv {
    let mut child_env: ChildProcessEnv = std::env::vars().collect();
    if let Some(overrides) = overrides {
        for (key, value) in overrides {
            child_env.insert(key.clone(), value.clone());
        }
    }

    for name in scrub_names_from_env() {
        let overridden = overrides.map(|o| o.contains_key(&name)).unwrap_or(false);
        if !overridden {
            child_env.remove(&name);
        }
    }

    child_env.remove("DRIP_SCRUB_ENV");

    child_env
}

/// The scrub as `env -u NAME…` arguments, for children that inherit an
/// environment we cannot rebuild — tmux panes run on a shared server whose
/// env predates the harness, so the pane command itself must unset the
/// credentials (TODOS P1: the PR #19 key-echo incident's remaining gap).
pub fn build_env_unset_arguments() -> Vec<String> {
    let mut names = scrub_names_from_env();
    names.push("DRIP_SCRUB_ENV".to_string());

    let mut arguments = Vec::with_capacity(names.len() * 2);
    for name in names {
        arguments.push("-u".to_string());
        arguments.push(name);
    }
    arguments
}

#[cfg(test)]
mod tests {
    use super::*;

    // Serializes the env-mutating tests; std::env is process-global and the
    // test harness runs tests on parallel threads, so concurrent tests would
    // otherwise see each other's env edits.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    const TOUCHED_KEYS: [&str; 4] = [
        "DRIP_SCRUB_ENV",
        "DRIP_TEST_KEEP",
        "DRIP_TEST_SECRET",
        "DRIP_TEST_SECRET_TWO",
    ];

    struct EnvGuard;
    impl EnvGuard {
        fn new() -> Self {
            for key in TOUCHED_KEYS {
                std::env::remove_var(key);
            }
            EnvGuard
        }
    }
    impl Drop for EnvGuard {
        fn drop(&mut self) {
            for key in TOUCHED_KEYS {
                std::env::remove_var(key);
            }
        }
    }

    // All env access must happen while ENV_LOCK is held. The lock is taken
    // BEFORE the guard runs: the guard wipes every TOUCHED_KEY, and if it
    // executed while another test held the lock it would delete that test's
    // fixtures mid-run (observed as an intermittent
    // strips_the_credential_names… failure when the suite runs in parallel).
    // Unlock order is reverse-declaration, so the guard (declared second)
    // restores the keys before the lock (declared first) is released.
    macro_rules! lock_env {
        ($name:ident) => {
            let $name = ENV_LOCK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let $name = EnvGuard::new();
        };
    }

    #[test]
    fn strips_the_credential_names_listed_in_drip_scrub_env_and_the_marker_itself() {
        lock_env!(_guard);

        std::env::set_var("DRIP_TEST_SECRET", "sk-secret");
        std::env::set_var("DRIP_TEST_SECRET_TWO", "sk-two");
        std::env::set_var("DRIP_TEST_KEEP", "visible");
        std::env::set_var("DRIP_SCRUB_ENV", "DRIP_TEST_SECRET, DRIP_TEST_SECRET_TWO");

        let child_env = build_child_process_env(None);

        assert!(!child_env.contains_key("DRIP_TEST_SECRET"));
        assert!(!child_env.contains_key("DRIP_TEST_SECRET_TWO"));
        assert!(!child_env.contains_key("DRIP_SCRUB_ENV"));
        assert_eq!(
            child_env.get("DRIP_TEST_KEEP").map(String::as_str),
            Some("visible")
        );
    }

    #[test]
    fn keeps_explicit_overrides_even_when_their_name_is_scrubbed() {
        lock_env!(_guard);

        std::env::set_var("DRIP_TEST_SECRET", "sk-secret");
        std::env::set_var("DRIP_SCRUB_ENV", "DRIP_TEST_SECRET");

        let mut overrides = BTreeMap::new();
        overrides.insert(
            "DRIP_TEST_SECRET".to_string(),
            "explicit-value".to_string(),
        );

        let child_env = build_child_process_env(Some(&overrides));

        assert_eq!(
            child_env.get("DRIP_TEST_SECRET").map(String::as_str),
            Some("explicit-value")
        );
    }

    #[test]
    fn passes_the_environment_through_untouched_when_no_scrub_list_is_set() {
        lock_env!(_guard);

        std::env::remove_var("DRIP_SCRUB_ENV");
        std::env::set_var("DRIP_TEST_KEEP", "visible");

        let child_env = build_child_process_env(None);

        assert_eq!(
            child_env.get("DRIP_TEST_KEEP").map(String::as_str),
            Some("visible")
        );
    }

    #[test]
    fn build_env_unset_arguments_lists_each_scrub_name_then_the_marker() {
        // build_env_unset_arguments is otherwise only exercised indirectly,
        // through the harness tools' env plumbing. Assert the documented
        // shape here so the `env -u NAME…` contract stays pinned.
        lock_env!(_guard);

        std::env::set_var("DRIP_SCRUB_ENV", "A , B,, C");
        assert_eq!(
            build_env_unset_arguments(),
            vec!["-u", "A", "-u", "B", "-u", "C", "-u", "DRIP_SCRUB_ENV"]
        );

        std::env::remove_var("DRIP_SCRUB_ENV");
        assert_eq!(
            build_env_unset_arguments(),
            vec!["-u", "DRIP_SCRUB_ENV"]
        );
    }
}