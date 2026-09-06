use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct Config {
    pub bind: String,
    pub interval: Duration,
    pub heartbeat_interval: Duration,
    pub sources: Vec<SourceConfig>,
    pub otlp_endpoint: String,
    pub allowlist_path: PathBuf,
    pub ledger_path: PathBuf,
    pub artifact_prefix: String,
    pub archive: Option<ArchiveConfig>,
}

#[derive(Debug, Clone)]
pub struct ArchiveConfig {
    pub artifact_prefix: String,
    pub dir: PathBuf,
    pub max_bytes: usize,
}

/// One repository Kartero reads, with the workflows and branch it trusts
/// there. Each source carries its own token: a fine-grained token is scoped to
/// the repositories it was minted for, so covering two repositories with one
/// token is a choice rather than a requirement.
#[derive(Debug, Clone)]
pub struct SourceConfig {
    pub token: String,
    pub owner: String,
    pub repo: String,
    pub workflows: Vec<String>,
    pub trusted_branch: String,
}

impl SourceConfig {
    /// `owner/repo`, for logs and for telling two sources apart.
    pub fn slug(&self) -> String {
        format!("{}/{}", self.owner, self.repo)
    }
}

#[derive(Debug, Deserialize)]
struct FileConfig {
    #[serde(default = "default_bind")]
    bind: String,
    #[serde(default = "default_interval")]
    interval: String,
    #[serde(default = "default_heartbeat_interval")]
    heartbeat_interval: String,
    /// One source, the shape every deployment used before `sources` existed.
    #[serde(default)]
    github: Option<FileSource>,
    #[serde(default)]
    sources: Vec<FileSource>,
    /// Token for sources that do not carry their own.
    #[serde(default)]
    token: String,
    token_file: Option<PathBuf>,
    otlp: FileOtlp,
    allowlist: PathBuf,
    ledger: PathBuf,
    #[serde(default = "default_prefix")]
    artifact_prefix: String,
    #[serde(default)]
    archive: Option<FileArchive>,
}

#[derive(Debug, Deserialize)]
struct FileArchive {
    #[serde(default)]
    enabled: bool,
    #[serde(default = "default_archive_prefix")]
    artifact_prefix: String,
    #[serde(default)]
    dir: PathBuf,
    #[serde(default = "default_archive_max_bytes")]
    max_bytes: usize,
}

#[derive(Debug, Deserialize)]
struct FileSource {
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
fn default_heartbeat_interval() -> String {
    "1m".into()
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
fn default_archive_prefix() -> String {
    "bench".into()
}
fn default_archive_dir() -> PathBuf {
    PathBuf::from("/var/lib/kartero-archive")
}
fn default_archive_max_bytes() -> usize {
    32 * 1024 * 1024
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
            heartbeat_interval: parse_duration(
                &std::env::var("KARTERO_HEARTBEAT_INTERVAL")
                    .unwrap_or_else(|_| default_heartbeat_interval()),
            )?,
            // The environment describes one source. Several sources need
            // per-source tokens and branches, which a flat namespace cannot
            // express without inventing an index convention; that is what
            // KARTERO_CONFIG is for.
            sources: vec![SourceConfig {
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
            }],
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
            archive: archive_from_env()?,
        })
    }

    pub fn from_file(path: &Path) -> Result<Self> {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("reading config {}", path.display()))?;
        let file: FileConfig = serde_yaml::from_str(&raw)
            .with_context(|| format!("parsing config {}", path.display()))?;
        let fallback = read_token(file.token_file.as_deref(), &file.token)?;
        Ok(Self {
            bind: file.bind,
            interval: parse_duration(&file.interval)?,
            heartbeat_interval: parse_duration(&file.heartbeat_interval)?,
            sources: resolve_sources(file.github, file.sources, &fallback)?,
            otlp_endpoint: file.otlp.endpoint,
            allowlist_path: file.allowlist,
            ledger_path: file.ledger,
            artifact_prefix: file.artifact_prefix,
            archive: archive_from_file(file.archive)?,
        })
    }
}

