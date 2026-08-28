#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
package_dir="$repo_root/packages/cli"
smoke_dir="$(mktemp -d)"
trap 'rm -rf -- "$smoke_dir"' EXIT
version="$(node -p "require('$package_dir/package.json').version")"
npm_cache="$smoke_dir/npm-cache"

tarball="$(cd "$package_dir" && npm pack --silent --pack-destination "$smoke_dir" --cache "$npm_cache")"
npm exec --yes --cache "$npm_cache" --package "$smoke_dir/$tarball" -- kartero --version | grep -Fx "$version"
npm exec --yes --cache "$npm_cache" --package "$smoke_dir/$tarball" -- kartero coverage \
  --input "$repo_root/fixtures/coverage/istanbul-summary.json" \
  --output "$smoke_dir/artifact" \
  --timestamp 2026-08-28T12:00:00Z
npm exec --yes --cache "$npm_cache" --package "$smoke_dir/$tarball" -- kartero validate --input "$smoke_dir/artifact"
npm exec --yes --cache "$npm_cache" --package "$smoke_dir/$tarball" -- kartero gauge \
  --name kache.bench.verdict.ok \
  --value 0 \
  --attribute kache.bench.project=bench-firefox \
  --output "$smoke_dir/gauge"
npm exec --yes --cache "$npm_cache" --package "$smoke_dir/$tarball" -- kartero validate --input "$smoke_dir/gauge"
