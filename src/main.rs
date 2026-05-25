//! Theia — portable media server with on-demand H.264/AAC HLS transcoding.
//!
//! Streams media directly with byte-range support, and (via the per-file "H264/AAC"
//! button) transcodes VP9/AV1/Opus or anything else to H.264/AAC HLS on the fly so it
//! plays on devices without VP9/AV1 hardware decode (e.g. an older iPad Pro).
mod auth;
mod config;
mod listing;
mod player;
mod probe;
mod scan;
mod stream;
mod transcode;

use axum::{middleware, routing::get, Router};
use config::{Args, Config};
use clap::Parser;
use std::sync::Arc;
use tower_http::trace::TraceLayer;

/// Shared application state.
#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub probe_cache: probe::ProbeCache,
    pub transcoder: transcode::TranscodeManager,
}

#[tokio::main]
async fn main() {
    let args = Args::parse();
    let config = Arc::new(Config::load(args));

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "theia=info,tower_http=info".into()),
        )
        .init();

    if config.is_segment_on_demand() {
        tracing::info!(
            "transcode_mode = \"segment\": segment-on-demand HLS enabled (experimental, #6) — \
             arbitrary seeks start fast; segments are transcoded on first request."
        );
        if config.uses_fmp4_segments() {
            tracing::warn!(
                "hls_segment_format = \"fmp4\" is not yet supported in segment mode; \
                 using MPEG-TS (.ts) segments."
            );
        }
    }

    if !config.root.exists() {
        std::fs::create_dir_all(&config.root).expect("Failed to create root directory");
    }
    std::fs::create_dir_all(&config.cache_dir).expect("Failed to create cache directory");

    let probe_cache = probe::new_cache();
    let transcoder = transcode::TranscodeManager::new(config.clone(), probe_cache.clone());
    transcode::spawn_sweeper(transcoder.clone());

    let state = AppState {
        config: config.clone(),
        probe_cache,
        transcoder,
    };

    let app = build_app(state);

    let addr: std::net::SocketAddr = ([0, 0, 0, 0], config.port).into();
    println!("Serving {} on port {}", config.root.display(), config.port);
    println!("Cache: {}", config.cache_dir.display());

    serve(addr, app, &config).await;
}

/// Build the application router. Exposed for integration testing (issue #7).
pub fn build_app(state: AppState) -> Router {
    Router::new()
        .route("/login", get(auth::login_handler))
        .route("/", get(listing::directory_handler))
        .route("/meta/:enc", get(probe::meta_handler))
        .route("/play/:enc", get(player::play_handler))
        .route("/playall/:enc", get(listing::playall_handler))
        .route("/stream/:enc", get(stream::stream_handler))
        .route("/hls/:enc/:seg", get(transcode::hls_handler))
        .route("/hls/:enc/ffmpeg.log", get(transcode::ffmpeg_log_handler))
        .route("/status", get(transcode::status_handler))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth::basic_auth_middleware,
        ))
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

#[cfg(feature = "tls")]
async fn serve(addr: std::net::SocketAddr, app: Router, config: &Config) {
    if let (Some(cert), Some(key)) = (&config.tls_cert, &config.tls_key) {
        let tls = axum_server::tls_rustls::RustlsConfig::from_pem_file(cert, key)
            .await
            .expect("Failed to load TLS cert/key");
        println!("HTTPS enabled at https://localhost:{}", config.port);
        axum_server::bind_rustls(addr, tls)
            .serve(app.into_make_service())
            .await
            .unwrap();
    } else {
        serve_plain(addr, app, config.port).await;
    }
}

#[cfg(not(feature = "tls"))]
async fn serve(addr: std::net::SocketAddr, app: Router, config: &Config) {
    if config.tls_cert.is_some() || config.tls_key.is_some() {
        eprintln!("Warning: --tls-cert/--tls-key ignored (build with --features tls).");
    }
    serve_plain(addr, app, config.port).await;
}

async fn serve_plain(addr: std::net::SocketAddr, app: Router, port: u16) {
    println!("Server running on http://localhost:{port}");
    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}

/// Router-level integration tests (issue #7).
///
/// These drive the *real* `build_app` router via `tower::oneshot` — exercising
/// the auth middleware and a handler end-to-end without binding a socket or
/// invoking ffmpeg, so they run anywhere. They live in-crate because this is a
/// binary-only crate: an external `tests/` file cannot reach `build_app` /
/// `Config::test_default`. (An ffmpeg-backed smoke test of the HLS path is the
/// remaining stretch goal in #7.)
#[cfg(test)]
mod tests {
    use super::{build_app, AppState};
    use crate::{config::Config, probe, transcode};
    use axum::body::Body;
    use axum::http::{header, Request, StatusCode};
    use base64::Engine;
    use http_body_util::BodyExt;
    use std::sync::Arc;
    use tower::ServiceExt; // for `oneshot`

    fn test_state() -> AppState {
        let root = std::env::temp_dir().join(format!("theia-it-root-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&root);
        let config = Arc::new(Config::test_default(root));
        let probe_cache = probe::new_cache();
        let transcoder = transcode::TranscodeManager::new(config.clone(), probe_cache.clone());
        AppState {
            config,
            probe_cache,
            transcoder,
        }
    }

    fn basic_auth(user: &str, pass: &str) -> String {
        let token = base64::engine::general_purpose::STANDARD.encode(format!("{user}:{pass}"));
        format!("Basic {token}")
    }

    fn get(uri: &str, auth: Option<&str>) -> Request<Body> {
        let mut b = Request::builder().uri(uri);
        if let Some(a) = auth {
            b = b.header(header::AUTHORIZATION, a);
        }
        b.body(Body::empty()).unwrap()
    }

    #[tokio::test]
    async fn unauthenticated_requests_are_rejected() {
        let res = build_app(test_state())
            .oneshot(get("/", None))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
        assert!(res.headers().contains_key("www-authenticate"));
    }

    #[tokio::test]
    async fn login_page_is_exempt_from_auth() {
        let res = build_app(test_state())
            .oneshot(get("/login", None))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn wrong_password_is_rejected() {
        let res = build_app(test_state())
            .oneshot(get("/", Some(&basic_auth("test", "wrong"))))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn authenticated_directory_listing_succeeds() {
        let res = build_app(test_state())
            .oneshot(get("/", Some(&basic_auth("test", "test"))))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);

        let body = res.into_body().collect().await.unwrap().to_bytes();
        let html = String::from_utf8_lossy(&body);
        assert!(
            html.contains("Theia"),
            "authenticated request should render the library page"
        );
    }
}
