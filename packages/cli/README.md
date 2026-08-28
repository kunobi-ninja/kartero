# `@kunobi/kartero`

Generate a Kartero artifact from an Istanbul summary, LLVM coverage JSON, or
LCOV report:

```bash
npx --yes @kunobi/kartero@0.2.0 coverage \
  --input coverage/coverage-summary.json \
  --output telemetry
```

The command writes `metrics.otlp.json` and `schema_version`. It refuses to
overwrite an existing artifact directory. Upload both files
at the root of an Actions artifact whose name starts with
`telemetry-otlp-v1`. The CLI does not contact Kartero, SigNoz, or any other
network service.

See the [coverage guide](https://github.com/kunobi-ninja/kartero/blob/main/docs/coverage.md)
and [artifact protocol](https://github.com/kunobi-ninja/kartero/blob/main/docs/artifact-protocol.md).
