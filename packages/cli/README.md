# `@kunobi/kartero`

Generate a Kartero artifact from an Istanbul summary, LLVM coverage JSON, or
LCOV report:

```bash
npx --yes @kunobi/kartero@0.3.0 coverage \
  --input coverage/coverage-summary.json \
  --output telemetry
```

The command writes `metrics.otlp.json` and `schema_version`. It refuses to
overwrite an existing artifact directory. Upload both files
at the root of an Actions artifact whose name starts with
`telemetry-otlp-v1`. The CLI does not contact Kartero, SigNoz, or any other
network service.

Create a minimal gauge artifact before a risky CI step so a failure still
produces telemetry:

```bash
npx --yes @kunobi/kartero@0.3.0 gauge \
  --name kache.bench.verdict.ok \
  --value 0 \
  --attribute kache.bench.project=bench-firefox \
  --output telemetry
```

See the [coverage guide](https://github.com/kunobi-ninja/kartero/blob/main/docs/coverage.md)
[fallback gauge guide](https://github.com/kunobi-ninja/kartero/blob/main/docs/gauges.md),
and [artifact protocol](https://github.com/kunobi-ninja/kartero/blob/main/docs/artifact-protocol.md).
