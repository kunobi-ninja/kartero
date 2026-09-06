use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

#[derive(Debug, Clone)]
pub struct Allowlist {
    pub metrics: BTreeSet<String>,
    pub attributes: BTreeSet<String>,
    pub projects: BTreeSet<String>,
    /// Attributes whose values are checked as well as their keys. A key absent
    /// here is admitted with whatever value the producer sent.
    pub attribute_values: BTreeMap<String, BTreeSet<String>>,
}

#[derive(Debug, Deserialize)]
struct FileAllowlist {
    metrics: Vec<String>,
    attributes: Vec<String>,
    projects: Vec<String>,
    #[serde(default)]
    attribute_values: BTreeMap<String, Vec<String>>,
}

impl Allowlist {
    pub fn load(path: &Path) -> Result<Self> {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("reading allowlist {}", path.display()))?;
        Self::parse(&raw)
    }

    pub fn parse(raw: &str) -> Result<Self> {
        let file: FileAllowlist = serde_yaml::from_str(raw).context("parsing allowlist")?;
        let attributes: BTreeSet<String> = file.attributes.into_iter().collect();
        let attribute_values: BTreeMap<String, BTreeSet<String>> = file
            .attribute_values
            .into_iter()
            .map(|(key, values)| (key, values.into_iter().collect()))
            .collect();
        // A bounded value set for a key nobody admits reads as protection that
        // is not there.
        for key in attribute_values.keys() {
            if !attributes.contains(key) {
                bail!("attribute_values names {key}, which is not in attributes");
            }
        }
        Ok(Self {
            metrics: file.metrics.into_iter().collect(),
            attributes,
            projects: file.projects.into_iter().collect(),
            attribute_values,
        })
    }

    pub fn allows_metric(&self, name: &str) -> bool {
        self.metrics.contains(name)
    }

    pub fn allows_attribute(&self, key: &str) -> bool {
        self.attributes.contains(key)
    }

    pub fn allows_project(&self, name: &str) -> bool {
        self.projects.contains(name)
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
        assert!(error.contains("not in attributes"), "{error}");
    }
}
