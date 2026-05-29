use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::{Component, Path, PathBuf};
use std::time::{Instant, UNIX_EPOCH};

use rayon::prelude::*;
use serde::Deserialize;
use serde_json::json;

use crate::cache_freshness::{self, FileFreshness};
use crate::inspect::{
    CallgraphOutboundCall, CallgraphSnapshot, FileContribution, InspectCategory, InspectJob,
    InspectResult, InspectScanSuccess,
};

const MAX_DRILL_DOWN_ITEMS: usize = 100;

type ExportNode = (String, String);

pub fn run_dead_code_scan(job: &InspectJob) -> InspectResult {
    let started = Instant::now();

    let Some(snapshot) = job.callgraph_snapshot.as_deref() else {
        let success = InspectScanSuccess {
            scanned_files: job.scope_files.clone(),
            contributions: Vec::new(),
            aggregate: callgraph_unavailable_aggregate(job.scope_files.len()),
        };
        return InspectResult::success(job, success, started.elapsed());
    };

    let liveness_root_files = snapshot
        .entry_points
        .iter()
        .map(|file| relative_path(&job.project_root, file))
        .collect::<BTreeSet<_>>();
    let public_api_files = collect_public_api_files(&job.project_root);
    let (exported_symbols_by_file, files_by_exported_symbol) =
        exported_symbol_indexes(job, snapshot);

    let contributions = job
        .scope_files
        .par_iter()
        .map(|file| {
            gather_file_contribution(
                job,
                snapshot,
                file,
                &exported_symbols_by_file,
                &files_by_exported_symbol,
                &liveness_root_files,
                &public_api_files,
            )
        })
        .collect::<Vec<_>>();

    let aggregate = aggregate_dead_code_contributions(&contributions, &public_api_files);
    let success = InspectScanSuccess {
        scanned_files: job.scope_files.clone(),
        contributions,
        aggregate,
    };

    InspectResult::success(job, success, started.elapsed())
}

fn exported_symbol_indexes(
    job: &InspectJob,
    snapshot: &CallgraphSnapshot,
) -> (
    BTreeMap<String, BTreeSet<String>>,
    BTreeMap<String, BTreeSet<String>>,
) {
    let mut exported_symbols_by_file: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut files_by_exported_symbol: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();

    for export in &snapshot.exported_symbols {
        let file = relative_path(&job.project_root, &export.file);
        exported_symbols_by_file
            .entry(file.clone())
            .or_default()
            .insert(export.symbol.clone());
        files_by_exported_symbol
            .entry(export.symbol.clone())
            .or_default()
            .insert(file);
    }

    (exported_symbols_by_file, files_by_exported_symbol)
}

fn gather_file_contribution(
    job: &InspectJob,
    snapshot: &CallgraphSnapshot,
    file: &Path,
    exported_symbols_by_file: &BTreeMap<String, BTreeSet<String>>,
    files_by_exported_symbol: &BTreeMap<String, BTreeSet<String>>,
    liveness_root_files: &BTreeSet<String>,
    public_api_files: &BTreeSet<String>,
) -> FileContribution {
    let file_name = relative_path(&job.project_root, file);
    let is_liveness_root_file = liveness_root_files.contains(&file_name);
    let is_public_api_file = public_api_files.contains(&file_name);
    let mut exports = snapshot
        .exported_symbols
        .iter()
        .filter(|export| same_file(&job.project_root, &export.file, file))
        .map(|export| ExportContribution {
            symbol: export.symbol.clone(),
            kind: export.kind.clone(),
            line: export.line,
            is_entry_point: false,
        })
        .collect::<Vec<_>>();

    let mut internal_calls = snapshot
        .outbound_calls
        .iter()
        .filter(|call| same_file(&job.project_root, &call.caller_file, file))
        .filter_map(|call| {
            project_internal_call(
                &job.project_root,
                call,
                &file_name,
                exported_symbols_by_file,
                files_by_exported_symbol,
            )
        })
        .collect::<Vec<_>>();
    internal_calls.sort_by(|left, right| {
        left.caller_symbol
            .cmp(&right.caller_symbol)
            .then_with(|| left.file.cmp(&right.file))
            .then_with(|| left.symbol.cmp(&right.symbol))
            .then_with(|| left.line.cmp(&right.line))
    });
    internal_calls.dedup_by(|left, right| {
        left.caller_symbol == right.caller_symbol
            && left.file == right.file
            && left.symbol == right.symbol
            && left.line == right.line
    });

    let liveness_roots = liveness_roots_for_file(
        &file_name,
        &exports,
        &internal_calls,
        is_liveness_root_file,
        is_public_api_file,
    );
    for export in &mut exports {
        export.is_entry_point = liveness_roots.contains(&export.symbol);
    }

    FileContribution::new(
        InspectCategory::DeadCode,
        file.to_path_buf(),
        collect_freshness(file),
        json!({
            "file": file_name,
            "exports": exports
                .iter()
                .map(|export| json!({
                    "symbol": export.symbol,
                    "kind": export.kind,
                    "line": export.line,
                    "is_entry_point": export.is_entry_point,
                }))
                .collect::<Vec<_>>(),
            "internal_calls": internal_calls
                .into_iter()
                .map(|call| json!({
                    "caller_symbol": call.caller_symbol,
                    "file": call.file,
                    "symbol": call.symbol,
                    "line": call.line,
                }))
                .collect::<Vec<_>>(),
            "liveness_roots": liveness_roots,
        }),
    )
}

