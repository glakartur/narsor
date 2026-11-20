//! What settings exist, what they are worth when nothing says otherwise, and
//! how to name where a value came from.
//!
//! This module is the single source of truth for the configuration surface and
//! it does no input or output at all: the cascade ([`super::cascade`]) finds a
//! file, the parser ([`super::file`]) reads one, and resolution
//! ([`super::resolve`]) layers them — but all three ask *this* module which
//! keys exist, what their environment variables are called, and what the
//! built-in value is.
//!
//! [`Source`] is deliberately a mirror of
//! [`WorkflowOrigin`](crate::workflows::WorkflowOrigin): a variant that carries
//! the concrete thing (a path, a variable name, a flag name), a `label()` for
//! the word in a table, and a `Display` that renders "word (concrete)". R8 is
//! the reason it carries the concrete thing rather than a category: an operator
//! asking where a value came from needs the path of the file that was actually
//! read, not the word "file".

use std::fmt;
use std::path::{Path, PathBuf};

use narsor_protocol::harness::{CLAUDE_CODE, HARNESSES};

/// Extra harness arguments by harness name; each value is a sequence of argv tokens.
pub type AgentArgs = std::collections::BTreeMap<String, Vec<String>>;

/// Environment variable runners may receive from a backend's configuration
/// map: the derived name of `runner.agent_args` (see
/// [`SettingKey::env_var`]; a unit test keeps the two equal).
pub const AGENT_ARGS_ENV: &str = "NARSOR_RUNNER__AGENT_ARGS";

/// Canonical configuration key for a known harness name or id.
pub fn agent_args_key(harness: &str) -> Option<&'static str> {
    HARNESSES
        .iter()
        .find(|known| known.short_name == harness || known.id == harness)
        .map(|known| known.short_name)
}

/// Arguments configured for a harness name or id.
pub fn agent_args_for<'a>(args: &'a AgentArgs, harness: &str) -> &'a [String] {
    agent_args_key(harness)
        .and_then(|key| args.get(key))
        .map_or(&[], Vec::as_slice)
}

/// Prefix of every environment variable that carries a setting.
///
/// `ORCHESTRATOR_ACTION_ID` lives outside this prefix on purpose: it is a
/// contract towards workflow scripts, not a setting (see
/// [`ACTION_ID_ENV_VAR`](crate::engine::actions::ACTION_ID_ENV_VAR)).
pub const ENV_PREFIX: &str = "NARSOR_";

/// What is printed instead of the value of a sensitive setting.
pub const MASK: &str = "********";

/// The logging levels a setting may name, in increasing verbosity.
///
/// A closed set rather than a full `EnvFilter` directive: `RUST_LOG` is the
/// tool for directives, and it wins over this setting anyway (D-K).
pub const LOG_LEVELS: [&str; 6] = [LOG_OFF, "error", "warn", "info", "debug", "trace"];

/// The level that turns logging off entirely.
///
/// Load-bearing rather than cosmetic: it is the value that decides whether a
/// subscriber is installed at all, which is what keeps an unconfigured run
/// silent (D-N).
/// Model tiers from the cheapest to the strongest. A workflow and the
/// conductor name a tier; each harness maps it to a model name.
pub const MODEL_TIERS: [&str; 4] = ["small", "medium", "large", "max"];

/// Efforts from the cheapest to the strongest.
pub const EFFORT_SCALE: [&str; 4] = ["low", "medium", "high", "max"];

pub const LOG_OFF: &str = "off";

/// The variable that carries a full `tracing` filter directive.
///
/// It belongs to the registry rather than to the resolver because this module
/// is where the names of the variables live. It beats `NARSOR_GLOBAL__LOG_LEVEL`
/// because it is the more precise instrument: it takes per-target directives
/// such as `narsor::engine=debug`, where the setting takes a bare level. There
/// is no sense in the sharper tool losing (D-K).
pub const RUST_LOG: &str = "RUST_LOG";

/// How a duration setting spells "no limit at all".
///
/// Writable in the file so that a value inherited from `/etc/narsor/` can be
/// revoked rather than only replaced.
pub const NO_LIMIT: &str = "none";

/// How often the terminal interface re-reads the daemon when nothing says
/// otherwise (KTD16): often enough that a waiting task is noticed without
/// `r`, rarely enough that an idle interface does not hammer the daemon.
pub const DEFAULT_TUI_REFRESH_SECS: u64 = 10;

