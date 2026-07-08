use std::collections::{BTreeMap, BTreeSet, HashSet, VecDeque};
use std::path::{Path, PathBuf};

use serde::Serialize;
use tree_sitter::{Node, Parser};

use crate::callgraph::{self, TraceToSymbolCandidate};
use crate::callgraph_store::{
    CallGraphRead, CallGraphStoreError, StoreCallSite, StoreNode, StoreUnresolvedCall,
};
use crate::edit::line_col_to_byte;
use crate::error::AftError;
use crate::inspect::job::is_test_file;
use crate::parser::{
    detect_language, extract_symbols_from_tree, grammar_for, FileParser, SharedSymbolCache,
};
use crate::protocol::Response;
use crate::symbols::Symbol;

pub type StoreAdapterResult<T> = Result<T, CallGraphStoreError>;

const TRACE_DATA_RESOLVER_PROVENANCE: &str = "treesitter+resolver";
const HUB_SUMMARY_THRESHOLD: usize = 20;
const HUB_SUMMARY_LIMIT: usize = 15;

#[derive(Debug, Clone, Serialize)]
pub struct StoreHubSummary {
    pub message: String,
    pub total: usize,
    pub hidden_tests: usize,
    pub shown: usize,
    pub threshold: usize,
    pub limit: usize,
}

