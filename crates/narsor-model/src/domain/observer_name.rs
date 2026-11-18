//! The naming rules of observers and their queues, shared by the observer
//! store and the workflow validator (a `decision` step's `watch.queue`).

/// Longest observer name.
pub const MAX_OBSERVER_NAME_LEN: usize = 64;

/// Prefix of every generated observer id.
pub const OBSERVER_ID_PREFIX: &str = "obs-";

/// Why `value` is not a valid name-like value, if it is not:
/// 1–[`MAX_OBSERVER_NAME_LEN`] characters of `[A-Za-z0-9_-]`, plus `.` when
/// `allow_dot`, never starting with `.`.
pub fn name_like_problem(value: &str, allow_dot: bool) -> Option<String> {
    let valid = !value.is_empty()
        && value.len() <= MAX_OBSERVER_NAME_LEN
        && !value.starts_with('.')
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(byte, b'-' | b'_')
                || (allow_dot && byte == b'.')
        });
    if valid {
        return None;
    }
    let allowed = if allow_dot {
        "[A-Za-z0-9._-]"
    } else {
        "[A-Za-z0-9_-]"
    };
    Some(format!(
        "'{value}' is not valid: use 1-{MAX_OBSERVER_NAME_LEN} characters of {allowed}, not starting with '.'"
    ))
}

/// Why `value` is not a valid observer name, if it is not. A name that looks
/// like an id could shadow another owner's observer where a name or an id is
/// accepted, so the id prefix is refused too.
pub fn observer_name_problem(value: &str) -> Option<String> {
    name_like_problem(value, true).or_else(|| {
        value.starts_with(OBSERVER_ID_PREFIX).then(|| {
            format!(
                "'{value}' is not valid: a name may not start with '{OBSERVER_ID_PREFIX}', the prefix of observer ids"
            )
        })
    })
}

/// Why `name` is not a valid param name of a gate's `watch.params` (and of
/// an automation's `params`), if it is not: not blank, no whitespace.
pub fn param_name_problem(name: &str) -> Option<String> {
    (name.trim().is_empty() || name.contains(char::is_whitespace))
        .then(|| format!("param name '{name}' is blank or contains whitespace"))
}

/// Why `value` is not a valid queue name, if it is not: the characters of
/// an observer name, so an observer's own name is always a valid queue.
pub fn queue_name_problem(value: &str) -> Option<String> {
    name_like_problem(value, true)
}
