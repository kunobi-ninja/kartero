#!/usr/bin/env bash
set -euo pipefail

expected="${1:-}"
cargo_version="$(awk -F '"' '/^version = "/ { print $2; exit }' Cargo.toml)"
npm_version="$(node -p "require('./packages/cli/package.json').version")"
chart_version="$(awk '/^version:/ { print $2; exit }' charts/kartero/Chart.yaml)"
app_version="$(awk '/^appVersion:/ { gsub(/\"/, "", $2); print $2; exit }' charts/kartero/Chart.yaml)"

versions=("$cargo_version" "$npm_version" "$chart_version" "$app_version")
for version in "${versions[@]}"; do
  if [[ "$version" != "$cargo_version" ]]; then
    echo "version mismatch: Cargo=$cargo_version npm=$npm_version chart=$chart_version app=$app_version" >&2
    exit 1
  fi
done

# Keep copyable installation commands on the version shipped by this tree.
node - "$cargo_version" <<'NODE'
const fs = require('node:fs');
const expected = process.argv[2];
for (const file of ['README.md', 'docs/coverage.md', 'docs/gauges.md', 'docs/deployment.md', 'docs/releasing.md']) {
  const text = fs.readFileSync(file, 'utf8');
  const examples = text.matchAll(/@kunobi\/kartero@(\d+\.\d+\.\d+)|--version (\d+\.\d+\.\d+)|check-version-consistency\.sh v(\d+\.\d+\.\d+)/g);
  for (const match of examples) {
    const version = match[1] ?? match[2] ?? match[3];
    if (version !== expected) {
      console.error(`${file}: installation example uses ${version}, expected ${expected}`);
      process.exitCode = 1;
    }
  }
}
NODE

if [[ -n "$expected" ]]; then
  expected="${expected#v}"
  if [[ "$expected" != "$cargo_version" ]]; then
    echo "tag version $expected does not match repository version $cargo_version" >&2
    exit 1
  fi
fi

echo "$cargo_version"
