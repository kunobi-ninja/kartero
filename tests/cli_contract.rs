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
