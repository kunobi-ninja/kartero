//! Per-job metrics for one sealed attempt.
//!
//! Queue time is the wait for a runner, which on a self-hosted pool is where a
//! slowdown hides and which the GitHub interface does not show at all.
//! Duration is the work itself. Conclusions are counted separately from both,
//! because a skipped job has neither a queue nor a duration but is exactly the
//! population that says how much work the path filter avoided.

use super::classify::{classify_attempt, classify_event, elapsed_seconds, is_carried_forward};
use super::model::{Anomaly, Derivation, Instrument, Job, JobsPayload, Point, RunAttempt};
use super::runs::run_attributes;
use crate::config::ActionsConfig;

pub const QUEUE_BOUNDS: [f64; 11] = [
    1.0, 5.0, 15.0, 30.0, 60.0, 120.0, 300.0, 600.0, 1200.0, 2400.0, 3600.0,
];
pub const DURATION_BOUNDS: [f64; 12] = [
    5.0, 15.0, 30.0, 60.0, 120.0, 240.0, 480.0, 900.0, 1500.0, 2400.0, 3600.0, 5400.0,
];

/// Which pool ran the job, from its requested labels.
///
/// Matched case-insensitively because the two vocabularies differ in case:
/// GitHub-hosted labels are lowercase (`ubuntu-latest`) while the self-hosted
/// set is capitalised (`self-hosted`, `macOS`, `ARM64`). A case-sensitive
/// match silently folds the macOS fleet into the wrong bucket.
pub fn classify_runner_pool(labels: &[String]) -> &'static str {
    let lower: Vec<String> = labels.iter().map(|l| l.to_lowercase()).collect();
    let has = |needle: &str| lower.iter().any(|l| l == needle);
    if lower.is_empty() {
        return "none";
    }
    if has("zondax-runners") {
        return "zondax_runners";
    }
    if has("macos") {
        if has("mac-mini") {
            return "macos_mac_mini";
        }
        // The gui label resolves to a single machine, so queueing there is
        // intentional serialisation rather than a capacity problem.
        if has("gui") {
            return "macos_gui";
        }
        return "macos_generic";
    }
    if lower.iter().any(|l| {
        ["ubuntu-", "windows-", "macos-"]
            .iter()
            .any(|prefix| l.starts_with(prefix))
    }) {
        return "github_hosted";
    }
    "unknown"
}

/// The base name of a matrix shard, or `None` when the name is not one.
///
/// Applied only after the full name has failed to match anything declared: a
/// trailing parenthetical is not necessarily a shard. `Build (macOS)` and
/// `Coverage (80%)` are whole job names, and stripping them first turned a
/// declared job into an undeclared one the guard could not see.
pub fn shard_base(name: &str) -> Option<&str> {
    let trimmed = name.trim_end();
    if let Some(open) = trimmed.strip_suffix(')').and_then(|r| r.rfind('(')) {
        let base = trimmed[..open].trim_end();
        if !base.is_empty() {
            return Some(base);
        }
    }
    let (base, shard) = trimmed.rsplit_once('/')?;
    let shard = shard.trim();
    if !shard.is_empty() && shard.chars().all(|c| c.is_ascii_digit()) {
        let base = base.trim_end();
        if !base.is_empty() {
            return Some(base);
        }
    }
    None
}

/// Collapse the name variants of one logical job onto one series.
///
/// A reusable caller reports its bare id when skipped and a composite when it
/// ran, so keying on the raw name puts the skipped half of a job on a
/// different series from the half that ran — which matters most for exactly
/// the jobs a path filter skips. Matrix shards collapse to their base for the
/// same reason: a shard count is a property of the workflow, not something
/// anyone wants multiplied into a cardinality budget.
pub fn bounded_job_name(name: &str, config: &ActionsConfig) -> (String, bool) {
    let canonical = config
        .job_aliases
        .get(name)
        .cloned()
        .unwrap_or_else(|| name.to_string());
    if config.canonical_jobs.contains(&canonical) {
        return (canonical, true);
    }
    if let Some(base) = shard_base(&canonical) {
        let base = config
            .job_aliases
            .get(base)
            .cloned()
            .unwrap_or_else(|| base.to_string());
        if config.canonical_jobs.contains(&base) {
            return (base, true);
        }
    }
    ("other".into(), false)
}

