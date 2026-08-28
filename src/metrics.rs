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
        Self {
            registry,
            artifacts,
            series_dropped,
            series_kept,
            collect_passes,
            collect_duration,
            last_collect_unix,
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

    pub fn observe_collect(&self, duration_s: f64, ok: bool) {
        self.collect_duration.observe(duration_s);
        self.collect_passes
            .with_label_values(&[if ok { "ok" } else { "error" }])
            .inc();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        self.last_collect_unix.set(now);
    }

    pub fn encode(&self) -> String {
        let mut buf = Vec::new();
        TextEncoder::new()
            .encode(&self.registry.gather(), &mut buf)
            .expect("encode prometheus");
        String::from_utf8(buf).expect("prometheus text is utf-8")
    }
}
