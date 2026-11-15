//! `narsor-web`: the HTTP server of the browser interface.
//!
//! It serves the files of the interface and passes the owner API on to a
//! `narsor-server`: the local daemon (`web_upstream: local`, discovered or
//! launched like the CLI and the TUI do) or a server at a URL. The settings
//! and their rules are in [`narsor_web::settings`], the proxy and its guards
//! in [`narsor_web::gateway`].

use std::io::Write as _;
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use clap::Parser;
use narsor_api_client::api::token::TokenSource;
use narsor_api_client::api::{ensure_daemon_running, ApiClient, DiscoveryError};
use narsor_shared::config::{self, Flags};
use narsor_web::gateway::{Gateway, Reconnect, Upstream};
use narsor_web::launch;
use narsor_web::settings::{TokenMode, Upstream as UpstreamSetting, WebSettings};
use tokio::net::TcpListener;

/// The file in the state directory that holds the session token.
const SESSION_FILE: &str = "web-session";

/// The browser interface of Narsor.
#[derive(Debug, Parser)]
#[command(name = "narsor-web", version = narsor_protocol::VERSION)]
struct Cli {
    /// One directory for the catalogue, the state and the work.
    #[arg(long, global = true, value_name = "DIR")]
    root: Option<PathBuf>,
    /// The configuration file.
    #[arg(long, global = true, value_name = "FILE")]
    config: Option<PathBuf>,
    /// Opens the interface in the browser, already signed in.
    #[arg(long)]
    open: bool,
    /// A file whose content is the server's token (for a URL upstream).
    #[arg(long, env = "NARSOR_TOKEN_FILE", value_name = "FILE")]
    token_file: Option<PathBuf>,
    /// A command that prints the server's token (for a URL upstream).
    #[arg(long, env = "NARSOR_TOKEN_COMMAND", value_name = "COMMAND")]
    token_command: Option<String>,
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .with_writer(std::io::stderr)
        .init();
    if let Err(message) = run(cli).await {
        eprintln!("narsor-web: {message}");
        std::process::exit(1);
    }
}

async fn run(cli: Cli) -> Result<(), String> {
    let flags = Flags {
        root: cli.root.clone(),
        config: cli.config.clone(),
    };
    let resolved = config::resolve(&flags, &config::resolve::environment_from_process());
    if !resolved.is_ok() {
        let faults: Vec<String> = resolved.errors().iter().map(ToString::to_string).collect();
        return Err(format!(
            "the configuration does not hold up:\n  {}",
            faults.join("\n  ")
        ));
    }
    let settings = WebSettings::from_config(&resolved).map_err(|problems| problems.join("\n  "))?;
    let state = resolved.state_dir().to_path_buf();

    let upstream = upstream(&settings, &cli, &state, &flags).await?;
    let listener = TcpListener::bind(settings.bind)
        .await
        .map_err(|error| format!("could not listen on {}: {error}", settings.bind))?;
    let address = listener.local_addr().map_err(|error| error.to_string())?;

    let session = match settings.token {
        TokenMode::Server => Some(new_session(&state)?),
        TokenMode::Passthrough => None,
    };
    let page = format!("http://{address}/ui/");
    println!(
        "narsor-web: {page} → {} (token: {})",
        describe(&settings.upstream),
        settings.token.word()
    );
    if session.is_some() {
        println!(
            "narsor-web: the session token is in {} (or start with --open)",
            state.join(SESSION_FILE).display()
        );
    }
    if cli.open {
        let url = match &session {
            Some(session) => format!("{page}#token={session}"),
            None => page.clone(),
        };
        match launch::write_launch_page(&state, &url).and_then(|file| launch::start_browser(&file))
        {
            Ok(()) => println!("narsor-web: opened {page} in the browser"),
            Err(error) => eprintln!(
                "narsor-web: could not open a browser with {} ({error})",
                launch::OPENER
            ),
        }
    }

    let app = Gateway::new(upstream, settings.token, session, address.port()).router();
    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await
        .map_err(|error| error.to_string())
}

fn describe(upstream: &UpstreamSetting) -> String {
    match upstream {
        UpstreamSetting::Local => "the local daemon".to_string(),
        UpstreamSetting::Url(url) => url.clone(),
    }
}

/// The client of the server the API goes to.
async fn upstream(
    settings: &WebSettings,
    cli: &Cli,
    state: &Path,
    flags: &Flags,
) -> Result<Upstream, String> {
    match &settings.upstream {
        UpstreamSetting::Local => {
            let client = local_daemon(state.to_path_buf(), flags.clone()).await?;
            let state = state.to_path_buf();
            let flags = flags.clone();
            let reconnect: Reconnect = Arc::new(move || {
                let state = state.clone();
                let flags = flags.clone();
                Box::pin(local_daemon(state, flags))
            });
            Ok(Upstream::reconnecting(client, reconnect))
        }
        UpstreamSetting::Url(url) => {
            let source = token_source(cli).unwrap_or_else(|| {
                // In the passthrough mode the browser's token is sent; the
                // client's own token is never used.
                TokenSource::Static(String::new())
            });
            if settings.token == TokenMode::Server && token_source(cli).is_none() {
                return Err(format!(
                    "no token for {url}: set NARSOR_TOKEN, NARSOR_TOKEN_FILE (--token-file) or \
                     NARSOR_TOKEN_COMMAND (--token-command), or set web_token: passthrough so the \
                     browser presents its own token"
                ));
            }
            let client = ApiClient::remote(url, source).map_err(|error| error.to_string())?;
            Ok(Upstream::fixed(client))
        }
    }
}

/// The local daemon of `state`, launched when none runs.
async fn local_daemon(state: PathBuf, flags: Flags) -> Result<ApiClient, String> {
    ensure_daemon_running(&state, &flags)
        .await
        .map_err(|error| match error {
            DiscoveryError::NetworkMode => format!(
                "{error}: set web_upstream to the server's URL and give narsor-web a token, or \
                 set web_token: passthrough"
            ),
            other => format!("could not reach the narsor daemon: {other}"),
        })
}

/// The server's token as the operator gave it.
fn token_source(cli: &Cli) -> Option<TokenSource> {
    if let Some(file) = &cli.token_file {
        return Some(TokenSource::File(file.clone()));
    }
    if let Some(command) = &cli.token_command {
        return Some(TokenSource::Command(command.clone()));
    }
    std::env::var("NARSOR_TOKEN")
        .ok()
        .map(|token| token.trim().to_string())
        .filter(|token| !token.is_empty())
        .map(TokenSource::Static)
}

/// A new session token, also written for the operator only.
fn new_session(state: &Path) -> Result<String, String> {
    let session = uuid::Uuid::new_v4().simple().to_string();
    std::fs::create_dir_all(state).map_err(|error| error.to_string())?;
    let path = state.join(SESSION_FILE);
    match std::fs::remove_file(&path) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => return Err(error.to_string()),
        _ => {}
    }
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .map_err(|error| format!("could not write {}: {error}", path.display()))?;
    writeln!(file, "{session}").map_err(|error| error.to_string())?;
    Ok(session)
}
