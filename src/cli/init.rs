use std::{
    env,
    fs::{self, File, OpenOptions},
    io::{self, IsTerminal, Write},
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use clap::{Args, ValueEnum};
use serde_yaml::{Mapping, Value};

use crate::{
    cli::{
        commands::{CliError, Result},
        prompt,
    },
    config::{self, ConfigCascade, SettingKey, SettingValue},
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
    let role = select_role(options.role, options.non_interactive)?;
    let config_path = select_config_path(
        options.config_file.clone(),
        global_config,
        options.non_interactive,
    )?;
    let keep_existing = config_exists(&config_path)?;
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

    let mut config = Value::Mapping(Mapping::new());
    set_root_paths(&mut config, &data_dir)?;
    let parsed = validate_config(&target, &config)?.1;
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

    let contents = render_config(&validate_config(&target, &config)?.0, &config);
    prepare_config_directory(&target)?;
    if let Some(backend) = &workstation_backend {
        write_workstation_backend(backend)?;
    }
    if keep_existing {
        replace_file(&target, contents.as_bytes())?;
    } else {
        write_new_atomic(&target, contents.as_bytes(), 0o600)?;
    }

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
    Ok(())
}

fn config_exists(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(CliError::failure(format!(
            "cannot inspect configuration '{}': {error}",
            path.display()
        ))),
    }
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

fn validate_config(path: &Path, config: &Value) -> Result<(String, config::FileValues)> {
    let text = serde_yaml::to_string(config)
        .map_err(|error| CliError::failure(format!("cannot encode configuration: {error}")))?;
    let parsed = config::file::parse(path, &text)
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
        SettingKey::AgentTimeout,
        "none",
        "Time limit of an agent action, e.g. 30m, 2h, or none.",
    ),
    (
        SettingKey::AgentArgs,
        "[]",
        "Extra harness arguments, e.g. [--dangerously-skip-permissions]; they win over Narsor's own.",
    ),
    (
        SettingKey::MaxConcurrentTasks,
        "none",
        "Tasks processed at once across all backends (replaces backend limits), or none.",
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
];

/// The generated file: the settings chosen by the installer followed by the
/// remaining ones as commented-out defaults.
fn render_config(yaml: &str, config: &Value) -> String {
    let mapping = config.as_mapping().expect("configuration mapping checked");
    let mut text = String::from(
        "# Narsor configuration generated by `narsor-cli init`.\n\
         # Lines starting with # show the default value; remove the # to change it.\n\
         # Every setting can also be given as a NARSOR_<NAME> environment variable.\n\n",
    );
    text.push_str(yaml);
    text.push_str("\n# Optional settings (defaults shown)\n");
    for (key, default, description) in OPTIONAL_SETTINGS {
        if !mapping.contains_key(Value::String(key.key().to_string())) {
            text.push_str(&format!("\n# {description}\n# {}: {default}\n", key.key()));
        }
    }
    text
}

fn set_root_paths(config: &mut Value, root: &Path) -> Result<()> {
    let mapping = config
        .as_mapping_mut()
        .expect("configuration mapping checked");
    for (key, directory) in [
        (SettingKey::CatalogDir, "catalog"),
        (SettingKey::StateDir, "state"),
        (SettingKey::WorkDir, "work"),
    ] {
        if environment_setting(key)?.is_none() {
            mapping.insert(
                Value::String(key.key().to_string()),
                Value::String(root.join(directory).to_string_lossy().into_owned()),
            );
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
        "server_mode",
    )?;
    require_fixed_setting(
        mapping,
        parsed,
        SettingKey::TlsTermination,
        "proxy",
        "tls_termination",
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
                "server-bind '{bind}' contradicts the existing server_bind value '{existing}'"
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
            "--trusted-proxy contradicts the existing trusted_proxies setting",
        ));
    }
    if proxy_setting.is_none() {
        let sequence = proxies
            .iter()
            .cloned()
            .map(Value::String)
            .collect::<Vec<_>>();
        mapping.insert(
            Value::String(SettingKey::TrustedProxies.key().to_string()),
            Value::Sequence(sequence),
        );
    }

    let backend_id = select_backend_id(options.backend_id.as_deref(), non_interactive)?;
    let catalog_dir = effective_setting(parsed, SettingKey::CatalogDir)?
        .as_ref()
        .and_then(SettingValue::as_path)
        .map(Path::to_path_buf)
        .ok_or_else(|| CliError::failure("catalog_dir is missing after installer configuration"))?;
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
    mapping.insert(
        Value::String(setting.key().to_string()),
        Value::String(value.to_string()),
    );
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
    [
        SettingKey::CatalogDir,
        SettingKey::StateDir,
        SettingKey::WorkDir,
    ]
    .into_iter()
    .filter_map(|key| mapping.get(key.key())?.as_str().map(PathBuf::from))
    .filter(|path| !path.starts_with(data_dir))
    .map(|path| path.display().to_string())
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commented_defaults_are_valid_settings() {
        let mut config = Value::Mapping(Mapping::new());
        config.as_mapping_mut().unwrap().insert(
            Value::String("catalog_dir".into()),
            Value::String("/data/catalog".into()),
        );
        let rendered = render_config("catalog_dir: /data/catalog\n", &config);
        let uncommented: String = rendered
            .lines()
            .filter_map(|line| line.strip_prefix("# "))
            .filter(|line| {
                line.split_once(": ")
                    .is_some_and(|(key, _)| SettingKey::from_key(key).is_some())
            })
            .map(|line| format!("{line}\n"))
            .collect();
        assert!(uncommented.contains("agent_timeout: none"));
        let parsed = config::file::parse(Path::new("config.yaml"), &uncommented).into_result();
        assert!(parsed.is_ok(), "{parsed:?}");
        assert!(config::file::parse(Path::new("config.yaml"), &rendered)
            .into_result()
            .is_ok());
    }
}
