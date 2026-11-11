//! `config show`: every setting, its value and its source.

use crate::cli::ConfigFormat;
use crate::config::{render_value, Resolved, SettingKey, Source};

use super::{CliError, Result};

/// `narsor config show [--format json]` — every setting, its value and its
/// source.
///
/// The complete set, including the settings nobody touched: a listing that
/// hides the built-ins answers "what did I change" when the question being
/// asked is "what is in force" (R7).
///
/// # It still works when the configuration does not
///
/// This is the only command that runs on a file it could not use (D-D). An
/// operator reaching for it is, by definition, reaching for it because
/// something is wrong; a diagnostic tool that stops working in the situation
/// it exists for is no tool at all. Broken keys are printed as rows carrying
/// the complaint instead of a value, and the command still exits 1 so that a
/// script branching on the code is not misled.
///
/// The one failure it does not survive is a `--config` path that is not there
/// — that is a usage error, handled before this is ever called.
pub fn config_show(config: &Resolved, format: ConfigFormat) -> Result<()> {
    match format {
        ConfigFormat::Table => print_config_table(config),
        ConfigFormat::Json => print_config_json(config),
    }

    if config.is_ok() {
        return Ok(());
    }

    // The payload has already gone to stdout; this is the diagnostic half, and
    // it carries the exit code.
    Err(CliError::failure(format!(
        "The configuration has {} problem(s); they are reported above.",
        config.errors().len()
    )))
}

/// What stands in the value column of a setting whose value did not hold up.
const CONFIG_ERROR_MARKER: &str = "error";

/// Width of the setting-name column.
const SETTING_WIDTH: usize = 16;

/// Width the value column is padded to.
///
/// A minimum, not a maximum: a value longer than this pushes the source column
/// right rather than being cut. `data_dir` is a path and routinely runs past
/// it, and a truncated path is a *wrong* answer to the one question this
/// command exists to answer (R8).
const VALUE_WIDTH: usize = 28;

/// The table `config show` prints by default.
///
/// The same fixed-width shape as `workflow list`, continuation lines and all,
/// because it is the same idea: one row per thing, with what is behind it on
/// the lines below.
fn print_config_table(config: &Resolved) {
    println!(
        "{:<SETTING_WIDTH$} {:<VALUE_WIDTH$} SOURCE",
        "SETTING", "VALUE"
    );

    for setting in SettingKey::ALL {
        match masking_error(config, setting) {
            Some(message) => {
                println!(
                    "{:<SETTING_WIDTH$} {:<VALUE_WIDTH$} -",
                    setting.key(),
                    format!("({CONFIG_ERROR_MARKER})")
                );
                println!("{:<SETTING_WIDTH$} {:<VALUE_WIDTH$} {}", "", "", message);
            }
            None => {
                let value = render_value(config.value(setting), setting.is_sensitive());
                println!(
                    "{:<SETTING_WIDTH$} {:<VALUE_WIDTH$} {}",
                    setting.key(),
                    value,
                    config.source(setting)
                );
            }
        }
    }

    if let Some(path) = config.file().path() {
        println!(
            "{:<SETTING_WIDTH$} {:<VALUE_WIDTH$} file: {}",
            "",
            "",
            path.display()
        );
    }
    for shadowed in &config.file().shadowed {
        println!(
            "{:<SETTING_WIDTH$} {:<VALUE_WIDTH$} shadowed: {}",
            "",
            "",
            shadowed.path.display()
        );
    }

    // Everything that is wrong but belongs to no row of the table: an unknown
    // key, a syntax error, a candidate that is not a readable file.
    for error in config.errors() {
        // Everything that marked no row of the table: an unknown key, a syntax
        // error, a candidate that is not a readable file — and a keyed fault
        // that a higher-precedence layer overruled.
        let marked_a_row = error
            .key()
            .and_then(SettingKey::from_key)
            .is_some_and(|setting| masking_error(config, setting).is_some());
        if !marked_a_row {
            println!(
                "{:<SETTING_WIDTH$} {:<VALUE_WIDTH$} {CONFIG_ERROR_MARKER}: {error}",
                "", ""
            );
        }
    }
}

/// The machine-readable shape, alone on stdout.
///
/// The version number is the point of the envelope: this is the contract a
/// `jq` invocation in someone's runbook will be written against, and a later
/// change of shape should be a recognisable change rather than a breakage
/// (D-O). The source is an object rather than the string `"file (/etc/…)"` so
/// that nothing downstream has to parse a sentence.
fn print_config_json(config: &Resolved) {
    let mut settings = serde_json::Map::new();
    for setting in SettingKey::ALL {
        let mut entry = serde_json::Map::new();
        match masking_error(config, setting) {
            Some(message) => {
                entry.insert("error".to_string(), serde_json::Value::String(message));
                entry.insert("value".to_string(), serde_json::Value::Null);
                entry.insert("source".to_string(), serde_json::Value::Null);
            }
            None => {
                entry.insert("error".to_string(), serde_json::Value::Null);
                entry.insert(
                    "value".to_string(),
                    serde_json::Value::String(render_value(
                        config.value(setting),
                        setting.is_sensitive(),
                    )),
                );
                let source = config.source(setting);
                entry.insert(
                    "source".to_string(),
                    serde_json::json!({
                        "kind": source.label(),
                        "detail": source.detail(),
                    }),
                );
            }
        }
        entry.insert(
            "sensitive".to_string(),
            serde_json::Value::Bool(setting.is_sensitive()),
        );
        settings.insert(setting.key().to_string(), serde_json::Value::Object(entry));
    }

    let document = serde_json::json!({
        "version": CONFIG_JSON_VERSION,
        "file": config.file().path().map(|path| path.display().to_string()),
        "shadowed": config
            .file()
            .shadowed
            .iter()
            .map(|candidate| candidate.path.display().to_string())
            .collect::<Vec<_>>(),
        "settings": serde_json::Value::Object(settings),
        "errors": config
            .errors()
            .iter()
            .map(|error| serde_json::json!({
                "key": error.key(),
                "message": error.to_string(),
            }))
            .collect::<Vec<_>>(),
    });

    println!(
        "{}",
        serde_json::to_string_pretty(&document).expect("the document is plain data")
    );
}

/// The version of the JSON shape.
///
/// Bumped when the shape changes in a way a reader would notice.
const CONFIG_JSON_VERSION: u32 = 1;

/// The complaint that actually cost this setting its value, if any.
///
/// A fault in a layer that lost decided nothing: with a bad `agent_harness` in
/// the file *and* a good one in the environment, the environment still wins,
/// and the row has to say so. Marking it `(error)` would hide the value that
/// is genuinely in force and contradict the per-setting layering this whole
/// module is about (R2).
///
/// A fault masks the value exactly when nothing outranked it — which is
/// exactly when the setting fell back to the value built into the binary.
fn masking_error(config: &Resolved, setting: SettingKey) -> Option<String> {
    if config.source(setting) != &Source::BuiltIn {
        return None;
    }
    config
        .errors()
        .iter()
        .find(|error| error.key() == Some(setting.key()))
        .map(ToString::to_string)
}
