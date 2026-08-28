#!/usr/bin/env bash
set -euo pipefail

dev_plan="$(
  IMAGE_TAG=dev VERSION=0.0.0 BUILD_COMMIT=0123456789abcdef \
    docker buildx bake -f docker-bake.hcl push --print
)"
jq -e '
  .target["kartero-push"].tags == [
    "zondax/kartero:dev",
    "zondax/kartero:sha-0123456"
  ]
' <<<"$dev_plan" >/dev/null

release_plan="$(
  IMAGE_TAG= VERSION=0.1.0 BUILD_COMMIT=0123456789abcdef \
    docker buildx bake -f docker-bake.hcl push --print
)"
jq -e '
  .target["kartero-push"].tags == [
    "zondax/kartero:sha-0123456",
    "zondax/kartero:latest",
    "zondax/kartero:v0.1.0",
    "zondax/kartero:v0.1",
    "zondax/kartero:v0"
  ]
' <<<"$release_plan" >/dev/null
