use std::{
    env,
    fs::{self, File, OpenOptions},
    io::{self, IsTerminal, Write},
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    process::Command,
    time::{SystemTime, UNIX_EPOCH},
};

use clap::{Args, ValueEnum};
use serde_yaml::{Mapping, Value};

use crate::{
    cli::{
        commands::{controller_program, CliError, Result},
        prompt,
    },
    config::{self, ConfigCascade, Scope, Section, SettingKey, SettingValue},
    domain::BackendId,
};

struct WorkstationBackend {
    id: BackendId,
    catalog_dir: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum InitRole {
    Local,
    Frontend,
    Server,
}

/// How the local controller runs tasks (`controller.local.mode`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum LocalWorkMode {
    /// On the host, as the user.
    Process,
    /// In a hardened container on the local Docker daemon.
    Docker,
}

/// Asks whether the Docker daemon can run the docker work mode: the
/// daemon's version, or why not.
type DockerCheck<'a> = &'a dyn Fn() -> std::result::Result<String, String>;

/// The secret the example task image gives Claude Code its token from.
const CLAUDE_TOKEN_SECRET: &str = "claude-token";

/// The example catalogue's task image (docs/user/local-docker.md).
const EXAMPLE_RUNTIME: &str = include_str!("../../examples/catalog/runtime.yaml");

/// The scripts of the components `init` keeps from the example task image,
/// by their path in the catalogue.
const TASK_IMAGE_SCRIPTS: &[(&str, &str)] = &[
    (
        "components/base-tools.sh",
        include_str!("../../examples/catalog/components/base-tools.sh"),
    ),
    (
        "components/claude-code/install.sh",
        include_str!("../../examples/catalog/components/claude-code/install.sh"),
    ),
    (
        "components/claude-code/version.sh",
        include_str!("../../examples/catalog/components/claude-code/version.sh"),
    ),
];

/// Where the example's components `init` leaves out begin: they need tokens
/// a new installation does not have, and a task refuses to start without
/// them.
const EXAMPLE_OPTIONAL_COMPONENTS: &str = "    - name: glab\n";

fn role_name(role: InitRole) -> &'static str {
    match role {
        InitRole::Local => "local",
        InitRole::Frontend => "frontend",
        InitRole::Server => "server",
    }
}

#[derive(Debug, Default, Args)]
pub struct InitOptions {
    /// What this installation will do.
    #[arg(long, value_enum)]
    role: Option<InitRole>,

    /// Do not prompt; all role-specific settings must be provided explicitly.
    #[arg(long)]
    non_interactive: bool,

    /// Root directory for Narsor data (default ~/.narsor); `catalog`, `state` and `work` are created under it.
    #[arg(long, value_name = "DIRECTORY")]
    data_dir: Option<PathBuf>,

    /// Configuration file to create or update instead of the effective Narsor configuration.
    #[arg(long, value_name = "FILE")]
    config_file: Option<PathBuf>,

    /// Address on which a network-mode server listens.
    #[arg(long, value_name = "HOST:PORT")]
    server_bind: Option<String>,

    /// HTTPS OIDC issuer for server mode.
    #[arg(long, value_name = "HTTPS_URL")]
    oidc_issuer: Option<String>,

    /// OIDC audience for server mode.
    #[arg(long, value_name = "AUDIENCE")]
    oidc_audience: Option<String>,

    /// Trusted TLS-terminating proxy address or CIDR (may be repeated).
    #[arg(long = "trusted-proxy", value_name = "IP_OR_CIDR")]
    trusted_proxies: Vec<String>,

    /// Unique catalogue ID for the same-VM workstation backend.
    #[arg(long, value_name = "ID")]
    backend_id: Option<String>,

    /// How local tasks run (local role): process (on the host as your user)
    /// or docker (in a hardened container; needs rootful Docker).
    #[arg(long, value_enum, value_name = "MODE")]
    local_mode: Option<LocalWorkMode>,
}

#[derive(Debug, Default, Args)]
pub struct PurgeOptions {
    /// Do not ask for confirmation.
    #[arg(long)]
    yes: bool,

    /// Narsor data directory to remove (defaults to ~/.narsor).
    #[arg(long, value_name = "DIRECTORY")]
    data_dir: Option<PathBuf>,

    /// Configuration file to remove instead of the effective Narsor configuration.
    #[arg(long, value_name = "FILE")]
    config_file: Option<PathBuf>,
}

pub fn run(options: InitOptions, global_config: Option<&Path>) -> Result<()> {
    run_with(options, global_config, &controller_docker_check)
}

fn run_with(
    options: InitOptions,
    global_config: Option<&Path>,
    docker_check: DockerCheck<'_>,
) -> Result<()> {
    let role = select_role(options.role, options.non_interactive)?;
    let config_path = select_config_path(
        options.config_file.clone(),
        global_config,
        options.non_interactive,
    )?;
    let keep_existing = path_exists(&config_path)?;
    let target = if keep_existing {
        let mut name = config_path.as_os_str().to_owned();
        name.push(".new");
        PathBuf::from(name)
    } else {
        config_path.clone()
    };
    let data_dir = select_data_directory(
        options.data_dir.clone(),
        options.non_interactive || keep_existing,
    )?;
    let local_mode = select_local_mode(
        role,
        options.local_mode,
        options.non_interactive,
        docker_check,
    )?;

    let mut config = Value::Mapping(Mapping::new());
    set_root_paths(&mut config, &data_dir)?;
    let parsed = validate_config(&target, &config)?.1;
    // Docker mode needs its task image in the catalogue the file names.
    let docker_catalog = match local_mode {
        LocalWorkMode::Docker => {
            insert_text(
                config
                    .as_mapping_mut()
                    .expect("configuration mapping checked"),
                SettingKey::LocalMode,
                "docker",
            );
            Some(resolved_directory(&parsed, SettingKey::CatalogDir)?)
        }
        LocalWorkMode::Process => None,
    };
    let workstation_backend = if role == InitRole::Server {
        Some(configure_server(
            &mut config,
            &parsed,
            &options,
            options.non_interactive,
        )?)
    } else {
        None
    };

    let contents = validate_config(&target, &config)?.0;
    prepare_config_directory(&target)?;
    if let Some(backend) = &workstation_backend {
        write_workstation_backend(backend)?;
    }
    if keep_existing {
        replace_file(&target, contents.as_bytes())?;
    } else {
        write_new_atomic(&target, contents.as_bytes(), 0o600)?;
    }
    let task_image_written = docker_catalog
        .as_deref()
        .map(write_task_image)
        .transpose()?;

    println!("Configured the {} role.", role_name(role));
    if keep_existing {
        println!(
            "Existing configuration left unchanged: {}",
            config_path.display()
        );
        println!("New defaults written to: {}", target.display());
        println!("Compare them and copy over what you want.");
    } else {
        println!("Configuration: {}", target.display());
    }
    if let Some(backend) = &workstation_backend {
        print_server_commands(backend, &target)?;
    }
    if let (Some(catalog), Some(written)) = (&docker_catalog, task_image_written) {
        print_docker_steps(catalog, written);
    }
    Ok(())
}

fn path_exists(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(CliError::failure(format!(
            "cannot inspect '{}': {error}",
            path.display()
        ))),
    }
}