/// `github:` and `sources:` describe the same thing, so accepting both would
/// mean guessing which one the operator meant.
fn resolve_sources(
    single: Option<FileSource>,
    listed: Vec<FileSource>,
    fallback_token: &str,
) -> Result<Vec<SourceConfig>> {
    let files = match (single, listed.is_empty()) {
        (Some(_), false) => bail!("set either github: or sources:, not both"),
        (Some(one), true) => vec![one],
        (None, false) => listed,
        (None, true) => bail!("at least one source is required under github: or sources:"),
    };

    let mut sources = Vec::with_capacity(files.len());
    for file in files {
        let token = read_token(file.token_file.as_deref(), &file.token)?;
        let token = if token.is_empty() {
            fallback_token.to_string()
        } else {
            token
        };
        let source = SourceConfig {
            token: require_github_token(token)?,
            owner: require_field(file.owner, "owner")?,
            repo: require_field(file.repo, "repo")?,
            workflows: validate_workflows(file.workflows)?,
            trusted_branch: require_field(file.trusted_branch, "trusted_branch")?,
        };
        // Two entries for one repository would list the same runs twice. The
        // ledger would absorb it, but only after paying for every extra call.
        if sources
            .iter()
            .any(|existing: &SourceConfig| existing.slug() == source.slug())
        {
            bail!(
                "source {} is configured twice; list its workflows under one entry",
                source.slug()
            );
        }
        sources.push(source);
    }
    Ok(sources)
}

fn read_token(token_file: Option<&Path>, inline: &str) -> Result<String> {
    let Some(path) = token_file else {
        return Ok(inline.trim().to_string());
    };
    Ok(std::fs::read_to_string(path)
        .with_context(|| format!("reading GitHub token from {}", path.display()))?
        .trim()
        .to_string())
}

