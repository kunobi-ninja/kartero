use crate::config::SourceConfig;
use anyhow::{Context, Result};
use serde::Deserialize;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;

/// GitHub reports a token's expiry on every response that uses it, as
/// `github-authentication-token-expiration`. The header is absent for tokens
/// that never expire, which is a real and separate answer rather than a
/// missing one.
const TOKEN_EXPIRY_HEADER: &str = "github-authentication-token-expiration";

#[derive(Debug, Clone)]
pub struct WorkflowRun {
    /// The payload as the API sent it, for the derivation rules.
    pub detail: crate::actions::RunAttempt,
    /// The attempt's own end, in seconds. The run-level value is the same for
    /// every attempt, so stamping two attempts of one run with it puts both at
    /// the same instant with the same attributes — and a store keyed on that
    /// pair keeps one and drops the other without a word.
    pub observed_at: f64,
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
    config: SourceConfig,
    /// Unix seconds, or 0 when this token has no expiry or none was seen yet.
    token_expires_unix: AtomicI64,
}

/// A source failure that waiting will not fix.
///
/// The retry loop exists for a slow network and a bad minute. Treating a
/// misconfiguration the same way is how a collector runs for a week, stays
/// Ready, and collects nothing.
#[derive(Debug, thiserror::Error)]
pub enum SourceFailure {
    #[error(
        "{owner}/{repo} workflow {workflow} returned 404: the repository or the workflow file does not exist, or this source's token cannot see the repository"
    )]
    NotFound {
        owner: String,
        repo: String,
        workflow: String,
    },
    #[error("{owner}/{repo} returned 401: this source's token has expired or been revoked")]
    Unauthorized { owner: String, repo: String },
    #[error(
        "{owner}/{repo} returned 403: this source's token is refused — an organisation approval or a permission has been withdrawn"
    )]
    Forbidden { owner: String, repo: String },
}

impl SourceFailure {
    /// The `kind` label on `kartero_source_listing_failures_total`.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::NotFound { .. } => "not_found",
            Self::Unauthorized { .. } => "unauthorized",
            Self::Forbidden { .. } => "forbidden",
        }
    }
}

/// Whether an error is one that waiting cannot fix.
pub fn permanent_kind(error: &anyhow::Error) -> Option<&'static str> {
    error
        .downcast_ref::<SourceFailure>()
        .map(SourceFailure::kind)
}

/// A 403 carrying an exhausted quota is a rate limit, not a refusal.
fn is_rate_limited(headers: &reqwest::header::HeaderMap) -> bool {
    headers
        .get("x-ratelimit-remaining")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<i64>().ok())
        .is_some_and(|remaining| remaining <= 0)
        || headers.contains_key("retry-after")
}