#[derive(Debug, Clone, Default)]
struct EdgeMarker {
    approximate: Option<bool>,
    resolved_by: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct StoreCallersResult {
    pub symbol: String,
    pub file: String,
    pub callers: Vec<StoreCallerGroup>,
    pub total_callers: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hub_summary: Option<StoreHubSummary>,
    pub scanned_files: usize,
    pub depth_limited: bool,
    pub truncated: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct StoreCallerGroup {
    pub file: String,
    pub callers: Vec<StoreCallerEntry>,
}

#[derive(Debug, Clone, Serialize)]
pub struct StoreCallerEntry {
    pub symbol: String,
    pub line: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub approximate: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolved_by: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct StoreCallTreeNode {
    pub name: String,
    pub file: String,
    pub line: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
    pub resolved: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub approximate: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolved_by: Option<String>,
    pub children: Vec<StoreCallTreeNode>,
    pub depth_limited: bool,
    pub truncated: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct StoreImpactResult {
    pub symbol: String,
    pub file: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
    pub parameters: Vec<String>,
    pub total_affected: usize,
    pub affected_files: usize,
    pub callers: Vec<StoreImpactCaller>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hub_summary: Option<StoreHubSummary>,
    pub depth_limited: bool,
    pub truncated: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct StoreImpactCaller {
    pub caller_symbol: String,
    pub caller_file: String,
    pub line: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
    pub is_entry_point: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub call_expression: Option<String>,
    pub parameters: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub approximate: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolved_by: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct StoreTraceHop {
    pub symbol: String,
    pub file: String,
    pub line: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
    pub is_entry_point: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub approximate: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolved_by: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct StoreTracePath {
    pub hops: Vec<StoreTraceHop>,
}

#[derive(Debug, Clone, Serialize)]
pub struct StoreTraceToResult {
    pub target_symbol: String,
    pub target_file: String,
    pub paths: Vec<StoreTracePath>,
    pub total_paths: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hub_summary: Option<StoreHubSummary>,
    pub entry_points_found: usize,
    pub max_depth_reached: bool,
    pub truncated_paths: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct StoreTraceToSymbolHop {
    pub symbol: String,
    pub file: String,
    pub line: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub approximate: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolved_by: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct StoreTraceToSymbolResult {
    pub path: Option<Vec<StoreTraceToSymbolHop>>,
    pub complete: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

enum ForwardCall {
    Resolved(StoreCallSite),
    Unresolved(StoreUnresolvedCall),
}

#[derive(Clone)]
enum TraceForwardCall {
    Resolved(StoreCallSite),
    Unresolved(StoreUnresolvedCall),
}

impl TraceForwardCall {
    fn byte_start(&self) -> usize {
        match self {
            Self::Resolved(site) => site.byte_start,
            Self::Unresolved(call) => call.byte_start,
        }
    }

    fn byte_end(&self) -> usize {
        match self {
            Self::Resolved(site) => site.byte_end,
            Self::Unresolved(call) => call.byte_end,
        }
    }

    fn line(&self) -> u32 {
        match self {
            Self::Resolved(site) => site.line,
            Self::Unresolved(call) => call.line,
        }
    }

    fn matches_position(&self, byte_start: usize, byte_end: usize) -> bool {
        self.byte_start() == byte_start && self.byte_end() == byte_end
    }
}

impl ForwardCall {
    fn byte_start(&self) -> usize {
        match self {
            Self::Resolved(site) => site.byte_start,
            Self::Unresolved(call) => call.byte_start,
        }
    }

    fn line(&self) -> u32 {
        match self {
            Self::Resolved(site) => site.line,
            Self::Unresolved(call) => call.line,
        }
    }

    fn call_site_key(&self) -> (String, u32, String) {
        match self {
            Self::Resolved(site) => (
                site.caller.file.clone(),
                site.line,
                format!("{}::{}", site.target_file, site.target_symbol),
            ),
            Self::Unresolved(call) => (call.caller.file.clone(), call.line, call.symbol.clone()),
        }
    }
}

#[derive(Clone)]
struct ResolvedStoreSymbol {
    representative: StoreNode,
    nodes: Vec<StoreNode>,
}

#[derive(Clone)]
struct TraceElem {
    node: StoreNode,
    edge: EdgeMarker,
}

fn edge_marker(site: &StoreCallSite) -> EdgeMarker {
    if let Some(resolved_by) = site.supplemental_resolution() {
        EdgeMarker {
            approximate: Some(site.approximate()),
            resolved_by: Some(resolved_by.to_string()),
        }
    } else {
        EdgeMarker::default()
    }
}

fn edge_approximate(site: &StoreCallSite) -> Option<bool> {
    site.supplemental_resolution().map(|_| site.approximate())
}

fn edge_resolved_by(site: &StoreCallSite) -> Option<String> {
    site.supplemental_resolution().map(ToString::to_string)
}

fn test_hidden_summary(
    kind: &str,
    total: usize,
    hidden_tests: usize,
    shown: usize,
) -> StoreHubSummary {
    StoreHubSummary {
        message: format!(
            "Next: {total} {kind} ({hidden_tests} in tests, hidden — pass includeTests) — narrow with scope"
        ),
        total,
        hidden_tests,
        shown,
        threshold: HUB_SUMMARY_THRESHOLD,
        limit: HUB_SUMMARY_LIMIT,
    }
}

fn included_summary(
    kind: &str,
    total: usize,
    hidden_tests: usize,
    shown: usize,
) -> StoreHubSummary {
    let test_note = if hidden_tests == 0 {
        String::new()
    } else {
        format!(" ({hidden_tests} in tests, included)")
    };
    StoreHubSummary {
        message: format!("Next: {total} {kind}{test_note} — showing {shown}; narrow with scope"),
        total,
        hidden_tests,
        shown,
        threshold: HUB_SUMMARY_THRESHOLD,
        limit: HUB_SUMMARY_LIMIT,
    }
}

fn callsite_is_from_test(site: &StoreCallSite) -> bool {
    is_test_file(&site.caller.file)
}

fn trace_path_starts_in_test(path: &StoreTracePath) -> bool {
    path.hops.first().is_some_and(|hop| is_test_file(&hop.file))
}

fn dedup_sites_for_summary(sites: Vec<StoreCallSite>) -> Vec<StoreCallSite> {
    let mut seen = BTreeSet::new();
    sites
        .into_iter()
        .filter(|site| seen.insert((site.caller.symbol.clone(), site.target_symbol.clone())))
        .collect()
}

fn trace_path_shape(path: &StoreTracePath) -> Vec<(String, String)> {
    path.hops
        .iter()
        .map(|hop| (hop.file.clone(), hop.symbol.clone()))
        .collect()
}

fn dedup_paths_for_summary(paths: Vec<StoreTracePath>) -> Vec<StoreTracePath> {
    let mut seen = BTreeSet::new();
    paths
        .into_iter()
        .filter(|path| seen.insert(trace_path_shape(path)))
        .collect()
}

fn filter_call_tree_tests(node: &mut StoreCallTreeNode) {
    node.children.retain(|child| !is_test_file(&child.file));
    for child in &mut node.children {
        filter_call_tree_tests(child);
    }
}

pub fn callers_result(
    store: &impl CallGraphRead,
    file: &Path,
    symbol: &str,
    depth: usize,
    include_tests: bool,
) -> StoreAdapterResult<StoreCallersResult> {
    let target = resolve_symbol_query(store, file, symbol)?;
    let effective_depth = depth.max(1);
    let mut visited = HashSet::new();
    let mut sites = Vec::new();
    let mut depth_limited = false;
    let mut truncated = 0usize;

    collect_callers_recursive(
        store,
        &target.representative.file,
        &target.representative.symbol,
        effective_depth,
        0,
        &mut visited,
        &mut sites,
        &mut depth_limited,
        &mut truncated,
    )?;

    let mut sites = dedup_call_sites(sites);
    sites.sort_by(|left, right| {
        left.caller
            .file
            .cmp(&right.caller.file)
            .then(left.line.cmp(&right.line))
            .then(left.caller.symbol.cmp(&right.caller.symbol))
    });
    let total_callers = sites.len();
    let hidden_tests = sites
        .iter()
        .filter(|site| callsite_is_from_test(site))
        .count();
    let summarize = total_callers > HUB_SUMMARY_THRESHOLD;
    let visible_sites = sites
        .into_iter()
        .filter(|site| include_tests || !callsite_is_from_test(site))
        .collect::<Vec<_>>();
    let visible_sites = if summarize {
        dedup_sites_for_summary(visible_sites)
            .into_iter()
            .take(HUB_SUMMARY_LIMIT)
            .collect::<Vec<_>>()
    } else {
        visible_sites
    };
    let hub_summary = if summarize {
        Some(if include_tests {
            included_summary("callers", total_callers, hidden_tests, visible_sites.len())
        } else {
            test_hidden_summary("callers", total_callers, hidden_tests, visible_sites.len())
        })
    } else {
        None
    };
    let mut groups: BTreeMap<String, Vec<StoreCallerEntry>> = BTreeMap::new();
    for site in visible_sites {
        groups
            .entry(site.caller.file.clone())
            .or_default()
            .push(StoreCallerEntry {
                symbol: site.caller.symbol.clone(),
                line: site.line,
                approximate: edge_approximate(&site),
                resolved_by: edge_resolved_by(&site),
            });
    }

    Ok(StoreCallersResult {
        symbol: target.representative.symbol,
        file: target.representative.file,
        callers: groups
            .into_iter()
            .map(|(file, callers)| StoreCallerGroup { file, callers })
            .collect(),
        total_callers,
        hub_summary,
        scanned_files: store.indexed_file_count()?,
        depth_limited,
        truncated,
    })
}

pub fn call_tree_result(
    store: &impl CallGraphRead,
    file: &Path,
    symbol: &str,
    depth: usize,
    include_tests: bool,
) -> StoreAdapterResult<StoreCallTreeNode> {
    let target = resolve_symbol_query(store, file, symbol)?;
    let mut visited = HashSet::new();
    let mut tree = call_tree_inner(store, &target, depth, 0, &mut visited)?;
    if !include_tests {
        filter_call_tree_tests(&mut tree);
    }
    Ok(tree)
}

pub fn impact_result(
    store: &impl CallGraphRead,
    file: &Path,
    symbol: &str,
    depth: usize,
    include_tests: bool,
) -> StoreAdapterResult<StoreImpactResult> {
    let target = resolve_symbol_query(store, file, symbol)?;
    let effective_depth = depth.max(1);
    let mut visited = HashSet::new();
    let mut sites = Vec::new();
    let mut depth_limited = false;
    let mut truncated = 0usize;

    collect_callers_recursive(
        store,
        &target.representative.file,
        &target.representative.symbol,
        effective_depth,
        0,
        &mut visited,
        &mut sites,
        &mut depth_limited,
        &mut truncated,
    )?;

    let mut sites = dedup_call_sites(sites);
    sites.sort_by(|left, right| {
        left.caller
            .file
            .cmp(&right.caller.file)
            .then(left.line.cmp(&right.line))
            .then(left.caller.symbol.cmp(&right.caller.symbol))
    });
    let total_affected = sites.len();
    let hidden_tests = sites
        .iter()
        .filter(|site| callsite_is_from_test(site))
        .count();
    let summarize = total_affected > HUB_SUMMARY_THRESHOLD;
    let affected_files = sites
        .iter()
        .map(|site| site.caller.file.clone())
        .collect::<BTreeSet<_>>()
        .len();
    let visible_sites = sites
        .into_iter()
        .filter(|site| include_tests || !callsite_is_from_test(site))
        .collect::<Vec<_>>();
    let visible_sites = if summarize {
        dedup_sites_for_summary(visible_sites)
            .into_iter()
            .take(HUB_SUMMARY_LIMIT)
            .collect::<Vec<_>>()
    } else {
        visible_sites
    };
    let hub_summary = if summarize {
        Some(if include_tests {
            included_summary(
                "affected callers",
                total_affected,
                hidden_tests,
                visible_sites.len(),
            )
        } else {
            test_hidden_summary(
                "affected callers",
                total_affected,
                hidden_tests,
                visible_sites.len(),
            )
        })
    } else {
        None
    };
    let target_signature = target.representative.signature.clone();
    let target_parameters = target_signature
        .as_deref()
        .map(|signature| callgraph::extract_parameters(signature, target.representative.lang))
        .unwrap_or_default();

    let mut callers = Vec::new();
    for site in visible_sites {
        callers.push(StoreImpactCaller {
            caller_symbol: site.caller.symbol.clone(),
            caller_file: site.caller.file.clone(),
            line: site.line,
            signature: site.caller.signature.clone(),
            is_entry_point: site.caller.is_entry_point,
            call_expression: read_source_line(
                &store.project_root().join(&site.caller.file),
                site.line,
            ),
            parameters: site
                .caller
                .signature
                .as_deref()
                .map(|signature| callgraph::extract_parameters(signature, site.caller.lang))
                .unwrap_or_default(),
            approximate: edge_approximate(&site),
            resolved_by: edge_resolved_by(&site),
        });
    }
    callers.sort_by(|left, right| {
        left.caller_file
            .cmp(&right.caller_file)
            .then(left.line.cmp(&right.line))
    });

    Ok(StoreImpactResult {
        symbol: target.representative.symbol,
        file: target.representative.file,
        signature: target_signature,
        parameters: target_parameters,
        total_affected,
        affected_files,
        callers,
        hub_summary,
        depth_limited,
        truncated,
    })
}

pub fn trace_to_result(
    store: &impl CallGraphRead,
    file: &Path,
    symbol: &str,
    max_depth: usize,
    include_tests: bool,
) -> StoreAdapterResult<StoreTraceToResult> {
    let target = resolve_symbol_query(store, file, symbol)?;
    let effective_max = if max_depth == 0 { 10 } else { max_depth };

    let initial = vec![TraceElem {
        node: target.representative.clone(),
        edge: EdgeMarker::default(),
    }];
    let mut complete_paths = Vec::new();
    if target.representative.is_entry_point {
        complete_paths.push(initial.clone());
    }

    let mut queue = vec![(initial, 0usize)];
    let mut max_depth_reached = false;
    let mut truncated_paths = 0usize;

    while let Some((path, depth)) = queue.pop() {
        if depth >= effective_max {
            max_depth_reached = true;
            continue;
        }
        let Some(current) = path.last() else {
            continue;
        };
        let callers = dedup_call_sites(
            store.direct_callers_of(Path::new(&current.node.file), &current.node.symbol)?,
        );
        if callers.is_empty() {
            if path.len() > 1 {
                truncated_paths += 1;
            }
            continue;
        }

        let mut has_new_path = false;
        for site in callers {
            if path.iter().any(|elem| {
                elem.node.file == site.caller.file && elem.node.symbol == site.caller.symbol
            }) {
                continue;
            }
            has_new_path = true;
            let mut next_path = path.clone();
            if let Some(current) = next_path.last_mut() {
                current.edge = edge_marker(&site);
            }
            next_path.push(TraceElem {
                node: site.caller.clone(),
                edge: EdgeMarker::default(),
            });
            if site.caller.is_entry_point {
                complete_paths.push(next_path.clone());
            }
            queue.push((next_path, depth + 1));
        }
        if !has_new_path && path.len() > 1 {
            truncated_paths += 1;
        }
    }

    let mut paths: Vec<StoreTracePath> = complete_paths
        .into_iter()
        .map(|mut elems| {
            elems.reverse();
            let hops = elems
                .iter()
                .enumerate()
                .map(|(index, elem)| StoreTraceHop {
                    symbol: elem.node.symbol.clone(),
                    file: elem.node.file.clone(),
                    line: elem.node.line,
                    signature: elem.node.signature.clone(),
                    is_entry_point: index == 0 && elem.node.is_entry_point,
                    approximate: elem.edge.approximate,
                    resolved_by: elem.edge.resolved_by.clone(),
                })
                .collect();
            StoreTracePath { hops }
        })
        .collect();
    paths.sort_by(|left, right| {
        let left_entry = left
            .hops
            .first()
            .map(|hop| hop.symbol.as_str())
            .unwrap_or("");
        let right_entry = right
            .hops
            .first()
            .map(|hop| hop.symbol.as_str())
            .unwrap_or("");
        left_entry
            .cmp(right_entry)
            .then(left.hops.len().cmp(&right.hops.len()))
    });
    let total_paths = paths.len();
    let hidden_tests = paths
        .iter()
        .filter(|path| trace_path_starts_in_test(path))
        .count();
    let summarize = total_paths > HUB_SUMMARY_THRESHOLD;
    let visible_paths = paths
        .into_iter()
        .filter(|path| include_tests || !trace_path_starts_in_test(path))
        .collect::<Vec<_>>();
    let paths = if summarize {
        dedup_paths_for_summary(visible_paths)
            .into_iter()
            .take(HUB_SUMMARY_LIMIT)
            .collect::<Vec<_>>()
    } else {
        visible_paths
    };
    let hub_summary = if summarize {
        Some(if include_tests {
            included_summary("paths", total_paths, hidden_tests, paths.len())
        } else {
            test_hidden_summary("paths", total_paths, hidden_tests, paths.len())
        })
    } else {
        None
    };

    let entry_points_found = paths
        .iter()
        .filter_map(|path| path.hops.first())
        .filter(|hop| hop.is_entry_point)
        .map(|hop| (hop.file.clone(), hop.symbol.clone()))
        .collect::<HashSet<_>>()
        .len();

    Ok(StoreTraceToResult {
        target_symbol: target.representative.symbol,
        target_file: target.representative.file,
        total_paths,
        hub_summary,
        paths,
        entry_points_found,
        max_depth_reached,
        truncated_paths,
    })
}

pub fn ensure_symbol_resolves(
    store: &impl CallGraphRead,
    file: &Path,
    symbol: &str,
) -> StoreAdapterResult<()> {
    resolve_symbol_query(store, file, symbol).map(|_| ())
}

pub fn trace_to_symbol_candidates(
    store: &impl CallGraphRead,
    to_symbol: &str,
) -> StoreAdapterResult<Vec<TraceToSymbolCandidate>> {
    store.trace_to_symbol_candidates(to_symbol)
}

pub fn trace_to_symbol_result(
    store: &impl CallGraphRead,
    file: &Path,
    symbol: &str,
    to_symbol: &str,
    to_file: Option<&Path>,
    max_depth: usize,
    include_tests: bool,
) -> StoreAdapterResult<StoreTraceToSymbolResult> {
    let origin = resolve_symbol_query(store, file, symbol)?;
    let target_file = to_file.map(|path| relative_file(store, path));
    let effective_max = if max_depth == 0 {
        10
    } else {
        max_depth.min(16)
    };

    let start_hop = trace_to_symbol_hop(&origin.representative);
    if trace_to_symbol_matches_target(
        &origin.representative.file,
        &origin.representative.symbol,
        to_symbol,
        target_file.as_deref(),
    ) {
        return Ok(StoreTraceToSymbolResult {
            path: Some(vec![start_hop]),
            complete: true,
            reason: None,
        });
    }

    let mut queue = VecDeque::new();
    queue.push_back((
        origin.representative.file.clone(),
        origin.representative.symbol.clone(),
        vec![start_hop],
        0usize,
    ));
    let mut visited = HashSet::new();
    visited.insert((
        origin.representative.file.clone(),
        origin.representative.symbol.clone(),
    ));
    let mut max_depth_exhausted = false;

    while let Some((current_file, current_symbol, path, depth)) = queue.pop_front() {
        let callees = forward_resolved_callees(store, &current_file, &current_symbol)?;

        if depth >= effective_max {
            if callees
                .iter()
                .any(|(node, _)| !visited.contains(&(node.file.clone(), node.symbol.clone())))
            {
                max_depth_exhausted = true;
            }
            continue;
        }

        for (callee, edge) in callees {
            if !include_tests && is_test_file(&callee.file) {
                continue;
            }
            if !visited.insert((callee.file.clone(), callee.symbol.clone())) {
                continue;
            }
            let mut next_path = path.clone();
            next_path.push(trace_to_symbol_hop_with_edge(&callee, edge));
            if trace_to_symbol_matches_target(
                &callee.file,
                &callee.symbol,
                to_symbol,
                target_file.as_deref(),
            ) {
                return Ok(StoreTraceToSymbolResult {
                    path: Some(next_path),
                    complete: true,
                    reason: None,
                });
            }
            queue.push_back((callee.file, callee.symbol, next_path, depth + 1));
        }
    }

    if max_depth_exhausted {
        Ok(StoreTraceToSymbolResult {
            path: None,
            complete: false,
            reason: Some("max_depth_exhausted".to_string()),
        })
    } else {
        Ok(StoreTraceToSymbolResult {
            path: None,
            complete: true,
            reason: Some("no_path_found".to_string()),
        })
    }
}

pub fn trace_data_result(
    store: &impl CallGraphRead,
    file: &Path,
    symbol: &str,
    expression: &str,
    max_depth: usize,
    symbol_cache: SharedSymbolCache,
) -> StoreAdapterResult<callgraph::TraceDataResult> {
    let origin_path = absolute_file(store, file);
    let origin_file = relative_file(store, &origin_path);
    let origin_symbol = resolve_symbol_query_with_cache(&origin_path, symbol, &symbol_cache)?;

    let mut hops = Vec::new();
    let mut depth_limited = false;
    let mut visited = HashSet::new();
    trace_data_inner(
        store,
        &symbol_cache,
        &origin_path,
        &origin_symbol,
        expression,
        max_depth,
        0,
        &mut hops,
        &mut depth_limited,
        &mut visited,
    )?;

    Ok(callgraph::TraceDataResult {
        expression: expression.to_string(),
        origin_file,
        origin_symbol,
        hops,
        depth_limited,
    })
}

#[allow(clippy::too_many_arguments)]
fn trace_data_inner(
    store: &impl CallGraphRead,
    symbol_cache: &SharedSymbolCache,
    file: &Path,
    symbol: &str,
    tracking_name: &str,
    max_depth: usize,
    current_depth: usize,
    hops: &mut Vec<callgraph::DataFlowHop>,
    depth_limited: &mut bool,
    visited: &mut HashSet<(String, String, String)>,
) -> StoreAdapterResult<()> {
    let rel_file = relative_file(store, file);
    let visit_key = (
        rel_file.clone(),
        symbol.to_string(),
        tracking_name.to_string(),
    );
    if visited.contains(&visit_key) {
        return Ok(());
    }
    visited.insert(visit_key);

    let current = resolve_exact_symbol(store, &rel_file, symbol, None)?
        .ok_or_else(|| CallGraphStoreError::StaleFiles(vec![rel_file.clone()]))?;
    let current_calls = trace_forward_calls_for_nodes(store, &current.nodes)?;

    // Keep the legacy value-flow posture: parse the current source for body walks
    // and use the store only for cross-hop call facts.
    let source = std::fs::read_to_string(file)?;
    let Some(lang) = detect_language(file) else {
        return Ok(());
    };
    let grammar = grammar_for(lang);
    let mut parser = Parser::new();
    parser
        .set_language(&grammar)
        .map_err(|error| AftError::ParseError {
            message: format!("grammar init failed for {:?}: {}", lang, error),
        })?;
    let tree = parser
        .parse(&source, None)
        .ok_or_else(|| AftError::ParseError {
            message: format!("parse failed for {}", file.display()),
        })?;
    let symbols = extract_symbols_from_tree(&source, &tree, lang)?;
    let sym_info = symbols
        .iter()
        .find(|candidate| {
            symbol_identity_from_cache(candidate) == symbol || candidate.name == symbol
        })
        .ok_or_else(|| CallGraphStoreError::StaleFiles(vec![rel_file.clone()]))?;

    let body_start = line_col_to_byte(&source, sym_info.range.start_line, sym_info.range.start_col);
    let body_end = line_col_to_byte(&source, sym_info.range.end_line, sym_info.range.end_col);
    let Some(body_node) = find_node_covering_range(tree.root_node(), body_start, body_end) else {
        return Ok(());
    };

    let mut tracked_names = vec![tracking_name.to_string()];
    walk_for_data_flow(
        store,
        symbol_cache,
        body_node,
        &source,
        &current_calls,
        &mut tracked_names,
        symbol,
        &rel_file,
        max_depth,
        current_depth,
        hops,
        depth_limited,
        visited,
    )
}

#[allow(clippy::too_many_arguments)]
fn walk_for_data_flow(
    store: &impl CallGraphRead,
    symbol_cache: &SharedSymbolCache,
    node: Node<'_>,
    source: &str,
    current_calls: &[TraceForwardCall],
    tracked_names: &mut Vec<String>,
    symbol: &str,
    rel_file: &str,
    max_depth: usize,
    current_depth: usize,
    hops: &mut Vec<callgraph::DataFlowHop>,
    depth_limited: &mut bool,
    visited: &mut HashSet<(String, String, String)>,
) -> StoreAdapterResult<()> {
    let kind = node.kind();
    let is_var_decl = matches!(
        kind,
        "variable_declarator"
            | "assignment_expression"
            | "augmented_assignment_expression"
            | "assignment"
            | "let_declaration"
            | "short_var_declaration"
    );

    if is_var_decl {
        if let Some((new_name, init_text, line, is_approx)) =
            extract_assignment_info(node, source, tracked_names)
        {
            if !is_approx {
                hops.push(callgraph::DataFlowHop {
                    file: rel_file.to_string(),
                    symbol: symbol.to_string(),
                    variable: new_name.clone(),
                    line,
                    flow_type: "assignment".to_string(),
                    approximate: false,
                });
                tracked_names.push(new_name);
            } else {
                hops.push(callgraph::DataFlowHop {
                    file: rel_file.to_string(),
                    symbol: symbol.to_string(),
                    variable: init_text,
                    line,
                    flow_type: "assignment".to_string(),
                    approximate: true,
                });
                return Ok(());
            }
        }
    }

    if kind == "call_expression" || kind == "call" || kind == "macro_invocation" {
        check_call_for_data_flow(
            store,
            symbol_cache,
            node,
            source,
            current_calls,
            tracked_names,
            symbol,
            rel_file,
            max_depth,
            current_depth,
            hops,
            depth_limited,
            visited,
        )?;
    }

    let mut cursor = node.walk();
    if cursor.goto_first_child() {
        loop {
            walk_for_data_flow(
                store,
                symbol_cache,
                cursor.node(),
                source,
                current_calls,
                tracked_names,
                symbol,
                rel_file,
                max_depth,
                current_depth,
                hops,
                depth_limited,
                visited,
            )?;
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }
    Ok(())
}

fn extract_assignment_info(
    node: Node<'_>,
    source: &str,
    tracked_names: &[String],
) -> Option<(String, String, u32, bool)> {
    let kind = node.kind();
    let line = node.start_position().row as u32 + 1;

    match kind {
        "variable_declarator" => {
            let name_node = node.child_by_field_name("name")?;
            let value_node = node.child_by_field_name("value")?;
            let name_text = trace_node_text(name_node, source);
            let value_text = trace_node_text(value_node, source);

            if name_node.kind() == "object_pattern" || name_node.kind() == "array_pattern" {
                if tracked_names
                    .iter()
                    .any(|tracked| value_text.contains(tracked))
                {
                    return Some((name_text.clone(), name_text, line, true));
                }
                return None;
            }

            if tracked_names.iter().any(|tracked| {
                value_text == *tracked
                    || value_text.starts_with(&format!("{}.", tracked))
                    || value_text.starts_with(&format!("{}[", tracked))
            }) {
                return Some((name_text, value_text, line, false));
            }
            None
        }
        "assignment_expression" | "augmented_assignment_expression" => {
            let left = node.child_by_field_name("left")?;
            let right = node.child_by_field_name("right")?;
            let left_text = trace_node_text(left, source);
            let right_text = trace_node_text(right, source);

            if tracked_names.iter().any(|tracked| right_text == *tracked) {
                return Some((left_text, right_text, line, false));
            }
            None
        }
        "assignment" => {
            let left = node.child_by_field_name("left")?;
            let right = node.child_by_field_name("right")?;
            let left_text = trace_node_text(left, source);
            let right_text = trace_node_text(right, source);

            if tracked_names.iter().any(|tracked| right_text == *tracked) {
                return Some((left_text, right_text, line, false));
            }
            None
        }
        "let_declaration" | "short_var_declaration" => {
            let left = node
                .child_by_field_name("pattern")
                .or_else(|| node.child_by_field_name("left"))?;
            let right = node
                .child_by_field_name("value")
                .or_else(|| node.child_by_field_name("right"))?;
            let left_text = trace_node_text(left, source);
            let right_text = trace_node_text(right, source);

            if tracked_names.iter().any(|tracked| right_text == *tracked) {
                return Some((left_text, right_text, line, false));
            }
            None
        }
        _ => None,
    }
}

#[allow(clippy::too_many_arguments)]
fn check_call_for_data_flow(
    store: &impl CallGraphRead,
    symbol_cache: &SharedSymbolCache,
    node: Node<'_>,
    source: &str,
    current_calls: &[TraceForwardCall],
    tracked_names: &[String],
    symbol: &str,
    rel_file: &str,
    max_depth: usize,
    current_depth: usize,
    hops: &mut Vec<callgraph::DataFlowHop>,
    depth_limited: &mut bool,
    visited: &mut HashSet<(String, String, String)>,
) -> StoreAdapterResult<()> {
    let args_node =
        find_child_by_kind(node, "arguments").or_else(|| find_child_by_kind(node, "argument_list"));
    let Some(args_node) = args_node else {
        return Ok(());
    };

    let mut arg_positions = Vec::new();
    let mut arg_idx = 0usize;
    let mut cursor = args_node.walk();
    if cursor.goto_first_child() {
        loop {
            let child = cursor.node();
            let child_kind = child.kind();
            if child_kind == "(" || child_kind == ")" || child_kind == "," {
                if !cursor.goto_next_sibling() {
                    break;
                }
                continue;
            }

            let arg_text = trace_node_text(child, source);
            if child_kind == "spread_element" || child_kind == "dictionary_splat" {
                if tracked_names
                    .iter()
                    .any(|tracked| arg_text.contains(tracked))
                {
                    hops.push(callgraph::DataFlowHop {
                        file: rel_file.to_string(),
                        symbol: symbol.to_string(),
                        variable: arg_text,
                        line: child.start_position().row as u32 + 1,
                        flow_type: "parameter".to_string(),
                        approximate: true,
                    });
                }
                if !cursor.goto_next_sibling() {
                    break;
                }
                arg_idx += 1;
                continue;
            }

            if tracked_names.iter().any(|tracked| arg_text == *tracked) {
                arg_positions.push((arg_idx, arg_text));
            }

            arg_idx += 1;
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }

    if arg_positions.is_empty() {
        return Ok(());
    }

    let matched_call = current_calls
        .iter()
        .find(|call| call.matches_position(node.start_byte(), node.end_byte()));

    match matched_call {
        Some(TraceForwardCall::Resolved(site)) => {
            let Some(target) = trace_target_node(store, site)? else {
                return Ok(());
            };
            if target.file != rel_file && current_depth + 1 > max_depth {
                *depth_limited = true;
                return Ok(());
            }
            let params = target
                .signature
                .as_deref()
                .map(|signature| callgraph::extract_parameters(signature, target.lang))
                .unwrap_or_default();
            let target_file = store.project_root().join(&target.file);
            for (pos, _tracked) in &arg_positions {
                if let Some(param_name) = params.get(*pos) {
                    hops.push(callgraph::DataFlowHop {
                        file: target.file.clone(),
                        symbol: target.symbol.clone(),
                        variable: param_name.clone(),
                        line: target.line,
                        flow_type: "parameter".to_string(),
                        approximate: false,
                    });
                    trace_data_inner(
                        store,
                        symbol_cache,
                        &target_file,
                        &target.symbol,
                        param_name,
                        max_depth,
                        current_depth + 1,
                        hops,
                        depth_limited,
                        visited,
                    )?;
                }
            }
        }
        Some(TraceForwardCall::Unresolved(call)) => {
            push_unresolved_parameter_hops(hops, rel_file, &call.symbol, &arg_positions, node);
        }
        None => {
            let (_full_callee, short_callee) = extract_callee_names(node, source);
            if let Some(callee_name) = short_callee {
                push_unresolved_parameter_hops(hops, rel_file, &callee_name, &arg_positions, node);
            }
        }
    }

    Ok(())
}

fn push_unresolved_parameter_hops(
    hops: &mut Vec<callgraph::DataFlowHop>,
    rel_file: &str,
    callee_name: &str,
    arg_positions: &[(usize, String)],
    call_node: Node<'_>,
) {
    for (_pos, tracked) in arg_positions {
        hops.push(callgraph::DataFlowHop {
            file: rel_file.to_string(),
            symbol: callee_name.to_string(),
            variable: tracked.clone(),
            line: call_node.start_position().row as u32 + 1,
            flow_type: "parameter".to_string(),
            approximate: true,
        });
    }
}

fn trace_target_node(
    store: &impl CallGraphRead,
    site: &StoreCallSite,
) -> StoreAdapterResult<Option<StoreNode>> {
    if let Some(target) = &site.target {
        return Ok(Some(target.clone()));
    }
    resolve_exact_symbol(store, &site.target_file, &site.target_symbol, None)
        .map(|resolved| resolved.map(|symbol| symbol.representative))
}

fn trace_forward_calls_for_nodes(
    store: &impl CallGraphRead,
    nodes: &[StoreNode],
) -> StoreAdapterResult<Vec<TraceForwardCall>> {
    let mut calls = Vec::new();
    for node in nodes {
        calls.extend(
            store
                .outgoing_calls_of(node)?
                .into_iter()
                .filter(|site| site.resolved_by() == TRACE_DATA_RESOLVER_PROVENANCE)
                .map(TraceForwardCall::Resolved),
        );
        calls.extend(
            store
                .resolved_self_calls_of(node)?
                .into_iter()
                .filter(|site| site.resolved_by() == TRACE_DATA_RESOLVER_PROVENANCE)
                .map(TraceForwardCall::Resolved),
        );
        calls.extend(
            store
                .unresolved_calls_of(node)?
                .into_iter()
                .map(TraceForwardCall::Unresolved),
        );
    }
    calls.sort_by(|left, right| {
        left.byte_start()
            .cmp(&right.byte_start())
            .then(left.byte_end().cmp(&right.byte_end()))
            .then(left.line().cmp(&right.line()))
    });
    Ok(calls)
}

fn resolve_symbol_query_with_cache(
    file: &Path,
    symbol: &str,
    symbol_cache: &SharedSymbolCache,
) -> StoreAdapterResult<String> {
    let mut parser = FileParser::with_symbol_cache(symbol_cache.clone());
    let symbols = parser.extract_symbols(file)?;
    let candidates = symbol_query_candidates_from_symbols(&symbols, symbol);
    match candidates.as_slice() {
        [candidate] => Ok(candidate.clone()),
        [] => Err(AftError::SymbolNotFound {
            name: symbol.to_string(),
            file: file.display().to_string(),
        }
        .into()),
        _ => Err(AftError::AmbiguousSymbol {
            name: symbol.to_string(),
            candidates,
        }
        .into()),
    }
}

fn symbol_query_candidates_from_symbols(symbols: &[Symbol], symbol_name: &str) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut candidates = Vec::new();
    let qualified_query = symbol_name.contains("::");

    let mut consider = |candidate: String| {
        let matches = if qualified_query {
            candidate == symbol_name
        } else {
            candidate == symbol_name || unqualified_name(&candidate) == symbol_name
        };
        if matches && seen.insert(candidate.clone()) {
            candidates.push(candidate);
        }
    };

    for symbol in symbols {
        consider(symbol_identity_from_cache(symbol));
        if symbol.exported {
            consider(symbol.name.clone());
        }
    }

    candidates.sort();
    candidates
}

fn symbol_identity_from_cache(symbol: &Symbol) -> String {
    if symbol.scope_chain.is_empty() {
        symbol.name.clone()
    } else {
        format!("{}::{}", symbol.scope_chain.join("::"), symbol.name)
    }
}

fn trace_node_text(node: Node<'_>, source: &str) -> String {
    source[node.start_byte()..node.end_byte()].to_string()
}

fn find_node_covering_range(root: Node<'_>, start: usize, end: usize) -> Option<Node<'_>> {
    let mut best = None;
    let mut cursor = root.walk();

    fn walk_covering<'a>(
        cursor: &mut tree_sitter::TreeCursor<'a>,
        start: usize,
        end: usize,
        best: &mut Option<Node<'a>>,
    ) {
        let node = cursor.node();
        if node.start_byte() <= start && node.end_byte() >= end {
            *best = Some(node);
            if cursor.goto_first_child() {
                loop {
                    walk_covering(cursor, start, end, best);
                    if !cursor.goto_next_sibling() {
                        break;
                    }
                }
                cursor.goto_parent();
            }
        }
    }

    walk_covering(&mut cursor, start, end, &mut best);
    best
}

fn find_child_by_kind<'tree>(node: Node<'tree>, kind: &str) -> Option<Node<'tree>> {
    let mut cursor = node.walk();
    if cursor.goto_first_child() {
        loop {
            if cursor.node().kind() == kind {
                return Some(cursor.node());
            }
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }
    None
}

fn extract_callee_names(node: Node<'_>, source: &str) -> (Option<String>, Option<String>) {
    let Some(callee) = node.child_by_field_name("function") else {
        return (None, None);
    };
    let full = trace_node_text(callee, source);
    let short = if full.contains('.') {
        full.rsplit('.').next().unwrap_or(&full).to_string()
    } else {
        full.clone()
    };
    (Some(full), Some(short))
}

pub fn store_error_response(req_id: &str, operation: &str, error: CallGraphStoreError) -> Response {
    match error {
        CallGraphStoreError::Aft(error) => Response::error(req_id, error.code(), error.to_string()),
        CallGraphStoreError::Unavailable(message) => Response::error(
            req_id,
            "callgraph_unavailable",
            format!("{operation}: persisted callgraph store unavailable: {message}"),
        ),
        CallGraphStoreError::StaleFiles(files) => Response::error(
            req_id,
            "callgraph_stale",
            format!(
                "{operation}: persisted callgraph store has stale files: {}",
                files.join(", ")
            ),
        ),
        other => Response::error(
            req_id,
            "callgraph_store_error",
            format!("{operation}: persisted callgraph store error: {other}"),
        ),
    }
}

/// The persisted callgraph store is cold-building in the background. The op did
/// not block the request thread; the agent should retry shortly. Mirrors how
/// semantic search reports a build in progress.
pub fn building_response(req_id: &str, operation: &str) -> Response {
    Response::error(
        req_id,
        "callgraph_building",
        format!("{operation}: callgraph store is building in the background; retry shortly"),
    )
}

pub fn unavailable_response(req_id: &str, operation: &str, worktree: bool) -> Response {
    let message = if worktree {
        format!(
            "{operation}: persisted callgraph store is unavailable in this read-only worktree; run a callgraph operation in the main checkout to build it first"
        )
    } else {
        format!("{operation}: project not configured — send 'configure' first")
    };
    let code = if worktree {
        "callgraph_unavailable"
    } else {
        "not_configured"
    };
    Response::error(req_id, code, message)
}

fn resolve_symbol_query(
    store: &impl CallGraphRead,
    file: &Path,
    symbol: &str,
) -> StoreAdapterResult<ResolvedStoreSymbol> {
    let nodes = store.nodes_for(file, symbol)?;
    collapse_symbol_nodes(store, file, symbol, nodes)
}

fn resolve_exact_symbol(
    store: &impl CallGraphRead,
    file: &str,
    symbol: &str,
    fallback: Option<StoreNode>,
) -> StoreAdapterResult<Option<ResolvedStoreSymbol>> {
    let nodes = store
        .nodes_for(Path::new(file), symbol)?
        .into_iter()
        .filter(|node| node.symbol == symbol)
        .collect::<Vec<_>>();
    if nodes.is_empty() {
        return Ok(fallback.map(|node| ResolvedStoreSymbol {
            representative: node.clone(),
            nodes: vec![node],
        }));
    }
    Ok(Some(collapse_exact_nodes(nodes)))
}

fn collapse_symbol_nodes(
    store: &impl CallGraphRead,
    file: &Path,
    query: &str,
    nodes: Vec<StoreNode>,
) -> StoreAdapterResult<ResolvedStoreSymbol> {
    let mut by_symbol: BTreeMap<String, Vec<StoreNode>> = BTreeMap::new();
    for node in nodes {
        by_symbol.entry(node.symbol.clone()).or_default().push(node);
    }

    match by_symbol.len() {
        0 => Err(CallGraphStoreError::Aft(AftError::SymbolNotFound {
            name: query.to_string(),
            file: display_file_for_error(store, file),
        })),
        1 => Ok(collapse_exact_nodes(
            by_symbol.into_values().next().unwrap_or_default(),
        )),
        _ => Err(CallGraphStoreError::Aft(AftError::AmbiguousSymbol {
            name: query.to_string(),
            candidates: by_symbol.into_keys().collect(),
        })),
    }
}

fn collapse_exact_nodes(mut nodes: Vec<StoreNode>) -> ResolvedStoreSymbol {
    nodes.sort_by(|left, right| {
        left.symbol
            .cmp(&right.symbol)
            .then(left.line.cmp(&right.line))
            .then(left.end_line.cmp(&right.end_line))
    });
    let representative = nodes[0].clone();
    ResolvedStoreSymbol {
        representative,
        nodes,
    }
}

#[allow(clippy::too_many_arguments)]
fn collect_callers_recursive(
    store: &impl CallGraphRead,
    file: &str,
    symbol: &str,
    max_depth: usize,
    current_depth: usize,
    visited: &mut HashSet<(String, String)>,
    result: &mut Vec<StoreCallSite>,
    depth_limited: &mut bool,
    truncated: &mut usize,
) -> StoreAdapterResult<()> {
    if current_depth >= max_depth {
        let omitted = dedup_call_site_count(store.direct_callers_of(Path::new(file), symbol)?);
        if omitted > 0 {
            *depth_limited = true;
            *truncated += omitted;
        }
        return Ok(());
    }

    if !visited.insert((file.to_string(), symbol.to_string())) {
        return Ok(());
    }

    let sites = store.direct_callers_of(Path::new(file), symbol)?;
    for site in sites {
        result.push(site.clone());
        if current_depth + 1 < max_depth {
            collect_callers_recursive(
                store,
                &site.caller.file,
                &site.caller.symbol,
                max_depth,
                current_depth + 1,
                visited,
                result,
                depth_limited,
                truncated,
            )?;
        } else {
            let omitted = dedup_call_site_count(
                store.direct_callers_of(Path::new(&site.caller.file), &site.caller.symbol)?,
            );
            if omitted > 0 {
                *depth_limited = true;
                *truncated += omitted;
            }
        }
    }
    Ok(())
}

fn call_tree_inner(
    store: &impl CallGraphRead,
    current: &ResolvedStoreSymbol,
    max_depth: usize,
    current_depth: usize,
    visited: &mut HashSet<(String, String)>,
) -> StoreAdapterResult<StoreCallTreeNode> {
    let node = &current.representative;
    let visit_key = (node.file.clone(), node.symbol.clone());
    if visited.contains(&visit_key) {
        return Ok(StoreCallTreeNode {
            name: node.symbol.clone(),
            file: node.file.clone(),
            line: node.line,
            signature: node.signature.clone(),
            resolved: true,
            approximate: None,
            resolved_by: None,
            children: Vec::new(),
            depth_limited: false,
            truncated: 0,
        });
    }
    visited.insert(visit_key.clone());

    let calls = forward_calls_for_nodes(store, &current.nodes)?;
    let mut children = Vec::new();
    let mut depth_limited = false;
    let mut truncated = 0usize;

    if current_depth < max_depth {
        for call in calls {
            match call {
                ForwardCall::Resolved(site) => {
                    let resolved = resolve_exact_symbol(
                        store,
                        &site.target_file,
                        &site.target_symbol,
                        site.target.clone(),
                    )?;
                    if let Some(child_symbol) = resolved {
                        let mut child = call_tree_inner(
                            store,
                            &child_symbol,
                            max_depth,
                            current_depth + 1,
                            visited,
                        )?;
                        child.approximate = edge_approximate(&site);
                        child.resolved_by = edge_resolved_by(&site);
                        depth_limited |= child.depth_limited;
                        truncated += child.truncated;
                        children.push(child);
                    } else {
                        children.push(StoreCallTreeNode {
                            name: site.target_symbol.clone(),
                            file: site.target_file.clone(),
                            line: site.line,
                            signature: None,
                            resolved: false,
                            approximate: edge_approximate(&site),
                            resolved_by: edge_resolved_by(&site),
                            children: Vec::new(),
                            depth_limited: false,
                            truncated: 0,
                        });
                    }
                }
                ForwardCall::Unresolved(call) => children.push(StoreCallTreeNode {
                    name: call.symbol,
                    file: call.caller.file,
                    line: call.line,
                    signature: None,
                    resolved: false,
                    approximate: None,
                    resolved_by: None,
                    children: Vec::new(),
                    depth_limited: false,
                    truncated: 0,
                }),
            }
        }
    } else if !calls.is_empty() {
        depth_limited = true;
        truncated = calls.len();
    }

    visited.remove(&visit_key);
    Ok(StoreCallTreeNode {
        name: node.symbol.clone(),
        file: node.file.clone(),
        line: node.line,
        signature: node.signature.clone(),
        resolved: true,
        approximate: None,
        resolved_by: None,
        children,
        depth_limited,
        truncated,
    })
}

fn forward_calls_for_nodes(
    store: &impl CallGraphRead,
    nodes: &[StoreNode],
) -> StoreAdapterResult<Vec<ForwardCall>> {
    let mut calls = Vec::new();
    for node in nodes {
        calls.extend(
            store
                .outgoing_calls_of(node)?
                .into_iter()
                .map(ForwardCall::Resolved),
        );
        calls.extend(
            store
                .unresolved_calls_of(node)?
                .into_iter()
                .map(ForwardCall::Unresolved),
        );
    }
    calls.sort_by(|left, right| {
        left.byte_start()
            .cmp(&right.byte_start())
            .then(left.line().cmp(&right.line()))
    });
    let mut seen = BTreeSet::new();
    calls.retain(|call| seen.insert(call.call_site_key()));
    Ok(calls)
}

fn forward_resolved_callees(
    store: &impl CallGraphRead,
    file: &str,
    symbol: &str,
) -> StoreAdapterResult<Vec<(StoreNode, EdgeMarker)>> {
    let Some(current) = resolve_exact_symbol(store, file, symbol, None)? else {
        return Ok(Vec::new());
    };
    let mut calls = Vec::new();
    for node in &current.nodes {
        calls.extend(store.outgoing_calls_of(node)?);
    }
    calls = dedup_call_sites(calls);
    calls.sort_by(|left, right| {
        left.byte_start
            .cmp(&right.byte_start)
            .then(left.line.cmp(&right.line))
    });

    let mut callees = Vec::new();
    for site in calls {
        let resolved = resolve_exact_symbol(
            store,
            &site.target_file,
            &site.target_symbol,
            site.target.clone(),
        )?;
        if let Some(target) = resolved {
            callees.push((target.representative, edge_marker(&site)));
        }
    }
    Ok(callees)
}

fn dedup_call_sites(sites: Vec<StoreCallSite>) -> Vec<StoreCallSite> {
    let mut seen = HashSet::new();
    let mut deduped = Vec::new();
    for site in sites {
        if seen.insert(call_site_key(&site)) {
            deduped.push(site);
        }
    }
    deduped
}

fn dedup_call_site_count(sites: Vec<StoreCallSite>) -> usize {
    sites
        .into_iter()
        .map(|site| call_site_key(&site))
        .collect::<HashSet<_>>()
        .len()
}

fn call_site_key(site: &StoreCallSite) -> (String, u32, String, String) {
    (
        site.caller.file.clone(),
        site.line,
        site.target_file.clone(),
        site.target_symbol.clone(),
    )
}

fn trace_to_symbol_hop(node: &StoreNode) -> StoreTraceToSymbolHop {
    trace_to_symbol_hop_with_edge(node, EdgeMarker::default())
}

fn trace_to_symbol_hop_with_edge(node: &StoreNode, edge: EdgeMarker) -> StoreTraceToSymbolHop {
    StoreTraceToSymbolHop {
        symbol: node.symbol.clone(),
        file: node.file.clone(),
        line: node.line,
        approximate: edge.approximate,
        resolved_by: edge.resolved_by,
    }
}

fn trace_to_symbol_matches_target(
    file: &str,
    symbol: &str,
    to_symbol: &str,
    to_file: Option<&str>,
) -> bool {
    if !(symbol == to_symbol || unqualified_name(symbol) == to_symbol) {
        return false;
    }
    match to_file {
        Some(target_file) => file == target_file,
        None => true,
    }
}

fn unqualified_name(symbol: &str) -> &str {
    symbol.rsplit("::").next().unwrap_or(symbol)
}

fn read_source_line(path: &Path, line: u32) -> Option<String> {
    let source = std::fs::read_to_string(path).ok()?;
    source
        .lines()
        .nth(line.saturating_sub(1) as usize)
        .map(|line| line.trim().to_string())
}

fn display_file_for_error(store: &impl CallGraphRead, file: &Path) -> String {
    absolute_file(store, file).display().to_string()
}

fn relative_file(store: &impl CallGraphRead, file: &Path) -> String {
    let absolute = absolute_file(store, file);
    absolute
        .strip_prefix(store.project_root())
        .unwrap_or(&absolute)
        .to_string_lossy()
        .replace('\\', "/")
}

fn absolute_file(store: &impl CallGraphRead, file: &Path) -> PathBuf {
    let full_path = if file.is_relative() {
        store.project_root().join(file)
    } else {
        file.to_path_buf()
    };
    std::fs::canonicalize(&full_path).unwrap_or(full_path)
}
