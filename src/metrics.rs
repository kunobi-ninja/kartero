use crate::self_telemetry::{ArchiveSnapshot, CollectSnapshot, SourceDelivery};
use prometheus::{
    Encoder, Histogram, IntCounterVec, IntGauge, IntGaugeVec, Registry, TextEncoder,
    histogram_opts, opts,
};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Mutex, OnceLock};

pub struct Metrics {
    registry: Registry,
    artifacts: IntCounterVec,
    series_dropped: IntCounterVec,
    series_kept: IntCounterVec,
    collect_passes: IntCounterVec,
    collect_duration: Histogram,
    last_collect_unix: IntGauge,
    sources: IntGauge,
    runs: IntCounterVec,
    artifacts_discovered: IntCounterVec,
    collect_errors: IntCounterVec,
    source_up: IntGaugeVec,
    source_listing_failures: IntCounterVec,
    anomalies: IntCounterVec,
    source_token_expires: IntGaugeVec,
    source_last_delivery: IntGaugeVec,
    delivery_times: Mutex<BTreeMap<(String, String), u64>>,
    otlp_rejected_points: IntCounterVec,
    otlp_response_issues: IntCounterVec,
    pending_count: IntGauge,
    pending_bytes: IntGauge,
    pending_expired: IntCounterVec,
    archive_artifacts: IntCounterVec,
    archive_passes: IntCounterVec,
    archive_duration: Histogram,
    archive_errors: IntCounterVec,
}

