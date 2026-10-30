use std::{
    env,
    fs::{self, File, OpenOptions},
    io::{self, IsTerminal, Write},
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use clap::{Args, ValueEnum};
use serde_yaml::{Mapping, Value};

use crate::{
    cli::commands::{CliError, Result},
    config::{self, ConfigCascade, SettingKey, SettingValue},
    domain::BackendId,
};

const ALL_BINARIES: &[&str] = &[
    "narsor",
    "narsor-cli",
    "narsor-server",
    "narsor-controller",
    "narsor-runner",
];

struct WorkstationBackend {
    id: BackendId,
    catalog_dir: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum InstallRole {
    Local,
    Frontend,
    Server,
}

fn role_name(role: InstallRole) -> &'static str {
    match role {
        InstallRole::Local => "local",
        InstallRole::Frontend => "frontend",
        InstallRole::Server => "server",
    }
}

#[derive(Debug, Default, Args)]
pub struct InstallOptions {
    /// What this installation will do.
    #[arg(long, value_enum)]
    role: Option<InstallRole>,

    /// Do not prompt; all role-specific settings must be provided explicitly.
    #[arg(long)]
    non_interactive: bool,

    /// Directory containing the release binaries (defaults to this executable's directory).
    #[arg(long, value_name = "DIRECTORY")]
    source_dir: Option<PathBuf>,

    /// Where to install the selected binaries.
    #[arg(long, value_name = "DIRECTORY")]
    bin_dir: Option<PathBuf>,

    /// Where local state, workspaces, and the catalogue live when not configured already.
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

    /// Also install narsor and narsor-cli in server mode.
    #[arg(long)]
    with_frontend: bool,

    /// Explicitly add the selected binary directory to ~/.bashrc.
    #[arg(long)]
    update_bashrc: bool,
}

pub fn run(options: InstallOptions, global_config: Option<&Path>) -> Result<()> {
    let role = select_role(options.role, options.non_interactive)?;
    let source_dir = match options.source_dir.clone() {
        Some(path) => path,
        None => executable_directory()?,
    };
    let bin_dir = select_directory(
        "Binary installation directory",
        options.bin_dir.clone(),
        default_bin_dir()?,
        options.non_interactive,
    )?;
    let data_dir = select_directory(
        "Narsor data directory",
        options.data_dir.clone(),
        default_data_dir()?,
        options.non_interactive,
    )?;
    let config_path = select_config_path(
        options.config_file.clone(),
        global_config,
        options.non_interactive,
    )?;
    let mut config = load_config(&config_path)?;
    let (_, mut parsed) = validate_config(&config_path, &config)?;

    set_missing_root_paths(&mut config, &parsed, &data_dir)?;
    parsed = validate_config(&config_path, &config)?.1;
    let workstation_backend = if role == InstallRole::Server {
        Some(configure_server(
            &mut config,
            &parsed,
            &options,
            options.non_interactive,
        )?)
    } else {
        None
    };

    let contents = validate_config(&config_path, &config)?.0;
    let binaries = binaries_for(role, options.with_frontend);
    validate_release_binaries(&source_dir, &binaries)?;
    prepare_config_directory(&config_path)?;
    install_binaries(&source_dir, &bin_dir, &binaries)?;
    if let Some(backend) = &workstation_backend {
        write_workstation_backend(backend)?;
    }
    write_config(&config_path, contents.as_bytes())?;
    offer_path_update(&bin_dir, options.non_interactive, options.update_bashrc)?;

    println!(
        "Installed the {} role in '{}'.",
        role_name(role),
        bin_dir.display()
    );
    println!("Configuration: {}", config_path.display());
    if let Some(backend) = &workstation_backend {
        print_server_commands(backend, &config_path)?;
    }
    Ok(())
}

fn select_role(role: Option<InstallRole>, non_interactive: bool) -> Result<InstallRole> {
    if let Some(role) = role {
        return Ok(role);
    }
    if non_interactive {
        return Err(CliError::usage(
            "non-interactive installation requires --role local, frontend, or server",
        ));
    }
    if !io::stdin().is_terminal() {
        return Err(CliError::usage(
            "interactive installation needs a terminal; provide --non-interactive and --role",
        ));
    }
    loop {
        println!("Select an installation role:\n  1. Local\n  2. Frontend\n  3. Server");
        match prompt("Role [1-3]: ")?.as_str() {
            "1" => return Ok(InstallRole::Local),
            "2" => return Ok(InstallRole::Frontend),
            "3" => return Ok(InstallRole::Server),
            _ => eprintln!("Choose 1, 2, or 3."),
        }
    }
}

fn select_directory(
    label: &str,
    selected: Option<PathBuf>,
    default: PathBuf,
    non_interactive: bool,
) -> Result<PathBuf> {
    if let Some(path) = selected {
        return absolute_path(&path);
    }
    if non_interactive {
        return Err(CliError::usage(format!(
            "non-interactive installation requires --{}",
            if label.starts_with("Binary") {
                "bin-dir"
            } else {
                "data-dir"
            }
        )));
    }
    absolute_path(&browse_directory(label, &default)?)
}

fn browse_directory(label: &str, initial: &Path) -> Result<PathBuf> {
    let mut current = initial.to_path_buf();
    while !current.is_dir() {
        if !current.pop() {
            return Err(CliError::failure(format!(
                "cannot find an existing directory above '{}'",
                initial.display()
            )));
        }
    }
    loop {
        println!("\n{label}: {}", current.display());
        println!("  0. Use this directory");
        let parent = current.parent().map(Path::to_path_buf);
        if parent.is_some() {
            println!("  p. Parent directory");
        }
        let mut children = fs::read_dir(&current)
            .map_err(|error| {
                CliError::failure(format!("cannot browse '{}': {error}", current.display()))
            })?
            .map(|entry| {
                entry.map_err(|error| {
                    CliError::failure(format!(
                        "cannot read a directory entry under '{}': {error}",
                        current.display()
                    ))
                })
            })
            .collect::<Result<Vec<_>>>()?;
        children.retain(|entry| entry.path().is_dir());
        children.sort_by_key(|entry| entry.file_name());
        for (index, entry) in children.iter().enumerate() {
            println!("  {}. {}/", index + 1, entry.file_name().to_string_lossy());
        }
        println!("  c. Enter a path");
        let choice = prompt("Choose a directory: ")?;
        match choice.as_str() {
            "0" => return Ok(current),
            "p" if parent.is_some() => current = parent.expect("parent checked above"),
            "c" => {
                let path = PathBuf::from(prompt("Directory path: ")?);
                if path.is_dir() {
                    current = path;
                } else {
                    eprintln!("That path is not an existing directory.");
                }
            }
            value => match value.parse::<usize>() {
                Ok(index) if index > 0 && index <= children.len() => {
                    current = children[index - 1].path();
                }
                _ => eprintln!("Choose one of the listed directories."),
            },
        }
    }
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
    let default = default_config_path()?;
    if non_interactive {
        return absolute_path(&default);
    }
    let directory = browse_directory(
        "Configuration directory (config.yaml will be written here)",
        default.parent().expect("default config has parent"),
    )?;
    absolute_path(&directory.join("config.yaml"))
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

fn load_config(path: &Path) -> Result<Value> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return Err(CliError::failure(format!(
                "configuration '{}' is a symbolic link; refusing to replace it atomically",
                path.display()
            )))
        }
        Ok(metadata) if !metadata.is_file() => {
            return Err(CliError::failure(format!(
                "configuration path '{}' is not a regular file",
                path.display()
            )))
        }
        Ok(_) => Some(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => {
            return Err(CliError::failure(format!(
                "cannot read configuration '{}': {error}",
                path.display()
            )))
        }
    };
    let Some(()) = metadata else {
        return Ok(Value::Mapping(Mapping::new()));
    };
    let bytes = fs::read(path).map_err(|error| {
        CliError::failure(format!(
            "cannot read configuration '{}': {error}",
            path.display()
        ))
    })?;
    let contents = String::from_utf8(bytes).map_err(|_| {
        CliError::failure(format!(
            "configuration '{}' is not UTF-8; it was not changed",
            path.display()
        ))
    })?;
    let _parsed = config::file::parse(path, &contents)
        .into_result()
        .map_err(|errors| {
            CliError::failure(format!(
                "configuration '{}' cannot be safely updated:\n{}",
                path.display(),
                errors
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join("\n")
            ))
        })?;
    let document: Value = serde_yaml::from_str(&contents).map_err(|error| {
        CliError::failure(format!(
            "cannot parse configuration '{}': {error}",
            path.display()
        ))
    })?;
    let mut mapping = match document {
        Value::Null => Mapping::new(),
        Value::Mapping(mapping) => mapping,
        _ => {
            return Err(CliError::failure(format!(
                "configuration '{}' must contain a mapping; it was not changed",
                path.display()
            )))
        }
    };
    Ok(Value::Mapping(std::mem::take(&mut mapping)))
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

fn set_missing_root_paths(
    config: &mut Value,
    parsed: &config::FileValues,
    data_dir: &Path,
) -> Result<()> {
    let mapping = config
        .as_mapping_mut()
        .expect("configuration mapping checked");
    for key in [
        SettingKey::CatalogDir,
        SettingKey::StateDir,
        SettingKey::WorkDir,
    ] {
        if parsed.get(key).is_none() && environment_setting(key)?.is_none() {
            mapping.insert(
                Value::String(key.key().to_string()),
                Value::String(data_dir.to_string_lossy().into_owned()),
            );
        }
    }
    Ok(())
}

fn configure_server(
    config: &mut Value,
    parsed: &config::FileValues,
    options: &InstallOptions,
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
    let contents = "kind: local\ntrust_scope: workstation\nlimits:\n  max_concurrent_tasks: 1\n  default_resources: { cpu_millicores: 1000, memory_mib: 2048 }\n";
    write_new_atomic(&path, contents.as_bytes(), 0o644)
}

fn validate_release_binaries(source_dir: &Path, binaries: &[&str]) -> Result<()> {
    for name in binaries {
        let path = source_dir.join(name);
        let metadata = fs::metadata(&path).map_err(|error| {
            CliError::failure(format!(
                "release binary '{}' is unavailable in '{}': {error}",
                name,
                source_dir.display()
            ))
        })?;
        if !metadata.is_file() || metadata.permissions().mode() & 0o111 == 0 {
            return Err(CliError::failure(format!(
                "release binary '{}' is not an executable file",
                path.display()
            )));
        }
    }
    Ok(())
}

fn install_binaries(source_dir: &Path, bin_dir: &Path, binaries: &[&str]) -> Result<()> {
    fs::create_dir_all(bin_dir).map_err(|error| {
        CliError::failure(format!(
            "cannot create binary directory '{}': {error}",
            bin_dir.display()
        ))
    })?;
    for name in binaries {
        let source = source_dir.join(name);
        let destination = bin_dir.join(name);
        if source == destination {
            continue;
        }
        fs::copy(&source, &destination).map_err(|error| {
            CliError::failure(format!(
                "cannot install '{}' to '{}': {error}",
                source.display(),
                destination.display()
            ))
        })?;
        let mut permissions = fs::metadata(&destination)
            .map_err(|error| {
                CliError::failure(format!(
                    "cannot inspect installed binary '{}': {error}",
                    destination.display()
                ))
            })?
            .permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&destination, permissions).map_err(|error| {
            CliError::failure(format!(
                "cannot make '{}' executable: {error}",
                destination.display()
            ))
        })?;
    }
    Ok(())
}

