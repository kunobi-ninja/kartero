use anyhow::{Context, Result};
use serde::Deserialize;
use std::collections::BTreeSet;
use std::path::Path;

#[derive(Debug, Clone)]
pub struct Allowlist {
    pub metrics: BTreeSet<String>,
    pub attributes: BTreeSet<String>,
    pub projects: BTreeSet<String>,
}

#[derive(Debug, Deserialize)]
struct FileAllowlist {
    metrics: Vec<String>,
    attributes: Vec<String>,
    projects: Vec<String>,
}

impl Allowlist {
    pub fn load(path: &Path) -> Result<Self> {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("reading allowlist {}", path.display()))?;
        Self::parse(&raw)
    }

    pub fn parse(raw: &str) -> Result<Self> {
        let file: FileAllowlist = serde_yaml::from_str(raw).context("parsing allowlist")?;
        Ok(Self {
            metrics: file.metrics.into_iter().collect(),
            attributes: file.attributes.into_iter().collect(),
            projects: file.projects.into_iter().collect(),
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
}
