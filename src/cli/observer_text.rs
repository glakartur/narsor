//! What `narsor-cli observer …` and the effect commands of an observer's
//! script print: pure functions of the server's answers, so the text is
//! tested without a server.
//!
//! Everything a script or a source supplied (event data,
//! stderr, error messages) is untrusted: control and invisible format
//! characters are removed before it reaches the terminal.

use chrono::{DateTime, Utc};
use serde_json::Value;

use super::task_text::age;
use crate::controller_protocol::{
    ObserverDetailView, ObserverEffectResponse, ObserverListResponse, ObserverRunErrorView,
    ObserverScriptView, ObserverView, ObserverWatchListResponse, ObserverWatchView,
};
use crate::domain::{without_unsafe_chars, without_unsafe_chars_json};

/// `text` cleaned of what a terminal would act on.
fn clean(text: &str) -> String {
    without_unsafe_chars(text)
}

/// `value` as one line of JSON, every string and key cleaned.
pub fn data_line(value: &Value) -> String {
    let mut value = value.clone();
    without_unsafe_chars_json(&mut value);
    serde_json::to_string(&value).unwrap_or_default()
}

/// `in 4m` for a moment ahead of `now`, `now` for one behind it.
fn until(now: DateTime<Utc>, at: DateTime<Utc>) -> String {
    if at <= now {
        "now".to_string()
    } else {
        format!("in {}", age(at, now))
    }
}

/// The script column of `observer list`: the approved version and a newer
/// draft (AE6).
fn script_versions(observer: &ObserverView) -> String {
    match (observer.approved_version, observer.latest_version) {
        (None, None) => "-".to_string(),
        (None, Some(latest)) => format!("draft v{latest}"),
        (Some(approved), Some(latest)) if latest > approved => {
            format!("v{approved} (draft v{latest})")
        }
        (Some(approved), _) => format!("v{approved}"),
    }
}

/// The warning `observer list` and `observer show` give when a due run
/// waits for a worker longer than one interval (R18).
fn worker_warning(observer: &ObserverView, now: DateTime<Utc>) -> Option<String> {
    if !observer.worker_missing {
        return None;
    }
    let since = observer
        .waiting_for_claim_since
        .map(|since| format!(" for {}", age(now, since)))
        .unwrap_or_default();
    Some(format!(
        "no worker: a due run has waited{since} without a worker claiming it; is \
         `narsor-cli observer worker` running?"
    ))
}

/// `succeeded 5m ago`, `failed 2m ago`.
fn last_run(observer: &ObserverView, now: DateTime<Utc>) -> String {
    match &observer.last_run {
        Some(run) => format!("{} {} ago", clean(&run.status), age(now, run.finished_at)),
        None => "-".to_string(),
    }
}

/// When the next scheduled run is due, as `observer list` shows it.
fn next_run(observer: &ObserverView, now: DateTime<Utc>) -> String {
    if let Some(run) = observer.active_run {
        return format!("running (#{run})");
    }
    match (observer.state.as_str(), observer.next_run_at) {
        ("draft" | "paused" | "needs_attention" | "error", _) | (_, None) => "-".to_string(),
        (_, Some(at)) => until(now, at),
    }
}

/// `observer list`: one line per observer with its state, script
/// versions, last run and next run, then its error and the warning about a
/// missing worker, if any.
pub fn observer_list_output(listing: &ObserverListResponse, now: DateTime<Utc>) -> String {
    if listing.observers.is_empty() {
        return "No observers. Create one with `narsor-cli observer create`.\n".to_string();
    }
    let mut out = format!(
        "{:<24} {:<16} {:<16} {:<22} {}\n",
        "NAME", "STATE", "SCRIPT", "LAST RUN", "NEXT RUN"
    );
    for observer in &listing.observers {
        out.push_str(&format!(
            "{:<24} {:<16} {:<16} {:<22} {}\n",
            clean(&observer.name),
            clean(&observer.state),
            script_versions(observer),
            last_run(observer, now),
            next_run(observer, now)
        ));
        if let Some(error) = observer
            .last_run
            .as_ref()
            .and_then(|run| run.error.as_ref())
        {
            out.push_str(&format!("  error: {}\n", clean(error)));
        }
        if let Some(warning) = worker_warning(observer, now) {
            out.push_str(&format!("  warning: {warning}\n"));
        }
    }
    out
}

