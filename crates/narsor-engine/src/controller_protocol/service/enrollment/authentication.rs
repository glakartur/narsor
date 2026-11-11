//! Verifying controllers and runners.

use crate::controller_protocol::credentials::{ControllerIdentity, RunnerIdentity};
use crate::controller_protocol::service::ProtocolService;

impl ProtocolService {
    /// The controller a signed request proves, if any.
    pub fn authenticate_controller(
        &self,
        request: &crate::auth::pop::SignedRequest<'_>,
    ) -> Option<ControllerIdentity> {
        self.controller_auth.authenticate_controller(request)
    }

    /// The runner a bearer token proves, if any.
    pub fn authenticate_runner(&self, bearer: &str) -> Option<RunnerIdentity> {
        self.runner_auth.authenticate_runner(bearer)
    }

    /// Authenticates a runner renewing its credential, which may have
    /// expired within [`ProtocolConfig::runner_renewal_grace`](crate::controller_protocol::service::ProtocolConfig::runner_renewal_grace).
    pub fn authenticate_runner_renewal(&self, bearer: &str) -> Option<RunnerIdentity> {
        self.runner_auth
            .authenticate_runner_renewal(bearer, self.config.runner_renewal_grace)
    }
}