fn write_config(path: &Path, contents: &[u8]) -> Result<()> {
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
    let existing = match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return Err(CliError::failure(format!(
                "configuration '{}' is a symbolic link; refusing to replace it atomically",
                path.display()
            )))
        }
        Ok(metadata) if !metadata.is_file() => {
            return Err(CliError::failure(format!(
                "configuration path '{}' is not a regular file",
                path.display()
            )))
        }
        Ok(metadata) => Some(metadata),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => {
            return Err(CliError::failure(format!(
                "cannot inspect configuration '{}': {error}",
                path.display()
            )))
        }
    };
    let mode = existing
        .as_ref()
        .map(|metadata| metadata.permissions().mode() & 0o777)
        .unwrap_or(0o600);
    if existing.is_some() {
        create_backup(path, mode)?;
    }
    let temporary = temporary_path(path, "tmp")?;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(&temporary)
        .map_err(|error| {
            CliError::failure(format!(
                "cannot create temporary configuration '{}': {error}",
                temporary.display()
            ))
        })?;
    file.write_all(contents).map_err(|error| {
        CliError::failure(format!(
            "cannot write temporary configuration '{}': {error}",
            temporary.display()
        ))
    })?;
    file.sync_all().map_err(|error| {
        CliError::failure(format!(
            "cannot flush temporary configuration '{}': {error}",
            temporary.display()
        ))
    })?;
    fs::rename(&temporary, path).map_err(|error| {
        CliError::failure(format!(
            "cannot atomically replace configuration '{}': {error}",
            path.display()
        ))
    })?;
    sync_directory(parent)?;
    Ok(())
}

