use crate::allowlist::Allowlist;
use crate::artifact::{self, MAX_ZIP_BYTES};
use crate::config::{Config, SourceConfig};
use crate::github::{self, ArtifactRef, GitHub, WorkflowRun};
use crate::ledger::{DeliveryKey, DeliveryStatus, Ledger, PendingMetrics};
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
    snapshot.source_last_delivery = Metrics::global().last_deliveries();
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
    let expired = ledger.prune_pending()?;
    ledger.prune_artifact_scans()?;
    if expired > 0 {
        metrics.add_pending_expired(expired as u64);
        warn!(expired, "withheld metrics passed the 30-day replay horizon");
    }
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
    let (pending_count, pending_bytes) = ledger.pending_stats()?;
    metrics.set_pending(pending_count, pending_bytes);
    snapshot.pending_count = pending_count as u64;
    snapshot.pending_bytes = pending_bytes as u64;
    if pending_bytes > 512 * 1024 * 1024 {
        warn!(
            pending_bytes,
            "withheld metric payloads occupy over 512 MiB of the ledger PVC"
        );
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
    if let Err(err) =
        replay_pending(config, source, allowlist, ledger, client, metrics, snapshot).await
    {
        warn!(source = %source.slug(), error = %err, "replaying allowlist-held metrics failed");
        had_errors = true;
    }
    for run in &runs {
        if !github.trusted(run) {
            continue;
        }
        snapshot.runs_trusted += 1;
        if !ledger.should_scan_artifacts(
            "collect",
            run.repo_id,
            run.run_id,
            run.attempt,
            run.observed_at,
        )? {
            continue;
        }
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
        let mut run_had_errors = false;
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
                run_had_errors = true;
            }
        }
        if !run_had_errors {
            ledger.mark_artifacts_scanned("collect", run.repo_id, run.run_id, run.attempt)?;
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

    // Job names come from the repository that declares them when it says
    // where. Failing here stops the derivation for this source rather than
    // falling back to whatever is configured locally: an empty or stale list
    // sends every job to `other`, and these are delta points, so a pass that
    // emits them cannot be taken back.
    let actions = &match resolve_job_names(source, actions, runs, github).await {
        Ok(resolved) => resolved,
        Err(err) => {
            // `{:#}` rather than Display: this stops the derivation for a whole
            // source, and the outermost context alone says which file could not
            // be read without ever saying why. A 404, a 403 for a token missing
            // repository contents, and a malformed file all need different
            // fixes and looked identical in the log.
            let kind = github::permanent_kind(&err).unwrap_or("job_names");
            error!(
                source = %source.slug(),
                kind,
                error = format!("{err:#}"),
                "job names unreadable; deriving nothing for this source"
            );
            metrics.inc_source_listing_failure(&source.slug(), kind);
            snapshot.sources_misconfigured += 1;
            snapshot.inc_anomaly(&source.slug(), "job_names_unreadable");
            metrics.inc_anomaly(&source.slug(), "job_names_unreadable");
            return Ok(());
        }
    };

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
                let partition = otlp::partition(&raw, allowlist, &envelope).with_context(|| {
                    format!(
                        "derived payload for {} run {} attempt {}",
                        source.slug(),
                        run.run_id,
                        attempt
                    )
                })?;
                if let Some(ref filtered) = partition.accepted {
                    deliver_derived(config, client, filtered, &source.slug(), metrics).await?;
                }
                record_filter_stats(metrics, snapshot, &partition.stats);
                let pending = partition.deferred.map(|payload| {
                    PendingMetrics::derived(
                        run.repo_id,
                        run.run_id,
                        attempt,
                        source.slug(),
                        envelope.pipeline_name,
                        envelope.repository_url,
                        payload,
                    )
                });
                ledger.seal_attempt_with_pending(
                    run.repo_id,
                    run.run_id,
                    attempt,
                    pending.as_ref(),
                    &allowlist.fingerprint(),
                )?;
            } else {
                ledger.seal_attempt(run.repo_id, run.run_id, attempt)?;
            }
            snapshot.attempts_derived += 1;
        }
    }
    Ok(())
}

async fn deliver_derived(
    config: &Config,
    client: &reqwest::Client,
    body: &[u8],
    source: &str,
    metrics: &Metrics,
) -> Result<()> {
    let status = post_metrics(config, client, body, source, metrics).await?;
    if !status.is_success() {
        bail!("OTLP backend rejected derived metrics with {status}");
    }
    Ok(())
}

fn record_filter_stats(
    metrics: &Metrics,
    snapshot: &mut CollectSnapshot,
    stats: &otlp::FilterStats,
) {
    metrics.add_dropped("metric", stats.metrics_dropped);
    metrics.add_dropped("point", stats.points_dropped);
    metrics.add_kept(stats.metrics_kept);
    snapshot.metrics_dropped += stats.metrics_dropped;
    snapshot.points_dropped += stats.points_dropped;
    snapshot.metrics_kept += stats.metrics_kept;
}