/// Whether this attempt was disabled on purpose.
///
/// The guard fails deliberately when a pull request carries the marker that
/// turns most of CI off. Counting it would put a deliberate signal into the
/// failure rate, and counting the jobs it caused to skip as avoided work would
/// misattribute them to the path filter.
pub fn is_intentional_failure(jobs: &[Job], config: &ActionsConfig) -> bool {
    jobs.iter().any(|job| {
        (job.name == config.guard_job && job.conclusion.as_deref() == Some("failure"))
            || (job.name == config.gate_job
                && job.steps.iter().any(|step| {
                    step.name == config.guard_step && step.conclusion.as_deref() == Some("failure")
                }))
    })
}

/// Why a job did not run.
///
/// `path_filter_code` is deliberately NOT the fallback. Making it the
/// catch-all put phantom counts on every push to a trunk branch, where the
/// comment-posting jobs skip because there is no pull request to comment on
/// while the filter itself fired positive and everything else ran. An
/// unattributable skip is `unknown`: a number someone can go and look at,
/// rather than quiet inflation of the one metric this attribute exists for.
pub fn infer_skip_reason(
    job: &Job,
    jobs: &[Job],
    run: &RunAttempt,
    config: &ActionsConfig,
) -> String {
    if job.conclusion.as_deref() != Some("skipped") {
        return "not_applicable".into();
    }
    // The guard gates on the triggering event, so on any non-pull-request run
    // it skips for that reason alone, whatever else is happening.
    if job.name == config.guard_job {
        return if run.event == "pull_request" {
            "unknown".into()
        } else {
            "event_gated".into()
        };
    }
    let Some(filter) = jobs.iter().find(|c| c.name == config.filter_job) else {
        return "unknown".into();
    };
    match filter.conclusion.as_deref() {
        Some("cancelled") => return "upstream_cancelled".into(),
        Some("failure") => return "upstream_failed".into(),
        _ => {}
    }
    if is_intentional_failure(jobs, config) {
        return "e2e_only_marker".into();
    }
    if job.name == config.docs_job {
        return "path_filter_docs".into();
    }
    if let Some((caller, _)) = job.name.split_once(" / ") {
        let prefix = format!("{caller} / ");
        let siblings: Vec<&Job> = jobs
            .iter()
            .filter(|c| c.name != job.name && c.name.starts_with(&prefix))
            .collect();
        let any = |what: &str| {
            siblings
                .iter()
                .any(|c| c.conclusion.as_deref() == Some(what))
        };
        // Collateral from a sibling that broke, reported as what actually
        // happened rather than folding a cancellation into a failure.
        if any("failure") {
            return "upstream_failed".into();
        }
        if any("cancelled") {
            return "upstream_cancelled".into();
        }
        // A sibling ran, so the caller was not filtered out: this job skipped
        // on a condition of its own inside the reusable workflow.
        if siblings
            .iter()
            .any(|c| c.conclusion.as_deref() != Some("skipped"))
        {
            return "job_condition".into();
        }
    }
    // Work the filter avoided, established positively: the filter ran and
    // concluded, and nothing under this job's caller executed.
    if filter.conclusion.as_deref() == Some("success") {
        return "path_filter_code".into();
    }
    "unknown".into()
}

/// Whether a job that gates its own work on an internal check actually did any.
///
/// Some jobs decide for themselves whether to work, by running a "Check for
/// ... changes" step and skipping everything after it. Their green runs are
/// mostly idle ones, so a duration percentile over the mixed population
/// describes neither mode.
///
/// Detected by the shape of the step list rather than a hardcoded job list, so
/// a newly gated job is handled the day it appears and a renamed one degrades
/// to `not_applicable` rather than to a wrong answer.
///
/// Every step after the gate being skipped is what marks an idle run.
/// Requiring all of them rather than any keeps a job that worked and then
/// failed partway from being reported as idle.
pub fn did_work(job: &Job) -> &'static str {
    let gate = job.steps.iter().position(|step| {
        let lower = step.name.to_lowercase();
        lower.starts_with("check for") && lower.contains("changes")
    });
    let Some(gate) = gate else {
        return "not_applicable";
    };
    let after: Vec<&super::model::JobStep> = job.steps[gate + 1..]
        .iter()
        .filter(|step| {
            let lower = step.name.to_lowercase();
            !(lower.starts_with("post ") || lower == "stop containers" || lower == "complete job")
        })
        .collect();
    if after.is_empty() {
        return "not_applicable";
    }
    if after
        .iter()
        .all(|step| step.conclusion.as_deref() == Some("skipped"))
    {
        "false"
    } else {
        "true"
    }
}

