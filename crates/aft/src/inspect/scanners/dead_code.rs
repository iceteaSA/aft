use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::time::{Instant, UNIX_EPOCH};

use rayon::prelude::*;
use serde::Deserialize;
use serde_json::json;

use crate::cache_freshness::{self, FileFreshness};
use crate::calls::extract_type_references;
use crate::inspect::job::DISPATCHED_CALLEE_SEPARATOR;
use crate::inspect::{
    CallgraphOutboundCall, CallgraphSnapshot, FileContribution, InspectCategory, InspectJob,
    InspectResult, InspectScanSuccess,
};
use crate::parser::{detect_language, grammar_for, LangId};

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
            is_type_like: is_type_like_kind(&export.kind),
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

    let dispatched_method_names = snapshot
        .outbound_calls
        .iter()
        .filter(|call| same_file(&job.project_root, &call.caller_file, file))
        .filter_map(dispatched_method_name_from_call)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    let type_ref_names = collect_type_ref_names(file);

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

    let mut payload = json!({
        "file": file_name,
        "exports": exports
            .iter()
            .map(|export| {
                let mut value = json!({
                    "symbol": export.symbol,
                    "kind": export.kind,
                    "line": export.line,
                    "is_entry_point": export.is_entry_point,
                });
                if export.is_type_like {
                    value["is_type_like"] = json!(true);
                }
                value
            })
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
    });
    if !dispatched_method_names.is_empty() {
        payload["dispatched_method_names"] = json!(dispatched_method_names);
    }

    FileContribution::new(
        InspectCategory::DeadCode,
        file.to_path_buf(),
        collect_freshness(file),
        payload,
    )
    .with_type_ref_names(type_ref_names)
}