async fn replay_pending(
    config: &Config,
    source: &SourceConfig,
    allowlist: &Allowlist,
    ledger: &Ledger,
    client: &reqwest::Client,
    metrics: &Metrics,
    snapshot: &mut CollectSnapshot,
) -> Result<()> {
    let fingerprint = allowlist.fingerprint();
    // Fetch one payload at a time: a filtered artifact can be 16 MiB, and the
    // collector container must never hold a hundred such blobs in memory.
    for _ in 0..100 {
        let Some(pending) = ledger
            .pending_for_source(&source.slug(), &fingerprint)?
            .into_iter()
            .next()
        else {
            break;
        };
        let envelope = Envelope {
            pipeline_name: pending.pipeline_name.clone(),
            repository_url: pending.repository_url.clone(),
        };
        let partition = otlp::partition(&pending.payload, allowlist, &envelope)
            .with_context(|| format!("replaying {} run {}", pending.source, pending.run_id))?;
        if let Some(ref body) = partition.accepted {
            let status = post_metrics(config, client, body, &pending.source, metrics).await?;
            if !status.is_success() {
                bail!("OTLP backend rejected replayed metrics with {status}");
            }
        }
        record_filter_stats(metrics, snapshot, &partition.stats);
        ledger.advance_pending(&pending, partition.deferred.as_deref(), &fingerprint)?;
    }
    Ok(())
}

/// A successful OTLP HTTP response can still reject some points. OTLP says a
/// partial success must not be retried as a whole, because accepted delta
/// points would be counted twice.
async fn post_metrics(
    config: &Config,
    client: &reqwest::Client,
    body: &[u8],
    source: &str,
    metrics: &Metrics,
) -> Result<StatusCode> {
    let url = format!("{}/v1/metrics", config.otlp_endpoint.trim_end_matches('/'));
    let mut response = client
        .post(url)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body(body.to_vec())
        .send()
        .await
        .context("posting OTLP metrics")?;
    let status = response.status();
    if !status.is_success() {
        return Ok(status);
    }
    const MAX_RESPONSE_BYTES: usize = 64 * 1024;
    let mut receipt = Vec::new();
    loop {
        match response.chunk().await {
            Ok(Some(chunk)) if receipt.len().saturating_add(chunk.len()) <= MAX_RESPONSE_BYTES => {
                receipt.extend_from_slice(&chunk);
            }
            Ok(None) => break,
            Ok(Some(_)) => {
                metrics.inc_otlp_response_issue(source);
                warn!(
                    source,
                    "OTLP success response exceeds 64 KiB; acceptance is unconfirmed"
                );
                return Ok(status);
            }
            Err(err) => {
                metrics.inc_otlp_response_issue(source);
                warn!(source, error = %err, "could not read OTLP success response; acceptance is unconfirmed");
                return Ok(status);
            }
        }
    }
    if receipt.is_empty() {
        metrics.inc_otlp_response_issue(source);
        warn!(
            source,
            "OTLP success response has no body; acceptance is unconfirmed"
        );
        return Ok(status);
    }
    let partial = match parse_otlp_partial(&receipt) {
        Ok(value) => value,
        Err(err) => {
            metrics.inc_otlp_response_issue(source);
            warn!(source, error = %err, "invalid OTLP success response; acceptance is unconfirmed");
            return Ok(status);
        }
    };
    if let Some((rejected, message)) = partial {
        if rejected > 0 {
            metrics.add_otlp_rejected(source, rejected);
            warn!(
                source,
                rejected, message, "OTLP backend rejected data points"
            );
            return Ok(status);
        }
        if !message.is_empty() {
            warn!(source, message, "OTLP backend returned a warning");
        }
    }
    metrics.record_delivered_families(source, body);
    Ok(status)
}

