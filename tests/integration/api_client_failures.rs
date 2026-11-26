//! The failure observer of the API client: every failed call is told once,
//! with the cause the client saw, how long the call took and nothing of the
//! request's credentials.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use narsor::api::{ApiClient, CallFailure, ClientError, TokenSource};

/// A server on a free loopback port: `/control/v1/tasks/hang/timeline`
/// never answers, every other path is a `404`.
async fn stub() -> SocketAddr {
    let app = axum::Router::new().route(
        "/control/v1/tasks/hang/timeline",
        axum::routing::get(|| std::future::pending::<&'static str>()),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    address
}

/// A loopback address where nothing listens.
async fn nothing_listening() -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    drop(listener);
    address
}

type Seen = Arc<Mutex<Vec<CallFailure>>>;

fn observed(client: ApiClient) -> (ApiClient, Seen) {
    let seen: Seen = Arc::default();
    let sink = Arc::clone(&seen);
    let client = client.with_failure_observer(move |failure| {
        sink.lock().unwrap().push(failure.clone());
    });
    (client, seen)
}

fn only(seen: &Seen) -> CallFailure {
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 1, "{seen:?}");
    seen[0].clone()
}

#[tokio::test]
async fn a_request_that_times_out_is_told_with_its_cause_and_duration() {
    let (client, seen) = observed(ApiClient::new(stub().await, "token"));

    let error = client.control_timeline("hang").await.unwrap_err();

    assert!(matches!(error, ClientError::Unavailable(_)), "{error}");
    let failure = only(&seen);
    assert_eq!(failure.method, "GET");
    assert_eq!(failure.path, "/control/v1/tasks/hang/timeline");
    assert_eq!(failure.kind, "unavailable");
    assert!(
        failure.detail.to_lowercase().contains("timed out") && failure.detail.contains("[timeout]"),
        "{}",
        failure.detail
    );
    assert!(
        failure.elapsed >= Duration::from_millis(800),
        "{:?}",
        failure.elapsed
    );
}

#[tokio::test]
async fn a_refused_connection_is_told_and_the_caller_gets_the_same_error_as_without_an_observer() {
    let address = nothing_listening().await;
    let plain = ApiClient::new(address, "token");
    let (client, seen) = observed(plain.clone());

    let without = plain.control_tasks().await.unwrap_err();
    let with = client.control_tasks().await.unwrap_err();

    assert!(matches!(with, ClientError::Unavailable(_)), "{with}");
    assert_eq!(with.to_string(), without.to_string());
    let failure = only(&seen);
    assert_eq!(failure.kind, "unavailable");
    assert!(
        failure.detail.to_lowercase().contains("refused") && failure.detail.contains("[connect]"),
        "{}",
        failure.detail
    );
}

#[tokio::test]
async fn a_refusal_is_told_with_its_kind_and_the_path_without_the_query() {
    let (client, seen) = observed(ApiClient::new(stub().await, "token"));

    let error = client.control_events("t1", 5).await.unwrap_err();

    assert!(matches!(error, ClientError::NotFound), "{error}");
    let failure = only(&seen);
    assert_eq!(failure.kind, "not_found");
    assert_eq!(failure.path, "/control/v1/tasks/t1/events");
}

#[tokio::test]
async fn nothing_told_carries_the_token() {
    const TOKEN: &str = "s3cr3t-bearer-token";
    let remote = |address: SocketAddr| {
        ApiClient::remote(
            &format!("http://{address}"),
            TokenSource::Static(TOKEN.to_string()),
        )
        .expect("a loopback client")
    };
    let (refusing, refused) = observed(remote(stub().await));
    let (unreachable, unavailable) = observed(remote(nothing_listening().await));

    let _ = refusing.control_events("t1", 5).await;
    let _ = unreachable.control_tasks().await;

    for failure in [only(&refused), only(&unavailable)] {
        assert!(!format!("{failure:?}").contains(TOKEN), "{failure:?}");
    }
}
