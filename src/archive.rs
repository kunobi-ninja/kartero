//! Optional on-disk copy of GitHub diagnostic artifacts.
//!
//! Independent of collect: its own prefix, size cap, ledger table, and
//! failure domain. Disabled unless `Config.archive` is set. Writes zip
//! files under a configured directory (a cluster PVC). Does not parse
//! OTLP, talk to object storage, or write to SigNoz.

use crate::config::{ArchiveConfig, Config};
use crate::github::{self, ArtifactRef, GitHub, WorkflowRun};
use crate::ledger::{ArchiveKey, ArchiveStatus, Ledger};
use crate::metrics::Metrics;
use crate::self_telemetry::{self, ArchiveSnapshot};
use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tracing::{info, warn};

pub async fn archive_once(config: &Config) -> Result<()> {
    let Some(archive) = config.archive.as_ref() else {
        return Ok(());
    };
    let started = Instant::now();
    let mut snapshot = ArchiveSnapshot::default();
    let result = archive_inner(config, archive, &mut snapshot).await;
    snapshot.ok = result.is_ok();
    snapshot.duration_s = started.elapsed().as_secs_f64();
    Metrics::global().observe_archive(&snapshot);
    emit_self_telemetry(config, &snapshot).await;
    result
}

async fn emit_self_telemetry(config: &Config, snapshot: &ArchiveSnapshot) {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let Ok(client) = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(30))
        .build()
    else {
        warn!("could not build HTTP client for kartero archive telemetry");
        return;
    };
    if let Err(err) = self_telemetry::post_archive(&client, &config.otlp_endpoint, snapshot).await {
        warn!(error = %err, "kartero archive self-telemetry failed");
    }
}

async fn archive_inner(
    config: &Config,
    archive: &ArchiveConfig,
    snapshot: &mut ArchiveSnapshot,
) -> Result<()> {
    std::fs::create_dir_all(&archive.dir)
        .with_context(|| format!("creating archive dir {}", archive.dir.display()))?;
    let ledger = Ledger::open(&config.ledger_path)?;
    let github = GitHub::new(config.github.clone())?;
    let runs = match github.list_completed_runs().await {
        Ok(runs) => runs,
        Err(err) => {
            snapshot.github_errors += 1;
            warn!(error = %err, "archive: listing completed GitHub workflow runs failed");
            return Err(err);
        }
    };
    snapshot.runs_seen = runs.len() as u64;
    let mut had_errors = false;
    for run in runs {
        if !github.trusted(&run) {
            continue;
        }
        snapshot.runs_trusted += 1;
        let artifacts = match github.list_artifacts(run.run_id).await {
            Ok(list) => list,
            Err(err) => {
                warn!(run_id = run.run_id, error = %err, "archive: listing artifacts failed");
                snapshot.github_errors += 1;
                had_errors = true;
                continue;
            }
        };
        snapshot.artifacts_seen += artifacts.len() as u64;
        for artifact in artifacts {
            if !github::artifact_name_matches(&artifact.name, &archive.artifact_prefix) {
                continue;
            }
            snapshot.artifacts_matched += 1;
            if let Err(err) =
                archive_one(config, archive, &ledger, &github, snapshot, &run, &artifact).await
            {
                warn!(
                    run_id = run.run_id,
                    artifact = %artifact.name,
                    error = %err,
                    "archive failed"
                );
                record_artifact(snapshot, "retryable");
                snapshot.store_errors += 1;
                had_errors = true;
            }
        }
    }
    if had_errors {
        bail!("one or more archive operations failed");
    }
    Ok(())
}

fn record_artifact(snapshot: &mut ArchiveSnapshot, outcome: &str) {
    Metrics::global().inc_archive_artifact(outcome);
    snapshot.inc_artifact(outcome);
}

