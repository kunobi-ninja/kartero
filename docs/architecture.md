# Architecture

Kartero separates producing telemetry from delivering it.

1. A trusted CI workflow runs the tool that owns the source data.
2. `@kunobi/kartero` converts the report into OTLP JSON and writes a versioned
   artifact.
3. The collector lists configured GitHub Actions workflows on a trusted branch.
4. It downloads matching artifacts, validates them, and applies `allowlist.yaml`.
5. It adds the repository and workflow identity, then sends the payload over
   OTLP/HTTP.
6. The SQLite ledger records the terminal result for that artifact.

The artifact boundary keeps OTLP credentials out of GitHub Actions. Producers
cannot choose arbitrary metric names or repository attributes because the
collector filters and stamps them before delivery.

The current collector watches one GitHub repository and multiple workflow files.
Run another Kartero instance for another repository until multi-source support is
added.

Kartero runs as a Deployment. One replica owns one SQLite ledger. The Helm chart
uses a `Recreate` strategy with PVC storage to preserve that single-writer model.

An optional **archive** pass is a sibling of collect, not part of it. Helm
`archive.enabled` turns it on in the same Deployment: after each collect tick,
artifacts matching a different name prefix (kache diagnostic zips, `bench-*`)
are copied to an S3-compatible bucket. Archive failures do not skip OTLP
delivery. The PVC still only holds the ledger, not the zips.
