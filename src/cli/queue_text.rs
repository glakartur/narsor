//! What `narsor-cli queue …` and the effect commands of an automation's
//! script print: pure functions of the server's answers, so the text is
//! tested without a server.
//!
//! Everything a script, a source or a catalogue file supplied (error
//! messages, stderr, params) is untrusted: control and invisible format
//! characters are removed before it reaches the terminal.

use chrono::{DateTime, Utc};

use super::task_text::age;
use crate::controller_protocol::{
    CatalogProblemView, ObserverEffectResponse, ObserverQueueListResponse, ObserverQueueView,
    ObserverRunErrorView, QueueAutomationView, QueueEntryView,
};
use crate::domain::without_unsafe_chars;

/// `text` cleaned of what a terminal would act on.
fn clean(text: &str) -> String {
    without_unsafe_chars(text)
}

/// `in 4m` for a moment ahead of `now`, `now` for one behind it.
fn until(now: DateTime<Utc>, at: DateTime<Utc>) -> String {
    if at <= now {
        "now".to_string()
    } else {
        format!("in {}", age(at, now))
    }
}

/// `every 300s` or `cron '*/10 * * * *' (UTC)`.
fn schedule_text(interval_secs: Option<u64>, cron: Option<&str>) -> String {
    match (cron, interval_secs) {
        (Some(cron), _) => format!("cron '{}' (UTC)", clean(cron)),
        (None, Some(secs)) => format!("every {secs}s"),
        (None, None) => "-".to_string(),
    }
}

/// A run mode with what it means.
fn mode_text(mode: &str) -> String {
    match mode {
        "on_episodes" => "on_episodes (runs for the entries of the queue only)".to_string(),
        "always" => "always (runs on every tick, with no entry)".to_string(),
        other => clean(other),
    }
}

/// `file: reason`, cleaned.
fn problem_line(problem: &CatalogProblemView) -> String {
    format!("{}: {}", clean(&problem.file), clean(&problem.reason))
}

/// `queue list`: one line per queue with its mode, schedule, automations
/// and entry count; a queue that does not run says why. Then the catalogue
/// files that failed to load.
pub fn queue_list_output(listing: &ObserverQueueListResponse) -> String {
    let mut out = String::new();
    if listing.queues.is_empty() {
        out.push_str(
            "No queues. A queue is a file of the catalogue (queues/NAME.yaml); a gate with a \
             `watch` block adds its wait episodes to the queue it names.\n",
        );
    } else {
        out.push_str(&format!(
            "{:<24} {:<12} {:<28} {:>7}  {}\n",
            "QUEUE", "MODE", "SCHEDULE", "ENTRIES", "AUTOMATIONS"
        ));
        for queue in &listing.queues {
            let automations = match &queue.problem {
                Some(problem) => format!("! {}", clean(problem)),
                None => queue
                    .automations
                    .iter()
                    .map(|automation| clean(automation))
                    .collect::<Vec<_>>()
                    .join(", "),
            };
            out.push_str(&format!(
                "{:<24} {:<12} {:<28} {:>7}  {}\n",
                clean(&queue.queue),
                queue
                    .mode
                    .as_deref()
                    .map(clean)
                    .unwrap_or_else(|| "-".into()),
                schedule_text(queue.interval_secs, queue.cron.as_deref()),
                queue.entries,
                automations
            ));
        }
    }
    if !listing.problems.is_empty() {
        out.push_str("Catalogue files that cannot be used:\n");
        for problem in &listing.problems {
            out.push_str(&format!("  {}\n", problem_line(problem)));
        }
    }
    out
}

/// `queue show`: the queue's schedule, its automations with their scripts,
/// state, runs and errors, then its entries with how each automation
/// processed them (R23), and the problems of its file.
pub fn queue_show_output(view: &ObserverQueueView, now: DateTime<Utc>) -> String {
    let mut out = String::new();
    let mut line = |text: String| {
        out.push_str(&text);
        out.push('\n');
    };
    line(format!("Queue: {}", clean(&view.queue)));
    match &view.mode {
        Some(mode) => line(format!("Mode: {}", mode_text(mode))),
        None => line("Mode: - (the queue does not run)".to_string()),
    }
    line(format!(
        "Schedule: {}",
        schedule_text(view.interval_secs, view.cron.as_deref())
    ));
    if let Some(workflow) = &view.workflow_id {
        line(format!("Creates tasks of: {}", clean(workflow)));
    }
    for problem in &view.problems {
        line(format!("Problem: {}", problem_line(problem)));
    }
    if !view.automations.is_empty() {
        line("Automations:".to_string());
        for automation in &view.automations {
            for text in automation_lines(automation, now) {
                line(text);
            }
        }
    }
    if view.entries.is_empty() {
        line("Entries: none".to_string());
        return out;
    }
    line("Entries (oldest first):".to_string());
    line(format!(
        "  {:<24} {:<20} {:<20} {}",
        "TASK", "STEP", "EPISODE", "WAITING"
    ));
    for entry in &view.entries {
        for text in entry_lines(entry, now) {
            line(text);
        }
    }
    out
}