pub(crate) fn callgraph_unavailable_aggregate(scanned_files: usize) -> serde_json::Value {
    json!({
        "count": 0,
        "items": [],
        "by_language": {},
        "drill_down_capped": false,
        "uncertain_count": 0,
        "uncertain_items": [],
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

    let edges_by_source = edges_by_source(&parsed);
    let reachable = reachable_exports(&parsed, &edges_by_source);
    let reachable_type_ref_names = collect_reachable_type_ref_names(&parsed, &reachable);
    let dispatched_method_names = collect_dispatched_method_names(&parsed);

    let mut by_language: BTreeMap<String, usize> = BTreeMap::new();
    let mut count = 0usize;
    let mut dead_items = Vec::new();
    let uncertain_count = 0usize;
    let uncertain_items: Vec<serde_json::Value> = Vec::new();
    for contribution in &parsed {
        let is_public_api_file = public_api_files.contains(&contribution.file);
        for export in &contribution.exports {
            let node = (contribution.file.clone(), export.symbol.clone());
            if reachable.contains(&node)
                || is_public_api_file
                || dispatched_method_names.contains(symbol_liveness_name(&export.symbol))
            {
                continue;
            }

            if (export.is_type_like || is_type_like_kind(&export.kind))
                && reachable_type_ref_names.contains(symbol_liveness_name(&export.symbol))
            {
                continue;
            }

            count += 1;
            *by_language
                .entry(language_for_file(&contribution.file).to_string())
                .or_default() += 1;
            if drill_down_limit.is_none_or(|limit| dead_items.len() < limit) {
                dead_items.push(json!({
                    "file": contribution.file,
                    "symbol": export.symbol,
                    "kind": export.kind,
                    "line": export.line,
                }));
            }
        }
    }

    json!({
        "count": count,
        "items": dead_items,
        "by_language": by_language,
        "drill_down_capped": drill_down_limit.is_some_and(|limit| count > limit),
        "uncertain_count": uncertain_count,
        "uncertain_items": uncertain_items,
        "callgraph_available": true,
        "scanned_files": contributions.len(),
    })
}

fn edges_by_source(
    contributions: &[DeadCodeContribution],
) -> BTreeMap<ExportNode, BTreeSet<ExportNode>> {
    let mut edges: BTreeMap<ExportNode, BTreeSet<ExportNode>> = BTreeMap::new();

    for contribution in contributions {
        for call in &contribution.internal_calls {
            // Keep EVERY resolved edge, regardless of whether the target is an
            // exported symbol. Liveness must traverse through private
            // intermediaries (a private router/helper that forwards a root to a
            // public handler). Restricting targets to exports severed the chain
            // at the first private hop and made every handler reachable only via
            // a private function look dead. Node identity is (file, symbol);
            // private and exported symbols share the same node space.
            if call.caller_symbol.is_empty() {
                continue;
            }
            let target = (call.file.clone(), call.symbol.clone());
            let source = (contribution.file.clone(), call.caller_symbol.clone());
            edges.entry(source).or_default().insert(target);
        }
    }

    edges
}

fn collect_dispatched_method_names(contributions: &[DeadCodeContribution]) -> BTreeSet<String> {
    contributions
        .iter()
        .flat_map(|contribution| contribution.dispatched_method_names.iter().cloned())
        .collect()
}

fn collect_reachable_type_ref_names(
    contributions: &[DeadCodeContribution],
    reachable: &BTreeSet<ExportNode>,
) -> BTreeSet<String> {
    contributions
        .iter()
        .filter(|contribution| file_is_reachable(contribution, reachable))
        .flat_map(|contribution| contribution.type_ref_names.iter().cloned())
        .collect()
}

fn file_is_reachable(
    contribution: &DeadCodeContribution,
    reachable: &BTreeSet<ExportNode>,
) -> bool {
    !contribution.liveness_roots.is_empty()
        || contribution
            .exports
            .iter()
            .any(|export| export.is_entry_point)
        || reachable
            .iter()
            .any(|(file, _symbol)| file == &contribution.file)
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
        // Qualified target (file::symbol). The snapshot builder already
        // resolved and validated this edge — cross-file targets are confirmed
        // exports of the target file, and same-file targets are confirmed
        // definitions (private functions included, e.g. `main.rs::dispatch`).
        // Keep the edge regardless of the target's export visibility: liveness
        // must flow THROUGH private intermediaries, otherwise a public handler
        // reached only via a private router/helper looks unreachable.
        Some(file) => file,
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

fn dispatched_method_name_from_call(call: &CallgraphOutboundCall) -> Option<String> {
    let (target, full_callee) = split_call_target_metadata(&call.target);
    if let Some(full_callee) = full_callee {
        return dispatched_method_name_from_callee(full_callee);
    }
    if target.contains("::") || target.contains('#') {
        return None;
    }
    dispatched_method_name_from_callee(target)
}

fn dispatched_method_name_from_callee(callee: &str) -> Option<String> {
    let callee = callee.trim();
    if !callee.contains('.') {
        return None;
    }

    clean_symbol(callee.rsplit('.').next()?.trim().trim_start_matches('?'))
}

fn split_call_target_metadata(target: &str) -> (&str, Option<&str>) {
    target
        .split_once(DISPATCHED_CALLEE_SEPARATOR)
        .map_or((target, None), |(target, full_callee)| {
            (target, Some(full_callee))
        })
}

fn symbol_liveness_name(symbol: &str) -> &str {
    symbol
        .rsplit(['.', ':', '#'])
        .find(|segment| !segment.is_empty())
        .unwrap_or(symbol)
}

fn is_type_like_kind(kind: &str) -> bool {
    matches!(
        kind,
        "struct" | "enum" | "trait" | "type" | "type_alias" | "interface"
    )
}

fn parse_target(project_root: &Path, target: &str) -> ParsedTarget {
    let (target, _) = split_call_target_metadata(target);
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

fn collect_type_ref_names(file: &Path) -> BTreeSet<String> {
    let Some(lang) = detect_language(file).filter(|lang| supports_type_refs(*lang)) else {
        return BTreeSet::new();
    };
    let Ok(source) = fs::read_to_string(file) else {
        return BTreeSet::new();
    };
    let grammar = grammar_for(lang);
    let mut parser = tree_sitter::Parser::new();
    if parser.set_language(&grammar).is_err() {
        return BTreeSet::new();
    }
    let Some(tree) = parser.parse(&source, None) else {
        return BTreeSet::new();
    };

    extract_type_references(&source, tree.root_node(), lang)
}

fn supports_type_refs(lang: LangId) -> bool {
    matches!(
        lang,
        LangId::TypeScript
            | LangId::Tsx
            | LangId::JavaScript
            | LangId::Python
            | LangId::Rust
            | LangId::Go
    )
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
    #[serde(default)]
    dispatched_method_names: Vec<String>,
    #[serde(default)]
    type_ref_names: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct ExportContribution {
    symbol: String,
    kind: String,
    line: u32,
    #[serde(default)]
    is_type_like: bool,
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, RwLock};

    use crate::config::Config;
    use crate::inspect::job::DISPATCHED_CALLEE_SEPARATOR;
    use crate::inspect::{CallgraphExport, JobKey};
    use crate::parser::SymbolCache;

    fn fixture_project(files: &[(&str, &str)]) -> (tempfile::TempDir, PathBuf, Vec<PathBuf>) {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let root = temp_dir.path().join("project");
        fs::create_dir_all(&root).expect("create project root");

        let paths = files
            .iter()
            .map(|(relative, contents)| {
                let path = root.join(relative);
                if let Some(parent) = path.parent() {
                    fs::create_dir_all(parent).expect("create parent");
                }
                fs::write(&path, contents).expect("write fixture file");
                path
            })
            .collect::<Vec<_>>();

        (temp_dir, root, paths)
    }

    fn job(root: &Path, scope_files: Vec<PathBuf>, snapshot: CallgraphSnapshot) -> InspectJob {
        InspectJob {
            job_id: 1,
            key: JobKey::for_project_category(InspectCategory::DeadCode),
            category: InspectCategory::DeadCode,
            scope_files,
            project_root: root.to_path_buf(),
            inspect_dir: root.join(".aft-cache").join("inspect"),
            config: Arc::new(Config {
                project_root: Some(root.to_path_buf()),
                ..Config::default()
            }),
            symbol_cache: Arc::new(RwLock::new(SymbolCache::new())),
            callgraph_snapshot: Some(Arc::new(snapshot)),
        }
    }

    fn snapshot(
        files: Vec<PathBuf>,
        exported_symbols: Vec<CallgraphExport>,
        outbound_calls: Vec<CallgraphOutboundCall>,
    ) -> CallgraphSnapshot {
        snapshot_with_entry_points(files, exported_symbols, outbound_calls, BTreeSet::new())
    }

    fn snapshot_with_entry_points(
        files: Vec<PathBuf>,
        exported_symbols: Vec<CallgraphExport>,
        outbound_calls: Vec<CallgraphOutboundCall>,
        entry_points: BTreeSet<PathBuf>,
    ) -> CallgraphSnapshot {
        CallgraphSnapshot {
            generated_at: None,
            files,
            exported_symbols,
            outbound_calls,
            entry_points,
        }
    }

    fn export(root: &Path, file: &str, symbol: &str, kind: &str) -> CallgraphExport {
        CallgraphExport {
            file: root.join(file),
            symbol: symbol.to_string(),
            kind: kind.to_string(),
            line: 1,
        }
    }

    fn outbound(
        root: &Path,
        caller_file: &str,
        caller_symbol: &str,
        target: &str,
    ) -> CallgraphOutboundCall {
        CallgraphOutboundCall {
            caller_file: root.join(caller_file),
            caller_symbol: caller_symbol.to_string(),
            target: target.to_string(),
            line: 1,
        }
    }

    fn dispatched_target(target: &str, full_callee: &str) -> String {
        format!("{target}{DISPATCHED_CALLEE_SEPARATOR}{full_callee}")
    }

    fn scan(job: InspectJob) -> serde_json::Value {
        run_dead_code_scan(&job)
            .outcome
            .expect("scan succeeds")
            .aggregate
    }

    #[test]
    fn method_dispatched_by_receiver_call_is_live() {
        let (_temp_dir, root, paths) = fixture_project(&[
            ("src/service.ts", "export class Service { render() {} }\n"),
            (
                "src/consumer.ts",
                "function run(service: Service) { service.render(); }\n",
            ),
        ]);
        let aggregate = scan(job(
            &root,
            paths.clone(),
            snapshot(
                paths,
                vec![export(&root, "src/service.ts", "render", "method")],
                vec![outbound(
                    &root,
                    "src/consumer.ts",
                    "run",
                    &dispatched_target("render", "service.render"),
                )],
            ),
        ));

        assert_eq!(aggregate["count"], 0);
        assert_eq!(aggregate["uncertain_count"], 0);
        assert!(aggregate["items"].as_array().unwrap().is_empty());
    }

    #[test]
    fn method_without_any_dispatch_is_still_dead() {
        let (_temp_dir, root, paths) =
            fixture_project(&[("src/service.ts", "export class Service { render() {} }\n")]);
        let aggregate = scan(job(
            &root,
            paths.clone(),
            snapshot(
                paths,
                vec![export(&root, "src/service.ts", "render", "method")],
                Vec::new(),
            ),
        ));

        assert_eq!(aggregate["count"], 1);
        assert_eq!(aggregate["items"][0]["symbol"], "render");
        assert_eq!(aggregate["uncertain_count"], 0);
    }

    #[test]
    fn rust_struct_referenced_only_in_types_is_live() {
        let (_temp_dir, root, paths) = fixture_project(&[
            ("src/types.rs", "pub struct Widget { id: u64 }\n"),
            (
                "src/main.rs",
                "use crate::types::Widget;\nstruct Holder { value: Widget }\npub fn main(input: Widget) -> Widget { input }\n",
            ),
        ]);
        let aggregate = scan(job(
            &root,
            paths.clone(),
            snapshot_with_entry_points(
                paths,
                vec![
                    export(&root, "src/types.rs", "Widget", "struct"),
                    export(&root, "src/main.rs", "main", "function"),
                ],
                Vec::new(),
                BTreeSet::from([root.join("src/main.rs")]),
            ),
        ));

        assert_eq!(aggregate["count"], 0);
        assert_eq!(aggregate["uncertain_count"], 0);
        assert!(aggregate["items"].as_array().unwrap().is_empty());
    }

    #[test]
    fn ts_interface_referenced_only_in_type_annotation_is_live() {
        let (_temp_dir, root, paths) = fixture_project(&[
            ("src/types.ts", "export interface Widget { id: string }\n"),
            (
                "src/main.ts",
                "import type { Widget } from './types';\nexport function run(input: Widget): void {}\n",
            ),
        ]);
        let aggregate = scan(job(
            &root,
            paths.clone(),
            snapshot_with_entry_points(
                paths,
                vec![
                    export(&root, "src/types.ts", "Widget", "interface"),
                    export(&root, "src/main.ts", "run", "function"),
                ],
                Vec::new(),
                BTreeSet::from([root.join("src/main.ts")]),
            ),
        ));

        assert_eq!(aggregate["count"], 0);
        assert_eq!(aggregate["uncertain_count"], 0);
        assert!(aggregate["items"].as_array().unwrap().is_empty());
    }

    #[test]
    fn type_like_export_without_call_or_type_ref_is_precise_dead() {
        let (_temp_dir, root, paths) =
            fixture_project(&[("src/types.ts", "export interface Widget { id: string }\n")]);
        let aggregate = scan(job(
            &root,
            paths.clone(),
            snapshot(
                paths,
                vec![export(&root, "src/types.ts", "Widget", "interface")],
                Vec::new(),
            ),
        ));

        assert_eq!(aggregate["count"], 1);
        assert_eq!(aggregate["items"][0]["symbol"], "Widget");
        assert_eq!(aggregate["uncertain_count"], 0);
        assert!(aggregate["uncertain_items"].as_array().unwrap().is_empty());
    }

    #[test]
    fn genuinely_unreachable_function_is_still_dead() {
        let (_temp_dir, root, paths) =
            fixture_project(&[("src/build.ts", "export function build() {}\n")]);
        let aggregate = scan(job(
            &root,
            paths.clone(),
            snapshot(
                paths,
                vec![export(&root, "src/build.ts", "build", "function")],
                Vec::new(),
            ),
        ));

        assert_eq!(aggregate["count"], 1);
        assert_eq!(aggregate["items"][0]["symbol"], "build");
        assert_eq!(aggregate["uncertain_count"], 0);
    }
}