/// The work mode of local tasks. Only the local role runs the built-in
/// local controller; the server role's workstation controller serves the
/// network server, which the docker work mode refuses (it needs the local
/// daemon as the control plane).
///
/// An explicit `--local-mode docker` is kept when Docker is not ready (with
/// a warning); a docker mode chosen at the prompt is confirmed first.
fn select_local_mode(
    role: InitRole,
    selected: Option<LocalWorkMode>,
    non_interactive: bool,
    docker_check: DockerCheck<'_>,
) -> Result<LocalWorkMode> {
    match (role, selected) {
        (InitRole::Local, _) => {}
        (InitRole::Frontend, Some(_)) => {
            return Err(CliError::usage(
                "--local-mode applies to the local role; the frontend role runs no tasks",
            ))
        }
        (InitRole::Server, Some(LocalWorkMode::Docker)) => {
            return Err(CliError::usage(
                "--local-mode docker applies to the local role: the server role's workstation \
                 controller serves the network server, and the docker work mode needs the local \
                 daemon as the control plane",
            ))
        }
        (InitRole::Frontend | InitRole::Server, _) => return Ok(LocalWorkMode::Process),
    }
    let ask = selected.is_none() && !non_interactive && prompt::is_available();
    let mode = match selected {
        Some(mode) => mode,
        None if ask => {
            let modes = [
                (
                    LocalWorkMode::Process,
                    "process",
                    "on the host as your user",
                ),
                (
                    LocalWorkMode::Docker,
                    "docker",
                    "in a hardened container; needs rootful Docker",
                ),
            ];
            let items: Vec<(&str, &str)> =
                modes.iter().map(|(_, name, text)| (*name, *text)).collect();
            modes[prompt::select("Work mode for local tasks", &items)?].0
        }
        None => LocalWorkMode::Process,
    };
    if mode == LocalWorkMode::Process {
        return Ok(mode);
    }
    match docker_check() {
        Ok(version) => {
            println!("Docker {version} is ready for the docker work mode.");
            Ok(LocalWorkMode::Docker)
        }
        Err(reason) if ask => {
            eprintln!("Docker is not ready: {reason}");
            let choices = [
                ("No", "use the process mode"),
                (
                    "Yes",
                    "write the docker mode; fix Docker before the first task",
                ),
            ];
            Ok(
                match prompt::select("Write docker mode anyway?", &choices)? {
                    1 => LocalWorkMode::Docker,
                    _ => LocalWorkMode::Process,
                },
            )
        }
        Err(reason) => {
            eprintln!("warning: Docker is not ready: {reason}");
            eprintln!(
                "warning: the docker mode is written as asked; local tasks do not start until Docker works"
            );
            Ok(LocalWorkMode::Docker)
        }
    }
}

/// `narsor-controller local docker check`, run next to this executable: the
/// same check the controller makes before a Docker-mode task.
fn controller_docker_check() -> std::result::Result<String, String> {
    let program = controller_program("it checks Docker").map_err(|error| error.message)?;
    let output = Command::new(&program)
        .args(["local", "docker", "check"])
        .output()
        .map_err(|error| format!("cannot run {}: {error}", program.display()))?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).trim().to_string())
    }
}

/// The task image `init` writes: the example's, without the components
/// that need tokens beyond Claude Code's.
fn initial_runtime() -> String {
    let kept = EXAMPLE_RUNTIME
        .split_once(EXAMPLE_OPTIONAL_COMPONENTS)
        .map_or(EXAMPLE_RUNTIME, |(kept, _)| kept);
    kept.replacen(
        "# The runtime catalogue: copy it to {catalog}/runtime.yaml.",
        "# The runtime catalogue, written by `narsor-cli init` from the example catalogue.",
        1,
    )
}

/// Writes the task image into `catalog` when it has no `runtime.yaml`;
/// false when one exists. An existing file is never replaced.
fn write_task_image(catalog: &Path) -> Result<bool> {
    let runtime = catalog.join("runtime.yaml");
    if path_exists(&runtime)? {
        return Ok(false);
    }
    for (path, script) in TASK_IMAGE_SCRIPTS {
        let target = catalog.join(path);
        if path_exists(&target)? {
            println!("Left unchanged: {}", target.display());
        } else {
            write_new_atomic(&target, script.as_bytes(), 0o644)?;
        }
    }
    write_new_atomic(&runtime, initial_runtime().as_bytes(), 0o644)?;
    Ok(true)
}

fn print_docker_steps(catalog: &Path, runtime_written: bool) {
    let runtime = catalog.join("runtime.yaml");
    if runtime_written {
        println!("Task image: {}", runtime.display());
    } else {
        println!("Existing task image left unchanged: {}", runtime.display());
    }
    println!("\nNext steps for the docker work mode:");
    println!("  1. claude setup-token   (prints a long-lived token; save it to a file)");
    println!("  2. narsor-cli secret set {CLAUDE_TOKEN_SECRET} < token-file");
    println!(
        "  3. Add the tools your tasks need to {}",
        runtime.display()
    );
    println!("See docs/user/local-docker.md.");
}

fn select_role(role: Option<InitRole>, non_interactive: bool) -> Result<InitRole> {
    if let Some(role) = role {
        return Ok(role);
    }
    if non_interactive {
        return Err(CliError::usage(
            "non-interactive configuration requires --role local, frontend, or server",
        ));
    }
    if !prompt::is_available() {
        return Err(CliError::usage(
            "interactive configuration needs a terminal; provide --non-interactive and --role",
        ));
    }
    let roles = [
        (
            InitRole::Local,
            "Local",
            "TUI, CLI, server and controller on this machine",
        ),
        (
            InitRole::Frontend,
            "Frontend",
            "TUI and CLI for a separately operated server",
        ),
        (
            InitRole::Server,
            "Server",
            "network server, controller and runner",
        ),
    ];
    let items: Vec<(&str, &str)> = roles.iter().map(|(_, name, text)| (*name, *text)).collect();
    Ok(roles[prompt::select("Role", &items)?].0)
}

/// The root directory for all Narsor data. Without `--data-dir` it is asked
/// for (default `~/.narsor`), unless `use_default` says to take the default.
fn select_data_directory(selected: Option<PathBuf>, use_default: bool) -> Result<PathBuf> {
    if let Some(path) = selected {
        return absolute_path(&prompt::expand_tilde(&path.to_string_lossy()));
    }
    let default = default_data_dir()?;
    if use_default {
        return absolute_path(&default);
    }
    let label = "Narsor data directory (Tab completes, Enter accepts)";
    let answer = if prompt::is_available() {
        prompt::input_path(label, DEFAULT_DATA_DIR_TEXT)?
    } else {
        prompt_with_default(
            &format!("Narsor data directory [{DEFAULT_DATA_DIR_TEXT}]: "),
            DEFAULT_DATA_DIR_TEXT,
        )?
    };
    if answer.trim().is_empty() {
        return absolute_path(&default);
    }
    absolute_path(&prompt::expand_tilde(answer.trim()))
}

fn select_config_path(
    explicit: Option<PathBuf>,
    global_config: Option<&Path>,
    non_interactive: bool,
) -> Result<PathBuf> {
    if let Some(explicit) = explicit {
        if global_config.is_some_and(|global| global != explicit) {
            return Err(CliError::usage(
                "--config and --config-file name different files; use only one",
            ));
        }
        return select_writable_config_path(absolute_path(&explicit)?, false, non_interactive);
    }
    if let Some(global) = global_config {
        return select_writable_config_path(absolute_path(global)?, false, non_interactive);
    }
    let resolution = ConfigCascade::new(None)
        .resolve()
        .map_err(|error| CliError::failure(error.to_string()))?;
    if let Some(path) = resolution.path() {
        return select_writable_config_path(absolute_path(path)?, true, non_interactive);
    }
    absolute_path(&default_config_path()?)
}

