//! Production adapters from the engine's HTTP identity to generic controller runtime.

use std::path::Path;

use narsor_controller_runtime::{
    load_or_enroll_with, ControllerLink, EnrollmentClient, EnrollmentError,
};
use narsor_protocol::controller::{
    AckResponse, CommandAck, CommandsResponse, HeartbeatResponse, LeaseView, ReportRequest,
    ReportResponse, RunnerCredentialRequest, RunnerCredentialResponse,
};
use narsor_shared::api_error::ClientError;

use crate::auth::ControllerCredential;
use crate::controller_protocol::{ClientOptions, ControllerClient};

impl ControllerLink for ControllerClient {
    async fn acquire_lease(&self, ttl: Option<u64>) -> Result<LeaseView, ClientError> {
        ControllerClient::acquire_lease(self, ttl).await
    }
    async fn renew_lease(&self, epoch: u64, ttl: Option<u64>) -> Result<LeaseView, ClientError> {
        ControllerClient::renew_lease(self, epoch, ttl).await
    }
    async fn release_lease(&self, epoch: u64) -> Result<LeaseView, ClientError> {
        ControllerClient::release_lease(self, epoch).await
    }
    async fn heartbeat(&self) -> Result<HeartbeatResponse, ClientError> {
        ControllerClient::heartbeat(self).await
    }
    async fn commands(&self, wait: u64, all: bool) -> Result<CommandsResponse, ClientError> {
        ControllerClient::commands(self, wait, all).await
    }
    async fn acknowledge(&self, acks: Vec<CommandAck>) -> Result<AckResponse, ClientError> {
        ControllerClient::acknowledge(self, acks).await
    }
    async fn report(&self, report: &ReportRequest) -> Result<ReportResponse, ClientError> {
        ControllerClient::report(self, report).await
    }
    async fn issue_runner_credential(
        &self,
        task: &str,
        request: &RunnerCredentialRequest,
    ) -> Result<RunnerCredentialResponse, ClientError> {
        ControllerClient::issue_runner_credential(self, task, request).await
    }
    async fn issue_reader_credential(
        &self,
        task: &str,
        request: &RunnerCredentialRequest,
    ) -> Result<RunnerCredentialResponse, ClientError> {
        ControllerClient::issue_reader_credential(self, task, request).await
    }
}

struct ProtocolEnrollment<'o> {
    options: &'o ClientOptions,
}

impl EnrollmentClient for ProtocolEnrollment<'_> {
    type Client = ControllerClient;
    fn is_valid_credential(&self, credential: &str) -> bool {
        ControllerCredential::parse(credential).is_some()
    }
    fn client(&self, url: &str, credential: &str) -> Self::Client {
        ControllerClient::new(url, credential).with_options(self.options)
    }
    fn enroll<'a>(
        &'a self,
        url: &'a str,
        code: &'a str,
    ) -> narsor_controller_runtime::EnrollmentFuture<'a, Self::Client> {
        Box::pin(async move {
            let (client, enrolled) = ControllerClient::enroll_with_options(url, code, self.options)
                .await
                .map_err(|e| e.to_string())?;
            let credential = client
                .credential()
                .ok_or_else(|| "enrollment returned no credential".to_string())?;
            tracing::info!(controller_id = %enrolled.controller_id, backend = %enrolled.backend_id, "controller: enrolled");
            Ok((client, credential))
        })
    }
}

/// Loads the controller credential or enrolls and saves one before first use.
pub async fn load_or_enroll(
    url: &str,
    path: &Path,
    code: Option<&str>,
) -> Result<ControllerClient, EnrollmentError> {
    load_or_enroll_with_options(url, path, code, &ClientOptions::default()).await
}

/// [`load_or_enroll`], the client behaving as `options` say — enrollment
/// included (KTD8).
pub async fn load_or_enroll_with_options(
    url: &str,
    path: &Path,
    code: Option<&str>,
    options: &ClientOptions,
) -> Result<ControllerClient, EnrollmentError> {
    load_or_enroll_with(&ProtocolEnrollment { options }, url, path, code).await
}
