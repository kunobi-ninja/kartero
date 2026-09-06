use crate::allowlist::Allowlist;
use anyhow::{Result, bail};
use serde_json::{Value, json};

const MAX_RESOURCE_METRICS: usize = 4;
const MAX_SCOPES_PER_RESOURCE: usize = 8;
const MAX_METRICS_PER_SCOPE: usize = 128;
// A producer that reports one observation per run carries a point per run, so
// a weekly sweep is hundreds rather than the handful a single build emits. The
// zip size cap is the real bound on input; this one only stops a single metric
// from being pathological.
const MAX_POINTS_PER_METRIC: usize = 1024;
const MAX_ATTRIBUTES: usize = 32;
const MAX_BUCKETS_PER_POINT: usize = 64;

/// OTLP carries a metric's points under exactly one of these keys, and the
/// shape of a data point depends on which. `exponentialHistogram` and
/// `summary` are deliberately absent: nothing produces them, and admitting a
/// shape with no test coverage is how the gauge-only assumption survived.
const INSTRUMENTS: [&str; 3] = ["gauge", "sum", "histogram"];

#[derive(Debug, Clone)]
pub struct Envelope {
    pub pipeline_name: String,
    pub repository_url: String,
}

#[derive(Debug, Default)]
pub struct FilterStats {
    pub metrics_kept: u64,
    pub metrics_dropped: u64,
    pub points_dropped: u64,
}

pub fn prepare(
    metrics_json: &[u8],
    allowlist: &Allowlist,
    envelope: &Envelope,
) -> Result<(Vec<u8>, FilterStats)> {
    let mut body: Value = serde_json::from_slice(metrics_json)?;
    let Some(resource_metrics) = body
        .get_mut("resourceMetrics")
        .and_then(Value::as_array_mut)
    else {
        bail!("OTLP body is missing resourceMetrics");
    };
    if resource_metrics.is_empty() {
        bail!("OTLP body has no resourceMetrics");
    }
    if resource_metrics.len() > MAX_RESOURCE_METRICS {
        bail!("OTLP body has too many resourceMetrics entries");
    }

    let mut stats = FilterStats::default();
    for rm in resource_metrics.iter_mut() {
        rewrite_resource(rm, envelope, allowlist)?;
        filter_scope_metrics(rm, allowlist, &mut stats)?;
    }

    if stats.metrics_kept == 0 {
        bail!("allowlist dropped every metric");
    }
    Ok((serde_json::to_vec(&body)?, stats))
}

fn rewrite_resource(rm: &mut Value, envelope: &Envelope, allowlist: &Allowlist) -> Result<()> {
    let attrs = rm
        .pointer_mut("/resource/attributes")
        .and_then(Value::as_array_mut);
    let Some(attrs) = attrs else {
        rm["resource"]["attributes"] = json!([
            str_attr("cicd.pipeline.name", &envelope.pipeline_name),
            str_attr("vcs.repository.url.full", &envelope.repository_url),
        ]);
        return Ok(());
    };
    if attrs.len() > MAX_ATTRIBUTES {
        bail!("resource has too many attributes");
    }
    attrs.retain(|attr| {
        attr.get("key").and_then(Value::as_str).is_some_and(|key| {
            !key.starts_with("cicd.") && !key.starts_with("vcs.") && allowlist.allows_attribute(key)
        })
    });
    attrs.push(str_attr("cicd.pipeline.name", &envelope.pipeline_name));
    attrs.push(str_attr(
        "vcs.repository.url.full",
        &envelope.repository_url,
    ));
    Ok(())
}

fn filter_scope_metrics(
    rm: &mut Value,
    allowlist: &Allowlist,
    stats: &mut FilterStats,
) -> Result<()> {
    let Some(scopes) = rm.get_mut("scopeMetrics").and_then(Value::as_array_mut) else {
        return Ok(());
    };
    if scopes.len() > MAX_SCOPES_PER_RESOURCE {
        bail!("resource has too many scopeMetrics entries");
    }
    for scope in scopes.iter_mut() {
        let Some(metrics) = scope.get_mut("metrics").and_then(Value::as_array_mut) else {
            continue;
        };
        if metrics.len() > MAX_METRICS_PER_SCOPE {
            bail!("scope has too many metrics");
        }
        let mut kept = Vec::new();
        for metric in metrics.drain(..) {
            let name = metric
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            if !allowlist.allows_metric(&name) {
                stats.metrics_dropped += 1;
                continue;
            }
            let mut metric = metric;
            if !filter_points(&mut metric, &name, allowlist, stats)? {
                stats.metrics_dropped += 1;
                continue;
            }
            stats.metrics_kept += 1;
            kept.push(metric);
        }
        *metrics = kept;
    }
    Ok(())
}

