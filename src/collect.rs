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
    snapshot.blocked = result
        .as_ref()
        .is_err_and(|err| err.is::<BackendNotReady>());
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
    let client = delivery_client()?;

    // Asked once before anything is downloaded. Once a check fails, no
    // collected data is sent for the rest of the pass: every source delivers
    // to the same backend.
    // Sources are still listed, so `source_up` and token expiry stay current
    // while delivery waits, which may be days if the URL itself is wrong.
    let mut blocked = require_backend_ready(config, &client, metrics).await.err();

    // One source failing must not skip the ones after it. A repository whose
    // token expired would otherwise silently stop collection for every other
    // repository in the same process.
    let mut failed = Vec::new();
    for source in &config.sources {
        let slug = source.slug();
        let result = collect_source(
            config,
            source,
            &allowlist,
            &ledger,
            &client,
            metrics,
            snapshot,
            &mut blocked,
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
    pass_result(&failed, blocked)
}

/// A failed source outranks a block: it needs someone to act on it, and the
/// readiness check has already logged the block.
fn pass_result(failed: &[String], blocked: Option<anyhow::Error>) -> Result<()> {
    if !failed.is_empty() {
        bail!("collection failed for {}", failed.join(", "));
    }
    match blocked {
        Some(err) => Err(err),
        None => Ok(()),
    }
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
    blocked: &mut Option<anyhow::Error>,
) -> Result<()> {
    let github = GitHub::new(source.clone(), &config.github_api)?;
    let listed = github.list_recent_runs(config.lookback).await;
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
                warn!(source = %source.slug(), error = %err, "listing GitHub workflow runs failed");
            }
            return Err(err);
        }
    };
    snapshot.runs_seen += runs.len() as u64;
    // Blocked earlier in the pass: listed, so this source's own health is
    // known, but nothing is downloaded or derived only to wait.
    if blocked.is_some() {
        return Ok(());
    }
    // A block met below stops delivery but not the bookkeeping: every path
    // reaches the same exit, so an error from before the block still fails
    // the source.
    let mut had_errors = false;
    if let Err(err) =
        replay_pending(config, source, allowlist, ledger, client, metrics, snapshot).await
    {
        if err.is::<BackendNotReady>() {
            *blocked = Some(err);
        } else {
            warn!(source = %source.slug(), error = %err, "replaying allowlist-held metrics failed");
            had_errors = true;
        }
    }
    for run in &runs {
        if blocked.is_some() {
            break;
        }
        if !github.trusted(run) {
            continue;
        }
        snapshot.runs_trusted += 1;
        if !ledger.should_scan_artifacts(
            "collect",
            run.repo_id,
            run.run_id,
            run.attempt,
            run.artifact_scan_stamp(),
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
                if err.is::<BackendNotReady>() {
                    *blocked = Some(err);
                    break;
                }
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
        // A blocked run stays unmarked, so the next pass looks at it again.
        if !run_had_errors && blocked.is_none() {
            ledger.mark_artifacts_scanned("collect", run.repo_id, run.run_id, run.attempt)?;
        }
    }
    // Only a finished attempt has a conclusion and a last job. Handing the
    // rest to the derivation would cost a jobs request per pass and come back
    // with nothing but a `RunNotCompleted` anomaly.
    let completed: Vec<WorkflowRun> = runs
        .iter()
        .filter(|run| run.is_completed())
        .cloned()
        .collect();
    if blocked.is_none()
        && let Some(actions) = source.actions.as_ref()
        && let Err(err) = derive_actions(
            config, source, actions, allowlist, &completed, ledger, &github, client, snapshot,
        )
        .await
    {
        if err.is::<BackendNotReady>() {
            *blocked = Some(err);
        } else {
            warn!(source = %source.slug(), error = %err, "deriving CI metrics failed");
            had_errors = true;
        }
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

fn delivery_client() -> Result<reqwest::Client> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    Ok(reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(60))
        .build()?)
}

