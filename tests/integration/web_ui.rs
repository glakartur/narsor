//! The browser interface (`narsor-web`) on a running API server: its files
//! are public, every call of the API it makes still passes the owner gate.

use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

use narsor_engine::controller_protocol::ProtocolService;
use narsor_engine::engine::Engine;
use narsor_engine::orchestration::ControlPlane;
use narsor_engine::persistence::{ExecutionStore, StatePaths};
use narsor_server::api::{start_with_services, ApiHandle, ApiServices};
use reqwest::{redirect, StatusCode};

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

async fn serve(root: &TempRoot) -> ApiHandle {
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

fn http() -> reqwest::Client {
    reqwest::Client::builder()
        .redirect(redirect::Policy::none())
        .build()
        .unwrap()
}

#[tokio::test]
async fn the_application_is_served_without_a_token_with_a_strict_policy() {
    let root = TempRoot::new();
    let server = serve(&root).await;
    let base = format!("http://{}", server.addr());

    let page = http().get(format!("{base}/ui/")).send().await.unwrap();
    assert_eq!(page.status(), StatusCode::OK);
    let policy = page.headers()["content-security-policy"]
        .to_str()
        .unwrap()
        .to_string();
    assert!(policy.contains("script-src 'self'"), "{policy}");
    assert!(!policy.contains("unsafe-inline"), "{policy}");
    assert!(page.text().await.unwrap().contains("js/main.js"));

    let module = http()
        .get(format!("{base}/ui/js/main.js"))
        .send()
        .await
        .unwrap();
    assert_eq!(module.status(), StatusCode::OK);
    assert!(module.headers()["content-type"]
        .to_str()
        .unwrap()
        .starts_with("text/javascript"));

    let root_page = http().get(format!("{base}/")).send().await.unwrap();
    assert_eq!(root_page.status(), StatusCode::TEMPORARY_REDIRECT);
    assert_eq!(root_page.headers()["location"], "/ui/");

    server.shutdown().await;
}

#[tokio::test]
async fn the_api_behind_the_application_still_needs_the_owner_token() {
    let root = TempRoot::new();
    let server = serve(&root).await;
    let tasks = format!("http://{}/control/v1/tasks?archived=false", server.addr());

    let anonymous = http().get(&tasks).send().await.unwrap();
    assert_eq!(anonymous.status(), StatusCode::UNAUTHORIZED);

    let owner = http()
        .get(&tasks)
        .bearer_auth(server.token())
        .send()
        .await
        .unwrap();
    assert_eq!(owner.status(), StatusCode::OK);
    let body: serde_json::Value = owner.json().await.unwrap();
    assert_eq!(body["tasks"], serde_json::json!([]));

    server.shutdown().await;
}
