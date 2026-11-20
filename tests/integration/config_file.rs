//! Reading a configuration file: a tree with one section per service, of
//! which a binary walks only the sections of its scope.
//!
//! The test that matters most for the two-phase shape is
//! [`several_errors_in_one_section_are_all_reported_sorted_by_path`]: an
//! implementation built on `#[serde(deny_unknown_fields)]` cannot pass it,
//! since that attribute aborts deserialization at the first unknown key
//! (D-E).

use std::fs;
use std::path::{Path, PathBuf};

use narsor_engine::config::file::{self, FileError, MAX_FILE_BYTES};
use narsor_engine::config::{Scope, SettingKey, SettingValue};

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

/// Parses text without touching the filesystem, with every section in scope.
///
/// Returns the same `Result` shape the assertions are written against; the
/// parser itself hands back values and errors together, because `config show`
/// prints the rows that held up beside the rows that did not.
fn parse(contents: &str) -> Result<narsor_engine::config::FileValues, Vec<FileError>> {
    parse_in(contents, Scope::ALL)
}

/// The same, with only the sections of `scope`.
fn parse_in(
    contents: &str,
    scope: Scope,
) -> Result<narsor_engine::config::FileValues, Vec<FileError>> {
    file::parse(Path::new("/etc/narsor/config.yaml"), contents, &scope).into_result()
}

/// The errors of a parse that was supposed to fail, as their messages.
fn messages(errors: &[FileError]) -> Vec<String> {
    errors.iter().map(ToString::to_string).collect()
}

/// The paths the errors are about, in the order reported.
fn keys(errors: &[FileError]) -> Vec<&str> {
    errors.iter().filter_map(FileError::key).collect()
}

/// Every binary's scope.
const BINARY_SCOPES: [Scope; 6] = [
    Scope::SERVER,
    Scope::WEB,
    Scope::TUI,
    Scope::CLI,
    Scope::RUNNER,
    Scope::CONTROLLER,
];

#[test]
fn a_binary_gets_the_values_of_its_own_sections_only() {
    let values = parse_in(
        "global:\n  log_level: debug\n\
         server:\n  catalog: /srv/catalog\n  agent_harness: opencode\n  oidc:\n    issuer: https://id.example.com\n\
         web:\n  bind: 127.0.0.1:9000\n",
        Scope::SERVER,
    )
    .expect("a correct file");

    assert_eq!(
        values.get(SettingKey::LogLevel),
        Some(&SettingValue::Text("debug".to_string()))
    );
    assert_eq!(
        values.get(SettingKey::CatalogDir),
        Some(&SettingValue::Path(PathBuf::from("/srv/catalog")))
    );
    assert_eq!(
        values.get(SettingKey::AgentHarness),
        Some(&SettingValue::Text("opencode".to_string()))
    );
    assert_eq!(
        values.get(SettingKey::OidcIssuer),
        Some(&SettingValue::Text("https://id.example.com".to_string()))
    );
    assert_eq!(
        values.get(SettingKey::WebBind),
        None,
        "the web section is not the server's to read"
    );
    assert_eq!(
        values.get(SettingKey::StateDir),
        None,
        "absent, not defaulted"
    );
}

#[test]
fn an_unknown_key_is_reported_only_to_the_binary_that_owns_the_section() {
    let contents = "server:\n  oidc:\n    isuer: https://id.example.com\n";

    let errors = parse_in(contents, Scope::SERVER).expect_err("the key is misspelled");
    assert_eq!(keys(&errors), vec!["server.oidc.isuer"]);
    let message = messages(&errors).join("\n");
    assert!(message.contains("/etc/narsor/config.yaml"), "{message}");
    assert!(
        message.contains("server.oidc.issuer"),
        "the known paths of the section are listed: {message}"
    );

    assert!(
        parse_in(contents, Scope::WEB).is_ok(),
        "an error in the server section never stops narsor-web"
    );
}

