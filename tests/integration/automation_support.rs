//! Catalogue automations and queues for the observer tests: files written
//! under a catalogue root, as a catalogue author writes them (R1).
//!
//! The tests seed the environment catalogue and the state on the same root
//! (`environment_support::seeded_resolver`), as an installation that sets
//! nothing does, so the files here sit next to the runtime state.

use std::fs;
use std::path::Path;

/// The `check` script of the helpers: prints an empty state, exits 0.
pub const CHECK_SCRIPT: &str = "echo '{\"state\": {}}'\n";

/// Writes `text` to `relative` under `root`, with its directories.
pub fn write(root: &Path, relative: &str, text: &str) {
    let path = root.join(relative);
    fs::create_dir_all(path.parent().expect("a parent")).expect("catalogue dir");
    fs::write(path, text).expect("catalogue file");
}

/// The automation `name` with `check.sh` = `check` and the given secrets
/// (`(name, env)`).
pub fn automation(root: &Path, name: &str, check: &str, secrets: &[(&str, &str)]) {
    let mut definition = format!("description: {name} of the tests\n");
    if !secrets.is_empty() {
        definition.push_str("secrets:\n");
        for (secret, env) in secrets {
            definition.push_str(&format!("  - {{name: {secret}, env: {env}}}\n"));
        }
    }
    write(
        root,
        &format!("automations/{name}/automation.yaml"),
        &definition,
    );
    write(root, &format!("automations/{name}/check.sh"), check);
}

/// The `on_episodes` queue `queue` served by `automations`, on `schedule`
/// (a YAML flow mapping such as `{interval_secs: 300}`).
pub fn on_episodes_queue(root: &Path, queue: &str, automations: &[&str], schedule: &str) {
    write(
        root,
        &format!("queues/{queue}.yaml"),
        &format!(
            "automations: [{}]\nmode: on_episodes\nschedule: {schedule}\n",
            automations.join(", ")
        ),
    );
}

/// The `always` queue `queue` served by `automation`, on `schedule`.
pub fn always_queue(root: &Path, queue: &str, automation: &str, schedule: &str) {
    write(
        root,
        &format!("queues/{queue}.yaml"),
        &format!("automations: [{automation}]\nmode: always\nschedule: {schedule}\n"),
    );
}

/// `mr-review` (every 300 s, `on_episodes`) served by `gitlab-mr-label`,
/// whose `check` script exits 0.
pub fn mr_review(root: &Path) {
    automation(root, "gitlab-mr-label", CHECK_SCRIPT, &[]);
    on_episodes_queue(
        root,
        "mr-review",
        &["gitlab-mr-label"],
        "{interval_secs: 300}",
    );
}
