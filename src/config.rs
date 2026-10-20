use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct Config {
    pub bind: String,
    pub interval: Duration,
    pub heartbeat_interval: Duration,
    /// How far back a pass lists runs. Wider than `interval` on purpose: a run
    /// still executing when one pass looks has to still be in the listing when
    /// the next one does.
    pub lookback: Duration,
    pub sources: Vec<SourceConfig>,
    pub otlp_endpoint: String,
    /// A URL that answers 2xx only while the store behind the OTLP endpoint
    /// can take writes. Collect checks it before a pass and before every POST,
    /// and delivers nothing while it fails. `None` delivers unchecked.
    ///
    /// A collector acknowledges a POST once it has queued the body, not once
    /// the store has it. When the store is down the collector still answers
    /// 2xx and drops the data later, and the ledger has already recorded the
    /// artifact as delivered, so nothing ever retries it.
    pub otlp_readiness_url: Option<String>,
    /// GitHub's REST API. Always `github::API` outside tests, which point it
    /// at a local server; nothing reads it from the file or the environment.
    pub github_api: String,
    pub allowlist_path: PathBuf,
    pub ledger_path: PathBuf,
    pub artifact_prefix: String,
    pub archive: Option<ArchiveConfig>,
    pub export: Option<ExportConfig>,
}

#[derive(Debug, Clone)]
pub struct ArchiveConfig {
    pub artifact_prefix: String,
    pub dir: PathBuf,
    pub max_bytes: usize,
    /// Days an archived file is kept. `None` keeps everything.
    ///
    /// Bench artifacts run to megabytes each and arrive nightly, so an archive
    /// with no horizon fills its volume and then fails every pass -- and the
    /// pass that fails is the one that would have archived today's run. A
    /// number here is the difference between a bounded cost and an outage with
    /// a delay fuse.
    pub retention_days: Option<u32>,
}

/// Reviewed SigNoz queries whose results are published to object storage.
///
/// The queries live here rather than in a dashboard on purpose: what a public
/// page shows then changes only through a reviewed change to this deployment.
/// A dashboard panel can be edited by anyone with SigNoz access, and an edit
/// there would reach the public bucket on the next pass without anyone
/// reviewing it.
#[derive(Debug, Clone)]
pub struct ExportConfig {
    /// SigNoz query service, the one serving `/api/v5/query_range`. Not the
    /// OTLP collector, which only ingests.
    pub signoz_endpoint: String,
    pub signoz_api_key: Secret,
    pub store: StoreConfig,
    pub jobs: Vec<ExportJob>,
}

/// An S3-compatible bucket. Path-style addressing, which R2 and MinIO both
/// accept and which does not need the bucket name to be a valid hostname.
#[derive(Debug, Clone)]
pub struct StoreConfig {
    pub endpoint: String,
    pub bucket: String,
    pub region: String,
    pub access_key_id: Secret,
    pub secret_access_key: Secret,
}

/// One published document: every query in it runs over the same window and
/// step, so a page can line their series up point for point.
#[derive(Debug, Clone)]
pub struct ExportJob {
    pub name: String,
    /// Object key rewritten on every pass.
    pub key: String,
    /// Object key with a `{date}` placeholder for the UTC date, also written on
    /// every pass, so the last pass of each day stays. It is what keeps a
    /// history once SigNoz's own retention has dropped it.
    pub snapshot_key: Option<String>,
    pub window: Duration,
    pub step: Duration,
    /// Keep the bucket SigNoz has not closed yet. Off by default, because an
    /// open bucket changes value until it closes. A nightly measurement lands
    /// once per day, so its bucket is final as soon as the night's run is in;
    /// leaving it out would publish each night a day late.
    pub include_partial: bool,
    pub cache_control: String,
    pub queries: Vec<ExportQuery>,
}

#[derive(Debug, Clone)]
pub struct ExportQuery {
    /// Key of this query's series in the published document.
    pub id: String,
    /// A SigNoz v5 builder query `spec`, passed through. Kartero sets `name`
    /// and `stepInterval` itself, so the spec may not.
    pub spec: serde_json::Map<String, serde_json::Value>,
}

