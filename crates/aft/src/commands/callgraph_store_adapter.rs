use std::collections::{BTreeMap, BTreeSet, HashSet, VecDeque};
use std::path::{Path, PathBuf};

use crate::callgraph::{
    self, CallTreeNode, CallerEntry, CallerGroup, CallersResult, ImpactCaller, ImpactResult,
    TraceHop, TracePath, TraceToResult, TraceToSymbolCandidate, TraceToSymbolHop,
    TraceToSymbolResult,
};
use crate::callgraph_store::{
    CallGraphStore, CallGraphStoreError, StoreCallSite, StoreNode, StoreUnresolvedCall,
};
use crate::error::AftError;
use crate::protocol::Response;

pub type StoreAdapterResult<T> = Result<T, CallGraphStoreError>;

enum ForwardCall {
    Resolved(StoreCallSite),
    Unresolved(StoreUnresolvedCall),
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
}

pub fn callers_result(
    store: &CallGraphStore,
    file: &Path,
    symbol: &str,
    depth: usize,
) -> StoreAdapterResult<CallersResult> {
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

    let sites = dedup_call_sites(sites);
    let total_callers = sites.len();
    let mut groups: BTreeMap<String, Vec<CallerEntry>> = BTreeMap::new();
    for site in sites {
        groups
            .entry(site.caller.file.clone())
            .or_default()
            .push(CallerEntry {
                symbol: site.caller.symbol,
                line: site.line,
            });
    }

    Ok(CallersResult {
        symbol: target.representative.symbol,
        file: target.representative.file,
        callers: groups
            .into_iter()
            .map(|(file, callers)| CallerGroup { file, callers })
            .collect(),
        total_callers,
        scanned_files: store.indexed_file_count()?,
        depth_limited,
        truncated,
    })
}

pub fn call_tree_result(
    store: &CallGraphStore,
    file: &Path,
    symbol: &str,
    depth: usize,
) -> StoreAdapterResult<CallTreeNode> {
    let target = resolve_symbol_query(store, file, symbol)?;
    let mut visited = HashSet::new();
    call_tree_inner(store, &target, depth, 0, &mut visited)
}

pub fn impact_result(
    store: &CallGraphStore,
    file: &Path,
    symbol: &str,
    depth: usize,
) -> StoreAdapterResult<ImpactResult> {
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

    let sites = dedup_call_sites(sites);
    let target_signature = target.representative.signature.clone();
    let target_parameters = target_signature
        .as_deref()
        .map(|signature| callgraph::extract_parameters(signature, target.representative.lang))
        .unwrap_or_default();

    let mut affected_files = BTreeSet::new();
    let mut callers = Vec::new();
    for site in sites {
        affected_files.insert(site.caller.file.clone());
        callers.push(ImpactCaller {
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
        });
    }
    callers.sort_by(|left, right| {
        left.caller_file
            .cmp(&right.caller_file)
            .then(left.line.cmp(&right.line))
    });

    Ok(ImpactResult {
        symbol: target.representative.symbol,
        file: target.representative.file,
        signature: target_signature,
        parameters: target_parameters,
        total_affected: callers.len(),
        affected_files: affected_files.len(),
        callers,
        depth_limited,
        truncated,
    })
}

