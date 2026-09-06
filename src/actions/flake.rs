//! What a rerun revealed.
//!
//! A flake is a property of a transition, so one attempt on its own says
//! nothing: the previous attempt of the same run is what makes it readable.
//! Everything here therefore takes two attempts.

use super::classify::{classify_branch, classify_event};
use super::jobs::{bounded_job_name, is_intentional_failure, is_measurable};
use super::model::{Anomaly, Derivation, Instrument, Job, JobsPayload, Point, RunAttempt};
use crate::config::ActionsConfig;
use std::collections::BTreeMap;

/// Conclusions that count as a failure a rerun could have recovered from.
///
/// `startup_failure` and `stale` are included because a rerun can clear them,
/// and a run that only ever fails to start is flaky in the way that matters
/// most to whoever is waiting on it. Leaving them out filed those recoveries
/// under `other`, where nobody would look.
const FAILED: [&str; 4] = ["failure", "timed_out", "startup_failure", "stale"];

fn is_failure(conclusion: Option<&str>) -> bool {
    conclusion.is_some_and(|c| FAILED.contains(&c))
}

/// Outcomes where a failure exists to attribute to a job.
const ATTRIBUTABLE: [&str; 2] = ["flake", "persistent_failure"];

/// Classify one transition.
///
/// Keyed on the attempt's own run conclusion, never on the gate job's. The
/// gate fails on `cancelled` as well as on `failure`, so reading it would turn
/// every ordinary supersede-cancel into a phantom flake — and cancellation is
/// routine, since a new push cancels the run it replaces.
pub fn classify_transition(previous: Option<&str>, current: Option<&str>) -> &'static str {
    // The current attempt was cut short, so it reports nothing about the code.
    if current == Some("cancelled") {
        return "superseded";
    }
    // The previous attempt never finished, so there is no failure to have
    // recovered from. Counting this as a flake is the single most likely way
    // to overstate the rate, because superseded attempts are common.
    if previous == Some("cancelled") {
        return "after_cancelled";
    }
    if is_failure(previous) {
        if current == Some("success") {
            return "flake";
        }
        if is_failure(current) {
            return "persistent_failure";
        }
    }
    // The same nondeterminism seen from the other side. Nothing changed
    // between the attempts, so a run that passed and then failed is exactly as
    // flaky as one that failed and then passed — and this is the direction
    // where the rerun reveals the problem rather than hiding it. Counting only
    // the first direction gives a one-sided rate.
    if previous == Some("success") && is_failure(current) {
        return "revealed_failure";
    }
    // A rerun of something already green, which people do to refresh an
    // artifact or re-run a deploy. No failure was involved on either side, so
    // there is nothing here to call a flake.
    if previous == Some("success") && current == Some("success") {
        return "rerun_of_green";
    }
    "other"
}

/// Which jobs actually re-executed in this attempt.
///
/// A partial rerun carries most jobs forward, and a carried-forward job is a
/// copy of an earlier result rather than a new observation — crediting one as
/// recovered would credit a rerun for a job it never ran. `is_measurable`
/// already rejects those along with skipped and cancelled ones.
///
/// The gate is excluded because it mirrors the run's own conclusion: leaving
/// it in means every flake also books a gate recovery, and "which job accounts
/// for most flakes" answers "the gate" by construction.
fn re_executed<'a>(jobs: &'a [Job], config: &ActionsConfig) -> Vec<&'a Job> {
    jobs.iter()
        .filter(|job| job.name != config.gate_job && is_measurable(job))
        .collect()
}

pub fn derive(
    current: &RunAttempt,
    current_jobs: &JobsPayload,
    previous: &RunAttempt,
    previous_jobs: &JobsPayload,
    config: &ActionsConfig,
) -> Derivation {
    let mut out = Derivation::default();

    // A guard job that failed on purpose fails its whole run, so a rerun of
    // one looks exactly like a recovery from a real failure.
    if is_intentional_failure(&current_jobs.jobs, config)
        || is_intentional_failure(&previous_jobs.jobs, config)
    {
        out.anomalies.push(Anomaly::IntentionalFailureAttempt);
        return out;
    }
    // An attempt still executing has no conclusion to compare. Both this and
    // the excluded-workflow rule mirror the other two derivers, or the flake
    // rate is computed over a different population from its denominator.
    if current.status != "completed" || previous.status != "completed" {
        out.anomalies.push(Anomaly::RunNotCompleted);
        return out;
    }
    if config.excluded_workflows.contains(&current.path) {
        return out;
    }

    let outcome = classify_transition(
        previous.conclusion.as_deref(),
        current.conclusion.as_deref(),
    );
    let shared = vec![
        (
            "repository".to_string(),
            current.repository.full_name.clone(),
        ),
        ("workflow_path".to_string(), current.path.clone()),
        (
            "event".to_string(),
            classify_event(&current.event).to_string(),
        ),
        ("branch_class".to_string(), classify_branch(current, config)),
    ];

    let mut outcome_attrs = shared.clone();
    outcome_attrs.push(("outcome".into(), outcome.into()));
    out.points.push(Point {
        metric: "ci.run.attempt.outcomes",
        instrument: Instrument::Counter,
        unit: "1",
        value: 1.0,
        attributes: outcome_attrs,
    });

    // Per-job attribution only where the run-level classification found a
    // failure to attribute. Without this the two metrics contradict each other
    // on real data: a run whose previous attempt was cancelled is refused as a
    // flake at run level while the job level books a recovery for it.
    if !ATTRIBUTABLE.contains(&outcome) {
        return out;
    }

    let mut before: BTreeMap<String, Option<String>> = BTreeMap::new();
    for job in &previous_jobs.jobs {
        let (name, _) = bounded_job_name(&job.name, config);
        before.insert(name, job.conclusion.clone());
    }

    for job in re_executed(&current_jobs.jobs, config) {
        let (name, known) = bounded_job_name(&job.name, config);
        if !known {
            out.anomalies.push(Anomaly::UnknownJobName);
        }
        // Attributed only when this job is what failed. A job that passed
        // before, or was skipped, or is absent from the earlier attempt, has
        // no failure to have recovered from.
        let prior = before.get(&name).and_then(Option::as_deref);
        if !is_failure(prior) {
            continue;
        }
        let recovery = if job.conclusion.as_deref() == Some("success") {
            "recovered"
        } else if is_failure(job.conclusion.as_deref()) {
            "still_failing"
        } else {
            continue;
        };
        let mut attrs = shared.clone();
        attrs.push(("job_name".into(), name));
        attrs.push(("recovery".into(), recovery.into()));
        out.points.push(Point {
            metric: "ci.job.rerun_recovered",
            instrument: Instrument::Counter,
            unit: "1",
            value: 1.0,
            attributes: attrs,
        });
    }
    out
}