/// The backend a task goes to when neither its request nor the operator names
/// one: the existing local daemon path (U4's local adapter), which is what an
/// installation without any remote backend has always used.
pub const DEFAULT_BACKEND: &str = "local";

/// Where a local task's tools find HOME when nothing says otherwise: the
/// daemon's own, so a harness runs with the subscription login it has on
/// the console — the way local tasks ran before they went through the
/// runner.
pub const DEFAULT_LOCAL_HOME: &str = "host";

/// The local HOME modes, as `narsor-runner --home` spells them.
pub const LOCAL_HOME_MODES: [&str; 2] = ["host", "private"];

/// The built-in `global.root` (R23): the directory every unconfigured run
/// has always used.
pub const DEFAULT_ROOT: &str = "./.narsor";

/// The server's security mode when nothing says otherwise (R36): the local
/// daemon's loopback-only random-token mode, which is what every
/// installation ran before network mode existed.
pub const SERVER_MODE_TRUSTED: &str = "trusted";

/// The networked mode (R35): OIDC users, one installation owner, TLS
/// terminated by a trusted proxy.
pub const SERVER_MODE_NETWORK: &str = "network";

/// Where the server listens when nothing says otherwise: a loopback port the
/// system picks, so several local daemons never collide.
pub const DEFAULT_SERVER_BIND: &str = "127.0.0.1:0";

/// No TLS policy: only valid in trusted mode, whose socket never leaves the
/// machine.
pub const TLS_TERMINATION_NONE: &str = "none";

/// TLS is terminated by a reverse proxy at one of `server.tls.trusted_proxies`; the
/// server accepts only connections from those addresses that say the client
/// spoke HTTPS.
pub const TLS_TERMINATION_PROXY: &str = "proxy";

/// Where `narsor-web` listens when nothing says otherwise: a fixed loopback
/// port, so the address of the browser interface can be bookmarked.
pub const DEFAULT_WEB_BIND: &str = "127.0.0.1:8484";

/// `web.upstream` naming the local daemon of the state directory, which
/// `narsor-web` discovers or launches like the CLI and the TUI do.
pub const WEB_UPSTREAM_LOCAL: &str = "local";

/// `narsor-web` holds the token of the server itself and gives the browser
/// a session token of its own; only on a loopback address.
pub const WEB_TOKEN_SERVER: &str = "server";

/// `narsor-web` passes the browser's own token to the server unchanged.
pub const WEB_TOKEN_PASSTHROUGH: &str = "passthrough";

// ---------------------------------------------------------------------------
// Sections
// ---------------------------------------------------------------------------

/// A top-level root of the configuration tree: `global` or the service that
/// owns the settings below it.
///
/// A setting belongs to exactly one section, the first segment of its path.
/// A binary reads `global` plus its own sections (see [`super::scope`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Section {
    /// Settings every binary reads.
    Global,
    /// `narsor-server`.
    Server,
    /// `narsor-controller local`.
    Controller,
    /// `narsor-runner`.
    Runner,
    /// The terminal interface (`narsor`, and `narsor-cli tui`).
    Tui,
    /// `narsor-web`.
    Web,
    /// `narsor-cli`; it has no setting of its own yet.
    Cli,
}

impl Section {
    /// Every section, in the order `config show` prints them.
    pub const ALL: [Section; 7] = [
        Section::Global,
        Section::Server,
        Section::Controller,
        Section::Runner,
        Section::Tui,
        Section::Web,
        Section::Cli,
    ];

    /// The root key as it is written in the file.
    pub fn name(self) -> &'static str {
        match self {
            Section::Global => "global",
            Section::Server => "server",
            Section::Controller => "controller",
            Section::Runner => "runner",
            Section::Tui => "tui",
            Section::Web => "web",
            Section::Cli => "cli",
        }
    }

    /// The prefix of the environment variables of this section's settings
    /// (`NARSOR_SERVER__`).
    pub fn env_prefix(self) -> String {
        format!("{ENV_PREFIX}{}__", self.name().to_uppercase())
    }

    /// The section written under this root key, if there is one.
    pub fn from_name(name: &str) -> Option<Section> {
        Section::ALL
            .into_iter()
            .find(|section| section.name() == name)
    }
}

