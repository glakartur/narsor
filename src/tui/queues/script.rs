//! Script editing in an external editor (KTD-10).
//!
//! `$VISUAL`, then `$EDITOR`, names the editor. The loop suspends the
//! terminal (`tui::suspend_terminal`), runs the editor on a private
//! temporary file that holds the text of the newest version, takes the
//! terminal back and hands the saved text to the application, which saves
//! it as the next version — never approved. With no editor set the view
//! opens a paste field instead.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::Command;

/// The editor `lookup` names: `VISUAL`, then `EDITOR`; a blank value is
/// none.
pub fn editor_from(lookup: impl Fn(&str) -> Option<String>) -> Option<String> {
    ["VISUAL", "EDITOR"]
        .into_iter()
        .filter_map(lookup)
        .map(|value| value.trim().to_string())
        .find(|value| !value.is_empty())
}

/// Runs `editor` on a temporary file that starts with `initial`, and
/// returns what the file holds when the editor exits. `None` when the
/// editor exits with an error: the owner gave up.
///
/// The editor runs through `sh -c`, so a value with arguments
/// (`code --wait`) works; the file is its last argument. The file is
/// created only for this user and removed afterwards.
pub fn edit_in_editor(editor: &str, initial: &str) -> io::Result<Option<String>> {
    let path = temp_path();
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let result = (|| {
        options.open(&path)?.write_all(initial.as_bytes())?;
        let status = Command::new("sh")
            .arg("-c")
            .arg(format!("{editor} \"$1\""))
            .arg("sh")
            .arg(&path)
            .status()?;
        if !status.success() {
            return Ok(None);
        }
        fs::read_to_string(&path).map(Some)
    })();
    let _ = fs::remove_file(&path);
    result
}

fn temp_path() -> PathBuf {
    std::env::temp_dir().join(format!("narsor-observer-{}.sh", uuid::Uuid::new_v4()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn visual_wins_over_editor_and_a_blank_value_is_none() {
        let both = |name: &str| match name {
            "VISUAL" => Some("vim".to_string()),
            "EDITOR" => Some("nano".to_string()),
            _ => None,
        };
        assert_eq!(editor_from(both).as_deref(), Some("vim"));
        let blank_visual = |name: &str| match name {
            "VISUAL" => Some("  ".to_string()),
            "EDITOR" => Some("nano".to_string()),
            _ => None,
        };
        assert_eq!(editor_from(blank_visual).as_deref(), Some("nano"));
        assert_eq!(editor_from(|_| None), None);
    }

    #[cfg(unix)]
    #[test]
    fn the_editor_gets_the_text_and_what_it_saves_comes_back() {
        // An "editor" that appends a line to the file it is given.
        let edited = edit_in_editor("printf 'echo two\\n' >>", "echo one\n").unwrap();
        assert_eq!(edited.as_deref(), Some("echo one\necho two\n"));
        assert_eq!(edit_in_editor("false", "x").unwrap(), None);
    }
}