#[allow(clippy::too_many_arguments)]
async fn archive_one(
    config: &Config,
    archive: &ArchiveConfig,
    ledger: &Ledger,
    github: &GitHub,
    snapshot: &mut ArchiveSnapshot,
    run: &WorkflowRun,
    artifact: &ArtifactRef,
) -> Result<()> {
    let key = ArchiveKey {
        repo_id: run.repo_id,
        run_id: run.run_id,
        attempt: run.attempt,
        artifact_id: artifact.id,
        digest: artifact.digest.clone(),
    };
    if ledger.archive_is_terminal(&key)? {
        record_artifact(snapshot, "skipped");
        return Ok(());
    }
    if artifact.expired {
        ledger.record_archive(&key, "", ArchiveStatus::Skipped)?;
        record_artifact(snapshot, "skipped");
        return Ok(());
    }
    if artifact.size_in_bytes > archive.max_bytes as u64 {
        warn!(
            artifact = %artifact.name,
            size = artifact.size_in_bytes,
            max = archive.max_bytes,
            "archive: skipping oversized artifact"
        );
        ledger.record_archive(&key, "", ArchiveStatus::Skipped)?;
        record_artifact(snapshot, "skipped");
        return Ok(());
    }

    let relative = relative_path(
        &config.github.owner,
        &config.github.repo,
        run.run_id,
        run.attempt,
        &artifact.name,
    );
    let dest = archive_dest(&archive.dir, &relative)?;
    let zip = github
        .download_zip_limited(artifact.id, archive.max_bytes)
        .await?;
    write_zip(&dest, &zip)?;
    ledger.record_archive(&key, &relative, ArchiveStatus::Archived)?;
    record_artifact(snapshot, "archived");
    info!(
        run_id = run.run_id,
        artifact = %artifact.name,
        path = %dest.display(),
        bytes = zip.len(),
        "archived"
    );
    Ok(())
}

pub fn relative_path(
    owner: &str,
    repo: &str,
    run_id: i64,
    attempt: i64,
    artifact_name: &str,
) -> String {
    format!(
        "{owner}/{repo}/{run_id}/{attempt}/{}.zip",
        sanitize_artifact_name(artifact_name)
    )
}

fn archive_dest(root: &Path, relative: &str) -> Result<PathBuf> {
    let dest = root.join(relative);
    if !dest.starts_with(root) {
        bail!("archive path escaped root: {relative}");
    }
    Ok(dest)
}

fn write_zip(dest: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let tmp = dest.with_extension("zip.partial");
    std::fs::write(&tmp, bytes).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, dest).with_context(|| format!("renaming {}", dest.display()))?;
    Ok(())
}

fn sanitize_artifact_name(name: &str) -> String {
    let sanitized: String = name
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-') {
                ch
            } else {
                '_'
            }
        })
        .collect();
    if sanitized.is_empty() {
        "artifact".into()
    } else {
        sanitized
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::GitHubConfig;

    fn config_disabled() -> Config {
        Config {
            bind: "127.0.0.1:0".into(),
            interval: Duration::from_secs(3600),
            heartbeat_interval: Duration::from_secs(60),
            github: GitHubConfig {
                token: "token".into(),
                owner: "kunobi-ninja".into(),
                repo: "kache".into(),
                workflows: vec!["bench.yml".into()],
                trusted_branch: "main".into(),
            },
            otlp_endpoint: "http://127.0.0.1:4318".into(),
            allowlist_path: "/etc/kartero/allowlist.yaml".into(),
            ledger_path: "/tmp/ledger.sqlite".into(),
            artifact_prefix: "telemetry-otlp-v1".into(),
            archive: None,
        }
    }

    #[tokio::test]
    async fn disabled_archive_is_a_noop() {
        archive_once(&config_disabled()).await.unwrap();
    }

    #[test]
    fn relative_path_sanitizes_the_artifact_name() {
        assert_eq!(
            relative_path("kunobi-ninja", "kache", 33286590263, 1, "bench-firefox"),
            "kunobi-ninja/kache/33286590263/1/bench-firefox.zip"
        );
        assert_eq!(
            relative_path("o", "r", 1, 1, "bench/../x y"),
            "o/r/1/1/bench_.._x_y.zip"
        );
    }

    #[test]
    fn write_zip_is_atomic_and_stays_under_root() {
        let dir = tempfile::tempdir().unwrap();
        let relative = relative_path("o", "r", 1, 1, "bench-firefox");
        let dest = archive_dest(dir.path(), &relative).unwrap();
        write_zip(&dest, b"zip-bytes").unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), b"zip-bytes");
        assert!(!dest.with_extension("zip.partial").exists());
    }

    #[test]
    fn bench_prefix_does_not_match_telemetry_artifacts() {
        assert!(github::artifact_name_matches("bench-firefox", "bench"));
        assert!(github::artifact_name_matches(
            "bench-firefox-sccache",
            "bench"
        ));
        assert!(!github::artifact_name_matches(
            "telemetry-otlp-v1-firefox",
            "bench"
        ));
    }
}
