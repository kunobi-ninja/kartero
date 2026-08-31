use crate::config::GitHubConfig;
use anyhow::{Context, Result};
use serde::Deserialize;
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct WorkflowRun {
    pub repo_id: i64,
    pub run_id: i64,
    pub attempt: i64,
    pub event: String,
    pub head_branch: String,
    #[allow(dead_code)]
    pub workflow_id: i64,
    pub workflow_name: String,
    #[allow(dead_code)]
    pub conclusion: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ArtifactRef {
    pub id: i64,
    pub name: String,
    pub digest: String,
    pub size_in_bytes: u64,
    pub expired: bool,
}

pub struct GitHub {
    client: reqwest::Client,
    config: GitHubConfig,
}

pub fn is_trusted(event: &str, head_branch: &str, trusted_branch: &str) -> bool {
    matches!(event, "schedule" | "workflow_dispatch" | "push") && head_branch == trusted_branch
}

pub fn artifact_name_matches(name: &str, prefix: &str) -> bool {
    name == prefix
        || name
            .strip_prefix(prefix)
            .is_some_and(|rest| rest.starts_with('-'))
}

impl GitHub {
    pub fn new(config: GitHubConfig) -> Result<Self> {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            reqwest::header::USER_AGENT,
            reqwest::header::HeaderValue::from_static("kartero"),
        );
        headers.insert(
            reqwest::header::ACCEPT,
            reqwest::header::HeaderValue::from_static("application/vnd.github+json"),
        );
        if !config.token.is_empty() {
            let mut value =
                reqwest::header::HeaderValue::from_str(&format!("Bearer {}", config.token))?;
            value.set_sensitive(true);
            headers.insert(reqwest::header::AUTHORIZATION, value);
        }
        let client = reqwest::Client::builder()
            .default_headers(headers)
            .redirect(reqwest::redirect::Policy::limited(10))
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(60))
            .build()?;
        Ok(Self { client, config })
    }

    pub fn trusted(&self, run: &WorkflowRun) -> bool {
        is_trusted(&run.event, &run.head_branch, &self.config.trusted_branch)
    }

    pub async fn list_completed_runs(&self) -> Result<Vec<WorkflowRun>> {
        let mut completed = Vec::new();
        for workflow in &self.config.workflows {
            let url = format!(
                "https://api.github.com/repos/{}/{}/actions/workflows/{workflow}/runs?status=completed&per_page=30",
                self.config.owner, self.config.repo
            );
            let body: RunsResponse = self
                .client
                .get(url)
                .send()
                .await?
                .error_for_status()?
                .json()
                .await
                .with_context(|| format!("listing workflow runs for {workflow}"))?;
            completed.extend(body.workflow_runs.into_iter().map(|run| WorkflowRun {
                repo_id: run.repository.id,
                run_id: run.id,
                attempt: run.run_attempt,
                event: run.event,
                head_branch: run.head_branch,
                workflow_id: run.workflow_id,
                workflow_name: run.name.unwrap_or_else(|| workflow.clone()),
                conclusion: run.conclusion,
            }));
        }
        completed.sort_unstable_by_key(|run| std::cmp::Reverse(run.run_id));
        Ok(completed)
    }

    pub async fn list_artifacts(&self, run_id: i64) -> Result<Vec<ArtifactRef>> {
        let url = format!(
            "https://api.github.com/repos/{}/{}/actions/runs/{run_id}/artifacts",
            self.config.owner, self.config.repo
        );
        let body: ArtifactsResponse = self
            .client
            .get(url)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await
            .context("listing artifacts")?;
        Ok(body
            .artifacts
            .into_iter()
            .map(|a| ArtifactRef {
                id: a.id,
                name: a.name,
                digest: a.digest.unwrap_or_default(),
                size_in_bytes: a.size_in_bytes,
                expired: a.expired,
            })
            .collect())
    }

    pub async fn download_zip(&self, artifact_id: i64) -> Result<Vec<u8>> {
        self.download_zip_limited(artifact_id, crate::artifact::MAX_ZIP_BYTES)
            .await
    }

    pub async fn download_zip_limited(
        &self,
        artifact_id: i64,
        max_bytes: usize,
    ) -> Result<Vec<u8>> {
        let url = format!(
            "https://api.github.com/repos/{}/{}/actions/artifacts/{artifact_id}/zip",
            self.config.owner, self.config.repo
        );
        let mut response = self.client.get(url).send().await?.error_for_status()?;
        if response
            .content_length()
            .is_some_and(|size| size > max_bytes as u64)
        {
            anyhow::bail!("artifact response exceeds the zip size limit");
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await? {
            if bytes.len().saturating_add(chunk.len()) > max_bytes {
                anyhow::bail!("artifact response exceeds the zip size limit");
            }
            bytes.extend_from_slice(&chunk);
        }
        Ok(bytes)
    }

    pub fn repository_url(&self) -> String {
        format!(
            "https://github.com/{}/{}",
            self.config.owner, self.config.repo
        )
    }
}

#[derive(Debug, Deserialize)]
struct RunsResponse {
    workflow_runs: Vec<RunJson>,
}

#[derive(Debug, Deserialize)]
struct RunJson {
    id: i64,
    run_attempt: i64,
    event: String,
    head_branch: String,
    workflow_id: i64,
    name: Option<String>,
    conclusion: Option<String>,
    repository: RepoJson,
}

#[derive(Debug, Deserialize)]
struct RepoJson {
    id: i64,
}

#[derive(Debug, Deserialize)]
struct ArtifactsResponse {
    artifacts: Vec<ArtifactJson>,
}

#[derive(Debug, Deserialize)]
struct ArtifactJson {
    id: i64,
    name: String,
    digest: Option<String>,
    size_in_bytes: u64,
    #[serde(default)]
    expired: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trusted_events_require_main() {
        for event in ["schedule", "workflow_dispatch", "push"] {
            assert!(is_trusted(event, "main", "main"));
            assert!(!is_trusted(event, "feat/foo", "main"));
        }
    }

    #[test]
    fn pull_requests_are_never_trusted() {
        assert!(!is_trusted("pull_request", "main", "main"));
    }

    #[test]
    fn artifact_prefix_requires_a_dash_before_the_rest() {
        assert!(artifact_name_matches("bench-firefox", "bench"));
        assert!(artifact_name_matches("bench", "bench"));
        assert!(!artifact_name_matches("benchmark-foo", "bench"));
        assert!(!artifact_name_matches("telemetry-otlp-v1-firefox", "bench"));
    }
}