#[test]
fn unknown_roots_and_old_flat_keys_are_ignored_by_every_binary() {
    // AE4, AE7: a misspelled root and a flat key of the previous format.
    let contents = "sever:\n  mode: network\nserver_mode: network\n";
    for scope in BINARY_SCOPES.into_iter().chain([Scope::ALL]) {
        let values = parse_in(contents, scope)
            .unwrap_or_else(|errors| panic!("{scope:?}: {:?}", messages(&errors)));
        assert!(
            values.is_empty(),
            "{scope:?} read a value from an unknown root"
        );
    }
}

#[test]
fn a_section_or_a_group_written_as_a_scalar_is_named_in_the_error() {
    for (contents, path) in [
        ("server: 5\n", "server"),
        ("server:\n  oidc: x\n", "server.oidc"),
    ] {
        let errors = parse_in(contents, Scope::SERVER).expect_err(contents);
        assert_eq!(keys(&errors), vec![path], "{contents}");
        assert!(
            messages(&errors)[0].contains("mapping"),
            "{:?}",
            messages(&errors)
        );
    }
}

#[test]
fn a_client_reads_server_state_and_nothing_else_of_the_server_section() {
    let broken_mode = "server:\n  mode: shouting\n  state: /srv/state\n";
    for client in [Scope::WEB, Scope::TUI, Scope::CLI] {
        let values = parse_in(broken_mode, client)
            .unwrap_or_else(|errors| panic!("{client:?}: {:?}", messages(&errors)));
        assert_eq!(
            values.get(SettingKey::StateDir),
            Some(&SettingValue::Path(PathBuf::from("/srv/state")))
        );
        assert_eq!(values.get(SettingKey::ServerMode), None);

        let errors =
            parse_in("server:\n  state: [a, b]\n", client).expect_err("a list is not a directory");
        assert_eq!(keys(&errors), vec!["server.state"], "{client:?}");
    }
}

#[test]
fn several_errors_in_one_section_are_all_reported_sorted_by_path() {
    // The shape-forcing case: `deny_unknown_fields` reports the first and
    // stops, which is the whole reason this parses in two phases (D-E).
    let errors = parse_in(
        "server:\n  zzz_unknown: 1\n  mode: shouting\n  agent_harness: nonesuch\n  aaa_unknown: 2\n",
        Scope::SERVER,
    )
    .expect_err("four faults");

    assert_eq!(
        keys(&errors),
        vec![
            "server.aaa_unknown",
            "server.agent_harness",
            "server.mode",
            "server.zzz_unknown"
        ],
        "every fault, sorted by path whatever the order of the file"
    );
}

#[test]
fn agent_args_are_a_mapping_inside_the_runner_section() {
    let values = parse_in(
        "runner:\n  agent_args:\n    claude: [--x, --max-turns, 5]\n",
        Scope::RUNNER,
    )
    .unwrap_or_else(|errors| panic!("{:?}", messages(&errors)));

    let text = values
        .get(SettingKey::AgentArgs)
        .and_then(SettingValue::as_text)
        .expect("agent args are held as text");
    assert!(text.contains("--max-turns"), "{text}");
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
    let values =
        parse("\u{feff}global:\n  log_level: info\n").expect("a BOM is not a syntax error");

    assert_eq!(
        values.get(SettingKey::LogLevel),
        Some(&SettingValue::Text("info".to_string()))
    );
}

#[test]
fn an_unsupported_harness_lists_the_supported_ones() {
    let errors =
        parse("server:\n  agent_harness: gpt-pilot\n").expect_err("not a harness this version has");

    let message = messages(&errors).join("\n");
    assert!(message.contains("gpt-pilot"), "{message}");
    assert!(message.contains("claude-code"), "{message}");
    assert!(message.contains("codex"), "{message}");
    assert!(message.contains("opencode"), "{message}");
}

#[test]
fn codex_is_an_accepted_configured_harness() {
    let values = parse("server:\n  agent_harness: codex\n").expect("codex is registered");
    assert_eq!(
        values.get(SettingKey::AgentHarness),
        Some(&SettingValue::Text("codex".to_string()))
    );
}

