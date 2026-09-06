# Artifact protocol

A Kartero artifact is a zip with these files at its root:

```text
metrics.otlp.json
schema_version
```

`schema_version` currently contains `1`. `metrics.otlp.json` is an OTLP/HTTP
JSON metrics request.

The collector rejects malformed zips, unsupported schema versions, oversized
payloads, and invalid OTLP JSON. It drops metric names and attributes absent
from `allowlist.yaml`. It replaces producer-supplied repository and pipeline
identity with values from the trusted GitHub run.

Artifact names must start with the configured prefix. The default is
`telemetry-otlp-v1`. Add a suffix that identifies the producer, for example
`telemetry-otlp-v1-coverage-rust`.

## Instruments

Kartero delivers `gauge`, `sum` and `histogram`. A metric carrying anything
else is dropped, and a metric carrying two of them rejects the whole payload.

Sums and histograms must declare an `aggregationTemporality`, as either the
proto name or its number. A metric that omits it is refused rather than
guessed at: backends assume one of the two, and the wrong assumption rescales
the series without saying so.

Histogram points must carry exactly one more `bucketCounts` entry than
`explicitBounds`, the last being the overflow above the final bound. A
mismatch is refused here because most backends accept it and misdescribe every
observation instead.

## Replay and delta counters

The ledger key includes repository, workflow run, attempt, artifact ID, digest,
and schema version. Delivered and rejected artifacts are terminal. Temporary
GitHub or OTLP transport failures remain eligible for retry.

That guarantees one artifact is delivered at most once. It does not guarantee
that two different artifacts describe disjoint windows, and nothing downstream
can tell that they do not.

Cumulative points are safe under that gap: a second observation of the same
counter carries the same number, so an overlap is harmless. Delta points are
not — the backend adds them, and an overlapping window inflates the count with
no signal that it happened. A producer emitting delta owns its own record of
what it has already emitted. Kartero cannot do it on the producer's behalf,
because it sees an opaque artifact rather than the window that artifact
describes.

Use `kartero validate --input telemetry` to check an unpacked artifact before
uploading it.
