use crate::allowlist::Allowlist;
use crate::artifact::{self, MAX_ZIP_BYTES};
use crate::config::{Config, SourceConfig};
use crate::github::{self, ArtifactRef, GitHub, WorkflowRun};
use crate::ledger::{DeliveryKey, DeliveryStatus, Ledger};
use crate::metrics::Metrics;
use crate::otlp::{self, Envelope};
use crate::self_telemetry::{self, CollectSnapshot, SourceStatus};
use anyhow::{Context, Result, bail};
use reqwest::StatusCode;
use std::time::{Duration, Instant};
use tracing::{error, info, warn};

pub async fn collect_once(config: &Config) -> Result<()> {
    let started = Instant::now();
    let mut snapshot = CollectSnapshot {
        sources: config.sources.len() as u64,
        ..CollectSnapshot::default()
    };
    let result = collect_inner(config, &mut snapshot).await;
    snapshot.ok = result.is_ok();
    snapshot.duration_s = started.elapsed().as_secs_f64();
    Metrics::global().observe_collect(&snapshot);
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
    let metrics = Metrics::global();
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(60))
        .build()?;

    // One source failing must not skip the ones after it. A repository whose
    // token expired would otherwise silently stop collection for every other
    // repository in the same process.
    let mut failed = Vec::new();
    for source in &config.sources {
        let slug = source.slug();
        let result = collect_source(
            config, source, &allowlist, &ledger, &client, metrics, snapshot,
        )
        .await;
        metrics.set_source_up(&slug, result.is_ok());
        snapshot.source_status.push(SourceStatus {
            slug: slug.clone(),
            up: result.is_ok(),
        });
        if let Err(err) = result {
            warn!(source = %slug, error = %err, "collecting source failed");
            failed.push(slug);
        }
    }
    if !failed.is_empty() {
        bail!("collection failed for {}", failed.join(", "));
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn collect_source(
    config: &Config,
    source: &SourceConfig,
    allowlist: &Allowlist,
    ledger: &Ledger,
    client: &reqwest::Client,
    metrics: &Metrics,
    snapshot: &mut CollectSnapshot,
) -> Result<()> {
    let github = GitHub::new(source.clone())?;
    let listed = github.list_completed_runs(config.lookback).await;
    // Recorded whatever the outcome: GitHub reports the expiry on every
    // response that carries the token, and knowing a token is days from
    // lapsing is most useful while it still works.
    record_token_expiry(metrics, snapshot, source, &github);
    let runs = match listed {
        Ok(runs) => runs,
        Err(err) => {
            snapshot.github_errors += 1;
            // A permanent failure gets ERROR and its own metric label. It is
            // not a bad minute, and logging it at the same level as one is how
            // it stays unnoticed: every pass looks like the last, and the
            // artifact counters sit at zero, which is indistinguishable from a
            // repository that has nothing to collect yet.
            if let Some(kind) = github::permanent_kind(&err) {
                metrics.inc_source_listing_failure(&source.slug(), kind);
                snapshot.sources_misconfigured += 1;
                error!(
                    source = %source.slug(),
                    kind,
                    trusted_branch = %source.trusted_branch,
                    error = %err,
                    "source will not recover without a change; check this source's token"
                );
            } else {
                metrics.inc_source_listing_failure(&source.slug(), "other");
                warn!(source = %source.slug(), error = %err, "listing completed GitHub workflow runs failed");
            }
            return Err(err);
        }
    };
    snapshot.runs_seen += runs.len() as u64;
    let mut had_errors = false;
    for run in &runs {
        if !github.trusted(run) {
            continue;
        }
        snapshot.runs_trusted += 1;
        let artifacts = match github.list_artifacts(run.run_id).await {
            Ok(list) => list,
            Err(err) => {
                warn!(source = %source.slug(), run_id = run.run_id, error = %err, "listing artifacts failed");
                snapshot.github_errors += 1;
                had_errors = true;
                continue;
            }
        };
        snapshot.artifacts_seen += artifacts.len() as u64;
        for artifact in artifacts {
            if !github::artifact_name_matches(&artifact.name, &config.artifact_prefix) {
                continue;
            }
            snapshot.artifacts_matched += 1;
            if let Err(err) = ingest_one(
                config, allowlist, ledger, &github, client, metrics, snapshot, run, &artifact,
            )
            .await
            {
                warn!(
                    source = %source.slug(),
                    run_id = run.run_id,
                    artifact = %artifact.name,
                    error = %err,
                    "ingest failed"
                );
                record_artifact(metrics, snapshot, "retryable");
                snapshot.ingest_errors += 1;
                had_errors = true;
            }
        }
    }
    if let Some(actions) = source.actions.as_ref()
        && let Err(err) = derive_actions(
            config, source, actions, allowlist, &runs, ledger, &github, client, snapshot,
        )
        .await
    {
        warn!(source = %source.slug(), error = %err, "deriving CI metrics failed");
        had_errors = true;
    }

    if had_errors {
        bail!("one or more artifact operations failed");
    }
    Ok(())
}

/// Derive and deliver the metrics for every attempt this source has not
/// already reported.
///
/// One request per attempt, sealed only after it is delivered. Doing a whole
/// window in one body would mean a failure that repeats re-sends everything
/// before it on every pass and seals none of it — and these are delta points,
/// so re-sending is not free.
#[allow(clippy::too_many_arguments)]
async fn derive_actions(
    config: &Config,
    source: &SourceConfig,
    actions: &crate::config::ActionsConfig,
    allowlist: &Allowlist,
    runs: &[WorkflowRun],
    ledger: &Ledger,
    github: &GitHub,
    client: &reqwest::Client,
    snapshot: &mut CollectSnapshot,
) -> Result<()> {
    let metrics = Metrics::global();
    for run in runs {
        // Attempt 1 has no predecessor, so a flake cannot be read from it; the
        // pair is fetched only where there is a transition to classify.
        for attempt in 1..=run.attempt {
            if ledger.attempt_is_sealed(run.repo_id, run.run_id, attempt)? {
                continue;
            }
            let jobs = github
                .list_jobs(run.run_id, attempt)
                .await
                .with_context(|| format!("{} run {}", source.slug(), run.run_id))?;
            let mut derived = crate::actions::derive(&run.detail, &jobs, actions);

            if attempt > 1 {
                let previous = github.list_jobs(run.run_id, attempt - 1).await?;
                let flake = crate::actions::flake::derive(
                    &run.detail,
                    &jobs,
                    &run.detail,
                    &previous,
                    actions,
                );
                derived.points.extend(flake.points);
                for anomaly in flake.anomalies {
                    derived.anomalies.push(anomaly);
                }
            }

            // Reported rather than discarded. Every one of these means a point
            // that could have existed does not, and the commonest of them --
            // a job renamed without `canonicalJobs` following -- shows up
            // nowhere else: the series simply stops, which looks the same as a
            // repository nobody pushed to this week.
            for anomaly in &derived.anomalies {
                metrics.inc_anomaly(&source.slug(), anomaly.as_str());
                snapshot.inc_anomaly(&source.slug(), anomaly.as_str());
            }

            if !derived.points.is_empty() {
                let observed = vec![run.observed_at; derived.points.len()];
                let body = crate::actions::emit::to_otlp(&derived.points, &observed);
                // Through the same filter as a producer's payload. Deriving a
                // metric here rather than importing it does not exempt it from
                // review: an undeclared name is still an undeclared name, and
                // the trusted-run envelope is what tells a derived series from
                // whatever else claims the same metric.
                let envelope = Envelope {
                    pipeline_name: run.workflow_name.clone(),
                    repository_url: github.repository_url(),
                };
                let raw = serde_json::to_vec(&body)?;
                match otlp::prepare(&raw, allowlist, &envelope) {
                    Ok((filtered, stats)) => {
                        deliver_derived(config, client, &filtered).await?;
                        metrics.add_dropped("metric", stats.metrics_dropped);
                        metrics.add_dropped("point", stats.points_dropped);
                        metrics.add_kept(stats.metrics_kept);
                        snapshot.metrics_dropped += stats.metrics_dropped;
                        snapshot.points_dropped += stats.points_dropped;
                        snapshot.metrics_kept += stats.metrics_kept;
                    }
                    Err(err) => {
                        warn!(
                            source = %source.slug(),
                            run_id = run.run_id,
                            error = %err,
                            "derived payload rejected by the allowlist"
                        );
                    }
                }
            }
            ledger.seal_attempt(run.repo_id, run.run_id, attempt)?;
            snapshot.attempts_derived += 1;
        }
    }
    Ok(())
}

async fn deliver_derived(config: &Config, client: &reqwest::Client, body: &[u8]) -> Result<()> {
    let url = format!("{}/v1/metrics", config.otlp_endpoint.trim_end_matches('/'));
    let response = client
        .post(url)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body(body.to_vec())
        .send()
        .await
        .context("posting derived CI metrics")?;
    if !response.status().is_success() {
        bail!(
            "OTLP backend rejected derived metrics with {}",
            response.status()
        );
    }
    Ok(())
}

/// Absent means the token does not expire, which is a real answer rather than
/// a missing one, so no series is written for it.
fn record_token_expiry(
    metrics: &Metrics,
    snapshot: &mut CollectSnapshot,
    source: &SourceConfig,
    github: &GitHub,
) {
    let Some(expires) = github.token_expires_unix() else {
        return;
    };
    metrics.set_source_token_expiry(&source.slug(), expires);
    snapshot.token_expiry.push((source.slug(), expires));
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
    use crate::github::artifact_name_matches;

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
