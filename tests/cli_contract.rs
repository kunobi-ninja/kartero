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
