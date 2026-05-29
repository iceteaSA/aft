use std::collections::BTreeSet;
use std::fmt;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

use crate::cache_freshness::FileFreshness;
use crate::config::Config;
use crate::parser::SharedSymbolCache;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InspectCategory {
    Diagnostics,
    Metrics,
    Todos,
    DeadCode,
    UnusedExports,
    Duplicates,
    Complexity,
    CircularDeps,
    OutdatedDeps,
    Vulnerabilities,
    TestCoverageGaps,
    ApiSurface,
}

impl InspectCategory {
    pub const ACTIVE: [InspectCategory; 6] = [
        InspectCategory::Diagnostics,
        InspectCategory::Metrics,
        InspectCategory::Todos,
        InspectCategory::DeadCode,
        InspectCategory::UnusedExports,
        InspectCategory::Duplicates,
    ];

    pub const DISABLED: [InspectCategory; 6] = [
        InspectCategory::Complexity,
        InspectCategory::CircularDeps,
        InspectCategory::OutdatedDeps,
        InspectCategory::Vulnerabilities,
        InspectCategory::TestCoverageGaps,
        InspectCategory::ApiSurface,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            InspectCategory::Diagnostics => "diagnostics",
            InspectCategory::Metrics => "metrics",
            InspectCategory::Todos => "todos",
            InspectCategory::DeadCode => "dead_code",
            InspectCategory::UnusedExports => "unused_exports",
            InspectCategory::Duplicates => "duplicates",
            InspectCategory::Complexity => "complexity",
            InspectCategory::CircularDeps => "circular_deps",
            InspectCategory::OutdatedDeps => "outdated_deps",
            InspectCategory::Vulnerabilities => "vulnerabilities",
            InspectCategory::TestCoverageGaps => "test_coverage_gaps",
            InspectCategory::ApiSurface => "api_surface",
        }
    }

    pub fn tier(self) -> InspectTier {
        match self {
            InspectCategory::Diagnostics | InspectCategory::Metrics | InspectCategory::Todos => {
                InspectTier::Tier1
            }
            InspectCategory::DeadCode
            | InspectCategory::UnusedExports
            | InspectCategory::Duplicates
            | InspectCategory::Complexity
            | InspectCategory::CircularDeps
            | InspectCategory::ApiSurface => InspectTier::Tier2,
            InspectCategory::OutdatedDeps
            | InspectCategory::Vulnerabilities
            | InspectCategory::TestCoverageGaps => InspectTier::Tier3,
        }
    }

    pub fn is_tier2(self) -> bool {
        self.tier() == InspectTier::Tier2
    }

    pub fn is_active(self) -> bool {
        Self::ACTIVE.contains(&self)
    }

    pub fn active() -> &'static [InspectCategory] {
        &Self::ACTIVE
    }

    pub fn disabled() -> &'static [InspectCategory] {
        &Self::DISABLED
    }
}

impl fmt::Display for InspectCategory {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for InspectCategory {
    type Err = InspectCategoryParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "diagnostics" => Ok(Self::Diagnostics),
            "metrics" => Ok(Self::Metrics),
            "todos" => Ok(Self::Todos),
            "dead_code" => Ok(Self::DeadCode),
            "unused_exports" => Ok(Self::UnusedExports),
            "duplicates" => Ok(Self::Duplicates),
            "complexity" => Ok(Self::Complexity),
            "circular_deps" => Ok(Self::CircularDeps),
            "outdated_deps" => Ok(Self::OutdatedDeps),
            "vulnerabilities" => Ok(Self::Vulnerabilities),
            "test_coverage_gaps" => Ok(Self::TestCoverageGaps),
            "api_surface" => Ok(Self::ApiSurface),
            other => Err(InspectCategoryParseError(other.to_string())),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InspectCategoryParseError(String);

impl fmt::Display for InspectCategoryParseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "unknown inspect category '{}'", self.0)
    }
}

