use crate::allowlist::Allowlist;
use crate::artifact::{self, MAX_ZIP_BYTES};
use crate::config::Config;
use crate::github::{ArtifactRef, GitHub, WorkflowRun};
use crate::ledger::{DeliveryKey, DeliveryStatus, Ledger};
use crate::metrics::Metrics;
use crate::otlp::{self, Envelope};
use crate::self_telemetry::{self, CollectSnapshot};
use anyhow::{Context, Result, bail};
use reqwest::StatusCode;
use std::time::{Duration, Instant};
use tracing::{info, warn};

pub async fn collect_once(config: &Config) -> Result<()> {
    let started = Instant::now();
    let mut snapshot = CollectSnapshot::default();
    let result = collect_inner(config, &mut snapshot).await;
    snapshot.ok = result.is_ok();
    snapshot.duration_s = started.elapsed().as_secs_f64();
    Metrics::global().observe_collect(snapshot.duration_s, snapshot.ok);
    emit_self_telemetry(config, &snapshot).await;
    result
}

async fn emit_self_telemetry(config: &Config, snapshot: &CollectSnapshot) {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let Ok(client) = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(30))
        .build()
    else {
        warn!("could not build HTTP client for kartero self-telemetry");
        return;
    };
    if let Err(err) = self_telemetry::post(&client, &config.otlp_endpoint, snapshot).await {
        warn!(error = %err, "kartero self-telemetry failed");
    }
}

async fn collect_inner(config: &Config, snapshot: &mut CollectSnapshot) -> Result<()> {
    let allowlist = Allowlist::load(&config.allowlist_path)?;
    let ledger = Ledger::open(&config.ledger_path)?;
    let github = GitHub::new(config.github.clone())?;
    let metrics = Metrics::global();
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(60))
        .build()?;

    let runs = github.list_completed_runs().await?;
    let mut had_errors = false;
    for run in runs {
        if !github.trusted(&run) {
            continue;
        }
        let artifacts = match github.list_artifacts(run.run_id).await {
            Ok(list) => list,
            Err(err) => {
                warn!(run_id = run.run_id, error = %err, "listing artifacts failed");
                had_errors = true;
                continue;
            }
        };
        for artifact in artifacts {
            if !artifact_name_matches(&artifact.name, &config.artifact_prefix) {
                continue;
            }
            if let Err(err) = ingest_one(
                config, &allowlist, &ledger, &github, &client, metrics, snapshot, &run, &artifact,
            )
            .await
            {
                warn!(
                    run_id = run.run_id,
                    artifact = %artifact.name,
                    error = %err,
                    "ingest failed"
                );
                record_artifact(metrics, snapshot, "retryable");
                had_errors = true;
            }
        }
    }
    if had_errors {
        bail!("one or more artifact operations failed");
    }
    Ok(())
}

fn artifact_name_matches(name: &str, prefix: &str) -> bool {
    name == prefix
        || name
            .strip_prefix(prefix)
            .is_some_and(|rest| rest.starts_with('-'))
}

fn record_artifact(metrics: &Metrics, snapshot: &mut CollectSnapshot, outcome: &str) {
    metrics.inc_artifact(outcome);
    snapshot.inc_artifact(outcome);
}

