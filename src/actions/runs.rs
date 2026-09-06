//! Run-level metrics for one sealed attempt.
//!
//! Three series, and the difference between two of them is the point:
//!
//! - `ci.run.gate.duration` is the mergeability clock. A pull request is not
//!   mergeable until the aggregating gate concludes, so the gate's own queue
//!   wait is deliberately inside the measurement.
//! - `ci.run.checks.duration` is the real-work clock, ending at the last job
//!   that did work. What separates it from the gate is what the gate costs.
//! - `ci.run.attempts` is the denominator. The gate histogram's count cannot
//!   serve as one, because a cancelled run usually never reaches a completed
//!   gate and so contributes nothing to it.

use super::classify::{
    attempt_clock_start, classify_attempt, classify_branch, classify_event, elapsed_seconds,
    is_carried_forward,
};
use super::model::{Anomaly, Derivation, Instrument, Job, JobsPayload, Point, RunAttempt};
use crate::config::ActionsConfig;

/// Bucket bounds for a run-level duration, in seconds.
///
/// The widest bound is the ceiling of what can be measured: anything past it
/// is indistinguishable from anything else past it, so a p99 during a bad week
/// is a floor rather than a measurement.
pub const RUN_DURATION_BOUNDS: [f64; 12] = [
    60.0, 120.0, 300.0, 600.0, 900.0, 1200.0, 1800.0, 2400.0, 3000.0, 3600.0, 4500.0, 5400.0,
];

/// Attributes carried by every run-level metric. All bounded, no identity.
pub fn run_attributes(run: &RunAttempt, config: &ActionsConfig) -> Vec<(String, String)> {
    vec![
        ("repository".into(), run.repository.full_name.clone()),
        ("workflow_path".into(), run.path.clone()),
        ("event".into(), classify_event(&run.event).into()),
        ("branch_class".into(), classify_branch(run, config)),
        ("attempt_class".into(), classify_attempt(run).into()),
    ]
}

/// A job counts toward the real-work clock when it actually did work in this
/// attempt: not the gate, not skipped, not carried forward from an earlier
/// attempt, and reached a completion.
fn did_work_this_attempt(job: &Job, config: &ActionsConfig) -> bool {
    job.name != config.gate_job
        && job.conclusion.as_deref() != Some("skipped")
        && job.completed_at.is_some()
        && !is_carried_forward(job)
}

pub fn derive(run: &RunAttempt, jobs: &JobsPayload, config: &ActionsConfig) -> Derivation {
    let mut out = Derivation::default();

    // An attempt still executing has no final conclusion and no last job.
    // Leaving it unsealed lets a later sweep pick it up.
    if run.status != "completed" {
        out.anomalies.push(Anomaly::RunNotCompleted);
        return out;
    }
    if super::jobs::is_intentional_failure(&jobs.jobs, config) {
        out.anomalies.push(Anomaly::IntentionalFailureAttempt);
        return out;
    }

    let attributes = run_attributes(run, config);
    let (start, mismatched) = attempt_clock_start(run);
    if mismatched {
        out.anomalies.push(Anomaly::Attempt1ClockMismatch);
    }

    let mut attempt_attributes = attributes.clone();
    attempt_attributes.push((
        "run_conclusion".into(),
        run.conclusion.clone().unwrap_or_else(|| "unknown".into()),
    ));
    out.points.push(Point {
        metric: "ci.run.attempts",
        instrument: Instrument::Counter,
        unit: "1",
        value: 1.0,
        attributes: attempt_attributes,
    });

    match jobs.jobs.iter().find(|job| job.name == config.gate_job) {
        Some(gate)
            if gate.conclusion.as_deref() != Some("skipped") && gate.completed_at.is_some() =>
        {
            let completed = gate.completed_at.as_deref().unwrap_or_default();
            match elapsed_seconds(start, completed) {
                Some(seconds) => out.points.push(Point {
                    metric: "ci.run.gate.duration",
                    instrument: Instrument::Histogram,
                    unit: "s",
                    value: seconds,
                    attributes: attributes.clone(),
                }),
                None => out.anomalies.push(Anomaly::UnusableTimestamp),
            }
        }
        // A run cancelled before its gate existed contributes to
        // `ci.run.attempts` and to nothing else, which is why that counter has
        // to exist separately.
        _ => out.anomalies.push(Anomaly::GateMissing),
    }

    let last_working_job = jobs
        .jobs
        .iter()
        .filter(|job| did_work_this_attempt(job, config))
        .filter_map(|job| job.completed_at.as_deref())
        .max();
    if let Some(completed) = last_working_job {
        match elapsed_seconds(start, completed) {
            Some(seconds) => out.points.push(Point {
                metric: "ci.run.checks.duration",
                instrument: Instrument::Histogram,
                unit: "s",
                value: seconds,
                attributes,
            }),
            None => out.anomalies.push(Anomaly::UnusableTimestamp),
        }
    }

    out
}
