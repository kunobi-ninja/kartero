# `@kunobi/kartero`

Generate a Kartero artifact from an Istanbul summary, LLVM coverage JSON, or
LCOV report:

```bash
npx --yes @kunobi/kartero@0.4.1 coverage \
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
npx --yes @kunobi/kartero@0.4.1 gauge \
  --name kache.bench.verdict.ok \
  --value 0 \
  --attribute kache.bench.project=bench-firefox \
  --output telemetry
```

See the [coverage guide](https://github.com/kunobi-ninja/kartero/blob/main/docs/coverage.md)
[fallback gauge guide](https://github.com/kunobi-ninja/kartero/blob/main/docs/gauges.md),
and [artifact protocol](https://github.com/kunobi-ninja/kartero/blob/main/docs/artifact-protocol.md).

## Build an artifact from your own points

For producers that derive many metrics rather than converting one report:

```ts
import { buildMetricsOtlp, writeArtifact, type MetricPoint } from '@kunobi/kartero'

const points: MetricPoint[] = [
  { metric: 'ci.run.attempts', instrument: 'counter', unit: '1', value: 1,
    attributes: { branch_class: 'trunk_dev' }, observedAt: finishedAt },
  { metric: 'ci.job.duration', instrument: 'histogram', unit: 's', value: 42,
    attributes: { job_name: 'checks-ts' }, observedAt: finishedAt, bounds: [30, 60, 300] },
]

await writeArtifact('telemetry', buildMetricsOtlp(points, {
  resource: { 'service.namespace': 'kunobi', 'service.name': 'github-actions-ci' },
  scope: { name: 'my-collector', version: '1' },
}))
```

Counters are monotonic delta sums by default; pass `temporality: 'cumulative'`
when the number is a running total. Every point carries its own `observedAt`,
so one artifact can describe a whole sweep of already-finished runs.
