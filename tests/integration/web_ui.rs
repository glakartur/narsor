//! `narsor-web` in front of a running `narsor-server`: the files of the
//! interface, the proxy of the owner API, and the guards of both token modes.

use std::fs;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use narsor_api_client::api::token::TokenSource;
use narsor_api_client::api::ApiClient;
use narsor_engine::controller_protocol::ProtocolService;
use narsor_engine::engine::Engine;
use narsor_engine::orchestration::ControlPlane;
use narsor_engine::persistence::{ExecutionStore, StatePaths};
use narsor_server::api::{start_with_services, ApiHandle, ApiServices};
use narsor_web::gateway::{Gateway, Upstream};
use narsor_web::settings::TokenMode;
use reqwest::{redirect, StatusCode};

const SESSION: &str = "session-token-of-the-test";

struct TempRoot(PathBuf);

impl TempRoot {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("narsor-web-ui-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&path).expect("create temp root");
        Self(path)
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

async fn server(root: &TempRoot) -> ApiHandle {
    let catalog = root.0.join("catalog");
    let state = root.0.join("state");
    fs::create_dir_all(&catalog).unwrap();
    fs::create_dir_all(&state).unwrap();
    let engine = Engine::new(ExecutionStore::new(root.0.join("store")));
    let control = ProtocolService::new(
        ControlPlane::new(StatePaths::new(&state)),
        crate::environment_support::seeded_resolver(&state),
    );
    start_with_services(
        Arc::new(engine),
        catalog,
        root.0.join("work"),
        ApiServices {
            control: Some(Arc::new(control)),
            ..ApiServices::default()
        },
    )
    .await
    .expect("start the server")
}

/// `narsor-web` on an ephemeral loopback port in front of `server`.
async fn web(server: &ApiHandle, mode: TokenMode) -> SocketAddr {
    let token = match mode {
        TokenMode::Server => server.token().to_string(),
        TokenMode::Passthrough => String::new(),
    };
    let client = ApiClient::remote(
        &format!("http://{}", server.addr()),
        TokenSource::Static(token),
    )
    .expect("a loopback client");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let session = (mode == TokenMode::Server).then(|| SESSION.to_string());
    let app = Gateway::new(Upstream::fixed(client), mode, session, address.port()).router();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    address
}

fn http() -> reqwest::Client {
    reqwest::Client::builder()
        .redirect(redirect::Policy::none())
        .build()
        .unwrap()
}

#[tokio::test]
async fn the_interface_is_served_with_a_strict_policy() {
    let root = TempRoot::new();
    let server = server(&root).await;
    let base = format!("http://{}", web(&server, TokenMode::Server).await);

    let page = http().get(format!("{base}/ui/")).send().await.unwrap();
    assert_eq!(page.status(), StatusCode::OK);
    let policy = page.headers()["content-security-policy"]
        .to_str()
        .unwrap()
        .to_string();
    assert!(policy.contains("script-src 'self'"), "{policy}");
    assert!(!policy.contains("unsafe-inline"), "{policy}");

    let root_page = http().get(format!("{base}/")).send().await.unwrap();
    assert_eq!(root_page.status(), StatusCode::TEMPORARY_REDIRECT);

    let info: serde_json::Value = http()
        .get(format!("{base}/gateway/v1/info"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(info["token"], "server");
    server.shutdown().await;
}

#[tokio::test]
async fn the_server_mode_needs_the_session_token_and_keeps_the_servers_own() {
    let root = TempRoot::new();
    let server = server(&root).await;
    let tasks = format!(
        "http://{}/control/v1/tasks?archived=false",
        web(&server, TokenMode::Server).await
    );

    let anonymous = http().get(&tasks).send().await.unwrap();
    assert_eq!(anonymous.status(), StatusCode::UNAUTHORIZED);

    // The server's token is not the session token: a browser that somehow
    // learned it still gets nothing through narsor-web.
    let servers = http()
        .get(&tasks)
        .bearer_auth(server.token())
        .send()
        .await
        .unwrap();
    assert_eq!(servers.status(), StatusCode::UNAUTHORIZED);

    let session = http()
        .get(&tasks)
        .bearer_auth(SESSION)
        .send()
        .await
        .unwrap();
    assert_eq!(session.status(), StatusCode::OK);
    let body: serde_json::Value = session.json().await.unwrap();
    assert_eq!(body["tasks"], serde_json::json!([]));
    server.shutdown().await;
}

#[tokio::test]
async fn another_site_and_dns_rebinding_are_refused_in_the_server_mode() {
    let root = TempRoot::new();
    let server = server(&root).await;
    let address = web(&server, TokenMode::Server).await;
    let tasks = format!("http://{address}/control/v1/tasks");

    let rebound = http()
        .get(&tasks)
        .header("host", format!("evil.example:{}", address.port()))
        .bearer_auth(SESSION)
        .send()
        .await
        .unwrap();
    assert_eq!(rebound.status(), StatusCode::FORBIDDEN);

    let cross_site = http()
        .post(format!("http://{address}/control/v1/tasks"))
        .header("origin", "https://evil.example")
        .bearer_auth(SESSION)
        .send()
        .await
        .unwrap();
    assert_eq!(cross_site.status(), StatusCode::FORBIDDEN);
    server.shutdown().await;
}

#[tokio::test]
async fn only_the_routes_of_the_interface_go_through() {
    let root = TempRoot::new();
    let server = server(&root).await;
    let base = format!("http://{}", web(&server, TokenMode::Server).await);

    for path in ["/auth/v1/claim", "/executions", "/control/v1"] {
        let answer = http()
            .get(format!("{base}{path}"))
            .bearer_auth(SESSION)
            .send()
            .await
            .unwrap();
        assert_eq!(answer.status(), StatusCode::NOT_FOUND, "{path}");
    }
    server.shutdown().await;
}

#[tokio::test]
async fn the_passthrough_mode_sends_the_browsers_token_unchanged() {
    let root = TempRoot::new();
    let server = server(&root).await;
    let tasks = format!(
        "http://{}/control/v1/tasks",
        web(&server, TokenMode::Passthrough).await
    );

    let anonymous = http().get(&tasks).send().await.unwrap();
    assert_eq!(anonymous.status(), StatusCode::UNAUTHORIZED);

    let owner = http()
        .get(&tasks)
        .bearer_auth(server.token())
        .send()
        .await
        .unwrap();
    assert_eq!(owner.status(), StatusCode::OK);
    server.shutdown().await;
}
