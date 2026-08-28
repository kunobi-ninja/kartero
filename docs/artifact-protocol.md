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

The ledger key includes repository, workflow run, attempt, artifact ID, digest,
and schema version. Delivered and rejected artifacts are terminal. Temporary
GitHub or OTLP transport failures remain eligible for retry.

Use `kartero validate --input telemetry` to check an unpacked artifact before
uploading it.
