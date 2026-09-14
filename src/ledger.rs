use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension};
use std::path::Path;
use std::sync::Mutex;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliveryStatus {
    Delivered,
    Held,
    /// Refused for a reason waiting will not change: an unsupported schema, a
    /// payload that would not parse.
    Skipped,
    /// Emptied by the allowlist. Terminal only while the allowlist that
    /// emptied it is still in force, because widening the allowlist is exactly
    /// the event that makes it wrong.
    Filtered,
}

impl DeliveryStatus {
    fn as_str(self) -> &'static str {
        match self {
            DeliveryStatus::Delivered => "delivered",
            DeliveryStatus::Held => "held",
            DeliveryStatus::Skipped => "skipped",
            DeliveryStatus::Filtered => "filtered",
        }
    }
}

#[derive(Debug, Clone)]
pub struct DeliveryKey {
    pub repo_id: i64,
    pub run_id: i64,
    pub attempt: i64,
    pub artifact_id: i64,
    pub digest: String,
    pub schema_version: u32,
}

/// The part of an immutable artifact or derived attempt that was withheld by
/// the allowlist. Its original OTLP body is small enough to be bounded by the
/// artifact protocol, and keeps replay independent of GitHub retention.
#[derive(Debug, Clone)]
pub struct PendingMetrics {
    pub kind: String,
    pub repo_id: i64,
    pub run_id: i64,
    pub attempt: i64,
    pub artifact_id: i64,
    pub digest: String,
    pub schema_version: u32,
    pub source: String,
    pub pipeline_name: String,
    pub repository_url: String,
    pub payload: Vec<u8>,
}

impl PendingMetrics {
    pub fn artifact(
        key: &DeliveryKey,
        source: String,
        pipeline_name: String,
        repository_url: String,
        payload: Vec<u8>,
    ) -> Self {
        Self {
            kind: "artifact".into(),
            repo_id: key.repo_id,
            run_id: key.run_id,
            attempt: key.attempt,
            artifact_id: key.artifact_id,
            digest: key.digest.clone(),
            schema_version: key.schema_version,
            source,
            pipeline_name,
            repository_url,
            payload,
        }
    }

