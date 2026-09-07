//! Job names read from the repository they belong to.
//!
//! A job's name is a property of the workflow that declares it, so the list of
//! declared names and the aliases that collapse their variants belong in that
//! repository rather than in this collector's deployment config. Keeping a
//! second copy here means a rename lands in one and not the other, and the
//! symptom is a series that quietly stops: `bounded_job_name` collapses the
//! unrecognised name to `other`, which looks exactly like a repository nobody
//! pushed to.
//!
//! The file is the same one the source repository's own checker validates
//! against its workflow, so the check that a name is covered happens in the
//! pull request that renames it.

use anyhow::{Context, Result};
use serde::Deserialize;
use std::collections::BTreeMap;

/// The file the artifact is expected to contain, matching the name the
/// repository commits it under.
pub const FILE: &str = "ci-metrics-job-aliases.json";

#[derive(Debug, Deserialize)]
struct FileJobNames {
    /// Declared names. Retired names stay in here while the listing window
    /// still reaches runs that used them.
    #[serde(default)]
    canonical: Vec<String>,
    /// Variants of one logical job, mapped onto the name they report under.
    #[serde(default)]
    aliases: BTreeMap<String, String>,
}

/// What one repository declares about its own job names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobNames {
    pub canonical: Vec<String>,
    pub aliases: BTreeMap<String, String>,
}

/// Parse the file, refusing anything that would silently label every job
/// `other`.
///
/// An empty `canonical` list is rejected rather than accepted. Accepting it
/// would collapse every job in the repository onto one series and raise an
/// anomaly per job, which is a worse outcome than deriving nothing: the points
/// are delta counters, so a pass that emits them cannot be taken back.
pub fn parse(raw: &str) -> Result<JobNames> {
    let file: FileJobNames = serde_json::from_str(raw)
        .context("parsing the job names file from the source repository")?;
    if file.canonical.is_empty() {
        anyhow::bail!("declares no canonical job names, which would collapse every job to `other`");
    }
    for (from, to) in &file.aliases {
        if !file.canonical.contains(to) {
            anyhow::bail!("aliases {from:?} onto {to:?}, which it does not declare as canonical");
        }
    }
    Ok(JobNames {
        canonical: file.canonical,
        aliases: file.aliases,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const REAL: &str = r#"{
        "_comment": "ignored",
        "canonical": ["CI Gate", "e2e", "rs-checks-linux"],
        "retired": ["rs-checks-linux"],
        "aliases": { "e2e / test": "e2e" }
    }"#;

    #[test]
    fn reads_the_shape_the_repository_actually_commits() {
        let names = parse(REAL).unwrap();
        assert_eq!(names.canonical.len(), 3);
        assert_eq!(
            names.aliases.get("e2e / test").map(String::as_str),
            Some("e2e")
        );
    }

    /// The failure that matters. An empty list parses as valid JSON and would
    /// send every job to `other` on delta counters that cannot be withdrawn.
    #[test]
    fn refuses_a_file_that_declares_nothing() {
        let err = parse(r#"{"canonical": [], "aliases": {}}"#).unwrap_err();
        assert!(
            err.to_string().contains("collapse every job"),
            "unhelpful error: {err}"
        );
    }

    #[test]
    fn refuses_an_alias_pointing_at_a_name_it_does_not_declare() {
        let err =
            parse(r#"{"canonical": ["e2e"], "aliases": {"e2e / test": "e2ee"}}"#).unwrap_err();
        assert!(
            err.to_string().contains("does not declare"),
            "unhelpful error: {err}"
        );
    }

    #[test]
    fn refuses_something_that_is_not_this_file_at_all() {
        assert!(parse("<!doctype html>").is_err());
        assert!(parse("{}").is_err());
    }
}

#[cfg(test)]
mod against_the_real_file {
    use super::*;

    /// The file this parser exists to read, committed verbatim, so a change to
    /// its shape in the repository that owns it fails here rather than at
    /// three in the morning in a pass that derives nothing.
    const COMMITTED: &str = include_str!("../../tests/fixtures/ci-metrics-job-aliases.json");

    #[test]
    fn accepts_the_file_kunobi_frontend_actually_commits() {
        let names = parse(COMMITTED).expect("the real file must parse");
        assert_eq!(names.canonical.len(), 25);
        assert_eq!(names.aliases.len(), 8);
        // The alias that motivated the file: a reusable caller reports its
        // bare id when skipped and a composite when it ran.
        assert_eq!(
            names.aliases.get("e2e / test").map(String::as_str),
            Some("e2e")
        );
        assert!(names.canonical.contains(&"CI Gate".to_string()));
    }

    /// Retired names stay canonical while the listing window still reaches
    /// runs that used them, so the parser must not drop them.
    #[test]
    fn keeps_every_retired_name_resolvable() {
        let raw: serde_json::Value = serde_json::from_str(COMMITTED).unwrap();
        let retired: Vec<String> = raw["retired"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        assert!(
            !retired.is_empty(),
            "the fixture has no retired names to check"
        );
        let names = parse(COMMITTED).unwrap();
        for name in retired {
            assert!(
                names.canonical.contains(&name),
                "{name} is retired but not canonical"
            );
        }
    }
}
