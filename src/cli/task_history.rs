//! `narsor task history …`: the executions from before the task scheduler,
//! read and recovered through `/control/v1/executions` (the TUI's *Historia*,
//! `H`).
//!
//! Only the trusted local daemon holds such executions; any other server
//! answers with an explanation drawn from its capabilities.

use clap::Subcommand;

use crate::api::{
    ApiClient, ChatAuthorDto, ChatEntryKindDto, ConversationResponse as PageResponse,
};
use crate::domain::{without_unsafe_chars, without_unsafe_chars_multiline};

use super::commands::{print_event, print_position, CliError, Result};
use super::control_text::{history_detail_lines, history_header, history_row};
use super::remote::{failure, print_conversation_entries, require_local_daemon};

/// `narsor task history …`.
#[derive(Debug, Subcommand)]
pub enum HistoryCommand {
    /// Lists the executions from before the scheduler, newest first.
    List,
    /// Shows one old execution: workflow, status, problem and whether it can
    /// be recovered.
    Show {
        /// The execution id.
        #[arg(value_name = "EXECUTION_ID")]
        execution_id: String,
    },
    /// Prints the execution's event log.
    Events {
        /// The execution id.
        #[arg(value_name = "EXECUTION_ID")]
        execution_id: String,
        /// Only the events after this sequence number.
        #[arg(long, default_value_t = 0)]
        after: u64,
    },
    /// Prints the execution's step timeline, one entry per attempt.
    Steps {
        /// The execution id.
        #[arg(value_name = "EXECUTION_ID")]
        execution_id: String,
    },
    /// Prints one attempt's recorded conversation.
    Conversation {
        /// The execution id.
        #[arg(value_name = "EXECUTION_ID")]
        execution_id: String,
        /// The attempt (see `steps`).
        #[arg(value_name = "ATTEMPT_ID")]
        attempt_id: String,
        /// First entry to read.
        #[arg(long, default_value_t = 0)]
        from: u64,
    },
    /// Prints what the execution's conductor kept: its state and its chat.
    Conductor {
        /// The execution id.
        #[arg(value_name = "EXECUTION_ID")]
        execution_id: String,
        /// Only the chat entries after this sequence number.
        #[arg(long, default_value_t = 0)]
        after: u64,
    },
    /// Starts a failed old execution again at the step it failed on.
    Recover {
        /// The execution id.
        #[arg(value_name = "EXECUTION_ID")]
        execution_id: String,
        /// A note shown to the agent in the new attempt.
        #[arg(long, value_name = "TEXT")]
        comment: Option<String>,
    },
}

/// Runs one `narsor task history` command.
pub async fn history(client: &ApiClient, command: HistoryCommand) -> Result<()> {
    require_local_daemon(client, "Executions from before the task scheduler").await?;
    let unreadable = |what: &'static str, id: String| {
        move |error| failure(&format!("{what} of execution '{id}' cannot be read"), error)
    };
    match command {
        HistoryCommand::List => {
            let listing = client
                .control_history()
                .await
                .map_err(|error| failure("Could not list the old executions", error))?;
            if listing.executions.is_empty() {
                println!("No executions from before the scheduler.");
                return Ok(());
            }
            println!("{}", history_header());
            for execution in &listing.executions {
                println!("{}", history_row(execution));
            }
            Ok(())
        }
        HistoryCommand::Show { execution_id } => {
            let view = client
                .control_history_execution(&execution_id)
                .await
                .map_err(unreadable("The record", execution_id))?;
            for line in history_detail_lines(&view) {
                println!("{}", line.text);
            }
            Ok(())
        }
        HistoryCommand::Events {
            execution_id,
            after,
        } => {
            let events = client
                .control_history_events(&execution_id, after)
                .await
                .map_err(unreadable("The events", execution_id))?;
            for event in &events {
                print_event(event);
            }
            Ok(())
        }
        HistoryCommand::Steps { execution_id } => {
            let timeline = client
                .control_history_timeline(&execution_id)
                .await
                .map_err(unreadable("The steps", execution_id))?;
            if timeline.positions.is_empty() {
                println!("No steps.");
            }
            for position in &timeline.positions {
                print_position(position);
            }
            Ok(())
        }
        HistoryCommand::Conversation {
            execution_id,
            attempt_id,
            from,
        } => {
            let page = client
                .control_history_conversation(&execution_id, &attempt_id, from)
                .await
                .map_err(unreadable("The conversation", execution_id.clone()))?;
            match page {
                PageResponse::Recorded {
                    entries,
                    next_entry,
                    closed,
                    ..
                } => {
                    print_conversation_entries(&entries, next_entry, closed);
                    Ok(())
                }
                PageResponse::NoRecord => Err(CliError::failure(format!(
                    "No conversation record for attempt '{attempt_id}' of execution \
                     '{execution_id}'."
                ))),
            }
        }
        HistoryCommand::Conductor {
            execution_id,
            after,
        } => conductor(client, &execution_id, after).await,
        HistoryCommand::Recover {
            execution_id,
            comment,
        } => {
            let accepted = client
                .control_recover_execution(&execution_id, comment)
                .await
                .map_err(|error| failure("The execution was not recovered", error))?;
            println!(
                "Recovery of execution {} accepted; it continues in the background \
                 (see `narsor task history show {}`).",
                without_unsafe_chars(&accepted.execution_id),
                without_unsafe_chars(&accepted.execution_id)
            );
            Ok(())
        }
    }
}

/// What the conductor of an old execution works on and kept, then its chat.
async fn conductor(client: &ApiClient, execution_id: &str, after: u64) -> Result<()> {
    let state = client
        .control_history_conductor(execution_id)
        .await
        .map_err(|error| failure("The conductor's state cannot be read", error))?;
    match &state.pending {
        Some(pending) => println!(
            "Working on:    {} ({}, since {})",
            pending.request.purpose.name(),
            pending.request.trigger.name(),
            pending.started_at.to_rfc3339()
        ),
        None => println!("Working on:    nothing"),
    }
    println!("Queued:        {}", state.queue.len());
    println!("Unanswered:    {}", state.unanswered_chat.len());
    if let Some(failure) = &state.failure {
        println!(
            "Last failure:  {} at '{}': {}",
            without_unsafe_chars(&failure.key),
            without_unsafe_chars(&failure.state_id),
            without_unsafe_chars(&failure.reason)
        );
    }
    let chat = client
        .control_history_conductor_chat(execution_id, after)
        .await
        .map_err(|error| failure("The conductor's chat cannot be read", error))?;
    for entry in &chat.entries {
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
        println!(
            "#{:<4} {}  {who}{what}: {}",
            entry.sequence,
            entry.at.to_rfc3339(),
            without_unsafe_chars_multiline(&entry.text)
        );
    }
    Ok(())
}