impl fmt::Display for Section {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.name())
    }
}

// ---------------------------------------------------------------------------
// The registry
// ---------------------------------------------------------------------------

/// Keys of the flat layout that were removed rather than moved (R22): no
/// setting replaces them.
pub const REMOVED_FLAT_KEYS: [&str; 2] = ["agent_timeout", "escalation_threshold"];

/// Every setting this version has.
///
/// An enum rather than a table of strings: the compiler then refuses a match
/// that forgets a setting, which is what keeps `config show` complete (R7) as
/// the set grows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SettingKey {
    /// The base directory of an installation; relative server directories
    /// are joined to it. `--root` sets it.
    GlobalRoot,
    /// The logging level, when `RUST_LOG` does not say otherwise.
    LogLevel,
    /// Where the workflow definitions and the repository registry are read
    /// from. Shareable between processes, and mountable read-only. Relative
    /// to `global.root` unless absolute.
    CatalogDir,
    /// Where executions are kept. Separable for mounting, not for sharing.
    /// Relative to `global.root` unless absolute.
    StateDir,
    /// Where task workspaces — the git checkouts — are created. Local
    /// scratch. Relative to `global.root` unless absolute.
    WorkDir,
    /// The agent harness used by an `agent` action that names none itself.
    AgentHarness,
    /// Most tasks processed at once, across all backends; `none` for no limit.
    MaxConcurrentTasks,
    /// The execution backend a new remote task is pinned to when its request
    /// names none (R10). Changing it never moves an existing task.
    DefaultBackend,
    /// The user profile a new remote task uses when its request names none
    /// (R16).
    DefaultProfile,
    /// Host directories a local-backend task may use as its workspace
    /// (absolute paths). Empty refuses every local workspace: a task is
    /// copied into its own storage, so which host directories it may read is
    /// the owner's decision.
    LocalWorkspaceRoots,
    /// `trusted` (loopback, random token) or `network` (OIDC, one owner).
    ServerMode,
    /// The address the daemon's API listens on.
    ServerBind,
    /// The OIDC issuer network mode trusts, matched exactly.
    OidcIssuer,
    /// The audience every accepted token must carry.
    OidcAudience,
    /// The OIDC subject that owns the installation (R38); unset means the
    /// first identity presenting the one-time claim code becomes the owner.
    OwnerSubject,
    /// Who terminates TLS in network mode: `proxy` (or `none` in trusted mode).
    TlsTermination,
    /// Addresses (or CIDR ranges) of the TLS-terminating proxies.
    TrustedProxies,
    /// Browser origins allowed to call the API; empty denies every
    /// cross-origin request.
    CorsAllowedOrigins,
    /// Whether the server starts the observer worker and supervises it, with
    /// a key it makes at each start (D5).
    ObserverEnabled,
    /// The API's URL the observer worker and its scripts use; empty is the
    /// server's own listening address, which network mode cannot use (its
    /// requests must come through the TLS proxy).
    ObserverServerUrl,
    /// The observer worker's secrets directory: one file per secret name,
    /// flat; an automation's script gets only the secrets the automation
    /// declares. Empty: no secrets.
    ObserverSecretsDir,
    /// The system user observer scripts run as; must not be the server's.
    ObserverScriptUser,
    /// `true` runs observer scripts as the server's own user, without
    /// isolation: for a trial only.
    ObserverInsecureSameUser,
    /// Where HOME points for a local task's tools: `host` (the daemon's own,
    /// harness logins included) or `private` (on the task storage). Read on
    /// every runner start.
    LocalHome,
    /// `codex`'s configuration directory for local tasks (`CODEX_HOME`);
    /// empty leaves it to the harness.
    LocalCodexHome,
    /// Claude Code's configuration directory for local tasks
    /// (`CLAUDE_CONFIG_DIR`); empty leaves it to the harness.
    LocalClaudeConfigDir,
    /// The Claude Code profile local tasks bring their configuration
    /// directory up to date with before every start; empty, or the
    /// configuration directory itself, copies nothing.
    LocalClaudeProfileSource,
    /// Extra command-line arguments of the harness runs, listed by harness
    /// (`claude: [--flag]`). They win over the same arguments Narsor works
    /// out itself.
    AgentArgs,
    /// The model tier (`small`, `medium`, `large`, `max`) the conductor runs
    /// on; empty is the harness's own default.
    ConductorModel,
    /// The effort (`low`, `medium`, `high`, `max`) the conductor runs with;
    /// empty is the harness's own default.
    ConductorEffort,
    /// How often the terminal interface re-reads the daemon on its own
    /// (KTD16), on top of the refreshes the live stream triggers.
    TuiRefreshInterval,
    /// The address `narsor-web` (the browser interface) listens on.
    WebBind,
    /// The server `narsor-web` passes the API to: `local` (the local daemon)
    /// or the server's URL.
    WebUpstream,
    /// Whose token `narsor-web` sends to the server: `server` (its own) or
    /// `passthrough` (the browser's).
    WebToken,
}

