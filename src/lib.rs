//! Kartero transports OTLP it did not produce.
//!
//! A collect pass lists GitHub Actions artifacts named `telemetry-otlp-v1*`,
//! validates the zip, drops anything outside a reviewed allowlist, stamps a
//! small CI envelope, and POSTs the request body to an OTLP/HTTP backend.
//!
//! An optional archive pass copies a different artifact prefix (kache benches:
//! `bench-*`) to an S3-compatible bucket. It does not parse OTLP.

pub mod allowlist;
pub mod archive;
pub mod artifact;
pub mod collect;
pub mod config;
pub mod github;
pub mod http;
pub mod ledger;
pub mod metrics;
pub mod otlp;
pub mod s3;
pub mod self_telemetry;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
