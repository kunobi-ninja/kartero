# Security

The CI exporter reads local report files and writes local artifact files. It
does not accept backend credentials and does not send network requests.

The collector needs a fine-grained GitHub token with access to the configured
repository and `Actions: read`. It does not need repository write, organization
owner, or administration permissions.

Kartero only reads runs from configured workflow files and the configured
trusted branch. Pull-request artifacts are not imported. Kartero stamps
repository and pipeline identity from the GitHub API, discarding whatever the
producer sent.

## What the allowlist does and does not promise

Names are admitted individually or by family pattern. A pattern such as
`ci\..+` means a producer can open a new `ci.*` series without anyone
approving that specific name. Patterns are anchored to the whole name, so a
family cannot be widened by accident, but the trade is real and is described
in [the allowlist](allowlist.md).

The trade is defensible because a producer is already a trusted workflow on a
trusted branch of a configured repository — anyone able to add a metric name
can edit the workflow that sends it. A new name also costs a bounded number of
series: one per attribute combination that already exists.

## Attribute values

Values are bounded only where `attribute_values` declares a set, and a point
carrying a value outside its set is dropped rather than merged into a
neighbouring series.

This is the check that matters most, because an unbounded value costs a series
*per run* where a new name costs a bounded number. A vocabulary fixed in a
producer's code is worth pinning, so changing it means changing a reviewed
file. A value that is legitimately open — a job name from a reviewed table, a
repository from this deployment's own source list — is taken on trust.

The identifiers that would be expensive to get wrong are handled a level up:
run ids, commit hashes, raw branch names, actor logins and runner names match
no entry or pattern, so they cannot be sent at all. Bounding a key's values
checks a producer's correctness; admitting no pattern that reaches a key
checks its reach.

Provide the token through a Kubernetes Secret owned by your secret controller.
Do not place token values in Helm values, Git, GitHub Actions artifacts, or logs.

Report vulnerabilities privately through GitHub's security advisory interface
for `kunobi-ninja/kartero`.
