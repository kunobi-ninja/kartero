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
and schema version. A recorded delivery is not sent again. Temporary GitHub or
OTLP transport failures remain eligible for retry. Metrics or points withheld
by the allowlist are retained separately and retried when its rules change;
already accepted points are not included in that replay.
The retained payloads live in the SQLite ledger for up to 30 days. The
collector removes older pending payloads and counts them in
`kartero_pending_expired_total`; `kartero_pending_metrics_bytes` shows the
space they occupy.

An [OTLP partial-success response](https://opentelemetry.io/docs/specs/otlp/)
may reject points while returning HTTP 200. Kartero counts those rejections
and records the request as terminal. Retrying the full request would resend
the points the backend did accept.

There is a crash window between an OTLP backend accepting a request and SQLite
recording it. A restart in that window can resend the request. Two different
artifacts can also describe overlapping windows, and Kartero cannot detect
that from an opaque payload.

Cumulative points are safer under these gaps: a second observation of the same
counter carries the same number. Delta points can be counted twice after a
crash or an overlapping producer window. A producer emitting delta owns its
record of emitted windows. For strict deduplication across the POST/ledger
crash window, the backend must also support idempotent ingestion.

Use `kartero validate --input telemetry` to check an unpacked artifact before
uploading it.