pub fn trace_to_result(
    store: &CallGraphStore,
    file: &Path,
    symbol: &str,
    max_depth: usize,
) -> StoreAdapterResult<TraceToResult> {
    let target = resolve_symbol_query(store, file, symbol)?;
    let effective_max = if max_depth == 0 { 10 } else { max_depth };

    let initial = vec![TraceElem {
        node: target.representative.clone(),
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
            next_path.push(TraceElem {
                node: site.caller.clone(),
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

    let mut paths: Vec<TracePath> = complete_paths
        .into_iter()
        .map(|mut elems| {
            elems.reverse();
            let hops = elems
                .iter()
                .enumerate()
                .map(|(index, elem)| TraceHop {
                    symbol: elem.node.symbol.clone(),
                    file: elem.node.file.clone(),
                    line: elem.node.line,
                    signature: elem.node.signature.clone(),
                    is_entry_point: index == 0 && elem.node.is_entry_point,
                })
                .collect();
            TracePath { hops }
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
    let entry_points_found = paths
        .iter()
        .filter_map(|path| path.hops.first())
        .filter(|hop| hop.is_entry_point)
        .map(|hop| (hop.file.clone(), hop.symbol.clone()))
        .collect::<HashSet<_>>()
        .len();

    Ok(TraceToResult {
        target_symbol: target.representative.symbol,
        target_file: target.representative.file,
        total_paths: paths.len(),
        paths,
        entry_points_found,
        max_depth_reached,
        truncated_paths,
    })
}

pub fn ensure_symbol_resolves(
    store: &CallGraphStore,
    file: &Path,
    symbol: &str,
) -> StoreAdapterResult<()> {
    resolve_symbol_query(store, file, symbol).map(|_| ())
}

pub fn trace_to_symbol_candidates(
    store: &CallGraphStore,
    to_symbol: &str,
) -> StoreAdapterResult<Vec<TraceToSymbolCandidate>> {
    store.trace_to_symbol_candidates(to_symbol)
}

pub fn trace_to_symbol_result(
    store: &CallGraphStore,
    file: &Path,
    symbol: &str,
    to_symbol: &str,
    to_file: Option<&Path>,
    max_depth: usize,
) -> StoreAdapterResult<TraceToSymbolResult> {
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
        return Ok(TraceToSymbolResult {
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
                .any(|node| !visited.contains(&(node.file.clone(), node.symbol.clone())))
            {
                max_depth_exhausted = true;
            }
            continue;
        }

        for callee in callees {
            if !visited.insert((callee.file.clone(), callee.symbol.clone())) {
                continue;
            }
            let mut next_path = path.clone();
            next_path.push(trace_to_symbol_hop(&callee));
            if trace_to_symbol_matches_target(
                &callee.file,
                &callee.symbol,
                to_symbol,
                target_file.as_deref(),
            ) {
                return Ok(TraceToSymbolResult {
                    path: Some(next_path),
                    complete: true,
                    reason: None,
                });
            }
            queue.push_back((callee.file, callee.symbol, next_path, depth + 1));
        }
    }

    if max_depth_exhausted {
        Ok(TraceToSymbolResult {
            path: None,
            complete: false,
            reason: Some("max_depth_exhausted".to_string()),
        })
    } else {
        Ok(TraceToSymbolResult {
            path: None,
            complete: true,
            reason: Some("no_path_found".to_string()),
        })
    }
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
    store: &CallGraphStore,
    file: &Path,
    symbol: &str,
) -> StoreAdapterResult<ResolvedStoreSymbol> {
    let nodes = store.nodes_for(file, symbol)?;
    collapse_symbol_nodes(store, file, symbol, nodes)
}

fn resolve_exact_symbol(
    store: &CallGraphStore,
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
    store: &CallGraphStore,
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
    store: &CallGraphStore,
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
    store: &CallGraphStore,
    current: &ResolvedStoreSymbol,
    max_depth: usize,
    current_depth: usize,
    visited: &mut HashSet<(String, String)>,
) -> StoreAdapterResult<CallTreeNode> {
    let node = &current.representative;
    let visit_key = (node.file.clone(), node.symbol.clone());
    if visited.contains(&visit_key) {
        return Ok(CallTreeNode {
            name: node.symbol.clone(),
            file: node.file.clone(),
            line: node.line,
            signature: node.signature.clone(),
            resolved: true,
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
                        let child = call_tree_inner(
                            store,
                            &child_symbol,
                            max_depth,
                            current_depth + 1,
                            visited,
                        )?;
                        depth_limited |= child.depth_limited;
                        truncated += child.truncated;
                        children.push(child);
                    } else {
                        children.push(CallTreeNode {
                            name: site.target_symbol,
                            file: site.target_file,
                            line: site.line,
                            signature: None,
                            resolved: false,
                            children: Vec::new(),
                            depth_limited: false,
                            truncated: 0,
                        });
                    }
                }
                ForwardCall::Unresolved(call) => children.push(CallTreeNode {
                    name: call.symbol,
                    file: call.caller.file,
                    line: call.line,
                    signature: None,
                    resolved: false,
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
    Ok(CallTreeNode {
        name: node.symbol.clone(),
        file: node.file.clone(),
        line: node.line,
        signature: node.signature.clone(),
        resolved: true,
        children,
        depth_limited,
        truncated,
    })
}

fn forward_calls_for_nodes(
    store: &CallGraphStore,
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
    store: &CallGraphStore,
    file: &str,
    symbol: &str,
) -> StoreAdapterResult<Vec<StoreNode>> {
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
            callees.push(target.representative);
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

fn trace_to_symbol_hop(node: &StoreNode) -> TraceToSymbolHop {
    TraceToSymbolHop {
        symbol: node.symbol.clone(),
        file: node.file.clone(),
        line: node.line,
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

fn display_file_for_error(store: &CallGraphStore, file: &Path) -> String {
    absolute_file(store, file).display().to_string()
}

fn relative_file(store: &CallGraphStore, file: &Path) -> String {
    let absolute = absolute_file(store, file);
    absolute
        .strip_prefix(store.project_root())
        .unwrap_or(&absolute)
        .to_string_lossy()
        .replace('\\', "/")
}

fn absolute_file(store: &CallGraphStore, file: &Path) -> PathBuf {
    let full_path = if file.is_relative() {
        store.project_root().join(file)
    } else {
        file.to_path_buf()
    };
    std::fs::canonicalize(&full_path).unwrap_or(full_path)
}
