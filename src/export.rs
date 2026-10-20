//! Optional export pass: reviewed SigNoz queries, published as JSON to an
//! S3-compatible bucket (Cloudflare R2 in production).
//!
//! The reverse of collect. Collect carries CI telemetry into SigNoz; export
//! carries a fixed set of aggregates back out, to a place a public page can
//! read without credentials. The queries live in the deployment's
//! configuration, so what is published changes only through a reviewed change.
//!
//! Independent of collect and archive: its own failure domain and no ledger.
//! Each pass rewrites every document whole, so a failed pass is repaired by the
//! next one rather than by replaying anything.

use crate::config::{Config, ExportConfig, ExportJob, ExportQuery, StoreConfig};
use crate::metrics::Metrics;
use anyhow::{Context, Result, bail};
use rusty_s3::{Bucket, Credentials, S3Action, UrlStyle};
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tracing::{info, warn};

/// Version of the published document. A reader checks it before trusting the
/// shape; a change to the shape is a new value, never an edit to this one.
pub const SCHEMA: &str = "kartero.export.v1";

/// How long a signed upload URL stays valid. It is used at once, so this only
/// has to cover clock skew between the pod and the store.
const PRESIGN_TTL: Duration = Duration::from_secs(300);

/// SigNoz answers a 90-day daily query in under a second; a minute means it is
/// stuck, not slow.
const QUERY_TIMEOUT: Duration = Duration::from_secs(60);

pub async fn export_once(config: &Config) -> Result<()> {
    let Some(export) = config.export.as_ref() else {
        return Ok(());
    };
    let started = Instant::now();
    let result = export_inner(export).await;
    Metrics::global().observe_export(result.is_ok(), started.elapsed().as_secs_f64());
    result
}

async fn export_inner(export: &ExportConfig) -> Result<()> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(10))
        .timeout(QUERY_TIMEOUT)
        .build()
        .context("building HTTP client for export")?;
    let store = Store::new(&export.store)?;
    let mut failed = Vec::new();
    for job in &export.jobs {
        // One job failing must not hold back the others: they publish
        // unrelated documents, and the next pass retries this one anyway.
        match export_job(&client, export, &store, job).await {
            Ok(series) => {
                Metrics::global().record_export_job(&job.name, true);
                info!(export = %job.name, series, "export: published");
            }
            Err(err) => {
                Metrics::global().record_export_job(&job.name, false);
                warn!(export = %job.name, error = %err, "export failed");
                failed.push(job.name.clone());
            }
        }
    }
    if !failed.is_empty() {
        bail!("export failed for {}", failed.join(", "));
    }
    Ok(())
}

async fn export_job(
    client: &reqwest::Client,
    export: &ExportConfig,
    store: &Store,
    job: &ExportJob,
) -> Result<usize> {
    let now_ms = unix_ms(SystemTime::now());
    let end = now_ms;
    let start = end.saturating_sub(duration_ms(job.window));
    let mut queries = BTreeMap::new();
    let mut series_count = 0;
    for query in &job.queries {
        let body = query_body(query, start, end, job.step);
        let response = run_query(client, export, &body)
            .await
            .with_context(|| format!("query {}", query.id))?;
        let series = parse_series(&response, job.include_partial)
            .with_context(|| format!("reading the answer to query {}", query.id))?;
        // An empty answer is what a renamed metric or a filter gone wrong looks
        // like. Publishing it would replace a working page with a blank one, so
        // the last good document stays until the query finds data again.
        if series.is_empty() {
            bail!("query {} returned no series", query.id);
        }
        series_count += series.len();
        queries.insert(query.id.clone(), QueryResult { series });
    }
    let document = Document {
        schema: SCHEMA,
        export: &job.name,
        generated_at: rfc3339(now_ms),
        start,
        end,
        step_seconds: job.step.as_secs(),
        queries,
    };
    let body = serde_json::to_vec(&document).context("serialising export document")?;
    store
        .put(client, &job.key, body.clone(), &job.cache_control)
        .await
        .with_context(|| format!("uploading {}", job.key))?;
    if let Some(template) = &job.snapshot_key {
        let key = template.replace("{date}", &utc_date(now_ms));
        store
            .put(client, &key, body, &job.cache_control)
            .await
            .with_context(|| format!("uploading {key}"))?;
    }
    Ok(series_count)
}