impl SettingKey {
    /// Every setting, in the order `config show` prints them: grouped by
    /// section, in the order of [`Section::ALL`].
    pub const ALL: [SettingKey; 34] = [
        SettingKey::GlobalRoot,
        SettingKey::LogLevel,
        SettingKey::CatalogDir,
        SettingKey::StateDir,
        SettingKey::WorkDir,
        SettingKey::AgentHarness,
        SettingKey::MaxConcurrentTasks,
        SettingKey::DefaultBackend,
        SettingKey::DefaultProfile,
        SettingKey::LocalWorkspaceRoots,
        SettingKey::ServerMode,
        SettingKey::ServerBind,
        SettingKey::OidcIssuer,
        SettingKey::OidcAudience,
        SettingKey::OwnerSubject,
        SettingKey::TlsTermination,
        SettingKey::TrustedProxies,
        SettingKey::CorsAllowedOrigins,
        SettingKey::ObserverEnabled,
        SettingKey::ObserverServerUrl,
        SettingKey::ObserverSecretsDir,
        SettingKey::ObserverScriptUser,
        SettingKey::ObserverInsecureSameUser,
        SettingKey::LocalHome,
        SettingKey::LocalCodexHome,
        SettingKey::LocalClaudeConfigDir,
        SettingKey::LocalClaudeProfileSource,
        SettingKey::AgentArgs,
        SettingKey::ConductorModel,
        SettingKey::ConductorEffort,
        SettingKey::TuiRefreshInterval,
        SettingKey::WebBind,
        SettingKey::WebUpstream,
        SettingKey::WebToken,
    ];

    /// The three server directories, which are joined to `global.root` when
    /// they are relative.
    pub const ROOTS: [SettingKey; 3] = [
        SettingKey::CatalogDir,
        SettingKey::StateDir,
        SettingKey::WorkDir,
    ];

