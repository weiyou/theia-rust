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

    let app = Router::new()
        .route("/login", get(auth::login_handler))
        .route("/", get(listing::directory_handler))
        .route("/meta/:enc", get(probe::meta_handler))
        .route("/play/:enc", get(player::play_handler))
        .route("/playall/:enc", get(listing::playall_handler))
        .route("/stream/:enc", get(stream::stream_handler))
        .route("/hls/:enc/:seg", get(transcode::hls_handler))
        .route("/status", get(transcode::status_handler))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth::basic_auth_middleware,
        ))
        .layer(TraceLayer::new_for_http())
        .with_state(state);

    let addr: std::net::SocketAddr = ([0, 0, 0, 0], config.port).into();
    println!("Serving {} on port {}", config.root.display(), config.port);
    println!("Cache: {}", config.cache_dir.display());

    serve(addr, app, &config).await;
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