fn select_writable_config_path(
    path: PathBuf,
    inferred: bool,
    non_interactive: bool,
) -> Result<PathBuf> {
    if config_parent_is_writable(&path)? {
        return Ok(path);
    }
    if inferred && !non_interactive {
        let alternate = prompt_required(
            &format!(
                "Configuration '{}' is not writable. Enter an alternate config file path: ",
                path.display()
            ),
            "config-file",
        )?;
        let alternate = absolute_path(Path::new(&alternate))?;
        if config_parent_is_writable(&alternate)? {
            return Ok(alternate);
        }
        return Err(CliError::failure(format!(
            "configuration directory for '{}' is not writable",
            alternate.display()
        )));
    }
    Err(CliError::failure(format!(
        "configuration '{}' is not writable; retry with --config-file PATH",
        path.display()
    )))
}

fn config_parent_is_writable(path: &Path) -> Result<bool> {
    let parent = path.parent().ok_or_else(|| {
        CliError::failure(format!(
            "configuration path '{}' has no parent directory",
            path.display()
        ))
    })?;
    if !parent.exists() {
        return Ok(true);
    }
    if !parent.is_dir() {
        return Err(CliError::failure(format!(
            "configuration parent '{}' is not a directory",
            parent.display()
        )));
    }
    let probe = temporary_path(path, "probe")?;
    let file = match OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&probe)
    {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::PermissionDenied => return Ok(false),
        Err(error) => {
            return Err(CliError::failure(format!(
                "cannot check configuration directory '{}': {error}",
                parent.display()
            )))
        }
    };
    drop(file);
    fs::remove_file(&probe).map_err(|error| {
        CliError::failure(format!(
            "cannot remove configuration write-check file '{}': {error}",
            probe.display()
        ))
    })?;
    Ok(true)
}

fn prepare_config_directory(path: &Path) -> Result<()> {
    let parent = path.parent().ok_or_else(|| {
        CliError::failure(format!(
            "configuration path '{}' has no parent directory",
            path.display()
        ))
    })?;
    fs::create_dir_all(parent).map_err(|error| {
        CliError::failure(format!(
            "cannot create configuration directory '{}': {error}",
            parent.display()
        ))
    })?;
    if !config_parent_is_writable(path)? {
        return Err(CliError::failure(format!(
            "configuration directory '{}' is not writable",
            parent.display()
        )));
    }
    Ok(())
}

/// Renders the file and parses it back with every section in scope, so
/// what is written is exactly what was checked.
fn validate_config(path: &Path, config: &Value) -> Result<(String, config::FileValues)> {
    let text = render_config(config);
    let parsed = config::file::parse(path, &text, &Scope::ALL)
        .into_result()
        .map_err(|errors| {
            CliError::failure(
                errors
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join("\n"),
            )
        })?;
    Ok((text, parsed))
}

/// Optional settings with their built-in values, written as comments so the
/// file documents what can be tuned without changing behaviour.
const OPTIONAL_SETTINGS: &[(SettingKey, &str, &str)] = &[
    (
        SettingKey::AgentHarness,
        "claude-code",
        "Agent harness used by actions that name none.",
    ),
    (
        SettingKey::AgentArgs,
        "{}",
        "Extra arguments by harness, e.g. { claude: [--dangerously-skip-permissions] }; they win over Narsor's own.",
    ),
    (
        SettingKey::MaxConcurrentTasks,
        "none",
        "Tasks processed at once across all backends (replaces backend limits), or none.",
    ),
    (
        SettingKey::ConductorModel,
        "\"\"",
        "Model tier of the conductor (small, medium, large, max); empty is the harness's default.",
    ),
    (
        SettingKey::ConductorEffort,
        "\"\"",
        "Effort of the conductor (low, medium, high, max); empty is the harness's default.",
    ),
    (
        SettingKey::LogLevel,
        "off",
        "off, error, warn, info, debug or trace (RUST_LOG wins).",
    ),
    (
        SettingKey::TuiRefreshInterval,
        "10",
        "Seconds between automatic TUI refreshes, or none.",
    ),
    (
        SettingKey::DefaultBackend,
        "local",
        "Backend a new task goes to when it names none.",
    ),
    (
        SettingKey::DefaultProfile,
        "default",
        "Profile a new task uses when it names none.",
    ),
    (
        SettingKey::LocalHome,
        "host",
        "HOME for local tasks' tools: host or private.",
    ),
    (
        SettingKey::LocalWorkspaceRoots,
        "[]",
        "Host directories a local task may use as workspace.",
    ),
    (
        SettingKey::LocalCodexHome,
        "\"\"",
        "CODEX_HOME for local tasks; empty leaves it to the harness.",
    ),
    (
        SettingKey::LocalClaudeConfigDir,
        "\"\"",
        "CLAUDE_CONFIG_DIR for local tasks; empty leaves it to the harness.",
    ),
    (
        SettingKey::LocalClaudeProfileSource,
        "\"\"",
        "Claude Code profile copied into local tasks before each start.",
    ),
    (
        SettingKey::LocalMode,
        "process",
        "How local tasks run: process (on the host) or docker (in a container; HOME is private).",
    ),
    (
        SettingKey::LocalDockerUpdateInterval,
        "24h",
        "How long a built task image is used before its component versions are checked again, or none.",
    ),
    (
        SettingKey::CorsAllowedOrigins,
        "[]",
        "Browser origins allowed to call the API.",
    ),
    (
        SettingKey::ServerMode,
        "trusted",
        "trusted (loopback) or network (OIDC); see the server role.",
    ),
    (
        SettingKey::ServerBind,
        "127.0.0.1:0",
        "Address the server listens on.",
    ),
    (
        SettingKey::ObserverEnabled,
        "false",
        "When true, the server starts the observer worker (needs server.observer.script_user; see the installation guide).",
    ),
    (
        SettingKey::ObserverServerUrl,
        "\"\"",
        "API URL for the observer worker and its scripts; empty is the server's own address (not in network mode).",
    ),
    (
        SettingKey::ObserverSecretsDir,
        "\"\"",
        "Observer secrets: one file per secret name; a script gets only the secrets its automation declares; empty: no secrets.",
    ),
    (
        SettingKey::ObserverScriptUser,
        "\"\"",
        "System user observer scripts run as; must not be the server's.",
    ),
    (
        SettingKey::ObserverInsecureSameUser,
        "false",
        "true runs observer scripts as the server's own user, without isolation: for a trial only.",
    ),
    (
        SettingKey::WebBind,
        "127.0.0.1:8484",
        "Address narsor-web (the browser interface) listens on; loopback only.",
    ),
    (
        SettingKey::WebUpstream,
        "local",
        "Server narsor-web passes the API to: local (the local daemon) or the server's URL.",
    ),
    (
        SettingKey::WebToken,
        "server",
        "Whose token narsor-web sends: server (its own; the browser gets a session token) or passthrough (the browser's).",
    ),
];

/// Settings never suggested as commented defaults: `init` writes the root
/// and the directories itself, and the network-mode ones belong to the
/// server role only (trusted mode refuses them, so a commented default would
/// mislead). Their descriptions go above them when the file writes them.
const INSTALLER_SETTINGS: &[(SettingKey, &str)] = &[
    (
        SettingKey::GlobalRoot,
        "Base directory of the installation; relative server directories are joined to it.",
    ),
    (
        SettingKey::CatalogDir,
        "Catalogue directory (workflows, backends, profiles), relative to global.root.",
    ),
    (
        SettingKey::StateDir,
        "Server state directory (database, daemon files), relative to global.root.",
    ),
    (
        SettingKey::WorkDir,
        "Directory for task workspaces, relative to global.root.",
    ),
    (
        SettingKey::OidcIssuer,
        "HTTPS OIDC issuer that signs the users' tokens (network mode).",
    ),
    (
        SettingKey::OidcAudience,
        "Audience the users' tokens must carry (network mode).",
    ),
    (
        SettingKey::OwnerSubject,
        "OIDC subject of the installation owner (network mode).",
    ),
    (
        SettingKey::TlsTermination,
        "Where TLS ends: proxy (a TLS-terminating proxy in front of the server).",
    ),
    (
        SettingKey::TrustedProxies,
        "Addresses or CIDRs of the TLS-terminating proxies the server trusts.",
    ),
];

