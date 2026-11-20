//! The server's configuration step: the resolved configuration turned into
//! the settings the daemon starts with, or every problem at once (R7, R8).
//!
//! It runs right after resolution, before the daemon touches anything: the
//! rules that span several settings — network mode needs an issuer, an
//! audience and a TLS policy; the observer worker needs a script user or
//! the waiver, and in network mode a URL — are checked here together with
//! the resolution errors. A trusted server also checks the configuration of
//! the local controller it will start (R21), because a broken `controller`
//! section would otherwise surface only once the API is up.

use narsor_engine::auth::{SecurityPlan, SecuritySettings, ServerMode};
use narsor_engine::config::{local_controller, Environment, Problem, Resolved};
use narsor_engine::launch::OwnerDefaults;

use crate::observer_worker::{observer_plan, ObserverPlan};

/// What the daemon starts with, every rule between the settings checked.
#[derive(Debug)]
pub struct ServerSettings {
    /// The security mode, its address and its policies.
    pub security: SecurityPlan,
    /// The observer worker to start, `None` for none.
    pub observer: Option<ObserverPlan>,
    /// The defaults of a task whose request names none.
    pub owner_defaults: OwnerDefaults,
}

impl ServerSettings {
    /// Whether the daemon starts the built-in local controller: a trusted
    /// server does, a network server serves remote backends only.
    pub fn runs_local_controller(&self) -> bool {
        matches!(self.security, SecurityPlan::Trusted { .. })
    }
}

/// The server's settings from `config`, or every problem: the resolution
/// errors, the rules between the settings, and — for a trusted server — the
/// errors of the local controller's configuration as the controller will
/// read it from the same file and the variables of `environment` it gets.
pub fn configure(
    config: &Resolved,
    environment: &Environment,
) -> Result<ServerSettings, Vec<Problem>> {
    let mut problems = Problem::of_resolution(config);

    let security_settings = SecuritySettings::from_config(config);
    let network = security_settings.mode == ServerMode::Network;
    let security = security_settings
        .validate()
        .map_err(|error| problems.extend(error.problems))
        .ok();
    let observer = observer_plan(config, network)
        .map_err(|found| problems.extend(found))
        .ok();
    if !network {
        let controller = local_controller::resolve(config.file().path(), environment);
        for error in controller.errors() {
            let problem = Problem::from(error);
            // A file that does not parse is the server's problem as well,
            // and already listed.
            if !problems.contains(&problem) {
                problems.push(problem.prefixed("local controller: "));
            }
        }
    }

    match (security, observer) {
        (Some(security), Some(observer)) if problems.is_empty() => Ok(ServerSettings {
            security,
            observer,
            owner_defaults: OwnerDefaults::from_config(config),
        }),
        _ => Err(problems),
    }
}
