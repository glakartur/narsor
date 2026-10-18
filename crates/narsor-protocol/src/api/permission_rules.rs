//! Claude Code allow rules for a denied Bash command (KTD3).
//!
//! Claude Code checks a compound command one part at a time: `a && b | c`
//! runs when each of `a`, `b` and `c` is allowed, and a rule naming the
//! whole text allows nothing. A grant for a command is therefore one rule
//! per part. Redirections to another descriptor or to `/dev/null` are not
//! part of what a rule names; Claude Code leaves them out when it matches.
//!
//! Only plain commands are granted: words, quoted words and the operators
//! `&&`, `||`, `;` and `|`. Anything whose meaning a rule could not pin down —
//! command or variable substitution, escapes, globs, subshells, braces,
//! comments, input or file redirection, a background `&` — is refused rather
//! than widened.

/// How far a grant reaches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BashGrantScope {
    /// Exactly the parts of this command.
    Exact,
    /// Each part's program and first argument, with any arguments after.
    Group,
}

/// Programs that run other programs: a group rule for one of them would
/// allow anything, so only the exact command is granted.
const RUNNERS: [&str; 19] = [
    "sh",
    "bash",
    "zsh",
    "fish",
    "dash",
    "ksh",
    "csh",
    "env",
    "sudo",
    "doas",
    "xargs",
    "find",
    "python",
    "python3",
    "node",
    "ruby",
    "perl",
    "pwsh",
    "powershell",
];

/// The parts of `command`, each as its words in their source spelling, or
/// `None` when the command is not plain (see the module documentation).
pub fn bash_command_parts(command: &str) -> Option<Vec<Vec<String>>> {
    let mut parts: Vec<Vec<String>> = Vec::new();
    let mut words: Vec<String> = Vec::new();
    let mut word = String::new();
    let mut chars = command.chars().peekable();
    let end_word = |word: &mut String, words: &mut Vec<String>| {
        if !word.is_empty() {
            words.push(std::mem::take(word));
        }
    };
    let end_part = |words: &mut Vec<String>, parts: &mut Vec<Vec<String>>| -> Option<()> {
        if words.is_empty() {
            return None;
        }
        parts.push(std::mem::take(words));
        Some(())
    };
    while let Some(c) = chars.next() {
        match c {
            ' ' | '\t' => end_word(&mut word, &mut words),
            '\'' => {
                word.push(c);
                loop {
                    let next = chars.next()?;
                    word.push(next);
                    if next == '\'' {
                        break;
                    }
                }
            }
            '"' => {
                word.push(c);
                loop {
                    let next = chars.next()?;
                    if matches!(next, '$' | '`' | '\\') {
                        return None;
                    }
                    word.push(next);
                    if next == '"' {
                        break;
                    }
                }
            }
            '&' if chars.peek() == Some(&'&') => {
                chars.next();
                end_word(&mut word, &mut words);
                end_part(&mut words, &mut parts)?;
            }
            '|' => {
                if chars.peek() == Some(&'|') {
                    chars.next();
                } else if chars.peek() == Some(&'&') {
                    return None;
                }
                end_word(&mut word, &mut words);
                end_part(&mut words, &mut parts)?;
            }
            ';' | '\n' => {
                end_word(&mut word, &mut words);
                end_part(&mut words, &mut parts)?;
            }
            '>' => {
                // `N>` / `>` / `>>` / `&>`: the descriptor digits, if any, are
                // the word so far; the target follows.
                if !word.chars().all(|c| c.is_ascii_digit()) {
                    return None;
                }
                word.clear();
                if chars.peek() == Some(&'>') {
                    chars.next();
                }
                if chars.peek() == Some(&'&') {
                    chars.next();
                    let mut descriptor = false;
                    while chars.peek().is_some_and(char::is_ascii_digit) {
                        chars.next();
                        descriptor = true;
                    }
                    if !descriptor {
                        return None;
                    }
                } else {
                    while chars.peek().is_some_and(|c| *c == ' ' || *c == '\t') {
                        chars.next();
                    }
                    let mut target = String::new();
                    while chars
                        .peek()
                        .is_some_and(|c| !matches!(c, ' ' | '\t' | ';' | '|' | '&' | '\n'))
                    {
                        target.push(chars.next().expect("peeked"));
                    }
                    if target != "/dev/null" {
                        return None;
                    }
                }
            }
            '&' if chars.peek() == Some(&'>') && word.is_empty() => {
                chars.next();
                while chars.peek().is_some_and(|c| *c == ' ' || *c == '\t') {
                    chars.next();
                }
                let mut target = String::new();
                while chars
                    .peek()
                    .is_some_and(|c| !matches!(c, ' ' | '\t' | ';' | '|' | '&' | '\n'))
                {
                    target.push(chars.next().expect("peeked"));
                }
                if target != "/dev/null" {
                    return None;
                }
            }
            '$' | '`' | '\\' | '(' | ')' | '{' | '}' | '<' | '*' | '?' | '[' | ']' | '~' | '#'
            | '!' | '&' => return None,
            c if c.is_control() => return None,
            c => word.push(c),
        }
    }
    end_word(&mut word, &mut words);
    if !words.is_empty() {
        parts.push(words);
    }
    (!parts.is_empty()).then_some(parts)
}