/// The request body for one query. Kartero owns `name` and `stepInterval`:
/// the name because each query runs alone, the step because every query in a
/// job shares it.
fn query_body(query: &ExportQuery, start: u64, end: u64, step: Duration) -> Value {
    let mut spec = query.spec.clone();
    spec.insert("name".into(), json!("A"));
    spec.insert("stepInterval".into(), json!(step.as_secs()));
    json!({
        "start": start,
        "end": end,
        "requestType": "time_series",
        "compositeQuery": {
            "queries": [{ "type": "builder_query", "spec": spec }]
        }
    })
}

async fn run_query(client: &reqwest::Client, export: &ExportConfig, body: &Value) -> Result<Value> {
    let url = format!("{}/api/v5/query_range", export.signoz_endpoint);
    let response = client
        .post(&url)
        .header("SIGNOZ-API-KEY", export.signoz_api_key.expose())
        .json(body)
        .send()
        .await
        .context("SigNoz query request failed")?;
    let status = response.status();
    let text = response.text().await.context("reading SigNoz answer")?;
    if !status.is_success() {
        bail!("SigNoz answered {status}: {}", truncate(&text));
    }
    let value: Value = serde_json::from_str(&text).context("SigNoz answer is not JSON")?;
    if value.get("status").and_then(Value::as_str) != Some("success") {
        bail!("SigNoz query did not succeed: {}", truncate(&text));
    }
    Ok(value)
}

#[derive(Debug, Serialize)]
struct Document<'a> {
    schema: &'static str,
    export: &'a str,
    generated_at: String,
    /// Unix milliseconds, as SigNoz takes and returns them.
    start: u64,
    end: u64,
    step_seconds: u64,
    queries: BTreeMap<String, QueryResult>,
}

#[derive(Debug, Serialize, PartialEq)]
struct QueryResult {
    series: Vec<Series>,
}

#[derive(Debug, Serialize, PartialEq)]
struct Series {
    labels: BTreeMap<String, String>,
    /// `[unix_ms, value]`, oldest first.
    points: Vec<(i64, f64)>,
}

/// Flatten SigNoz's answer into series ordered by their labels, so two passes
/// over the same data write the same bytes.
///
/// Points SigNoz marks `partial` are dropped unless the job keeps them, and so
/// is any value that is not a finite number, which JSON cannot carry.
fn parse_series(response: &Value, include_partial: bool) -> Result<Vec<Series>> {
    let results = response
        .pointer("/data/data/results")
        .and_then(Value::as_array)
        .context("answer has no data.data.results")?;
    let mut out = Vec::new();
    for result in results {
        let aggregations = result
            .get("aggregations")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default();
        for aggregation in aggregations {
            let series = aggregation
                .get("series")
                .and_then(Value::as_array)
                .map(Vec::as_slice)
                .unwrap_or_default();
            for raw in series {
                let labels = raw
                    .get("labels")
                    .and_then(Value::as_array)
                    .map(Vec::as_slice)
                    .unwrap_or_default()
                    .iter()
                    .filter_map(|label| {
                        let name = label.pointer("/key/name")?.as_str()?;
                        let value = match label.get("value")? {
                            Value::String(text) => text.clone(),
                            other => other.to_string(),
                        };
                        Some((name.to_string(), value))
                    })
                    .collect();
                let mut points: Vec<(i64, f64)> = raw
                    .get("values")
                    .and_then(Value::as_array)
                    .map(Vec::as_slice)
                    .unwrap_or_default()
                    .iter()
                    .filter(|point| {
                        include_partial
                            || !point
                                .get("partial")
                                .and_then(Value::as_bool)
                                .unwrap_or(false)
                    })
                    .filter_map(|point| {
                        let timestamp = point.get("timestamp")?.as_i64()?;
                        let value = point.get("value")?.as_f64()?;
                        value.is_finite().then_some((timestamp, value))
                    })
                    .collect();
                points.sort_by_key(|(timestamp, _)| *timestamp);
                if !points.is_empty() {
                    out.push(Series { labels, points });
                }
            }
        }
    }
    out.sort_by(|a, b| a.labels.cmp(&b.labels));
    Ok(out)
}

struct Store {
    bucket: Bucket,
    credentials: Credentials,
}

impl Store {
    fn new(config: &StoreConfig) -> Result<Self> {
        let endpoint = config
            .endpoint
            .parse()
            .context("export store endpoint is not a URL")?;
        let bucket = Bucket::new(
            endpoint,
            UrlStyle::Path,
            config.bucket.clone(),
            config.region.clone(),
        )
        .context("export store bucket")?;
        Ok(Self {
            bucket,
            credentials: Credentials::new(
                config.access_key_id.expose(),
                config.secret_access_key.expose(),
            ),
        })
    }

