# kartero

Pulls OTLP JSON that CI already wrote, and delivers it to an OTLP/HTTP
backend. It does not produce telemetry, convert formats, or hold backend
credentials in GitHub Actions.

Nightly kache benches and trusted `main` CI runs upload
`telemetry-otlp-v1-*` artifacts. Kartero lists `bench.yml` and `ci.yml`
with a read-only GitHub token. It accepts scheduled runs and `main` pushes,
rejects pull-request telemetry, validates each zip, applies the reviewed
allowlist, adds a small `cicd.*` / `vcs.*` envelope, and POSTs the request.

```text
bench / main CI job               cluster
  └─ metrics.otlp.json             └─ kartero (Deployment)
       └─ upload-artifact               ├─ Actions: read PAT
                                        ├─ allowlist + ledger
                                        └─ OTLP/HTTP → SigNoz
```

## Run

```text
KARTERO_GITHUB_TOKEN=… \
KARTERO_OTLP_ENDPOINT=http://localhost:4318 \
KARTERO_ALLOWLIST=./allowlist.yaml \
KARTERO_LEDGER=./tmp/ledger.sqlite \
  kartero collect          # one pass
  kartero run              # HTTP /metrics + collect on KARTERO_INTERVAL
```

`KARTERO_GITHUB_TOKEN` is required. The chart always reads it from the
Secret named by `github.existingSecret`; it will not start without that
Secret and key. The platform may create the Secret with External Secrets.

After each collect pass kartero POSTs its own gauges (`kartero.collect.*`)
to the same OTLP endpoint, and serves Prometheus `/metrics` for scrape.
Those gauges include configured sources, runs seen/trusted, artifacts
seen/matched/delivered, filtered series, bounded error components, duration,
and pass status. Logs are JSON on stdout; GitHub API failures include the
request error and cause the pass status plus `github` error count to fail.

The SQLite ledger records each artifact by repository, run, attempt,
artifact ID, digest, and schema version. Delivered, rejected, and held
artifacts are terminal. Only temporary transport failures are retried.

The Helm chart lives in `charts/kartero`. It is a Deployment (not a
CronJob) so the ledger PVC stays single-writer and SigNoz can scrape
`/metrics`. The chart always mounts ledger storage: `pvc` is the default;
use `ephemeral` only for disposable local tests. Pin the image tag in the
cluster; do not track `latest`.
