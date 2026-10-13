//! `narsor task repo|workflows|validate|capabilities` and `task create --file`:
//! the registry of repositories and the catalogue of workflows of the control
//! plane, the way the TUI's `R` and `n` use them (`/control/v1/repositories`,
//! `/workflows`, `/capabilities`).
//!
//! Registering a repository by a path of the server's disk, validating a
//! workflow file and creating a task from one are all things only the
//! trusted local daemon does; any other server refuses them with the reason
//! its capabilities give.

use std::path::Path;

use clap::Subcommand;

use crate::api::ApiClient;
use crate::controller_protocol::CreateRepositoryRequest;
use crate::domain::without_unsafe_chars;

use super::commands::{CliError, Result};
use super::control_text::{
    capabilities_lines, repository_header, repository_row, typed_inputs, validation_lines,
    workflow_lines,
};
use super::remote::{failure, print_created, require_local_daemon};

/// `narsor task repo …`.
#[derive(Debug, Subcommand)]
pub enum RepoCommand {
    /// Lists the registered repositories, ordered by id.
    List,
    /// Shows every registered field.
    Show {
        /// The repository identifier.
        #[arg(value_name = "ID")]
        id: String,
    },
    /// Registers a repository under an id workflows can name.
    Add {
        /// The identifier a workflow writes in `globals.workspace.repository`.
        #[arg(value_name = "ID")]
        id: String,
        /// Anything `git clone` accepts: a URL, or — on a local daemon only —
        /// an absolute path.
        #[arg(long, value_name = "URL|PATH")]
        remote: String,
        /// Human-readable name. Defaults to the id.
        #[arg(long, value_name = "NAME")]
        name: Option<String>,
        /// Where the repository is hosted. Inferred from the remote when omitted.
        #[arg(long, value_name = "PROVIDER")]
        provider: Option<String>,
        /// Branch used when a workflow declares no `ref` of its own.
        #[arg(long = "default-branch", value_name = "BRANCH")]
        default_branch: Option<String>,
        /// Free-form note about what the repository is.
        #[arg(long, value_name = "TEXT")]
        description: Option<String>,
    },
    /// Removes a repository from the registry.
    Remove {
        /// The repository identifier.
        #[arg(value_name = "ID")]
        id: String,
    },
}

/// Runs one `narsor task repo` command.
pub async fn repo(client: &ApiClient, command: RepoCommand) -> Result<()> {
    match command {
        RepoCommand::List => {
            let listing = client
                .control_repositories()
                .await
                .map_err(|error| failure("Could not list the repositories", error))?;
            if listing.repositories.is_empty() && listing.unreadable.is_empty() {
                println!("No repositories registered.");
                return Ok(());
            }
            if !listing.repositories.is_empty() {
                println!("{}", repository_header());
                for repository in &listing.repositories {
                    println!("{}", repository_row(repository));
                }
            }
            if listing.unreadable.is_empty() {
                return Ok(());
            }
            // Like `narsor repo list`: what loaded is shown, what did not is
            // named, and the exit code says the registry is not whole.
            for unreadable in &listing.unreadable {
                eprintln!("{}", without_unsafe_chars(unreadable));
            }
            Err(CliError::failure(format!(
                "{} registry entr(y/ies) could not be read; they are named above.",
                listing.unreadable.len()
            )))
        }
        RepoCommand::Show { id } => {
            let listing = client
                .control_repositories()
                .await
                .map_err(|error| failure("Could not read the repository registry", error))?;
            let entry = listing
                .repositories
                .iter()
                .find(|repository| repository.id == id)
                .ok_or_else(|| {
                    CliError::failure(format!("Repository '{id}' is not registered."))
                })?;
            println!("Repository:     {}", entry.id);
            println!("Name:           {}", entry.name);
            println!("Provider:       {}", entry.provider);
            println!("Remote:         {}", entry.remote);
            println!(
                "Default branch: {}",
                entry.default_branch.as_deref().unwrap_or("-")
            );
            println!(
                "Description:    {}",
                entry.description.as_deref().unwrap_or("-")
            );
            Ok(())
        }
        RepoCommand::Add {
            id,
            remote,
            name,
            provider,
            default_branch,
            description,
        } => {
            let added = client
                .control_add_repository(&CreateRepositoryRequest {
                    id,
                    remote,
                    name,
                    provider,
                    default_branch,
                    description,
                })
                .await
                .map_err(|error| failure("Could not register the repository", error))?;
            println!(
                "Registered repository '{}' -> {}",
                without_unsafe_chars(&added.id),
                without_unsafe_chars(&added.remote)
            );
            Ok(())
        }
        RepoCommand::Remove { id } => {
            let removed = client
                .control_remove_repository(&id)
                .await
                .map_err(|error| failure(&format!("Could not remove repository '{id}'"), error))?;
            println!(
                "Removed repository '{}'.",
                without_unsafe_chars(&removed.id)
            );
            Ok(())
        }
    }
}

