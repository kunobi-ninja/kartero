use kartero::allowlist::Allowlist;
use kartero::artifact::{METRICS_FILE, SCHEMA_VERSION_FILE, open};
use kartero::otlp::{Envelope, prepare};
use serde_json::Value;
use std::io::{Cursor, Write};
use std::path::PathBuf;
use zip::write::SimpleFileOptions;

fn fixture(path: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(path)
}

#[test]
fn cli_fixture_is_accepted_by_the_rust_consumer() {
    let schema = std::fs::read(fixture("fixtures/coverage/expected/schema_version")).unwrap();
    let metrics = std::fs::read(fixture("fixtures/coverage/expected/metrics.otlp.json")).unwrap();
    let mut buffer = Cursor::new(Vec::new());
    {
        let mut zip = zip::ZipWriter::new(&mut buffer);
        let options = SimpleFileOptions::default();
        zip.start_file(SCHEMA_VERSION_FILE, options).unwrap();
        zip.write_all(&schema).unwrap();
        zip.start_file(METRICS_FILE, options).unwrap();
        zip.write_all(&metrics).unwrap();
        zip.finish().unwrap();
    }

    let payload = open(&buffer.into_inner()).unwrap();
    assert_eq!(payload.schema_version, 1);
    let allowlist = Allowlist::load(&fixture("allowlist.yaml")).unwrap();
    let (prepared, stats) = prepare(
        &payload.metrics_json,
        &allowlist,
        &Envelope {
            pipeline_name: "CI".into(),
            repository_url: "https://github.com/Zondax/kunobi-frontend".into(),
        },
    )
    .unwrap();
    assert_eq!(stats.metrics_kept, 3);
    assert_eq!(stats.metrics_dropped, 0);
    assert_eq!(stats.points_dropped, 0);

    let body: Value = serde_json::from_slice(&prepared).unwrap();
    let names: Vec<_> = body["resourceMetrics"][0]["scopeMetrics"][0]["metrics"]
        .as_array()
        .unwrap()
        .iter()
        .map(|metric| metric["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        [
            "ci.coverage.percent",
            "ci.coverage.covered",
            "ci.coverage.total"
        ]
    );
}

/// Every name the allowlist admitted before it was collapsed into patterns.
///
/// A pattern is only worth having if it covers what the enumeration covered.
/// This is the list as it stood immediately before the change, so a family
/// pattern that is narrower than the names it replaced fails here rather than
/// by a series quietly going missing.
#[test]
fn patterns_admit_every_name_the_enumeration_did() {
    let allowlist = Allowlist::load(&fixture("allowlist.yaml")).unwrap();

    for metric in [
        "kache.bench.build.duration",
        "kache.bench.compile.time_saved",
        "kache.bench.compile.units",
        "kache.bench.cache.hit_rate",
        "kache.bench.cache.weighted_hit_rate",
        "kache.bench.leak_warnings",
        "kache.bench.speedup",
        "kache.bench.cache.size",
        "kache.bench.objdir.size",
        "kache.bench.disk.consumed",
        "kache.bench.key_stability",
        "kache.bench.verdict.ok",
        "kache.ci.coverage.lines",
        "kache.ci.coverage.lines.covered",
        "kache.ci.coverage.lines.total",
        "ci.coverage.percent",
        "ci.coverage.covered",
        "ci.coverage.total",
        "ci.run.attempts",
        "ci.run.gate.duration",
        "ci.run.checks.duration",
        "ci.run.attempt.outcomes",
        "ci.job.duration",
        "ci.job.queue.duration",
        "ci.job.conclusions",
        "ci.job.rerun_recovered",
        "ci.collector.last_success",
        "ci.collector.anomalies",
        "kache.cache.store.size",
        "kache.cache.store.entries",
        "kache.cache.store.max",
        "kache.cache.uploads",
        "kache.cache.uploads.pending",
        "kache.cache.downloads",
        "kache.cache.downloads.active",
        "kache.cache.bytes",
        "kache.cache.s3.concurrency",
        "kache.cache.remote.degraded",
        "kache.cache.remote_checks",
        "kache.cache.negative_hits",
        "kache.cache.negative_entries",
        "kache.prefetch.downloads",
        "kache.prefetch.bytes",
        "kache.prefetch.keys_used",
        "kache.prefetch.keys_cancelled",
        "kache.prefetch.keys_over_budget",
        "kache.prefetch.plans",
        "kache.prefetch.list.requests",
        "kache.prefetch.list.failures",
        "kache.prefetch.pack.requests",
        "kache.prefetch.v3.requests",
        "kache.prefetch.cancelled",
        "kache.prefetch.last_plan.candidates",
        "kache.prefetch.last_plan.wall",
    ] {
        assert!(
            allowlist.allows_metric(metric),
            "{metric} is no longer admitted"
        );
    }

    for attribute in [
        "kache.telemetry.schema_version",
        "kache.bench.git_ref",
        "kache.bench.project",
        "kache.bench.cache_tool",
        "kache.bench.phase",
        "kache.bench.result",
        "kache.cache.remote",
        "kache.cache.scenario",
        "kache.cache.phase",
        "kache.cache.result",
        "kache.cache.direction",
        "kache.cache.limit",
        "kache.prefetch.kind",
        "cicd.pipeline.name",
        "vcs.repository.url.full",
        "service.namespace",
        "service.name",
        "service.version",
        "deployment.environment",
        "telemetry.plane",
        "language",
        "kind",
        "branch_class",
        "repository",
        "workflow_path",
        "event",
        "attempt_class",
        "run_conclusion",
        "conclusion",
        "skip_reason",
        "job_name",
        "runner_pool",
        "did_work",
        "outcome",
        "recovery",
        "mode",
        "anomaly",
    ] {
        assert!(
            allowlist.allows_attribute(attribute),
            "{attribute} is no longer admitted"
        );
    }
}