#[test]
fn an_unknown_logging_level_lists_the_allowed_ones() {
    let errors = parse("global:\n  log_level: verbose\n").expect_err("not one of the levels");

    let message = messages(&errors).join("\n");
    assert!(message.contains("verbose"), "{message}");
    for level in ["off", "error", "warn", "info", "debug", "trace"] {
        assert!(message.contains(level), "{level} must be listed: {message}");
    }
}

#[test]
fn a_value_of_the_wrong_shape_says_what_was_expected() {
    let errors = parse("global:\n  log_level:\n    - info\n").expect_err("a list is not a level");

    let message = messages(&errors).join("\n");
    assert!(message.contains("global.log_level"), "{message}");
    assert!(message.contains("list"), "{message}");
}

#[test]
fn a_document_that_is_not_a_mapping_is_refused() {
    let errors = parse("- global\n- server\n").expect_err("a list is not a configuration");

    assert!(matches!(errors.as_slice(), [FileError::NotAMapping { .. }]));
    assert!(messages(&errors)[0].contains("/etc/narsor/config.yaml"));
}

#[test]
fn a_syntax_error_names_the_file_and_the_place() {
    let errors = parse("global:\n  log_level: [unterminated\n").expect_err("the list never closes");

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
    let mut contents = String::from("global:\n  log_level: info\n# ");
    contents.push_str(&"padding ".repeat(((MAX_FILE_BYTES as usize) / 8) + 16));
    let path = root.file(&contents);

    let errors = file::load(&path, &Scope::ALL)
        .into_result()
        .expect_err("over the limit");

    assert!(matches!(errors.as_slice(), [FileError::TooLarge { .. }]));
    let message = messages(&errors)[0].clone();
    assert!(message.contains(&path.display().to_string()), "{message}");
}

#[test]
fn bytes_that_are_not_text_say_so_rather_than_sounding_like_a_permission_problem() {
    let root = TempRoot::new();
    let path = root.bytes(&[0xff, 0xfe, 0x00, 0x41]);

    let errors = file::load(&path, &Scope::ALL)
        .into_result()
        .expect_err("not UTF-8");

    assert!(matches!(errors.as_slice(), [FileError::NotUtf8 { .. }]));
    let message = messages(&errors)[0].clone();
    assert!(message.contains("UTF-8"), "{message}");
    assert!(message.contains(&path.display().to_string()), "{message}");
}

#[test]
fn every_message_names_the_file() {
    let root = TempRoot::new();
    let path = root.file("global:\n  nonsense: 1\n  log_level: shouting\n");

    let errors = file::load(&path, &Scope::ALL)
        .into_result()
        .expect_err("two faults");

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

    let errors = file::load(&path, &Scope::ALL)
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
        "server:\n  state: /srv/narsor\n  bind: tomorrow\n",
        &Scope::SERVER,
    );

    assert_eq!(parsed.errors.len(), 1, "only the bind address is wrong");
    assert_eq!(
        parsed.values.get(SettingKey::StateDir),
        Some(&SettingValue::Path(PathBuf::from("/srv/narsor"))),
        "the key the operator got right is still in force"
    );
    assert!(!parsed.is_ok());
}

#[test]
fn the_tui_refresh_interval_takes_a_duration_or_none_but_never_zero() {
    for (written, expected) in [
        ("30", Some(30u64)),
        ("2m", Some(120)),
        ("none", None),
        ("~", None),
    ] {
        let values = parse(&format!("tui:\n  refresh_interval: {written}\n"))
            .unwrap_or_else(|errors| panic!("'{written}' must parse: {:?}", messages(&errors)));
        assert_eq!(
            values.get(SettingKey::TuiRefreshInterval),
            Some(&SettingValue::Duration(expected)),
            "'{written}'"
        );
    }

    // Zero would re-read the daemon on every redraw.
    for written in ["0", "0s"] {
        let errors =
            parse(&format!("tui:\n  refresh_interval: {written}\n")).expect_err("zero is refused");
        let report = messages(&errors).join("\n");
        assert!(report.contains("at least one second"), "{report}");
    }
}