#[allow(clippy::too_many_arguments)]
async fn ingest_one(
    config: &Config,
    allowlist: &Allowlist,
    ledger: &Ledger,
    github: &GitHub,
    client: &reqwest::Client,
    metrics: &Metrics,
    snapshot: &mut CollectSnapshot,
    run: &WorkflowRun,
    artifact: &ArtifactRef,
) -> Result<()> {
    let key_without_version = |schema_version: u32| DeliveryKey {
        repo_id: run.repo_id,
        run_id: run.run_id,
        attempt: run.attempt,
        artifact_id: artifact.id,
        digest: artifact.digest.clone(),
        schema_version,
    };

    if artifact.size_in_bytes > MAX_ZIP_BYTES as u64 {
        warn!(
            artifact = %artifact.name,
            size = artifact.size_in_bytes,
            "skipping oversized artifact"
        );
        ledger.record(&key_without_version(1), DeliveryStatus::Skipped)?;
        record_artifact(metrics, snapshot, "skipped");
        return Ok(());
    }
    if artifact.expired {
        ledger.record(&key_without_version(1), DeliveryStatus::Skipped)?;
        record_artifact(metrics, snapshot, "skipped");
        return Ok(());
    }

    if ledger.is_terminal(&key_without_version(1))? {
        record_artifact(metrics, snapshot, "skipped");
        return Ok(());
    }

    let zip = github.download_zip(artifact.id).await?;
    let payload = match artifact::open(&zip) {
        Ok(payload) => payload,
        Err(err) => {
            warn!(artifact = %artifact.name, error = %err, "artifact rejected");
            ledger.record(&key_without_version(1), DeliveryStatus::Skipped)?;
            record_artifact(metrics, snapshot, "skipped");
            return Ok(());
        }
    };
    let key = key_without_version(payload.schema_version);
    if ledger.is_terminal(&key)? {
        record_artifact(metrics, snapshot, "skipped");
        return Ok(());
    }
    if payload.schema_version != 1 {
        warn!(
            artifact = %artifact.name,
            version = payload.schema_version,
            "unsupported schema_version"
        );
        ledger.record(&key, DeliveryStatus::Skipped)?;
        record_artifact(metrics, snapshot, "skipped");
        return Ok(());
    }

    let envelope = Envelope {
        pipeline_name: run.workflow_name.clone(),
        repository_url: github.repository_url(),
    };
    let (body, stats) = match otlp::prepare(&payload.metrics_json, allowlist, &envelope) {
        Ok(prepared) => prepared,
        Err(err) => {
            warn!(artifact = %artifact.name, error = %err, "payload rejected");
            ledger.record(&key, DeliveryStatus::Skipped)?;
            record_artifact(metrics, snapshot, "skipped");
            return Ok(());
        }
    };
    metrics.add_dropped("metric", stats.metrics_dropped);
    metrics.add_dropped("point", stats.points_dropped);
    metrics.add_kept(stats.metrics_kept);
    snapshot.metrics_dropped += stats.metrics_dropped;
    snapshot.points_dropped += stats.points_dropped;
    snapshot.metrics_kept += stats.metrics_kept;

    let url = format!("{}/v1/metrics", config.otlp_endpoint.trim_end_matches('/'));
    let response = client
        .post(url)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body(body)
        .send()
        .await
        .context("posting OTLP metrics")?;
    let status = response.status();
    if status.is_success() {
        ledger.record(&key, DeliveryStatus::Delivered)?;
        record_artifact(metrics, snapshot, "delivered");
        info!(
            run_id = run.run_id,
            artifact = %artifact.name,
            kept = stats.metrics_kept,
            "delivered"
        );
        return Ok(());
    }
    if matches!(
        status,
        StatusCode::TOO_MANY_REQUESTS
            | StatusCode::BAD_GATEWAY
            | StatusCode::SERVICE_UNAVAILABLE
            | StatusCode::GATEWAY_TIMEOUT
    ) {
        bail!("OTLP backend returned retryable status {status}");
    }
    let (ledger_status, outcome) = if matches!(
        status,
        StatusCode::BAD_REQUEST | StatusCode::PAYLOAD_TOO_LARGE
    ) {
        (DeliveryStatus::Skipped, "skipped")
    } else {
        (DeliveryStatus::Held, "held")
    };
    ledger.record(&key, ledger_status)?;
    record_artifact(metrics, snapshot, outcome);
    warn!(%status, artifact = %artifact.name, "OTLP backend rejected payload");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::artifact_name_matches;

    #[test]
    fn artifact_prefix_does_not_match_the_next_major_version() {
        assert!(artifact_name_matches(
            "telemetry-otlp-v1-bench-firefox",
            "telemetry-otlp-v1"
        ));
        assert!(artifact_name_matches(
            "telemetry-otlp-v1",
            "telemetry-otlp-v1"
        ));
        assert!(!artifact_name_matches(
            "telemetry-otlp-v10-bench-firefox",
            "telemetry-otlp-v1"
        ));
    }
}
