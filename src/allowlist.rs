use anyhow::{Context, Result, bail};
use regex::Regex;
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

/// A name family admitted by pattern rather than one entry at a time.
///
/// The pattern is kept alongside the compiled form so an error or a
/// `config-check` can quote what an operator actually wrote.
#[derive(Debug, Clone)]
pub struct Pattern {
    pub source: String,
    regex: Regex,
}

impl Pattern {
    /// Compile one pattern, anchored to the whole name.
    ///
    /// Anchoring is not left to the author. An unanchored `ci\.` admits
    /// `anything.ci.whatever`, and it reviews as if it does not — the failure
    /// is invisible in exactly the file whose job is being reviewable.
    /// Wrapping costs no expressiveness, and a pattern that already carries
    /// its own `^`/`$` keeps working: those are zero-width assertions matching
    /// at the same positions as the ones added here.
    fn compile(source: String) -> Result<Self> {
        let regex = Regex::new(&format!("^(?:{source})$"))
            .with_context(|| format!("compiling allowlist pattern {source}"))?;
        Ok(Self { source, regex })
    }

    fn matches(&self, name: &str) -> bool {
        self.regex.is_match(name)
    }
}

#[derive(Debug, Clone)]
pub struct Allowlist {
    pub metrics: BTreeSet<String>,
    pub metric_patterns: Vec<Pattern>,
    pub attributes: BTreeSet<String>,
    pub attribute_patterns: Vec<Pattern>,
    pub projects: BTreeSet<String>,
    pub project_patterns: Vec<Pattern>,
    /// Attributes whose values are checked as well as their keys. A key absent
    /// here is admitted with whatever value the producer sent.
    pub attribute_values: BTreeMap<String, BTreeSet<String>>,
}

#[derive(Debug, Deserialize)]
struct FileAllowlist {
    #[serde(default)]
    metrics: Vec<String>,
    #[serde(default)]
    metric_patterns: Vec<String>,
    #[serde(default)]
    attributes: Vec<String>,
    #[serde(default)]
    attribute_patterns: Vec<String>,
    #[serde(default)]
    projects: Vec<String>,
    #[serde(default)]
    project_patterns: Vec<String>,
    #[serde(default)]
    attribute_values: BTreeMap<String, Vec<String>>,
}

fn compile_all(sources: Vec<String>) -> Result<Vec<Pattern>> {
    sources.into_iter().map(Pattern::compile).collect()
}

impl Allowlist {
    /// A stable fingerprint of what this allowlist admits.
    ///
    /// Recorded beside an artifact the allowlist emptied, so the decision is
    /// sealed against the rules that made it rather than forever. Widen the
    /// allowlist and the fingerprint changes, which un-seals exactly the
    /// artifacts that were refused for a reason that no longer holds.
    ///
    /// Built from the compiled sets rather than the file's bytes: a comment,
    /// a reordering or a change in indentation does not alter what is
    /// admitted, and re-reading every refused artifact because someone fixed a
    /// typo in a comment is a cost with nothing on the other side.
    pub fn fingerprint(&self) -> String {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        let mut hasher = DefaultHasher::new();
        for name in &self.metrics {
            name.hash(&mut hasher);
        }
        for pattern in &self.metric_patterns {
            pattern.source.hash(&mut hasher);
        }
        for name in &self.attributes {
            name.hash(&mut hasher);
        }
        for pattern in &self.attribute_patterns {
            pattern.source.hash(&mut hasher);
        }
        for name in &self.projects {
            name.hash(&mut hasher);
        }
        for pattern in &self.project_patterns {
            pattern.source.hash(&mut hasher);
        }
        for (key, values) in &self.attribute_values {
            key.hash(&mut hasher);
            for value in values {
                value.hash(&mut hasher);
            }
        }
        format!("{:016x}", hasher.finish())
    }