    pub fn derived(
        repo_id: i64,
        run_id: i64,
        attempt: i64,
        source: String,
        pipeline_name: String,
        repository_url: String,
        payload: Vec<u8>,
    ) -> Self {
        Self {
            kind: "derived".into(),
            repo_id,
            run_id,
            attempt,
            artifact_id: 0,
            digest: String::new(),
            schema_version: 0,
            source,
            pipeline_name,
            repository_url,
            payload,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArchiveStatus {
    Archived,
    Skipped,
    /// Archived once and since deleted by retention.
    ///
    /// Still terminal. The row outlives the file on purpose: without it the
    /// next pass would see an unarchived artifact, download it again, and
    /// retention would delete it again, forever.
    Pruned,
}

impl ArchiveStatus {
    fn as_str(self) -> &'static str {
        match self {
            ArchiveStatus::Archived => "archived",
            ArchiveStatus::Skipped => "skipped",
            ArchiveStatus::Pruned => "pruned",
        }
    }
}

/// An archived artifact and where its bytes were put.
#[derive(Debug, Clone)]
pub struct ArchiveRow {
    pub key: ArchiveKey,
    pub object_key: String,
}

#[derive(Debug, Clone)]
pub struct ArchiveKey {
    pub repo_id: i64,
    pub run_id: i64,
    pub attempt: i64,
    pub artifact_id: i64,
    pub digest: String,
}

pub struct Ledger {
    conn: Mutex<Connection>,
}

impl Ledger {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating ledger dir {}", parent.display()))?;
        }
        let conn =
            Connection::open(path).with_context(|| format!("opening ledger {}", path.display()))?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS deliveries (
                repo_id INTEGER NOT NULL,
                run_id INTEGER NOT NULL,
                attempt INTEGER NOT NULL,
                artifact_id INTEGER NOT NULL,
                digest TEXT NOT NULL,
                schema_version INTEGER NOT NULL,
                status TEXT NOT NULL,
                delivered_at TEXT NOT NULL,
                allowlist TEXT,
                PRIMARY KEY (repo_id, run_id, attempt, artifact_id, digest, schema_version)
            );
            CREATE TABLE IF NOT EXISTS archives (
                repo_id INTEGER NOT NULL,
                run_id INTEGER NOT NULL,
                attempt INTEGER NOT NULL,
                artifact_id INTEGER NOT NULL,
                digest TEXT NOT NULL,
                object_key TEXT NOT NULL,
                status TEXT NOT NULL,
                archived_at TEXT NOT NULL,
                PRIMARY KEY (repo_id, run_id, attempt, artifact_id, digest)
            );
            -- Attempts whose derived metrics have been delivered.
            --
            -- Keyed on the attempt rather than the run, because a rerun bumps
            -- the attempt number on an existing run instead of creating a new
            -- one. A run-keyed table would seal a run at its first attempt and
            -- never look again, losing every rerun and with it the flake
            -- signal that only a second attempt can carry.
            CREATE TABLE IF NOT EXISTS attempts (
                repo_id INTEGER NOT NULL,
                run_id INTEGER NOT NULL,
                attempt INTEGER NOT NULL,
                derived_at TEXT NOT NULL,
                PRIMARY KEY (repo_id, run_id, attempt)
            );
            CREATE TABLE IF NOT EXISTS pending_metrics (
                kind TEXT NOT NULL,
                repo_id INTEGER NOT NULL,
                run_id INTEGER NOT NULL,
                attempt INTEGER NOT NULL,
                artifact_id INTEGER NOT NULL,
                digest TEXT NOT NULL,
                schema_version INTEGER NOT NULL,
                source TEXT NOT NULL,
                pipeline_name TEXT NOT NULL,
                repository_url TEXT NOT NULL,
                payload BLOB NOT NULL,
                allowlist TEXT NOT NULL,
                queued_at TEXT NOT NULL DEFAULT (datetime('now')),
                PRIMARY KEY (kind, repo_id, run_id, attempt, artifact_id, digest, schema_version)
            );
            CREATE TABLE IF NOT EXISTS artifact_scans (
                purpose TEXT NOT NULL,
                repo_id INTEGER NOT NULL,
                run_id INTEGER NOT NULL,
                attempt INTEGER NOT NULL,
                scanned_at INTEGER NOT NULL,
                PRIMARY KEY (purpose, repo_id, run_id, attempt)
            );",
        )?;
        // `CREATE TABLE IF NOT EXISTS` leaves an existing database untouched,
        // so a column added to the definition above never reaches one. Added
        // separately, tolerating the error a second run raises.
        if let Err(err) = conn.execute("ALTER TABLE deliveries ADD COLUMN allowlist TEXT", []) {
            let message = err.to_string();
            if !message.contains("duplicate column name") {
                return Err(err).context("adding the allowlist column to the ledger");
            }
        }
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// Whether this artifact has already been decided, given the allowlist in
    /// force now.
    ///
    /// `delivered`, `held` and `skipped` are decided for good. `filtered` is
    /// decided only while the allowlist that emptied it is unchanged: widening
    /// the allowlist is precisely the event that makes that refusal wrong, and
    /// a seal that outlived it silently kept out the data the widening was for.
    ///
    /// A `filtered` row written before this column existed has a NULL
    /// allowlist. It compares unequal to any fingerprint, so it is retried
    /// once and then sealed against a fingerprint that can be reasoned about.
    pub fn is_terminal(&self, key: &DeliveryKey, allowlist: &str) -> Result<bool> {
        let conn = self.conn.lock().expect("ledger mutex");
        let mut stmt = conn.prepare(
            "SELECT 1 FROM deliveries
             WHERE repo_id = ?1 AND run_id = ?2 AND attempt = ?3
               AND artifact_id = ?4 AND digest = ?5 AND schema_version = ?6
               AND (status IN ('delivered', 'held', 'skipped')
                    OR (status = 'filtered' AND allowlist IS ?7)
                    OR EXISTS (
                        SELECT 1 FROM pending_metrics p
                        WHERE p.kind = 'artifact' AND p.repo_id = deliveries.repo_id
                          AND p.run_id = deliveries.run_id AND p.attempt = deliveries.attempt
                          AND p.artifact_id = deliveries.artifact_id
                          AND p.digest = deliveries.digest
                          AND p.schema_version = deliveries.schema_version
                    ))",
        )?;
        let exists = stmt.exists(rusqlite::params![
            key.repo_id,
            key.run_id,
            key.attempt,
            key.artifact_id,
            key.digest,
            key.schema_version,
            allowlist,
        ])?;
        Ok(exists)
    }