    /// The path of the setting in the tree, segments joined by `.`.
    ///
    /// The one place a path is spelled: the segments, the section, the
    /// environment variable and every message derive from it.
    pub fn key(self) -> &'static str {
        match self {
            SettingKey::GlobalRoot => "global.root",
            SettingKey::LogLevel => "global.log_level",
            SettingKey::CatalogDir => "server.catalog",
            SettingKey::StateDir => "server.state",
            SettingKey::WorkDir => "server.work",
            SettingKey::AgentHarness => "server.agent_harness",
            SettingKey::MaxConcurrentTasks => "server.max_concurrent_tasks",
            SettingKey::DefaultBackend => "server.defaults.backend",
            SettingKey::DefaultProfile => "server.defaults.profile",
            SettingKey::LocalWorkspaceRoots => "server.local_workspace_roots",
            SettingKey::ServerMode => "server.mode",
            SettingKey::ServerBind => "server.bind",
            SettingKey::OidcIssuer => "server.oidc.issuer",
            SettingKey::OidcAudience => "server.oidc.audience",
            SettingKey::OwnerSubject => "server.owner_subject",
            SettingKey::TlsTermination => "server.tls.termination",
            SettingKey::TrustedProxies => "server.tls.trusted_proxies",
            SettingKey::CorsAllowedOrigins => "server.cors_allowed_origins",
            SettingKey::ObserverEnabled => "server.observer.enabled",
            SettingKey::ObserverServerUrl => "server.observer.server_url",
            SettingKey::ObserverSecretsDir => "server.observer.secrets_dir",
            SettingKey::ObserverScriptUser => "server.observer.script_user",
            SettingKey::ObserverInsecureSameUser => "server.observer.insecure_same_user",
            SettingKey::LocalHome => "controller.local.home",
            SettingKey::LocalCodexHome => "controller.local.codex_home",
            SettingKey::LocalClaudeConfigDir => "controller.local.claude_config_dir",
            SettingKey::LocalClaudeProfileSource => "controller.local.claude_profile_source",
            SettingKey::AgentArgs => "runner.agent_args",
            SettingKey::ConductorModel => "runner.conductor.model",
            SettingKey::ConductorEffort => "runner.conductor.effort",
            SettingKey::TuiRefreshInterval => "tui.refresh_interval",
            SettingKey::WebBind => "web.bind",
            SettingKey::WebUpstream => "web.upstream",
            SettingKey::WebToken => "web.token",
        }
    }

    /// The key of the flat layout this setting replaces, `None` for a setting
    /// that had none (`global.root` was only the `--root` flag).
    ///
    /// The one list of the old names: the startup warning about an
    /// un-migrated configuration (R26) and the comments `init` writes (R19)
    /// both read it, so they cannot disagree.
    pub fn former_key(self) -> Option<&'static str> {
        Some(match self {
            SettingKey::GlobalRoot => return None,
            SettingKey::LogLevel => "log_level",
            SettingKey::CatalogDir => "catalog_dir",
            SettingKey::StateDir => "state_dir",
            SettingKey::WorkDir => "work_dir",
            SettingKey::AgentHarness => "agent_harness",
            SettingKey::MaxConcurrentTasks => "max_concurrent_tasks",
            SettingKey::DefaultBackend => "default_backend",
            SettingKey::DefaultProfile => "default_profile",
            SettingKey::LocalWorkspaceRoots => "local_workspace_roots",
            SettingKey::ServerMode => "server_mode",
            SettingKey::ServerBind => "server_bind",
            SettingKey::OidcIssuer => "oidc_issuer",
            SettingKey::OidcAudience => "oidc_audience",
            SettingKey::OwnerSubject => "owner_subject",
            SettingKey::TlsTermination => "tls_termination",
            SettingKey::TrustedProxies => "trusted_proxies",
            SettingKey::CorsAllowedOrigins => "cors_allowed_origins",
            SettingKey::ObserverEnabled => "observer_worker",
            SettingKey::ObserverServerUrl => "observer_worker_server",
            SettingKey::ObserverSecretsDir => "observer_secrets_dir",
            SettingKey::ObserverScriptUser => "observer_script_user",
            SettingKey::ObserverInsecureSameUser => "observer_insecure_same_user",
            SettingKey::LocalHome => "local_home",
            SettingKey::LocalCodexHome => "local_codex_home",
            SettingKey::LocalClaudeConfigDir => "local_claude_config_dir",
            SettingKey::LocalClaudeProfileSource => "local_claude_profile_source",
            SettingKey::AgentArgs => "agent_args",
            SettingKey::ConductorModel => "conductor_model",
            SettingKey::ConductorEffort => "conductor_effort",
            SettingKey::TuiRefreshInterval => "tui_refresh_interval",
            SettingKey::WebBind => "web_bind",
            SettingKey::WebUpstream => "web_upstream",
            SettingKey::WebToken => "web_token",
        })
    }

    /// The segments of the path, from the section down to the leaf.
    pub fn segments(self) -> impl Iterator<Item = &'static str> {
        self.key().split('.')
    }

    /// The section that owns the setting: the first segment of its path.
    pub fn section(self) -> Section {
        let root = self.segments().next().expect("a path has a first segment");
        Section::from_name(root).expect("every path starts with a section")
    }

    /// The setting at this dotted path, if there is one.
    pub fn from_key(key: &str) -> Option<SettingKey> {
        SettingKey::ALL.into_iter().find(|entry| entry.key() == key)
    }

    /// Whether this dotted path names a group of settings (a section or a
    /// mapping inside one) rather than a setting.
    pub fn is_group(path: &str) -> bool {
        SettingKey::ALL.iter().any(|entry| {
            entry
                .key()
                .strip_prefix(path)
                .is_some_and(|rest| rest.starts_with('.'))
        })
    }

    /// The environment variable that carries it: [`ENV_PREFIX`] plus the
    /// upper-cased path segments joined by `__` (`server.oidc.issuer` is
    /// `NARSOR_SERVER__OIDC__ISSUER`).
    ///
    /// Derived mechanically from the path so that the mapping is total (R6)
    /// and so that a new setting cannot arrive without one.
    pub fn env_var(self) -> String {
        let segments: Vec<String> = self.segments().map(str::to_uppercase).collect();
        format!("{ENV_PREFIX}{}", segments.join("__"))
    }

    /// Whether the value is masked when printed.
    ///
    /// No setting is sensitive in this version; the flag exists because
    /// sensitivity has to be an attribute of the entry rather than a rule
    /// guessed from the key's name (R11).
    pub fn is_sensitive(self) -> bool {
        match self {
            SettingKey::GlobalRoot
            | SettingKey::LogLevel
            | SettingKey::CatalogDir
            | SettingKey::StateDir
            | SettingKey::WorkDir
            | SettingKey::AgentHarness
            | SettingKey::MaxConcurrentTasks
            | SettingKey::DefaultBackend
            | SettingKey::DefaultProfile
            | SettingKey::LocalWorkspaceRoots
            | SettingKey::ServerMode
            | SettingKey::ServerBind
            | SettingKey::OidcIssuer
            | SettingKey::OidcAudience
            | SettingKey::OwnerSubject
            | SettingKey::TlsTermination
            | SettingKey::TrustedProxies
            | SettingKey::CorsAllowedOrigins
            | SettingKey::ObserverEnabled
            | SettingKey::ObserverServerUrl
            | SettingKey::ObserverSecretsDir
            | SettingKey::ObserverScriptUser
            | SettingKey::ObserverInsecureSameUser
            | SettingKey::LocalHome
            | SettingKey::LocalCodexHome
            | SettingKey::LocalClaudeConfigDir
            | SettingKey::LocalClaudeProfileSource
            | SettingKey::AgentArgs
            | SettingKey::ConductorModel
            | SettingKey::ConductorEffort
            | SettingKey::TuiRefreshInterval
            | SettingKey::WebBind
            | SettingKey::WebUpstream
            | SettingKey::WebToken => false,
        }
    }

    /// The value in force when nothing else provides one (R1).
    ///
    /// The logging level is `off` rather than `info` on purpose: with
    /// `RUST_LOG` unset today no subscriber is installed and every subcommand
    /// is silent (see `src/main.rs`). A built-in of `info` would turn logging
    /// on for every command at once and make `tui` create a data directory and
    /// a log file on every run — a change of behaviour no requirement asks for.
    ///
    /// The server directories are `.`, the root itself (R23), so an
    /// unconfigured run writes where it always did.
    pub fn built_in(self) -> SettingValue {
        match self {
            SettingKey::GlobalRoot => SettingValue::Path(PathBuf::from(DEFAULT_ROOT)),
            SettingKey::CatalogDir | SettingKey::StateDir | SettingKey::WorkDir => {
                SettingValue::Path(PathBuf::from("."))
            }
            SettingKey::AgentHarness => SettingValue::Text(CLAUDE_CODE.to_string()),
            SettingKey::MaxConcurrentTasks => SettingValue::Text("1".to_string()),
            SettingKey::LogLevel => SettingValue::Text(LOG_OFF.to_string()),
            SettingKey::TuiRefreshInterval => {
                SettingValue::Duration(Some(DEFAULT_TUI_REFRESH_SECS))
            }
            SettingKey::DefaultBackend => SettingValue::Text(DEFAULT_BACKEND.to_string()),
            SettingKey::DefaultProfile => SettingValue::Text("default".to_string()),
            SettingKey::ServerMode => SettingValue::Text(SERVER_MODE_TRUSTED.to_string()),
            SettingKey::ServerBind => SettingValue::Text(DEFAULT_SERVER_BIND.to_string()),
            SettingKey::TlsTermination => SettingValue::Text(TLS_TERMINATION_NONE.to_string()),
            SettingKey::LocalHome => SettingValue::Text(DEFAULT_LOCAL_HOME.to_string()),
            SettingKey::ObserverEnabled | SettingKey::ObserverInsecureSameUser => {
                SettingValue::Text("false".to_string())
            }
            SettingKey::WebBind => SettingValue::Text(DEFAULT_WEB_BIND.to_string()),
            SettingKey::WebUpstream => SettingValue::Text(WEB_UPSTREAM_LOCAL.to_string()),
            SettingKey::WebToken => SettingValue::Text(WEB_TOKEN_SERVER.to_string()),
            // Empty text is "not set": network mode refuses to start without
            // the ones it needs (`crate::auth::SecuritySettings::validate`).
            SettingKey::OidcIssuer
            | SettingKey::OidcAudience
            | SettingKey::OwnerSubject
            | SettingKey::AgentArgs
            | SettingKey::ConductorModel
            | SettingKey::ConductorEffort
            | SettingKey::TrustedProxies
            | SettingKey::CorsAllowedOrigins
            | SettingKey::LocalWorkspaceRoots
            | SettingKey::LocalCodexHome
            | SettingKey::LocalClaudeConfigDir
            | SettingKey::LocalClaudeProfileSource
            | SettingKey::ObserverServerUrl
            | SettingKey::ObserverSecretsDir
            | SettingKey::ObserverScriptUser => SettingValue::Text(String::new()),
        }
    }

    /// Whether the value is a list, which the file may also write as a YAML
    /// sequence and the environment writes comma-separated.
    pub fn is_list(self) -> bool {
        matches!(
            self,
            SettingKey::TrustedProxies
                | SettingKey::CorsAllowedOrigins
                | SettingKey::LocalWorkspaceRoots
        )
    }

    /// Whether the value is a duration, which the file may also write as a
    /// bare number of seconds or as an explicit null.
    pub fn is_duration(self) -> bool {
        matches!(self, SettingKey::TuiRefreshInterval)
    }

    /// Whether the value is a boolean, which the file may also write as a
    /// YAML `true`/`false`.
    pub fn is_bool(self) -> bool {
        matches!(
            self,
            SettingKey::ObserverEnabled | SettingKey::ObserverInsecureSameUser
        )
    }
}

