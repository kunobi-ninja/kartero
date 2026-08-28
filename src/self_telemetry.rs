//! Kartero's own OTLP gauges. Posted to the same backend it delivers
//! producer payloads to, so Signoz sees the collector even when scrape
//! annotations are inert.
//!
//! Gauges for collect passes and process heartbeats. No OpenTelemetry SDK.

use crate::VERSION;
use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Default, Clone)]
pub struct CollectSnapshot {
    pub duration_s: f64,
    pub ok: bool,
    pub sources: u64,
    pub runs_seen: u64,
    pub runs_trusted: u64,
    pub artifacts_seen: u64,
    pub artifacts_matched: u64,
    pub delivered: u64,
    pub skipped: u64,
    pub held: u64,
    pub retryable: u64,
    pub metrics_kept: u64,
    pub metrics_dropped: u64,
    pub points_dropped: u64,
    pub github_errors: u64,
    pub ingest_errors: u64,
}

#[derive(Debug, Clone, Copy)]
pub struct ProcessSnapshot {
    pub start_time_unix_s: u64,
    pub uptime_s: f64,
}

impl CollectSnapshot {
    pub fn inc_artifact(&mut self, outcome: &str) {
        match outcome {
            "delivered" => self.delivered += 1,
            "skipped" => self.skipped += 1,
            "held" => self.held += 1,
            "retryable" => self.retryable += 1,
            _ => {}
        }
    }
}

pub fn serialize(snapshot: &CollectSnapshot) -> Value {
    let time = now_unix_nano();
    let run_attrs: Vec<Value> = Vec::new();
    json!({
        "resourceMetrics": [{
            "resource": {
                "attributes": [
                    str_attr("service.name", "kartero"),
                    str_attr("service.version", VERSION),
                ]
            },
            "scopeMetrics": [{
                "scope": { "name": "kartero", "version": VERSION },
                "metrics": [
                    gauge("kartero.collect.duration", "s", vec![as_double(snapshot.duration_s, &time, &run_attrs)]),
                    gauge("kartero.collect.ok", "1", vec![as_int(u64::from(snapshot.ok), &time, &run_attrs)]),
                    gauge("kartero.collect.sources", "{source}", vec![as_int(snapshot.sources, &time, &run_attrs)]),
                    gauge("kartero.collect.runs", "{run}", vec![
                        as_int(snapshot.runs_seen, &time, &[str_attr("kartero.run.state", "seen")]),
                        as_int(snapshot.runs_trusted, &time, &[str_attr("kartero.run.state", "trusted")]),
                    ]),
                    gauge("kartero.collect.artifacts_discovered", "{artifact}", vec![
                        as_int(snapshot.artifacts_seen, &time, &[str_attr("kartero.discovery.state", "seen")]),
                        as_int(snapshot.artifacts_matched, &time, &[str_attr("kartero.discovery.state", "matched")]),
                    ]),
                    gauge("kartero.collect.artifacts", "{artifact}", vec![
                        as_int(snapshot.delivered, &time, &[str_attr("kartero.artifact.outcome", "delivered")]),
                        as_int(snapshot.skipped, &time, &[str_attr("kartero.artifact.outcome", "skipped")]),
                        as_int(snapshot.held, &time, &[str_attr("kartero.artifact.outcome", "held")]),
                        as_int(snapshot.retryable, &time, &[str_attr("kartero.artifact.outcome", "retryable")]),
                    ]),
                    gauge("kartero.collect.series_kept", "{series}", vec![as_int(snapshot.metrics_kept, &time, &run_attrs)]),
                    gauge("kartero.collect.series_dropped", "{series}", vec![
                        as_int(snapshot.metrics_dropped, &time, &[str_attr("kartero.drop.kind", "metric")]),
                        as_int(snapshot.points_dropped, &time, &[str_attr("kartero.drop.kind", "point")]),
                    ]),
                    gauge("kartero.collect.errors", "{error}", vec![
                        as_int(snapshot.github_errors, &time, &[str_attr("kartero.error.component", "github")]),
                        as_int(snapshot.ingest_errors, &time, &[str_attr("kartero.error.component", "ingest")]),
                    ]),
                ]
            }]
        }]
    })
}

pub fn serialize_process(snapshot: &ProcessSnapshot) -> Value {
    let time = now_unix_nano();
    let attrs: Vec<Value> = Vec::new();
    json!({
        "resourceMetrics": [{
            "resource": {
                "attributes": [
                    str_attr("service.name", "kartero"),
                    str_attr("service.version", VERSION),
                ]
            },
            "scopeMetrics": [{
                "scope": { "name": "kartero", "version": VERSION },
                "metrics": [
                    gauge("kartero.process.up", "1", vec![as_int(1, &time, &attrs)]),
                    gauge("kartero.process.uptime", "s", vec![as_double(snapshot.uptime_s, &time, &attrs)]),
                    gauge(
                        "kartero.process.start_time",
                        "s",
                        vec![as_int(snapshot.start_time_unix_s, &time, &attrs)],
                    ),
                ]
            }]
        }]
    })
}

