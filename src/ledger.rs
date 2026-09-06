use anyhow::{Context, Result};
use rusqlite::Connection;
use std::path::Path;
use std::sync::Mutex;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliveryStatus {
    Delivered,
    Held,
    Skipped,
}

impl DeliveryStatus {
    fn as_str(self) -> &'static str {
        match self {
            DeliveryStatus::Delivered => "delivered",
            DeliveryStatus::Held => "held",
            DeliveryStatus::Skipped => "skipped",
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
}

impl ArchiveStatus {
    fn as_str(self) -> &'static str {
        match self {
            ArchiveStatus::Archived => "archived",
            ArchiveStatus::Skipped => "skipped",
        }
    }
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
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    pub fn is_terminal(&self, key: &DeliveryKey) -> Result<bool> {
        let conn = self.conn.lock().expect("ledger mutex");
        let mut stmt = conn.prepare(
            "SELECT 1 FROM deliveries
             WHERE repo_id = ?1 AND run_id = ?2 AND attempt = ?3
               AND artifact_id = ?4 AND digest = ?5 AND schema_version = ?6
               AND status IN ('delivered', 'held', 'skipped')",
        )?;
        let exists = stmt.exists(rusqlite::params![
            key.repo_id,
            key.run_id,
            key.attempt,
            key.artifact_id,
            key.digest,
            key.schema_version,
        ])?;
        Ok(exists)
    }

    pub fn archive_is_terminal(&self, key: &ArchiveKey) -> Result<bool> {
        let conn = self.conn.lock().expect("ledger mutex");
        let mut stmt = conn.prepare(
            "SELECT 1 FROM archives
             WHERE repo_id = ?1 AND run_id = ?2 AND attempt = ?3
               AND artifact_id = ?4 AND digest = ?5
               AND status IN ('archived', 'skipped')",
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
        let conn = self.conn.lock().expect("ledger mutex");
        conn.execute(
            "INSERT OR REPLACE INTO deliveries
             (repo_id, run_id, attempt, artifact_id, digest, schema_version, status, delivered_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, datetime('now'))",
            rusqlite::params![
                key.repo_id,
                key.run_id,
                key.attempt,
                key.artifact_id,
                key.digest,
                key.schema_version,
                status.as_str(),
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
        assert!(!ledger.is_terminal(&key).unwrap());
        ledger.record(&key, DeliveryStatus::Delivered).unwrap();
        assert!(ledger.is_terminal(&key).unwrap());
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
        assert!(ledger.is_terminal(&key).unwrap());
        ledger.record(&key, DeliveryStatus::Skipped).unwrap();
        assert!(ledger.is_terminal(&key).unwrap());
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
        assert!(ledger.is_terminal(&delivery).unwrap());
    }
}
