use crate::self_telemetry::{ArchiveSnapshot, CollectSnapshot};
use prometheus::{
    Encoder, Histogram, IntCounterVec, IntGauge, Registry, TextEncoder, histogram_opts, opts,
};
use std::sync::OnceLock;

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
            "GitHub workflows configured as telemetry sources.",
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
        for outcome in ["delivered", "skipped", "held", "retryable"] {
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
            "Wall time of one archive pass, including GitHub download and object PUT.",
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
            archive_artifacts,
            archive_passes,
            archive_duration,
            archive_errors,
        }
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