    async fn put(
        &self,
        client: &reqwest::Client,
        key: &str,
        body: Vec<u8>,
        cache_control: &str,
    ) -> Result<()> {
        let mut action = self.bucket.put_object(Some(&self.credentials), key);
        // Signed, so the store keeps exactly these and a page reads them back.
        action
            .headers_mut()
            .insert("content-type", "application/json");
        action.headers_mut().insert("cache-control", cache_control);
        let url = action.sign(PRESIGN_TTL);
        let response = client
            .put(url)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .header(reqwest::header::CACHE_CONTROL, cache_control)
            .body(body)
            .send()
            .await
            .context("store request failed")?;
        let status = response.status();
        if !status.is_success() {
            let text = response.text().await.unwrap_or_default();
            bail!("store answered {status}: {}", truncate(&text));
        }
        Ok(())
    }
}

/// Error bodies end up in logs. Long ones say nothing more past their start.
fn truncate(text: &str) -> &str {
    let end = text
        .char_indices()
        .nth(300)
        .map_or(text.len(), |(index, _)| index);
    &text[..end]
}

fn unix_ms(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}

fn duration_ms(duration: Duration) -> u64 {
    duration.as_millis() as u64
}

fn timestamp(ms: u64) -> jiff::Timestamp {
    jiff::Timestamp::from_millisecond(ms as i64).unwrap_or(jiff::Timestamp::UNIX_EPOCH)
}

/// Whole seconds: a reader compares it with a date, never with another pass.
fn rfc3339(ms: u64) -> String {
    timestamp(ms - ms % 1000).to_string()
}

