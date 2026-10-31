//! Terminal prompts for `narsor-cli init`: an arrow-key menu and a path field
//! with a prefilled default and Tab completion. Both draw on stderr and need
//! a terminal on stdin; callers fall back to plain line prompts otherwise.

use std::{
    env, fs,
    io::{self, IsTerminal, Write},
    path::PathBuf,
};

use crossterm::{
    cursor::{MoveToColumn, MoveUp},
    event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    queue,
    style::{Attribute, Print, SetAttribute},
    terminal::{self, Clear, ClearType},
};

use crate::cli::commands::{CliError, Result};

pub fn is_available() -> bool {
    io::stdin().is_terminal() && io::stderr().is_terminal()
}

struct RawMode;

impl RawMode {
    fn enter() -> Result<Self> {
        terminal::enable_raw_mode()
            .map_err(|error| CliError::failure(format!("cannot use the terminal: {error}")))?;
        Ok(Self)
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        let _ = terminal::disable_raw_mode();
    }
}

fn terminal_error(error: io::Error) -> CliError {
    CliError::failure(format!("terminal error: {error}"))
}

fn cancelled() -> CliError {
    CliError::failure("cancelled")
}

fn read_key() -> Result<KeyEvent> {
    loop {
        match event::read().map_err(terminal_error)? {
            Event::Key(key) if key.kind != KeyEventKind::Release => {
                if key.code == KeyCode::Esc
                    || (key.code == KeyCode::Char('c') && key.modifiers == KeyModifiers::CONTROL)
                {
                    return Err(cancelled());
                }
                return Ok(key);
            }
            _ => {}
        }
    }
}

/// Lets the user pick one item with the arrow keys (or `j`/`k`) and Enter.
/// Items are `(name, description)`; returns the index of the chosen one.
pub fn select(label: &str, items: &[(&str, &str)]) -> Result<usize> {
    let mut err = io::stderr();
    let _raw = RawMode::enter()?;
    let mut index = 0;
    let width = items.iter().map(|(name, _)| name.len()).max().unwrap_or(0);
    queue!(
        err,
        Print(format!("{label} (↑/↓ to move, Enter to confirm)\r\n"))
    )
    .map_err(terminal_error)?;
    loop {
        for (position, (name, description)) in items.iter().enumerate() {
            if position == index {
                queue!(
                    err,
                    SetAttribute(Attribute::Reverse),
                    Print(format!("  › {name:width$}  {description} ")),
                    SetAttribute(Attribute::Reset),
                    Print("\r\n")
                )
            } else {
                queue!(err, Print(format!("    {name:width$}  {description}\r\n")))
            }
            .map_err(terminal_error)?;
        }
        err.flush().map_err(terminal_error)?;
        match read_key()?.code {
            KeyCode::Up | KeyCode::Char('k') => index = (index + items.len() - 1) % items.len(),
            KeyCode::Down | KeyCode::Char('j') => index = (index + 1) % items.len(),
            KeyCode::Enter => break,
            _ => {}
        }
        queue!(
            err,
            MoveUp(items.len() as u16),
            MoveToColumn(0),
            Clear(ClearType::FromCursorDown)
        )
        .map_err(terminal_error)?;
    }
    queue!(
        err,
        MoveUp(items.len() as u16 + 1),
        MoveToColumn(0),
        Clear(ClearType::FromCursorDown),
        Print(format!("{label}: {}\r\n", items[index].0))
    )
    .map_err(terminal_error)?;
    err.flush().map_err(terminal_error)?;
    Ok(index)
}