/// A credential. `Debug` prints whether one is set and nothing else, so a
/// logged `Config` cannot carry it.
#[derive(Clone, Default)]
pub struct Secret(String);

impl Secret {
    pub fn expose(&self) -> &str {
        &self.0
    }

    #[cfg(test)]
    pub(crate) fn for_tests(value: &str) -> Self {
        Self(value.into())
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(if self.0.is_empty() {
            "<unset>"
        } else {
            "<redacted>"
        })
    }
}

/// One repository Kartero reads, with the workflows and branch it trusts
/// there. Each source carries its own token: a fine-grained token is scoped to
/// the repositories it was minted for, so covering two repositories with one
/// token is a choice rather than a requirement.
/// Deriving metrics from a source's workflow runs, rather than importing an
/// artifact it uploaded.
///
/// The vocabulary is configuration rather than code: a repository's trunk
/// branches, the name of its aggregating gate job and the aliases its jobs
/// report under are its own business, and hard-coding one repository's answers
/// into a collector that serves several is how this ended up as a
/// repository-local script in the first place.
#[derive(Debug, Clone)]
pub struct ActionsConfig {
    /// Branch name to the class emitted for it. Anything unlisted is `other`,
    /// so a raw branch name can never reach an attribute.
    pub branch_classes: BTreeMap<String, String>,
    /// The aggregating job a pull request waits on. Its own queue wait is
    /// inside the mergeability clock, because a PR is not mergeable until it
    /// concludes.
    pub gate_job: String,
    /// The job whose deliberate failure means the attempt says something about
    /// the pull request rather than about the pipeline.
    pub guard_job: String,
    /// The step that replaced the guard job once it was folded into the gate.
    pub guard_step: String,
    /// The path-filter job every skip reason keys on. Its absence is reported
    /// rather than absorbed, because a rename degrades every reason silently.
    pub filter_job: String,
    /// The job a docs-only change skips.
    pub docs_job: String,
    /// Declared job names. Anything else collapses to `other` and raises an
    /// anomaly, rather than opening a series nobody approved.
    #[allow(clippy::doc_markdown)]
    pub canonical_jobs: Vec<String>,
    /// Name variants of one logical job, mapped onto a single series. A
    /// reusable caller reports its bare id when skipped and a composite when
    /// it ran, so without this the two halves of one job land apart.
    pub job_aliases: BTreeMap<String, String>,
    /// Workflow *paths* whose conclusions are inverted by design, such as a
    /// nightly flake detector where green means it measured and red means the
    /// detector broke. Excluded whole, not per job.
    pub excluded_workflows: Vec<String>,
    /// Name of the artifact a trusted run uploads carrying the file that
    /// declares this repository's job names and aliases. Set it and
    /// `canonical_jobs`/`job_aliases` come from there instead of from here, so
    /// a rename is covered by the pull request that makes it rather than by a
    /// later change to this deployment.
    ///
    /// An artifact rather than the contents API: reading a file out of a
    /// private repository needs `contents: read`, which is the whole source
    /// tree, and this collector otherwise needs only `actions: read`. A short
    /// list of job names does not justify handing it the code.
    pub job_names_artifact: Option<String>,
}

#[derive(Debug, Clone)]
pub struct SourceConfig {
    pub token: String,
    pub owner: String,
    pub repo: String,
    pub workflows: Vec<String>,
    pub trusted_branch: String,
    /// Absent when this source only has artifacts imported from it.
    pub actions: Option<ActionsConfig>,
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
    #[serde(default = "default_lookback")]
    lookback: String,
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
    #[serde(default)]
    export: Option<FileExport>,
}

