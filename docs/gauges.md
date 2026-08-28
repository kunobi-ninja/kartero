# Record a fallback gauge from CI

Use `gauge` when a job must emit at least one metric even if its main command
fails. Create the failure artifact before the risky work. A successful producer
can overwrite `metrics.otlp.json` with its complete result.

```yaml
- name: Seed failure telemetry
  run: |
    npx --yes @kunobi/kartero@0.3.0 gauge \
      --name kache.bench.verdict.ok \
      --value 0 \
      --unit 1 \
      --attribute kache.bench.project=bench-firefox \
      --attribute kache.bench.cache_tool=kache \
      --output telemetry

- name: Run benchmark
  run: ./run-benchmark --telemetry-dir telemetry

- name: Validate telemetry
  if: always()
  run: npx --yes @kunobi/kartero@0.3.0 validate --input telemetry

- name: Upload telemetry
  if: always()
  uses: actions/upload-artifact@330a01c490aca151604b8cf639adc76d48f6c5d4 # v5
  with:
    name: telemetry-otlp-v1-benchmark
    path: telemetry/
    if-no-files-found: error
```

`--attribute` can be repeated. Kartero refuses producer-supplied `cicd.*` and
`vcs.*` attributes because the collector adds those values from the trusted
GitHub run. Metric and attribute names still need to be present in the
collector allowlist.

A final upload cannot run after runner loss or a hard job timeout. Leave enough
time for the validation and upload steps.
