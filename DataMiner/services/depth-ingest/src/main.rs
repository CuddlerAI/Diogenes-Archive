use std::sync::Arc;

use anyhow::{Context, anyhow};
use dataminer_depth_ingest::auth::DepthTokenVerifier;
use dataminer_depth_ingest::config::ServiceConfig;
use dataminer_depth_ingest::http::SystemClock;
use dataminer_depth_ingest::r2::CloudflareR2Storage;
use dataminer_depth_ingest::repository::FileDepthIndexRepository;
use dataminer_depth_ingest::{DurableDepthIngest, HttpState, build_router};
use tokio::net::TcpListener;
use tracing::info;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let config = ServiceConfig::from_env().context("depth-ingest configuration rejected")?;
    let storage =
        CloudflareR2Storage::new(config.r2).context("independent depth R2 provider rejected")?;
    let repository = FileDepthIndexRepository::new(config.index_directory)
        .context("independent depth index failed to open")?;
    let boundary =
        DurableDepthIngest::open(storage, config.grant_ttl_seconds, Box::new(repository))
            .map_err(|error| anyhow!("depth-ingest state failed to open: {error:?}"))?;
    let auth = DepthTokenVerifier::new(config.auth_hmac_secret.as_bytes())
        .context("independent depth auth rejected")?;
    let state = HttpState::new(
        boundary,
        auth,
        Arc::new(SystemClock),
        config.cleanup_grace_seconds,
    )
    .map_err(|_| anyhow!("depth-ingest HTTP state rejected"))?;
    let listener = TcpListener::bind(config.bind_address)
        .await
        .context("failed to bind depth-ingest HTTP listener")?;
    info!(address = %config.bind_address, "independent depth-ingest listening");
    axum::serve(listener, build_router(state))
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("depth-ingest HTTP server failed")?;
    Ok(())
}

async fn shutdown_signal() {
    if let Err(error) = tokio::signal::ctrl_c().await {
        tracing::error!(%error, "failed to install shutdown signal");
    }
}