    pub fn archive_is_terminal(&self, key: &ArchiveKey) -> Result<bool> {
        let conn = self.conn.lock().expect("ledger mutex");
        let mut stmt = conn.prepare(
            "SELECT 1 FROM archives
             WHERE repo_id = ?1 AND run_id = ?2 AND attempt = ?3
               AND artifact_id = ?4 AND digest = ?5
               AND status IN ('archived', 'skipped', 'pruned')",
        )?;
        let exists = stmt.exists(rusqlite::params![
            key.repo_id,
            key.run_id,
            key.attempt,
            key.artifact_id,
            key.digest,
        ])?;
        Ok(exists)
    }

    /// Recent runs are checked every pass for late artifacts. Older completed
    /// runs are revisited every six hours, and a failed scan is never cached.
    pub fn should_scan_artifacts(
        &self,
        purpose: &str,
        repo_id: i64,
        run_id: i64,
        attempt: i64,
        completed_at: f64,
    ) -> Result<bool> {
        let now = unix_now();
        if completed_at <= 0.0 || (now as f64 - completed_at) < 2.0 * 3600.0 {
            return Ok(true);
        }
        let conn = self.conn.lock().expect("ledger mutex");
        let last: Option<i64> = conn
            .query_row(
                "SELECT scanned_at FROM artifact_scans
             WHERE purpose = ?1 AND repo_id = ?2 AND run_id = ?3 AND attempt = ?4",
                rusqlite::params![purpose, repo_id, run_id, attempt],
                |row| row.get(0),
            )
            .optional()?;
        Ok(last.is_none_or(|scanned| now.saturating_sub(scanned) >= 6 * 3600))
    }