/// One automation of `queue show`: its scripts, state, runs and newest
/// errors.
fn automation_lines(automation: &QueueAutomationView, now: DateTime<Utc>) -> Vec<String> {
    let mut lines = vec![format!(
        "  {} ({}): {}",
        clean(&automation.automation),
        automation
            .scripts
            .iter()
            .map(|script| clean(script))
            .collect::<Vec<_>>()
            .join(", "),
        clean(&automation.state)
    )];
    if !automation.description.is_empty() {
        lines.push(format!("    {}", clean(&automation.description)));
    }
    if matches!(automation.state.as_str(), "needs_attention" | "error") {
        lines.push(
            "    stopped: it runs again when its check script changes in the catalogue".to_string(),
        );
    }
    let mut runs = Vec::new();
    if let Some(sequence) = automation.active_run {
        runs.push(format!("run #{sequence} in progress"));
    }
    if let Some(run) = &automation.last_run {
        runs.push(format!(
            "last run {} {} ago",
            clean(&run.status),
            age(now, run.finished_at)
        ));
    }
    if let Some(at) = automation.next_run_at {
        runs.push(format!("next {}", until(now, at)));
    }
    if automation.consecutive_failures > 0 {
        runs.push(format!(
            "{} failed in a row (backoff)",
            automation.consecutive_failures
        ));
    }
    if !runs.is_empty() {
        lines.push(format!("    {}", runs.join("; ")));
    }
    if automation.worker_missing {
        lines.push(
            "    warning: a due run waits for a worker; is the observer worker running?"
                .to_string(),
        );
    }
    for error in automation.errors.iter().take(SHOWN_ERRORS) {
        lines.push(format!("    error {}", error_line(error, now)));
    }
    lines
}

/// Most errors of one automation `queue show` prints, newest first.
const SHOWN_ERRORS: usize = 3;

/// One execution error of `queue show`.
fn error_line(error: &ObserverRunErrorView, now: DateTime<Utc>) -> String {
    let mut text = format!(
        "#{} {}: {} {} ago",
        error.sequence,
        clean(&error.phase),
        clean(&error.status),
        age(now, error.finished_at)
    );
    if let Some(code) = error.exit_code {
        text.push_str(&format!(", exit {code}"));
    }
    if let Some(entry) = &error.entry_id {
        text.push_str(&format!(", entry {}", short_entry(entry)));
    }
    text.push_str(&format!(": {}", clean(&error.error)));
    text
}

/// One entry of `queue show`: its row, its params and one line per
/// automation that processed it.
fn entry_lines(entry: &QueueEntryView, now: DateTime<Utc>) -> Vec<String> {
    let mut lines = vec![format!(
        "  {:<24} {:<20} {:<20} {}",
        clean(&entry.task_id),
        clean(&entry.step),
        clean(&entry.episode),
        age(now, entry.started_at)
    )];
    if !entry.params.is_empty() {
        lines.push(format!(
            "    params: {}",
            entry
                .params
                .iter()
                .map(|(name, value)| format!("{}={}", clean(name), clean(value)))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    for processed in &entry.automations {
        let mut text = format!("    {}: ", clean(&processed.automation));
        text.push_str(&match (processed.running, processed.last_run_at) {
            (true, _) => "running".to_string(),
            (false, Some(at)) => format!("last run {} ago", age(now, at)),
            (false, None) => "no run yet".to_string(),
        });
        if let Some(error) = &processed.last_error {
            text.push_str(&format!(
                "; last error: {} {} ago",
                clean(&error.status),
                age(now, error.finished_at)
            ));
            if let Some(code) = error.exit_code {
                text.push_str(&format!(", exit {code}"));
            }
            text.push_str(&format!(": {}", clean(&error.error)));
        }
        lines.push(text);
    }
    lines
}

/// The first characters of a queue entry id: enough to tell entries apart
/// in a listing.
fn short_entry(entry_id: &str) -> String {
    clean(entry_id).chars().take(12).collect()
}

/// What `task create --data` and `task decide` print for the script's log:
/// the server's line, cleaned.
pub fn effect_output(response: &ObserverEffectResponse) -> String {
    format!("{}\n", clean(&response.message))
}