fn filter_points(
    metric: &mut Value,
    metric_name: &str,
    allowlist: &Allowlist,
    stats: &mut FilterStats,
) -> Result<bool> {
    let Some(instrument) = instrument_of(metric, metric_name)? else {
        return Ok(false);
    };
    if instrument != "gauge" {
        check_temporality(metric, instrument, metric_name)?;
    }
    let Some(points) = metric
        .pointer_mut(&format!("/{instrument}/dataPoints"))
        .and_then(Value::as_array_mut)
    else {
        return Ok(false);
    };
    if points.len() > MAX_POINTS_PER_METRIC {
        bail!("metric has too many data points");
    }
    let mut kept = Vec::new();
    for mut point in points.drain(..) {
        if instrument == "histogram" {
            check_buckets(&point, metric_name)?;
        }
        if !retain_point(&mut point, metric_name, allowlist)? {
            stats.points_dropped += 1;
            continue;
        }
        kept.push(point);
    }
    let empty = kept.is_empty();
    *points = kept;
    Ok(!empty)
}

/// Which of the OTLP instrument fields this metric carries, if any.
///
/// `None` means an instrument Kartero does not deliver, and the caller drops
/// the metric. Two instruments on one metric is malformed OTLP rather than a
/// filtering decision, so it rejects the whole payload: a producer that sends
/// it is not describing anything a backend can read.
fn instrument_of(metric: &Value, metric_name: &str) -> Result<Option<&'static str>> {
    let mut found = None;
    for instrument in INSTRUMENTS {
        if metric.get(instrument).is_none() {
            continue;
        }
        if found.is_some() {
            bail!("metric {metric_name} carries more than one instrument");
        }
        found = Some(instrument);
    }
    Ok(found)
}

/// Sums and histograms mean nothing without a temporality.
///
/// Both are accepted. A cumulative point is safe to replay because a second
/// observation of the same counter carries the same number; a delta point is
/// not, and deduplication for those stays with the producer. The ledger
/// guarantees an artifact is delivered at most once, which is a different
/// promise. An absent or unspecified temporality is the case worth refusing:
/// backends assume one of the two, and the wrong guess silently rescales the
/// series.
///
/// The enum arrives either as its proto name or as its number, depending on
/// how the producer serialised it, and neither encoding is more correct.
fn check_temporality(metric: &Value, instrument: &str, metric_name: &str) -> Result<()> {
    let declared = match metric.pointer(&format!("/{instrument}/aggregationTemporality")) {
        Some(Value::String(name)) => matches!(
            name.as_str(),
            "AGGREGATION_TEMPORALITY_DELTA" | "AGGREGATION_TEMPORALITY_CUMULATIVE"
        ),
        Some(Value::Number(code)) => matches!(code.as_u64(), Some(1 | 2)),
        _ => false,
    };
    if !declared {
        bail!("{metric_name} must declare a delta or cumulative aggregationTemporality");
    }
    Ok(())
}

/// OTLP requires exactly one more bucket count than bound, the last being the
/// overflow above the final bound.
///
/// A payload that gets this wrong is accepted by most backends and
/// misdescribes every observation in it rather than being refused, so the
/// check has to happen here.
fn check_buckets(point: &Value, metric_name: &str) -> Result<()> {
    let counts = point.get("bucketCounts").and_then(Value::as_array);
    let bounds = point.get("explicitBounds").and_then(Value::as_array);
    let (Some(counts), Some(bounds)) = (counts, bounds) else {
        bail!("histogram point for {metric_name} is missing bucketCounts or explicitBounds");
    };
    if counts.len() > MAX_BUCKETS_PER_POINT {
        bail!("histogram point for {metric_name} has too many buckets");
    }
    if counts.len() != bounds.len() + 1 {
        bail!(
            "histogram point for {metric_name} has {} bucket counts for {} bounds",
            counts.len(),
            bounds.len()
        );
    }
    if let Some(sum) = point.get("sum")
        && !sum.as_f64().is_some_and(f64::is_finite)
    {
        bail!("histogram point for {metric_name} has a non-finite sum");
    }
    Ok(())
}

