# Export coverage from CI

`@kunobi/kartero` reads a report that already exists. It writes
`metrics.otlp.json` and `schema_version` without contacting GitHub, Kartero, or
the OTLP backend.

Pin the package version in CI. Do not use `@latest`.

## Istanbul

Generate `coverage-summary.json` with Jest, Vitest, or nyc, then add:

```yaml
- name: Build coverage artifact
  if: always()
  run: npx --yes @kunobi/kartero@0.2.0 coverage \
    --input coverage/coverage-summary.json \
    --format istanbul \
    --output telemetry-coverage

- name: Upload coverage telemetry
  if: always()
  uses: actions/upload-artifact@330a01c490aca151604b8cf639adc76d48f6c5d4 # v5
  with:
    name: telemetry-otlp-v1-coverage-typescript
    path: telemetry-coverage/
    if-no-files-found: error
    retention-days: 14
```

## Rust with cargo-llvm-cov

Produce LLVM's JSON export, then convert it:

```yaml
- name: Run Rust coverage
  run: cargo llvm-cov --json --summary-only --output-path coverage.json

- name: Build coverage artifact
  if: always()
  run: npx --yes @kunobi/kartero@0.2.0 coverage \
    --input coverage.json \
    --format llvm-cov-json \
    --output telemetry-coverage

- name: Upload coverage telemetry
  if: always()
  uses: actions/upload-artifact@330a01c490aca151604b8cf639adc76d48f6c5d4 # v5
  with:
    name: telemetry-otlp-v1-coverage-rust
    path: telemetry-coverage/
    if-no-files-found: error
    retention-days: 14
```

## LCOV

LCOV works for any producer that writes standard `SF`, `DA`, `FNDA`, and `BRDA`
records:

```bash
npx --yes @kunobi/kartero@0.2.0 coverage \
  --input coverage/lcov.info \
  --format lcov \
  --language typescript \
  --output telemetry-coverage
```

The exporter emits these gauges:

- `ci.coverage.percent`
- `ci.coverage.covered`
- `ci.coverage.total`

Each point has `language`, `kind`, and `branch_class`. Kartero adds the trusted
workflow and repository identity during import.

`if: always()` lets the conversion run after a failing test command. It cannot
run after a hard job timeout or runner loss. Keep report generation and upload
inside the same job, and leave enough timeout for the final steps.
