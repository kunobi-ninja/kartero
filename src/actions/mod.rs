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
pub mod model;
pub mod runs;

pub use model::{Anomaly, Derivation, Instrument, Job, JobsPayload, Point, RunAttempt};

use crate::config::ActionsConfig;

/// Derive every metric for one sealed run attempt.
pub fn derive(run: &RunAttempt, jobs: &JobsPayload, config: &ActionsConfig) -> Derivation {
    runs::derive(run, jobs, config)
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

    #[test]
    fn an_incomplete_attempt_derives_nothing_and_says_why() {
        let (mut run, jobs) = fixture("first-attempt-success");
        run.status = "in_progress".into();
        let derived = derive(&run, &jobs, &config());
        assert!(derived.points.is_empty());
        assert_eq!(derived.anomalies, vec![Anomaly::RunNotCompleted]);
    }
}