fn parse_otlp_partial(receipt: &[u8]) -> Result<Option<(u64, String)>> {
    let value: serde_json::Value = serde_json::from_slice(receipt)?;
    if !value.is_object() {
        bail!("OTLP success response is not an object");
    }
    let Some(partial) = value
        .get("partialSuccess")
        .or_else(|| value.get("partial_success"))
    else {
        return Ok(None);
    };
    let rejected = match partial
        .get("rejectedDataPoints")
        .or_else(|| partial.get("rejected_data_points"))
    {
        None => 0,
        Some(serde_json::Value::Number(value)) => value.as_u64().context("rejectedDataPoints")?,
        Some(serde_json::Value::String(value)) => value.parse().context("rejectedDataPoints")?,
        _ => bail!("rejectedDataPoints is not a nonnegative integer"),
    };
    let message = partial
        .get("errorMessage")
        .or_else(|| partial.get("error_message"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_string();
    Ok(Some((rejected, message)))
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
    // Cheap: a hash of a few dozen short strings, and it has to match what a
    // later pass computes from the allowlist then in force.
    let allowlist_fingerprint = &allowlist.fingerprint();
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

    if ledger.is_terminal(&key_without_version(1), allowlist_fingerprint)? {
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
    if ledger.is_terminal(&key, allowlist_fingerprint)? {
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
    let partition = match otlp::partition(&payload.metrics_json, allowlist, &envelope) {
        Ok(prepared) => prepared,
        Err(err) => {
            warn!(artifact = %artifact.name, error = %err, "artifact payload rejected");
            ledger.record(&key, DeliveryStatus::Skipped)?;
            record_artifact(metrics, snapshot, "skipped");
            return Ok(());
        }
    };
    record_filter_stats(metrics, snapshot, &partition.stats);
    let pending = partition.deferred.map(|payload| {
        PendingMetrics::artifact(
            &key,
            github.source_slug(),
            envelope.pipeline_name,
            envelope.repository_url,
            payload,
        )
    });
    let Some(body) = partition.accepted else {
        ledger.record_with_pending(
            &key,
            DeliveryStatus::Filtered,
            pending.as_ref(),
            allowlist_fingerprint,
        )?;
        record_artifact(metrics, snapshot, "skipped");
        return Ok(());
    };
    let status = post_metrics(config, client, &body, &github.source_slug(), metrics).await?;
    if status.is_success() {
        ledger.record_with_pending(
            &key,
            DeliveryStatus::Delivered,
            pending.as_ref(),
            allowlist_fingerprint,
        )?;
        record_artifact(metrics, snapshot, "delivered");
        info!(
            run_id = run.run_id,
            artifact = %artifact.name,
            kept = partition.stats.metrics_kept,
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

/// The job names this source's derivation should use.
///
/// When `job_names_artifact` is set the repository owns them and this returns
/// a copy of the deployment config with `canonical_jobs` and `job_aliases`
/// replaced by what the newest trusted run uploaded. When it is not, the
/// deployment's own lists are used unchanged, so a source without the artifact
/// keeps working.
///
/// Read from an artifact rather than the contents API. Reading a file out of a
/// private repository needs `contents: read`, which grants the whole source
/// tree; artifacts need `actions: read`, which this collector already has for
/// everything else it does. A short list of job names does not justify handing
/// a telemetry collector the code.
///
/// Only trusted runs are considered, so the names come from the trusted branch
/// and a pull request cannot rename a series for everyone by editing one file.
///
/// Errors rather than falling back. A fallback would be the quiet failure this
/// whole mechanism exists to remove: the names would silently be the wrong
/// ones, every job would collapse to `other`, and the only symptom would be
/// every per-job series stopping at once.
async fn resolve_job_names(
    source: &SourceConfig,
    actions: &crate::config::ActionsConfig,
    runs: &[WorkflowRun],
    github: &GitHub,
) -> Result<crate::config::ActionsConfig> {
    let Some(want) = actions.job_names_artifact.as_deref() else {
        return Ok(actions.clone());
    };

    // Newest first: the most recent trusted run describes the job names as
    // they are now, and older runs are only reached if it did not upload them.
    let mut trusted: Vec<&WorkflowRun> = runs.iter().filter(|run| github.trusted(run)).collect();
    trusted.sort_by_key(|run| std::cmp::Reverse(run.run_id));
    if trusted.is_empty() {
        anyhow::bail!("no trusted run in this window carries {want}");
    }

    let mut last_err = None;
    for run in trusted {
        let artifacts = match github.list_artifacts(run.run_id).await {
            Ok(list) => list,
            Err(err) => {
                last_err = Some(err);
                continue;
            }
        };
        let Some(found) = artifacts
            .iter()
            .find(|artifact| artifact.name == want && !artifact.expired)
        else {
            continue;
        };
        let zip = github
            .download_zip_limited(found.id, MAX_ZIP_BYTES)
            .await
            .with_context(|| format!("downloading {want} from run {}", run.run_id))?;
        let raw = crate::artifact::open_named(&zip, crate::actions::names::FILE)
            .with_context(|| format!("{want} from run {}", run.run_id))?;
        let names = crate::actions::names::parse(&String::from_utf8_lossy(&raw))
            .with_context(|| format!("{want} from run {}", run.run_id))?;
        let mut resolved = actions.clone();
        resolved.canonical_jobs = names.canonical;
        resolved.job_aliases = names.aliases;
        return Ok(resolved);
    }

    match last_err {
        Some(err) => Err(err.context(format!("looking for {want} in {}", source.slug()))),
        None => anyhow::bail!(
            "no trusted run in this window uploaded an unexpired {want}; the workflow that \
             publishes it may have stopped running or the artifacts may have expired"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::parse_otlp_partial;
    use crate::github::artifact_name_matches;

    #[test]
    fn otlp_partial_success_reports_rejected_points() {
        let receipt =
            br#"{"partialSuccess":{"rejectedDataPoints":"3","errorMessage":"bad labels"}}"#;
        assert_eq!(
            parse_otlp_partial(receipt).unwrap(),
            Some((3, "bad labels".into()))
        );
        assert_eq!(parse_otlp_partial(br#"{}"#).unwrap(), None);
        assert!(parse_otlp_partial(br#"[]"#).is_err());
        assert!(parse_otlp_partial(br#"{"partialSuccess":{"rejectedDataPoints":-1}}"#).is_err());
    }

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
