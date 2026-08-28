//! Kartero's own OTLP gauges. Posted to the same backend it delivers
//! producer payloads to, so Signoz sees the collector even when scrape
//! annotations are inert.
//!
//! Gauges, one observation per collect pass. No OpenTelemetry SDK.

use crate::VERSION;
use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Default, Clone)]
pub struct CollectSnapshot {
    pub duration_s: f64,
    pub ok: bool,
    pub delivered: u64,
    pub skipped: u64,
    pub held: u64,
    pub retryable: u64,
    pub metrics_kept: u64,
    pub metrics_dropped: u64,
    pub points_dropped: u64,
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
    let url = format!("{}/v1/metrics", endpoint.trim_end_matches('/'));
    let body = serde_json::to_vec(&serialize(snapshot))?;
    let response = client
        .post(url)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body(body)
        .send()
        .await
        .context("posting kartero self-telemetry")?;
    let status = response.status();
    if status.is_success() {
        return Ok(());
    }
    anyhow::bail!("kartero self-telemetry rejected with {status}");
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
            delivered: 2,
            skipped: 1,
            held: 0,
            retryable: 1,
            metrics_kept: 12,
            metrics_dropped: 3,
            points_dropped: 4,
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
        let dumped = body.to_string();
        assert!(!dumped.contains("cicd."));
        assert!(!dumped.contains("run_id"));
        let duration =
            &body["resourceMetrics"][0]["scopeMetrics"][0]["metrics"][0]["gauge"]["dataPoints"][0];
        assert!(duration["timeUnixNano"].is_string());
        assert_eq!(duration["asDouble"], 1.5);
    }
}