pub(crate) fn callgraph_unavailable_aggregate(scanned_files: usize) -> serde_json::Value {
    json!({
        "count": 0,
        "items": [],
        "by_language": {},
        "drill_down_capped": false,
        "callgraph_available": false,
        "scanned_files": scanned_files,
        "notes": ["callgraph_unavailable"],
    })
}

pub(crate) fn aggregate_dead_code_contributions(
    contributions: &[FileContribution],
    public_api_files: &BTreeSet<String>,
) -> serde_json::Value {
    aggregate_dead_code_contributions_with_limit(
        contributions,
        public_api_files,
        Some(MAX_DRILL_DOWN_ITEMS),
    )
}

pub(crate) fn aggregate_dead_code_contributions_with_limit(
    contributions: &[FileContribution],
    public_api_files: &BTreeSet<String>,
    drill_down_limit: Option<usize>,
) -> serde_json::Value {
    let parsed = contributions
        .iter()
        .filter_map(|contribution| {
            serde_json::from_value::<DeadCodeContribution>(contribution.contribution.clone()).ok()
        })
        .collect::<Vec<_>>();

    let export_nodes = export_nodes(&parsed);
    let edges_by_source = edges_by_source_export(&parsed, &export_nodes);
    let reachable = reachable_exports(&parsed, &edges_by_source);

    let mut by_language: BTreeMap<String, usize> = BTreeMap::new();
    let mut dead_items = Vec::new();
    for contribution in &parsed {
        let is_public_api_file = public_api_files.contains(&contribution.file);
        for export in &contribution.exports {
            let node = (contribution.file.clone(), export.symbol.clone());
            if reachable.contains(&node) || is_public_api_file {
                continue;
            }

            *by_language
                .entry(language_for_file(&contribution.file).to_string())
                .or_default() += 1;
            dead_items.push(json!({
                "file": contribution.file,
                "symbol": export.symbol,
                "kind": export.kind,
                "line": export.line,
            }));
        }
    }

    let count = dead_items.len();
    let drill_down_capped = drill_down_limit.is_some_and(|limit| count > limit);
    if let Some(limit) = drill_down_limit {
        dead_items.truncate(limit);
    }

    json!({
        "count": count,
        "items": dead_items,
        "by_language": by_language,
        "drill_down_capped": drill_down_capped,
        "callgraph_available": true,
        "scanned_files": contributions.len(),
    })
}

fn export_nodes(contributions: &[DeadCodeContribution]) -> BTreeSet<ExportNode> {
    contributions
        .iter()
        .flat_map(|contribution| {
            contribution
                .exports
                .iter()
                .map(|export| (contribution.file.clone(), export.symbol.clone()))
        })
        .collect()
}