/// `2027-09-07 13:14:22 UTC`, the only shape GitHub sends.
///
/// Parsed by hand rather than by pulling in a date library for one header:
/// the format is fixed, and anything that does not match it is treated as no
/// expiry rather than guessed at.
pub fn parse_token_expiry(raw: &str) -> Option<i64> {
    let raw = raw.trim().strip_suffix(" UTC")?;
    let (date, time) = raw.split_once(' ')?;
    let mut date = date.split('-');
    let year: i64 = date.next()?.parse().ok()?;
    let month: i64 = date.next()?.parse().ok()?;
    let day: i64 = date.next()?.parse().ok()?;
    if date.next().is_some() {
        return None;
    }
    let mut time = time.split(':');
    let hour: i64 = time.next()?.parse().ok()?;
    let minute: i64 = time.next()?.parse().ok()?;
    let second: i64 = time.next()?.parse().ok()?;
    if time.next().is_some() || !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    Some(days_from_civil(year, month, day) * 86_400 + hour * 3_600 + minute * 60 + second)
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
    pub fn new(config: SourceConfig) -> Result<Self> {
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
        Ok(Self {
            client,
            config,
            token_expires_unix: AtomicI64::new(0),
        })
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
            let response = self.client.get(url).send().await?;
            self.record_token_expiry(response.headers());
            if let Some(failure) = self.classify(response.status(), response.headers(), workflow) {
                return Err(failure.into());
            }
            let body: RunsResponse = response
                .error_for_status()?
                .json()
                .await
                .with_context(|| format!("listing workflow runs for {workflow}"))?;
            completed.extend(body.workflow_runs.into_iter().map(|run| {
                let head_branch = run.head_branch.clone().unwrap_or_default();
                let observed_at = crate::actions::classify::parse_rfc3339(&run.updated_at)
                    .or_else(|| crate::actions::classify::parse_rfc3339(&run.run_started_at))
                    .unwrap_or_default();
                WorkflowRun {
                    detail: crate::actions::RunAttempt {
                        id: run.id,
                        run_attempt: run.run_attempt,
                        event: run.event.clone(),
                        status: run.status,
                        conclusion: run.conclusion.clone(),
                        created_at: run.created_at,
                        run_started_at: run.run_started_at,
                        path: run.path,
                        head_branch: run.head_branch,
                        repository: crate::actions::model::Repository {
                            full_name: run.repository.full_name,
                        },
                    },
                    observed_at,
                    repo_id: run.repository.id,
                    run_id: run.id,
                    attempt: run.run_attempt,
                    event: run.event,
                    head_branch,
                    workflow_id: run.workflow_id,
                    workflow_name: run.name.unwrap_or_else(|| workflow.clone()),
                    conclusion: run.conclusion,
                }
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

    /// Every job of one attempt, paged.
    ///
    /// `filter=latest` would return only the newest attempt's jobs, which is
    /// wrong for a rerun: the carried-forward jobs are exactly what tells a
    /// partial rerun from a full one.
    pub async fn list_jobs(
        &self,
        run_id: i64,
        attempt: i64,
    ) -> Result<crate::actions::JobsPayload> {
        let mut all = Vec::new();
        for page in 1..=10 {
            let url = format!(
                "https://api.github.com/repos/{}/{}/actions/runs/{run_id}/attempts/{attempt}/jobs?per_page=100&page={page}",
                self.config.owner, self.config.repo
            );
            let response = self.client.get(url).send().await?;
            self.record_token_expiry(response.headers());
            if let Some(failure) = self.classify(response.status(), response.headers(), "jobs") {
                return Err(failure.into());
            }
            let body: crate::actions::JobsPayload = response
                .error_for_status()?
                .json()
                .await
                .with_context(|| format!("listing jobs for run {run_id} attempt {attempt}"))?;
            let count = body.jobs.len();
            all.extend(body.jobs);
            if count < 100 {
                break;
            }
        }
        Ok(crate::actions::JobsPayload { jobs: all })
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

    /// Statuses that mean the configuration is wrong rather than the moment.
    ///
    /// GitHub answers 404 rather than 403 for a private repository a token
    /// cannot see, so as not to leak existence: a missing grant and a missing
    /// workflow file look identical. 401 is an expired or revoked token. 403
    /// is a withdrawn approval or permission — except when it is a secondary
    /// rate limit, which is emphatically temporary and is told apart by the
    /// remaining-quota header. Calling a rate limit permanent would flip a
    /// healthy source to misconfigured during a busy hour.
    fn classify(
        &self,
        status: reqwest::StatusCode,
        headers: &reqwest::header::HeaderMap,
        workflow: &str,
    ) -> Option<SourceFailure> {
        let owner = self.config.owner.clone();
        let repo = self.config.repo.clone();
        match status {
            reqwest::StatusCode::NOT_FOUND => Some(SourceFailure::NotFound {
                owner,
                repo,
                workflow: workflow.to_string(),
            }),
            reqwest::StatusCode::UNAUTHORIZED => Some(SourceFailure::Unauthorized { owner, repo }),
            reqwest::StatusCode::FORBIDDEN if !is_rate_limited(headers) => {
                Some(SourceFailure::Forbidden { owner, repo })
            }
            _ => None,
        }
    }

    fn record_token_expiry(&self, headers: &reqwest::header::HeaderMap) {
        let parsed = headers
            .get(TOKEN_EXPIRY_HEADER)
            .and_then(|value| value.to_str().ok())
            .and_then(parse_token_expiry)
            .unwrap_or(0);
        self.token_expires_unix.store(parsed, Ordering::Relaxed);
    }

    /// Unix seconds at which this source's token expires, if GitHub said so.
    /// `None` means the token does not expire, or has not been used yet.
    pub fn token_expires_unix(&self) -> Option<i64> {
        match self.token_expires_unix.load(Ordering::Relaxed) {
            0 => None,
            seconds => Some(seconds),
        }
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
    head_branch: Option<String>,
    workflow_id: i64,
    name: Option<String>,
    conclusion: Option<String>,
    repository: RepoJson,
    #[serde(default)]
    status: String,
    #[serde(default)]
    created_at: String,
    #[serde(default)]
    run_started_at: String,
    #[serde(default)]
    updated_at: String,
    #[serde(default)]
    path: String,
}

#[derive(Debug, Deserialize)]
struct RepoJson {
    id: i64,
    #[serde(default)]
    full_name: String,
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
    fn permanent_failures_carry_a_kind_and_say_what_to_check() {
        let cases: [(SourceFailure, &str); 3] = [
            (
                SourceFailure::NotFound {
                    owner: "Zondax".into(),
                    repo: "kunobi-frontend".into(),
                    workflow: "ci.yaml".into(),
                },
                "not_found",
            ),
            (
                SourceFailure::Unauthorized {
                    owner: "Zondax".into(),
                    repo: "kunobi-frontend".into(),
                },
                "unauthorized",
            ),
            (
                SourceFailure::Forbidden {
                    owner: "Zondax".into(),
                    repo: "kunobi-frontend".into(),
                },
                "forbidden",
            ),
        ];
        for (failure, kind) in cases {
            let err: anyhow::Error = failure.into();
            assert_eq!(permanent_kind(&err), Some(kind));
            let text = err.to_string();
            assert!(text.contains("Zondax/kunobi-frontend"), "{text}");
            assert!(text.contains("token"), "{text}");
        }
    }

    #[test]
    fn an_ordinary_failure_is_not_permanent() {
        let err = anyhow::anyhow!("connection reset");
        assert!(permanent_kind(&err).is_none());
    }

    /// A 403 from an exhausted quota is temporary. Calling it permanent would
    /// flip a healthy source to misconfigured during a busy hour.
    #[test]
    fn a_rate_limited_403_is_not_a_refusal() {
        let mut limited = reqwest::header::HeaderMap::new();
        limited.insert("x-ratelimit-remaining", "0".parse().unwrap());
        assert!(is_rate_limited(&limited));

        let mut retry = reqwest::header::HeaderMap::new();
        retry.insert("retry-after", "60".parse().unwrap());
        assert!(is_rate_limited(&retry));

        let mut healthy = reqwest::header::HeaderMap::new();
        healthy.insert("x-ratelimit-remaining", "4931".parse().unwrap());
        assert!(!is_rate_limited(&healthy));
        assert!(!is_rate_limited(&reqwest::header::HeaderMap::new()));
    }

    /// The exact header GitHub returned for the kunobi-frontend token.
    #[test]
    fn token_expiry_header_parses_to_unix_seconds() {
        assert_eq!(
            parse_token_expiry("2027-09-07 13:14:22 UTC"),
            Some(1_820_322_862)
        );
        assert_eq!(parse_token_expiry("1970-01-01 00:00:00 UTC"), Some(0));
        assert_eq!(
            parse_token_expiry("2024-02-29 00:00:00 UTC"),
            Some(1709164800)
        );
    }

    /// Anything unrecognised means no expiry rather than a guessed one.
    #[test]
    fn an_unparseable_expiry_is_not_invented() {
        for raw in [
            "",
            "never",
            "2027-09-07 13:14:22",
            "2027-09-07T13:14:22 UTC",
            "2027-13-07 13:14:22 UTC",
            "2027-09-07 13:14 UTC",
        ] {
            assert_eq!(parse_token_expiry(raw), None, "{raw:?} must not parse");
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
