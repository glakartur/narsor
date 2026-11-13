//! The browser interface of Narsor: a single-page application over the
//! control plane (`/control/v1`), the same API the TUI uses.
//!
//! # What this crate is
//!
//! Only the files of the application and the routes that serve them. The
//! files live in `ui/` and are compiled into the binary ([`include_dir`]),
//! so an installation needs no extra directory and no Node toolchain: the
//! application is plain ES modules, with one vendored library
//! (`ui/vendor/preact-htm.js`).
//!
//! # Where it is served
//!
//! [`routes`] answers `GET /ui/` and `GET /ui/<file>` on the address of the
//! API server itself. The application therefore calls the API on its own
//! origin, and no CORS setting is needed.
//!
//! # Authentication
//!
//! The files are public: they hold no data of the installation. Every call
//! the application makes carries `Authorization: Bearer <token>`, exactly
//! like the TUI and the CLI, and passes the owner gate of the server. The
//! token reaches the application in the fragment of the address
//! (`/ui/#token=...`, which a browser never sends to a server) or from the
//! sign-in form; the application keeps it in the session storage of the tab.
//!
//! # Response headers
//!
//! Each file goes out with a strict content security policy (scripts,
//! styles and connections only from the same origin; no framing), with
//! `nosniff` and without a referrer, so the token cannot leak to another
//! site. `Cache-Control: no-cache` makes a browser revalidate after an
//! upgrade of the binary.

use axum::extract::Path;
use axum::http::{header, HeaderName, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::get;
use axum::Router;
use include_dir::{include_dir, Dir};

/// The application's files, as they are in `ui/`.
static FILES: Dir<'static> = include_dir!("$CARGO_MANIFEST_DIR/ui");

/// The path the application is served under.
pub const UI_PATH: &str = "/ui/";

/// The document the application starts from.
const INDEX: &str = "index.html";

const CONTENT_SECURITY_POLICY: &str = "default-src 'self'; \
    script-src 'self'; style-src 'self'; img-src 'self' data:; \
    connect-src 'self'; object-src 'none'; base-uri 'none'; \
    form-action 'none'; frame-ancestors 'none'";

/// The routes of the browser interface: `/ui/` and everything below it,
/// plus a redirect from `/` and `/ui`.
///
/// They need no state and no authentication; merge them outside the owner
/// gate.
pub fn routes<S>() -> Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    Router::new()
        .route("/", get(redirect_to_ui))
        .route("/ui", get(redirect_to_ui))
        .route(UI_PATH, get(index))
        .route("/ui/{*path}", get(file))
}

async fn redirect_to_ui() -> Redirect {
    Redirect::temporary(UI_PATH)
}

async fn index() -> Response {
    serve(INDEX)
}

async fn file(Path(path): Path<String>) -> Response {
    serve(&path)
}

/// The file at `path`, or a bare `404`.
fn serve(path: &str) -> Response {
    let Some(file) = FILES.get_file(path) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let headers = [
        (header::CONTENT_TYPE, content_type(path)),
        (header::CACHE_CONTROL, "no-cache"),
        (header::CONTENT_SECURITY_POLICY, CONTENT_SECURITY_POLICY),
        (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        (header::REFERRER_POLICY, "no-referrer"),
    ]
    .map(|(name, value): (HeaderName, &'static str)| (name, HeaderValue::from_static(value)));
    (headers, file.contents()).into_response()
}

/// The media type of a file, by its extension.
fn content_type(path: &str) -> &'static str {
    match path.rsplit('.').next().unwrap_or_default() {
        "html" => "text/html; charset=utf-8",
        "js" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "svg" => "image/svg+xml",
        "json" => "application/json",
        "txt" => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    async fn get_path(path: &str) -> Response {
        routes::<()>()
            .oneshot(Request::get(path).body(Body::empty()).unwrap())
            .await
            .unwrap()
    }

    fn header_text<'a>(response: &'a Response, name: &HeaderName) -> &'a str {
        response.headers()[name].to_str().unwrap()
    }

    #[tokio::test]
    async fn serves_the_index_with_the_security_headers() {
        let response = get_path("/ui/").await;

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            header_text(&response, &header::CONTENT_TYPE),
            "text/html; charset=utf-8"
        );
        assert!(
            header_text(&response, &header::CONTENT_SECURITY_POLICY).contains("script-src 'self'")
        );
        assert_eq!(
            header_text(&response, &header::X_CONTENT_TYPE_OPTIONS),
            "nosniff"
        );
        assert_eq!(
            header_text(&response, &header::REFERRER_POLICY),
            "no-referrer"
        );
    }

    #[tokio::test]
    async fn serves_modules_as_javascript() {
        let response = get_path("/ui/js/main.js").await;

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            header_text(&response, &header::CONTENT_TYPE),
            "text/javascript; charset=utf-8"
        );
    }

    #[tokio::test]
    async fn redirects_the_root_to_the_application() {
        for path in ["/", "/ui"] {
            let response = get_path(path).await;

            assert_eq!(response.status(), StatusCode::TEMPORARY_REDIRECT);
            assert_eq!(header_text(&response, &header::LOCATION), UI_PATH);
        }
    }

    #[tokio::test]
    async fn refuses_unknown_files_and_paths_outside_the_application() {
        for path in [
            "/ui/missing.js",
            "/ui/../Cargo.toml",
            "/ui/%2e%2e/Cargo.toml",
        ] {
            assert_eq!(
                get_path(path).await.status(),
                StatusCode::NOT_FOUND,
                "{path}"
            );
        }
    }

    #[test]
    fn every_embedded_file_has_a_known_media_type() {
        fn check(dir: &Dir<'_>) {
            for file in dir.files() {
                let path = file.path().to_str().unwrap();
                assert_ne!(content_type(path), "application/octet-stream", "{path}");
            }
            dir.dirs().for_each(check);
        }
        check(&FILES);
    }
}