impl fmt::Display for SettingKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.key())
    }
}

// ---------------------------------------------------------------------------
// Values
// ---------------------------------------------------------------------------

/// The value of one setting, in the shape that setting takes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SettingValue {
    /// A filesystem path.
    Path(PathBuf),
    /// A word out of a closed set — a harness name, a logging level.
    Text(String),
    /// A number of seconds, or `None` for "no limit at all".
    Duration(Option<u64>),
}

impl SettingValue {
    /// The path, for the settings that are one.
    pub fn as_path(&self) -> Option<&Path> {
        match self {
            SettingValue::Path(path) => Some(path),
            _ => None,
        }
    }

    /// The text, for the settings that are one.
    pub fn as_text(&self) -> Option<&str> {
        match self {
            SettingValue::Text(text) => Some(text),
            _ => None,
        }
    }

    /// The number of seconds, for the settings that are a duration.
    ///
    /// The outer `Option` says "this is not a duration"; the inner one says
    /// "no limit".
    pub fn as_duration(&self) -> Option<Option<u64>> {
        match self {
            SettingValue::Duration(seconds) => Some(*seconds),
            _ => None,
        }
    }
}

impl fmt::Display for SettingValue {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SettingValue::Path(path) => write!(formatter, "{}", path.display()),
            SettingValue::Text(text) => formatter.write_str(text),
            SettingValue::Duration(Some(seconds)) => write!(formatter, "{seconds}s"),
            SettingValue::Duration(None) => formatter.write_str(NO_LIMIT),
        }
    }
}