fn edges_by_source_export(
    contributions: &[DeadCodeContribution],
    export_nodes: &BTreeSet<ExportNode>,
) -> BTreeMap<ExportNode, BTreeSet<ExportNode>> {
    let mut edges: BTreeMap<ExportNode, BTreeSet<ExportNode>> = BTreeMap::new();

    for contribution in contributions {
        for call in &contribution.internal_calls {
            let target = (call.file.clone(), call.symbol.clone());
            if !export_nodes.contains(&target) || call.caller_symbol.is_empty() {
                continue;
            }

            let source = (contribution.file.clone(), call.caller_symbol.clone());
            edges.entry(source).or_default().insert(target);
        }
    }

    edges
}

fn reachable_exports(
    contributions: &[DeadCodeContribution],
    edges_by_source: &BTreeMap<ExportNode, BTreeSet<ExportNode>>,
) -> BTreeSet<ExportNode> {
    let mut reachable = BTreeSet::new();
    let mut queue = VecDeque::new();

    for contribution in contributions {
        for root in &contribution.liveness_roots {
            queue.push_back((contribution.file.clone(), root.clone()));
        }
        for export in &contribution.exports {
            if export.is_entry_point {
                queue.push_back((contribution.file.clone(), export.symbol.clone()));
            }
        }
    }

    while let Some(node) = queue.pop_front() {
        if !reachable.insert(node.clone()) {
            continue;
        }
        if let Some(targets) = edges_by_source.get(&node) {
            for target in targets {
                if !reachable.contains(target) {
                    queue.push_back(target.clone());
                }
            }
        }
    }

    reachable
}

fn project_internal_call(
    project_root: &Path,
    call: &CallgraphOutboundCall,
    caller_file: &str,
    exported_symbols_by_file: &BTreeMap<String, BTreeSet<String>>,
    files_by_exported_symbol: &BTreeMap<String, BTreeSet<String>>,
) -> Option<InternalCall> {
    let target = parse_target(project_root, &call.target);
    let symbol = target.symbol?;
    let file = match target.file {
        Some(file) => {
            if exported_symbols_by_file
                .get(&file)
                .is_some_and(|symbols| symbols.contains(&symbol))
            {
                file
            } else {
                return None;
            }
        }
        None => resolve_unqualified_target(
            caller_file,
            &symbol,
            exported_symbols_by_file,
            files_by_exported_symbol,
        )?,
    };

    Some(InternalCall {
        caller_symbol: call.caller_symbol.clone(),
        file,
        symbol,
        line: call.line,
    })
}

fn resolve_unqualified_target(
    caller_file: &str,
    symbol: &str,
    exported_symbols_by_file: &BTreeMap<String, BTreeSet<String>>,
    files_by_exported_symbol: &BTreeMap<String, BTreeSet<String>>,
) -> Option<String> {
    if exported_symbols_by_file
        .get(caller_file)
        .is_some_and(|symbols| symbols.contains(symbol))
    {
        return Some(caller_file.to_string());
    }

    let files = files_by_exported_symbol.get(symbol)?;
    if files.len() == 1 {
        files.iter().next().cloned()
    } else {
        None
    }
}

fn parse_target(project_root: &Path, target: &str) -> ParsedTarget {
    let trimmed = target.trim();
    if trimmed.is_empty() {
        return ParsedTarget {
            file: None,
            symbol: None,
        };
    }

    if let Some((file, symbol)) = trimmed.rsplit_once("::") {
        return ParsedTarget {
            file: Some(relative_path(project_root, Path::new(file))),
            symbol: clean_symbol(symbol),
        };
    }

    if let Some((file, symbol)) = trimmed.rsplit_once('#') {
        return ParsedTarget {
            file: Some(relative_path(project_root, Path::new(file))),
            symbol: clean_symbol(symbol),
        };
    }

    ParsedTarget {
        file: None,
        symbol: clean_symbol(trimmed),
    }
}