fn retain_point(point: &mut Value, metric_name: &str, allowlist: &Allowlist) -> Result<bool> {
    let Some(fields) = point.as_object_mut() else {
        return Ok(false);
    };
    // A point may legitimately carry no attributes at all. Treat that as the
    // empty set and apply the same rules, rather than dropping it unread.
    let attrs = fields.entry("attributes").or_insert_with(|| json!([]));
    let Some(attrs) = attrs.as_array_mut() else {
        return Ok(false);
    };
    if attrs.len() > MAX_ATTRIBUTES {
        bail!("data point has too many attributes");
    }
    let mut project = None;
    let mut project_count = 0usize;
    // An attribute with a declared value set that carries something outside it
    // drops the point rather than the attribute. Dropping the attribute would
    // silently merge the point into a different series, which is harder to
    // notice than a missing one.
    let mut unbounded_value = false;
    attrs.retain(|attr| {
        let Some(key) = attr.get("key").and_then(Value::as_str) else {
            return false;
        };
        if key.starts_with("cicd.") || key.starts_with("vcs.") {
            return false;
        }
        if !allowlist.allows_attribute(key) {
            return false;
        }
        let value = attr
            .pointer("/value/stringValue")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if !allowlist.allows_attribute_value(key, value) {
            unbounded_value = true;
            return false;
        }
        if key == "kache.bench.project" {
            project_count += 1;
            project = Some(value.to_string());
        }
        true
    });
    if unbounded_value {
        return Ok(false);
    }
    if metric_name.starts_with("kache.bench.") {
        Ok(project_count == 1 && project.is_some_and(|name| allowlist.allows_project(&name)))
    } else {
        Ok(project_count == 0)
    }
}