fn require_field(value: String, field: &str) -> Result<String> {
    let value = value.trim().to_string();
    if value.is_empty() {
        bail!("source {field} must not be empty");
    }
    Ok(value)
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

fn archive_from_env() -> Result<Option<ArchiveConfig>> {
    if !env_flag("KARTERO_ARCHIVE") {
        return Ok(None);
    }
    Ok(Some(require_archive(ArchiveConfig {
        artifact_prefix: std::env::var("KARTERO_ARCHIVE_ARTIFACT_PREFIX")
            .unwrap_or_else(|_| default_archive_prefix()),
        dir: std::env::var("KARTERO_ARCHIVE_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| default_archive_dir()),
        max_bytes: parse_max_bytes(
            &std::env::var("KARTERO_ARCHIVE_MAX_BYTES")
                .unwrap_or_else(|_| default_archive_max_bytes().to_string()),
        )?,
    })?))
}

fn archive_from_file(file: Option<FileArchive>) -> Result<Option<ArchiveConfig>> {
    let Some(file) = file else {
        return Ok(None);
    };
    if !file.enabled {
        return Ok(None);
    }
    Ok(Some(require_archive(ArchiveConfig {
        artifact_prefix: file.artifact_prefix,
        dir: if file.dir.as_os_str().is_empty() {
            default_archive_dir()
        } else {
            file.dir
        },
        max_bytes: file.max_bytes,
    })?))
}

fn require_archive(config: ArchiveConfig) -> Result<ArchiveConfig> {
    if config.dir.as_os_str().is_empty() {
        bail!("archive.dir / KARTERO_ARCHIVE_DIR is required when archive is enabled");
    }
    if config.artifact_prefix.trim().is_empty() {
        bail!("archive artifact prefix must be non-empty");
    }
    if config.max_bytes == 0 {
        bail!("archive max bytes must be greater than zero");
    }
    Ok(ArchiveConfig {
        artifact_prefix: config.artifact_prefix.trim().to_string(),
        dir: config.dir,
        max_bytes: config.max_bytes,
    })
}

fn env_flag(name: &str) -> bool {
    std::env::var(name)
        .map(|value| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(false)
}

fn parse_max_bytes(spec: &str) -> Result<usize> {
    spec.parse()
        .with_context(|| format!("archive max bytes {spec}"))
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
        assert_eq!(parse_duration("15s").unwrap(), Duration::from_secs(15));
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

    #[test]
    fn archive_requires_a_directory_and_a_prefix() {
        let incomplete = ArchiveConfig {
            artifact_prefix: "bench".into(),
            dir: PathBuf::new(),
            max_bytes: 32,
        };
        assert!(require_archive(incomplete).is_err());
        let ready = require_archive(ArchiveConfig {
            artifact_prefix: " bench ".into(),
            dir: PathBuf::from("/var/lib/kartero-archive"),
            max_bytes: 1024,
        })
        .unwrap();
        assert_eq!(ready.artifact_prefix, "bench");
        assert_eq!(ready.dir, PathBuf::from("/var/lib/kartero-archive"));
    }

    fn write_config(body: &str) -> tempfile::NamedTempFile {
        use std::io::Write;
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(body.as_bytes()).unwrap();
        file.flush().unwrap();
        file
    }

    const TAIL: &str = "
otlp:
  endpoint: http://127.0.0.1:4318
allowlist: /etc/kartero/allowlist.yaml
ledger: /var/lib/kartero/ledger.sqlite
";

    #[test]
    fn a_single_github_block_is_still_one_source() {
        let file = write_config(&format!(
            "github:
  token: t
  owner: kunobi-ninja
  repo: kache
  workflows: [bench.yml]
{TAIL}"
        ));
        let config = Config::from_file(file.path()).unwrap();
        assert_eq!(config.sources.len(), 1);
        assert_eq!(config.sources[0].slug(), "kunobi-ninja/kache");
        assert_eq!(config.sources[0].trusted_branch, "main");
    }

    #[test]
    fn sources_carry_their_own_branch_and_fall_back_to_a_shared_token() {
        let file = write_config(&format!(
            "token: shared
sources:
  - owner: kunobi-ninja
    repo: kache
    workflows: [bench.yml]
  - owner: kunobi-ninja
    repo: kunobi-frontend
    workflows: [ci.yaml]
    trusted_branch: dev
    token: its-own
{TAIL}"
        ));
        let config = Config::from_file(file.path()).unwrap();
        assert_eq!(config.sources.len(), 2);
        assert_eq!(config.sources[0].token, "shared");
        assert_eq!(config.sources[0].trusted_branch, "main");
        assert_eq!(config.sources[1].token, "its-own");
        assert_eq!(config.sources[1].trusted_branch, "dev");
    }

    #[test]
    fn github_and_sources_together_are_ambiguous() {
        let file = write_config(&format!(
            "token: t
github:
  owner: kunobi-ninja
  repo: kache
sources:
  - owner: kunobi-ninja
    repo: kunobi-frontend
{TAIL}"
        ));
        let error = Config::from_file(file.path()).unwrap_err().to_string();
        assert!(error.contains("not both"), "{error}");
    }

    #[test]
    fn one_repository_may_not_be_listed_twice() {
        let file = write_config(&format!(
            "token: t
sources:
  - owner: kunobi-ninja
    repo: kache
    workflows: [bench.yml]
  - owner: kunobi-ninja
    repo: kache
    workflows: [ci.yml]
{TAIL}"
        ));
        let error = Config::from_file(file.path()).unwrap_err().to_string();
        assert!(error.contains("configured twice"), "{error}");
    }

    #[test]
    fn a_source_without_a_token_anywhere_is_refused() {
        let file = write_config(&format!(
            "sources:
  - owner: kunobi-ninja
    repo: kache
{TAIL}"
        ));
        assert!(Config::from_file(file.path()).is_err());
    }

    #[test]
    fn no_source_at_all_is_refused() {
        let file = write_config(&format!("token: t{TAIL}"));
        let error = Config::from_file(file.path()).unwrap_err().to_string();
        assert!(error.contains("at least one source"), "{error}");
    }
}
