//! Command-line interface.
//!
//! The operator-facing half of the orchestrator: validate a definition, start a
//! workflow, look at what is on disk, and answer a question a workflow asked.
//! Everything here is a thin shell around the library — argument parsing,
//! resolving the workflow definition, and formatting — so that every operation
//! reachable from the terminal is also reachable from the engine API (and, from
//! U10 on, from the TUI).
//!
//! # Two streams
//!
//! Command output goes to stdout, diagnostics and `tracing` logs to stderr.
//! That is what makes `orchestrator list | grep paused` work.
//!
//! # Exit codes are a contract
//!
//! CI is expected to branch on them, so they are fixed:
//!
//! | code | meaning |
//! |------|---------|
//! | [`EXIT_SUCCESS`] | the command did what it was asked |
//! | [`EXIT_FAILURE`] | the definition is invalid, the execution failed, or the store could not answer |
//! | [`EXIT_USAGE`]   | the invocation itself is wrong: bad flags, an unknown input, a missing required input |
//!
//! A *failing workflow* is [`EXIT_FAILURE`], not a usage error: the engine ran
//! exactly as asked and the workflow ended in a failure terminal state.
//! Conversely a missing `--input` is [`EXIT_USAGE`], because nothing was run —
//! which is also the code clap itself returns for a malformed command line.

pub mod commands;

use std::path::PathBuf;

use clap::{Parser, Subcommand};

use crate::persistence::DEFAULT_ROOT;

/// The command did what it was asked.
pub const EXIT_SUCCESS: i32 = 0;

/// A validation error, a failed execution, or an unreadable store.
pub const EXIT_FAILURE: i32 = 1;

/// The invocation itself was wrong.
pub const EXIT_USAGE: i32 = 2;

/// The `orchestrator` command line.
#[derive(Debug, Parser)]
#[command(
    name = "orchestrator",
    version,
    about = "Orkiestrator workflow AI — walidacja definicji, uruchamianie i podgląd wykonań",
    disable_help_subcommand = true
)]
pub struct Cli {
    /// Katalog danych orkiestratora (wykonania i workspace'y).
    #[arg(
        long,
        global = true,
        value_name = "KATALOG",
        default_value = DEFAULT_ROOT
    )]
    pub root: PathBuf,

    /// Podkomenda do wykonania.
    #[command(subcommand)]
    pub command: Command,
}

/// Everything the operator can ask for.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Sprawdza definicję workflow i wypisuje wszystkie naruszenia naraz.
    Validate {
        /// Plik YAML z definicją workflow.
        #[arg(value_name = "PLIK")]
        file: PathBuf,
    },

    /// Uruchamia workflow z podanymi wejściami.
    Run {
        /// Plik YAML z definicją workflow.
        #[arg(value_name = "PLIK")]
        file: PathBuf,

        /// Wejście workflow w postaci `nazwa=wartość` (można podać wielokrotnie).
        #[arg(long = "input", value_name = "NAZWA=WARTOŚĆ")]
        inputs: Vec<String>,
    },

    /// Wypisuje wykonania zapisane w katalogu danych.
    List,

    /// Pokazuje stan, status i zmienne kontekstu jednego wykonania.
    Show {
        /// Identyfikator wykonania.
        #[arg(value_name = "ID_WYKONANIA")]
        execution_id: String,
    },

    /// Wypisuje historię zdarzeń wykonania.
    Events {
        /// Identyfikator wykonania.
        #[arg(value_name = "ID_WYKONANIA")]
        execution_id: String,

        /// Śledzi nowe zdarzenia, aż wykonanie się zatrzyma.
        #[arg(long)]
        follow: bool,
    },

    /// Dostarcza wejście człowieka do wstrzymanego wykonania i wznawia je.
    Input {
        /// Identyfikator wykonania.
        #[arg(value_name = "ID_WYKONANIA")]
        execution_id: String,

        /// Wartość pola formularza w postaci `nazwa=wartość` (można podać wielokrotnie).
        #[arg(long = "field", value_name = "NAZWA=WARTOŚĆ")]
        fields: Vec<String>,

        /// Etykieta wyjścia wybrana wprost (gdy akcja nie ma pól albo ma ich kilka).
        #[arg(long, value_name = "ETYKIETA")]
        label: Option<String>,

        /// Plik definicji workflow, gdy nie da się go odczytać z zapisanego wykonania.
        #[arg(long = "file", value_name = "PLIK")]
        file: Option<PathBuf>,
    },

    /// Wznawia wstrzymane wykonanie, które nie czeka na wejście człowieka.
    Resume {
        /// Identyfikator wykonania.
        #[arg(value_name = "ID_WYKONANIA")]
        execution_id: String,

        /// Plik definicji workflow, gdy nie da się go odczytać z zapisanego wykonania.
        #[arg(long = "file", value_name = "PLIK")]
        file: Option<PathBuf>,
    },

    /// Pełnoekranowy interfejs terminalowy (jeszcze niedostępny).
    Tui,
}

/// Runs the parsed command line and returns the process exit code.
///
/// Nothing here panics on an operator mistake: every failure becomes a message
/// on stderr plus one of the three documented codes.
pub async fn run(cli: Cli) -> i32 {
    let outcome = match cli.command {
        Command::Validate { file } => commands::validate(&file),
        Command::Run { file, inputs } => commands::run(&cli.root, &file, &inputs).await,
        Command::List => commands::list(&cli.root),
        Command::Show { execution_id } => commands::show(&cli.root, &execution_id),
        Command::Events {
            execution_id,
            follow,
        } => commands::events(&cli.root, &execution_id, follow).await,
        Command::Input {
            execution_id,
            fields,
            label,
            file,
        } => commands::input(&cli.root, &execution_id, &fields, label, file.as_deref()).await,
        Command::Resume { execution_id, file } => {
            commands::resume(&cli.root, &execution_id, file.as_deref()).await
        }
        Command::Tui => commands::tui(),
    };

    match outcome {
        Ok(()) => EXIT_SUCCESS,
        Err(error) => {
            eprintln!("{}", error.message);
            error.exit_code
        }
    }
}
