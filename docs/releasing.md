# Release Kartero

One `vX.Y.Z` tag publishes the matching version of:

- `zondax/kartero:vX.Y.Z` on Docker Hub
- `@kunobi/kartero@X.Y.Z` on npm
- `oci://registry-1.docker.io/zondax/kartero:X.Y.Z` as a Helm chart
- a GitHub release with the packaged chart

Update `Cargo.toml`, `packages/cli/package.json`, and
`charts/kartero/Chart.yaml` together. Run:

```bash
./scripts/check-version-consistency.sh v0.2.0
```

The release workflow accepts tags whose commit is reachable from `main`. It
tests every artifact before publishing, then publishes the image, npm package,
chart, and GitHub release in that order.

npm publishing uses a Trusted Publisher and GitHub OIDC. A brand-new package has
no settings page, so publish `@kunobi/kartero` once from a maintainer session
with 2FA. Then configure `kunobi-ninja/kartero`, workflow `release.yml`, and
`npm publish` permission under the package's Trusted Publisher settings. The
release workflow does not use an `NPM_TOKEN` secret.

Docker Hub publishing uses `DOCKERHUB_USER` and `DOCKERHUB_TOKEN`. The token
needs write access to `zondax/kartero`.
