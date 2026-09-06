//! The GitHub Actions payloads the derivation reads.
//!
//! Only the fields the rules actually use. Everything else the API sends is
//! ignored, so a new field upstream cannot break deserialisation and an
//! unused one cannot quietly become load-bearing.

use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct RunAttempt {
    pub id: i64,
    pub run_attempt: i64,
    pub event: String,
    pub status: String,
    pub conclusion: Option<String>,
    pub created_at: String,
    pub run_started_at: String,
    pub path: String,
    pub head_branch: Option<String>,
    pub repository: Repository,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Repository {
    pub full_name: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct JobStep {
    pub name: String,
    pub conclusion: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Job {
    pub name: String,
    pub conclusion: Option<String>,
    pub created_at: String,
    pub started_at: Option<String>,
    pub completed_at: Option<String>,
    /// False when the job never reached a runner, so its timestamps describe
    /// a wait rather than work.
    #[serde(default)]
    pub runner_present: bool,
    #[serde(default)]
    pub labels: Vec<String>,
    #[serde(default)]
    pub steps: Vec<JobStep>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct JobsPayload {
    #[serde(default)]
    pub jobs: Vec<Job>,
}

/// One derived observation, before it becomes OTLP.
#[derive(Debug, Clone, PartialEq)]
pub struct Point {
    pub metric: &'static str,
    pub instrument: Instrument,
    pub unit: &'static str,
    pub value: f64,
    pub attributes: Vec<(String, String)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Instrument {
    Counter,
    Histogram,
}

impl Point {
    pub fn attribute(&self, key: &str) -> Option<&str> {
        self.attributes
            .iter()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value.as_str())
    }
}

/// What a sweep had to throw away, and why.
///
/// Named rather than free text so the vocabulary stays bounded: these become
/// an attribute on `ci.collector.anomalies`, and an unbounded one there opens
/// a series per oddity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Anomaly {
    RunNotCompleted,
    IntentionalFailureAttempt,
    Attempt1ClockMismatch,
    GateMissing,
    UnusableTimestamp,
    UnknownJobName,
    FilterJobMissing,
}

impl Anomaly {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::RunNotCompleted => "run_not_completed",
            Self::IntentionalFailureAttempt => "intentional_failure_attempt",
            Self::Attempt1ClockMismatch => "attempt1_clock_mismatch",
            Self::GateMissing => "gate_missing",
            Self::UnusableTimestamp => "unusable_timestamp",
            Self::UnknownJobName => "unknown_job_name",
            Self::FilterJobMissing => "filter_job_missing",
        }
    }
}

#[derive(Debug, Default)]
pub struct Derivation {
    pub points: Vec<Point>,
    pub anomalies: Vec<Anomaly>,
}