/// Delivery stops on this for the rest of the pass, and nothing is recorded.
///
/// A type of its own because every loop in a pass otherwise steps over a
/// failure and carries on with the next artifact, attempt or source. They all
/// deliver to the same backend, so after one failed check the rest would
/// download and parse their payloads only to wait as well.
#[derive(Debug)]
pub struct BackendNotReady {
    reason: String,
}

impl std::fmt::Display for BackendNotReady {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "OTLP backend is not ready: {}", self.reason)
    }
}

impl std::error::Error for BackendNotReady {}

/// Long enough for a loaded store to answer `SELECT 1`; short enough that a
/// hung one costs seconds per check rather than the client's full minute.
const READINESS_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Readiness {
    Ready,
    /// The store may come back by itself: a 5xx, 408 or 429, or no answer.
    Unavailable,
    /// Waiting will not help. A redirect or any other 4xx means the URL is
    /// wrong or needs access Kartero was not given.
    Misconfigured,
}

impl Readiness {
    fn of(status: StatusCode) -> Self {
        if status.is_success() {
            Self::Ready
        } else if status.is_server_error()
            || matches!(
                status,
                StatusCode::REQUEST_TIMEOUT | StatusCode::TOO_MANY_REQUESTS
            )
        {
            Self::Unavailable
        } else {
            Self::Misconfigured
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Ready => "ready",
            Self::Unavailable => "unavailable",
            Self::Misconfigured => "misconfigured",
        }
    }
}

/// Asks the configured readiness URL whether the store behind the OTLP
/// endpoint can take writes. Without one, delivery goes ahead unchecked.
///
/// A 2xx from the collector cannot answer this. It acknowledges a body once it
/// has queued it, so with the store down it accepts data it later drops, and
/// the ledger records each artifact as delivered and never retries it.
async fn require_backend_ready(
    config: &Config,
    client: &reqwest::Client,
    metrics: &Metrics,
) -> Result<()> {
    match config.otlp_readiness_url.as_deref() {
        Some(url) => check_readiness(client, url, READINESS_TIMEOUT, metrics).await,
        None => Ok(()),
    }
}

