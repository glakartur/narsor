//! What `narsor-cli observer …` and the effect commands of an observer's
//! script print: pure functions of the server's answers, so the text is
//! tested without a server.
//!
//! Everything a script or a source supplied (effect keys, event data,
//! stderr, error messages) is untrusted: control and invisible format
//! characters are removed before it reaches the terminal.

use chrono::{DateTime, Utc};
use serde_json::Value;

use super::task_text::age;
use crate::controller_protocol::{
    ObserverDetailView, ObserverDryRunView, ObserverEffectOutcome, ObserverEffectResponse,
    ObserverListResponse, ObserverRunView, ObserverScriptView, ObserverView,
    ObserverWatchListResponse, ObserverWatchView,
};
use crate::domain::{without_unsafe_chars, without_unsafe_chars_json};

/// The run status of a run that has not ended.
pub const RUNNING: &str = "running";

/// The run status of a run that ended well.
pub const SUCCEEDED: &str = "succeeded";

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
/// secret references without values, and its latest runs.
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
    if let Some(pending) = &observer.pending_dry_run {
        line(format!(
            "Pending dry run: version {}{}, asked {} ago",
            pending.version,
            secrets_note(pending.with_secrets),
            age(now, pending.requested_at)
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
    if !detail.runs.is_empty() {
        line("Runs (newest first):".to_string());
        for run in &detail.runs {
            line(format!("  {}", run_line(run, now)));
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

fn secrets_note(with_secrets: bool) -> &'static str {
    if with_secrets {
        " with secrets"
    } else {
        ""
    }
}

/// One run of `observer show`.
fn run_line(run: &ObserverRunView, now: DateTime<Utc>) -> String {
    let kind = if run.dry_run { "dry run" } else { "run" };
    let when = run.finished_at.unwrap_or(run.claimed_at);
    let mut text = format!(
        "#{} {kind} of v{}: {} {} ago",
        run.sequence,
        run.script_version,
        clean(&run.status),
        age(now, when)
    );
    if let Some(code) = run.exit_code {
        text.push_str(&format!(", exit {code}"));
    }
    if run.tasks_created > 0 {
        text.push_str(&format!(", {} task(s) created", run.tasks_created));
    }
    if run.dry_run && !run.planned_effects.is_empty() {
        text.push_str(&format!(
            ", {} effect(s) planned",
            run.planned_effects.len()
        ));
    }
    if let Some(error) = &run.error {
        text.push_str(&format!(": {}", clean(error)));
    }
    text
}

/// The answer to a finished dry run: what it would have done, or why it
/// failed with the script's stderr.
pub fn dry_run_output(run: &ObserverRunView) -> String {
    let mut out = format!(
        "Dry run of version {} (run {}): {}",
        run.script_version,
        clean(&run.run_id),
        clean(&run.status)
    );
    if let Some(code) = run.exit_code {
        out.push_str(&format!(", exit {code}"));
    }
    if let Some(error) = &run.error {
        out.push_str(&format!(": {}", clean(error)));
    }
    out.push('\n');
    if run.planned_effects.is_empty() {
        out.push_str("It would have done nothing in Narsor.\n");
    } else {
        out.push_str("Nothing was created. It would have:\n");
        for effect in &run.planned_effects {
            let task = effect
                .task_id
                .as_deref()
                .map(clean)
                .unwrap_or_else(|| "-".to_string());
            let what = match effect.kind.as_str() {
                "create_task" => format!("created task {task}"),
                "deliver_event" => format!("delivered an event to task {task}"),
                other => format!("{} for task {task}", clean(other)),
            };
            out.push_str(&format!(
                "  {what} (key {}) {}\n",
                clean(&effect.effect_key),
                data_line(&effect.data)
            ));
        }
    }
    if run.status != SUCCEEDED {
        if let Some(stderr) = run.stderr.as_deref().filter(|text| !text.is_empty()) {
            out.push_str("stderr:\n");
            for text in stderr.lines() {
                out.push_str(&format!("  {}\n", clean(text)));
            }
        }
    }
    out
}

/// The answer to `observer dry-run` once the request is stored.
pub fn dry_run_requested_output(name: &str, requested: &ObserverDryRunView) -> String {
    format!(
        "Asked for a dry run of version {} of '{}'{}; waiting for a worker to run it.\n",
        requested.version,
        clean(name),
        secrets_note(requested.with_secrets)
    )
}

/// The refusal when no worker claimed a dry run in time.
pub fn no_worker_output(name: &str, waited_secs: u64) -> String {
    format!(
        "No worker: no worker claimed the dry run of '{}' within {waited_secs}s. Is \
         `narsor-cli observer worker` running with a credential of yours? The dry run stays \
         asked for; `narsor-cli observer show {}` shows its result once a worker runs it.",
        clean(name),
        clean(name)
    )
}

/// The answer to `observer script set`.
pub fn script_saved_output(name: &str, script: &ObserverScriptView) -> String {
    format!(
        "Saved version {} of '{}' ({}). It is not approved: try it with `narsor-cli observer \
         dry-run {} --version {}`, then `narsor-cli observer approve {} --version {}`.\n",
        script.version,
        clean(name),
        clean(&script.sha256),
        clean(name),
        script.version,
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

/// What `task create --effect-key` and `task event` print for the script's
/// log: the server's line, cleaned, with the key.
pub fn effect_output(response: &ObserverEffectResponse) -> String {
    let message = clean(&response.message);
    match response.outcome {
        ObserverEffectOutcome::Duplicate => {
            format!("{message} (effect key {})\n", clean(&response.effect_key))
        }
        _ => format!("{message}\n"),
    }
}