fn str_attr(key: &str, value: &str) -> Value {
    json!({"key": key, "value": {"stringValue": value}})
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::allowlist::Allowlist;

    fn list() -> Allowlist {
        Allowlist::parse(
            r#"
metrics:
  - kache.bench.speedup
  - kache.bench.surprise
  - kache.ci.coverage.lines
  - kache.cache.uploads
  - ci.job.duration
attributes:
  - kache.bench.project
  - kache.bench.cache_tool
  - kache.cache.result
  - cicd.pipeline.name
  - vcs.repository.url.full
  - kache.telemetry.schema_version
projects:
  - bench-firefox
"#,
        )
        .unwrap()
    }

    fn envelope() -> Envelope {
        Envelope {
            pipeline_name: "Bench".into(),
            repository_url: "https://github.com/kunobi-ninja/kache".into(),
        }
    }

    #[test]
    fn drops_unknown_metric_and_unknown_project() {
        let body = json!({
            "resourceMetrics": [{
                "resource": {"attributes": [
                    {"key": "kache.telemetry.schema_version", "value": {"stringValue": "1"}},
                    {"key": "cicd.pipeline.run.id", "value": {"stringValue": "nope"}}
                ]},
                "scopeMetrics": [{
                    "metrics": [
                        {
                            "name": "kache.bench.speedup",
                            "gauge": {"dataPoints": [
                                {"asDouble": 2.0, "attributes": [
                                    {"key": "kache.bench.project", "value": {"stringValue": "bench-firefox"}}
                                ]},
                                {"asDouble": 9.0, "attributes": [
                                    {"key": "kache.bench.project", "value": {"stringValue": "typo"}}
                                ]}
                            ]}
                        },
                        {
                            "name": "kache.bench.not_in_v0",
                            "gauge": {"dataPoints": [{"asDouble": 1.0, "attributes": []}]}
                        }
                    ]
                }]
            }]
        });
        let (out, stats) =
            prepare(&serde_json::to_vec(&body).unwrap(), &list(), &envelope()).unwrap();
        let out: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(stats.metrics_dropped, 1);
        assert_eq!(stats.points_dropped, 1);
        let names: Vec<_> = out["resourceMetrics"][0]["scopeMetrics"][0]["metrics"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, vec!["kache.bench.speedup"]);
        let points =
            out["resourceMetrics"][0]["scopeMetrics"][0]["metrics"][0]["gauge"]["dataPoints"]
                .as_array()
                .unwrap();
        assert_eq!(points.len(), 1);
        let resource_keys: Vec<_> = out["resourceMetrics"][0]["resource"]["attributes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| a["key"].as_str().unwrap())
            .collect();
        assert!(!resource_keys.contains(&"cicd.pipeline.run.id"));
        assert!(resource_keys.contains(&"cicd.pipeline.name"));
        assert!(resource_keys.contains(&"vcs.repository.url.full"));
    }

    #[test]
    fn drops_bench_points_without_a_project() {
        let body = json!({
            "resourceMetrics": [{
                "scopeMetrics": [{
                    "metrics": [
                        {
                            "name": "kache.bench.surprise",
                            "gauge": {"dataPoints": [{"asDouble": 1.0, "attributes": [
                                {"key": "kache.bench.cache_tool", "value": {"stringValue": "kache"}}
                            ]}]}
                        },
                        {
                            "name": "kache.bench.speedup",
                            "gauge": {"dataPoints": [{"asDouble": 3.0, "attributes": [
                                {"key": "kache.bench.project", "value": {"stringValue": "bench-firefox"}}
                            ]}]}
                        }
                    ]
                }]
            }]
        });
        let (out, stats) =
            prepare(&serde_json::to_vec(&body).unwrap(), &list(), &envelope()).unwrap();
        let out: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(stats.metrics_kept, 1);
        assert_eq!(stats.metrics_dropped, 1);
        assert_eq!(stats.points_dropped, 1);
        assert_eq!(
            out["resourceMetrics"][0]["scopeMetrics"][0]["metrics"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
    }

    /// The shape `kache/src/otel.rs` writes for its daemon counters. These
    /// were allowlisted and then silently discarded at import.
    #[test]
    fn delivers_cumulative_sums() {
        let body = json!({
            "resourceMetrics": [{
                "scopeMetrics": [{
                    "metrics": [{
                        "name": "kache.cache.uploads",
                        "unit": "{upload}",
                        "sum": {
                            "aggregationTemporality": "AGGREGATION_TEMPORALITY_CUMULATIVE",
                            "isMonotonic": true,
                            "dataPoints": [
                                {
                                    "asInt": "10",
                                    "timeUnixNano": "1700000000000000000",
                                    "startTimeUnixNano": "1699999999000000000",
                                    "attributes": [
                                        {"key": "kache.cache.result", "value": {"stringValue": "completed"}},
                                        {"key": "kache.cache.secret", "value": {"stringValue": "drop me"}}
                                    ]
                                }
                            ]
                        }
                    }]
                }]
            }]
        });
        let (out, stats) =
            prepare(&serde_json::to_vec(&body).unwrap(), &list(), &envelope()).unwrap();
        let out: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(stats.metrics_kept, 1);
        assert_eq!(stats.metrics_dropped, 0);
        let point =
            &out["resourceMetrics"][0]["scopeMetrics"][0]["metrics"][0]["sum"]["dataPoints"][0];
        assert_eq!(point["asInt"], "10");
        let keys: Vec<_> = point["attributes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| a["key"].as_str().unwrap())
            .collect();
        assert_eq!(keys, vec!["kache.cache.result"]);
    }

    #[test]
    fn delivers_delta_sums() {
        let body = json!({
            "resourceMetrics": [{
                "scopeMetrics": [{
                    "metrics": [{
                        "name": "kache.cache.uploads",
                        "sum": {
                            "aggregationTemporality": 1,
                            "isMonotonic": true,
                            "dataPoints": [{"asInt": "3", "attributes": []}]
                        }
                    }]
                }]
            }]
        });
        let (_, stats) =
            prepare(&serde_json::to_vec(&body).unwrap(), &list(), &envelope()).unwrap();
        assert_eq!(stats.metrics_kept, 1);
    }

    #[test]
    fn refuses_a_sum_with_no_temporality() {
        let body = json!({
            "resourceMetrics": [{
                "scopeMetrics": [{
                    "metrics": [{
                        "name": "kache.cache.uploads",
                        "sum": {"dataPoints": [{"asInt": "3", "attributes": []}]}
                    }]
                }]
            }]
        });
        let error = prepare(&serde_json::to_vec(&body).unwrap(), &list(), &envelope())
            .unwrap_err()
            .to_string();
        assert!(error.contains("aggregationTemporality"), "{error}");
    }

    #[test]
    fn delivers_histograms() {
        let (_, stats) = prepare(
            &serde_json::to_vec(&histogram_body(
                vec![json!("0"), json!("1")],
                vec![json!(30.0)],
            ))
            .unwrap(),
            &list(),
            &envelope(),
        )
        .unwrap();
        assert_eq!(stats.metrics_kept, 1);
    }

    #[test]
    fn refuses_a_histogram_whose_buckets_do_not_match_its_bounds() {
        let body = histogram_body(vec![json!("0"), json!("1")], vec![json!(30.0), json!(60.0)]);
        let error = prepare(&serde_json::to_vec(&body).unwrap(), &list(), &envelope())
            .unwrap_err()
            .to_string();
        assert!(error.contains("bucket counts"), "{error}");
    }

    #[test]
    fn drops_instruments_kartero_does_not_deliver() {
        let body = json!({
            "resourceMetrics": [{
                "scopeMetrics": [{
                    "metrics": [{
                        "name": "kache.cache.uploads",
                        "summary": {"dataPoints": [{"count": "1", "sum": 2.0}]}
                    }]
                }]
            }]
        });
        let error = prepare(&serde_json::to_vec(&body).unwrap(), &list(), &envelope())
            .unwrap_err()
            .to_string();
        assert!(error.contains("dropped every metric"), "{error}");
    }

    #[test]
    fn refuses_a_metric_carrying_two_instruments() {
        let body = json!({
            "resourceMetrics": [{
                "scopeMetrics": [{
                    "metrics": [{
                        "name": "kache.cache.uploads",
                        "gauge": {"dataPoints": [{"asInt": "1", "attributes": []}]},
                        "sum": {
                            "aggregationTemporality": 2,
                            "dataPoints": [{"asInt": "1", "attributes": []}]
                        }
                    }]
                }]
            }]
        });
        let error = prepare(&serde_json::to_vec(&body).unwrap(), &list(), &envelope())
            .unwrap_err()
            .to_string();
        assert!(error.contains("more than one instrument"), "{error}");
    }

    #[test]
    fn keeps_a_point_that_carries_no_attributes_key() {
        let body = json!({
            "resourceMetrics": [{
                "scopeMetrics": [{
                    "metrics": [{
                        "name": "kache.ci.coverage.lines",
                        "gauge": {"dataPoints": [{"asDouble": 89.5}]}
                    }]
                }]
            }]
        });
        let (out, stats) =
            prepare(&serde_json::to_vec(&body).unwrap(), &list(), &envelope()).unwrap();
        let out: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(stats.metrics_kept, 1);
        assert_eq!(stats.points_dropped, 0);
        let point =
            &out["resourceMetrics"][0]["scopeMetrics"][0]["metrics"][0]["gauge"]["dataPoints"][0];
        assert_eq!(point["attributes"], json!([]));
    }

    #[test]
    fn a_bounded_attribute_drops_the_point_when_its_value_is_not_declared() {
        let list = Allowlist::parse(
            r#"
metrics: [ci.run.attempts]
attributes: [branch_class]
projects: []
attribute_values:
  branch_class: [trunk_main]
"#,
        )
        .unwrap();
        let point = |branch: &str| {
            json!({
                "asInt": "1",
                "attributes": [{"key": "branch_class", "value": {"stringValue": branch}}]
            })
        };
        let body = json!({
            "resourceMetrics": [{
                "scopeMetrics": [{
                    "metrics": [{
                        "name": "ci.run.attempts",
                        "sum": {
                            "aggregationTemporality": 1,
                            "dataPoints": [point("trunk_main"), point("feat/whatever")]
                        }
                    }]
                }]
            }]
        });
        let (out, stats) =
            prepare(&serde_json::to_vec(&body).unwrap(), &list, &envelope()).unwrap();
        let out: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(stats.points_dropped, 1);
        let kept = out["resourceMetrics"][0]["scopeMetrics"][0]["metrics"][0]["sum"]["dataPoints"]
            .as_array()
            .unwrap();
        assert_eq!(kept.len(), 1);
        // The surviving point keeps its attribute rather than being merged
        // into an unlabelled series.
        assert_eq!(
            kept[0]["attributes"][0]["value"]["stringValue"],
            "trunk_main"
        );
    }

    fn histogram_body(bucket_counts: Vec<Value>, explicit_bounds: Vec<Value>) -> Value {
        json!({
            "resourceMetrics": [{
                "scopeMetrics": [{
                    "metrics": [{
                        "name": "ci.job.duration",
                        "unit": "s",
                        "histogram": {
                            "aggregationTemporality": "AGGREGATION_TEMPORALITY_DELTA",
                            "dataPoints": [{
                                "count": "1",
                                "sum": 12.0,
                                "bucketCounts": bucket_counts,
                                "explicitBounds": explicit_bounds,
                                "timeUnixNano": "1700000000000000000",
                                "attributes": []
                            }]
                        }
                    }]
                }]
            }]
        })
    }

    #[test]
    fn accepts_allowlisted_ci_metric_without_bench_project() {
        let body = json!({
            "resourceMetrics": [{
                "scopeMetrics": [{
                    "metrics": [{
                        "name": "kache.ci.coverage.lines",
                        "gauge": {"dataPoints": [{"asDouble": 89.5, "attributes": []}]}
                    }]
                }]
            }]
        });

        let (out, stats) =
            prepare(&serde_json::to_vec(&body).unwrap(), &list(), &envelope()).unwrap();
        let out: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(stats.metrics_kept, 1);
        assert_eq!(stats.points_dropped, 0);
        assert_eq!(
            out["resourceMetrics"][0]["scopeMetrics"][0]["metrics"][0]["name"],
            "kache.ci.coverage.lines"
        );
    }
}