/// The Claude Code allow rules that grant `command` at `scope`, in the order
/// of its parts and without repeats, or `None` when it cannot be granted at
/// that scope.
pub fn bash_grant_rules(command: &str, scope: BashGrantScope) -> Option<Vec<String>> {
    let parts = bash_command_parts(command.trim())?;
    let mut rules: Vec<String> = Vec::new();
    for words in parts {
        let rule = match scope {
            BashGrantScope::Exact => format!("Bash({})", words.join(" ")),
            BashGrantScope::Group => {
                let program = &words[0];
                let name = program.rsplit('/').next().unwrap_or(program);
                if RUNNERS.contains(&name) {
                    return None;
                }
                match words.get(1) {
                    Some(argument) if !argument.starts_with(['\'', '"']) => {
                        format!("Bash({program} {argument} *)")
                    }
                    Some(_) => return None,
                    None => format!("Bash({program})"),
                }
            }
        };
        if !rules.contains(&rule) {
            rules.push(rule);
        }
    }
    Some(rules)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exact(command: &str) -> Option<Vec<String>> {
        bash_grant_rules(command, BashGrantScope::Exact)
    }

    fn group(command: &str) -> Option<Vec<String>> {
        bash_grant_rules(command, BashGrantScope::Group)
    }

    fn rules(rules: &[&str]) -> Option<Vec<String>> {
        Some(rules.iter().map(ToString::to_string).collect())
    }

    #[test]
    fn a_plain_command_is_one_rule() {
        assert_eq!(exact("cargo test"), rules(&["Bash(cargo test)"]));
        assert_eq!(group("cargo test"), rules(&["Bash(cargo test *)"]));
        assert_eq!(group("cargo"), rules(&["Bash(cargo)"]));
    }

    #[test]
    fn a_compound_command_is_a_rule_per_part_without_its_redirections() {
        assert_eq!(
            exact("cargo fmt --all -- --check && echo FMT_OK; cargo clippy --all-targets 2>&1 | tail -15"),
            rules(&[
                "Bash(cargo fmt --all -- --check)",
                "Bash(echo FMT_OK)",
                "Bash(cargo clippy --all-targets)",
                "Bash(tail -15)",
            ])
        );
        assert_eq!(
            group("cargo test --test a 2>&1 | tail -20 || cargo test > /dev/null"),
            rules(&["Bash(cargo test *)", "Bash(tail -20 *)"])
        );
        assert_eq!(exact("make &>/dev/null"), rules(&["Bash(make)"]));
    }

    #[test]
    fn quoted_words_keep_their_spelling() {
        assert_eq!(
            exact(r#"git diff -U0 -- tests/ | grep -E "^\+fn" | grep -v '#\[test\]'"#),
            None,
            "an escape inside double quotes is refused"
        );
        assert_eq!(
            exact(r#"git commit -m "fix: a b" && grep -v '#[test]' x"#),
            rules(&[
                r#"Bash(git commit -m "fix: a b")"#,
                "Bash(grep -v '#[test]' x)"
            ])
        );
        assert_eq!(
            group(r#"echo "a b""#),
            None,
            "a quoted argument names no group"
        );
    }

    #[test]
    fn what_a_rule_cannot_pin_down_is_refused() {
        for command in [
            "echo $(id)",
            "echo `id`",
            "echo $HOME",
            r#"echo "$HOME""#,
            r"echo a\ b",
            "rm -rf *",
            "ls ~",
            "(cd x && make)",
            "{ make; }",
            "make # quiet",
            "make &",
            "make > out.txt",
            "make 2> err.log",
            "sort < input",
            "make |& tee",
            "a && && b",
            "| a",
            "'unterminated",
            "",
        ] {
            assert_eq!(exact(command), None, "{command}");
        }
    }

    #[test]
    fn programs_that_run_programs_have_no_group() {
        assert_eq!(exact("bash -c ls"), rules(&["Bash(bash -c ls)"]));
        assert_eq!(group("bash -c ls"), None);
        assert_eq!(group("cargo test && /usr/bin/env x"), None);
    }
}