/// Renders a value for a human, masking it when the entry says it is sensitive.
///
/// Takes the flag rather than the [`SettingKey`] so that masking stays an
/// attribute of the entry (R11) and stays testable while no real setting is
/// sensitive.
pub fn render_value(value: &SettingValue, sensitive: bool) -> String {
    if sensitive {
        MASK.to_string()
    } else {
        value.to_string()
    }
}

// ---------------------------------------------------------------------------
// Where a value came from
// ---------------------------------------------------------------------------

/// Where the effective value of one setting came from.
///
/// Each variant carries the concrete thing rather than a category, because the
/// question this whole layer answers is "where did *this* value come from" and
/// "file" alone does not answer it (R8).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// A command-line flag, by the name the operator typed.
    Flag(&'static str),
    /// An environment variable, by name.
    Env(String),
    /// The configuration file that won the cascade, by path.
    File(PathBuf),
    /// Nothing provided one; this is the value compiled into the binary.
    BuiltIn,
}

impl Source {
    /// The word printed in the source column.
    pub fn label(&self) -> &'static str {
        match self {
            Source::Flag(_) => "flag",
            Source::Env(_) => "env",
            Source::File(_) => "file",
            Source::BuiltIn => "built-in",
        }
    }

    /// The concrete thing behind the word — `None` for a built-in, which has
    /// nothing to name.
    pub fn detail(&self) -> Option<String> {
        match self {
            Source::Flag(flag) => Some((*flag).to_string()),
            Source::Env(name) => Some(name.clone()),
            Source::File(path) => Some(path.display().to_string()),
            Source::BuiltIn => None,
        }
    }
}

