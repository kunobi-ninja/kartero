use anyhow::{Context, Result};
use rusqlite::Connection;
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
                    OR (status = 'filtered' AND allowlist IS ?7))",
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

#[cfg(test)]
mod tests {
    use super::*;

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
