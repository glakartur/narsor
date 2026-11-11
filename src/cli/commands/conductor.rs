//! `conductor chat|history|confirm|reject|undo|status`: talking to the conductor of
//! an execution and settling what it proposes.

use std::path::Path;
use std::time::Duration;

use serde_json::Value;

use crate::api::{ApiClient, ChatAuthorDto, ChatEntryDto, ChatEntryKindDto, ClientError};
use crate::config::Flags;

use super::daemon_access::{client_error, daemon_client};
use super::{CliError, Result};

/// How long `conductor chat` waits for the conductor's answer.
const CHAT_REPLY_TIMEOUT: Duration = Duration::from_secs(600);

/// How often it looks for the answer meanwhile.
const CHAT_POLL_INTERVAL: Duration = Duration::from_millis(250);

/// `narsor conductor chat <id> "<message>"` — writes to the conductor of an
/// execution (F4) and, unless told not to wait, prints its answer and the
/// actions it proposes. The message goes into the same history the TUI and
/// the API read.
pub async fn conductor_chat(
    state: &Path,
    flags: &Flags,
    execution_id: &str,
    message: &str,
    wait: bool,
) -> Result<()> {
    let message = message.trim();
    if message.is_empty() {
        return Err(CliError::usage("A chat message cannot be empty."));
    }
    let client = daemon_client(state, flags).await?;
    let posted = client
        .post_chat(execution_id, message)
        .await
        .map_err(|error| client_error(execution_id, error))?;
    print_chat_entry(&posted);
    if !wait {
        return Ok(());
    }

    let deadline = tokio::time::Instant::now() + CHAT_REPLY_TIMEOUT;
    let mut after = posted.sequence;
    let mut answered = false;
    loop {
        let entries = client
            .conductor_chat(execution_id, after)
            .await
            .map_err(|error| client_error(execution_id, error))?
            .entries;
        for entry in &entries {
            print_chat_entry(entry);
            after = entry.sequence;
            answered |= entry.in_reply_to.as_deref() == Some(posted.id.as_str());
        }
        // The answer and its proposals are written together, so once the
        // answer is there, so is everything that goes with it.
        if answered {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(CliError::failure(format!(
                "The conductor has not answered yet; see its answer later with \
                 'narsor conductor history {execution_id}'."
            )));
        }
        tokio::time::sleep(CHAT_POLL_INTERVAL).await;
    }
}

/// `narsor conductor history <id>` — the whole chat of an execution.
pub async fn conductor_history(state: &Path, flags: &Flags, execution_id: &str) -> Result<()> {
    let client = daemon_client(state, flags).await?;
    let entries = client
        .conductor_chat(execution_id, 0)
        .await
        .map_err(|error| client_error(execution_id, error))?
        .entries;
    if entries.is_empty() {
        println!("The conductor's chat of execution {execution_id} is empty.");
    }
    for entry in &entries {
        print_chat_entry(entry);
    }
    Ok(())
}

/// `narsor conductor confirm|reject <id> <proposal>` — settles a proposal of
/// the conductor's (R28). `<proposal>` is its id or its number in the chat.
pub async fn conductor_settle(
    state: &Path,
    flags: &Flags,
    execution_id: &str,
    proposal: &str,
    confirm: bool,
) -> Result<()> {
    let client = daemon_client(state, flags).await?;
    let proposal_id = chat_entry_id(&client, execution_id, proposal, ChatEntryKindDto::Proposal)
        .await?
        .unwrap_or_else(|| proposal.to_string());
    let outcome = if confirm {
        client.confirm_proposal(execution_id, &proposal_id).await
    } else {
        client.reject_proposal(execution_id, &proposal_id).await
    }
    .map_err(|error| match error {
        ClientError::NotFound => CliError::failure(format!(
            "Execution '{execution_id}' has no proposal '{proposal}'."
        )),
        other => client_error(execution_id, other),
    })?;

    println!("{}", outcome.message);
    if outcome.repeated {
        println!("(It had already been settled; nothing was done now.)");
    }
    Ok(())
}

