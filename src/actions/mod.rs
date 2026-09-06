//! Deriving CI metrics from the workflow runs Kartero already lists.
//!
//! The artifact path exists for data only CI can produce: a build's coverage,
//! a benchmark's result. Run metadata is not that. It is observable from the
//! Actions API, which this process already reads for every configured source,
//! with a token, on an interval, against a ledger that survives a restart.
//!
//! Deriving it here rather than in each repository removes a workflow, a
//! credential path and a per-repository script, and gives every configured
//! source the same metrics without any of them opting in.
//!
//! Everything below the entry point is a pure function of its arguments — no
//! network, no clock, no filesystem — so the rules are tested against recorded
//! API payloads in `fixtures/actions` rather than against a live API.

pub mod classify;
pub mod flake;
pub mod jobs;
pub mod model;
pub mod runs;

pub use model::{Anomaly, Derivation, Instrument, Job, JobsPayload, Point, RunAttempt};

use crate::config::ActionsConfig;

/// Derive every metric for one sealed run attempt.
pub fn derive(run: &RunAttempt, jobs: &JobsPayload, config: &ActionsConfig) -> Derivation {
    let mut out = runs::derive(run, jobs, config);
    let per_job = jobs::derive(run, jobs, config);
    out.points.extend(per_job.points);
    for anomaly in per_job.anomalies {
        if !out.anomalies.contains(&anomaly) {
            out.anomalies.push(anomaly);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    fn fixture(name: &str) -> (RunAttempt, JobsPayload) {
        let base = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures/actions");
        let run = std::fs::read_to_string(base.join(format!("{name}.run.json")))
            .unwrap_or_else(|_| panic!("missing fixture {name}.run.json"));
        let jobs = std::fs::read_to_string(base.join(format!("{name}.jobs.json")))
            .unwrap_or_else(|_| panic!("missing fixture {name}.jobs.json"));
        (
            serde_json::from_str(&run).expect("run fixture parses"),
            serde_json::from_str(&jobs).expect("jobs fixture parses"),
        )
    }

    fn config() -> ActionsConfig {
        ActionsConfig {
            branch_classes: BTreeMap::from([
                ("dev".into(), "trunk_dev".into()),
                ("main".into(), "trunk_main".into()),
                ("pre".into(), "release_pre".into()),
            ]),
            gate_job: "CI Gate".into(),
            guard_job: "E2E-only filter detected".into(),
            guard_step: "Reject filtered CI as a complete validation".into(),
            filter_job: "changes".into(),
            docs_job: "Docs checks".into(),
            canonical_jobs: Vec::new(),
            job_aliases: BTreeMap::new(),
            excluded_workflows: Vec::new(),
        }
    }

    fn point<'a>(derivation: &'a Derivation, metric: &str) -> Option<&'a Point> {
        derivation.points.iter().find(|p| p.metric == metric)
    }

    /// Every recorded payload must deserialise. The fixtures are real API
    /// responses, so this is the check that the model matches what GitHub
    /// actually sends rather than what the docs describe.
    #[test]
    fn every_fixture_parses() {
        for name in [
            "first-attempt-success",
            "first-attempt-failed",
            "cancelled-then-success-a1",
            "cancelled-then-success-a2",
            "flake-recovered-a1",
            "flake-recovered-a2",
            "rerun-failed-jobs",
            "path-filter-skipped",
            "push-event-gated",
            "supersede-cancelled",
            "three-attempts-a1",
            "three-attempts-a3",
        ] {
            let (run, jobs) = fixture(name);
            assert!(run.id > 0, "{name} has no run id");
            assert!(!jobs.jobs.is_empty(), "{name} has no jobs");
        }
    }

    #[test]
    fn a_successful_first_attempt_yields_all_three_run_series() {
        let (run, jobs) = fixture("first-attempt-success");
        let derived = derive(&run, &jobs, &config());
        let names: Vec<_> = derived.points.iter().map(|p| p.metric).collect();
        assert!(names.contains(&"ci.run.attempts"), "{names:?}");
        assert!(names.contains(&"ci.run.gate.duration"), "{names:?}");
        assert!(names.contains(&"ci.run.checks.duration"), "{names:?}");

        let attempts = point(&derived, "ci.run.attempts").unwrap();
        assert_eq!(attempts.attribute("attempt_class"), Some("first"));
        assert_eq!(attempts.attribute("run_conclusion"), Some("success"));
    }

    /// The gate is the mergeability clock and ends no earlier than the work,
    /// so it cannot be the shorter of the two.
    #[test]
    fn the_gate_clock_is_never_shorter_than_the_work_clock() {
        for name in ["first-attempt-success", "first-attempt-failed"] {
            let (run, jobs) = fixture(name);
            let derived = derive(&run, &jobs, &config());
            let (Some(gate), Some(checks)) = (
                point(&derived, "ci.run.gate.duration"),
                point(&derived, "ci.run.checks.duration"),
            ) else {
                continue;
            };
            assert!(
                gate.value >= checks.value,
                "{name}: gate {} < checks {}",
                gate.value,
                checks.value
            );
        }
    }

    /// A raw branch name must never reach an attribute, whatever the payload
    /// carried.
    #[test]
    fn no_point_carries_an_unbounded_value() {
        let allowed_branch_classes = [
            "pull_request",
            "trunk_dev",
            "trunk_main",
            "release_pre",
            "other",
        ];
        for name in [
            "first-attempt-success",
            "first-attempt-failed",
            "cancelled-then-success-a2",
            "rerun-failed-jobs",
            "push-event-gated",
            "supersede-cancelled",
        ] {
            let (run, jobs) = fixture(name);
            for p in derive(&run, &jobs, &config()).points {
                let class = p.attribute("branch_class").unwrap();
                assert!(allowed_branch_classes.contains(&class), "{name}: {class}");
                for (key, value) in &p.attributes {
                    assert!(!key.contains("sha"), "{name}: {key} looks like identity");
                    assert!(
                        !value.contains(&run.id.to_string()),
                        "{name}: {key} carries the run id"
                    );
                }
            }
        }
    }

    /// A cancelled run usually never reaches a completed gate, which is why
    /// `ci.run.attempts` exists separately from the gate histogram.
    #[test]
    fn a_cancelled_attempt_still_counts_even_without_a_gate() {
        let (run, jobs) = fixture("supersede-cancelled");
        let derived = derive(&run, &jobs, &config());
        assert!(point(&derived, "ci.run.attempts").is_some());
        if point(&derived, "ci.run.gate.duration").is_none() {
            assert!(derived.anomalies.contains(&Anomaly::GateMissing));
        }
    }

    /// A rerun's clock starts at the attempt, not the run: otherwise the hours
    /// a human took to click rerun are counted as CI time.
    #[test]
    fn a_rerun_does_not_bill_the_wait_for_a_human() {
        let (run, jobs) = fixture("three-attempts-a3");
        assert!(run.run_attempt > 1, "fixture must be a rerun");
        let derived = derive(&run, &jobs, &config());
        assert_eq!(
            point(&derived, "ci.run.attempts")
                .unwrap()
                .attribute("attempt_class"),
            Some("rerun")
        );
        if let Some(gate) = point(&derived, "ci.run.gate.duration") {
            let since_creation =
                classify::elapsed_seconds(&run.created_at, &run.run_started_at).unwrap_or(0.0);
            assert!(
                gate.value < since_creation + 1.0 || since_creation == 0.0,
                "rerun gate {} includes the {since_creation}s wait before the attempt",
                gate.value
            );
        }
    }

    /// The pipeline's own names, so the alias and shard rules are exercised
    /// against what the fixtures actually contain.
    fn config_with_names() -> ActionsConfig {
        let mut config = config();
        config.canonical_jobs = vec![
            "changes".into(),
            "CI Gate".into(),
            "Docs checks".into(),
            "e2e".into(),
            "ts-checks-linux".into(),
            "rs-checks-linux".into(),
        ];
        config.job_aliases = BTreeMap::from([("e2e / test".into(), "e2e".into())]);
        config
    }

    #[test]
    fn a_job_that_ran_gets_a_queue_and_a_duration() {
        let (run, jobs) = fixture("first-attempt-success");
        let derived = derive(&run, &jobs, &config_with_names());
        assert!(point(&derived, "ci.job.conclusions").is_some());
        assert!(point(&derived, "ci.job.queue.duration").is_some());
        assert!(point(&derived, "ci.job.duration").is_some());
    }

    /// A skipped job has neither a queue nor a duration, but is exactly the
    /// population that says how much work the filter avoided.
    #[test]
    fn a_skipped_job_is_counted_but_never_timed() {
        let (run, jobs) = fixture("path-filter-skipped");
        let derived = derive(&run, &jobs, &config_with_names());
        let skipped: Vec<&Point> = derived
            .points
            .iter()
            .filter(|p| {
                p.metric == "ci.job.conclusions" && p.attribute("conclusion") == Some("skipped")
            })
            .collect();
        assert!(!skipped.is_empty(), "fixture must contain skipped jobs");
        for p in &skipped {
            assert_ne!(p.attribute("skip_reason"), Some("not_applicable"));
        }
        for p in derived
            .points
            .iter()
            .filter(|p| p.metric.ends_with("duration"))
        {
            assert_ne!(p.attribute("conclusion"), Some("skipped"));
        }
    }

    /// `path_filter_code` must not be the catch-all: it inflates the one
    /// metric the attribute exists to produce.
    #[test]
    fn an_unattributable_skip_is_unknown_rather_than_the_filter() {
        let (run, jobs) = fixture("push-event-gated");
        let derived = derive(&run, &jobs, &config_with_names());
        let reasons: Vec<&str> = derived
            .points
            .iter()
            .filter(|p| p.metric == "ci.job.conclusions")
            .filter_map(|p| p.attribute("skip_reason"))
            .collect();
        assert!(!reasons.is_empty());
        for reason in reasons {
            assert!(
                [
                    "not_applicable",
                    "path_filter_code",
                    "path_filter_docs",
                    "job_condition",
                    "e2e_only_marker",
                    "event_gated",
                    "upstream_failed",
                    "upstream_cancelled",
                    "unknown",
                ]
                .contains(&reason),
                "unbounded skip reason {reason}"
            );
        }
    }

    /// A cancelled job's duration records when the cancel arrived, and the
    /// histogram carries no conclusion attribute to filter it out later.
    #[test]
    fn cancelled_jobs_never_reach_a_duration_histogram() {
        let (_run, jobs) = fixture("supersede-cancelled");
        let cancelled: Vec<&str> = jobs
            .jobs
            .iter()
            .filter(|j| j.conclusion.as_deref() == Some("cancelled"))
            .map(|j| j.name.as_str())
            .collect();
        assert!(!cancelled.is_empty(), "fixture must contain cancelled jobs");
        for job in jobs
            .jobs
            .iter()
            .filter(|j| j.conclusion.as_deref() == Some("cancelled"))
        {
            assert!(!jobs::is_measurable(job), "{} was measured", job.name);
        }
    }

    /// Carried-forward jobs are the only ones whose timestamps invert, and
    /// measuring them would report a negative or nonsense duration.
    #[test]
    fn carried_forward_jobs_are_excluded_from_timing() {
        let (_, jobs) = fixture("rerun-failed-jobs");
        let carried: Vec<&Job> = jobs
            .jobs
            .iter()
            .filter(|j| classify::is_carried_forward(j))
            .collect();
        assert!(
            !carried.is_empty(),
            "fixture must contain carried-forward jobs"
        );
        for job in carried {
            assert!(!jobs::is_measurable(job), "{} was measured", job.name);
        }
    }

    #[test]
    fn runner_pools_are_matched_case_insensitively() {
        assert_eq!(
            jobs::classify_runner_pool(&["zondax-runners".into()]),
            "zondax_runners"
        );
        assert_eq!(
            jobs::classify_runner_pool(&["macOS".into(), "mac-mini".into()]),
            "macos_mac_mini"
        );
        assert_eq!(
            jobs::classify_runner_pool(&["macOS".into(), "GUI".into()]),
            "macos_gui"
        );
        assert_eq!(
            jobs::classify_runner_pool(&["macOS".into()]),
            "macos_generic"
        );
        assert_eq!(
            jobs::classify_runner_pool(&["ubuntu-latest".into()]),
            "github_hosted"
        );
        assert_eq!(jobs::classify_runner_pool(&[]), "none");
        assert_eq!(jobs::classify_runner_pool(&["something".into()]), "unknown");
    }

    /// A trailing parenthetical is not necessarily a shard: stripping it first
    /// turned a declared job into an undeclared one.
    #[test]
    fn shard_bases_are_only_taken_after_the_full_name_fails() {
        assert_eq!(jobs::shard_base("checks (1)"), Some("checks"));
        assert_eq!(jobs::shard_base("checks / 2"), Some("checks"));
        assert_eq!(jobs::shard_base("plain-name"), None);

        let mut config = config();
        config.canonical_jobs = vec!["Build (macOS)".into(), "checks".into()];
        // Declared in full, so it must not be collapsed to `Build`.
        assert_eq!(
            jobs::bounded_job_name("Build (macOS)", &config),
            ("Build (macOS)".into(), true)
        );
        assert_eq!(
            jobs::bounded_job_name("checks (3)", &config),
            ("checks".into(), true)
        );
    }

    #[test]
    fn an_undeclared_job_collapses_and_is_reported() {
        let mut config = config();
        config.canonical_jobs = vec!["changes".into()];
        let (run, jobs) = fixture("first-attempt-success");
        let derived = derive(&run, &jobs, &config);
        assert!(derived.anomalies.contains(&Anomaly::UnknownJobName));
        assert!(
            derived
                .points
                .iter()
                .filter(|p| p.metric == "ci.job.conclusions")
                .any(|p| p.attribute("job_name") == Some("other"))
        );
    }

    /// The gate fails on `cancelled` as well as on `failure`, so reading it
    /// instead of the run conclusion turns every supersede-cancel into a
    /// phantom flake.
    #[test]
    fn a_supersede_cancel_is_never_a_flake() {
        assert_eq!(
            flake::classify_transition(Some("failure"), Some("cancelled")),
            "superseded"
        );
        assert_eq!(
            flake::classify_transition(Some("cancelled"), Some("success")),
            "after_cancelled"
        );
    }

    #[test]
    fn a_rerun_that_went_green_is_a_flake_and_the_reverse_is_revealed() {
        assert_eq!(
            flake::classify_transition(Some("failure"), Some("success")),
            "flake"
        );
        assert_eq!(
            flake::classify_transition(Some("success"), Some("failure")),
            "revealed_failure"
        );
        assert_eq!(
            flake::classify_transition(Some("failure"), Some("failure")),
            "persistent_failure"
        );
        assert_eq!(
            flake::classify_transition(Some("success"), Some("success")),
            "rerun_of_green"
        );
        assert_eq!(flake::classify_transition(None, None), "other");
    }

    /// A rerun can clear these, so a run that only ever fails to start is
    /// flaky in the way that matters most to whoever is waiting.
    #[test]
    fn failing_to_start_counts_as_a_recoverable_failure() {
        for conclusion in ["timed_out", "startup_failure", "stale"] {
            assert_eq!(
                flake::classify_transition(Some(conclusion), Some("success")),
                "flake",
                "{conclusion} should be recoverable"
            );
        }
    }

    /// The recorded pair: attempt 1 failed, attempt 2 went green.
    #[test]
    fn the_recorded_flake_pair_classifies_and_attributes() {
        let (previous, previous_jobs) = fixture("flake-recovered-a1");
        let (current, current_jobs) = fixture("flake-recovered-a2");
        let derived = flake::derive(
            &current,
            &current_jobs,
            &previous,
            &previous_jobs,
            &config_with_names(),
        );
        let outcome = derived
            .points
            .iter()
            .find(|p| p.metric == "ci.run.attempt.outcomes")
            .expect("an outcome point");
        assert_eq!(outcome.attribute("outcome"), Some("flake"));

        // The gate mirrors the run conclusion, so crediting it would make
        // "which job accounts for most flakes" answer "the gate" by
        // construction.
        for p in derived
            .points
            .iter()
            .filter(|p| p.metric == "ci.job.rerun_recovered")
        {
            assert_ne!(p.attribute("job_name"), Some("CI Gate"));
            assert!(matches!(
                p.attribute("recovery"),
                Some("recovered" | "still_failing")
            ));
        }
    }

    /// Run level and job level must agree on the same transition, or a
    /// dashboard shows a recovery for a run that was refused as a flake.
    #[test]
    fn a_cancelled_predecessor_attributes_nothing_to_any_job() {
        let (previous, previous_jobs) = fixture("cancelled-then-success-a1");
        let (current, current_jobs) = fixture("cancelled-then-success-a2");
        let derived = flake::derive(
            &current,
            &current_jobs,
            &previous,
            &previous_jobs,
            &config_with_names(),
        );
        assert_eq!(
            derived
                .points
                .iter()
                .find(|p| p.metric == "ci.run.attempt.outcomes")
                .and_then(|p| p.attribute("outcome")),
            Some("after_cancelled")
        );
        assert!(
            !derived
                .points
                .iter()
                .any(|p| p.metric == "ci.job.rerun_recovered"),
            "no job may be credited when the run level found nothing to attribute"
        );
    }

    #[test]
    fn an_incomplete_attempt_derives_nothing_and_says_why() {
        let (mut run, jobs) = fixture("first-attempt-success");
        run.status = "in_progress".into();
        let derived = derive(&run, &jobs, &config());
        assert!(derived.points.is_empty());
        assert_eq!(derived.anomalies, vec![Anomaly::RunNotCompleted]);
    }
}
