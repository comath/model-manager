mod api;
mod config;
mod download;
mod error;
mod gpu;
mod hf;
mod mtls;
mod openai;
mod recipe;
mod runtime;
mod store;

use axum::Router;
use config::Config;
use download::Manager;
use hf::HfClient;
use std::sync::Arc;
use std::time::Duration;
use tower_http::cors::CorsLayer;
use tower_http::services::{ServeDir, ServeFile};
use tower_http::trace::TraceLayer;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "model_manager=info,tower_http=warn".into()),
        )
        .init();

    let cfg = Config::from_env();
    tokio::fs::create_dir_all(cfg.models_dir()).await?;

    let http = reqwest::Client::builder()
        .user_agent(concat!("model-manager/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(20))
        // No overall timeout: model shards take as long as they take.
        .build()?;
    let gpus = gpu::discover().await;
    tracing::info!(count = gpus.len(), "GPU discovery complete");
    let manager =
        Arc::new(Manager::new(cfg.clone(), HfClient::new(http, cfg.hf_token.clone()), gpus));

    let index = cfg.web_dir.join("index.html");
    let app = Router::new()
        .nest("/api", api::router(manager.clone()))
        .nest("/v1", openai::router(manager.clone()))
        // Any non-API path falls through to the SPA so client-side routes work.
        .fallback_service(ServeDir::new(&cfg.web_dir).fallback(ServeFile::new(&index)))
        .layer(TraceLayer::new_for_http())
        .layer(CorsLayer::permissive());

    if let Some(ref ts_bind) = cfg.tailscale_bind {
        let ts_listener = tokio::net::TcpListener::bind(ts_bind).await?;
        tracing::info!(
            "Tailscale unauthenticated HTTP portal listening on http://{}",
            ts_listener.local_addr()?
        );
        let ts_app = app.clone();
        tokio::spawn(async move {
            if let Err(err) = axum::serve(ts_listener, ts_app).await {
                tracing::error!(%err, "Tailscale HTTP listener failed");
            }
        });
    }

    let listener = tokio::net::TcpListener::bind(&cfg.bind).await?;
    let local_addr = listener.local_addr()?;

    if cfg.hf_token.is_none() {
        tracing::info!("no HF_TOKEN set — gated repos will fail to download");
    }

    if cfg.is_tls_enabled() {
        let acceptor = mtls::create_tls_acceptor(&cfg)?;
        if cfg.is_mtls_enabled() {
            tracing::info!(
                "model-manager listening with mTLS authentication on https://{} (models in {})",
                local_addr,
                cfg.models_dir().display()
            );
        } else {
            tracing::info!(
                "model-manager listening with TLS on https://{} (models in {})",
                local_addr,
                cfg.models_dir().display()
            );
        }
        mtls::serve_tls(listener, acceptor, app).await?;
    } else {
        tracing::info!(
            "model-manager listening on http://{} (models in {})",
            local_addr,
            cfg.models_dir().display()
        );
        axum::serve(listener, app).await?;
    }

    Ok(())
}