impl Metrics {
    pub fn global() -> &'static Self {
        static METRICS: OnceLock<Metrics> = OnceLock::new();
        METRICS.get_or_init(Self::new)
    }

    fn new() -> Self {
        let registry = Registry::new();
        let artifacts = IntCounterVec::new(
            opts!(
                "kartero_artifacts_total",
                "Artifacts considered by collect, split by outcome."
            ),
            &["outcome"],
        )
        .expect("artifacts counter");
        let series_dropped = IntCounterVec::new(
            opts!(
                "kartero_series_dropped_total",
                "Metrics or data points dropped by the allowlist."
            ),
            &["kind"],
        )
        .expect("dropped counter");
        let series_kept = IntCounterVec::new(
            opts!(
                "kartero_series_kept_total",
                "Metrics kept after allowlist filtering."
            ),
            &["kind"],
        )
        .expect("kept counter");
        let collect_passes = IntCounterVec::new(
            opts!(
                "kartero_collect_passes_total",
                "Finished collect passes, split by whether the pass itself failed."
            ),
            &["outcome"],
        )
        .expect("collect counter");
        let collect_duration = Histogram::with_opts(histogram_opts!(
            "kartero_collect_duration_seconds",
            "Wall time of one collect pass, including GitHub download and OTLP POST.",
            vec![0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0, 120.0]
        ))
        .expect("collect duration");
        let last_collect_unix = IntGauge::new(
            "kartero_last_collect_timestamp_seconds",
            "Unix time of the last finished collect pass.",
        )
        .expect("last collect gauge");
        let sources = IntGauge::new(
            "kartero_sources_configured",
            "Repositories configured as telemetry sources.",
        )
        .expect("sources gauge");
        let runs = IntCounterVec::new(
            opts!("kartero_runs_total", "Workflow runs observed by Kartero."),
            &["state"],
        )
        .expect("runs counter");
        let artifacts_discovered = IntCounterVec::new(
            opts!(
                "kartero_artifacts_discovered_total",
                "Artifacts discovered before delivery outcome processing."
            ),
            &["state"],
        )
        .expect("artifact discovery counter");
        let collect_errors = IntCounterVec::new(
            opts!(
                "kartero_collect_errors_total",
                "Collect errors split by bounded component."
            ),
            &["component"],
        )
        .expect("collect errors counter");
        // Per source, because with several in one process a single dead one
        // is invisible in any total. This is the series worth alerting on:
        // 0 for longer than an interval means that source is delivering
        // nothing, whatever the aggregate says.
        let source_up = IntGaugeVec::new(
            opts!(
                "kartero_source_up",
                "1 when a source listed its workflow runs on the last pass, 0 when it failed."
            ),
            &["source"],
        )
        .expect("source up gauge");
        let source_listing_failures = IntCounterVec::new(
            opts!(
                "kartero_source_listing_failures_total",
                "Listing failures per source. kind=not_found is a configuration or token problem and will not resolve on its own."
            ),
            &["source", "kind"],
        )
        .expect("source listing failures counter");
        // Every rule that drops a derived point counts it here instead of
        // discarding it silently. kind=unknown_job_name rising is a job that
        // was renamed without the collector's configuration following, which
        // otherwise shows up only as a panel quietly going flat.
        let anomalies = IntCounterVec::new(
            opts!(
                "kartero_anomalies_total",
                "Derivations the collector had to give up on, per source and kind."
            ),
            &["source", "kind"],
        )
        .expect("anomalies counter");
        // An absolute instant rather than a countdown: a "seconds remaining"
        // gauge is wrong the moment scraping stops, and right only by
        // accident. Alert with `- time()`.
        let source_token_expires = IntGaugeVec::new(
            opts!(
                "kartero_source_token_expires_timestamp_seconds",
                "Unix time at which a source's token expires. Absent when the token does not expire."
            ),
            &["source"],
        )
        .expect("token expiry gauge");
        let source_last_delivery = IntGaugeVec::new(
            opts!(
                "kartero_source_last_delivery_timestamp_seconds",
                "Last fully accepted OTLP metric delivery by source and bounded family."
            ),
            &["source", "family"],
        )
        .expect("last delivery gauge");
        let otlp_rejected_points = IntCounterVec::new(
            opts!(
                "kartero_otlp_rejected_points_total",
                "Data points rejected in OTLP partial-success responses."
            ),
            &["source"],
        )
        .expect("OTLP rejected points counter");
        let otlp_response_issues = IntCounterVec::new(
            opts!(
                "kartero_otlp_response_issues_total",
                "Successful OTLP responses whose acceptance could not be confirmed."
            ),
            &["source"],
        )
        .expect("OTLP response issues counter");
        let pending_count = IntGauge::new(
            "kartero_pending_metrics",
            "Metric payloads withheld by the allowlist and awaiting a rule change.",
        )
        .expect("pending count gauge");
        let pending_bytes = IntGauge::new(
            "kartero_pending_metrics_bytes",
            "Bytes of withheld OTLP payloads stored in SQLite.",
        )
        .expect("pending bytes gauge");
        let pending_expired = IntCounterVec::new(
            opts!(
                "kartero_pending_expired_total",
                "Withheld payloads removed after the 30-day replay horizon."
            ),
            &["reason"],
        )
        .expect("pending expired counter");
        for collector in [
            Box::new(source_last_delivery.clone()) as Box<dyn prometheus::core::Collector>,
            Box::new(otlp_rejected_points.clone()),
            Box::new(otlp_response_issues.clone()),
            Box::new(pending_count.clone()),
            Box::new(pending_bytes.clone()),
            Box::new(pending_expired.clone()),
        ] {
            registry
                .register(collector)
                .expect("register OTLP delivery metric");
        }
        registry
            .register(Box::new(source_token_expires.clone()))
            .expect("register token expiry");
        registry
            .register(Box::new(source_up.clone()))
            .expect("register source up");
        registry
            .register(Box::new(source_listing_failures.clone()))
            .expect("register source listing failures");
        registry
            .register(Box::new(anomalies.clone()))
            .expect("register anomalies");
        registry
            .register(Box::new(artifacts.clone()))
            .expect("register artifacts");
        registry
            .register(Box::new(series_dropped.clone()))
            .expect("register dropped");
        registry
            .register(Box::new(series_kept.clone()))
            .expect("register kept");
        registry
            .register(Box::new(collect_passes.clone()))
            .expect("register collect");
        registry
            .register(Box::new(collect_duration.clone()))
            .expect("register duration");
        registry
            .register(Box::new(last_collect_unix.clone()))
            .expect("register last collect");
        registry
            .register(Box::new(sources.clone()))
            .expect("register sources");
        registry
            .register(Box::new(runs.clone()))
            .expect("register runs");
        registry
            .register(Box::new(artifacts_discovered.clone()))
            .expect("register artifact discovery");
        registry
            .register(Box::new(collect_errors.clone()))
            .expect("register collect errors");
        for outcome in ["delivered", "skipped", "rejected", "held", "retryable"] {
            let _ = artifacts.with_label_values(&[outcome]);
        }
        for kind in ["metric", "point"] {
            let _ = series_dropped.with_label_values(&[kind]);
            let _ = series_kept.with_label_values(&[kind]);
        }
        for outcome in ["ok", "error"] {
            let _ = collect_passes.with_label_values(&[outcome]);
        }
        for state in ["seen", "trusted"] {
            let _ = runs.with_label_values(&[state]);
        }
        for state in ["seen", "matched"] {
            let _ = artifacts_discovered.with_label_values(&[state]);
        }
        for component in ["github", "ingest"] {
            let _ = collect_errors.with_label_values(&[component]);
        }
        let archive_artifacts = IntCounterVec::new(
            opts!(
                "kartero_archive_artifacts_total",
                "Artifacts considered by archive, split by outcome."
            ),
            &["outcome"],
        )
        .expect("archive artifacts counter");
        let archive_passes = IntCounterVec::new(
            opts!(
                "kartero_archive_passes_total",
                "Finished archive passes, split by whether the pass itself failed."
            ),
            &["outcome"],
        )
        .expect("archive passes counter");
        let archive_duration = Histogram::with_opts(histogram_opts!(
            "kartero_archive_duration_seconds",
            "Wall time of one archive pass, including GitHub download and disk write.",
            vec![0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0, 120.0]
        ))
        .expect("archive duration");
        let archive_errors = IntCounterVec::new(
            opts!(
                "kartero_archive_errors_total",
                "Archive errors split by bounded component."
            ),
            &["component"],
        )
        .expect("archive errors counter");
        registry
            .register(Box::new(archive_artifacts.clone()))
            .expect("register archive artifacts");
        registry
            .register(Box::new(archive_passes.clone()))
            .expect("register archive passes");
        registry
            .register(Box::new(archive_duration.clone()))
            .expect("register archive duration");
        registry
            .register(Box::new(archive_errors.clone()))
            .expect("register archive errors");
        for outcome in ["archived", "skipped", "retryable"] {
            let _ = archive_artifacts.with_label_values(&[outcome]);
        }
        for outcome in ["ok", "error"] {
            let _ = archive_passes.with_label_values(&[outcome]);
        }
        for component in ["github", "store"] {
            let _ = archive_errors.with_label_values(&[component]);
        }
        Self {
            registry,
            artifacts,
            series_dropped,
            series_kept,
            collect_passes,
            collect_duration,
            last_collect_unix,
            sources,
            runs,
            artifacts_discovered,
            collect_errors,
            source_up,
            source_listing_failures,
            anomalies,
            source_token_expires,
            source_last_delivery,
            delivery_times: Mutex::new(BTreeMap::new()),
            otlp_rejected_points,
            otlp_response_issues,
            pending_count,
            pending_bytes,
            pending_expired,
            archive_artifacts,
            archive_passes,
            archive_duration,
            archive_errors,
        }
    }

    /// Called for every configured source on every pass, so a source that
    /// starts failing moves rather than simply stopping. A series that stops
    /// being written looks the same as a scrape that stopped.
    pub fn set_source_up(&self, source: &str, up: bool) {
        self.source_up
            .with_label_values(&[source])
            .set(i64::from(up));
    }

    pub fn set_source_token_expiry(&self, source: &str, expires_unix: i64) {
        self.source_token_expires
            .with_label_values(&[source])
            .set(expires_unix);
    }

    pub fn inc_source_listing_failure(&self, source: &str, kind: &str) {
        self.source_listing_failures
            .with_label_values(&[source, kind])
            .inc();
    }

    pub fn inc_anomaly(&self, source: &str, kind: &str) {
        self.anomalies.with_label_values(&[source, kind]).inc();
    }

    pub fn add_otlp_rejected(&self, source: &str, rejected: u64) {
        self.otlp_rejected_points
            .with_label_values(&[source])
            .inc_by(rejected);
    }

    pub fn inc_otlp_response_issue(&self, source: &str) {
        self.otlp_response_issues.with_label_values(&[source]).inc();
    }

    pub fn record_delivered_families(&self, source: &str, body: &[u8]) {
        let Ok(value) = serde_json::from_slice::<serde_json::Value>(body) else {
            return;
        };
        let mut families = BTreeSet::new();
        if let Some(resources) = value
            .get("resourceMetrics")
            .and_then(serde_json::Value::as_array)
        {
            for resource in resources {
                if let Some(scopes) = resource
                    .get("scopeMetrics")
                    .and_then(serde_json::Value::as_array)
                {
                    for scope in scopes {
                        if let Some(metrics) =
                            scope.get("metrics").and_then(serde_json::Value::as_array)
                        {
                            for metric in metrics {
                                if let Some(name) =
                                    metric.get("name").and_then(serde_json::Value::as_str)
                                {
                                    families.insert(metric_family(name));
                                }
                            }
                        }
                    }
                }
            }
        }
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let mut delivery_times = self
            .delivery_times
            .lock()
            .expect("delivery timestamps mutex");
        for family in families {
            self.source_last_delivery
                .with_label_values(&[source, family])
                .set(now);
            delivery_times.insert((source.to_string(), family.to_string()), now as u64);
        }
    }

    pub fn last_deliveries(&self) -> Vec<SourceDelivery> {
        self.delivery_times
            .lock()
            .expect("delivery timestamps mutex")
            .iter()
            .map(|((slug, family), unix_seconds)| SourceDelivery {
                slug: slug.clone(),
                family: family.clone(),
                unix_seconds: *unix_seconds,
            })
            .collect()
    }

    pub fn set_pending(&self, count: i64, bytes: i64) {
        self.pending_count.set(count);
        self.pending_bytes.set(bytes);
    }

    pub fn add_pending_expired(&self, count: u64) {
        self.pending_expired
            .with_label_values(&["age"])
            .inc_by(count);
    }

    pub fn inc_artifact(&self, outcome: &str) {
        self.artifacts.with_label_values(&[outcome]).inc();
    }

    pub fn add_dropped(&self, kind: &str, n: u64) {
        self.series_dropped.with_label_values(&[kind]).inc_by(n);
    }

    pub fn add_kept(&self, n: u64) {
        self.series_kept.with_label_values(&["metric"]).inc_by(n);
    }

    pub fn observe_collect(&self, snapshot: &CollectSnapshot) {
        self.collect_duration.observe(snapshot.duration_s);
        self.collect_passes
            .with_label_values(&[if snapshot.ok { "ok" } else { "error" }])
            .inc();
        self.sources.set(snapshot.sources as i64);
        self.runs
            .with_label_values(&["seen"])
            .inc_by(snapshot.runs_seen);
        self.runs
            .with_label_values(&["trusted"])
            .inc_by(snapshot.runs_trusted);
        self.artifacts_discovered
            .with_label_values(&["seen"])
            .inc_by(snapshot.artifacts_seen);
        self.artifacts_discovered
            .with_label_values(&["matched"])
            .inc_by(snapshot.artifacts_matched);
        self.collect_errors
            .with_label_values(&["github"])
            .inc_by(snapshot.github_errors);
        self.collect_errors
            .with_label_values(&["ingest"])
            .inc_by(snapshot.ingest_errors);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        self.last_collect_unix.set(now);
    }

    pub fn inc_archive_artifact(&self, outcome: &str) {
        self.archive_artifacts.with_label_values(&[outcome]).inc();
    }

    pub fn observe_archive(&self, snapshot: &ArchiveSnapshot) {
        self.archive_duration.observe(snapshot.duration_s);
        self.archive_passes
            .with_label_values(&[if snapshot.ok { "ok" } else { "error" }])
            .inc();
        self.archive_errors
            .with_label_values(&["github"])
            .inc_by(snapshot.github_errors);
        self.archive_errors
            .with_label_values(&["store"])
            .inc_by(snapshot.store_errors);
    }

    pub fn encode(&self) -> String {
        let mut buf = Vec::new();
        TextEncoder::new()
            .encode(&self.registry.gather(), &mut buf)
            .expect("encode prometheus");
        String::from_utf8(buf).expect("prometheus text is utf-8")
    }
}