/// `observer show`: everything about one observer the owner may need,
/// secret references without values, and the execution errors of its
/// failed or lost runs (a successful run leaves none).
pub fn observer_show_output(detail: &ObserverDetailView, now: DateTime<Utc>) -> String {
    let observer = &detail.observer;
    let mut out = String::new();
    let mut line = |text: String| {
        out.push_str(&text);
        out.push('\n');
    };
    line(format!(
        "Observer: {} ({})",
        clean(&observer.name),
        clean(&observer.observer_id)
    ));
    line(format!("State: {}", clean(&observer.state)));
    line(format!("Source: {}", clean(&observer.source)));
    match &observer.workflow_id {
        Some(workflow) => line(format!(
            "Effect: creates tasks of workflow {} (at most {} per run)",
            clean(workflow),
            observer.task_limit_per_run
        )),
        None => line("Effect: delivers events to the tasks on its watch list".to_string()),
    }
    if observer.watch_list {
        line(format!(
            "Watches: its watch list (objects under {})",
            detail
                .allowed_object_prefixes
                .iter()
                .map(|prefix| clean(prefix))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    } else {
        line("Watches: everything its condition selects".to_string());
    }
    line(format!("Interval: {}s", observer.interval_secs));
    line(format!("Script: {}", script_line(observer)));
    line(format!("Next run: {}", next_run(observer, now)));
    match &observer.last_run {
        Some(run) => {
            let error = run
                .error
                .as_ref()
                .map(|error| format!(": {}", clean(error)))
                .unwrap_or_default();
            line(format!(
                "Last run: #{} {} {} ago{error}",
                run.sequence,
                clean(&run.status),
                age(now, run.finished_at)
            ));
        }
        None => line("Last run: none yet".to_string()),
    }
    if observer.consecutive_failures > 0 {
        line(format!(
            "Failed runs in a row: {}",
            observer.consecutive_failures
        ));
    }
    if let Some(warning) = worker_warning(observer, now) {
        line(format!("Warning: {warning}"));
    }
    if !detail.mapping.is_empty() {
        line(format!(
            "Mapping: {}",
            detail
                .mapping
                .iter()
                .map(|(input, field)| format!("{} <- {}", clean(input), clean(field)))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if !detail.secrets.is_empty() {
        line(format!(
            "Secrets: {}",
            detail
                .secrets
                .iter()
                .map(|secret| format!("{} (file {})", clean(&secret.env), clean(&secret.name)))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    line(format!("Condition: {}", data_line(&detail.condition)));
    if !detail.scripts.is_empty() {
        line("Script versions:".to_string());
        for script in &detail.scripts {
            let mark = if observer.approved_version == Some(script.version) {
                "approved"
            } else {
                "draft"
            };
            line(format!(
                "  v{:<4} {:<9} saved {} ago  {}",
                script.version,
                mark,
                age(now, script.created_at),
                clean(&script.sha256)
            ));
        }
    }
    if !detail.errors.is_empty() {
        line("Errors (newest first):".to_string());
        for error in &detail.errors {
            line(format!("  {}", error_line(error, now)));
            for (stream, text) in [("stdout", &error.stdout), ("stderr", &error.stderr)] {
                if let Some(tail) = text.as_deref().and_then(output_tail) {
                    line(format!("    {stream}:"));
                    for text_line in tail {
                        line(format!("      {text_line}"));
                    }
                }
            }
        }
    }
    out
}

/// The approved version against the newest (AE6): saving a script never
/// approves it, and `show` says which one runs.
fn script_line(observer: &ObserverView) -> String {
    match (observer.approved_version, observer.latest_version) {
        (None, None) => "none yet; save one with `narsor-cli observer script set`".to_string(),
        (None, Some(latest)) => {
            format!("no approved version; draft version {latest} (not approved)")
        }
        (Some(approved), Some(latest)) if latest > approved => {
            format!("approved version {approved}; draft version {latest} (not approved)")
        }
        (Some(approved), _) => format!("approved version {approved}"),
    }
}

/// One execution error of `observer show`.
fn error_line(error: &ObserverRunErrorView, now: DateTime<Utc>) -> String {
    let mut text = format!(
        "#{} run of v{}: {} {} ago",
        error.sequence,
        error.script_version,
        clean(&error.status),
        age(now, error.finished_at)
    );
    if let Some(code) = error.exit_code {
        text.push_str(&format!(", exit {code}"));
    }
    if let Some(object) = &error.watch_object {
        text.push_str(&format!(", {}", clean(object)));
    }
    text.push_str(&format!(": {}", clean(&error.error)));
    text
}

/// Most lines of a failed run's stdout or stderr that `observer show`
/// prints: the end, where a script's error usually is.
const SHOWN_OUTPUT_LINES: usize = 10;

/// The last [`SHOWN_OUTPUT_LINES`] non-empty lines of `text`, cleaned;
/// `None` when it has none.
fn output_tail(text: &str) -> Option<Vec<String>> {
    let lines: Vec<&str> = text
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();
    if lines.is_empty() {
        return None;
    }
    let skip = lines.len().saturating_sub(SHOWN_OUTPUT_LINES);
    Some(lines[skip..].iter().map(|line| clean(line)).collect())
}

/// The answer to `observer script set`.
pub fn script_saved_output(name: &str, script: &ObserverScriptView) -> String {
    format!(
        "Saved version {} of '{}' ({}). It is not approved: test it by hand, then \
         `narsor-cli observer approve {} --version {}`.\n",
        script.version,
        clean(name),
        clean(&script.sha256),
        clean(name),
        script.version
    )
}

/// One line about an observer after a command changed it.
pub fn observer_changed_output(what: &str, observer: &ObserverView) -> String {
    format!(
        "{what} observer '{}': state {}, {}.\n",
        clean(&observer.name),
        clean(&observer.state),
        script_line(observer)
    )
}

/// `observer watch list`.
pub fn watch_list_output(listing: &ObserverWatchListResponse, now: DateTime<Utc>) -> String {
    if listing.watches.is_empty() {
        return "No watch entries.\n".to_string();
    }
    let mut out = format!(
        "{:<16} {:<28} {:<20} {:<9} {}\n",
        "ENTRY", "TASK", "EVENTS", "ORIGIN", "OBJECT"
    );
    for entry in &listing.watches {
        out.push_str(&format!(
            "{:<16} {:<28} {:<20} {:<9} {}  (added {} ago)\n",
            short_entry(&entry.entry_id),
            clean(&entry.task_id),
            event_types(entry),
            clean(&entry.origin),
            clean(&entry.object),
            age(now, entry.created_at)
        ));
    }
    out
}

/// The answer to `observer watch add`.
pub fn watch_added_output(entry: &ObserverWatchView) -> String {
    format!(
        "Watching {} for task {} ({}); entry {}.\n",
        clean(&entry.object),
        clean(&entry.task_id),
        event_types(entry),
        clean(&entry.entry_id)
    )
}

/// The answer to `observer watch remove`.
pub fn watch_removed_output(entry: &ObserverWatchView) -> String {
    format!(
        "Removed watch entry {}: {} for task {}.\n",
        clean(&entry.entry_id),
        clean(&entry.object),
        clean(&entry.task_id)
    )
}

fn event_types(entry: &ObserverWatchView) -> String {
    entry
        .event_types
        .iter()
        .map(|event| clean(event))
        .collect::<Vec<_>>()
        .join(",")
}

/// The first characters of an entry id: enough to tell entries apart in a
/// listing. `watch remove` takes the full id or this prefix.
fn short_entry(entry_id: &str) -> String {
    clean(entry_id).chars().take(12).collect()
}

/// What `task create --data` and `task event` print for the script's log:
/// the server's line, cleaned.
pub fn effect_output(response: &ObserverEffectResponse) -> String {
    format!("{}\n", clean(&response.message))
}
