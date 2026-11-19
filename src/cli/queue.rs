//! `narsor-cli queue …`: the read-only queue views.
//!
//! | command | what |
//! | --- | --- |
//! | `queue list [--json]` | the catalogue's queues with mode, schedule, automations and entry counts, and the catalogue files that cannot be used |
//! | `queue show NAME [--json]` | one queue: its automations (scripts, state, runs, errors) and its entries |
//!
//! Queues and automations are catalogue files: no command creates, changes
//! or deletes them (R21, R22). The commands go through [`QueueApi`],
//! implemented on the API client, and print through the pure functions of
//! [`super::queue_text`]: both are what the tests drive.

use std::path::Path;

use chrono::{DateTime, Utc};
use clap::Subcommand;
use futures::future::BoxFuture;

use super::commands::{CliError, Result};
use super::queue_text::{queue_list_output, queue_show_output};
use super::remote::failure;
use super::task_text::safe_json;
use crate::api::{ApiClient, ClientError, ServerTarget};
use crate::config::Flags;
use crate::controller_protocol::{ObserverQueueListResponse, ObserverQueueView};
use crate::domain::without_unsafe_chars;

/// What `narsor-cli queue` can be asked to do.
#[derive(Debug, Subcommand)]
pub enum QueueCommand {
    /// Lists the catalogue's queues with their mode, schedule, automations
    /// and how many gate episodes wait on them, and the catalogue files
    /// that cannot be used.
    List {
        /// Print the server's answer as JSON (the queues and the problems).
        #[arg(long)]
        json: bool,
    },

    /// Shows one queue: its automations with their scripts, state, runs
    /// and errors, and the gate episodes that wait on it.
    Show {
        /// The queue's name.
        #[arg(value_name = "QUEUE")]
        queue: String,
        /// Print the queue view as JSON instead.
        #[arg(long)]
        json: bool,
    },
}

/// Runs one `narsor-cli queue` command.
pub async fn queue(
    command: QueueCommand,
    target: &ServerTarget,
    state: &Path,
    flags: &Flags,
) -> Result<()> {
    let client = crate::api::connect(target, state, flags)
        .await
        .map_err(CliError::failure)?;
    let output = operate(&client, command, Utc::now).await?;
    print!("{output}");
    Ok(())
}

/// The owner's queue routes, as the commands use them: the API client in
/// `narsor-cli`, the service itself in tests.
pub trait QueueApi: Send + Sync {
    /// The queues and the catalogue files that cannot be used.
    fn observer_queues(
        &self,
    ) -> BoxFuture<'_, std::result::Result<ObserverQueueListResponse, ClientError>>;

    /// One queue with its automations and entries.
    fn observer_queue<'a>(
        &'a self,
        queue: &'a str,
    ) -> BoxFuture<'a, std::result::Result<ObserverQueueView, ClientError>>;
}

impl QueueApi for ApiClient {
    fn observer_queues(
        &self,
    ) -> BoxFuture<'_, std::result::Result<ObserverQueueListResponse, ClientError>> {
        Box::pin(ApiClient::observer_queues(self))
    }

    fn observer_queue<'a>(
        &'a self,
        queue: &'a str,
    ) -> BoxFuture<'a, std::result::Result<ObserverQueueView, ClientError>> {
        Box::pin(ApiClient::observer_queue(self, queue))
    }
}

/// Runs one queue command on `api` and returns what it prints; `now` is
/// the clock ages are measured with.
pub async fn operate<A, N>(api: &A, command: QueueCommand, now: N) -> Result<String>
where
    A: QueueApi + ?Sized,
    N: Fn() -> DateTime<Utc>,
{
    match command {
        QueueCommand::List { json } => {
            let listing = api
                .observer_queues()
                .await
                .map_err(|error| failure("Could not list the queues", error))?;
            if json {
                return json_output(&listing);
            }
            Ok(queue_list_output(&listing))
        }
        QueueCommand::Show { queue, json } => {
            let view = api.observer_queue(&queue).await.map_err(|error| {
                failure(
                    &format!("Could not read queue '{}'", without_unsafe_chars(&queue)),
                    error,
                )
            })?;
            if json {
                return json_output(&view);
            }
            Ok(queue_show_output(&view, now()))
        }
    }
}

/// `value` as pretty JSON, cleaned like `task show --json`, with a final
/// newline.
fn json_output<T: serde::Serialize>(value: &T) -> Result<String> {
    safe_json(value)
        .map(|json| format!("{json}\n"))
        .map_err(|error| CliError::failure(format!("Could not write JSON: {error}")))
}
