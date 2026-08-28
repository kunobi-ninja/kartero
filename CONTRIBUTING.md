# Contributing

Open an issue before large protocol or deployment changes. Small fixes can go
directly to a pull request.

Run the relevant checks before submitting:

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
npm ci --prefix packages/cli
npm test --prefix packages/cli
helm lint charts/kartero
```

Changes to emitted metrics or attributes must update both copies of
`allowlist.yaml` and include a producer-to-consumer contract test.

Contributions are licensed under Apache-2.0.
