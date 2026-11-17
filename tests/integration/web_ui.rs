//! `narsor-web` in front of a running `narsor-server`: the files of the
//! interface, the proxy of the owner API, and the guards of both token modes.

use std::fs;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use narsor_api_client::api::token::TokenSource;
use narsor_api_client::api::ApiClient;
use narsor_engine::controller_protocol::ProtocolService;
use narsor_engine::orchestration::ControlPlane;
use narsor_engine::persistence::StatePaths;
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
    let control = ProtocolService::new(
        ControlPlane::new(StatePaths::new(&state)),
        crate::environment_support::seeded_resolver(&state),
    );
    start_with_services(
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
    // The client holds the server's real token in both modes, so the test
    // of the passthrough mode proves this token is never lent to a browser.
    let token = server.token().to_string();
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

    // No header of its own: refused, although the client of narsor-web
    // holds the server's token.
    let anonymous = http().get(&tasks).send().await.unwrap();
    assert_eq!(anonymous.status(), StatusCode::UNAUTHORIZED);

    let wrong = http()
        .get(&tasks)
        .bearer_auth("not-the-token")
        .send()
        .await
        .unwrap();
    assert_eq!(wrong.status(), StatusCode::UNAUTHORIZED);

    let owner = http()
        .get(&tasks)
        .bearer_auth(server.token())
        .send()
        .await
        .unwrap();
    assert_eq!(owner.status(), StatusCode::OK);
    server.shutdown().await;
}

/// The text of a file of the interface, as `narsor-web` serves it.
async fn served(base: &str, path: &str) -> String {
    let answer = http().get(format!("{base}{path}")).send().await.unwrap();
    assert_eq!(answer.status(), StatusCode::OK, "{path}");
    answer.text().await.unwrap()
}

#[tokio::test]
async fn the_queues_view_is_served_with_its_route_and_texts() {
    let root = TempRoot::new();
    let server = server(&root).await;
    let base = format!("http://{}", web(&server, TokenMode::Server).await);

    // The page module, reached from the shell and its address (`#/queues`).
    let app = served(&base, "/ui/js/ui/app.js").await;
    assert!(app.contains("from './queues.js'"), "app.js imports the queues view");
    let store = served(&base, "/ui/js/store.js").await;
    assert!(store.contains("#/queues"), "store.js knows the queues route");

    let queues = served(&base, "/ui/js/ui/queues.js").await;
    for text in [
        "Kolejki obserwatorów",
        "+ Nowy obserwator",
        "Zapisz nową wersję",
        "Usunąć obserwatora",
        "ustawienia z CLI (tylko do odczytu)",
        "narsor-cli observer create",
    ] {
        assert!(queues.contains(text), "queues.js lacks {text:?}");
    }
    // AE10: the marker of a newest version the schedule does not run.
    let texts = served(&base, "/ui/js/text.js").await;
    assert!(texts.contains("nie zatwierdzona"), "text.js lacks the marker");
    // R21: the one icon of a task on a queue.
    let list = served(&base, "/ui/js/ui/tasklist.js").await;
    assert!(list.contains("queueEntryMark"), "the task list shows the queue icon");

    let api = served(&base, "/ui/js/api.js").await;
    for route in [
        "/control/v1/observer-queues",
        "'PATCH', `/control/v1/observers/",
        "/scripts/",
        "/approve`",
    ] {
        assert!(api.contains(route), "api.js lacks {route:?}");
    }
    server.shutdown().await;
}

#[tokio::test]
async fn the_observer_routes_go_through_and_a_refusal_keeps_the_servers_reason() {
    let root = TempRoot::new();
    let server = server(&root).await;
    let base = format!("http://{}", web(&server, TokenMode::Server).await);
    let control = |path: &str| format!("{base}/control/v1/{path}");

    let empty: serde_json::Value = http()
        .get(control("observer-queues"))
        .bearer_auth(SESSION)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(empty["queues"], serde_json::json!([]));

    let created = http()
        .post(control("observers"))
        .bearer_auth(SESSION)
        .json(&serde_json::json!({
            "name": "gitlab-mr", "source": "gitlab", "queue": "mr-review",
            "mode": "on_episodes", "interval_secs": 300
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(created.status(), StatusCode::CREATED);
    let saved = http()
        .post(control("observers/gitlab-mr/scripts"))
        .bearer_auth(SESSION)
        .json(&serde_json::json!({ "script": "echo '{}'\n" }))
        .send()
        .await
        .unwrap();
    assert_eq!(saved.status(), StatusCode::CREATED);

    // The approve dialog reads the text before the owner confirms (R32).
    let text: serde_json::Value = http()
        .get(control("observers/gitlab-mr/scripts/1"))
        .bearer_auth(SESSION)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(text["script"], "echo '{}'\n");

    // AE10: saved, not approved; the queue page sees both versions.
    let queue: serde_json::Value = http()
        .get(control("observer-queues/mr-review"))
        .bearer_auth(SESSION)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(queue["observers"][0]["latest_version"], 1);
    assert!(queue["observers"][0].get("approved_version").is_none());

    // A refused create and a refused edit reach the form with the server's
    // code and reason (AE8, AE9); PATCH goes through the proxy.
    let mixed = http()
        .post(control("observers"))
        .bearer_auth(SESSION)
        .json(&serde_json::json!({
            "name": "jira-new", "source": "jira", "queue": "mr-review",
            "mode": "always", "interval_secs": 300
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(mixed.status(), StatusCode::CONFLICT);
    let reason: serde_json::Value = mixed.json().await.unwrap();
    assert_eq!(reason["code"], "queue_mode_mismatch");
    assert!(
        reason["message"].as_str().unwrap().contains("on_episodes"),
        "{reason}"
    );

    let bad_cron = http()
        .patch(control("observers/gitlab-mr"))
        .bearer_auth(SESSION)
        .json(&serde_json::json!({ "cron": "* * * * * *" }))
        .send()
        .await
        .unwrap();
    assert_eq!(bad_cron.status(), StatusCode::BAD_REQUEST);
    let reason: serde_json::Value = bad_cron.json().await.unwrap();
    assert_eq!(reason["code"], "invalid_observer");
    assert!(!reason["message"].as_str().unwrap().is_empty());

    let deleted = http()
        .delete(control("observers/gitlab-mr"))
        .bearer_auth(SESSION)
        .send()
        .await
        .unwrap();
    assert_eq!(deleted.status(), StatusCode::OK);
    server.shutdown().await;
}