/// One setting of the generated file: written with a value, or shown as a
/// commented-out default.
struct Entry {
    setting: SettingKey,
    segments: Vec<&'static str>,
    line: EntryLine,
}

enum EntryLine {
    Written(String),
    Default(&'static str),
}

impl Entry {
    fn is_written(&self) -> bool {
        matches!(self.line, EntryLine::Written(_))
    }
}

/// The generated file: one tree in registry order, each section holding its
/// written settings and, as comments in their place, the remaining defaults.
/// A section or group with nothing written is itself commented out, so
/// removing every `# ` in front of a key still gives a valid file.
fn render_config(config: &Value) -> String {
    let mapping = config.as_mapping().expect("configuration mapping checked");
    let mut text = String::from(
        "# Narsor configuration generated by `narsor-cli init`.\n\
         # Lines starting with `# key:` show the default value; remove the `# ` to change it.\n\
         # Every setting can also be given as a NARSOR_<SECTION>__<KEY> environment variable;\n\
         # a group adds one more level, e.g. NARSOR_SERVER__OIDC__ISSUER for server.oidc.issuer.\n",
    );
    for section in Section::ALL {
        let entries: Vec<Entry> = SettingKey::ALL
            .into_iter()
            .filter(|setting| setting.section() == section)
            .filter_map(|setting| entry_of(mapping, setting))
            .collect();
        if !entries.is_empty() {
            text.push('\n');
            render_group(&mut text, &entries, 0);
        }
    }
    text
}

fn entry_of(mapping: &Mapping, setting: SettingKey) -> Option<Entry> {
    let line = match setting_in(mapping, setting) {
        Some(value) => EntryLine::Written(inline_yaml(value)),
        None => EntryLine::Default(default_of(mapping, setting)?),
    };
    Some(Entry {
        setting,
        segments: setting.segments().collect(),
        line,
    })
}

/// The commented default of a setting. With the docker mode written, the
/// HOME suggested is `private`: a container never gets the host's HOME, and
/// an uncommented `host` would stop the daemon.
fn default_of(mapping: &Mapping, setting: SettingKey) -> Option<&'static str> {
    let docker =
        setting_in(mapping, SettingKey::LocalMode).and_then(Value::as_str) == Some("docker");
    if setting == SettingKey::LocalHome && docker {
        return Some("private");
    }
    OPTIONAL_SETTINGS
        .iter()
        .find(|(key, _, _)| *key == setting)
        .map(|(_, default, _)| *default)
}

/// Renders entries that share their first `depth` segments, grouping the
/// consecutive ones that share the next segment.
fn render_group(text: &mut String, entries: &[Entry], depth: usize) {
    let indent = "  ".repeat(depth);
    let mut rest = entries;
    while let Some(first) = rest.first() {
        let name = first.segments[depth];
        let size = rest
            .iter()
            .take_while(|entry| entry.segments[depth] == name)
            .count();
        let (group, next) = rest.split_at(size);
        if first.segments.len() == depth + 1 {
            text.push_str(&format!("{indent}# {}\n", comment_of(first.setting)));
            match &first.line {
                EntryLine::Written(value) => text.push_str(&format!("{indent}{name}: {value}\n")),
                EntryLine::Default(default) => {
                    text.push_str(&format!("{indent}# {name}: {default}\n"))
                }
            }
        } else {
            let mark = if group.iter().any(Entry::is_written) {
                ""
            } else {
                "# "
            };
            text.push_str(&format!("{indent}{mark}{name}:\n"));
            render_group(text, group, depth + 1);
        }
        rest = next;
    }
}

/// The description of a setting followed by the flat key it replaces (R19).
fn comment_of(setting: SettingKey) -> String {
    let description = OPTIONAL_SETTINGS
        .iter()
        .find(|(key, _, _)| *key == setting)
        .map(|(_, _, description)| *description)
        .or_else(|| {
            INSTALLER_SETTINGS
                .iter()
                .find(|(key, _)| *key == setting)
                .map(|(_, description)| *description)
        })
        .unwrap_or_default();
    match setting.former_key() {
        Some(former) => format!("{description} Formerly `{former}`.")
            .trim_start()
            .to_string(),
        None => description.to_string(),
    }
}