/// `narsor conductor undo <id> <decision>` — undoes the conductor's choice
/// of a step's output (KTD13). `<decision>` is its id or the number of its
/// entry in the chat.
pub async fn conductor_undo(
    state: &Path,
    flags: &Flags,
    execution_id: &str,
    decision: &str,
) -> Result<()> {
    let client = daemon_client(state, flags).await?;
    let decision_id = match decision.parse::<u64>() {
        Ok(sequence) => {
            let entries = client
                .conductor_chat(execution_id, sequence.saturating_sub(1))
                .await
                .map_err(|error| client_error(execution_id, error))?
                .entries;
            entries
                .iter()
                .find(|entry| {
                    entry.sequence == sequence && entry.kind == ChatEntryKindDto::Decision
                })
                .and_then(|entry| entry.payload.as_ref()?.get("decision_id")?.as_str())
                .map(ToString::to_string)
                .unwrap_or_else(|| decision.to_string())
        }
        Err(_) => decision.to_string(),
    };
    let outcome = client
        .undo_decision(execution_id, &decision_id)
        .await
        .map_err(|error| match error {
            ClientError::NotFound => CliError::failure(format!(
                "Execution '{execution_id}' has no decision '{decision}' of the conductor's."
            )),
            other => client_error(execution_id, other),
        })?;
    println!("{}", outcome.message);
    if let Some(warning) = &outcome.warning {
        eprintln!("Warning: {warning}");
    }
    Ok(())
}

/// `narsor conductor status <id>` — what the conductor of an execution is
/// doing and what it kept (`GET /executions/:id/conductor`), as the TUI's
/// conductor pane reads it.
pub async fn conductor_status(state: &Path, flags: &Flags, execution_id: &str) -> Result<()> {
    let client = daemon_client(state, flags).await?;
    let conductor = client
        .conductor_state(execution_id)
        .await
        .map_err(|error| client_error(execution_id, error))?;

    match &conductor.pending {
        Some(pending) => println!(
            "Working on:    {} ({}, since {}){}",
            pending.request.purpose.name(),
            pending.request.trigger.name(),
            pending.started_at.to_rfc3339(),
            episode_suffix(pending.request.episode.as_deref())
        ),
        None => println!("Working on:    nothing"),
    }
    println!("Queued:        {}", conductor.queue.len());
    for queued in &conductor.queue {
        println!(
            "  {} ({}){}",
            queued.purpose.name(),
            queued.trigger.name(),
            episode_suffix(queued.episode.as_deref())
        );
    }
    println!("Unanswered:    {}", conductor.unanswered_chat.len());
    for (episode, attempts) in &conductor.invocation_attempts {
        println!("Attempts:      {attempts} for {episode}");
    }
    for episode in &conductor.used_retries {
        println!("Retry used:    {episode}");
    }
    if !conductor.unapplied_decisions.is_empty() {
        println!(
            "Unapplied:     {} decision(s)",
            conductor.unapplied_decisions.len()
        );
    }
    if let Some(failure) = &conductor.failure {
        println!(
            "Last failure:  {} at '{}': {}",
            failure.key, failure.state_id, failure.reason
        );
    }
    Ok(())
}

/// ` for <episode>`, or nothing.
fn episode_suffix(episode: Option<&str>) -> String {
    episode
        .map(|episode| format!(" for {episode}"))
        .unwrap_or_default()
}

/// The id of the `kind` entry numbered `reference` in the chat, when
/// `reference` is a number; `None` when it is not (it is then an id itself).
async fn chat_entry_id(
    client: &ApiClient,
    execution_id: &str,
    reference: &str,
    kind: ChatEntryKindDto,
) -> Result<Option<String>> {
    let Ok(sequence) = reference.parse::<u64>() else {
        return Ok(None);
    };
    let entries = client
        .conductor_chat(execution_id, sequence.saturating_sub(1))
        .await
        .map_err(|error| client_error(execution_id, error))?
        .entries;
    Ok(entries
        .into_iter()
        .find(|entry| entry.sequence == sequence && entry.kind == kind)
        .map(|entry| entry.id))
}

/// One chat entry: its number, who wrote it and what it is, its text, and
/// — for a proposal or a decision — the id to act on it by.
fn print_chat_entry(entry: &ChatEntryDto) {
    let who = match entry.author {
        ChatAuthorDto::Operator => "operator",
        ChatAuthorDto::Conductor => "conductor",
    };
    let what = match entry.kind {
        ChatEntryKindDto::Message => "",
        ChatEntryKindDto::Error => " (error)",
        ChatEntryKindDto::Decision => " (decision)",
        ChatEntryKindDto::Proposal => " (proposal)",
    };
    println!("#{:<4} {who}{what}: {}", entry.sequence, entry.text);
    match entry.kind {
        ChatEntryKindDto::Proposal => println!("      proposal id: {}", entry.id),
        ChatEntryKindDto::Decision => {
            if let Some(id) = entry
                .payload
                .as_ref()
                .and_then(|payload| payload.get("decision_id"))
                .and_then(Value::as_str)
            {
                println!("      decision id: {id}");
            }
        }
        ChatEntryKindDto::Message | ChatEntryKindDto::Error => {}
    }
}
