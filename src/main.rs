mod api;
mod config;
mod download;
mod error;
mod gpu;
mod hf;
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
        .nest("/api", api::router(manager))
        // Any non-API path falls through to the SPA so client-side routes work.
        .fallback_service(ServeDir::new(&cfg.web_dir).fallback(ServeFile::new(&index)))
        .layer(TraceLayer::new_for_http())
        .layer(CorsLayer::permissive());

    let listener = tokio::net::TcpListener::bind(&cfg.bind).await?;
    tracing::info!(
        "model-manager listening on http://{} (models in {})",
        listener.local_addr()?,
        cfg.models_dir().display()
    );
    if cfg.hf_token.is_none() {
        tracing::info!("no HF_TOKEN set — gated repos will fail to download");
    }
    axum::serve(listener, app).await?;
    Ok(())
}
