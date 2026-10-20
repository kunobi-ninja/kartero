# Export to object storage

Export runs SigNoz queries on every pass and writes their results as JSON to
an S3-compatible bucket, such as Cloudflare R2. A public page, a README badge
or another dashboard can then read that JSON without SigNoz credentials.

```text
CI -> Kartero collect -> SigNoz -> Kartero export -> bucket -> page
```

The queries live in Kartero's configuration. Editing a SigNoz dashboard does
not change what is published; changing the configuration does, through the
same review as any other deployment change.

## Configure a job

Export needs the configuration file (`KARTERO_CONFIG`). With the Helm chart,
that means `sources` must be set.

```yaml
export:
  enabled: true
  signoz:
    # The query service, not the OTLP collector.
    endpoint: http://signoz.signoz.svc.cluster.local:8080
    api_key_file: /etc/kartero/export/signoz/api-key
  store:
    endpoint: https://<account id>.r2.cloudflarestorage.com
    bucket: kunobi-public
    region: auto
    access_key_id_file: /etc/kartero/export/store/access-key-id
    secret_access_key_file: /etc/kartero/export/store/secret-access-key
  jobs:
    - name: kache-bench
      key: kache/bench/v1/latest.json
      snapshot_key: kache/bench/v1/days/{date}.json
      window: 90d
      step: 1d
      include_partial: true
      cache_control: "public, max-age=300"
      queries:
        - id: build_duration
          spec:
            signal: metrics
            aggregations:
              - metricName: kache.bench.build.duration
                timeAggregation: avg
                spaceAggregation: avg
            filter:
              expression: "kache.bench.cache_tool = 'kache'"
            groupBy:
              - name: kache.bench.project
              - name: kache.bench.phase
            limit: 500
```

Each `spec` is a SigNoz v5 builder query, sent to `/api/v5/query_range` as
written. Kartero sets `name` and `stepInterval`, so a spec may not. Each query
has exactly one aggregation, so every point is one number.

Set `limit` when a query groups by attributes. SigNoz returns 100 series by
default and drops the rest without saying so.

| Field | Meaning |
| --- | --- |
| `key` | Object rewritten on every pass. |
| `snapshot_key` | Optional. Also written each pass, with `{date}` replaced by the UTC date, so the last pass of a day stays. This keeps a history after SigNoz's retention drops it. |
| `window`, `step` | Time range and bucket size, shared by every query in the job. Units: `d`, `h`, `m`, `s`. The step is at least a minute. |
| `include_partial` | Keep the bucket SigNoz has not closed yet. Off by default. Turn it on for a measurement that lands once per step, such as a nightly benchmark: its day is final once the night's run is in, and leaving it out publishes each night a day late. |
| `cache_control` | Stored with the object and returned to readers. Defaults to `public, max-age=300`. |

## The published document

```json
{
  "schema": "kartero.export.v1",
  "export": "kache-bench",
  "generated_at": "2026-10-20T10:00:00Z",
  "start": 1784735252000,
  "end": 1792511252000,
  "step_seconds": 86400,
  "queries": {
    "build_duration": {
      "series": [
        {
          "labels": { "kache.bench.phase": "cold", "kache.bench.project": "bench-llvm" },
          "points": [[1792368000000, 1301.259]]
        }
      ]
    }
  }
}
```

Times are Unix milliseconds. Points are `[time, value]`, oldest first, where
the time is the start of the bucket. Series are ordered by their labels, so
the same data always produces the same bytes. A series with no usable points
is left out, and so is any value that is not a finite number.

A reader checks `schema` first. A change to this shape gets a new value.

## Credentials

The SigNoz key comes from a service account (Settings → Service Accounts).
Give it the smallest role that can run queries.

The store key needs write access to the bucket and nothing else. For R2,
create an API token with Object Read & Write scoped to that bucket.

Kartero reads both from files and never prints them. `config-check` names the
bucket and keys of each job.

## Failures

A failed job does not stop the other jobs. Nothing is uploaded for a job until
all its queries have answered, so a reader never sees half a document. The
next pass tries again.

A query that returns no series fails its job too. That is what a renamed
metric or a broken filter looks like, and publishing it would replace a
working page with an empty one. The last good document stays in place.

Kartero exports these on `/metrics`:

- `kartero_export_passes_total{outcome}`
- `kartero_export_jobs_total{export,outcome}`
- `kartero_export_last_published_unix{export}`
- `kartero_export_duration_seconds`

Alert on the age of `kartero_export_last_published_unix`. It is the metric
that shows when a public page has stopped updating.

Run one pass locally with `kartero export`.