/// Keep Prometheus labels bounded even when a producer adds metric names.
fn metric_family(name: &str) -> &'static str {
    let mut parts = name.split('.');
    match (parts.next(), parts.next()) {
        (Some("ci"), Some("run")) => "ci.run",
        (Some("ci"), Some("job")) => "ci.job",
        (Some("ci"), Some("coverage")) => "ci.coverage",
        (Some("ci"), Some("collector")) => "ci.collector",
        (Some("ci"), Some("probe")) => "ci.probe",
        (Some("kache"), Some("bench")) => "kache.bench",
        (Some("kache"), Some("cache")) => "kache.cache",
        (Some("kache"), Some("prefetch")) => "kache.prefetch",
        (Some("kache"), Some("ci")) => "kache.ci",
        _ => "other",
    }
}

#[cfg(test)]
mod tests {
    use super::{Metrics, metric_family};

    /// An alert on rejected artifacts needs the series to exist at zero, and
    /// needs a rejection to move it rather than the skipped count.
    #[test]
    fn rejected_artifacts_have_their_own_series() {
        let metrics = Metrics::new();
        assert!(
            metrics
                .encode()
                .contains("kartero_artifacts_total{outcome=\"rejected\"} 0")
        );
        let mut snapshot = crate::self_telemetry::CollectSnapshot::default();
        metrics.inc_artifact("rejected");
        snapshot.inc_artifact("rejected");
        let encoded = metrics.encode();
        assert!(encoded.contains("kartero_artifacts_total{outcome=\"rejected\"} 1"));
        assert!(encoded.contains("kartero_artifacts_total{outcome=\"skipped\"} 0"));
        assert_eq!((snapshot.rejected, snapshot.skipped), (1, 0));
    }

    #[test]
    fn delivery_family_labels_are_bounded() {
        assert_eq!(metric_family("ci.probe.ms_per_row"), "ci.probe");
        assert_eq!(metric_family("kache.bench.speedup"), "kache.bench");
        assert_eq!(metric_family("vendor.unique.customer_id"), "other");
    }
}