/// Only a 2xx counts. The body is not read, and a redirect is not followed: a
/// login page in front of the real check would answer 200.
async fn check_readiness(
    client: &reqwest::Client,
    url: &str,
    timeout: Duration,
    metrics: &Metrics,
) -> Result<()> {
    let (readiness, answered, reason) = match client.get(url).timeout(timeout).send().await {
        Ok(response) => {
            let status = response.status();
            let reason = match response.headers().get(reqwest::header::LOCATION) {
                // Where it points, without its query: a redirect can carry a
                // token, and this ends up in the log.
                Some(location) if status.is_redirection() => {
                    let location = String::from_utf8_lossy(location.as_bytes());
                    let target = location.split(['?', '#']).next().unwrap_or_default();
                    format!("{url} answered {status}, redirecting to {target}")
                }
                _ => format!("{url} answered {status}"),
            };
            (Readiness::of(status), true, reason)
        }
        // With its causes: reqwest's own message stops at "error sending
        // request", which is the one part every failure has in common.
        Err(err) => (
            Readiness::Unavailable,
            false,
            format!("{url} did not answer: {:#}", anyhow::Error::from(err)),
        ),
    };
    metrics.inc_otlp_readiness_check(readiness.as_str());
    let was_blocked = metrics.set_otlp_delivery_blocked(readiness != Readiness::Ready);
    match readiness {
        Readiness::Ready => {
            if was_blocked {
                info!(url, "OTLP backend is ready again; delivering");
            }
            return Ok(());
        }
        Readiness::Unavailable if answered => warn!(
            reason = %reason,
            "OTLP backend is not ready; deliveries wait for the next pass"
        ),
        // A wrong host name or an untrusted certificate fails here too, and
        // cannot be told from a store that is down without matching on error
        // text.
        Readiness::Unavailable => warn!(
            reason = %reason,
            "OTLP readiness URL did not answer; deliveries wait for the next pass. \
             If the store is up, check the URL's host and certificate"
        ),
        // Logged like a source that cannot be listed: nothing arrives until
        // someone changes the configuration.
        Readiness::Misconfigured => error!(
            reason = %reason,
            "OTLP readiness URL refuses the check; deliveries are blocked until it is fixed"
        ),
    }
    Err(BackendNotReady { reason }.into())
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
    // Per POST, not only per pass: a store that goes down halfway through a
    // pass would otherwise take the rest of the backlog with it.
    require_backend_ready(config, client, metrics).await?;
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

/// `skipped` is every artifact a pass had nothing to do for, which includes
/// each one delivered on an earlier pass, so it is never zero. `rejected` is
/// an artifact refused for what it contains: its producer has to change before
/// anything it uploads can arrive.
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
        record_artifact(metrics, snapshot, "rejected");
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
            record_artifact(metrics, snapshot, "rejected");
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
        record_artifact(metrics, snapshot, "rejected");
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
            record_artifact(metrics, snapshot, "rejected");
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
        (DeliveryStatus::Skipped, "rejected")
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
    use super::collect_inner;
    use super::{
        BackendNotReady, Readiness, check_readiness, delivery_client, parse_otlp_partial,
        pass_result, post_metrics, replay_pending, require_backend_ready,
    };
    use crate::allowlist::Allowlist;
    use crate::config::{Config, SourceConfig};
    use crate::github::artifact_name_matches;
    use crate::ledger::{DeliveryKey, DeliveryStatus, Ledger, PendingMetrics};
    use crate::metrics::Metrics;
    use crate::self_telemetry::CollectSnapshot;
    use axum::Router;
    use axum::extract::{Path as Segments, Query, State};
    use axum::response::IntoResponse;
    use axum::routing::{get, post};
    use reqwest::StatusCode;
    use serde_json::{Value, json};
    use std::collections::HashMap;
    use std::path::Path;
    use std::sync::atomic::{AtomicU16, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    /// The store's health check and the collector's OTLP route on one port.
    #[derive(Clone, Default)]
    struct Backend {
        health: Arc<AtomicU16>,
        /// Checks from this one on answer 503, as if the store failed then.
        fail_from: Arc<AtomicUsize>,
        checks: Arc<AtomicUsize>,
        posts: Arc<AtomicUsize>,
    }

    impl Backend {
        fn answering(status: u16) -> Self {
            let backend = Self::default();
            backend.answer(status);
            backend.fail_from_check(usize::MAX);
            backend
        }

        fn fail_from_check(&self, check: usize) {
            self.fail_from.store(check, Ordering::SeqCst);
        }

        fn answer(&self, status: u16) {
            self.health.store(status, Ordering::SeqCst);
        }

        fn checks(&self) -> usize {
            self.checks.load(Ordering::SeqCst)
        }

        fn posts(&self) -> usize {
            self.posts.load(Ordering::SeqCst)
        }
    }

    async fn serve(backend: &Backend) -> String {
        let app = Router::new()
            .route(
                "/health",
                get(|State(backend): State<Backend>| async move {
                    let check = backend.checks.fetch_add(1, Ordering::SeqCst);
                    let status = if check >= backend.fail_from.load(Ordering::SeqCst) {
                        503
                    } else {
                        backend.health.load(Ordering::SeqCst)
                    };
                    axum::http::StatusCode::from_u16(status).unwrap()
                }),
            )
            .route(
                "/moved",
                get(|| async { axum::response::Redirect::temporary("/health") }),
            )
            .route(
                "/hung",
                get(|| async {
                    tokio::time::sleep(Duration::from_secs(60)).await;
                    "ready, a minute late"
                }),
            )
            .route(
                "/v1/metrics",
                post(|State(backend): State<Backend>| async move {
                    backend.posts.fetch_add(1, Ordering::SeqCst);
                    "{}"
                }),
            )
            .with_state(backend.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{address}")
    }

    /// Accepts every connection and closes it unanswered. A closed port would
    /// do the same job if nothing else could bind it in the meantime.
    async fn hang_up() -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                drop(stream);
            }
        });
        format!("http://{address}")
    }

    fn source() -> SourceConfig {
        SourceConfig {
            token: "token".into(),
            owner: "owner".into(),
            repo: "repo".into(),
            workflows: vec!["ci.yml".into()],
            trusted_branch: "main".into(),
            actions: None,
        }
    }

    fn config(endpoint: &str, readiness_url: Option<String>, ledger: &Path) -> Config {
        Config {
            bind: "127.0.0.1:0".into(),
            interval: Duration::from_secs(3600),
            heartbeat_interval: Duration::from_secs(60),
            lookback: Duration::from_secs(86_400),
            sources: vec![source()],
            otlp_endpoint: endpoint.into(),
            otlp_readiness_url: readiness_url,
            github_api: crate::github::API.into(),
            allowlist_path: concat!(env!("CARGO_MANIFEST_DIR"), "/allowlist.yaml").into(),
            ledger_path: ledger.into(),
            artifact_prefix: "telemetry-otlp-v1".into(),
            archive: None,
        }
    }

    async fn check(readiness_url: String, metrics: &Metrics) -> anyhow::Result<()> {
        let config = config(
            "http://127.0.0.1:9",
            Some(readiness_url),
            Path::new("unused.sqlite"),
        );
        require_backend_ready(&config, &delivery_client().unwrap(), metrics).await
    }

    fn checks(metrics: &Metrics, outcome: &str) -> u64 {
        let series = format!("kartero_otlp_readiness_checks_total{{outcome=\"{outcome}\"}} ");
        metrics
            .encode()
            .lines()
            .find_map(|line| line.strip_prefix(series.as_str()))
            .and_then(|value| value.parse().ok())
            .unwrap_or_else(|| panic!("no series {series}"))
    }

    fn blocked(metrics: &Metrics) -> bool {
        metrics
            .encode()
            .lines()
            .any(|line| line == "kartero_otlp_delivery_blocked 1")
    }

    /// The GitHub REST API as far as a pass uses it: runs per repository,
    /// artifacts per run (`None` answers 502) and zips. Records each request.
    #[derive(Clone, Default)]
    struct FakeGitHub {
        runs: Arc<HashMap<String, Vec<Value>>>,
        artifacts: Arc<HashMap<i64, Option<Vec<Value>>>>,
        zips: Arc<HashMap<i64, Vec<u8>>>,
        asked: Arc<Mutex<Vec<String>>>,
    }

    impl FakeGitHub {
        fn new(runs: &[(&str, &[i64])], artifacts: &[(i64, Option<&[i64]>)]) -> Self {
            let repo_ids: HashMap<&str, i64> = runs
                .iter()
                .zip(1..)
                .map(|((slug, _), id)| (*slug, id))
                .collect();
            let zips = artifacts
                .iter()
                .flat_map(|(_, ids)| ids.unwrap_or_default().iter())
                .map(|id| (*id, telemetry_zip()))
                .collect();
            Self {
                runs: Arc::new(
                    runs.iter()
                        .map(|(slug, ids)| {
                            let runs = ids.iter().map(|id| run(*id, repo_ids[slug], slug));
                            ((*slug).to_string(), runs.collect())
                        })
                        .collect(),
                ),
                artifacts: Arc::new(
                    artifacts
                        .iter()
                        .map(|(run, ids)| {
                            (
                                *run,
                                ids.map(|ids| ids.iter().map(|id| artifact(*id)).collect()),
                            )
                        })
                        .collect(),
                ),
                zips: Arc::new(zips),
                asked: Arc::default(),
            }
        }

        fn asked(&self) -> Vec<String> {
            self.asked.lock().unwrap().clone()
        }

        fn times_asked(&self, request: &str) -> usize {
            self.asked()
                .iter()
                .filter(|asked| *asked == request)
                .count()
        }
    }

    async fn serve_github(github: &FakeGitHub) -> String {
        let app = Router::new()
            .route(
                "/repos/{owner}/{repo}/actions/workflows/{workflow}/runs",
                get(
                    |State(github): State<FakeGitHub>,
                     Segments((owner, repo, _)): Segments<(String, String, String)>,
                     Query(query): Query<HashMap<String, String>>| async move {
                        let slug = format!("{owner}/{repo}");
                        github.asked.lock().unwrap().push(format!("runs {slug}"));
                        let runs = match query.get("page").map(String::as_str) {
                            Some("1") => github.runs.get(&slug).cloned().unwrap_or_default(),
                            _ => Vec::new(),
                        };
                        axum::Json(json!({"total_count": runs.len(), "workflow_runs": runs}))
                    },
                ),
            )
            .route(
                "/repos/{owner}/{repo}/actions/runs/{run}/artifacts",
                get(
                    |State(github): State<FakeGitHub>,
                     Segments((_, _, run)): Segments<(String, String, i64)>,
                     Query(query): Query<HashMap<String, String>>| async move {
                        github
                            .asked
                            .lock()
                            .unwrap()
                            .push(format!("artifacts {run}"));
                        let first_page = query.get("page").map(String::as_str) == Some("1");
                        match github.artifacts.get(&run) {
                            Some(None) => axum::http::StatusCode::BAD_GATEWAY.into_response(),
                            Some(Some(artifacts)) if first_page => axum::Json(
                                json!({"total_count": artifacts.len(), "artifacts": artifacts}),
                            )
                            .into_response(),
                            _ => axum::Json(json!({"total_count": 0, "artifacts": []}))
                                .into_response(),
                        }
                    },
                ),
            )
            .route(
                "/repos/{owner}/{repo}/actions/artifacts/{id}/zip",
                get(
                    |State(github): State<FakeGitHub>,
                     Segments((_, _, id)): Segments<(String, String, i64)>| async move {
                        github.asked.lock().unwrap().push(format!("zip {id}"));
                        github.zips.get(&id).cloned().unwrap_or_default()
                    },
                ),
            )
            .with_state(github.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{address}")
    }

    /// Finished long ago. A pass that marks it scanned makes the next one skip
    /// it for six hours, so a delivery on the next pass shows it was left
    /// unmarked.
    fn run(id: i64, repo_id: i64, slug: &str) -> Value {
        json!({
            "id": id,
            "run_attempt": 1,
            "event": "schedule",
            "head_branch": "main",
            "head_sha": "9f3c1ab",
            "workflow_id": 1,
            "name": "CI",
            "conclusion": "success",
            "status": "completed",
            "repository": {"id": repo_id, "full_name": slug},
            "created_at": "2026-01-01T00:00:00Z",
            "run_started_at": "2026-01-01T00:00:00Z",
            "updated_at": "2026-01-01T01:00:00Z",
            "path": ".github/workflows/ci.yml"
        })
    }

    fn artifact(id: i64) -> Value {
        json!({
            "id": id,
            "name": format!("telemetry-otlp-v1-{id}"),
            "digest": format!("sha256:{id}"),
            "size_in_bytes": 512,
            "expired": false
        })
    }

    fn telemetry_zip() -> Vec<u8> {
        use std::io::Write;
        let mut buf = std::io::Cursor::new(Vec::new());
        {
            let mut zip = zip::ZipWriter::new(&mut buf);
            let options = zip::write::SimpleFileOptions::default();
            zip.start_file(crate::artifact::SCHEMA_VERSION_FILE, options)
                .unwrap();
            zip.write_all(b"1").unwrap();
            zip.start_file(crate::artifact::METRICS_FILE, options)
                .unwrap();
            zip.write_all(
                br#"{"resourceMetrics":[{"scopeMetrics":[{"metrics":[
                    {"name":"ci.probe.ms_per_row","gauge":{"dataPoints":[{"asDouble":3.0}]}}
                ]}]}]}"#,
            )
            .unwrap();
            zip.finish().unwrap();
        }
        buf.into_inner()
    }

    /// A pass over `slugs` against the fakes, with an allowlist that admits
    /// the probe metric and a ledger that lives as long as `dir`.
    fn pass_config(backend: &str, api: &str, slugs: &[&str], dir: &Path) -> Config {
        let allowlist = dir.join("allowlist.yaml");
        std::fs::write(
            &allowlist,
            "metrics: [ci.probe.ms_per_row]\nattributes: []\n",
        )
        .unwrap();
        let mut config = config(
            backend,
            Some(format!("{backend}/health")),
            &dir.join("ledger.sqlite"),
        );
        config.github_api = api.into();
        config.allowlist_path = allowlist;
        config.sources = slugs
            .iter()
            .map(|slug| {
                let (owner, repo) = slug.split_once('/').unwrap();
                SourceConfig {
                    owner: owner.into(),
                    repo: repo.into(),
                    ..source()
                }
            })
            .collect();
        config
    }

    #[tokio::test]
    async fn a_pass_blocked_from_the_start_lists_every_source_and_asks_once() {
        let backend = Backend::answering(503);
        let base = serve(&backend).await;
        let github = FakeGitHub::new(
            &[("owner/a", &[101]), ("owner/b", &[201])],
            &[(101, Some(&[1001])), (201, Some(&[2001]))],
        );
        let api = serve_github(&github).await;
        let dir = tempfile::tempdir().unwrap();
        let config = pass_config(&base, &api, &["owner/a", "owner/b"], dir.path());
        let mut snapshot = CollectSnapshot::default();

        let err = collect_inner(&config, &mut snapshot).await.unwrap_err();
        assert!(err.is::<BackendNotReady>(), "{err:#}");
        assert_eq!(github.asked(), ["runs owner/a", "runs owner/b"]);
        assert_eq!(backend.checks(), 1);
        assert_eq!(backend.posts(), 0);
        assert_eq!(snapshot.source_status.len(), 2);
        assert!(snapshot.source_status.iter().all(|status| status.up));
    }

    #[tokio::test]
    async fn a_store_that_fails_mid_pass_keeps_the_rest_for_the_next_pass() {
        let backend = Backend::answering(200);
        // Runs go newest first. The checks before the pass and before 1021
        // pass; the store is gone by the check before 1022.
        backend.fail_from_check(2);
        let base = serve(&backend).await;
        let github = FakeGitHub::new(
            &[("owner/a", &[101, 102]), ("owner/b", &[201])],
            &[
                (102, Some(&[1021, 1022])),
                (101, Some(&[1011])),
                (201, Some(&[2001])),
            ],
        );
        let api = serve_github(&github).await;
        let dir = tempfile::tempdir().unwrap();
        let config = pass_config(&base, &api, &["owner/a", "owner/b"], dir.path());
        let mut snapshot = CollectSnapshot::default();

        let err = collect_inner(&config, &mut snapshot).await.unwrap_err();
        assert!(err.is::<BackendNotReady>(), "{err:#}");
        assert_eq!(backend.posts(), 1);
        assert_eq!(backend.checks(), 3);
        // Nothing after the block is looked at, in this source or the next,
        // which is still listed.
        assert_eq!(github.times_asked("artifacts 101"), 0);
        assert_eq!(github.times_asked("runs owner/b"), 1);
        assert_eq!(github.times_asked("artifacts 201"), 0);
        assert!(snapshot.source_status.iter().all(|status| status.up));

        // The store is back. 1022 was not recorded and run 102 was not marked
        // scanned, so this pass delivers it with 1011 and 2001. 1021 was
        // recorded, so it is not downloaded or sent again.
        backend.fail_from_check(usize::MAX);
        let mut snapshot = CollectSnapshot::default();
        collect_inner(&config, &mut snapshot).await.unwrap();
        assert_eq!(backend.posts(), 4);
        assert_eq!(github.times_asked("zip 1021"), 1);
    }

    #[tokio::test]
    async fn an_error_before_a_block_still_fails_the_source() {
        let backend = Backend::answering(200);
        // Runs go newest first, so listing run 102's artifacts fails before
        // artifact 1011 of run 101 meets the check that blocks it.
        backend.fail_from_check(1);
        let base = serve(&backend).await;
        let github = FakeGitHub::new(
            &[("owner/a", &[101, 102])],
            &[(102, None), (101, Some(&[1011]))],
        );
        let api = serve_github(&github).await;
        let dir = tempfile::tempdir().unwrap();
        let config = pass_config(&base, &api, &["owner/a"], dir.path());
        let mut snapshot = CollectSnapshot::default();

        let err = collect_inner(&config, &mut snapshot).await.unwrap_err();
        assert!(!err.is::<BackendNotReady>(), "{err:#}");
        assert_eq!(github.times_asked("artifacts 102"), 1);
        assert_eq!(github.times_asked("zip 1011"), 1);
        assert_eq!(backend.posts(), 0);
        assert_eq!(snapshot.source_status.len(), 1);
        assert!(!snapshot.source_status[0].up);
    }

    #[tokio::test]
    async fn a_failing_readiness_check_blocks_the_post_until_it_passes() {
        let backend = Backend::answering(503);
        let base = serve(&backend).await;
        let config = config(
            &base,
            Some(format!("{base}/health")),
            Path::new("unused.sqlite"),
        );
        let client = delivery_client().unwrap();
        let metrics = Metrics::new();

        let err = post_metrics(&config, &client, b"{}", "owner/repo", &metrics)
            .await
            .unwrap_err();
        assert!(err.is::<BackendNotReady>(), "{err:#}");
        assert_eq!(backend.posts(), 0);
        assert_eq!(checks(&metrics, "unavailable"), 1);
        assert!(blocked(&metrics));

        backend.answer(200);
        let status = post_metrics(&config, &client, b"{}", "owner/repo", &metrics)
            .await
            .unwrap();
        assert!(status.is_success());
        assert_eq!(backend.posts(), 1);
        assert_eq!(checks(&metrics, "ready"), 1);
        assert!(!blocked(&metrics));
    }

    #[tokio::test]
    async fn without_a_readiness_url_nothing_is_checked() {
        let backend = Backend::answering(503);
        let base = serve(&backend).await;
        let config = config(&base, None, Path::new("unused.sqlite"));
        let metrics = Metrics::new();

        post_metrics(
            &config,
            &delivery_client().unwrap(),
            b"{}",
            "owner/repo",
            &metrics,
        )
        .await
        .unwrap();
        assert_eq!(backend.checks(), 0);
        assert_eq!(backend.posts(), 1);
        assert!(!blocked(&metrics));
        for outcome in ["ready", "unavailable", "misconfigured"] {
            assert_eq!(checks(&metrics, outcome), 0, "{outcome}");
        }
    }

    #[tokio::test]
    async fn only_a_2xx_from_the_readiness_url_counts() {
        let backend = Backend::answering(204);
        let base = serve(&backend).await;
        let metrics = Metrics::new();
        check(format!("{base}/health"), &metrics).await.unwrap();

        backend.answer(404);
        let err = check(format!("{base}/health"), &metrics).await.unwrap_err();
        assert!(err.is::<BackendNotReady>(), "{err:#}");

        // A redirect is not followed: the URL configured is the check, and
        // the page it points to, which answers 2xx here, is never asked.
        backend.answer(200);
        let asked = backend.checks();
        let err = check(format!("{base}/moved"), &metrics).await.unwrap_err();
        assert!(err.is::<BackendNotReady>(), "{err:#}");
        assert!(err.to_string().contains("redirecting to /health"), "{err}");
        assert_eq!(backend.checks(), asked);

        let err = check(format!("{}/health", hang_up().await), &metrics)
            .await
            .unwrap_err();
        assert!(err.is::<BackendNotReady>(), "{err:#}");

        assert_eq!(checks(&metrics, "ready"), 1);
        assert_eq!(checks(&metrics, "misconfigured"), 2);
        assert_eq!(checks(&metrics, "unavailable"), 1);
    }

    #[tokio::test]
    async fn a_hung_readiness_check_counts_as_not_ready() {
        let backend = Backend::answering(200);
        let base = serve(&backend).await;
        // The handler answers 200 after a minute, so only the timeout can
        // turn this into an error.
        let err = check_readiness(
            &delivery_client().unwrap(),
            &format!("{base}/hung"),
            Duration::from_millis(200),
            &Metrics::new(),
        )
        .await
        .unwrap_err();
        assert!(err.is::<BackendNotReady>(), "{err:#}");
    }

    #[test]
    fn waiting_can_clear_a_5xx_but_not_a_redirect_or_another_4xx() {
        for ready in [StatusCode::OK, StatusCode::NO_CONTENT] {
            assert_eq!(Readiness::of(ready), Readiness::Ready, "{ready}");
        }
        for unavailable in [
            StatusCode::INTERNAL_SERVER_ERROR,
            StatusCode::SERVICE_UNAVAILABLE,
            StatusCode::REQUEST_TIMEOUT,
            StatusCode::TOO_MANY_REQUESTS,
        ] {
            assert_eq!(
                Readiness::of(unavailable),
                Readiness::Unavailable,
                "{unavailable}"
            );
        }
        for misconfigured in [
            StatusCode::FOUND,
            StatusCode::BAD_REQUEST,
            StatusCode::UNAUTHORIZED,
            StatusCode::FORBIDDEN,
            StatusCode::NOT_FOUND,
            StatusCode::METHOD_NOT_ALLOWED,
        ] {
            assert_eq!(
                Readiness::of(misconfigured),
                Readiness::Misconfigured,
                "{misconfigured}"
            );
        }
    }

    /// Every loop recognises a block by its type, so it has to survive the
    /// context the callers add on the way up.
    #[test]
    fn a_block_is_still_a_block_under_context() {
        let err = anyhow::Error::new(BackendNotReady {
            reason: "503".into(),
        })
        .context("replaying owner/repo run 2")
        .context("collecting owner/repo");
        assert!(err.is::<BackendNotReady>());
    }

    #[test]
    fn a_failed_source_outranks_a_block() {
        let block = || {
            Some(anyhow::Error::new(BackendNotReady {
                reason: "503".into(),
            }))
        };
        let err = pass_result(&["owner/repo".into()], block()).unwrap_err();
        assert!(!err.is::<BackendNotReady>(), "{err:#}");
        assert!(
            pass_result(&[], block())
                .unwrap_err()
                .is::<BackendNotReady>()
        );
        assert!(pass_result(&[], None).is_ok());
    }

    #[tokio::test]
    async fn a_blocked_replay_stays_pending() {
        let backend = Backend::answering(503);
        let base = serve(&backend).await;
        let dir = tempfile::tempdir().unwrap();
        let config = config(
            &base,
            Some(format!("{base}/health")),
            &dir.path().join("ledger.sqlite"),
        );
        let ledger = Ledger::open(&config.ledger_path).unwrap();
        let allowlist =
            Allowlist::parse("metrics: [ci.probe.ms_per_row]\nattributes: []\n").unwrap();
        let key = DeliveryKey {
            repo_id: 1,
            run_id: 2,
            attempt: 1,
            artifact_id: 3,
            digest: "sha256:abc".into(),
            schema_version: 1,
        };
        let payload = br#"{"resourceMetrics":[{"scopeMetrics":[{"metrics":[
            {"name":"ci.probe.ms_per_row","gauge":{"dataPoints":[{"asDouble":3.0}]}}
        ]}]}]}"#;
        let pending = PendingMetrics::artifact(
            &key,
            "owner/repo".into(),
            "CI".into(),
            "https://github.com/owner/repo".into(),
            payload.to_vec(),
        );
        // Withheld under an older allowlist, so the current one replays it.
        ledger
            .record_with_pending(&key, DeliveryStatus::Filtered, Some(&pending), "older")
            .unwrap();
        let fingerprint = allowlist.fingerprint();
        let client = delivery_client().unwrap();
        let metrics = Metrics::new();
        let mut snapshot = CollectSnapshot::default();

        let err = replay_pending(
            &config,
            &source(),
            &allowlist,
            &ledger,
            &client,
            &metrics,
            &mut snapshot,
        )
        .await
        .unwrap_err();
        assert!(err.is::<BackendNotReady>(), "{err:#}");
        assert_eq!(backend.posts(), 0);
        assert_eq!(
            ledger
                .pending_for_source("owner/repo", &fingerprint)
                .unwrap()
                .len(),
            1
        );

        backend.answer(200);
        replay_pending(
            &config,
            &source(),
            &allowlist,
            &ledger,
            &client,
            &metrics,
            &mut snapshot,
        )
        .await
        .unwrap();
        assert_eq!(backend.posts(), 1);
        assert!(
            ledger
                .pending_for_source("owner/repo", &fingerprint)
                .unwrap()
                .is_empty()
        );
    }

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
