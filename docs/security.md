# Security

The CI exporter reads local report files and writes local artifact files. It
does not accept backend credentials and does not send network requests.

The collector needs a fine-grained GitHub token with access to the configured
repository and `Actions: read`. It does not need repository write, organization
owner, or administration permissions.

Kartero only reads runs from configured workflow files and the configured
trusted branch. Pull-request artifacts are not imported. The reviewed allowlist
limits metric and attribute names, and Kartero stamps repository identity from
the GitHub API.

## Attribute values

Keys are bounded by the allowlist everywhere. Values are bounded only where
`attribute_values` declares a set, and a point carrying a value outside its
declared set is dropped rather than merged into a neighbouring series.

That distinction is deliberate. A vocabulary fixed in a producer's code is
worth pinning here, so changing it means changing a reviewed file. A value
that is legitimately open — a job name from a reviewed table, a repository from
this deployment's own source list — is taken on trust from a producer that is
already trusted enough to name a metric.

The values that would be expensive to get wrong are handled a level up: run
ids, commit hashes, raw branch names, actor logins and runner names are absent
from `attributes` entirely, so they cannot be sent at all. Bounding a key's
values is a check on a producer's correctness; leaving a key out is a check on
its reach.

Provide the token through a Kubernetes Secret owned by your secret controller.
Do not place token values in Helm values, Git, GitHub Actions artifacts, or logs.

Report vulnerabilities privately through GitHub's security advisory interface
for `kunobi-ninja/kartero`.