impl std::error::Error for InspectCategoryParseError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InspectTier {
    Tier1,
    Tier2,
    Tier3,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobScope {
    project_root: PathBuf,
    roots: Vec<PathBuf>,
    scope_hash: String,
}

impl JobScope {
    pub fn for_project(project_root: impl Into<PathBuf>) -> Self {
        let project_root = project_root.into();
        Self {
            roots: Vec::new(),
            scope_hash: "project".to_string(),
            project_root,
        }
    }

    pub fn from_roots(project_root: impl Into<PathBuf>, roots: Vec<PathBuf>) -> Self {
        let project_root = project_root.into();
        let mut roots = roots
            .into_iter()
            .map(|root| normalize_path(&root))
            .collect::<Vec<_>>();
        roots.sort();
        roots.dedup();

        if roots.is_empty() || (roots.len() == 1 && normalize_path(&project_root) == roots[0]) {
            return Self::for_project(project_root);
        }

        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        for root in &roots {
            root.to_string_lossy().hash(&mut hasher);
            "\0".hash(&mut hasher);
        }

        Self {
            project_root,
            roots,
            scope_hash: format!("{:016x}", hasher.finish()),
        }
    }

    pub fn project_root(&self) -> &Path {
        &self.project_root
    }

    pub fn roots(&self) -> &[PathBuf] {
        &self.roots
    }

    pub fn scope_hash(&self) -> &str {
        &self.scope_hash
    }

    pub fn is_project_wide(&self) -> bool {
        self.roots.is_empty()
    }

    pub fn contains(&self, path: &Path) -> bool {
        if self.roots.is_empty() {
            return true;
        }
        let normalized = normalize_path(path);
        self.roots.iter().any(|root| normalized.starts_with(root))
    }