fn utc_date(ms: u64) -> String {
    timestamp(ms)
        .to_zoned(jiff::tz::TimeZone::UTC)
        .date()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Secret, SourceConfig};
    use axum::Router;
    use axum::body::Bytes;
    use axum::extract::{Request, State};
    use axum::http::{HeaderMap, StatusCode};
    use axum::routing::{post, put};
    use std::sync::{Arc, Mutex};

    /// The shape SigNoz's v5 `query_range` answered for a daily kache bench
    /// query, trimmed to two series. The last point is today's open bucket.
    fn answer() -> Value {
        json!({
            "status": "success",
            "data": {
                "type": "time_series",
                "data": {
                    "results": [{
                        "queryName": "A",
                        "aggregations": [{
                            "index": 0,
                            "series": [
                                {
                                    "labels": [{ "key": { "name": "kache.bench.phase" }, "value": "warm" }],
                                    "values": [
                                        { "timestamp": 1_792_368_000_000_i64, "value": 43.048 },
                                        { "timestamp": 1_792_281_600_000_i64, "value": 43.801 },
                                        { "timestamp": 1_792_454_400_000_i64, "partial": true, "value": 50.625 }
                                    ]
                                },
                                {
                                    "labels": [{ "key": { "name": "kache.bench.phase" }, "value": "cold" }],
                                    "values": [
                                        { "timestamp": 1_792_368_000_000_i64, "value": 1301.259 },
                                        { "timestamp": 1_792_454_400_000_i64, "partial": true, "value": 1615.008 }
                                    ]
                                }
                            ]
                        }]
                    }]
                }
            }
        })
    }

    fn phase(value: &str) -> BTreeMap<String, String> {
        BTreeMap::from([("kache.bench.phase".to_string(), value.to_string())])
    }

    #[test]
    fn open_buckets_are_dropped_and_series_are_ordered() {
        let series = parse_series(&answer(), false).unwrap();
        assert_eq!(
            series,
            vec![
                Series {
                    labels: phase("cold"),
                    points: vec![(1_792_368_000_000, 1301.259)],
                },
                Series {
                    labels: phase("warm"),
                    points: vec![(1_792_281_600_000, 43.801), (1_792_368_000_000, 43.048)],
                },
            ]
        );
    }

    #[test]
    fn a_job_that_keeps_open_buckets_gets_them() {
        let series = parse_series(&answer(), true).unwrap();
        assert_eq!(
            series[0].points.last(),
            Some(&(1_792_454_400_000, 1615.008))
        );
        assert_eq!(series[1].points.len(), 3);
    }

    #[test]
    fn a_series_with_no_usable_point_is_left_out() {
        let answer = json!({
            "status": "success",
            "data": { "data": { "results": [{ "aggregations": [{ "series": [
                { "labels": [], "values": [{ "timestamp": 1, "value": "NaN" }] },
                { "labels": [], "values": [{ "timestamp": 2, "partial": true, "value": 1.0 }] }
            ]}]}]}}
        });
        assert!(parse_series(&answer, false).unwrap().is_empty());
    }

    #[test]
    fn an_answer_without_results_is_an_error_rather_than_an_empty_document() {
        let answer = json!({ "status": "success", "data": {} });
        assert!(parse_series(&answer, false).is_err());
    }

    #[test]
    fn kartero_sets_the_query_name_and_step() {
        let query = ExportQuery {
            id: "duration".into(),
            spec: serde_json::from_value(json!({
                "signal": "metrics",
                "aggregations": [{ "metricName": "kache.bench.build.duration" }]
            }))
            .unwrap(),
        };
        let body = query_body(&query, 10, 20, Duration::from_secs(86_400));
        assert_eq!(body["start"], 10);
        assert_eq!(body["end"], 20);
        assert_eq!(body["requestType"], "time_series");
        let spec = &body["compositeQuery"]["queries"][0]["spec"];
        assert_eq!(spec["name"], "A");
        assert_eq!(spec["stepInterval"], 86_400);
        assert_eq!(spec["signal"], "metrics");
    }

    #[test]
    fn dates_and_times_are_utc() {
        // 2026-10-19T23:30:00.250Z
        let ms = 1_792_452_600_250;
        assert_eq!(utc_date(ms), "2026-10-19");
        assert_eq!(rfc3339(ms), "2026-10-19T23:30:00Z");
    }

    #[derive(Default)]
    struct Seen {
        api_keys: Vec<String>,
        uploads: Vec<(String, String, String, Vec<u8>)>,
    }

    async fn spawn(app: Router) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{addr}")
    }

    fn config(signoz: String, store: String) -> Config {
        Config {
            bind: "127.0.0.1:0".into(),
            interval: Duration::from_secs(3600),
            heartbeat_interval: Duration::from_secs(60),
            lookback: Duration::from_secs(86_400),
            sources: vec![SourceConfig {
                token: "token".into(),
                owner: "kunobi-ninja".into(),
                repo: "kache".into(),
                workflows: vec!["bench.yml".into()],
                trusted_branch: "main".into(),
                actions: None,
            }],
            otlp_endpoint: "http://127.0.0.1:4318".into(),
            otlp_readiness_url: None,
            github_api: crate::github::API.into(),
            allowlist_path: "/etc/kartero/allowlist.yaml".into(),
            ledger_path: "/tmp/ledger.sqlite".into(),
            artifact_prefix: "telemetry-otlp-v1".into(),
            archive: None,
            export: Some(ExportConfig {
                signoz_endpoint: signoz,
                signoz_api_key: Secret::for_tests("signoz-key"),
                store: StoreConfig {
                    endpoint: store,
                    bucket: "public".into(),
                    region: "auto".into(),
                    access_key_id: Secret::for_tests("id"),
                    secret_access_key: Secret::for_tests("secret"),
                },
                jobs: vec![ExportJob {
                    name: "kache-bench".into(),
                    key: "kache/bench/v1/latest.json".into(),
                    snapshot_key: Some("kache/bench/v1/days/{date}.json".into()),
                    window: Duration::from_secs(90 * 86_400),
                    step: Duration::from_secs(86_400),
                    include_partial: false,
                    cache_control: "public, max-age=300".into(),
                    queries: vec![ExportQuery {
                        id: "duration".into(),
                        spec: serde_json::from_value(json!({
                            "signal": "metrics",
                            "aggregations": [{ "metricName": "kache.bench.build.duration" }]
                        }))
                        .unwrap(),
                    }],
                }],
            }),
        }
    }

    #[tokio::test]
    async fn a_pass_queries_signoz_and_writes_the_latest_and_the_days_snapshot() {
        let seen = Arc::new(Mutex::new(Seen::default()));
        let signoz = spawn(
            Router::new()
                .route(
                    "/api/v5/query_range",
                    post(
                        |State(seen): State<Arc<Mutex<Seen>>>, headers: HeaderMap| async move {
                            let key = headers
                                .get("SIGNOZ-API-KEY")
                                .and_then(|value| value.to_str().ok())
                                .unwrap_or_default()
                                .to_string();
                            seen.lock().unwrap().api_keys.push(key);
                            axum::Json(answer())
                        },
                    ),
                )
                .with_state(seen.clone()),
        )
        .await;
        let store = spawn(
            Router::new()
                .route(
                    "/{*path}",
                    put(
                        |State(seen): State<Arc<Mutex<Seen>>>, request: Request| async move {
                            let path = request.uri().path().to_string();
                            let query = request.uri().query().unwrap_or_default().to_string();
                            let cache = request
                                .headers()
                                .get("cache-control")
                                .and_then(|value| value.to_str().ok())
                                .unwrap_or_default()
                                .to_string();
                            let body: Bytes = axum::body::to_bytes(request.into_body(), usize::MAX)
                                .await
                                .unwrap();
                            seen.lock()
                                .unwrap()
                                .uploads
                                .push((path, query, cache, body.to_vec()));
                            StatusCode::OK
                        },
                    ),
                )
                .with_state(seen.clone()),
        )
        .await;

        export_once(&config(signoz, store)).await.unwrap();

        let seen = seen.lock().unwrap();
        assert_eq!(seen.api_keys, ["signoz-key"]);
        assert_eq!(seen.uploads.len(), 2);
        let (latest_path, latest_query, cache, body) = &seen.uploads[0];
        assert_eq!(latest_path, "/public/kache/bench/v1/latest.json");
        assert!(latest_query.contains("X-Amz-Signature="), "{latest_query}");
        assert_eq!(cache, "public, max-age=300");
        let (snapshot_path, _, _, snapshot_body) = &seen.uploads[1];
        let today = utc_date(unix_ms(SystemTime::now()));
        assert_eq!(
            snapshot_path,
            &format!("/public/kache/bench/v1/days/{today}.json")
        );
        assert_eq!(snapshot_body, body);

        let document: Value = serde_json::from_slice(body).unwrap();
        assert_eq!(document["schema"], SCHEMA);
        assert_eq!(document["export"], "kache-bench");
        assert_eq!(document["step_seconds"], 86_400);
        let series = &document["queries"]["duration"]["series"];
        assert_eq!(series[0]["labels"]["kache.bench.phase"], "cold");
        assert_eq!(
            series[0]["points"],
            json!([[1_792_368_000_000_i64, 1301.259]])
        );
    }

    #[tokio::test]
    async fn a_refused_upload_fails_the_pass() {
        let signoz = spawn(Router::new().route(
            "/api/v5/query_range",
            post(|| async { axum::Json(answer()) }),
        ))
        .await;
        let store = spawn(Router::new().route(
            "/{*path}",
            put(|| async {
                (
                    StatusCode::FORBIDDEN,
                    "<Error><Code>AccessDenied</Code></Error>",
                )
            }),
        ))
        .await;
        let err = export_once(&config(signoz, store)).await.unwrap_err();
        assert!(err.to_string().contains("kache-bench"), "{err:#}");
    }

    #[tokio::test]
    async fn an_empty_answer_keeps_the_last_published_document() {
        let uploads = Arc::new(Mutex::new(0_usize));
        let signoz = spawn(Router::new().route(
            "/api/v5/query_range",
            post(|| async {
                axum::Json(json!({
                    "status": "success",
                    "data": { "data": { "results": [{ "aggregations": [{ "series": [] }] }] } }
                }))
            }),
        ))
        .await;
        let store = spawn(
            Router::new()
                .route(
                    "/{*path}",
                    put(|State(uploads): State<Arc<Mutex<usize>>>| async move {
                        *uploads.lock().unwrap() += 1;
                        StatusCode::OK
                    }),
                )
                .with_state(uploads.clone()),
        )
        .await;
        assert!(export_once(&config(signoz, store)).await.is_err());
        assert_eq!(*uploads.lock().unwrap(), 0);
    }

    #[tokio::test]
    async fn a_failed_query_publishes_nothing() {
        let uploads = Arc::new(Mutex::new(0_usize));
        let signoz = spawn(Router::new().route(
            "/api/v5/query_range",
            post(|| async { (StatusCode::BAD_REQUEST, r#"{"status":"error"}"#) }),
        ))
        .await;
        let store = spawn(
            Router::new()
                .route(
                    "/{*path}",
                    put(|State(uploads): State<Arc<Mutex<usize>>>| async move {
                        *uploads.lock().unwrap() += 1;
                        StatusCode::OK
                    }),
                )
                .with_state(uploads.clone()),
        )
        .await;
        assert!(export_once(&config(signoz, store)).await.is_err());
        assert_eq!(*uploads.lock().unwrap(), 0);
    }
}
