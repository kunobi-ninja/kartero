//! Optional on-disk copy of GitHub diagnostic artifacts.
//!
//! Independent of collect: its own prefix, size cap, ledger table, and
//! failure domain. Disabled unless `Config.archive` is set. Writes zip
//! files under a configured directory (a cluster PVC). Does not parse
//! OTLP, talk to object storage, or write to SigNoz.

use crate::config::{ArchiveConfig, Config, SourceConfig};
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
    let mut failed = Vec::new();
    for source in &config.sources {
        if let Err(err) = archive_source(archive, source, config.lookback, &ledger, snapshot).await
        {
            warn!(source = %source.slug(), error = %err, "archiving source failed");
            failed.push(source.slug());
        }
    }
    if !failed.is_empty() {
        bail!("archive failed for {}", failed.join(", "));
    }
    Ok(())
}

async fn archive_source(
    archive: &ArchiveConfig,
    source: &SourceConfig,
    lookback: std::time::Duration,
    ledger: &Ledger,
    snapshot: &mut ArchiveSnapshot,
) -> Result<()> {
    let github = GitHub::new(source.clone())?;
    let runs = match github.list_completed_runs(lookback).await {
        Ok(runs) => runs,
        Err(err) => {
            snapshot.github_errors += 1;
            warn!(source = %source.slug(), error = %err, "archive: listing completed GitHub workflow runs failed");
            return Err(err);
        }
    };
    snapshot.runs_seen += runs.len() as u64;
    let mut had_errors = false;
    for run in runs {
        if !github.trusted(&run) {
            continue;
        }
        snapshot.runs_trusted += 1;
        let artifacts = match github.list_artifacts(run.run_id).await {
            Ok(list) => list,
            Err(err) => {
                warn!(source = %source.slug(), run_id = run.run_id, error = %err, "archive: listing artifacts failed");
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
                archive_one(archive, source, ledger, &github, snapshot, &run, &artifact).await
            {
                warn!(
                    source = %source.slug(),
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
    archive: &ArchiveConfig,
    source: &SourceConfig,
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
        &source.owner,
        &source.repo,
        run.run_id,
        run.attempt,
        &artifact.name,
    );
    let dest = archive_dest(&archive.dir, &relative)?;
    let zip = github
        .download_zip_limited(artifact.id, archive.max_bytes)
        .await?;
    write_archived(&dest, &zip, source, run, artifact)?;
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

/// What an archived artifact belongs to, beside the artifact.
///
/// A directory of zips keyed on `run_id` answers "the artifact for run
/// 34074500942" and nothing else. The question people actually arrive with is
/// "the trace for the commit that regressed on Tuesday", and neither the path
/// nor the metrics can answer it: the metrics carry no run id on purpose,
/// because that is one series per run.
///
/// So the join lives here, in a file next to the zip, where naming a commit
/// and an instant costs a few hundred bytes and opens no series at all.
#[derive(serde::Serialize)]
struct Sidecar<'a> {
    owner: &'a str,
    repo: &'a str,
    /// The commit the run built. The field the whole sidecar exists for.
    head_sha: &'a str,
    head_branch: &'a str,
    event: &'a str,
    workflow: &'a str,
    run_id: i64,
    attempt: i64,
    artifact: &'a str,
    artifact_id: i64,
    /// GitHub's digest of the artifact, so a zip can be told apart from a
    /// re-upload under the same name.
    digest: &'a str,
    /// When the run was created, as GitHub reports it. The join to a metric
    /// point, which carries a timestamp and no identifiers.
    run_created_at: &'a str,
    archived_at: String,
    schema: u32,
}

/// The sidecar's own version, so a reader can tell a field that is absent from
/// one this writer never wrote.
const SIDECAR_SCHEMA: u32 = 1;

fn sidecar_path(zip: &Path) -> PathBuf {
    zip.with_extension("json")
}

fn write_sidecar(
    zip: &Path,
    source: &SourceConfig,
    run: &WorkflowRun,
    artifact: &ArtifactRef,
) -> Result<()> {
    let sidecar = Sidecar {
        owner: &source.owner,
        repo: &source.repo,
        head_sha: &run.head_sha,
        head_branch: &run.head_branch,
        event: &run.event,
        workflow: &run.workflow_name,
        run_id: run.run_id,
        attempt: run.attempt,
        artifact: &artifact.name,
        artifact_id: artifact.id,
        digest: &artifact.digest,
        run_created_at: &run.detail.created_at,
        archived_at: now_rfc3339(),
        schema: SIDECAR_SCHEMA,
    };
    let path = sidecar_path(zip);
    let body = serde_json::to_vec_pretty(&sidecar)?;
    write_atomic(&path, &body).with_context(|| format!("writing sidecar {}", path.display()))
}

/// `YYYY-MM-DDTHH:MM:SSZ`, formatted from the clock rather than pulled in with
/// a date library for one field.
fn now_rfc3339() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rem / 3_600,
        (rem % 3_600) / 60,
        rem % 60
    )
}

/// The inverse of the civil-date algorithm `github` uses for token expiry.
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (y + i64::from(m <= 2), m, d)
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

/// The zip and the note saying what it belongs to, together.
///
/// One function rather than two calls, because they are not independently
/// useful: a zip with no sidecar is a file keyed on a run id nobody can map to
/// a commit, which is the state this whole thing exists to leave behind.
fn write_archived(
    dest: &Path,
    zip: &[u8],
    source: &SourceConfig,
    run: &WorkflowRun,
    artifact: &ArtifactRef,
) -> Result<()> {
    write_zip(dest, zip)?;
    write_sidecar(dest, source, run, artifact)
}

fn write_zip(dest: &Path, bytes: &[u8]) -> Result<()> {
    write_atomic(dest, bytes)
}

/// Write through a sibling temporary file and rename.
///
/// A reader that finds the final name finds a whole file: an archive pass that
/// dies mid-write leaves a `.partial` nobody reads rather than a truncated zip
/// or a sidecar naming a commit for bytes that were never finished.
fn write_atomic(dest: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let tmp = dest.with_extension(match dest.extension().and_then(|e| e.to_str()) {
        Some(ext) => format!("{ext}.partial"),
        None => "partial".to_string(),
    });
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
    fn config_disabled() -> Config {
        Config {
            bind: "127.0.0.1:0".into(),
            interval: Duration::from_secs(3600),
            heartbeat_interval: Duration::from_secs(60),
            lookback: Duration::from_secs(86_400),
            sources: vec![SourceConfig {
                token: "token".into(),
                owner: "kunobi-ninja".into(),
                repo: "kache".into(),
                workflows: vec!["bench.yml".into()],
                trusted_branch: "main".into(),
                actions: None,
            }],
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

#[cfg(test)]
mod sidecar_tests {
    use super::*;

    fn run(head_sha: &str) -> WorkflowRun {
        WorkflowRun {
            detail: crate::actions::RunAttempt {
                id: 34_074_500_942,
                run_attempt: 1,
                event: "schedule".into(),
                status: "completed".into(),
                conclusion: Some("failure".into()),
                created_at: "2026-09-07T01:54:18Z".into(),
                run_started_at: "2026-09-07T01:54:20Z".into(),
                path: ".github/workflows/bench.yml".into(),
                head_branch: Some("main".into()),
                repository: crate::actions::model::Repository {
                    full_name: "kunobi-ninja/kache".into(),
                },
            },
            observed_at: 0.0,
            head_sha: head_sha.into(),
            repo_id: 7,
            run_id: 34_074_500_942,
            attempt: 1,
            event: "schedule".into(),
            head_branch: "main".into(),
            workflow_id: 297_515_584,
            workflow_name: "Bench".into(),
            conclusion: Some("failure".into()),
        }
    }

    fn source() -> SourceConfig {
        SourceConfig {
            token: String::new(),
            owner: "kunobi-ninja".into(),
            repo: "kache".into(),
            workflows: vec!["bench.yml".into()],
            trusted_branch: "main".into(),
            actions: None,
        }
    }

    fn artifact() -> ArtifactRef {
        ArtifactRef {
            id: 10_004_612_582,
            name: "bench-substrate".into(),
            digest: "sha256:abc".into(),
            size_in_bytes: 1_856_417,
            expired: false,
        }
    }

    /// The whole point: a zip on disk can be traced back to the commit that
    /// produced it. The path carries a run id and nothing else, and the metrics
    /// carry no run id at all, so without this a directory of archives answers
    /// no question anyone actually arrives with.
    #[test]
    fn a_zip_can_be_traced_back_to_its_commit() {
        let dir = tempfile::tempdir().unwrap();
        let zip = dir
            .path()
            .join("kunobi-ninja/kache/34074500942/1/bench-substrate.zip");
        write_archived(
            &zip,
            b"zip-bytes",
            &source(),
            &run("9f3c1ab2de4501776e0d3c1a5b7e9042f8c6d1aa"),
            &artifact(),
        )
        .unwrap();

        let side = sidecar_path(&zip);
        let read: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&side).unwrap()).unwrap();
        assert_eq!(read["head_sha"], "9f3c1ab2de4501776e0d3c1a5b7e9042f8c6d1aa");
        assert_eq!(read["artifact"], "bench-substrate");
        assert_eq!(read["run_id"], 34_074_500_942_i64);
        assert_eq!(read["workflow"], "Bench");
    }

    /// The other join. A metric point carries a timestamp and no identifiers,
    /// so the run's own creation time is what lets a regression seen at an
    /// instant reach the artifact that produced it.
    #[test]
    fn it_carries_the_run_time_a_metric_point_can_be_joined_on() {
        let dir = tempfile::tempdir().unwrap();
        let zip = dir.path().join("a.zip");
        write_archived(&zip, b"z", &source(), &run("abc"), &artifact()).unwrap();
        let read: serde_json::Value =
            serde_json::from_slice(&std::fs::read(sidecar_path(&zip)).unwrap()).unwrap();
        assert_eq!(read["run_created_at"], "2026-09-07T01:54:18Z");
        let archived = read["archived_at"].as_str().unwrap();
        assert!(
            archived.len() == 20 && archived.ends_with('Z') && archived.starts_with("20"),
            "archived_at is not an RFC3339 instant: {archived}"
        );
    }

    /// It sits beside the zip under the same name, so finding one from the
    /// other needs no index.
    #[test]
    fn it_sits_beside_the_zip() {
        let zip = Path::new("/archive/o/r/1/1/bench-firefox.zip");
        assert_eq!(
            sidecar_path(zip),
            Path::new("/archive/o/r/1/1/bench-firefox.json")
        );
    }

    /// A partial write must never be readable under the final name: a sidecar
    /// naming a commit for a zip that was never finished is worse than none.
    #[test]
    fn a_sidecar_is_written_atomically() {
        let dir = tempfile::tempdir().unwrap();
        let zip = dir.path().join("a.zip");
        write_archived(&zip, b"z", &source(), &run("abc"), &artifact()).unwrap();
        assert!(!dir.path().join("a.json.partial").exists());
        assert!(sidecar_path(&zip).exists());
    }

    #[test]
    fn the_instant_formatter_agrees_with_a_known_epoch() {
        // 1788782058 == 2026-09-07T11:54:18Z
        let (y, m, d) = civil_from_days(1_788_782_058_i64.div_euclid(86_400));
        assert_eq!((y, m, d), (2026, 9, 7));
    }
}