/// A job is measurable when it ran in THIS attempt and reached a runner.
pub fn is_measurable(job: &Job) -> bool {
    job.conclusion.as_deref() != Some("skipped")
        // A cancelled job's duration records when the cancel arrived, not how
        // long the work takes. Concurrency cancellation is routine, so leaving
        // these in mixes a truncation into every percentile — and the duration
        // histogram carries no conclusion attribute, so no dashboard could
        // filter them out afterwards.
        && job.conclusion.as_deref() != Some("cancelled")
        && job.runner_present
        && job.started_at.is_some()
        && job.completed_at.is_some()
        && !is_carried_forward(job)
}

pub fn derive(run: &RunAttempt, jobs: &JobsPayload, config: &ActionsConfig) -> Derivation {
    let mut out = Derivation::default();
    if run.status != "completed" || config.excluded_workflows.contains(&run.path) {
        return out;
    }
    if is_intentional_failure(&jobs.jobs, config) {
        out.anomalies.push(Anomaly::IntentionalFailureAttempt);
        return out;
    }

    // The same dimensions the run-level metrics carry, so a job panel and a
    // run panel can be filtered alike.
    let mut base = run_attributes(run, config);
    base.retain(|(key, _)| key != "attempt_class");
    base.push(("attempt_class".into(), classify_attempt(run).into()));
    base.retain(|(key, _)| key != "event");
    base.push(("event".into(), classify_event(&run.event).into()));

    // Every skip reason keys on the filter job by display name. A rename would
    // degrade them all silently, so its absence is reported.
    if !jobs.jobs.iter().any(|j| j.name == config.filter_job) {
        out.anomalies.push(Anomaly::FilterJobMissing);
    }

    for job in &jobs.jobs {
        let (name, known) = bounded_job_name(&job.name, config);
        if !known {
            out.anomalies.push(Anomaly::UnknownJobName);
        }
        let pool = classify_runner_pool(&job.labels);

        let mut conclusion_attrs = base.clone();
        conclusion_attrs.push(("job_name".into(), name.clone()));
        conclusion_attrs.push(("runner_pool".into(), pool.into()));
        conclusion_attrs.push((
            "conclusion".into(),
            job.conclusion.clone().unwrap_or_else(|| "unknown".into()),
        ));
        conclusion_attrs.push((
            "skip_reason".into(),
            infer_skip_reason(job, &jobs.jobs, run, config),
        ));
        out.points.push(Point {
            metric: "ci.job.conclusions",
            instrument: Instrument::Counter,
            unit: "1",
            value: 1.0,
            attributes: conclusion_attrs,
        });

        if !is_measurable(job) {
            continue;
        }
        let mut measured = base.clone();
        measured.push(("job_name".into(), name));
        measured.push(("runner_pool".into(), pool.into()));

        let started = job.started_at.as_deref().unwrap_or_default();
        let completed = job.completed_at.as_deref().unwrap_or_default();

        match elapsed_seconds(&job.created_at, started) {
            Some(queue) => out.points.push(Point {
                metric: "ci.job.queue.duration",
                instrument: Instrument::Histogram,
                unit: "s",
                value: queue,
                attributes: measured.clone(),
            }),
            None => out.anomalies.push(Anomaly::UnusableTimestamp),
        }

        match elapsed_seconds(started, completed) {
            Some(duration) => {
                let mut attrs = measured;
                attrs.push(("did_work".into(), did_work(job).into()));
                out.points.push(Point {
                    metric: "ci.job.duration",
                    instrument: Instrument::Histogram,
                    unit: "s",
                    value: duration,
                    attributes: attrs,
                });
            }
            None => out.anomalies.push(Anomaly::UnusableTimestamp),
        }
    }
    out
}

/// Which bucket bounds a job-level metric uses.
pub fn bounds_for(metric: &str) -> &'static [f64] {
    if metric == "ci.job.queue.duration" {
        &QUEUE_BOUNDS
    } else {
        &DURATION_BOUNDS
    }
}
