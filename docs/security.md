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

Provide the token through a Kubernetes Secret owned by your secret controller.
Do not place token values in Helm values, Git, GitHub Actions artifacts, or logs.

Report vulnerabilities privately through GitHub's security advisory interface
for `kunobi-ninja/kartero`.