    pub fn load(path: &Path) -> Result<Self> {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("reading allowlist {}", path.display()))?;
        Self::parse(&raw)
    }

    pub fn parse(raw: &str) -> Result<Self> {
        let file: FileAllowlist = serde_yaml::from_str(raw).context("parsing allowlist")?;
        let attributes: BTreeSet<String> = file.attributes.into_iter().collect();
        let attribute_patterns = compile_all(file.attribute_patterns)?;
        let attribute_values: BTreeMap<String, BTreeSet<String>> = file
            .attribute_values
            .into_iter()
            .map(|(key, values)| (key, values.into_iter().collect()))
            .collect();
        // A bounded value set for a key nobody admits reads as protection that
        // is not there. A pattern counts as admitting it.
        for key in attribute_values.keys() {
            let admitted = attributes.contains(key)
                || attribute_patterns
                    .iter()
                    .any(|pattern| pattern.matches(key));
            if !admitted {
                bail!("attribute_values names {key}, which no attribute or pattern admits");
            }
        }
        let list = Self {
            metrics: file.metrics.into_iter().collect(),
            metric_patterns: compile_all(file.metric_patterns)?,
            attributes,
            attribute_patterns,
            projects: file.projects.into_iter().collect(),
            project_patterns: compile_all(file.project_patterns)?,
            attribute_values,
        };
        if list.metrics.is_empty() && list.metric_patterns.is_empty() {
            bail!("allowlist admits no metrics");
        }
        // Kartero stamps these from the trusted GitHub run and strips whatever
        // a producer sent. A pattern wide enough to cover them would read as
        // permitting something the collector overwrites anyway.
        for reserved in ["cicd.pipeline.run.id", "vcs.ref.head.name"] {
            if list.attribute_patterns.iter().any(|p| p.matches(reserved)) {
                bail!("attribute_patterns matches {reserved}, which Kartero always strips");
            }
        }
        Ok(list)
    }

    pub fn allows_metric(&self, name: &str) -> bool {
        self.metrics.contains(name)
            || self
                .metric_patterns
                .iter()
                .any(|pattern| pattern.matches(name))
    }

    pub fn allows_attribute(&self, key: &str) -> bool {
        self.attributes.contains(key)
            || self
                .attribute_patterns
                .iter()
                .any(|pattern| pattern.matches(key))
    }

    /// Whether a bench may report under this project name.
    ///
    /// Enumerating them drifted: kache grew ten bench variants the list never
    /// learned about, and every point from them was dropped for carrying an
    /// unlisted project — the whole artifact rejected as "allowlist dropped
    /// every metric". A family is the honest unit here, because a project name
    /// is chosen by whoever adds a bench, not derived from a run.
    pub fn allows_project(&self, name: &str) -> bool {
        self.projects.contains(name)
            || self
                .project_patterns
                .iter()
                .any(|pattern| pattern.matches(name))
    }

    /// Whether this key may carry this value.
    ///
    /// Keys are bounded everywhere; values only where a set is declared. That
    /// is the line between an attribute whose vocabulary is fixed in the
    /// producer's code — worth pinning so a change to it has to be reviewed —
    /// and one whose values are legitimately open.
    pub fn allows_attribute_value(&self, key: &str, value: &str) -> bool {
        match self.attribute_values.get(key) {
            Some(allowed) => allowed.contains(value),
            None => true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_metric_is_refused() {
        let list = Allowlist::parse(
            r#"
metrics: [kache.bench.speedup]
attributes: [kache.bench.project]
projects: [bench-firefox]
"#,
        )
        .unwrap();
        assert!(list.allows_metric("kache.bench.speedup"));
        assert!(!list.allows_metric("kache.bench.surprise"));
        assert!(!list.allows_project("typo-firefox"));
    }

    /// The drift this exists to stop: ten bench variants kache added that the
    /// enumeration never learned, each rejecting a whole artifact.
    #[test]
    fn a_project_family_admits_variants_the_list_never_learned() {
        let list = Allowlist::parse(
            r#"
metrics: [kache.bench.speedup]
attributes: [kache.bench.project]
projects: []
project_patterns: ['bench-.+']
"#,
        )
        .unwrap();
        for project in [
            "bench-firefox",
            "bench-substrate-mbx",
            "bench-surrealdb-mbx",
            "bench-hk-pull",
            "bench-eza",
        ] {
            assert!(list.allows_project(project), "{project} should be admitted");
        }
        // Still a family, not everything.
        assert!(!list.allows_project("something-else"));
        assert!(!list.allows_project("bench-"));
    }

    #[test]
    fn declared_attribute_values_bound_the_vocabulary() {
        let list = Allowlist::parse(
            r#"
metrics: [ci.run.attempts]
attributes: [branch_class, job_name]
projects: []
attribute_values:
  branch_class: [trunk_main, other]
"#,
        )
        .unwrap();
        assert!(list.allows_attribute_value("branch_class", "trunk_main"));
        assert!(!list.allows_attribute_value("branch_class", "feat/whatever"));
        // No set declared, so any value passes.
        assert!(list.allows_attribute_value("job_name", "anything"));
    }

    #[test]
    fn patterns_admit_a_family_without_listing_it() {
        let list = Allowlist::parse(
            r#"
metrics: [kache.telemetry.schema_version]
metric_patterns:
  - 'ci\..+'
  - 'kache\.(bench|cache)\..+'
attributes: [branch_class]
attribute_patterns: ['kache\..+']
projects: []
"#,
        )
        .unwrap();
        assert!(list.allows_metric("ci.job.duration"));
        assert!(list.allows_metric("kache.bench.speedup"));
        assert!(list.allows_metric("kache.telemetry.schema_version"));
        assert!(!list.allows_metric("kache.prefetch.plans"));
        assert!(!list.allows_metric("ci"));
        assert!(list.allows_attribute("kache.cache.result"));
        assert!(list.allows_attribute("branch_class"));
        assert!(!list.allows_attribute("job_name"));
    }

    /// An unanchored pattern would admit anything merely containing the
    /// family name, and would look correct while doing it.
    #[test]
    fn patterns_are_anchored_to_the_whole_name() {
        let list = Allowlist::parse(
            r#"
metrics: []
metric_patterns: ['ci\.']
attributes: []
projects: []
"#,
        )
        .unwrap();
        assert!(!list.allows_metric("anything.ci.whatever"));
        assert!(!list.allows_metric("ci.job.duration"));
        assert!(list.allows_metric("ci."));
    }

    /// A pattern an author already anchored must not be broken by anchoring
    /// it again.
    #[test]
    fn an_already_anchored_pattern_still_works() {
        let list = Allowlist::parse(
            r#"
metrics: []
metric_patterns: ['^ci\..+$']
attributes: []
projects: []
"#,
        )
        .unwrap();
        assert!(list.allows_metric("ci.job.duration"));
        assert!(!list.allows_metric("evil.ci.job.duration"));
    }

    #[test]
    fn an_invalid_pattern_is_refused_at_load() {
        let error = Allowlist::parse(
            r#"
metrics: []
metric_patterns: ['ci\.(']
attributes: []
projects: []
"#,
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("compiling allowlist pattern"), "{error}");
    }

    #[test]
    fn an_allowlist_admitting_no_metric_is_refused() {
        let error = Allowlist::parse("metrics: []\nattributes: []\nprojects: []\n")
            .unwrap_err()
            .to_string();
        assert!(error.contains("admits no metrics"), "{error}");
    }

    #[test]
    fn a_pattern_may_not_cover_what_kartero_stamps() {
        let error = Allowlist::parse(
            r#"
metrics: [ci.run.attempts]
attributes: []
attribute_patterns: ['.+']
projects: []
"#,
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("always strips"), "{error}");
    }

    #[test]
    fn a_pattern_can_admit_the_key_a_value_set_bounds() {
        let list = Allowlist::parse(
            r#"
metrics: [ci.run.attempts]
attributes: []
attribute_patterns: ['branch_class']
projects: []
attribute_values:
  branch_class: [trunk_main]
"#,
        )
        .unwrap();
        assert!(list.allows_attribute("branch_class"));
        assert!(!list.allows_attribute_value("branch_class", "other"));
    }

    #[test]
    fn bounding_an_attribute_nobody_admits_is_refused() {
        let error = Allowlist::parse(
            r#"
metrics: [ci.run.attempts]
attributes: [branch_class]
projects: []
attribute_values:
  commit_sha: [abc]
"#,
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("no attribute or pattern admits"), "{error}");
    }
}