/// A value on one line, in YAML flow style.
fn inline_yaml(value: &Value) -> String {
    match value {
        Value::Sequence(items) => format!(
            "[{}]",
            items.iter().map(inline_yaml).collect::<Vec<_>>().join(", ")
        ),
        Value::Mapping(entries) if entries.is_empty() => "{}".to_string(),
        Value::Mapping(entries) => format!(
            "{{ {} }}",
            entries
                .iter()
                .map(|(key, value)| format!("{}: {}", inline_yaml(key), inline_yaml(value)))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        scalar => serde_yaml::to_string(scalar)
            .map(|text| text.trim_end().to_string())
            .unwrap_or_default(),
    }
}

/// Writes `global.root` and the three server directories relative to it,
/// so an init-ed installation keeps `catalog`, `state` and `work` under the
/// root. A setting the environment already gives is left out.
fn set_root_paths(config: &mut Value, root: &Path) -> Result<()> {
    let mapping = config
        .as_mapping_mut()
        .expect("configuration mapping checked");
    for (key, value) in [
        (SettingKey::GlobalRoot, root.to_string_lossy().into_owned()),
        (SettingKey::CatalogDir, "catalog".to_string()),
        (SettingKey::StateDir, "state".to_string()),
        (SettingKey::WorkDir, "work".to_string()),
    ] {
        if environment_setting(key)?.is_none() {
            insert_setting(mapping, key, Value::String(value));
        }
    }
    Ok(())
}

fn configure_server(
    config: &mut Value,
    parsed: &config::FileValues,
    options: &InitOptions,
    non_interactive: bool,
) -> Result<WorkstationBackend> {
    if non_interactive {
        let mut missing = Vec::new();
        for (setting, provided, label) in [
            (
                SettingKey::ServerBind,
                options.server_bind.as_deref(),
                "server-bind",
            ),
            (
                SettingKey::OidcIssuer,
                options.oidc_issuer.as_deref(),
                "oidc-issuer",
            ),
            (
                SettingKey::OidcAudience,
                options.oidc_audience.as_deref(),
                "oidc-audience",
            ),
        ] {
            if provided.is_none() && setting_is_missing(parsed, setting)? {
                missing.push(format!("--{label}"));
            }
        }
        if options.trusted_proxies.is_empty()
            && setting_is_missing(parsed, SettingKey::TrustedProxies)?
        {
            missing.push("--trusted-proxy".to_string());
        }
        if options.backend_id.is_none() {
            missing.push("--backend-id".to_string());
        }
        if !missing.is_empty() {
            return Err(CliError::usage(format!(
                "non-interactive server installation is missing: {}",
                missing.join(", ")
            )));
        }
    }

    let mapping = config
        .as_mapping_mut()
        .expect("configuration mapping checked");
    require_fixed_setting(
        mapping,
        parsed,
        SettingKey::ServerMode,
        "network",
        "server.mode",
    )?;
    require_fixed_setting(
        mapping,
        parsed,
        SettingKey::TlsTermination,
        "proxy",
        "server.tls.termination",
    )?;

    let bind = get_or_prompt(
        parsed,
        SettingKey::ServerBind,
        options.server_bind.as_deref(),
        "server-bind",
        non_interactive,
    )?;
    let bind_value =
        config::file::value_from_text(SettingKey::ServerBind, &bind).map_err(CliError::usage)?;
    reject_loopback_bind(&bind)?;
    let existing_bind = effective_setting(parsed, SettingKey::ServerBind)?;
    if let Some(existing) = &existing_bind {
        if existing != &bind_value {
            return Err(CliError::usage(format!(
                "server-bind '{bind}' contradicts the existing server.bind value '{existing}'"
            )));
        }
    } else {
        insert_text(mapping, SettingKey::ServerBind, &bind);
    }

    for (setting, provided, label) in [
        (
            SettingKey::OidcIssuer,
            options.oidc_issuer.as_deref(),
            "oidc-issuer",
        ),
        (
            SettingKey::OidcAudience,
            options.oidc_audience.as_deref(),
            "oidc-audience",
        ),
    ] {
        let value = get_or_prompt(parsed, setting, provided, label, non_interactive)?;
        let validated = config::file::value_from_text(setting, &value).map_err(CliError::usage)?;
        if value.trim().is_empty() {
            return Err(CliError::usage(format!("--{label} must not be empty")));
        }
        if matches!(setting, SettingKey::OidcIssuer) && !value.starts_with("https://") {
            return Err(CliError::usage("server-mode OIDC issuer must use HTTPS"));
        }
        let current = effective_setting(parsed, setting)?;
        if current
            .as_ref()
            .is_some_and(|existing| existing != &validated)
        {
            return Err(CliError::usage(format!(
                "--{label} contradicts the existing {} value",
                setting.key()
            )));
        }
        if current.is_none() {
            insert_text(mapping, setting, &value);
        }
    }

    let proxy_setting = effective_setting(parsed, SettingKey::TrustedProxies)?;
    let proxies = if options.trusted_proxies.is_empty() {
        proxy_setting
            .as_ref()
            .and_then(SettingValue::as_text)
            .map(config::file::split_list)
    } else {
        Some(options.trusted_proxies.clone())
    };
    let proxies = match proxies {
        Some(proxies) if !proxies.is_empty() => proxies,
        _ if non_interactive => {
            return Err(CliError::usage(
                "non-interactive server installation requires --trusted-proxy (one or more IPs or CIDRs)",
            ))
        }
        _ => config::file::split_list(&prompt_required("Trusted proxy IP or CIDR: ", "trusted-proxy")?),
    };
    let proxy_text = proxies.join(",");
    let proxy_value = config::file::value_from_text(SettingKey::TrustedProxies, &proxy_text)
        .map_err(CliError::usage)?;
    if proxy_setting
        .as_ref()
        .is_some_and(|existing| existing != &proxy_value)
    {
        return Err(CliError::usage(
            "--trusted-proxy contradicts the existing server.tls.trusted_proxies setting",
        ));
    }
    if proxy_setting.is_none() {
        let sequence = proxies
            .iter()
            .cloned()
            .map(Value::String)
            .collect::<Vec<_>>();
        insert_setting(
            mapping,
            SettingKey::TrustedProxies,
            Value::Sequence(sequence),
        );
    }

    let backend_id = select_backend_id(options.backend_id.as_deref(), non_interactive)?;
    let catalog_dir = resolved_directory(parsed, SettingKey::CatalogDir)?;
    let backend_path = catalog_dir
        .join("backends")
        .join(format!("{backend_id}.yaml"));
    if backend_path.exists() {
        return Err(CliError::failure(format!(
            "workstation backend file '{}' already exists; choose a different --backend-id",
            backend_path.display()
        )));
    }
    Ok(WorkstationBackend {
        id: backend_id,
        catalog_dir,
    })
}

/// A server directory as the server will use it: a relative value is joined
/// to `global.root`, the way configuration resolution does it.
fn resolved_directory(parsed: &config::FileValues, setting: SettingKey) -> Result<PathBuf> {
    let path_of = |setting: SettingKey| -> Result<PathBuf> {
        Ok(effective_setting(parsed, setting)?
            .unwrap_or_else(|| setting.built_in())
            .as_path()
            .map(Path::to_path_buf)
            .unwrap_or_default())
    };
    let directory = path_of(setting)?;
    if directory.is_absolute() {
        return Ok(directory);
    }
    absolute_path(&path_of(SettingKey::GlobalRoot)?.join(directory))
}

fn require_fixed_setting(
    mapping: &mut Mapping,
    parsed: &config::FileValues,
    setting: SettingKey,
    expected: &str,
    label: &str,
) -> Result<()> {
    let expected_value =
        config::file::value_from_text(setting, expected).map_err(CliError::failure)?;
    let existing = effective_setting(parsed, setting)?;
    if let Some(existing) = existing {
        if existing != expected_value {
            return Err(CliError::usage(format!(
                "server role requires {label}: {expected}; existing value is '{existing}'"
            )));
        }
    } else {
        insert_text(mapping, setting, expected);
    }
    Ok(())
}

fn get_or_prompt(
    parsed: &config::FileValues,
    setting: SettingKey,
    provided: Option<&str>,
    label: &str,
    non_interactive: bool,
) -> Result<String> {
    let existing = effective_setting(parsed, setting)?.map(|value| value.to_string());
    match (provided, existing) {
        (Some(provided), Some(existing)) if provided != existing => Err(CliError::usage(format!(
            "--{label} contradicts the existing {} value '{existing}'",
            setting.key()
        ))),
        (Some(provided), _) => Ok(provided.to_string()),
        (_, Some(existing)) => Ok(existing),
        (None, None) if non_interactive => Err(CliError::usage(format!(
            "non-interactive server installation is missing --{label}"
        ))),
        (None, None) => prompt_required(&format!("{}: ", setting.key()), label),
    }
}

fn setting_is_missing(parsed: &config::FileValues, setting: SettingKey) -> Result<bool> {
    match effective_setting(parsed, setting)? {
        None => Ok(true),
        Some(SettingValue::Text(value)) => Ok(value.trim().is_empty()),
        Some(_) => Ok(false),
    }
}

fn environment_setting(setting: SettingKey) -> Result<Option<SettingValue>> {
    let variable = setting.env_var();
    let Some(value) = env::var(&variable)
        .ok()
        .filter(|value| !value.trim().is_empty())
    else {
        return Ok(None);
    };
    config::file::value_from_text(setting, &value)
        .map(Some)
        .map_err(|message| CliError::failure(format!("environment variable {variable}: {message}")))
}

fn effective_setting(
    parsed: &config::FileValues,
    setting: SettingKey,
) -> Result<Option<SettingValue>> {
    Ok(environment_setting(setting)?.or_else(|| parsed.get(setting).cloned()))
}

fn insert_text(mapping: &mut Mapping, setting: SettingKey, value: &str) {
    insert_setting(mapping, setting, Value::String(value.to_string()));
}

/// Writes one setting at its place in the tree, creating the sections and
/// groups on the way.
fn insert_setting(mapping: &mut Mapping, setting: SettingKey, value: Value) {
    let segments: Vec<&str> = setting.segments().collect();
    let (leaf, groups) = segments.split_last().expect("a path has a leaf");
    let mut node = mapping;
    for group in groups {
        let entry = node
            .entry(Value::String((*group).to_string()))
            .or_insert_with(|| Value::Mapping(Mapping::new()));
        if !entry.is_mapping() {
            *entry = Value::Mapping(Mapping::new());
        }
        node = entry
            .as_mapping_mut()
            .expect("a group was just made a mapping");
    }
    node.insert(Value::String((*leaf).to_string()), value);
}

/// The raw value of one setting in the tree, if the mapping writes it.
fn setting_in(mapping: &Mapping, setting: SettingKey) -> Option<&Value> {
    let mut segments = setting.segments();
    let first = mapping.get(segments.next()?)?;
    segments.try_fold(first, |node, segment| node.as_mapping()?.get(segment))
}

fn reject_loopback_bind(bind: &str) -> Result<()> {
    let (host, _) = bind
        .rsplit_once(':')
        .ok_or_else(|| CliError::usage("server bind must be written as HOST:PORT"))?;
    let host = host.trim_start_matches('[').trim_end_matches(']');
    let loopback = host
        .parse::<std::net::IpAddr>()
        .map(|address| address.is_loopback())
        .unwrap_or_else(|_| host.eq_ignore_ascii_case("localhost"));
    if loopback {
        return Err(CliError::usage(
            "server role cannot bind to loopback; use an address reachable only through the corporate network proxy",
        ));
    }
    Ok(())
}

fn select_backend_id(provided: Option<&str>, non_interactive: bool) -> Result<BackendId> {
    let value = match provided {
        Some(value) => value.to_string(),
        None if non_interactive => {
            return Err(CliError::usage(
                "non-interactive server installation requires --backend-id",
            ))
        }
        None => prompt_with_default("Workstation backend ID [workstation]: ", "workstation")?,
    };
    let id = BackendId::parse(&value).map_err(|error| CliError::usage(error.to_string()))?;
    if id.as_str() == "local" {
        return Err(CliError::usage(
            "the workstation backend ID must not be 'local'; that ID belongs to the trusted local daemon",
        ));
    }
    Ok(id)
}

fn write_workstation_backend(backend: &WorkstationBackend) -> Result<()> {
    let path = backend
        .catalog_dir
        .join("backends")
        .join(format!("{}.yaml", backend.id));
    let contents = "kind: local\ntrust_scope: workstation\nlimits:\n  default_resources: { cpu_millicores: 1000, memory_mib: 2048 }\n";
    write_new_atomic(&path, contents.as_bytes(), 0o644)
}

fn replace_file(path: &Path, contents: &[u8]) -> Result<()> {
    let parent = path.parent().ok_or_else(|| {
        CliError::failure(format!(
            "configuration path '{}' has no parent directory",
            path.display()
        ))
    })?;
    match fs::symlink_metadata(path) {
        Ok(metadata) if !metadata.is_file() => {
            return Err(CliError::failure(format!(
                "'{}' is not a regular file; refusing to replace it",
                path.display()
            )))
        }
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(CliError::failure(format!(
                "cannot inspect '{}': {error}",
                path.display()
            )))
        }
    }
    let temporary = temporary_path(path, "tmp")?;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temporary)
        .map_err(|error| {
            CliError::failure(format!(
                "cannot create temporary file '{}': {error}",
                temporary.display()
            ))
        })?;
    file.write_all(contents)
        .and_then(|()| file.sync_all())
        .map_err(|error| {
            CliError::failure(format!("cannot write '{}': {error}", temporary.display()))
        })?;
    fs::rename(&temporary, path).map_err(|error| {
        CliError::failure(format!("cannot replace '{}': {error}", path.display()))
    })?;
    sync_directory(parent)
}

