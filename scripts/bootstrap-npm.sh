#!/usr/bin/env bash
set -euo pipefail

package='@kunobi/kartero'
repository='kunobi-ninja/kartero'
workflow='release.yml'
repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
placeholder="$repo_root/packages/npm-placeholder"
npm_cache="$(mktemp -d)"
trap 'rm -rf -- "$npm_cache"' EXIT
export npm_config_cache="$npm_cache"

if [[ "${1:-}" == '--help' ]]; then
  echo "Usage: $0 [--yes]"
  exit 0
fi
if [[ $# -gt 1 || ($# -eq 1 && "${1:-}" != '--yes') ]]; then
  echo "Usage: $0 [--yes]" >&2
  exit 2
fi

node -e 'const [major, minor] = process.versions.node.split(".").map(Number); if (major < 22 || (major === 22 && minor < 14)) process.exit(1)' || {
  echo 'Node.js 22.14 or newer is required.' >&2
  exit 1
}
npm_version="$(npm --version)"
node -e 'const [major, minor, patch] = process.argv[1].split(".").map(Number); if (major < 11 || (major === 11 && (minor < 5 || (minor === 5 && patch < 1)))) process.exit(1)' "$npm_version" || {
  echo 'npm 11.5.1 or newer is required.' >&2
  exit 1
}

if ! npm whoami >/dev/null 2>&1; then
  echo 'Opening npm web authentication...'
  npm login --auth-type=web
  npm whoami >/dev/null
fi

if npm access get status "$package" >/dev/null 2>&1; then
  echo "$package already exists; skipping placeholder publish."
else
  if [[ "${1:-}" != '--yes' ]]; then
    read -r -p "Publish $package@0.0.0-bootstrap.0 under the bootstrap tag? [y/N] " answer
    [[ "$answer" == 'y' || "$answer" == 'Y' ]] || exit 1
  fi

  (cd "$placeholder" && npm publish --access public --tag bootstrap)
fi

npm trust github "$package" \
  --repository "$repository" \
  --file "$workflow" \
  --allow-publish \
  --yes

echo "Trusted Publisher configured for $repository/.github/workflows/$workflow."