/// Unknown keys are refused throughout: a misspelt `include_partial` or
/// `snapshot_key` would otherwise change what is published without a word.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileExport {
    #[serde(default)]
    enabled: bool,
    signoz: FileSignoz,
    store: FileStore,
    #[serde(default)]
    jobs: Vec<FileExportJob>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileSignoz {
    endpoint: String,
    api_key_file: PathBuf,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileStore {
    endpoint: String,
    bucket: String,
    #[serde(default = "default_store_region")]
    region: String,
    access_key_id_file: PathBuf,
    secret_access_key_file: PathBuf,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileExportJob {
    name: String,
    key: String,
    #[serde(default)]
    snapshot_key: Option<String>,
    window: String,
    step: String,
    #[serde(default)]
    include_partial: bool,
    #[serde(default = "default_cache_control")]
    cache_control: String,
    queries: Vec<FileExportQuery>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileExportQuery {
    id: String,
    spec: serde_json::Map<String, serde_json::Value>,
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
    #[serde(default)]
    retention_days: Option<u32>,
}

#[derive(Debug, Deserialize)]
struct FileActions {
    #[serde(default)]
    branch_classes: BTreeMap<String, String>,
    gate_job: String,
    guard_job: String,
    guard_step: String,
    filter_job: String,
    docs_job: String,
    #[serde(default)]
    canonical_jobs: Vec<String>,
    #[serde(default)]
    job_aliases: BTreeMap<String, String>,
    #[serde(default)]
    excluded_workflows: Vec<String>,
    #[serde(default)]
    job_names_artifact: Option<String>,
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
    #[serde(default)]
    actions: Option<FileActions>,
}

/// Unknown keys are refused here, unlike elsewhere in the file: a misspelt
/// `readiness_url` would otherwise switch the check off without a word.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileOtlp {
    endpoint: String,
    #[serde(default)]
    readiness_url: Option<String>,
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
fn default_lookback() -> String {
    "24h".into()
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
fn default_store_region() -> String {
    // R2's region. Any S3 store accepts a region in the signature; R2 accepts
    // only this one.
    "auto".into()
}
fn default_cache_control() -> String {
    "public, max-age=300".into()
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
            lookback: parse_duration(
                &std::env::var("KARTERO_LOOKBACK").unwrap_or_else(|_| default_lookback()),
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
                // Deriving metrics needs a vocabulary — branch classes, a gate
                // job name, job aliases — which has no sensible flat
                // environment form. That is what KARTERO_CONFIG is for.
                actions: None,
            }],
            otlp_endpoint: std::env::var("KARTERO_OTLP_ENDPOINT").unwrap_or_else(|_| {
                "http://signoz-otel-collector.signoz.svc.cluster.local:4318".into()
            }),
            otlp_readiness_url: readiness_url(std::env::var("KARTERO_OTLP_READINESS_URL").ok())?,
            github_api: crate::github::API.into(),
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
            // Queries, a destination and two credentials have no flat
            // environment form either. Export needs KARTERO_CONFIG.
            export: None,
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
            lookback: parse_duration(&file.lookback)?,
            sources: resolve_sources(file.github, file.sources, &fallback)?,
            otlp_endpoint: file.otlp.endpoint,
            otlp_readiness_url: readiness_url(file.otlp.readiness_url)?,
            github_api: crate::github::API.into(),
            allowlist_path: file.allowlist,
            ledger_path: file.ledger,
            artifact_prefix: file.artifact_prefix,
            archive: archive_from_file(file.archive)?,
            export: export_from_file(file.export)?,
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
            actions: file.actions.map(|actions| ActionsConfig {
                branch_classes: actions.branch_classes,
                gate_job: actions.gate_job,
                guard_job: actions.guard_job,
                guard_step: actions.guard_step,
                filter_job: actions.filter_job,
                docs_job: actions.docs_job,
                canonical_jobs: actions.canonical_jobs,
                job_aliases: actions.job_aliases,
                excluded_workflows: actions.excluded_workflows,
                job_names_artifact: actions.job_names_artifact,
            }),
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
    let duration = if let Some(days) = spec.strip_suffix('d') {
        let n: u64 = days.parse().with_context(|| format!("duration {spec}"))?;
        Duration::from_secs(n.saturating_mul(86_400))
    } else if let Some(hours) = spec.strip_suffix('h') {
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
        bail!("interval {spec} must end in d, h, m, or s");
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
        retention_days: match std::env::var("KARTERO_ARCHIVE_RETENTION_DAYS") {
            Ok(raw) => Some(
                raw.trim()
                    .parse()
                    .context("KARTERO_ARCHIVE_RETENTION_DAYS is not a whole number of days")?,
            ),
            Err(_) => None,
        },
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
        retention_days: file.retention_days,
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
    if config.retention_days == Some(0) {
        bail!("archive retention must be at least one day; omit it to keep everything");
    }
    Ok(ArchiveConfig {
        artifact_prefix: config.artifact_prefix.trim().to_string(),
        dir: config.dir,
        max_bytes: config.max_bytes,
        retention_days: config.retention_days,
    })
}

/// SigNoz refuses steps under a minute, and a window this many steps wide is
/// already more points than a page can draw.
const MIN_EXPORT_STEP: Duration = Duration::from_secs(60);
const MAX_EXPORT_POINTS: u64 = 10_000;

fn export_from_file(file: Option<FileExport>) -> Result<Option<ExportConfig>> {
    let Some(file) = file else {
        return Ok(None);
    };
    if !file.enabled {
        return Ok(None);
    }
    let signoz_endpoint = service_url(&file.signoz.endpoint, "export signoz endpoint")?;
    let store_endpoint = service_url(&file.store.endpoint, "export store endpoint")?;
    if file.jobs.is_empty() {
        bail!("export is enabled with no jobs");
    }
    let mut jobs: Vec<ExportJob> = Vec::with_capacity(file.jobs.len());
    for job in file.jobs {
        let job = export_job(job)?;
        if jobs.iter().any(|existing| existing.name == job.name) {
            bail!("export job {} is configured twice", job.name);
        }
        // Two jobs writing one object would each overwrite the other on every
        // pass, and which one a reader saw would depend on timing.
        let keys =
            |job: &ExportJob| std::iter::once(job.key.clone()).chain(job.snapshot_key.clone());
        for key in keys(&job) {
            if jobs.iter().flat_map(keys).any(|existing| existing == key) {
                bail!("export object key {key} is written by two jobs");
            }
        }
        jobs.push(job);
    }
    Ok(Some(ExportConfig {
        signoz_endpoint,
        signoz_api_key: read_secret(&file.signoz.api_key_file, "export signoz api key")?,
        store: StoreConfig {
            endpoint: store_endpoint,
            bucket: require_named(file.store.bucket, "export store bucket")?,
            region: require_named(file.store.region, "export store region")?,
            access_key_id: read_secret(
                &file.store.access_key_id_file,
                "export store access key id",
            )?,
            secret_access_key: read_secret(
                &file.store.secret_access_key_file,
                "export store secret access key",
            )?,
        },
        jobs,
    }))
}

fn export_job(file: FileExportJob) -> Result<ExportJob> {
    let name = file.name.trim().to_string();
    if name.is_empty()
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    {
        bail!("export job name {name:?} must be lowercase letters, digits and dashes");
    }
    let key = object_key(&file.key, &name)?;
    let snapshot_key = match file.snapshot_key {
        Some(raw) => {
            let raw = object_key(&raw, &name)?;
            // Without the placeholder every day would overwrite one object, and
            // the snapshot would be a second copy of `key` rather than a history.
            if !raw.contains("{date}") {
                bail!("export job {name}: snapshot_key must contain {{date}}");
            }
            Some(raw)
        }
        None => None,
    };
    let window = parse_duration(&file.window)?;
    let step = parse_duration(&file.step)?;
    if step < MIN_EXPORT_STEP {
        bail!("export job {name}: step must be at least a minute");
    }
    if window <= step {
        bail!("export job {name}: window must be wider than one step");
    }
    if window.as_secs() / step.as_secs() > MAX_EXPORT_POINTS {
        bail!("export job {name}: window holds more than {MAX_EXPORT_POINTS} steps");
    }
    if file.queries.is_empty() {
        bail!("export job {name} has no queries");
    }
    let mut queries: Vec<ExportQuery> = Vec::with_capacity(file.queries.len());
    for query in file.queries {
        let id = query.id.trim().to_string();
        if id.is_empty()
            || !id
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
        {
            bail!(
                "export job {name}: query id {id:?} must be lowercase letters, digits and underscores"
            );
        }
        if queries.iter().any(|existing| existing.id == id) {
            bail!("export job {name}: query {id} is configured twice");
        }
        for owned in ["name", "stepInterval"] {
            if query.spec.contains_key(owned) {
                bail!("export job {name}: query {id} sets {owned}, which Kartero sets itself");
            }
        }
        // One aggregation per query keeps a series one number per point. Two
        // would need the published document to say which number is which.
        let aggregations = query
            .spec
            .get("aggregations")
            .and_then(serde_json::Value::as_array)
            .map_or(0, Vec::len);
        if aggregations != 1 {
            bail!("export job {name}: query {id} must have exactly one aggregation");
        }
        queries.push(ExportQuery {
            id,
            spec: query.spec,
        });
    }
    Ok(ExportJob {
        name,
        key,
        snapshot_key,
        window,
        step,
        include_partial: file.include_partial,
        cache_control: require_named(file.cache_control, "export cache_control")?,
        queries,
    })
}

/// A relative key without empty or dot segments. A leading slash or a `..`
/// means the same object to some stores and a different one to others.
fn object_key(raw: &str, job: &str) -> Result<String> {
    let key = raw.trim();
    if key.is_empty()
        || key
            .split('/')
            .any(|segment| segment.is_empty() || segment == "." || segment == "..")
    {
        bail!(
            "export job {job}: object key {key:?} must be a relative path without empty or dot segments"
        );
    }
    Ok(key.to_string())
}

/// An http(s) URL without credentials, without a trailing slash. Credentials
/// come from files, so one in the URL is a mistake that would also be logged.
fn service_url(raw: &str, setting: &str) -> Result<String> {
    let raw = raw.trim().trim_end_matches('/');
    let url = reqwest::Url::parse(raw).with_context(|| format!("{setting} is not a URL"))?;
    if !url.username().is_empty() || url.password().is_some() {
        bail!("{setting} must not carry credentials");
    }
    if !matches!(url.scheme(), "http" | "https") {
        bail!("{setting} must use http or https");
    }
    Ok(raw.to_string())
}

fn read_secret(path: &Path, setting: &str) -> Result<Secret> {
    let value = std::fs::read_to_string(path)
        .with_context(|| format!("reading {setting} from {}", path.display()))?
        .trim()
        .to_string();
    if value.is_empty() {
        bail!("{setting} at {} is empty", path.display());
    }
    Ok(Secret(value))
}

fn require_named(value: String, setting: &str) -> Result<String> {
    let value = value.trim().to_string();
    if value.is_empty() {
        bail!("{setting} must not be empty");
    }
    Ok(value)
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

/// Empty means unset, so a chart that renders `""` leaves the check off rather
/// than failing to start. Anything else must be an http(s) URL without
/// credentials, because it is logged on every failed check.
///
/// The errors name the setting and never the value, not even its scheme:
/// without `http://`, `user:password@host` parses with `user` as the scheme.
fn readiness_url(raw: Option<String>) -> Result<Option<String>> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    let raw = raw.trim();
    if raw.is_empty() {
        return Ok(None);
    }
    let url = reqwest::Url::parse(raw).context("otlp readiness URL is not a URL")?;
    if !url.username().is_empty() || url.password().is_some() {
        bail!("otlp readiness URL must not carry credentials; it is logged on every failed check");
    }
    if !matches!(url.scheme(), "http" | "https") {
        bail!("otlp readiness URL must use http or https");
    }
    Ok(Some(raw.to_string()))
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
            retention_days: None,
        };
        assert!(require_archive(incomplete).is_err());
        let ready = require_archive(ArchiveConfig {
            artifact_prefix: " bench ".into(),
            dir: PathBuf::from("/var/lib/kartero-archive"),
            max_bytes: 1024,
            retention_days: None,
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
        assert_eq!(config.otlp_readiness_url, None);
    }

    #[test]
    fn the_readiness_url_comes_from_the_otlp_block() {
        let file = write_config(
            "token: t
github:
  owner: kunobi-ninja
  repo: kache
otlp:
  endpoint: http://127.0.0.1:4318
  readiness_url: http://127.0.0.1:8080/api/v1/health?live=1
allowlist: /etc/kartero/allowlist.yaml
ledger: /var/lib/kartero/ledger.sqlite
",
        );
        let config = Config::from_file(file.path()).unwrap();
        assert_eq!(
            config.otlp_readiness_url.as_deref(),
            Some("http://127.0.0.1:8080/api/v1/health?live=1")
        );
    }

    #[test]
    fn a_misspelt_readiness_url_is_refused_rather_than_ignored() {
        let file = write_config(
            "token: t
github:
  owner: kunobi-ninja
  repo: kache
otlp:
  endpoint: http://127.0.0.1:4318
  readiness_ur: http://127.0.0.1:8080/api/v1/health?live=1
allowlist: /etc/kartero/allowlist.yaml
ledger: /var/lib/kartero/ledger.sqlite
",
        );
        let error = format!("{:#}", Config::from_file(file.path()).unwrap_err());
        assert!(error.contains("readiness_ur"), "{error}");
    }

    #[test]
    fn a_readiness_url_is_optional_and_must_be_plain_http() {
        assert_eq!(readiness_url(None).unwrap(), None);
        assert_eq!(readiness_url(Some("  ".into())).unwrap(), None);
        assert_eq!(
            readiness_url(Some(" https://signoz:8080/api/v1/health?live=1\n".into()))
                .unwrap()
                .as_deref(),
            Some("https://signoz:8080/api/v1/health?live=1")
        );
        assert!(readiness_url(Some("/api/v1/health".into())).is_err());
        assert!(readiness_url(Some("ftp://signoz/health".into())).is_err());
        // However the value is refused, a password in it never reaches the
        // error, which ends up in the pod log.
        for leaky in [
            "http://user:hunter2@signoz/health",
            "ftp://user:hunter2@signoz/health",
            "https//user:hunter2@signoz:8080/health",
            "user:hunter2@signoz:8080/api/v1/health",
            "http://admin:hunter2@signoz:80800/health",
            "hunter2:x-oauth-basic@signoz:8080/api/v1/health",
        ] {
            let error = format!("{:#}", readiness_url(Some(leaky.into())).unwrap_err());
            assert!(!error.contains("hunter2"), "{error}");
        }
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

    /// Secret files for an export block, and the block itself with `jobs`
    /// left for the caller.
    fn export_config(jobs: &str) -> (tempfile::TempDir, tempfile::NamedTempFile) {
        let secrets = tempfile::tempdir().unwrap();
        for name in ["api-key", "access-key-id", "secret-access-key"] {
            std::fs::write(secrets.path().join(name), format!("{name}-value\n")).unwrap();
        }
        let dir = secrets.path().display();
        let file = write_config(&format!(
            "token: t
github:
  owner: kunobi-ninja
  repo: kache
export:
  enabled: true
  signoz:
    endpoint: http://signoz.signoz.svc.cluster.local:8080/
    api_key_file: {dir}/api-key
  store:
    endpoint: https://account.r2.cloudflarestorage.com
    bucket: kunobi-public
    access_key_id_file: {dir}/access-key-id
    secret_access_key_file: {dir}/secret-access-key
  jobs:
{jobs}{TAIL}"
        ));
        (secrets, file)
    }

    const BENCH_JOB: &str = "    - name: kache-bench
      key: kache/bench/v1/latest.json
      snapshot_key: kache/bench/v1/days/{date}.json
      window: 90d
      step: 1d
      include_partial: true
      queries:
        - id: duration
          spec:
            signal: metrics
            aggregations:
              - metricName: kache.bench.build.duration
                timeAggregation: avg
                spaceAggregation: avg
            filter:
              expression: kache.bench.cache_tool = 'kache'
            groupBy:
              - name: kache.bench.project
";

    #[test]
    fn an_export_block_resolves_its_jobs_and_reads_its_secrets() {
        let (_secrets, file) = export_config(BENCH_JOB);
        let config = Config::from_file(file.path()).unwrap();
        let export = config.export.expect("export enabled");
        assert_eq!(
            export.signoz_endpoint,
            "http://signoz.signoz.svc.cluster.local:8080"
        );
        assert_eq!(export.signoz_api_key.expose(), "api-key-value");
        assert_eq!(export.store.region, "auto");
        assert_eq!(
            export.store.secret_access_key.expose(),
            "secret-access-key-value"
        );
        let job = &export.jobs[0];
        assert_eq!(job.window, Duration::from_secs(90 * 86_400));
        assert_eq!(job.step, Duration::from_secs(86_400));
        assert!(job.include_partial);
        assert_eq!(job.cache_control, "public, max-age=300");
        assert_eq!(job.queries[0].id, "duration");
        assert_eq!(
            job.queries[0].spec["filter"]["expression"],
            "kache.bench.cache_tool = 'kache'"
        );
    }

    #[test]
    fn secrets_never_reach_debug_output() {
        let (_secrets, file) = export_config(BENCH_JOB);
        let config = Config::from_file(file.path()).unwrap();
        let printed = format!("{config:?}");
        assert!(!printed.contains("api-key-value"), "{printed}");
        assert!(!printed.contains("secret-access-key-value"), "{printed}");
        assert!(printed.contains("<redacted>"));
    }

    #[test]
    fn an_export_job_that_would_publish_something_unreviewed_is_refused() {
        for (job, expected) in [
            (
                BENCH_JOB.replace("include_partial", "include_partials"),
                "unknown field",
            ),
            (
                BENCH_JOB.replace("            signal: metrics\n", "            signal: metrics\n            stepInterval: 60\n"),
                "sets stepInterval",
            ),
            (
                BENCH_JOB.replace(
                    "                spaceAggregation: avg\n",
                    "                spaceAggregation: avg\n              - metricName: kache.bench.speedup\n",
                ),
                "exactly one aggregation",
            ),
            (
                BENCH_JOB.replace("/days/{date}.json", "/days/latest.json"),
                "must contain {date}",
            ),
            (
                BENCH_JOB.replace("key: kache/bench", "key: ../kache/bench"),
                "dot segments",
            ),
            (BENCH_JOB.replace("step: 1d", "step: 30s"), "at least a minute"),
            (BENCH_JOB.replace("window: 90d", "window: 1d"), "wider than one step"),
            (BENCH_JOB.replace("id: duration", "id: Build-Duration"), "lowercase"),
            (format!("{BENCH_JOB}{BENCH_JOB}"), "configured twice"),
        ] {
            let (_secrets, file) = export_config(&job);
            let error = format!("{:#}", Config::from_file(file.path()).unwrap_err());
            assert!(error.contains(expected), "expected {expected:?} in: {error}");
        }
    }

    #[test]
    fn two_jobs_cannot_write_one_object() {
        let other = BENCH_JOB
            .replace("name: kache-bench", "name: kache-bench-copy")
            .replace("      snapshot_key: kache/bench/v1/days/{date}.json\n", "");
        let (_secrets, file) = export_config(&format!("{BENCH_JOB}{other}"));
        let error = format!("{:#}", Config::from_file(file.path()).unwrap_err());
        assert!(error.contains("written by two jobs"), "{error}");
    }

    #[test]
    fn a_disabled_export_block_is_not_read() {
        let file = write_config(&format!(
            "token: t
github:
  owner: kunobi-ninja
  repo: kache
export:
  enabled: false
  signoz:
    endpoint: http://signoz:8080
    api_key_file: /nonexistent
  store:
    endpoint: https://r2
    bucket: b
    access_key_id_file: /nonexistent
    secret_access_key_file: /nonexistent
{TAIL}"
        ));
        assert!(Config::from_file(file.path()).unwrap().export.is_none());
    }

    #[test]
    fn days_are_a_duration_unit() {
        assert_eq!(
            parse_duration("90d").unwrap(),
            Duration::from_secs(90 * 86_400)
        );
    }
}
