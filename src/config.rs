use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct Config {
    pub bind: String,
    pub interval: Duration,
    pub github: GitHubConfig,
    pub otlp_endpoint: String,
    pub allowlist_path: PathBuf,
    pub ledger_path: PathBuf,
    pub artifact_prefix: String,
}

#[derive(Debug, Clone)]
pub struct GitHubConfig {
    pub token: String,
    pub owner: String,
    pub repo: String,
    pub workflows: Vec<String>,
    pub trusted_branch: String,
}

#[derive(Debug, Deserialize)]
struct FileConfig {
    #[serde(default = "default_bind")]
    bind: String,
    #[serde(default = "default_interval")]
    interval: String,
    github: FileGitHub,
    otlp: FileOtlp,
    allowlist: PathBuf,
    ledger: PathBuf,
    #[serde(default = "default_prefix")]
    artifact_prefix: String,
}

#[derive(Debug, Deserialize)]
struct FileGitHub {
    #[serde(default)]
    token: String,
    token_file: Option<PathBuf>,
    owner: String,
    repo: String,
    #[serde(default = "default_workflows")]
    workflows: Vec<String>,
    #[serde(default = "default_branch")]
    trusted_branch: String,
}

#[derive(Debug, Deserialize)]
struct FileOtlp {
    endpoint: String,
}

fn default_bind() -> String {
    "0.0.0.0:8080".into()
}
fn default_interval() -> String {
    "1h".into()
}
fn default_prefix() -> String {
    "telemetry-otlp-v1".into()
}
fn default_workflows() -> Vec<String> {
    vec!["bench.yml".into(), "ci.yml".into()]
}
fn default_branch() -> String {
    "main".into()
}

impl Config {
    pub fn from_env() -> Result<Self> {
        if let Ok(path) = std::env::var("KARTERO_CONFIG") {
            return Self::from_file(Path::new(&path));
        }
        let token = match std::env::var("KARTERO_GITHUB_TOKEN_FILE") {
            Ok(path) => std::fs::read_to_string(&path)
                .with_context(|| format!("reading GitHub token from {path}"))?
                .trim()
                .to_string(),
            Err(_) => std::env::var("KARTERO_GITHUB_TOKEN").unwrap_or_default(),
        };
        let token = require_github_token(token)?;
        Ok(Self {
            bind: std::env::var("KARTERO_BIND").unwrap_or_else(|_| default_bind()),
            interval: parse_duration(
                &std::env::var("KARTERO_INTERVAL").unwrap_or_else(|_| default_interval()),
            )?,
            github: GitHubConfig {
                token,
                owner: std::env::var("KARTERO_GITHUB_OWNER")
                    .unwrap_or_else(|_| "kunobi-ninja".into()),
                repo: std::env::var("KARTERO_GITHUB_REPO").unwrap_or_else(|_| "kache".into()),
                workflows: parse_workflows(
                    &std::env::var("KARTERO_GITHUB_WORKFLOWS")
                        .unwrap_or_else(|_| default_workflows().join(",")),
                )?,
                trusted_branch: std::env::var("KARTERO_TRUSTED_BRANCH")
                    .unwrap_or_else(|_| default_branch()),
            },
            otlp_endpoint: std::env::var("KARTERO_OTLP_ENDPOINT").unwrap_or_else(|_| {
                "http://signoz-otel-collector.signoz.svc.cluster.local:4318".into()
            }),
            allowlist_path: PathBuf::from(
                std::env::var("KARTERO_ALLOWLIST")
                    .unwrap_or_else(|_| "/etc/kartero/allowlist.yaml".into()),
            ),
            ledger_path: PathBuf::from(
                std::env::var("KARTERO_LEDGER")
                    .unwrap_or_else(|_| "/var/lib/kartero/ledger.sqlite".into()),
            ),
            artifact_prefix: std::env::var("KARTERO_ARTIFACT_PREFIX")
                .unwrap_or_else(|_| default_prefix()),
        })
    }

    pub fn from_file(path: &Path) -> Result<Self> {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("reading config {}", path.display()))?;
        let file: FileConfig = serde_yaml::from_str(&raw)
            .with_context(|| format!("parsing config {}", path.display()))?;
        let token = if let Some(token_file) = file.github.token_file {
            std::fs::read_to_string(&token_file)
                .with_context(|| format!("reading GitHub token from {}", token_file.display()))?
                .trim()
                .to_string()
        } else {
            file.github.token
        };
        let token = require_github_token(token)?;
        Ok(Self {
            bind: file.bind,
            interval: parse_duration(&file.interval)?,
            github: GitHubConfig {
                token,
                owner: file.github.owner,
                repo: file.github.repo,
                workflows: validate_workflows(file.github.workflows)?,
                trusted_branch: file.github.trusted_branch,
            },
            otlp_endpoint: file.otlp.endpoint,
            allowlist_path: file.allowlist,
            ledger_path: file.ledger,
            artifact_prefix: file.artifact_prefix,
        })
    }
}

fn parse_duration(spec: &str) -> Result<Duration> {
    let duration = if let Some(hours) = spec.strip_suffix('h') {
        let n: u64 = hours.parse().with_context(|| format!("duration {spec}"))?;
        Duration::from_secs(n.saturating_mul(3600))
    } else if let Some(minutes) = spec.strip_suffix('m') {
        let n: u64 = minutes
            .parse()
            .with_context(|| format!("duration {spec}"))?;
        Duration::from_secs(n.saturating_mul(60))
    } else if let Some(seconds) = spec.strip_suffix('s') {
        let n: u64 = seconds
            .parse()
            .with_context(|| format!("duration {spec}"))?;
        Duration::from_secs(n)
    } else {
        bail!("interval {spec} must end in h, m, or s");
    };
    if duration.is_zero() {
        bail!("interval must be greater than zero");
    }
    Ok(duration)
}

fn parse_workflows(spec: &str) -> Result<Vec<String>> {
    validate_workflows(spec.split(',').map(str::trim).map(str::to_string).collect())
}

fn validate_workflows(workflows: Vec<String>) -> Result<Vec<String>> {
    if workflows.is_empty() || workflows.iter().any(String::is_empty) {
        bail!("at least one non-empty GitHub workflow is required");
    }
    Ok(workflows)
}

fn require_github_token(token: String) -> Result<String> {
    let token = token.trim().to_string();
    if token.is_empty() {
        bail!("KARTERO_GITHUB_TOKEN or KARTERO_GITHUB_TOKEN_FILE is required");
    }
    Ok(token)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_hour_interval() {
        assert_eq!(parse_duration("1h").unwrap(), Duration::from_secs(3600));
        assert_eq!(parse_duration("30m").unwrap(), Duration::from_secs(1800));
        assert!(parse_duration("0s").is_err());
    }

    #[test]
    fn parses_workflow_list() {
        assert_eq!(
            parse_workflows("bench.yml, ci.yml").unwrap(),
            ["bench.yml", "ci.yml"]
        );
        assert!(parse_workflows("").is_err());
        assert!(parse_workflows("bench.yml,").is_err());
    }

    #[test]
    fn github_token_is_required() {
        assert!(require_github_token(String::new()).is_err());
        assert!(require_github_token("  ".into()).is_err());
        assert_eq!(require_github_token(" token\n".into()).unwrap(), "token");
    }
}