fn write_new_atomic(path: &Path, contents: &[u8], mode: u32) -> Result<()> {
    let parent = path.parent().ok_or_else(|| {
        CliError::failure(format!("path '{}' has no parent directory", path.display()))
    })?;
    fs::create_dir_all(parent).map_err(|error| {
        CliError::failure(format!(
            "cannot create directory '{}': {error}",
            parent.display()
        ))
    })?;
    let temporary = temporary_path(path, "tmp")?;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(&temporary)
        .map_err(|error| {
            CliError::failure(format!(
                "cannot create temporary file '{}': {error}",
                temporary.display()
            ))
        })?;
    file.write_all(contents)
        .and_then(|()| file.sync_all())
        .map_err(|error| {
            CliError::failure(format!("cannot write '{}': {error}", temporary.display()))
        })?;
    fs::hard_link(&temporary, path).map_err(|error| {
        CliError::failure(format!("cannot create '{}': {error}", path.display()))
    })?;
    fs::remove_file(&temporary).map_err(|error| {
        CliError::failure(format!(
            "cannot remove temporary file '{}': {error}",
            temporary.display()
        ))
    })?;
    sync_directory(parent)
}

fn temporary_path(path: &Path, suffix: &str) -> Result<PathBuf> {
    let parent = path.parent().ok_or_else(|| {
        CliError::failure(format!("path '{}' has no parent directory", path.display()))
    })?;
    let name = path
        .file_name()
        .ok_or_else(|| CliError::failure(format!("path '{}' has no file name", path.display())))?;
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| CliError::failure(format!("system clock error: {error}")))?
        .as_nanos();
    Ok(parent.join(format!(
        "{}.{}.{}.{}",
        name.to_string_lossy(),
        suffix,
        std::process::id(),
        stamp
    )))
}

fn sync_directory(path: &Path) -> Result<()> {
    File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| {
            CliError::failure(format!(
                "cannot flush directory '{}': {error}",
                path.display()
            ))
        })
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn print_server_commands(backend: &WorkstationBackend, config_path: &Path) -> Result<()> {
    println!(
        "\nWorkstation tasks run as the VM account, without a container, and can access files that account can read."
    );
    println!(
        "Start the network server separately: narsor-server --config {}",
        shell_quote(&config_path.to_string_lossy())
    );
    println!(
        "Create the workstation enrollment code (explicit risk consent required):\n  NARSOR_SERVER='https://<corporate-server>' narsor-cli controller enroll-code {} --accept-workstation-risk",
        backend.id
    );
    println!(
        "Start its separate outbound controller process (replace the placeholders):\n  NARSOR_ENROLLMENT_CODE='<code>' narsor-controller local --control-plane 'https://<corporate-server>' --storage {}",
        shell_quote(
            &backend
                .catalog_dir
                .join("workstation-controller")
                .to_string_lossy()
        )
    );
    Ok(())
}

fn prompt_required(label: &str, name: &str) -> Result<String> {
    loop {
        let value = prompt(label)?;
        if !value.trim().is_empty() {
            return Ok(value.trim().to_string());
        }
        eprintln!("{name} is required.");
    }
}

fn prompt_with_default(label: &str, default: &str) -> Result<String> {
    let answer = prompt(label)?;
    Ok(if answer.trim().is_empty() {
        default.to_string()
    } else {
        answer.trim().to_string()
    })
}

fn prompt(label: &str) -> Result<String> {
    eprint!("{label}");
    io::stderr()
        .flush()
        .map_err(|error| CliError::failure(format!("could not display prompt: {error}")))?;
    let mut answer = String::new();
    io::stdin()
        .read_line(&mut answer)
        .map_err(|error| CliError::failure(format!("could not read answer: {error}")))?;
    Ok(answer.trim().to_string())
}

fn absolute_path(path: &Path) -> Result<PathBuf> {
    std::path::absolute(path).map_err(|error| {
        CliError::failure(format!("cannot resolve path '{}': {error}", path.display()))
    })
}

const DEFAULT_DATA_DIR_TEXT: &str = "~/.narsor";