    pub fn contains_display_path(&self, value: &str) -> bool {
        let path = PathBuf::from(value);
        if path.is_absolute() {
            self.contains(&path)
        } else {
            self.contains(&self.project_root.join(path))
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct JobKey {
    pub category: InspectCategory,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope_hash: Option<String>,
}

impl JobKey {
    pub fn for_category_scope(category: InspectCategory, scope: &JobScope) -> Self {
        if category.is_tier2() {
            Self::for_project_category(category)
        } else {
            Self {
                category,
                scope_hash: Some(scope.scope_hash().to_string()),
            }
        }
    }

    pub fn for_project_category(category: InspectCategory) -> Self {
        Self {
            category,
            scope_hash: None,
        }
    }

    pub fn display_key(&self) -> String {
        match &self.scope_hash {
            Some(scope_hash) => format!("{}:{scope_hash}", self.category),
            None => self.category.to_string(),
        }
    }
}

#[derive(Clone)]
pub struct InspectSnapshot {
    pub project_root: PathBuf,
    pub inspect_dir: PathBuf,
    pub config: Arc<Config>,
    pub symbol_cache: SharedSymbolCache,
}

impl InspectSnapshot {
    pub fn new(
        project_root: PathBuf,
        inspect_dir: PathBuf,
        config: Arc<Config>,
        symbol_cache: SharedSymbolCache,
    ) -> Self {
        Self {
            project_root,
            inspect_dir,
            config,
            symbol_cache,
        }
    }
}

#[derive(Clone)]
pub struct WorkerCtx {
    pub project_root: PathBuf,
    pub inspect_dir: PathBuf,
    pub config: Arc<Config>,
    pub symbol_cache: SharedSymbolCache,
}

impl From<&InspectSnapshot> for WorkerCtx {
    fn from(snapshot: &InspectSnapshot) -> Self {
        Self {
            project_root: snapshot.project_root.clone(),
            inspect_dir: snapshot.inspect_dir.clone(),
            config: Arc::clone(&snapshot.config),
            symbol_cache: Arc::clone(&snapshot.symbol_cache),
        }
    }
}

#[derive(Clone)]
pub struct InspectJob {
    pub job_id: u64,
    pub key: JobKey,
    pub category: InspectCategory,
    pub scope_files: Vec<PathBuf>,
    pub project_root: PathBuf,
    pub inspect_dir: PathBuf,
    pub config: Arc<Config>,
    pub symbol_cache: SharedSymbolCache,
    pub callgraph_snapshot: Option<Arc<CallgraphSnapshot>>,
}

impl InspectJob {
    pub fn worker_ctx(&self) -> WorkerCtx {
        WorkerCtx {
            project_root: self.project_root.clone(),
            inspect_dir: self.inspect_dir.clone(),
            config: Arc::clone(&self.config),
            symbol_cache: Arc::clone(&self.symbol_cache),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct CallgraphSnapshot {
    pub generated_at: Option<SystemTime>,
    pub files: Vec<PathBuf>,
    pub exported_symbols: Vec<CallgraphExport>,
    pub outbound_calls: Vec<CallgraphOutboundCall>,
    pub entry_points: BTreeSet<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallgraphExport {
    pub file: PathBuf,
    pub symbol: String,
    pub kind: String,
    pub line: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallgraphOutboundCall {
    pub caller_file: PathBuf,
    pub caller_symbol: String,
    pub target: String,
    pub line: u32,
}

#[derive(Debug, Clone)]
pub struct FileContribution {
    pub category: InspectCategory,
    pub file_path: PathBuf,
    pub freshness: FileFreshness,
    pub contribution: serde_json::Value,
}

impl FileContribution {
    pub fn new(
        category: InspectCategory,
        file_path: impl Into<PathBuf>,
        freshness: FileFreshness,
        contribution: serde_json::Value,
    ) -> Self {
        Self {
            category,
            file_path: file_path.into(),
            freshness,
            contribution,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobStatus {
    Queued,
    Running,
    Completed,
    Failed,
}

#[derive(Debug, Clone)]
pub struct InspectScanSuccess {
    pub scanned_files: Vec<PathBuf>,
    pub contributions: Vec<FileContribution>,
    pub aggregate: serde_json::Value,
}

#[derive(Debug, Clone)]
pub struct InspectResult {
    pub job_id: u64,
    pub key: JobKey,
    pub category: InspectCategory,
    pub project_root: PathBuf,
    pub inspect_dir: PathBuf,
    pub outcome: Result<InspectScanSuccess, String>,
    pub duration: Duration,
}

impl InspectResult {
    pub fn success(job: &InspectJob, success: InspectScanSuccess, duration: Duration) -> Self {
        Self {
            job_id: job.job_id,
            key: job.key.clone(),
            category: job.category,
            project_root: job.project_root.clone(),
            inspect_dir: job.inspect_dir.clone(),
            outcome: Ok(success),
            duration,
        }
    }

    pub fn failed(job: &InspectJob, message: impl Into<String>, duration: Duration) -> Self {
        Self {
            job_id: job.job_id,
            key: job.key.clone(),
            category: job.category,
            project_root: job.project_root.clone(),
            inspect_dir: job.inspect_dir.clone(),
            outcome: Err(message.into()),
            duration,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum JobOutcome {
    Fresh {
        payload: serde_json::Value,
    },
    Stale {
        cached: Option<serde_json::Value>,
        in_flight: bool,
    },
    Pending {
        in_flight: bool,
    },
    Failed {
        message: String,
    },
}

impl JobOutcome {
    pub fn payload(&self) -> Option<&serde_json::Value> {
        match self {
            JobOutcome::Fresh { payload } => Some(payload),
            JobOutcome::Stale { cached, .. } => cached.as_ref(),
            JobOutcome::Pending { .. } | JobOutcome::Failed { .. } => None,
        }
    }

    pub fn is_stale(&self) -> bool {
        matches!(self, JobOutcome::Stale { .. })
    }

    pub fn is_pending(&self) -> bool {
        matches!(self, JobOutcome::Pending { .. })
    }

    pub fn summary_status(&self) -> Option<&'static str> {
        match self {
            JobOutcome::Fresh { .. } => None,
            JobOutcome::Stale { .. } => Some("stale"),
            JobOutcome::Pending { .. } => Some("pending"),
            JobOutcome::Failed { .. } => Some("failed"),
        }
    }
}

pub(crate) fn normalize_path(path: &Path) -> PathBuf {
    let mut result = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                if !result.pop() {
                    result.push(component);
                }
            }
            other => result.push(other.as_os_str()),
        }
    }
    result
}
