//! The Kubernetes manifests carry the runner contract variable by its exact
//! name: the controller forwards only it to runner Pods, and the admission
//! policy lets only it through (U5). A rename that misses one of them stops
//! runners from receiving `runner.agent_args` without any error.

use narsor_engine::config::AGENT_ARGS_ENV;

fn manifest(name: &str) -> String {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("deploy/kubernetes")
        .join(name);
    std::fs::read_to_string(&path).unwrap_or_else(|error| panic!("{}: {error}", path.display()))
}

/// Every `NARSOR_…` word of `text`, as written.
fn narsor_words(text: &str) -> Vec<&str> {
    text.match_indices("NARSOR_")
        .map(|(start, _)| {
            let rest = &text[start..];
            let end = rest
                .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                .unwrap_or(rest.len());
            &rest[..end]
        })
        .collect()
}

#[test]
fn the_manifests_name_the_runner_variable_exactly_and_never_a_prefix() {
    for name in ["admission-policy.yaml", "controller.yaml"] {
        let text = manifest(name);
        let words = narsor_words(&text);
        assert!(
            words.contains(&AGENT_ARGS_ENV),
            "{name} does not name {AGENT_ARGS_ENV}"
        );
        for word in words {
            assert!(
                !word.ends_with('_'),
                "{name}: '{word}' reads like a prefix match"
            );
            assert!(
                word == AGENT_ARGS_ENV || !word.contains("AGENT_ARGS"),
                "{name}: '{word}' is not the runner variable's current name"
            );
        }
    }

    // The policy's allowlist holds that one variable and nothing else.
    let policy = manifest("admission-policy.yaml");
    assert!(
        narsor_words(&policy)
            .iter()
            .all(|word| *word == AGENT_ARGS_ENV),
        "{:?}",
        narsor_words(&policy)
    );
    assert!(!policy.contains("NARSOR_*") && !policy.contains("startsWith('NARSOR"));
}
