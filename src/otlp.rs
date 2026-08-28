use crate::allowlist::Allowlist;
use anyhow::{Result, bail};
use serde_json::{Value, json};

const MAX_RESOURCE_METRICS: usize = 4;
const MAX_SCOPES_PER_RESOURCE: usize = 8;
const MAX_METRICS_PER_SCOPE: usize = 128;
const MAX_POINTS_PER_METRIC: usize = 128;
const MAX_ATTRIBUTES: usize = 32;

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
                .unwrap_or_default();
            if !allowlist.allows_metric(name) {
                stats.metrics_dropped += 1;
                continue;
            }
            let mut metric = metric;
            if !filter_points(&mut metric, allowlist, stats)? {
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
    allowlist: &Allowlist,
    stats: &mut FilterStats,
) -> Result<bool> {
    let Some(points) = metric
        .pointer_mut("/gauge/dataPoints")
        .and_then(Value::as_array_mut)
    else {
        return Ok(false);
    };
    if points.len() > MAX_POINTS_PER_METRIC {
        bail!("metric has too many data points");
    }
    let mut kept = Vec::new();
    for mut point in points.drain(..) {
        if !retain_point(&mut point, allowlist)? {
            stats.points_dropped += 1;
            continue;
        }
        kept.push(point);
    }
    let empty = kept.is_empty();
    *points = kept;
    Ok(!empty)
}

fn retain_point(point: &mut Value, allowlist: &Allowlist) -> Result<bool> {
    let Some(attrs) = point.get_mut("attributes").and_then(Value::as_array_mut) else {
        return Ok(false);
    };
    if attrs.len() > MAX_ATTRIBUTES {
        bail!("data point has too many attributes");
    }
    let mut project = None;
    let mut project_count = 0usize;
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
        if key == "kache.bench.project" {
            project_count += 1;
            project = attr
                .pointer("/value/stringValue")
                .and_then(Value::as_str)
                .map(str::to_string);
        }
        true
    });
    Ok(project_count == 1 && project.is_some_and(|name| allowlist.allows_project(&name)))
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
attributes:
  - kache.bench.project
  - kache.bench.cache_tool
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
    fn drops_non_gauges_and_points_without_a_project() {
        let body = json!({
            "resourceMetrics": [{
                "scopeMetrics": [{
                    "metrics": [
                        {
                            "name": "kache.bench.speedup",
                            "sum": {"dataPoints": [{"asDouble": 2.0, "attributes": []}]}
                        },
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
        assert_eq!(stats.metrics_dropped, 2);
        assert_eq!(stats.points_dropped, 1);
        assert_eq!(
            out["resourceMetrics"][0]["scopeMetrics"][0]["metrics"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
    }
}
