//! Reading a configuration file (U4).
//!
//! The test that matters most here is
//! [`two_unknown_keys_produce_two_errors_not_one`]: it is the one an
//! implementation built on `#[serde(deny_unknown_fields)]` cannot pass, since
//! that attribute aborts deserialization at the first unknown key. Everything
//! else about the two-phase shape follows from it (D-E).

use std::fs;
use std::path::{Path, PathBuf};

use narsor::config::file::{self, FileError, MAX_FILE_BYTES};
use narsor::config::{SettingKey, SettingValue};

/// A temporary directory that deletes itself when it goes out of scope.
struct TempRoot {
    path: PathBuf,
}

impl TempRoot {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("narsor-config-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&path).expect("create temp root");
        Self { path }
    }

    /// Writes a configuration file and returns its path.
    fn file(&self, contents: &str) -> PathBuf {
        let path = self.path.join("config.yaml");
        fs::write(&path, contents).expect("write the file");
        path
    }

    /// The same, from raw bytes that need not be text.
    fn bytes(&self, contents: &[u8]) -> PathBuf {
        let path = self.path.join("config.yaml");
        fs::write(&path, contents).expect("write the file");
        path
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

/// Parses text without touching the filesystem.
///
/// Returns the same `Result` shape the assertions are written against; the
/// parser itself hands back values and errors together, because `config show`
/// prints the rows that held up beside the rows that did not.
fn parse(contents: &str) -> Result<narsor::config::FileValues, Vec<FileError>> {
    file::parse(Path::new("/etc/narsor/config.yaml"), contents).into_result()
}

/// The errors of a parse that was supposed to fail, as their messages.
fn messages(errors: &[FileError]) -> Vec<String> {
    errors.iter().map(ToString::to_string).collect()
}

#[test]
fn two_unknown_keys_produce_two_errors_not_one() {
    // The shape-forcing case: `deny_unknown_fields` reports the first and
    // stops, which is the whole reason this parses in two phases (D-E).
    let errors = parse("nonsense: 1\ngibberish: 2\n").expect_err("both keys are unknown");

    assert_eq!(
        errors.len(),
        2,
        "both unknown keys must be reported: {:?}",
        messages(&errors)
    );
    let report = messages(&errors).join("\n");
    assert!(report.contains("nonsense"));
    assert!(report.contains("gibberish"));
}

#[test]
fn a_file_setting_everything_yields_every_value_and_no_errors() {
    let values = parse(
        "catalog_dir: /srv/catalog\nstate_dir: /srv/state\nwork_dir: /scratch\n\
         agent_harness: opencode\nagent_timeout: 30m\nlog_level: debug\n",
    )
    .expect("a complete, correct file");

    assert_eq!(
        values.get(SettingKey::CatalogDir),
        Some(&SettingValue::Path(PathBuf::from("/srv/catalog")))
    );
    assert_eq!(
        values.get(SettingKey::StateDir),
        Some(&SettingValue::Path(PathBuf::from("/srv/state")))
    );
    assert_eq!(
        values.get(SettingKey::WorkDir),
        Some(&SettingValue::Path(PathBuf::from("/scratch")))
    );
    assert_eq!(
        values.get(SettingKey::AgentHarness),
        Some(&SettingValue::Text("opencode".to_string()))
    );
    assert_eq!(
        values.get(SettingKey::AgentTimeout),
        Some(&SettingValue::Duration(Some(1800)))
    );
    assert_eq!(
        values.get(SettingKey::LogLevel),
        Some(&SettingValue::Text("debug".to_string()))
    );
}

#[test]
fn a_key_the_file_does_not_mention_is_absent_not_defaulted() {
    // Turning absence into the built-in value belongs one layer up; this type
    // says what the *file* provides.
    let values = parse("log_level: warn\n").expect("one key is enough");

    assert_eq!(
        values.get(SettingKey::LogLevel),
        Some(&SettingValue::Text("warn".to_string()))
    );
    assert_eq!(values.get(SettingKey::CatalogDir), None);
    assert_eq!(values.get(SettingKey::StateDir), None);
    assert_eq!(values.get(SettingKey::WorkDir), None);
    assert_eq!(values.get(SettingKey::AgentHarness), None);
    assert_eq!(values.get(SettingKey::AgentTimeout), None);
}

#[test]
fn a_duration_is_accepted_in_every_spelling_the_workflows_use() {
    for (written, seconds) in [("30m", 1800u64), ("2h", 7200), ("90", 90), ("1h30m", 5400)] {
        let values = parse(&format!("agent_timeout: {written}\n"))
            .unwrap_or_else(|errors| panic!("'{written}' must parse: {:?}", messages(&errors)));
        assert_eq!(
            values.get(SettingKey::AgentTimeout),
            Some(&SettingValue::Duration(Some(seconds))),
            "'{written}' is {seconds} seconds"
        );
    }
}

#[test]
fn no_limit_can_be_written_down() {
    // Otherwise a timeout inherited from /etc/narsor/ could only be replaced,
    // never revoked.
    for written in ["none", "None", "~"] {
        let values = parse(&format!("agent_timeout: {written}\n"))
            .unwrap_or_else(|errors| panic!("'{written}' must parse: {:?}", messages(&errors)));
        assert_eq!(
            values.get(SettingKey::AgentTimeout),
            Some(&SettingValue::Duration(None)),
            "'{written}' means no limit"
        );
    }
}

#[test]
fn an_empty_file_sets_nothing_and_is_not_an_error() {
    // D-H: an empty file is a valid statement that every value is built in.
    for contents in ["", "   \n", "# nothing but a comment\n", "---\n"] {
        let values = parse(contents)
            .unwrap_or_else(|errors| panic!("{contents:?} must parse: {:?}", messages(&errors)));
        assert!(values.is_empty(), "{contents:?} sets nothing");
    }
}

#[test]
fn a_leading_byte_order_mark_does_not_stop_the_parse() {
    let values = parse("\u{feff}log_level: info\n").expect("a BOM is not a syntax error");

    assert_eq!(
        values.get(SettingKey::LogLevel),
        Some(&SettingValue::Text("info".to_string()))
    );
}

#[test]
fn an_unknown_key_names_the_file_and_the_key() {
    // AE3.
    let errors = parse("lgo_level: info\n").expect_err("the key is misspelled");

    let message = messages(&errors).join("\n");
    assert!(message.contains("/etc/narsor/config.yaml"), "{message}");
    assert!(message.contains("lgo_level"), "{message}");
    assert!(
        message.contains("log_level"),
        "the known keys are short enough to print: {message}"
    );
}

#[test]
fn two_independent_mistakes_are_both_reported() {
    let errors = parse("agent_harness: nonesuch\nagent_timeout: tomorrow\n")
        .expect_err("both values are wrong");

    assert_eq!(errors.len(), 2, "{:?}", messages(&errors));
    let report = messages(&errors).join("\n");
    assert!(report.contains("nonesuch"));
    assert!(report.contains("tomorrow"));
}

#[test]
fn an_unknown_key_and_a_bad_value_land_in_one_sorted_list() {
    let errors =
        parse("log_level: shouting\nabsolutely_not_a_key: 1\n").expect_err("two different faults");

    assert_eq!(errors.len(), 2);
    let keys: Vec<&str> = errors.iter().filter_map(FileError::key).collect();
    assert_eq!(
        keys,
        vec!["absolutely_not_a_key", "log_level"],
        "sorted by key, so the same file always reports the same way"
    );
}

#[test]
fn the_order_of_the_file_does_not_change_the_order_of_the_report() {
    let one = parse("zzz_unknown: 1\naaa_unknown: 2\n").expect_err("two unknown keys");
    let other = parse("aaa_unknown: 2\nzzz_unknown: 1\n").expect_err("two unknown keys");

    assert_eq!(messages(&one), messages(&other));
}

#[test]
fn an_unsupported_harness_lists_the_supported_ones() {
    let errors = parse("agent_harness: gpt-pilot\n").expect_err("not a harness this version has");

    let message = messages(&errors).join("\n");
    assert!(message.contains("gpt-pilot"), "{message}");
    assert!(message.contains("claude-code"), "{message}");
    assert!(message.contains("codex"), "{message}");
    assert!(message.contains("opencode"), "{message}");
}

#[test]
fn codex_is_an_accepted_configured_harness() {
    let values = parse("agent_harness: codex\n").expect("codex is registered");
    assert_eq!(
        values.get(SettingKey::AgentHarness),
        Some(&SettingValue::Text("codex".to_string()))
    );
}

#[test]
fn an_unknown_logging_level_lists_the_allowed_ones() {
    let errors = parse("log_level: verbose\n").expect_err("not one of the levels");

    let message = messages(&errors).join("\n");
    assert!(message.contains("verbose"), "{message}");
    for level in ["off", "error", "warn", "info", "debug", "trace"] {
        assert!(message.contains(level), "{level} must be listed: {message}");
    }
}

#[test]
fn a_value_of_the_wrong_shape_says_what_was_expected() {
    let errors = parse("log_level:\n  - info\n").expect_err("a list is not a level");

    let message = messages(&errors).join("\n");
    assert!(message.contains("log_level"), "{message}");
    assert!(message.contains("list"), "{message}");
}

#[test]
fn a_document_that_is_not_a_mapping_is_refused() {
    let errors = parse("- log_level\n- info\n").expect_err("a list is not a configuration");

    assert!(matches!(errors.as_slice(), [FileError::NotAMapping { .. }]));
    assert!(messages(&errors)[0].contains("/etc/narsor/config.yaml"));
}

#[test]
fn a_syntax_error_names_the_file_and_the_place() {
    let errors = parse("log_level: [unterminated\n").expect_err("the list never closes");

    assert!(matches!(errors.as_slice(), [FileError::Syntax { .. }]));
    let message = messages(&errors)[0].clone();
    assert!(message.contains("/etc/narsor/config.yaml"), "{message}");
    assert!(
        message.contains("line") && message.contains("column"),
        "serde_yaml's own location has to survive into the message: {message}"
    );
}

#[test]
fn a_file_over_the_size_limit_is_refused_without_being_parsed() {
    // D-L: YAML anchors let a small file expand in memory, on the start of
    // every single subcommand.
    let root = TempRoot::new();
    let mut contents = String::from("log_level: info\n# ");
    contents.push_str(&"padding ".repeat(((MAX_FILE_BYTES as usize) / 8) + 16));
    let path = root.file(&contents);

    let errors = file::load(&path).into_result().expect_err("over the limit");

    assert!(matches!(errors.as_slice(), [FileError::TooLarge { .. }]));
    let message = messages(&errors)[0].clone();
    assert!(message.contains(&path.display().to_string()), "{message}");
}

#[test]
fn bytes_that_are_not_text_say_so_rather_than_sounding_like_a_permission_problem() {
    let root = TempRoot::new();
    let path = root.bytes(&[0xff, 0xfe, 0x00, 0x41]);

    let errors = file::load(&path).into_result().expect_err("not UTF-8");

    assert!(matches!(errors.as_slice(), [FileError::NotUtf8 { .. }]));
    let message = messages(&errors)[0].clone();
    assert!(message.contains("UTF-8"), "{message}");
    assert!(message.contains(&path.display().to_string()), "{message}");
}

#[test]
fn every_message_names_the_file() {
    let root = TempRoot::new();
    let path = root.file("nonsense: 1\nlog_level: shouting\n");

    let errors = file::load(&path).into_result().expect_err("two faults");

    for error in &errors {
        assert_eq!(error.path(), path.as_path());
        assert!(
            error.to_string().contains(&path.display().to_string()),
            "{error}"
        );
    }
}

#[test]
fn a_file_that_is_not_there_is_a_read_error_naming_it() {
    let root = TempRoot::new();
    let path = root.path.join("absent.yaml");

    let errors = file::load(&path)
        .into_result()
        .expect_err("nothing to read");

    assert!(matches!(errors.as_slice(), [FileError::Read { .. }]));
    assert!(messages(&errors)[0].contains("absent.yaml"));
}

#[test]
fn the_keys_that_held_up_survive_beside_the_ones_that_did_not() {
    // `config show` prints the good rows beside the bad ones, so a file with
    // one mistake still has to yield the values of every other key (D-D).
    let parsed = file::parse(
        Path::new("/etc/narsor/config.yaml"),
        "state_dir: /srv/narsor\nagent_timeout: tomorrow\n",
    );

    assert_eq!(parsed.errors.len(), 1, "only the timeout is wrong");
    assert_eq!(
        parsed.values.get(SettingKey::StateDir),
        Some(&SettingValue::Path(PathBuf::from("/srv/narsor"))),
        "the key the operator got right is still in force"
    );
    assert!(!parsed.is_ok());
}
