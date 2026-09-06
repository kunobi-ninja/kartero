//! Derived points to an OTLP/HTTP request body.
//!
//! The same wire shape the artifact path carries, built here rather than read
//! from a producer — so it goes through `otlp::prepare` afterwards like any
//! other payload, and is subject to the same allowlist. Deriving a metric does
//! not exempt it from review.

use super::jobs::bounds_for;
use super::model::{Instrument, Point};
use serde_json::{Value, json};

/// The plane these belong to. `ci.*` sits outside any product namespace, so
/// nobody reading a dashboard can mistake a pipeline metric for a product one.
const RESOURCE: [(&str, &str); 4] = [
    ("service.namespace", "kunobi"),
    ("service.name", "github-actions-ci"),
    ("deployment.environment", "ci"),
    ("telemetry.plane", "engineering"),
];

/// Place one observation into explicit buckets. OTLP requires exactly one more
/// bucket count than bound, the last being the overflow above the final bound.
fn bucketise(value: f64, bounds: &[f64]) -> Vec<String> {
    let mut counts = vec![0u64; bounds.len() + 1];
    let index = bounds
        .iter()
        .position(|bound| value <= *bound)
        .unwrap_or(bounds.len());
    counts[index] = 1;
    counts.into_iter().map(|c| c.to_string()).collect()
}

fn attributes(point: &Point) -> Vec<Value> {
    point
        .attributes
        .iter()
        .map(|(key, value)| json!({"key": key, "value": {"stringValue": value}}))
        .collect()
}

/// One OTLP body for a whole sweep.
///
/// Every point carries the instant it describes rather than the moment of
/// collection, which is what lets one request hold attempts that finished days
/// apart. Delta temporality, because a sweep reports what happened in a window
/// and the producer-side ledger is what stops a window being reported twice.
pub fn to_otlp(points: &[Point], observed: &[f64]) -> Value {
    debug_assert_eq!(points.len(), observed.len());
    let mut by_metric: Vec<(&'static str, Instrument, &'static str, Vec<usize>)> = Vec::new();
    for (index, point) in points.iter().enumerate() {
        match by_metric
            .iter_mut()
            .find(|(name, ..)| *name == point.metric)
        {
            Some((_, _, _, indices)) => indices.push(index),
            None => by_metric.push((point.metric, point.instrument, point.unit, vec![index])),
        }
    }

    let metrics: Vec<Value> = by_metric
        .into_iter()
        .map(|(name, instrument, unit, indices)| {
            let data_points: Vec<Value> = indices
                .iter()
                .map(|index| {
                    let point = &points[*index];
                    let nanos = (observed[*index] * 1_000_000_000.0) as i64;
                    let stamp = nanos.max(0).to_string();
                    match instrument {
                        Instrument::Counter => json!({
                            "asInt": (point.value as i64).to_string(),
                            "startTimeUnixNano": stamp,
                            "timeUnixNano": stamp,
                            "attributes": attributes(point),
                        }),
                        Instrument::Histogram => {
                            let bounds = bounds_for(name);
                            json!({
                                "count": "1",
                                "sum": point.value,
                                "bucketCounts": bucketise(point.value, bounds),
                                "explicitBounds": bounds,
                                "startTimeUnixNano": stamp,
                                "timeUnixNano": stamp,
                                "attributes": attributes(point),
                            })
                        }
                    }
                })
                .collect();
            match instrument {
                Instrument::Counter => json!({
                    "name": name,
                    "unit": unit,
                    "sum": {
                        "aggregationTemporality": "AGGREGATION_TEMPORALITY_DELTA",
                        "isMonotonic": true,
                        "dataPoints": data_points,
                    }
                }),
                Instrument::Histogram => json!({
                    "name": name,
                    "unit": unit,
                    "histogram": {
                        "aggregationTemporality": "AGGREGATION_TEMPORALITY_DELTA",
                        "dataPoints": data_points,
                    }
                }),
            }
        })
        .collect();

    json!({
        "resourceMetrics": [{
            "resource": {
                "attributes": RESOURCE
                    .iter()
                    .map(|(key, value)| json!({"key": key, "value": {"stringValue": value}}))
                    .collect::<Vec<_>>()
            },
            "scopeMetrics": [{
                "scope": {"name": "kartero.actions", "version": crate::VERSION},
                "metrics": metrics,
            }]
        }]
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn counter(metric: &'static str) -> Point {
        Point {
            metric,
            instrument: Instrument::Counter,
            unit: "1",
            value: 1.0,
            attributes: vec![("branch_class".into(), "trunk_dev".into())],
        }
    }

    #[test]
    fn a_bucket_count_is_always_one_longer_than_its_bounds() {
        let point = Point {
            metric: "ci.job.duration",
            instrument: Instrument::Histogram,
            unit: "s",
            value: 42.0,
            attributes: vec![],
        };
        let body = to_otlp(&[point], &[1_788_696_000.0]);
        let dp = &body["resourceMetrics"][0]["scopeMetrics"][0]["metrics"][0]["histogram"]["dataPoints"]
            [0];
        let counts = dp["bucketCounts"].as_array().unwrap().len();
        let bounds = dp["explicitBounds"].as_array().unwrap().len();
        assert_eq!(counts, bounds + 1);
        assert_eq!(dp["count"], "1");
        assert_eq!(dp["sum"], 42.0);
    }

    /// One artifact holds attempts that finished days apart, so a point must
    /// carry the instant it describes rather than the moment of collection.
    #[test]
    fn points_keep_their_own_instants() {
        let body = to_otlp(
            &[counter("ci.run.attempts"), counter("ci.run.attempts")],
            &[1_788_696_000.0, 1_788_000_000.0],
        );
        let dps = body["resourceMetrics"][0]["scopeMetrics"][0]["metrics"][0]["sum"]["dataPoints"]
            .as_array()
            .unwrap();
        assert_eq!(dps.len(), 2);
        assert_ne!(dps[0]["timeUnixNano"], dps[1]["timeUnixNano"]);
    }

    #[test]
    fn counters_are_delta_because_a_sweep_reports_a_window() {
        let body = to_otlp(&[counter("ci.run.attempts")], &[1_788_696_000.0]);
        let sum = &body["resourceMetrics"][0]["scopeMetrics"][0]["metrics"][0]["sum"];
        assert_eq!(
            sum["aggregationTemporality"],
            "AGGREGATION_TEMPORALITY_DELTA"
        );
        assert_eq!(sum["isMonotonic"], true);
    }

    #[test]
    fn one_metric_groups_its_points_rather_than_repeating_itself() {
        let body = to_otlp(
            &[counter("ci.run.attempts"), counter("ci.job.conclusions")],
            &[1.0, 2.0],
        );
        let metrics = body["resourceMetrics"][0]["scopeMetrics"][0]["metrics"]
            .as_array()
            .unwrap();
        assert_eq!(metrics.len(), 2);
    }
}
