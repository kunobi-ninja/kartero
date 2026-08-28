# kartero

Pulls OTLP JSON that CI already wrote, and delivers it to an OTLP/HTTP
backend. It does not produce telemetry, convert formats, or hold backend
credentials in GitHub Actions.

Nightly kache benches upload a `telemetry-otlp-v1-*` artifact. Kartero,
running in the cluster, lists those artifacts with a read-only GitHub
token, validates the zip, drops anything outside a reviewed allowlist,
adds a small `cicd.*` / `vcs.*` envelope, and POSTs the request body.

```text
bench job                         cluster
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

After each collect pass kartero POSTs its own gauges (`kartero.collect.*`)
to the same OTLP endpoint, and serves Prometheus `/metrics` for scrape.
Logs are JSON on stdout.

The SQLite ledger records each artifact by repository, run, attempt,
artifact ID, digest, and schema version. Delivered, rejected, and held
artifacts are terminal. Only temporary transport failures are retried.

The Helm chart lives in `charts/kartero`. It is a Deployment (not a
CronJob) so the ledger PVC stays single-writer and SigNoz can scrape
`/metrics`. The chart always mounts ledger storage: `pvc` is the default;
use `ephemeral` only for disposable local tests. Pin the image tag in the
cluster; do not track `latest`.
