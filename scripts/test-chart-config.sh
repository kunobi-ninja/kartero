#!/usr/bin/env bash
# Render the chart with several sources and load the result with the same
# parser the collector uses.
#
# `helm lint` only checks that the templates produce valid YAML. It cannot
# tell that `max_bytes` rendered in scientific notation, or that a field was
# named `trustedBranch` where the collector expects `trusted_branch`. Both
# fail at container start, which is the worst place to find out.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

cat >"$work/values.yaml" <<'YAML'
sources:
  - owner: kunobi-ninja
    repo: kache
    workflows: [bench.yml, ci.yml]
    trustedBranch: main
    existingSecret: kartero-github-kache
    existingSecretKey: token
  - owner: kunobi-ninja
    repo: kunobi-frontend
    workflows: [ci.yaml]
    trustedBranch: dev
    existingSecret: kartero-github-kunobi-frontend
    existingSecretKey: token
    actions:
      gateJob: CI Gate
      guardJob: E2E-only filter detected
      guardStep: Reject filtered CI as a complete validation
      filterJob: changes
      docsJob: Docs checks
      branchClasses:
        dev: trunk_dev
        main: trunk_main
      jobAliases:
        "e2e / test": e2e
      canonicalJobs: [changes, CI Gate, e2e]
      excludedWorkflows: [.github/workflows/e2e-flake-nightly.yaml]
  - owner: kunobi-ninja
    repo: kobe
    workflows: [ci.yaml]
    trustedBranch: main
    existingSecret: kartero-github-kobe
    existingSecretKey: token
    actions:
      gateJob: CI Gate
      guardJob: E2E-only filter detected
      guardStep: Reject filtered CI as a complete validation
      filterJob: changes
      docsJob: Docs checks
      jobNamesPath: scripts/ci/ci-metrics-job-aliases.json
archive:
  enabled: true
allowlist:
  content: |
    metric_patterns:
      - 'owned-by-the-deployment\..+'
    metrics: []
    attributes: []
    projects: []
YAML

helm template kartero "$root/charts/kartero" -f "$work/values.yaml" >"$work/rendered.yaml"

# Pull kartero.yaml out of the ConfigMap and undo the four-space block indent.
python3 - "$work/rendered.yaml" "$work/kartero.yaml" <<'PY'
import sys

rendered, out = sys.argv[1], sys.argv[2]
lines = open(rendered).read().splitlines()
start = next(i for i, line in enumerate(lines) if line.strip() == 'kartero.yaml: |')
body = []
for line in lines[start + 1:]:
    if line and not line.startswith('    '):
        break
    body.append(line[4:])
open(out, 'w').write('\n'.join(body) + '\n')
PY

# The rendered config points at Secret mounts that only exist in a pod. Swap
# the root for a directory holding stand-in tokens: the shape under test is the
# wiring, not the secret material.
mkdir -p "$work/tokens/0" "$work/tokens/1" "$work/tokens/2"
echo "token-a" >"$work/tokens/0/token"
echo "token-b" >"$work/tokens/1/token"
echo "token-c" >"$work/tokens/2/token"
sed -i.bak "s#/etc/kartero/tokens#$work/tokens#g" "$work/kartero.yaml"

output="$(cd "$root" && KARTERO_CONFIG="$work/kartero.yaml" cargo run --quiet -- config-check)"
echo "$output"

for expected in \
  "source kunobi-ninja/kache branch=main workflows=bench.yml,ci.yml token=present" \
  "source kunobi-ninja/kunobi-frontend branch=dev workflows=ci.yaml token=present derives=yes" \
  "archive=true"; do
  if ! grep -qF "$expected" <<<"$output"; then
    echo "rendered chart config did not resolve as expected: $expected" >&2
    exit 1
  fi
done

# A field the schema permits but no template renders is accepted and silently
# ignored, which is how the actions block itself shipped once doing nothing.
# Assert against the rendered config rather than the values.
if ! grep -q 'job_names_path: "scripts/ci/ci-metrics-job-aliases.json"' "$work/kartero.yaml"; then
  echo "jobNamesPath did not reach the pod's config" >&2
  exit 1
fi

# The allowlist a deployment supplies has to reach the pod, or owning it is
# a setting that does nothing.
if ! grep -q 'owned-by-the-deployment' "$work/rendered.yaml"; then
  echo "the deployment's allowlist did not reach the ConfigMap" >&2
  exit 1
fi

echo "chart config loads"
