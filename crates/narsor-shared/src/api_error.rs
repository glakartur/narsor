//! Shared error values and HTTP response decoding for API clients.

use narsor_protocol::api::RequestErrorResponse;
use serde::de::DeserializeOwned;

#[derive(Debug)]
pub enum ClientError {
    Unavailable(String),
    Unauthorized,
    Forbidden,
    Credential(String),
    NotFound,
    Conflict,
    Refused { code: String, message: String },
    Degraded { code: String, message: String },
    Request { code: String, message: String },
    Server(String),
}

impl std::fmt::Display for ClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unavailable(detail) => write!(f, "API unavailable: {detail}"),
            Self::Unauthorized => write!(f, "API rejected the request: bad token"),
            Self::Forbidden => write!(
                f,
                "API refused the request: not the owner of this installation, or not reached through its trusted proxy"
            ),
            Self::Credential(detail) => write!(f, "no usable API token: {detail}"),
            Self::NotFound => write!(f, "API: not found"),
            Self::Conflict => write!(f, "API: conflict (409)"),
            Self::Refused { code, message } => write!(f, "API refused the request ({code}): {message}"),
            Self::Degraded { code, message } => write!(f, "API could not answer ({code}): {message}"),
            Self::Request { code, message } => write!(f, "API request rejected ({code}): {message}"),
            Self::Server(detail) => write!(f, "API error: {detail}"),
        }
    }
}

impl std::error::Error for ClientError {}

pub async fn handle_json_response<T: DeserializeOwned>(
    response: reqwest::Response,
) -> Result<T, ClientError> {
    use reqwest::StatusCode;

    match response.status() {
        StatusCode::OK | StatusCode::CREATED | StatusCode::ACCEPTED => response
            .json::<T>()
            .await
            .map_err(|error| ClientError::Server(error.to_string())),
        StatusCode::UNAUTHORIZED => Err(ClientError::Unauthorized),
        StatusCode::FORBIDDEN => Err(ClientError::Forbidden),
        StatusCode::NOT_FOUND => Err(ClientError::NotFound),
        StatusCode::CONFLICT => match response.json::<RequestErrorResponse>().await {
            Ok(error) => Err(ClientError::Refused {
                code: error.code,
                message: error.message,
            }),
            Err(_) => Err(ClientError::Conflict),
        },
        StatusCode::SERVICE_UNAVAILABLE => match response.json::<RequestErrorResponse>().await {
            Ok(error) => Err(ClientError::Degraded {
                code: error.code,
                message: error.message,
            }),
            Err(_) => Err(ClientError::Server("unexpected status 503".into())),
        },
        StatusCode::BAD_REQUEST => {
            let error = response
                .json::<RequestErrorResponse>()
                .await
                .map_err(|error| {
                    ClientError::Server(format!("invalid bad-request response: {error}"))
                })?;
            Err(ClientError::Request {
                code: error.code,
                message: error.message,
            })
        }
        status => Err(ClientError::Server(format!("unexpected status {status}"))),
    }
}