fn default_data_dir() -> Result<PathBuf> {
    Ok(home_dir()?.join(".narsor"))
}

fn default_config_path() -> Result<PathBuf> {
    if let Some(path) = env::var_os("XDG_CONFIG_HOME").filter(|path| !path.is_empty()) {
        return Ok(PathBuf::from(path).join("narsor/config.yaml"));
    }
    Ok(home_dir()?.join(".config/narsor/config.yaml"))
}

fn home_dir() -> Result<PathBuf> {
    env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| CliError::failure("HOME is not set; provide explicit installation paths"))
}

/// Removes the configuration file, its backups and the data directory.
///
/// Directories named by the configuration but outside the data directory are
/// reported and left in place: only the directory `init` itself chooses is
/// treated as owned by Narsor.
pub fn purge(options: PurgeOptions, global_config: Option<&Path>) -> Result<()> {
    let config_path = match (options.config_file.as_deref(), global_config) {
        (Some(explicit), _) => absolute_path(explicit)?,
        (None, Some(global)) => absolute_path(global)?,
        (None, None) => match ConfigCascade::new(None)
            .resolve()
            .map_err(|error| CliError::failure(error.to_string()))?
            .path()
        {
            Some(path) => absolute_path(path)?,
            None => absolute_path(&default_config_path()?)?,
        },
    };
    let data_dir = absolute_path(&options.data_dir.clone().map_or_else(default_data_dir, Ok)?)?;
    let home = home_dir()?;
    if data_dir == Path::new("/") || home.starts_with(&data_dir) {
        return Err(CliError::usage(format!(
            "refusing to remove '{}': it contains the home directory",
            data_dir.display()
        )));
    }

    let mut files = Vec::new();
    match fs::symlink_metadata(&config_path) {
        Ok(metadata) if metadata.file_type().is_file() => files.push(config_path.clone()),
        Ok(_) => {
            return Err(CliError::failure(format!(
                "configuration '{}' is not a regular file; remove it by hand",
                config_path.display()
            )))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(CliError::failure(format!(
                "cannot inspect configuration '{}': {error}",
                config_path.display()
            )))
        }
    }
    if let (Some(parent), Some(name)) = (config_path.parent(), config_path.file_name()) {
        let prefix = format!("{}.bak.", name.to_string_lossy());
        let generated = format!("{}.new", name.to_string_lossy());
        if let Ok(entries) = fs::read_dir(parent) {
            let mut backups = entries
                .filter_map(std::result::Result::ok)
                .filter(|entry| {
                    let name = entry.file_name().to_string_lossy().into_owned();
                    name.starts_with(&prefix) || name == generated
                })
                .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_file()))
                .map(|entry| entry.path())
                .collect::<Vec<_>>();
            backups.sort();
            files.extend(backups);
        }
    }
    let remove_data_dir = match fs::symlink_metadata(&data_dir) {
        Ok(metadata) if metadata.file_type().is_dir() => true,
        Ok(_) => {
            return Err(CliError::failure(format!(
                "data path '{}' is not a directory; remove it by hand",
                data_dir.display()
            )))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => false,
        Err(error) => {
            return Err(CliError::failure(format!(
                "cannot inspect data directory '{}': {error}",
                data_dir.display()
            )))
        }
    };

    let outside = directories_outside(&config_path, &data_dir);
    if files.is_empty() && !remove_data_dir {
        println!("Nothing to remove.");
        return Ok(());
    }
    println!("The following will be removed permanently:");
    for file in &files {
        println!("  {}", file.display());
    }
    if remove_data_dir {
        println!("  {}/ (everything inside)", data_dir.display());
    }
    for directory in &outside {
        println!("Left in place (outside the data directory): {directory}");
    }
    if !options.yes {
        if !io::stdin().is_terminal() {
            return Err(CliError::usage(
                "confirmation needs a terminal; pass --yes to remove without asking",
            ));
        }
        if !matches!(
            prompt("Remove these? [y/N]: ")?
                .to_ascii_lowercase()
                .as_str(),
            "y" | "yes"
        ) {
            return Err(CliError::failure("nothing was removed"));
        }
    }

    for file in &files {
        fs::remove_file(file).map_err(|error| {
            CliError::failure(format!("cannot remove '{}': {error}", file.display()))
        })?;
    }
    if remove_data_dir {
        fs::remove_dir_all(&data_dir).map_err(|error| {
            CliError::failure(format!("cannot remove '{}': {error}", data_dir.display()))
        })?;
    }
    if let Some(parent) = config_path.parent() {
        // Only an emptied directory goes; anything else is not ours.
        let _ = fs::remove_dir(parent);
    }
    println!("Configuration and data removed.");
    Ok(())
}