/// The families are wide, but they are still families.
#[test]
fn patterns_do_not_admit_a_neighbouring_namespace() {
    let allowlist = Allowlist::load(&fixture("allowlist.yaml")).unwrap();
    for stranger in [
        "kache.secret.token",
        "cicd.pipeline.run.id",
        "vcs.ref.head.name",
        "evil.ci.job.duration",
        "ci.job.duration.extra.nested.but.fine",
        "ci",
        "",
    ] {
        if stranger == "ci.job.duration.extra.nested.but.fine" {
            // Deeper nesting inside an admitted family is admitted; that is
            // what a family pattern means.
            assert!(allowlist.allows_metric(stranger));
            continue;
        }
        assert!(
            !allowlist.allows_metric(stranger),
            "{stranger} should not be admitted"
        );
    }
    assert!(!allowlist.allows_attribute("cicd.pipeline.run.id"));
    assert!(!allowlist.allows_attribute("commit_sha"));
    assert!(!allowlist.allows_attribute("actor_login"));
}

/// Every bench project kache actually runs, read from its workflow at the
/// time this was written. Ten of these were missing from the enumeration and
/// each one rejected a whole artifact.
#[test]
fn every_bench_project_kache_runs_is_admitted() {
    let allowlist = Allowlist::load(&fixture("allowlist.yaml")).unwrap();
    for project in [
        "bench-eza",
        "bench-firefox",
        "bench-firefox-pull",
        "bench-firefox-pull-windows",
        "bench-firefox-sccache",
        "bench-firefox-windows",
        "bench-hk",
        "bench-hk-mbx",
        "bench-hk-pull",
        "bench-lance",
        "bench-lance-mbx",
        "bench-llvm",
        "bench-mbx",
        "bench-opendal",
        "bench-opendal-mbx",
        "bench-sccache",
        "bench-substrate",
        "bench-substrate-mbx",
        "bench-surrealdb",
        "bench-surrealdb-mbx",
    ] {
        assert!(allowlist.allows_project(project), "{project} is dropped");
    }
    // A family, not a free pass.
    assert!(!allowlist.allows_project("not-a-bench"));
}

#[test]
fn chart_allowlist_matches_root() {
    let root = std::fs::read_to_string(fixture("allowlist.yaml")).unwrap();
    let chart = std::fs::read_to_string(fixture("charts/kartero/allowlist.yaml")).unwrap();
    assert_eq!(root, chart);
}

#[test]
fn kache_cache_gauges_are_accepted_without_bench_project() {
    let body = serde_json::json!({
        "resourceMetrics": [{
            "resource": {"attributes": [
                {"key": "service.name", "value": {"stringValue": "kache"}},
                {"key": "service.version", "value": {"stringValue": "0.16.1"}},
                {"key": "kache.telemetry.schema_version", "value": {"stringValue": "1"}},
                {"key": "kache.cache.remote", "value": {"stringValue": "s3"}},
                {"key": "kache.cache.scenario", "value": {"stringValue": "bench-firefox"}},
                {"key": "kache.cache.phase", "value": {"stringValue": "warm"}}
            ]},
            "scopeMetrics": [{
                "scope": {"name": "kache.cache", "version": "0.16.1"},
                "metrics": [
                    {
                        "name": "kache.cache.store.size",
                        "unit": "By",
                        "gauge": {"dataPoints": [
                            {"asInt": "1234", "timeUnixNano": "1", "attributes": []}
                        ]}
                    },
                    {
                        "name": "kache.cache.uploads",
                        "unit": "{upload}",
                        "gauge": {"dataPoints": [
                            {"asInt": "10", "timeUnixNano": "1", "attributes": [
                                {"key": "kache.cache.result", "value": {"stringValue": "completed"}}
                            ]}
                        ]}
                    },
                    {
                        "name": "kache.prefetch.plans",
                        "unit": "{plan}",
                        "gauge": {"dataPoints": [
                            {"asInt": "2", "timeUnixNano": "1", "attributes": [
                                {"key": "kache.prefetch.kind", "value": {"stringValue": "advisory"}}
                            ]}
                        ]}
                    }
                ]
            }]
        }]
    });

    let allowlist = Allowlist::load(&fixture("allowlist.yaml")).unwrap();
    let (prepared, stats) = prepare(
        &serde_json::to_vec(&body).unwrap(),
        &allowlist,
        &Envelope {
            pipeline_name: "Bench".into(),
            repository_url: "https://github.com/kunobi-ninja/kache".into(),
        },
    )
    .unwrap();
    assert_eq!(stats.metrics_dropped, 0);
    assert_eq!(stats.points_dropped, 0);
    assert_eq!(stats.metrics_kept, 3);

    let out: Value = serde_json::from_slice(&prepared).unwrap();
    let resource_keys: Vec<_> = out["resourceMetrics"][0]["resource"]["attributes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|attr| attr["key"].as_str().unwrap())
        .collect();
    assert!(resource_keys.contains(&"kache.cache.scenario"));
    assert!(resource_keys.contains(&"kache.cache.phase"));
    assert!(resource_keys.contains(&"cicd.pipeline.name"));
    assert!(resource_keys.contains(&"vcs.repository.url.full"));
    assert!(
        !resource_keys
            .iter()
            .any(|key| key.starts_with("kache.bench."))
    );
}