pub async fn post(
    client: &reqwest::Client,
    endpoint: &str,
    snapshot: &CollectSnapshot,
) -> Result<()> {
    post_body(
        client,
        endpoint,
        serialize(snapshot),
        "kartero self-telemetry",
    )
    .await
}

pub async fn post_process(
    client: &reqwest::Client,
    endpoint: &str,
    snapshot: &ProcessSnapshot,
) -> Result<()> {
    post_body(
        client,
        endpoint,
        serialize_process(snapshot),
        "kartero heartbeat",
    )
    .await
}

async fn post_body(
    client: &reqwest::Client,
    endpoint: &str,
    body: Value,
    operation: &str,
) -> Result<()> {
    let url = format!("{}/v1/metrics", endpoint.trim_end_matches('/'));
    let response = client
        .post(url)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body(serde_json::to_vec(&body)?)
        .send()
        .await
        .with_context(|| format!("posting {operation}"))?;
    let status = response.status();
    if status.is_success() {
        return Ok(());
    }
    anyhow::bail!("{operation} rejected with {status}");
}

fn now_unix_nano() -> String {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
        .to_string()
}

fn gauge(name: &str, unit: &str, data_points: Vec<Value>) -> Value {
    json!({ "name": name, "unit": unit, "gauge": { "dataPoints": data_points } })
}

fn str_attr(key: &str, value: &str) -> Value {
    json!({"key": key, "value": {"stringValue": value}})
}

fn as_double(value: f64, time_unix_nano: &str, attributes: &[Value]) -> Value {
    json!({
        "asDouble": value,
        "timeUnixNano": time_unix_nano,
        "attributes": attributes,
    })
}

fn as_int(value: u64, time_unix_nano: &str, attributes: &[Value]) -> Value {
    json!({
        "asInt": value.to_string(),
        "timeUnixNano": time_unix_nano,
        "attributes": attributes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn self_payload_is_gauges_under_service_kartero() {
        let body = serialize(&CollectSnapshot {
            duration_s: 1.5,
            ok: true,
            sources: 2,
            runs_seen: 10,
            runs_trusted: 2,
            artifacts_seen: 4,
            artifacts_matched: 3,
            delivered: 2,
            skipped: 1,
            held: 0,
            retryable: 1,
            metrics_kept: 12,
            metrics_dropped: 3,
            points_dropped: 4,
            github_errors: 1,
            ingest_errors: 2,
        });
        assert_eq!(
            body["resourceMetrics"][0]["resource"]["attributes"][0]["value"]["stringValue"],
            "kartero"
        );
        let names: Vec<_> = body["resourceMetrics"][0]["scopeMetrics"][0]["metrics"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["name"].as_str().unwrap())
            .collect();
        assert!(names.contains(&"kartero.collect.duration"));
        assert!(names.contains(&"kartero.collect.series_dropped"));
        assert!(names.contains(&"kartero.collect.sources"));
        assert!(names.contains(&"kartero.collect.errors"));
        let dumped = body.to_string();
        assert!(!dumped.contains("cicd."));
        assert!(!dumped.contains("run_id"));
        let duration =
            &body["resourceMetrics"][0]["scopeMetrics"][0]["metrics"][0]["gauge"]["dataPoints"][0];
        assert!(duration["timeUnixNano"].is_string());
        assert_eq!(duration["asDouble"], 1.5);
    }

    #[test]
    fn process_payload_reports_start_and_heartbeat() {
        let body = serialize_process(&ProcessSnapshot {
            start_time_unix_s: 1_700_000_000,
            uptime_s: 12.5,
        });
        let metrics = body["resourceMetrics"][0]["scopeMetrics"][0]["metrics"]
            .as_array()
            .unwrap();
        let names: Vec<_> = metrics
            .iter()
            .map(|metric| metric["name"].as_str().unwrap())
            .collect();
        assert_eq!(
            names,
            [
                "kartero.process.up",
                "kartero.process.uptime",
                "kartero.process.start_time",
            ]
        );
        assert_eq!(metrics[0]["gauge"]["dataPoints"][0]["asInt"], "1");
        assert_eq!(metrics[1]["gauge"]["dataPoints"][0]["asDouble"], 12.5);
        assert_eq!(metrics[2]["gauge"]["dataPoints"][0]["asInt"], "1700000000");
    }
}
