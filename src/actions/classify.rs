//! Turning unbounded API values into bounded attributes.
//!
//! Every rule here exists because something in the payload is either
//! unbounded — a branch name, an event type — or does not mean what it
//! appears to. The second kind is documented where it is implemented, and
//! each has a fixture behind it.

use super::model::{Job, RunAttempt};
use crate::config::ActionsConfig;

/// Events worth telling apart. Anything else collapses to `other` rather than
/// opening a series for a webhook nobody is watching.
const EVENTS: [&str; 4] = ["push", "pull_request", "schedule", "workflow_dispatch"];

pub fn classify_event(event: &str) -> &'static str {
    EVENTS
        .into_iter()
        .find(|known| *known == event)
        .unwrap_or("other")
}

/// Branch names are unbounded, so only their class is emitted.
///
/// The classes are configured per source: a repository's trunk is its own
/// business, and hard-coding `dev` would be a kunobi-frontend rule living in
/// a collector that serves everyone.
pub fn classify_branch(run: &RunAttempt, config: &ActionsConfig) -> String {
    if run.event == "pull_request" {
        return "pull_request".into();
    }
    let Some(branch) = run.head_branch.as_deref() else {
        return "other".into();
    };
    config
        .branch_classes
        .get(branch)
        .cloned()
        .unwrap_or_else(|| "other".into())
}

pub fn classify_attempt(run: &RunAttempt) -> &'static str {
    if run.run_attempt == 1 {
        "first"
    } else {
        "rerun"
    }
}

/// A job carried forward from an earlier attempt reports the NEW attempt's
/// `created_at` against the ORIGINAL attempt's `started_at`, so its timestamps
/// are inverted by however long ago that attempt ran.
///
/// `run_attempt` cannot spot them: every job in a rerun attempt reports the
/// new number, carried forward or not. The inversion is the only available
/// discriminator.
pub fn is_carried_forward(job: &Job) -> bool {
    job.started_at
        .as_deref()
        .is_some_and(|started| started < job.created_at.as_str())
}

/// Where an attempt's clock starts.
///
/// On a first attempt this is when the run was created: a run waiting for a
/// concurrency slot has genuinely not started, and that wait is latency a
/// developer feels. On a rerun it is the attempt's own start, because the
/// run's creation belongs to attempt 1 — using it would count the hours a
/// human took to decide to click rerun as CI time.
///
/// A first attempt whose two timestamps disagree is reported but not
/// suppressed: discarding it would drop exactly the slow-to-start runs the
/// metric exists to measure.
pub fn attempt_clock_start(run: &RunAttempt) -> (&str, bool) {
    if run.run_attempt > 1 {
        return (&run.run_started_at, false);
    }
    let mismatched = run.created_at != run.run_started_at;
    (&run.created_at, mismatched)
}

/// Elapsed seconds, or `None` when either timestamp is unusable.
///
/// An unparseable timestamp must not become a number. In the TypeScript this
/// replaced, `Date.parse` answered NaN, NaN failed every comparison including
/// `< 0`, and the value landed in the overflow bucket — recording a broken
/// timestamp as the slowest run ever observed and dragging the percentile up
/// with it. `None` forces the caller to decide, and the only correct decision
/// is to drop the point and say so.
pub fn elapsed_seconds(from: &str, to: &str) -> Option<f64> {
    let elapsed = parse_rfc3339(to)? - parse_rfc3339(from)?;
    if !elapsed.is_finite() || elapsed < 0.0 {
        return None;
    }
    Some(elapsed)
}

/// Seconds since the epoch for the `2026-09-06T12:00:00Z` shape the API sends.
///
/// Parsed by hand rather than by taking a date dependency: the format is
/// fixed, and anything not matching it is unusable rather than guessed at.
pub fn parse_rfc3339(raw: &str) -> Option<f64> {
    let raw = raw.strip_suffix('Z')?;
    let (date, time) = raw.split_once('T')?;
    let mut date = date.split('-');
    let year: i64 = date.next()?.parse().ok()?;
    let month: i64 = date.next()?.parse().ok()?;
    let day: i64 = date.next()?.parse().ok()?;
    if date.next().is_some() || !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let mut time = time.split(':');
    let hour: i64 = time.next()?.parse().ok()?;
    let minute: i64 = time.next()?.parse().ok()?;
    let second: f64 = time.next()?.parse().ok()?;
    if time.next().is_some() || !(0..=23).contains(&hour) || !(0..=59).contains(&minute) {
        return None;
    }
    if !(0.0..61.0).contains(&second) {
        return None;
    }
    Some((days_from_civil(year, month, day) * 86_400 + hour * 3_600 + minute * 60) as f64 + second)
}

/// Howard Hinnant's civil-date algorithm: days since 1970-01-01.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = year - i64::from(month <= 2);
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let day_of_year = (153 * (month + if month > 2 { -3 } else { 9 }) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamps_parse_to_the_same_instant_as_a_date_library() {
        // 2026-09-06T12:00:00Z, cross-checked independently.
        assert_eq!(parse_rfc3339("2026-09-06T12:00:00Z"), Some(1_788_696_000.0));
        assert_eq!(parse_rfc3339("1970-01-01T00:00:00Z"), Some(0.0));
        assert_eq!(parse_rfc3339("2024-02-29T00:00:00Z"), Some(1_709_164_800.0));
    }

    #[test]
    fn an_unusable_timestamp_is_none_rather_than_a_number() {
        for raw in [
            "",
            "not a date",
            "2026-09-06 12:00:00Z",
            "2026-13-06T12:00:00Z",
            "2026-09-06T25:00:00Z",
            "2026-09-06T12:00:00",
        ] {
            assert_eq!(parse_rfc3339(raw), None, "{raw:?} must not parse");
        }
    }

    /// The bug this replaces: NaN failed `< 0` and became the slowest run ever.
    #[test]
    fn a_broken_timestamp_never_becomes_an_elapsed_value() {
        assert_eq!(
            elapsed_seconds("nonsense", "2026-09-06T12:00:00Z"),
            None,
            "an unparseable start must not produce a duration"
        );
        assert_eq!(
            elapsed_seconds("2026-09-06T12:00:00Z", "2026-09-06T11:00:00Z"),
            None,
            "time must not run backwards"
        );
        assert_eq!(
            elapsed_seconds("2026-09-06T12:00:00Z", "2026-09-06T12:01:30Z"),
            Some(90.0)
        );
    }

    #[test]
    fn unknown_events_collapse_rather_than_opening_a_series() {
        assert_eq!(classify_event("push"), "push");
        assert_eq!(classify_event("pull_request"), "pull_request");
        assert_eq!(classify_event("issue_comment"), "other");
    }
}