fn clean_symbol(symbol: &str) -> Option<String> {
    let trimmed = symbol.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn liveness_roots_for_file(
    file_name: &str,
    exports: &[ExportContribution],
    internal_calls: &[InternalCall],
    is_liveness_root_file: bool,
    is_public_api_file: bool,
) -> Vec<String> {
    if !is_liveness_root_file {
        return Vec::new();
    }

    let mut roots = BTreeSet::new();
    if is_public_api_file {
        roots.extend(exports.iter().map(|export| export.symbol.clone()));
    } else {
        roots.extend(
            exports
                .iter()
                .filter(|export| is_explicit_liveness_symbol(file_name, &export.symbol))
                .map(|export| export.symbol.clone()),
        );
        roots.extend(
            internal_calls
                .iter()
                .map(|call| call.caller_symbol.as_str())
                .filter(|symbol| is_explicit_liveness_symbol(file_name, symbol))
                .map(str::to_string),
        );
    }

    roots.into_iter().collect()
}

fn is_explicit_liveness_symbol(file_name: &str, symbol: &str) -> bool {
    let symbol = symbol.rsplit("::").next().unwrap_or(symbol);
    if symbol == "<top-level>" {
        return true;
    }

    let lower = symbol.to_ascii_lowercase();
    if matches!(
        lower.as_str(),
        "main" | "init" | "setup" | "bootstrap" | "run"
    ) {
        return true;
    }

    Path::new(file_name)
        .file_stem()
        .and_then(|stem| stem.to_str())
        .is_some_and(|stem| stem == symbol)
}

pub(crate) fn collect_public_api_files(project_root: &Path) -> BTreeSet<String> {
    crate::inspect::entry_points::resolve_entry_points(project_root)
        .public_api_files_relative(project_root)
}

fn language_for_file(file: &str) -> &'static str {
    let extension = Path::new(file)
        .extension()
        .and_then(|extension| extension.to_str())
        .map(|extension| extension.to_ascii_lowercase())
        .unwrap_or_default();

    match extension.as_str() {
        "rs" => "rust",
        "ts" | "tsx" | "mts" | "cts" => "typescript",
        "js" | "jsx" | "mjs" | "cjs" => "javascript",
        "py" => "python",
        "go" => "go",
        "c" | "h" => "c",
        "cc" | "cpp" | "cxx" | "hpp" | "hh" => "cpp",
        "zig" => "zig",
        "cs" => "csharp",
        "sh" | "bash" | "zsh" | "fish" => "bash",
        "html" | "htm" => "html",
        "md" | "markdown" => "markdown",
        "sol" => "solidity",
        "vue" => "vue",
        "json" => "json",
        "scala" => "scala",
        "java" => "java",
        "rb" => "ruby",
        "kt" | "kts" => "kotlin",
        "swift" => "swift",
        "php" => "php",
        "lua" => "lua",
        "pl" | "pm" => "perl",
        _ => "unknown",
    }
}

fn collect_freshness(file: &Path) -> FileFreshness {
    cache_freshness::collect(file).unwrap_or_else(|_| FileFreshness {
        mtime: UNIX_EPOCH,
        size: 0,
        content_hash: cache_freshness::zero_hash(),
    })
}

fn same_file(project_root: &Path, left: &Path, right: &Path) -> bool {
    normalize_absolute(project_root, left) == normalize_absolute(project_root, right)
}

fn relative_path(project_root: &Path, path: &Path) -> String {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        project_root.join(path)
    };
    let normalized = normalize_path(&absolute);
    normalized
        .strip_prefix(&normalize_path(project_root))
        .unwrap_or(normalized.as_path())
        .to_string_lossy()
        .replace('\\', "/")
}

fn normalize_absolute(project_root: &Path, path: &Path) -> PathBuf {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        project_root.join(path)
    };
    normalize_path(&absolute)
}

fn normalize_path(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() {
                    normalized.push(component.as_os_str());
                }
            }
            _ => normalized.push(component.as_os_str()),
        }
    }
    normalized
}

#[derive(Debug, Clone, Deserialize)]
struct DeadCodeContribution {
    file: String,
    exports: Vec<ExportContribution>,
    internal_calls: Vec<InternalCallContribution>,
    #[serde(default)]
    liveness_roots: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct ExportContribution {
    symbol: String,
    kind: String,
    line: u32,
    #[serde(default)]
    is_entry_point: bool,
}

#[derive(Debug, Clone, Deserialize)]
struct InternalCallContribution {
    #[serde(default)]
    caller_symbol: String,
    file: String,
    symbol: String,
}

#[derive(Debug, Clone)]
struct InternalCall {
    caller_symbol: String,
    file: String,
    symbol: String,
    line: u32,
}

#[derive(Debug, Clone)]
struct ParsedTarget {
    file: Option<String>,
    symbol: Option<String>,
}
