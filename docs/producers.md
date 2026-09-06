# Producing artifacts

Two shapes of producer write Kartero artifacts. Both end at the same place — a
zip named with the configured prefix, uploaded from a trusted workflow — but
they differ in what they can observe and in what they have to remember.

## In-job producers

The job that does the work also reports on it. A test job writes its coverage
summary, a benchmark writes its verdict. `@kunobi/kartero coverage` and
`@kunobi/kartero gauge` cover this shape; see [coverage](coverage.md) and
[fallback gauges](gauges.md).

The observation is of the job it runs in, taken as the job ends, so the
timestamp is the moment of upload and there is nothing to remember between
runs. This is the shape to reach for unless something makes it impossible.

## Post-hoc sweeps

Some things cannot be observed from inside the run they describe. Whether an
attempt was a flake is the clearest case: the evidence is a *later* attempt
that succeeded, which does not exist while the first one is still running. A
step inside a run cannot classify the run it belongs to.

A sweep runs on a schedule, reads the Actions API for runs that have already
finished, derives metrics for many of them at once, and writes one artifact.
That brings three requirements the in-job shape does not have.

Use the library rather than writing OTLP by hand:

```ts
import { buildMetricsOtlp, writeArtifact } from '@kunobi/kartero'

await writeArtifact('telemetry', buildMetricsOtlp(points, {
  resource: { 'service.namespace': 'kunobi', 'service.name': 'github-actions-ci' },
  scope: { name: 'my-collector', version: '1' },
}))
```

Each point names its metric, instrument (`gauge`, `counter`, `histogram`),
unit, value, attributes and the instant it describes. Counters become
monotonic delta sums; histograms need explicit bounds and are bucketed for
you. The builder refuses one metric carrying two instruments or two units,
non-ascending bounds, and any `cicd.*` or `vcs.*` attribute — all things a
hand-rolled emitter gets wrong quietly.

**Every point carries its own timestamp.** A run that finished three days ago
has to land three days ago, not at the moment of the sweep. One artifact can
carry points spanning a week, and OTLP allows that because the timestamp lives
on the data point rather than on the request.

**The producer keeps its own ledger.** Kartero's ledger stops one artifact
being delivered twice. It cannot stop two artifacts describing overlapping
windows, and for delta counters that difference matters — see
[replay and delta counters](artifact-protocol.md#replay-and-delta-counters).
A sweep therefore records which units of work it has already reported and
derives nothing for them again. The unit is usually an attempt rather than a
run, because a rerun bumps the attempt on an existing run instead of creating
a new one.

**Windows overlap on purpose.** A run still executing when one sweep passes has
to still be in the listing when the next one looks, so the lookback is wider
than the interval. The producer's ledger is what makes that cheap: everything
already sealed derives nothing.

Name the artifact for the sweep rather than the build — `telemetry-otlp-v1-ci`
rather than `telemetry-otlp-v1-coverage-rust`. Two sweeps in the same run would
otherwise collide.

## Backfill

Deriving history is the awkward case. A local run has no delivery path: the
artifact boundary means a producer can only write a file, and a file only
reaches the backend if a collector is watching the workflow that uploaded it.

Make backfill a dispatchable workflow that uploads like any other producer.
The depth becomes a dispatch input, the artifact is indistinguishable from a
scheduled sweep's, and the collector needs no special case.

The alternative — letting Kartero import a local directory — reopens exactly
the question the artifact boundary exists to close, by requiring backend
credentials somewhere other than the cluster. It is not supported, and adding
it would need a better reason than convenience.

Two limits are worth knowing before planning a deep backfill. The Actions API
is rate limited per repository per hour, so a sweep should stop with budget in
reserve rather than starving everything else that shares it. And artifact
retention is finite: history is recoverable only while GitHub still holds the
reports a sweep reads.
