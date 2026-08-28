use crate::config::Config;
use crate::metrics::Metrics;
use anyhow::{Context, Result};
use axum::Router;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use std::net::SocketAddr;
use tokio::net::TcpListener;
use tracing::info;

pub async fn serve(config: Config) -> Result<()> {
    let bind: SocketAddr = config.bind.parse().context("KARTERO_BIND")?;
    let app = Router::new()
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        .route("/metrics", get(metrics));

    let listener = TcpListener::bind(bind).await?;
    info!(%bind, "listening");

    let interval = config.interval;
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            ticker.tick().await;
            if let Err(err) = crate::collect::collect_once(&config).await {
                tracing::error!(error = %err, "collect pass failed");
            }
        }
    });

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown())
        .await?;
    Ok(())
}

async fn healthz() -> impl IntoResponse {
    "ok"
}

async fn readyz() -> impl IntoResponse {
    "ok"
}

async fn metrics() -> impl IntoResponse {
    (
        StatusCode::OK,
        [(
            axum::http::header::CONTENT_TYPE,
            "text/plain; version=0.0.4",
        )],
        Metrics::global().encode(),
    )
}

async fn shutdown() {
    let _ = tokio::signal::ctrl_c().await;
}