    pub fn mark_artifacts_scanned(
        &self,
        purpose: &str,
        repo_id: i64,
        run_id: i64,
        attempt: i64,
    ) -> Result<()> {
        let conn = self.conn.lock().expect("ledger mutex");
        conn.execute(
            "INSERT OR REPLACE INTO artifact_scans
             (purpose, repo_id, run_id, attempt, scanned_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![purpose, repo_id, run_id, attempt, unix_now()],
        )?;
        Ok(())
    }

    pub fn prune_artifact_scans(&self) -> Result<()> {
        let conn = self.conn.lock().expect("ledger mutex");
        conn.execute(
            "DELETE FROM artifact_scans WHERE scanned_at < ?1",
            rusqlite::params![unix_now().saturating_sub(30 * 86_400)],
        )?;
        Ok(())
    }

    /// Archived files older than `days`, oldest first.
    ///
    /// Returns the stored relative path so the caller can delete it. Rows
    /// already pruned are excluded, so a pass does not walk what it has
    /// already cleared.
    pub fn archives_older_than(&self, days: u32, limit: usize) -> Result<Vec<ArchiveRow>> {
        let conn = self.conn.lock().expect("ledger mutex");
        let mut stmt = conn.prepare(
            "SELECT repo_id, run_id, attempt, artifact_id, digest, object_key
             FROM archives
             WHERE status = 'archived'
               AND object_key <> ''
               AND archived_at < datetime('now', ?1)
             ORDER BY archived_at ASC
             LIMIT ?2",
        )?;
        let rows = stmt
            .query_map(
                rusqlite::params![format!("-{days} days"), limit as i64],
                |row| {
                    Ok(ArchiveRow {
                        key: ArchiveKey {
                            repo_id: row.get(0)?,
                            run_id: row.get(1)?,
                            attempt: row.get(2)?,
                            artifact_id: row.get(3)?,
                            digest: row.get(4)?,
                        },
                        object_key: row.get(5)?,
                    })
                },
            )?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Move an archive row's timestamp back, for tests that need a horizon.
    #[cfg(test)]
    pub fn backdate_archive_for_test(&self, key: &ArchiveKey, days: i64) -> Result<()> {
        let conn = self.conn.lock().expect("ledger mutex");
        conn.execute(
            "UPDATE archives SET archived_at = datetime('now', ?6)
             WHERE repo_id = ?1 AND run_id = ?2 AND attempt = ?3
               AND artifact_id = ?4 AND digest = ?5",
            rusqlite::params![
                key.repo_id,
                key.run_id,
                key.attempt,
                key.artifact_id,
                key.digest,
                format!("-{days} days")
            ],
        )?;
        Ok(())
    }

    pub fn record_archive(
        &self,
        key: &ArchiveKey,
        object_key: &str,
        status: ArchiveStatus,
    ) -> Result<()> {
        let conn = self.conn.lock().expect("ledger mutex");
        conn.execute(
            "INSERT OR REPLACE INTO archives
             (repo_id, run_id, attempt, artifact_id, digest, object_key, status, archived_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, datetime('now'))",
            rusqlite::params![
                key.repo_id,
                key.run_id,
                key.attempt,
                key.artifact_id,
                key.digest,
                object_key,
                status.as_str(),
            ],
        )?;
        Ok(())
    }

    /// Whether this attempt's metrics have already been delivered.
    pub fn attempt_is_sealed(&self, repo_id: i64, run_id: i64, attempt: i64) -> Result<bool> {
        let conn = self.conn.lock().expect("ledger mutex");
        let mut stmt = conn.prepare(
            "SELECT 1 FROM attempts WHERE repo_id = ?1 AND run_id = ?2 AND attempt = ?3",
        )?;
        Ok(stmt.exists(rusqlite::params![repo_id, run_id, attempt])?)
    }

    /// Sealed only after delivery, so a failed POST replays on the next pass.
    ///
    /// Sealed even when an attempt derived nothing: some legitimately do, and
    /// leaving those open means re-reading them on every sweep until they age
    /// out of the listing.
    pub fn seal_attempt(&self, repo_id: i64, run_id: i64, attempt: i64) -> Result<()> {
        let conn = self.conn.lock().expect("ledger mutex");
        conn.execute(
            "INSERT OR REPLACE INTO attempts (repo_id, run_id, attempt, derived_at)
             VALUES (?1, ?2, ?3, datetime('now'))",
            rusqlite::params![repo_id, run_id, attempt],
        )?;
        Ok(())
    }

    /// Seal a derived attempt and retain only the points refused by this
    /// allowlist. Both decisions must commit together.
    pub fn seal_attempt_with_pending(
        &self,
        repo_id: i64,
        run_id: i64,
        attempt: i64,
        pending: Option<&PendingMetrics>,
        allowlist: &str,
    ) -> Result<()> {
        let mut conn = self.conn.lock().expect("ledger mutex");
        let tx = conn.transaction()?;
        tx.execute(
            "INSERT OR REPLACE INTO attempts (repo_id, run_id, attempt, derived_at)
             VALUES (?1, ?2, ?3, datetime('now'))",
            rusqlite::params![repo_id, run_id, attempt],
        )?;
        if let Some(pending) = pending {
            write_pending(&tx, pending, allowlist)?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Record delivery and the withheld remainder in one SQLite transaction.
    pub fn record_with_pending(
        &self,
        key: &DeliveryKey,
        status: DeliveryStatus,
        pending: Option<&PendingMetrics>,
        allowlist: &str,
    ) -> Result<()> {
        let mut conn = self.conn.lock().expect("ledger mutex");
        let tx = conn.transaction()?;
        tx.execute(
            "INSERT OR REPLACE INTO deliveries
             (repo_id, run_id, attempt, artifact_id, digest, schema_version, status, delivered_at, allowlist)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, datetime('now'), ?8)",
            rusqlite::params![key.repo_id, key.run_id, key.attempt, key.artifact_id,
                key.digest, key.schema_version, status.as_str(), allowlist],
        )?;
        if let Some(pending) = pending {
            write_pending(&tx, pending, allowlist)?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Pending payloads are replayed without another GitHub download. The
    /// configured source must still list successfully before its old data is
    /// replayed.
    pub fn pending_for_source(&self, source: &str, allowlist: &str) -> Result<Vec<PendingMetrics>> {
        let conn = self.conn.lock().expect("ledger mutex");
        let mut stmt = conn.prepare(
            "SELECT kind, repo_id, run_id, attempt, artifact_id, digest,
                    schema_version, source, pipeline_name, repository_url, payload
             FROM pending_metrics
             WHERE source = ?1 AND allowlist <> ?2
             ORDER BY queued_at ASC LIMIT 1",
        )?;
        let rows = stmt
            .query_map(rusqlite::params![source, allowlist], |row| {
                Ok(PendingMetrics {
                    kind: row.get(0)?,
                    repo_id: row.get(1)?,
                    run_id: row.get(2)?,
                    attempt: row.get(3)?,
                    artifact_id: row.get(4)?,
                    digest: row.get(5)?,
                    schema_version: row.get(6)?,
                    source: row.get(7)?,
                    pipeline_name: row.get(8)?,
                    repository_url: row.get(9)?,
                    payload: row.get(10)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// The ledger PVC is bounded. A pending payload has a 30-day replay
    /// horizon; the returned count lets the caller report removed rows.
    pub fn prune_pending(&self) -> Result<usize> {
        let conn = self.conn.lock().expect("ledger mutex");
        let count = conn.execute(
            "DELETE FROM pending_metrics WHERE queued_at < datetime('now', '-30 days')",
            [],
        )?;
        Ok(count)
    }

    pub fn pending_stats(&self) -> Result<(i64, i64)> {
        let conn = self.conn.lock().expect("ledger mutex");
        Ok(conn.query_row(
            "SELECT count(*), coalesce(sum(length(payload)), 0) FROM pending_metrics",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?)
    }

    /// Replace the withheld remainder after a changed allowlist. Removing the
    /// last part also turns an entirely filtered artifact into delivered.
    pub fn advance_pending(
        &self,
        previous: &PendingMetrics,
        remainder: Option<&[u8]>,
        allowlist: &str,
    ) -> Result<()> {
        let mut conn = self.conn.lock().expect("ledger mutex");
        let tx = conn.transaction()?;
        if let Some(payload) = remainder {
            tx.execute(
                "UPDATE pending_metrics SET payload = ?8, allowlist = ?9
                 WHERE kind = ?1 AND repo_id = ?2 AND run_id = ?3 AND attempt = ?4
                   AND artifact_id = ?5 AND digest = ?6 AND schema_version = ?7",
                rusqlite::params![
                    previous.kind,
                    previous.repo_id,
                    previous.run_id,
                    previous.attempt,
                    previous.artifact_id,
                    previous.digest,
                    previous.schema_version,
                    payload,
                    allowlist
                ],
            )?;
        } else {
            tx.execute(
                "DELETE FROM pending_metrics
                 WHERE kind = ?1 AND repo_id = ?2 AND run_id = ?3 AND attempt = ?4
                   AND artifact_id = ?5 AND digest = ?6 AND schema_version = ?7",
                rusqlite::params![
                    previous.kind,
                    previous.repo_id,
                    previous.run_id,
                    previous.attempt,
                    previous.artifact_id,
                    previous.digest,
                    previous.schema_version
                ],
            )?;
            if previous.kind == "artifact" {
                tx.execute(
                    "UPDATE deliveries SET status = 'delivered', allowlist = ?7
                     WHERE repo_id = ?1 AND run_id = ?2 AND attempt = ?3
                       AND artifact_id = ?4 AND digest = ?5 AND schema_version = ?6",
                    rusqlite::params![
                        previous.repo_id,
                        previous.run_id,
                        previous.attempt,
                        previous.artifact_id,
                        previous.digest,
                        previous.schema_version,
                        allowlist
                    ],
                )?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn record(&self, key: &DeliveryKey, status: DeliveryStatus) -> Result<()> {
        self.record_against(key, status, None)
    }

    /// Record a decision, noting the allowlist that made it.
    ///
    /// Only meaningful for `Filtered`: the fingerprint is what lets a later
    /// pass tell "the allowlist still refuses this" from "the allowlist used
    /// to refuse this".
    pub fn record_against(
        &self,
        key: &DeliveryKey,
        status: DeliveryStatus,
        allowlist: Option<&str>,
    ) -> Result<()> {
        let conn = self.conn.lock().expect("ledger mutex");
        conn.execute(
            "INSERT OR REPLACE INTO deliveries
             (repo_id, run_id, attempt, artifact_id, digest, schema_version, status, delivered_at, allowlist)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, datetime('now'), ?8)",
            rusqlite::params![
                key.repo_id,
                key.run_id,
                key.attempt,
                key.artifact_id,
                key.digest,
                key.schema_version,
                status.as_str(),
                allowlist,
            ],
        )?;
        Ok(())
    }
}

fn write_pending(
    tx: &rusqlite::Transaction<'_>,
    pending: &PendingMetrics,
    allowlist: &str,
) -> Result<()> {
    tx.execute(
        "INSERT OR REPLACE INTO pending_metrics
         (kind, repo_id, run_id, attempt, artifact_id, digest, schema_version,
          source, pipeline_name, repository_url, payload, allowlist)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
        rusqlite::params![
            pending.kind,
            pending.repo_id,
            pending.run_id,
            pending.attempt,
            pending.artifact_id,
            pending.digest,
            pending.schema_version,
            pending.source,
            pending.pipeline_name,
            pending.repository_url,
            pending.payload,
            allowlist
        ],
    )?;
    Ok(())
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filtered_artifact_replays_from_ledger_without_resending_accepted_points() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = Ledger::open(&dir.path().join("ledger.sqlite")).unwrap();
        let key = DeliveryKey {
            repo_id: 1,
            run_id: 2,
            attempt: 1,
            artifact_id: 3,
            digest: "sha256:abc".into(),
            schema_version: 1,
        };
        let pending = PendingMetrics::artifact(
            &key,
            "owner/repo".into(),
            "CI".into(),
            "https://github.com/owner/repo".into(),
            b"withheld".to_vec(),
        );
        ledger
            .record_with_pending(&key, DeliveryStatus::Filtered, Some(&pending), "old")
            .unwrap();
        assert!(ledger.is_terminal(&key, "new").unwrap());
        assert!(
            ledger
                .pending_for_source("owner/repo", "old")
                .unwrap()
                .is_empty()
        );
        let replay = ledger.pending_for_source("owner/repo", "new").unwrap();
        assert_eq!(replay.len(), 1);
        assert_eq!(replay[0].payload, b"withheld");
        ledger
            .advance_pending(&replay[0], Some(b"still withheld"), "new")
            .unwrap();
        assert!(
            ledger
                .pending_for_source("owner/repo", "new")
                .unwrap()
                .is_empty()
        );
        let replay = ledger.pending_for_source("owner/repo", "another").unwrap();
        assert_eq!(replay[0].payload, b"still withheld");
        ledger.advance_pending(&replay[0], None, "another").unwrap();
        assert!(
            ledger
                .pending_for_source("owner/repo", "another")
                .unwrap()
                .is_empty()
        );
        assert!(ledger.is_terminal(&key, "another").unwrap());
    }

    #[test]
    fn pending_payloads_expire_after_thirty_days() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = Ledger::open(&dir.path().join("ledger.sqlite")).unwrap();
        let key = DeliveryKey {
            repo_id: 1,
            run_id: 2,
            attempt: 1,
            artifact_id: 3,
            digest: "sha256:abc".into(),
            schema_version: 1,
        };
        let pending = PendingMetrics::artifact(
            &key,
            "owner/repo".into(),
            "CI".into(),
            "https://github.com/owner/repo".into(),
            b"withheld".to_vec(),
        );
        ledger
            .record_with_pending(&key, DeliveryStatus::Filtered, Some(&pending), "old")
            .unwrap();
        assert_eq!(ledger.pending_stats().unwrap(), (1, 8));
        ledger
            .conn
            .lock()
            .unwrap()
            .execute(
                "UPDATE pending_metrics SET queued_at = datetime('now', '-31 days')",
                [],
            )
            .unwrap();
        assert_eq!(ledger.prune_pending().unwrap(), 1);
        assert_eq!(ledger.pending_stats().unwrap(), (0, 0));
    }

    #[test]
    fn delivered_artifact_keeps_only_its_withheld_remainder() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = Ledger::open(&dir.path().join("ledger.sqlite")).unwrap();
        let key = DeliveryKey {
            repo_id: 1,
            run_id: 2,
            attempt: 1,
            artifact_id: 3,
            digest: "sha256:abc".into(),
            schema_version: 1,
        };
        let pending = PendingMetrics::artifact(
            &key,
            "owner/repo".into(),
            "CI".into(),
            "https://github.com/owner/repo".into(),
            b"only dropped points".to_vec(),
        );
        ledger
            .record_with_pending(&key, DeliveryStatus::Delivered, Some(&pending), "old")
            .unwrap();
        assert!(ledger.is_terminal(&key, "new").unwrap());
        let replay = ledger.pending_for_source("owner/repo", "new").unwrap();
        assert_eq!(replay[0].payload, b"only dropped points");
        ledger.advance_pending(&replay[0], None, "new").unwrap();
        assert!(ledger.is_terminal(&key, "newer").unwrap());
        assert!(
            ledger
                .pending_for_source("owner/repo", "newer")
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn a_derived_attempt_can_replay_its_withheld_points() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = Ledger::open(&dir.path().join("ledger.sqlite")).unwrap();
        let pending = PendingMetrics::derived(
            1,
            2,
            3,
            "owner/repo".into(),
            "CI".into(),
            "https://github.com/owner/repo".into(),
            b"withheld".to_vec(),
        );
        ledger
            .seal_attempt_with_pending(1, 2, 3, Some(&pending), "old")
            .unwrap();
        assert!(ledger.attempt_is_sealed(1, 2, 3).unwrap());
        let replay = ledger.pending_for_source("owner/repo", "new").unwrap();
        assert_eq!(replay[0].payload, b"withheld");
        ledger.advance_pending(&replay[0], None, "new").unwrap();
        assert!(
            ledger
                .pending_for_source("owner/repo", "newer")
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn old_runs_are_scanned_periodically_and_failed_scans_are_not_cached() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = Ledger::open(&dir.path().join("ledger.sqlite")).unwrap();
        let old = unix_now() as f64 - 3.0 * 3600.0;
        assert!(
            ledger
                .should_scan_artifacts("collect", 1, 2, 1, old)
                .unwrap()
        );
        ledger.mark_artifacts_scanned("collect", 1, 2, 1).unwrap();
        assert!(
            !ledger
                .should_scan_artifacts("collect", 1, 2, 1, old)
                .unwrap()
        );
        assert!(
            ledger
                .should_scan_artifacts("archive", 1, 2, 1, old)
                .unwrap()
        );
        assert!(
            ledger
                .should_scan_artifacts("collect", 1, 2, 2, old)
                .unwrap()
        );
        assert!(
            ledger
                .should_scan_artifacts("collect", 1, 2, 1, unix_now() as f64)
                .unwrap()
        );
    }

    #[test]
    fn delivered_key_is_not_retried() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = Ledger::open(&dir.path().join("ledger.sqlite")).unwrap();
        let key = DeliveryKey {
            repo_id: 1,
            run_id: 2,
            attempt: 1,
            artifact_id: 9,
            digest: "sha256:abc".into(),
            schema_version: 1,
        };
        assert!(!ledger.is_terminal(&key, "test").unwrap());
        ledger.record(&key, DeliveryStatus::Delivered).unwrap();
        assert!(ledger.is_terminal(&key, "test").unwrap());
    }

    #[test]
    fn held_and_skipped_are_terminal() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = Ledger::open(&dir.path().join("ledger.sqlite")).unwrap();
        let key = DeliveryKey {
            repo_id: 1,
            run_id: 2,
            attempt: 1,
            artifact_id: 9,
            digest: "sha256:abc".into(),
            schema_version: 1,
        };
        ledger.record(&key, DeliveryStatus::Held).unwrap();
        assert!(ledger.is_terminal(&key, "test").unwrap());
        ledger.record(&key, DeliveryStatus::Skipped).unwrap();
        assert!(ledger.is_terminal(&key, "test").unwrap());
    }

    #[test]
    fn archived_key_is_not_retried_and_is_separate_from_delivery() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = Ledger::open(&dir.path().join("ledger.sqlite")).unwrap();
        let delivery = DeliveryKey {
            repo_id: 1,
            run_id: 2,
            attempt: 1,
            artifact_id: 9,
            digest: "sha256:abc".into(),
            schema_version: 1,
        };
        let archive = ArchiveKey {
            repo_id: 1,
            run_id: 2,
            attempt: 1,
            artifact_id: 9,
            digest: "sha256:abc".into(),
        };
        ledger.record(&delivery, DeliveryStatus::Delivered).unwrap();
        assert!(!ledger.archive_is_terminal(&archive).unwrap());
        ledger
            .record_archive(&archive, "kache/bench/9.zip", ArchiveStatus::Archived)
            .unwrap();
        assert!(ledger.archive_is_terminal(&archive).unwrap());
        assert!(ledger.is_terminal(&delivery, "test").unwrap());
    }
}

#[cfg(test)]
mod allowlist_sealing {
    use super::*;

    fn key() -> DeliveryKey {
        DeliveryKey {
            repo_id: 1,
            run_id: 2,
            attempt: 1,
            artifact_id: 3,
            digest: "sha256:abc".into(),
            schema_version: 1,
        }
    }

    fn ledger() -> (Ledger, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let ledger = Ledger::open(&dir.path().join("ledger.sqlite")).unwrap();
        (ledger, dir)
    }

    /// The whole point. An artifact the allowlist emptied is not read again
    /// while that allowlist stands, so a pass does not re-download it hourly.
    #[test]
    fn a_filtered_artifact_is_not_re_read_under_the_same_allowlist() {
        let (l, _d) = ledger();
        l.record_against(&key(), DeliveryStatus::Filtered, Some("aaaa"))
            .unwrap();
        assert!(l.is_terminal(&key(), "aaaa").unwrap());
    }

    /// And the other half. Widening the allowlist is exactly the event that
    /// makes the refusal wrong, so the seal must not survive it -- this is the
    /// case that stranded four nights of kache bench data.
    #[test]
    fn widening_the_allowlist_un_seals_it() {
        let (l, _d) = ledger();
        l.record_against(&key(), DeliveryStatus::Filtered, Some("aaaa"))
            .unwrap();
        assert!(
            !l.is_terminal(&key(), "bbbb").unwrap(),
            "a changed allowlist must re-read what the old one refused"
        );
    }

    /// A refusal that is a property of the artifact stays decided whatever the
    /// allowlist does. An unsupported schema does not become supported.
    #[test]
    fn a_skipped_artifact_stays_decided_across_allowlists() {
        let (l, _d) = ledger();
        l.record(&key(), DeliveryStatus::Skipped).unwrap();
        assert!(l.is_terminal(&key(), "aaaa").unwrap());
        assert!(l.is_terminal(&key(), "bbbb").unwrap());
    }

    #[test]
    fn delivered_and_held_are_unaffected() {
        let (l, _d) = ledger();
        l.record(&key(), DeliveryStatus::Delivered).unwrap();
        assert!(l.is_terminal(&key(), "anything").unwrap());
        let (l2, _d2) = ledger();
        l2.record(&key(), DeliveryStatus::Held).unwrap();
        assert!(l2.is_terminal(&key(), "anything").unwrap());
    }

    /// Rows written before the column existed carry NULL. `IS ?` compares them
    /// unequal to every fingerprint, so they are read once more and then
    /// sealed against something that can be reasoned about -- which is how the
    /// nights already stranded come back.
    #[test]
    fn a_row_from_before_the_column_is_read_once_more() {
        let (l, _d) = ledger();
        l.record_against(&key(), DeliveryStatus::Filtered, None)
            .unwrap();
        assert!(
            !l.is_terminal(&key(), "aaaa").unwrap(),
            "a legacy filtered row must not stay sealed forever"
        );
    }
}

#[cfg(test)]
mod migration {
    use super::*;
    use rusqlite::Connection;

    /// The ledger in the cluster predates the allowlist column. Opening it
    /// must add the column rather than leave it behind: `CREATE TABLE IF NOT
    /// EXISTS` does nothing to a table that already exists, so a column added
    /// to the definition alone would never reach a running deployment.
    #[test]
    fn opens_a_ledger_written_before_the_column_existed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.sqlite");

        // The schema exactly as it was, without `allowlist`.
        let old = Connection::open(&path).unwrap();
        old.execute_batch(
            "CREATE TABLE deliveries (
                repo_id INTEGER NOT NULL,
                run_id INTEGER NOT NULL,
                attempt INTEGER NOT NULL,
                artifact_id INTEGER NOT NULL,
                digest TEXT NOT NULL,
                schema_version INTEGER NOT NULL,
                status TEXT NOT NULL,
                delivered_at TEXT NOT NULL,
                PRIMARY KEY (repo_id, run_id, attempt, artifact_id, digest, schema_version)
            );",
        )
        .unwrap();
        old.execute(
            "INSERT INTO deliveries VALUES (1, 2, 1, 3, 'sha256:abc', 1, 'skipped', datetime('now'))",
            [],
        )
        .unwrap();
        drop(old);

        let ledger = Ledger::open(&path).unwrap();
        let key = DeliveryKey {
            repo_id: 1,
            run_id: 2,
            attempt: 1,
            artifact_id: 3,
            digest: "sha256:abc".into(),
            schema_version: 1,
        };
        // The pre-existing row survives and keeps its meaning.
        assert!(ledger.is_terminal(&key, "aaaa").unwrap());
        // And the new column is usable.
        ledger
            .record_against(&key, DeliveryStatus::Filtered, Some("aaaa"))
            .unwrap();
        assert!(ledger.is_terminal(&key, "aaaa").unwrap());
        assert!(!ledger.is_terminal(&key, "bbbb").unwrap());
    }

    /// Opening twice must not fail on the column already being there.
    #[test]
    fn opening_an_already_migrated_ledger_is_fine() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.sqlite");
        drop(Ledger::open(&path).unwrap());
        drop(Ledger::open(&path).unwrap());
        Ledger::open(&path).expect("a third open must also succeed");
    }
}