/// A one-line path field. The default is prefilled and editable; Tab completes
/// directory names; Enter accepts.
pub fn input_path(label: &str, default: &str) -> Result<String> {
    let mut err = io::stderr();
    let _raw = RawMode::enter()?;
    let mut text = default.to_string();
    loop {
        queue!(
            err,
            MoveToColumn(0),
            Clear(ClearType::CurrentLine),
            Print(format!("{label}: {text}"))
        )
        .map_err(terminal_error)?;
        err.flush().map_err(terminal_error)?;
        let key = read_key()?;
        match key.code {
            KeyCode::Enter => break,
            KeyCode::Backspace => {
                text.pop();
            }
            KeyCode::Tab => {
                let (completed, candidates) = complete(&text);
                text = completed;
                if candidates.len() > 1 {
                    queue!(err, Print("\r\n")).map_err(terminal_error)?;
                    for candidate in candidates {
                        queue!(err, Print(format!("  {candidate}/\r\n")))
                            .map_err(terminal_error)?;
                    }
                }
            }
            KeyCode::Char('u') if key.modifiers == KeyModifiers::CONTROL => text.clear(),
            KeyCode::Char('w') if key.modifiers == KeyModifiers::CONTROL => {
                let trimmed = text.trim_end_matches('/');
                text.truncate(trimmed.rfind('/').map_or(0, |at| at + 1));
            }
            KeyCode::Char(character)
                if !key.modifiers.contains(KeyModifiers::CONTROL)
                    && !key.modifiers.contains(KeyModifiers::ALT) =>
            {
                text.push(character);
            }
            _ => {}
        }
    }
    queue!(err, Print("\r\n")).map_err(terminal_error)?;
    err.flush().map_err(terminal_error)?;
    Ok(text)
}

/// Completes the last path segment of `text` against existing directories.
/// Returns the new text and, when several directories match, their names.
fn complete(text: &str) -> (String, Vec<String>) {
    let (directory_part, prefix) = match text.rfind('/') {
        Some(at) => (&text[..=at], &text[at + 1..]),
        None => ("", text),
    };
    let directory = if directory_part.is_empty() {
        PathBuf::from(".")
    } else {
        expand_tilde(directory_part)
    };
    let Ok(entries) = fs::read_dir(&directory) else {
        return (text.to_string(), Vec::new());
    };
    let mut names: Vec<String> = entries
        .filter_map(std::result::Result::ok)
        .filter(|entry| entry.path().is_dir())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| {
            name.starts_with(prefix) && (prefix.starts_with('.') || !name.starts_with('.'))
        })
        .collect();
    names.sort();
    match names.as_slice() {
        [] => (text.to_string(), Vec::new()),
        [only] => (format!("{directory_part}{only}/"), Vec::new()),
        _ => {
            let common = names.iter().skip(1).fold(names[0].clone(), |common, name| {
                common
                    .chars()
                    .zip(name.chars())
                    .take_while(|(left, right)| left == right)
                    .map(|(character, _)| character)
                    .collect()
            });
            (format!("{directory_part}{common}"), names)
        }
    }
}

/// Expands a leading `~` or `~/` to the home directory.
pub fn expand_tilde(text: &str) -> PathBuf {
    let home = env::var_os("HOME").filter(|home| !home.is_empty());
    match (text, home) {
        ("~", Some(home)) => PathBuf::from(home),
        (text, Some(home)) if text.starts_with("~/") => PathBuf::from(home).join(&text[2..]),
        _ => PathBuf::from(text),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn completes_a_unique_directory_and_the_common_prefix() {
        let root = env::temp_dir().join(format!("narsor-prompt-{}", std::process::id()));
        fs::create_dir_all(root.join("alpha-one")).unwrap();
        fs::create_dir_all(root.join("alpha-two")).unwrap();
        fs::create_dir_all(root.join("beta")).unwrap();
        fs::create_dir_all(root.join(".hidden")).unwrap();
        let base = format!("{}/", root.display());

        assert_eq!(
            complete(&format!("{base}be")),
            (format!("{base}beta/"), Vec::new())
        );
        let (text, names) = complete(&format!("{base}al"));
        assert_eq!(text, format!("{base}alpha-"));
        assert_eq!(names, ["alpha-one", "alpha-two"]);
        assert_eq!(complete(&format!("{base}.h")).0, format!("{base}.hidden/"));
        assert_eq!(complete(&format!("{base}zz")).0, format!("{base}zz"));
        fs::remove_dir_all(root).unwrap();
    }
}