fn create_backup(path: &Path, mode: u32) -> Result<()> {
    let backup = temporary_path(path, "bak")?;
    let mut source = File::open(path).map_err(|error| {
        CliError::failure(format!(
            "cannot read configuration '{}' for backup: {error}",
            path.display()
        ))
    })?;
    let mut destination = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(&backup)
        .map_err(|error| {
            CliError::failure(format!(
                "cannot create configuration backup '{}': {error}",
                backup.display()
            ))
        })?;
    io::copy(&mut source, &mut destination).map_err(|error| {
        CliError::failure(format!("cannot write configuration backup: {error}"))
    })?;
    destination.sync_all().map_err(|error| {
        CliError::failure(format!("cannot flush configuration backup: {error}"))
    })?;
    Ok(())
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

fn offer_path_update(bin_dir: &Path, non_interactive: bool, update_bashrc: bool) -> Result<()> {
    let path_missing =
        !env::split_paths(&env::var_os("PATH").unwrap_or_default()).any(|entry| entry == bin_dir);
    if !path_missing {
        return Ok(());
    }
    let home = home_dir()?;
    let bashrc = home.join(".bashrc");
    let consent = if update_bashrc {
        true
    } else if non_interactive || !io::stdin().is_terminal() {
        false
    } else {
        println!("'{}' is not on PATH.", bin_dir.display());
        matches!(
            prompt("Add it to ~/.bashrc? [y/N]: ")?
                .to_ascii_lowercase()
                .as_str(),
            "y" | "yes"
        )
    };
    if consent {
        add_to_bashrc(&bashrc, bin_dir)?;
        println!("Added '{}' to {}.", bin_dir.display(), bashrc.display());
    } else {
        println!(
            "Add '{}' to PATH in your shell configuration to run Narsor by name.",
            bin_dir.display()
        );
    }
    Ok(())
}

fn add_to_bashrc(path: &Path, bin_dir: &Path) -> Result<()> {
    let mut contents = match fs::read(path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == io::ErrorKind::NotFound => Vec::new(),
        Err(error) => {
            return Err(CliError::failure(format!(
                "cannot read '{}': {error}",
                path.display()
            )))
        }
    };
    let line = format!(
        "export PATH=\"$PATH\":{}",
        shell_quote(&bin_dir.to_string_lossy())
    );
    let text = String::from_utf8_lossy(&contents);
    if text.lines().any(|existing| existing.trim() == line) {
        return Ok(());
    }
    if !contents.is_empty() && !contents.ends_with(b"\n") {
        contents.push(b'\n');
    }
    contents.extend_from_slice(line.as_bytes());
    contents.push(b'\n');
    write_config(path, &contents)
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

fn binaries_for(role: InstallRole, with_frontend: bool) -> Vec<&'static str> {
    match role {
        InstallRole::Local => ALL_BINARIES.to_vec(),
        InstallRole::Frontend => vec!["narsor", "narsor-cli"],
        InstallRole::Server if with_frontend => ALL_BINARIES.to_vec(),
        InstallRole::Server => vec!["narsor-server", "narsor-controller", "narsor-runner"],
    }
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

fn executable_directory() -> Result<PathBuf> {
    let executable = env::current_exe()
        .map_err(|error| CliError::failure(format!("cannot locate narsor-cli: {error}")))?
        .parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| CliError::failure("narsor-cli executable has no parent directory"))?;
    absolute_path(&executable)
}

fn absolute_path(path: &Path) -> Result<PathBuf> {
    std::path::absolute(path).map_err(|error| {
        CliError::failure(format!("cannot resolve path '{}': {error}", path.display()))
    })
}

fn default_bin_dir() -> Result<PathBuf> {
    Ok(home_dir()?.join(".local/bin"))
}

fn default_data_dir() -> Result<PathBuf> {
    if let Some(path) = env::var_os("XDG_DATA_HOME").filter(|path| !path.is_empty()) {
        return Ok(PathBuf::from(path).join("narsor"));
    }
    Ok(home_dir()?.join(".local/share/narsor"))
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
