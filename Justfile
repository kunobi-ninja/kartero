# kartero — local commands. `just` (no args) lists them.

default:
  @just --list --unsorted

[group('dev')]
fmt:
  cargo fmt --all

[group('dev')]
fmt-check:
  cargo fmt --all -- --check

[group('dev')]
lint:
  cargo clippy --all-targets --locked -- -D warnings

[group('dev')]
test:
  cargo test --locked

[group('deploy')]
helm-lint:
  helm lint charts/kartero

[group('docker')]
docker:
  docker buildx bake -f docker-bake.hcl --load

[group('docker')]
docker-print:
  docker buildx bake -f docker-bake.hcl --print

[group('docker')]
docker-push:
  docker buildx bake -f docker-bake.hcl push