/// `narsor task workflows` — the workflows a task can be created from.
pub async fn workflows(client: &ApiClient) -> Result<()> {
    let listing = client
        .control_workflows()
        .await
        .map_err(|error| failure("Could not list the workflows", error))?;
    if listing.workflows.is_empty() {
        println!("No workflows.");
    }
    for workflow in &listing.workflows {
        for line in workflow_lines(workflow) {
            println!("{}", line.text);
        }
    }
    Ok(())
}

/// `narsor task validate <PATH>` — every violation of a workflow file on the
/// local daemon's disk; fails when there is one.
pub async fn validate(client: &ApiClient, path: &Path) -> Result<()> {
    require_local_daemon(client, "Validating a workflow file").await?;
    let path = absolute(path)?;
    let verdict = client
        .control_validate_workflow(&path)
        .await
        .map_err(|error| failure("The file could not be validated", error))?;
    for line in validation_lines(&verdict) {
        println!("{}", line.text);
    }
    if verdict.valid {
        Ok(())
    } else {
        Err(CliError::failure(format!(
            "'{path}' is not a valid workflow definition."
        )))
    }
}

/// `narsor task capabilities` — what the control plane offers.
pub async fn capabilities(client: &ApiClient) -> Result<()> {
    let view = client
        .control_capabilities()
        .await
        .map_err(|error| failure("Could not read the capabilities", error))?;
    for line in capabilities_lines(&view) {
        println!("{}", line.text);
    }
    Ok(())
}

/// `narsor task create --file <PATH>` — a task from a workflow file of the
/// local daemon's disk, through the local façade (`POST /tasks`), exactly as
/// the TUI's "path" entry of the new-task form does.
///
/// The file is checked first, so that every violation is named and a missing
/// or mistyped input is reported before anything is queued.
pub async fn create_from_file(
    client: &ApiClient,
    file: &Path,
    inputs: &[String],
    task_id: Option<String>,
) -> Result<()> {
    require_local_daemon(client, "Creating a task from a workflow file").await?;
    let path = absolute(file)?;
    let verdict = client
        .control_validate_workflow(&path)
        .await
        .map_err(|error| failure("The file could not be validated", error))?;
    if !verdict.valid {
        let mut message = format!("'{path}' is invalid — no task was created:");
        for violation in &verdict.violations {
            message.push_str(&format!("\n  - {}", without_unsafe_chars(violation)));
        }
        return Err(CliError::failure(message));
    }
    let inputs = typed_inputs(&verdict.inputs, inputs)?;
    let task_id = task_id.unwrap_or_else(|| format!("task-{}", uuid::Uuid::new_v4().simple()));
    let started = client
        .start_task(&task_id, &path, inputs)
        .await
        .map_err(|error| failure("Could not create the task", error))?;
    print_created(&started.task);
    Ok(())
}

/// `path` as the absolute path the server needs, with `~/` expanded.
fn absolute(path: &Path) -> Result<String> {
    let expanded = match (path.strip_prefix("~"), std::env::var_os("HOME")) {
        (Ok(rest), Some(home)) => std::path::PathBuf::from(home).join(rest),
        _ => path.to_path_buf(),
    };
    std::path::absolute(&expanded)
        .map(|absolute| absolute.display().to_string())
        .map_err(|error| {
            CliError::usage(format!(
                "Cannot make '{}' absolute: {error}",
                path.display()
            ))
        })
}