impl fmt::Display for Source {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.detail() {
            Some(detail) => write!(formatter, "{} ({})", self.label(), detail),
            None => formatter.write_str(self.label()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_environment_variable_is_derived_from_the_path_and_distinct() {
        let mut names = Vec::new();
        for key in SettingKey::ALL {
            let variable = key.env_var();
            let expected: Vec<String> = key.segments().map(str::to_uppercase).collect();
            assert_eq!(
                variable,
                format!("{ENV_PREFIX}{}", expected.join("__")),
                "{key}: one `__` between each path segment"
            );
            names.push(variable);
        }
        assert_eq!(
            SettingKey::OidcIssuer.env_var(),
            "NARSOR_SERVER__OIDC__ISSUER"
        );
        assert_eq!(
            SettingKey::AgentArgs.env_var(),
            AGENT_ARGS_ENV,
            "the runner contract variable is the derived name of runner.agent_args"
        );

        names.sort();
        let before = names.len();
        names.dedup();
        assert_eq!(
            before,
            names.len(),
            "two settings must not share a variable"
        );
    }

    #[test]
    fn every_path_starts_with_its_section_and_no_leaf_is_also_a_group() {
        for key in SettingKey::ALL {
            assert_eq!(
                key.segments().next(),
                Some(key.section().name()),
                "{key}: the first segment is the section"
            );
            assert_eq!(SettingKey::from_key(key.key()), Some(key));
            assert!(
                !SettingKey::is_group(key.key()),
                "{key} is a setting and also a group of settings"
            );
            for other in SettingKey::ALL {
                if other != key {
                    assert_ne!(other.key(), key.key(), "{key} is registered twice");
                }
            }
        }
        assert!(SettingKey::is_group("server.oidc"));
        assert!(SettingKey::is_group("server"));
        assert!(!SettingKey::is_group("server.oid"));
    }

    #[test]
    fn the_built_in_directories_keep_todays_layout() {
        // R23: the root is ./.narsor and each server directory is the root.
        assert_eq!(
            SettingKey::GlobalRoot.built_in(),
            SettingValue::Path(PathBuf::from("./.narsor"))
        );
        for directory in SettingKey::ROOTS {
            assert_eq!(
                directory.built_in(),
                SettingValue::Path(PathBuf::from(".")),
                "{directory}"
            );
        }
        // D-N: `info` would turn logging on for every subcommand at once.
        assert_eq!(
            SettingKey::LogLevel.built_in(),
            SettingValue::Text("off".to_string())
        );
    }

    #[test]
    fn a_source_renders_its_concrete_origin_and_a_built_in_renders_one_word() {
        let file = Source::File(PathBuf::from("/etc/narsor/config.yaml"));
        assert_eq!(file.to_string(), "file (/etc/narsor/config.yaml)");
        assert_eq!(Source::BuiltIn.to_string(), "built-in");
        assert_eq!(Source::Flag("--root").to_string(), "flag (--root)");
        assert_eq!(
            Source::Env("NARSOR_SERVER__STATE".to_string()).to_string(),
            "env (NARSOR_SERVER__STATE)"
        );
    }

    #[test]
    fn masking_is_an_attribute_of_the_entry_never_a_guess_about_the_value() {
        let value = SettingValue::Text("token-abc123".to_string());
        assert_eq!(render_value(&value, true), MASK);
        assert_eq!(render_value(&value, false), "token-abc123");
    }

    #[test]
    fn a_duration_renders_seconds_or_the_word_for_no_limit() {
        assert_eq!(SettingValue::Duration(Some(1800)).to_string(), "1800s");
        assert_eq!(SettingValue::Duration(None).to_string(), NO_LIMIT);
    }
}
