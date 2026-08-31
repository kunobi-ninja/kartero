use crate::config::Config;
use crate::metrics::Metrics;
use anyhow::{Context, Result};
use axum::Router;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use std::net::SocketAddr;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::net::TcpListener;
use tracing::{info, warn};

pub async fn serve(config: Config) -> Result<()> {
    let bind: SocketAddr = config.bind.parse().context("KARTERO_BIND")?;
    let app = Router::new()
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        .route("/metrics", get(metrics));

    let listener = TcpListener::bind(bind).await?;
    let started_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    info!(%bind, version = crate::VERSION, "started");

    tokio::spawn(heartbeat(
        config.otlp_endpoint.clone(),
        config.heartbeat_interval,
        started_at,
    ));

    let interval = config.interval;
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            ticker.tick().await;
            if let Err(err) = crate::collect::collect_once(&config).await {
                tracing::error!(error = %err, "collect pass failed");
            }
            if let Err(err) = crate::archive::archive_once(&config).await {
                tracing::error!(error = %err, "archive pass failed");
            }
        }
    });

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown())
        .await?;
    Ok(())
}

async fn heartbeat(endpoint: String, interval: Duration, started_at: u64) {
    let started = Instant::now();
    let _ = rustls::crypto::ring::default_provider().install_default();
    let client = match reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(30))
        .build()
    {
        Ok(client) => client,
        Err(err) => {
            warn!(error = %err, "could not build HTTP client for kartero heartbeat");
            return;
        }
    };
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        ticker.tick().await;
        let snapshot = crate::self_telemetry::ProcessSnapshot {
            start_time_unix_s: started_at,
            uptime_s: started.elapsed().as_secs_f64(),
        };
        if let Err(err) = crate::self_telemetry::post_process(&client, &endpoint, &snapshot).await {
            warn!(error = %err, "kartero heartbeat telemetry failed");
        }
    }
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
