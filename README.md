# Kartero

Kartero carries engineering telemetry from CI into an OTLP backend such as
SigNoz. CI creates a small, validated artifact. The in-cluster service reads
trusted GitHub Actions runs, filters the payload, and delivers it once.

```text
CI report/metric -> @kunobi/kartero -> GitHub artifact -> Kartero -> OTLP/SigNoz
```

## Export coverage

The public npm package accepts Istanbul summary JSON, LLVM coverage export JSON,
and LCOV. It does not need credentials or network access.

```bash
npx --yes @kunobi/kartero@0.3.0 coverage \
  --input coverage/coverage-summary.json \
  --output telemetry
```

Upload the resulting directory as a GitHub Actions artifact whose name starts
with `telemetry-otlp-v1`:

```yaml
- if: always()
  run: npx --yes @kunobi/kartero@0.3.0 coverage \
    --input coverage/coverage-summary.json --output telemetry
- if: always()
  uses: actions/upload-artifact@330a01c490aca151604b8cf639adc76d48f6c5d4 # v5
  with:
    name: telemetry-otlp-v1-coverage
    path: telemetry/
    if-no-files-found: error
```

See [coverage setup](docs/coverage.md) and the
[artifact protocol](docs/artifact-protocol.md).

## Record a fallback metric

Create an artifact before a long-running CI command, then overwrite it with the
full result on success. If the command fails, the fallback remains available to
the final `if: always()` upload step.

```bash
npx --yes @kunobi/kartero@0.3.0 gauge \
  --name kache.bench.verdict.ok \
  --value 0 \
  --attribute kache.bench.project=bench-firefox \
  --output telemetry
```

See [fallback gauges](docs/gauges.md).

## Run the collector

```bash
KARTERO_GITHUB_TOKEN=... \
KARTERO_OTLP_ENDPOINT=http://localhost:4318 \
KARTERO_ALLOWLIST=./allowlist.yaml \
KARTERO_LEDGER=./tmp/ledger.sqlite \
  kartero collect
```

The token needs read access to the source repository and `Actions: read`.
Kartero stores delivery state in SQLite so a restart does not import the same
artifact again. SigNoz is only the destination; it is not the deduplication
store.

For Kubernetes, install the OCI chart and supply the token through a Kubernetes
Secret, commonly managed by External Secrets:

```bash
helm install kartero oci://registry-1.docker.io/zondax/kartero \
  --version 0.3.0 \
  --namespace signoz \
  --set github.owner=kunobi-ninja \
  --set github.repo=kunobi-frontend \
  --set github.existingSecret=kartero-github
```

Persistent ledger storage is the chart default. Use `ephemeral` only for local
or disposable installations. See [deployment](docs/deployment.md).

## Runtime telemetry

Kartero exposes `/healthz`, `/readyz`, and Prometheus `/metrics`. It also sends
its heartbeat and collection metrics to the configured OTLP endpoint. JSON logs
include GitHub, artifact, validation, ledger, and OTLP failures.

## Documentation

- [Architecture](docs/architecture.md)
- [Coverage exporters](docs/coverage.md)
- [Fallback gauges](docs/gauges.md)
- [Artifact protocol](docs/artifact-protocol.md)
- [Deployment](docs/deployment.md)
- [Security](docs/security.md)
- [Releasing](docs/releasing.md)

Kartero is licensed under Apache-2.0.