/// Root directories the configuration names that lie outside `data_dir`.
fn directories_outside(config_path: &Path, data_dir: &Path) -> Vec<String> {
    let Ok(text) = fs::read_to_string(config_path) else {
        return Vec::new();
    };
    let Ok(Value::Mapping(mapping)) = serde_yaml::from_str::<Value>(&text) else {
        return Vec::new();
    };
    let root = setting_in(&mapping, SettingKey::GlobalRoot)
        .and_then(Value::as_str)
        .map(PathBuf::from);
    SettingKey::ROOTS
        .into_iter()
        .filter_map(|key| setting_in(&mapping, key)?.as_str().map(PathBuf::from))
        .filter_map(|path| match &root {
            _ if path.is_absolute() => Some(path),
            Some(root) => absolute_path(&root.join(path)).ok(),
            // Relative to a root the file does not name: not ours to judge.
            None => None,
        })
        .filter(|path| !path.starts_with(data_dir))
        .map(|path| path.display().to_string())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const PATH: &str = "/etc/narsor/config.yaml";

    #[test]
    fn every_setting_is_written_or_listed_as_a_commented_default() {
        for setting in SettingKey::ALL {
            let commented = OPTIONAL_SETTINGS.iter().any(|(key, _, _)| *key == setting);
            let installer = INSTALLER_SETTINGS.iter().any(|(key, _)| *key == setting);
            assert!(
                commented != installer,
                "{setting}: add it to OPTIONAL_SETTINGS (or, if it must not be suggested, to \
                 INSTALLER_SETTINGS) so `narsor-cli init` writes every setting"
            );
        }
    }

    /// The file with the `# ` in front of every key removed, the way a user
    /// switches all defaults on.
    fn uncomment_all(text: &str) -> String {
        text.lines()
            .map(|line| {
                let content = line.trim_start();
                let indent = &line[..line.len() - content.len()];
                match content.strip_prefix("# ") {
                    Some(key_line) if is_key_line(key_line) => format!("{indent}{key_line}\n"),
                    _ => format!("{line}\n"),
                }
            })
            .collect()
    }

    fn is_key_line(text: &str) -> bool {
        text.split_once(':').is_some_and(|(key, rest)| {
            !key.is_empty()
                && key.chars().all(|c| c.is_ascii_lowercase() || c == '_')
                && (rest.is_empty() || rest.starts_with(' '))
        })
    }

    #[test]
    fn local_install_keeps_the_layout_and_every_default_can_be_switched_on() {
        let mut config = Value::Mapping(Mapping::new());
        set_root_paths(&mut config, Path::new("/srv/narsor")).unwrap();
        let (text, written) = validate_config(Path::new(PATH), &config).unwrap();
        assert_eq!(
            written.get(SettingKey::GlobalRoot),
            Some(&SettingValue::Path(PathBuf::from("/srv/narsor")))
        );
        assert_eq!(
            written.get(SettingKey::CatalogDir),
            Some(&SettingValue::Path(PathBuf::from("catalog")))
        );
        assert!(written.get(SettingKey::AgentHarness).is_none());

        let uncommented = uncomment_all(&text);
        for scope in [
            Scope::SERVER,
            Scope::WEB,
            Scope::TUI,
            Scope::CLI,
            Scope::RUNNER,
            Scope::CONTROLLER,
        ] {
            let parsed = config::file::parse(Path::new(PATH), &uncommented, &scope);
            assert!(
                parsed.is_ok(),
                "{scope:?}: {:?}\n{uncommented}",
                parsed.errors
            );
        }
        let all = config::file::parse(Path::new(PATH), &uncommented, &Scope::ALL)
            .into_result()
            .unwrap();
        for (setting, _, _) in OPTIONAL_SETTINGS {
            assert!(all.get(*setting).is_some(), "{setting} is not switched on");
        }
    }

    #[test]
    fn server_role_writes_network_settings_uncommented() {
        let mut config = Value::Mapping(Mapping::new());
        set_root_paths(&mut config, Path::new("/srv/narsor")).unwrap();
        let parsed = validate_config(Path::new(PATH), &config).unwrap().1;
        let options = InitOptions {
            role: Some(InitRole::Server),
            non_interactive: true,
            server_bind: Some("10.0.0.5:8080".to_string()),
            oidc_issuer: Some("https://id.example.com".to_string()),
            oidc_audience: Some("narsor".to_string()),
            trusted_proxies: vec!["10.0.0.1".to_string()],
            backend_id: Some("workstation".to_string()),
            ..InitOptions::default()
        };

        let backend = configure_server(&mut config, &parsed, &options, true).unwrap();
        let (text, written) = validate_config(Path::new(PATH), &config).unwrap();

        assert_eq!(backend.catalog_dir, Path::new("/srv/narsor/catalog"));
        for setting in [
            SettingKey::ServerMode,
            SettingKey::ServerBind,
            SettingKey::OidcIssuer,
            SettingKey::OidcAudience,
            SettingKey::TlsTermination,
            SettingKey::TrustedProxies,
        ] {
            assert!(written.get(setting).is_some(), "{setting} not written");
        }
        let lines: Vec<&str> = text.lines().collect();
        let issuer = lines
            .iter()
            .position(|line| line.trim_start().starts_with("issuer: "))
            .expect("issuer written");
        assert!(lines[issuer - 1].contains("`oidc_issuer`"), "{text}");
    }

    /// A scratch installation: the data and the configuration file under
    /// one temporary directory, removed on drop.
    struct Installation {
        root: PathBuf,
    }

    impl Installation {
        fn new(name: &str) -> Self {
            let root = env::temp_dir().join(format!("narsor-init-{}-{name}", std::process::id()));
            let _ = fs::remove_dir_all(&root);
            fs::create_dir_all(&root).unwrap();
            Self { root }
        }

        fn config(&self) -> PathBuf {
            self.root.join("config.yaml")
        }

        fn catalog(&self) -> PathBuf {
            self.root.join("data/catalog")
        }

        fn init(
            &self,
            role: InitRole,
            local_mode: Option<LocalWorkMode>,
            docker_check: DockerCheck<'_>,
        ) -> Result<()> {
            let options = InitOptions {
                role: Some(role),
                non_interactive: true,
                data_dir: Some(self.root.join("data")),
                config_file: Some(self.config()),
                local_mode,
                ..InitOptions::default()
            };
            run_with(options, None, docker_check)
        }

        /// The local work mode the written file sets, if any.
        fn written_mode(&self, file: &Path) -> Option<SettingValue> {
            let text = fs::read_to_string(file).unwrap();
            config::file::parse(file, &text, &Scope::ALL)
                .into_result()
                .unwrap()
                .get(SettingKey::LocalMode)
                .cloned()
        }
    }

    impl Drop for Installation {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn docker_ready() -> std::result::Result<String, String> {
        Ok("27.0.1".to_string())
    }

    fn docker_missing() -> std::result::Result<String, String> {
        Err("docker not found".to_string())
    }

    fn docker_never_checked() -> std::result::Result<String, String> {
        panic!("Docker must not be checked")
    }

    #[test]
    fn explicit_docker_mode_writes_the_setting_and_a_task_image_the_controller_can_seal() {
        for (case, check) in [
            ("ready", docker_ready as fn() -> _),
            ("unavailable", docker_missing),
        ] {
            let install = Installation::new(&format!("docker-{case}"));

            install
                .init(InitRole::Local, Some(LocalWorkMode::Docker), &check)
                .unwrap();

            assert_eq!(
                install.written_mode(&install.config()),
                Some(SettingValue::Text("docker".to_string())),
                "{case}"
            );
            let text = fs::read_to_string(install.config()).unwrap();
            let all_on = uncomment_all(&text);
            config::file::parse(&install.config(), &all_on, &Scope::ALL)
                .into_result()
                .unwrap_or_else(|errors| panic!("{case}: {errors:?}\n{all_on}"));
            assert!(all_on.contains("home: private"), "{case}: {all_on}");

            let catalog = install.catalog();
            let runtime: narsor_engine::environment::RuntimeCatalog =
                serde_yaml::from_str(&fs::read_to_string(catalog.join("runtime.yaml")).unwrap())
                    .unwrap();
            assert_eq!(runtime.problems(), Vec::<String>::new(), "{case}");
            let task_image = runtime.task_image.expect("task image written");
            let names: Vec<&str> = task_image
                .components
                .iter()
                .map(|component| component.name.as_str())
                .collect();
            assert_eq!(names, ["base-tools", "claude-code"], "{case}");
            assert!(
                task_image.seal(&catalog).is_ok(),
                "{case}: the component scripts are in the catalogue"
            );
            assert!(initial_runtime().contains(&format!("name: {CLAUDE_TOKEN_SECRET}")));
        }
    }

    #[test]
    fn rerun_keeps_the_configuration_and_an_existing_task_image() {
        let install = Installation::new("rerun");
        fs::write(install.config(), "global:\n  root: /elsewhere\n").unwrap();
        fs::create_dir_all(install.catalog()).unwrap();
        let custom = b"task_image:\n  base: alpine:3\n  components: []\n";
        fs::write(install.catalog().join("runtime.yaml"), custom).unwrap();

        install
            .init(InitRole::Local, Some(LocalWorkMode::Docker), &docker_ready)
            .unwrap();

        assert_eq!(
            fs::read(install.catalog().join("runtime.yaml")).unwrap(),
            custom
        );
        assert!(!install.catalog().join("components").exists());
        assert_eq!(
            fs::read_to_string(install.config()).unwrap(),
            "global:\n  root: /elsewhere\n"
        );
        assert_eq!(
            install.written_mode(&install.root.join("config.yaml.new")),
            Some(SettingValue::Text("docker".to_string()))
        );
    }

    #[test]
    fn without_a_chosen_mode_tasks_stay_on_the_host() {
        let install = Installation::new("process");

        install
            .init(InitRole::Local, None, &docker_never_checked)
            .unwrap();

        assert_eq!(install.written_mode(&install.config()), None);
        assert!(!install.catalog().join("runtime.yaml").exists());
    }

    #[test]
    fn docker_mode_is_refused_for_roles_without_the_local_controller() {
        for role in [InitRole::Server, InitRole::Frontend] {
            let install = Installation::new(&format!("refused-{}", role_name(role)));

            let error = install
                .init(role, Some(LocalWorkMode::Docker), &docker_never_checked)
                .unwrap_err();

            assert!(error.message.contains("--local-mode"), "{}", error.message);
            assert!(!install.config().exists());
        }
    }
}
