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

The artifact boundary keeps OTLP credentials out of GitHub Actions. A producer
cannot claim a repository or pipeline identity: the collector strips whatever
arrived and stamps its own from the trusted GitHub run.

Metric and attribute names are constrained rather than fixed. The allowlist
admits them individually or by family pattern, so within a family the
allowlist already trusts, a producer can open a name nobody approved
one at a time. See [the allowlist](allowlist.md) for where that line sits.

## Sources

One collector watches several repositories. Each source names its own owner,
repo, workflow list, trusted branch and token. A source that fails is reported
and stepped over rather than stopping the ones after it: an expired token on
one repository must not quietly halt collection for the rest.

One instance keeps one ledger and one allowlist. The ledger key already
includes the repository, so two sources cannot collide in it. The allowlist
stays global — a metric name admitted for one repository is admitted for all of
them, which is one review rather than several, and is worth splitting only if
two sources ever need genuinely different surfaces.

Each source carries its own token, because a fine-grained token is scoped to
the repositories it was minted for. One token covering several is a decision
about blast radius rather than a default.

Environment variables describe a single source. Several need `KARTERO_CONFIG`,
since per-source tokens and branches have no flat environment form that does
not invent an index convention. The Helm chart renders that file when `sources`
is set.

Kartero runs as a Deployment. One replica owns one SQLite ledger. The Helm chart
uses a `Recreate` strategy with PVC storage to preserve that single-writer model.

An optional **archive** pass is a sibling of collect, not part of it. Helm
`archive.enabled` turns it on in the same Deployment: after each collect tick,
artifacts matching a different name prefix (kache diagnostic zips, `bench-*`)
are written to a directory, usually a second PVC. Archive failures do not skip
OTLP delivery. The ledger PVC stays small; the archive volume holds the zips.
