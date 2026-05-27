use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use serde_json::{Map, Value};

use crate::callgraph::EdgeResolution;
use crate::context::AppContext;
use crate::inspect::{
    CallgraphExport, CallgraphOutboundCall, CallgraphSnapshot, InspectCategory, InspectSnapshot,
    JobOutcome, JobScope,
};
use crate::protocol::{RawRequest, Response};
use crate::symbols::SymbolKind;

const DEFAULT_TOP_K: usize = 20;
const MAX_TOP_K: usize = 100;

pub fn handle_inspect(req: &RawRequest, ctx: &AppContext) -> Response {
    let top_k = match parse_top_k(&req.params) {
        Ok(top_k) => top_k,
        Err(message) => return invalid_request(&req.id, message),
    };
    let sections = match parse_sections(req.params.get("sections")) {
        Ok(sections) => sections,
        Err(message) => return invalid_request(&req.id, message),
    };

    let snapshot = match build_snapshot(ctx) {
        Ok(snapshot) => snapshot,
        Err(response) => return response.with_id(&req.id),
    };
    let scope = match parse_scope(req, ctx, &snapshot.project_root) {
        Ok(scope) => scope,
        Err(response) => return response,
    };

    let callgraph_snapshot = build_callgraph_snapshot(ctx, &snapshot.project_root);
    let manager = ctx.inspect_manager();
    let mut outcomes = BTreeMap::new();
    for category in InspectCategory::active() {
        let outcome = manager.submit_category_with_callgraph(
            snapshot.clone(),
            *category,
            scope.clone(),
            callgraph_snapshot.clone(),
        );
        outcomes.insert(*category, outcome);
    }

    let payload = build_inspect_payload(&snapshot, &outcomes, &sections, top_k, &manager);
    Response::success(&req.id, payload)
}

pub fn handle_inspect_tier2_run(req: &RawRequest, ctx: &AppContext) -> Response {
    let categories = match parse_tier2_categories(req.params.get("categories")) {
        Ok(categories) => categories,
        Err(message) => return invalid_request(&req.id, message),
    };
    let snapshot = match build_snapshot(ctx) {
        Ok(snapshot) => snapshot,
        Err(response) => return response.with_id(&req.id),
    };
    let scope = JobScope::for_project(snapshot.project_root.clone());
    let callgraph_snapshot = build_callgraph_snapshot(ctx, &snapshot.project_root);
    let manager = ctx.inspect_manager();

    let mut queued = Vec::new();
    let mut errors = Vec::new();
    for category in categories {
        match manager.submit_background_with_callgraph(
            snapshot.clone(),
            category,
            scope.clone(),
            callgraph_snapshot.clone(),
        ) {
            Ok(key) => queued.push(key.display_key()),
            Err(message) => errors.push(serde_json::json!({
                "category": category.as_str(),
                "message": message,
            })),
        }
    }

    Response::success(
        &req.id,
        serde_json::json!({
            "queued_categories": queued,
            "errors": errors,
        }),
    )
}

trait ResponseIdExt {
    fn with_id(self, id: &str) -> Self;
}

impl ResponseIdExt for Response {
    fn with_id(mut self, id: &str) -> Self {
        self.id = id.to_string();
        self
    }
}

#[derive(Debug, Clone)]
struct Sections {
    detail_categories: BTreeSet<InspectCategory>,
}

impl Sections {
    fn summary_only() -> Self {
        Self {
            detail_categories: BTreeSet::new(),
        }
    }

    fn all() -> Self {
        Self {
            detail_categories: InspectCategory::active().iter().copied().collect(),
        }
    }

    fn includes(&self, category: InspectCategory) -> bool {
        self.detail_categories.contains(&category)
    }
}

fn build_snapshot(ctx: &AppContext) -> Result<InspectSnapshot, Response> {
    if ctx.harness_opt().is_none() {
        return Err(Response::error(
            "inspect",
            "not_configured",
            "inspect: configure must run before aft_inspect so the harness-scoped cache path is known",
        ));
    }

    let config = ctx.config().clone();
    let project_root = config
        .project_root
        .clone()
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    let project_root = std::fs::canonicalize(&project_root).unwrap_or(project_root);
    Ok(InspectSnapshot::new(
        project_root,
        ctx.inspect_dir(),
        Arc::new(config),
        ctx.symbol_cache(),
    ))
}

fn build_callgraph_snapshot(
    ctx: &AppContext,
    project_root: &Path,
) -> Option<Arc<CallgraphSnapshot>> {
    let mut graph_ref = ctx.callgraph().borrow_mut();
    let graph = graph_ref.as_mut()?;
    let graph_files = graph.project_files().to_vec();
    let files = graph_files
        .iter()
        .map(canonicalize_for_snapshot)
        .collect::<Vec<_>>();

    let mut exported_symbols = Vec::new();
    let mut outbound_calls = Vec::new();
    let mut entry_points = BTreeSet::new();

    for file in &graph_files {
        let snapshot_file = canonicalize_for_snapshot(file);
        if is_entry_point_file(project_root, &snapshot_file) {
            entry_points.insert(snapshot_file.clone());
        }

        let file_data = match graph.build_file(file) {
            Ok(file_data) => file_data.clone(),
            Err(_) => continue,
        };

        for symbol in &file_data.exported_symbols {
            let metadata = file_data.symbol_metadata.get(symbol);
            exported_symbols.push(CallgraphExport {
                file: snapshot_file.clone(),
                symbol: symbol.clone(),
                kind: metadata
                    .map(|metadata| symbol_kind_name(&metadata.kind))
                    .unwrap_or("unknown")
                    .to_string(),
                line: metadata.map(|metadata| metadata.line).unwrap_or(1),
            });
        }

        for calls in file_data.calls_by_symbol.values() {
            for call in calls {
                let target = match graph.resolve_cross_file_edge(
                    &call.full_callee,
                    &call.callee_name,
                    file,
                    &file_data.import_block,
                ) {
                    EdgeResolution::Resolved { file, symbol } => {
                        let file = canonicalize_for_snapshot(&file);
                        format!("{}::{symbol}", file.display())
                    }
                    EdgeResolution::Unresolved { callee_name } => callee_name,
                };
                outbound_calls.push(CallgraphOutboundCall {
                    caller_file: snapshot_file.clone(),
                    target,
                    line: call.line,
                });
            }
        }
    }

    Some(Arc::new(CallgraphSnapshot {
        generated_at: Some(SystemTime::now()),
        files,
        exported_symbols,
        outbound_calls,
        entry_points,
    }))
}

fn canonicalize_for_snapshot(path: &PathBuf) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.clone())
}

fn is_entry_point_file(project_root: &Path, file: &Path) -> bool {
    let relative = file.strip_prefix(project_root).unwrap_or(file);
    let relative_display = relative.to_string_lossy().replace('\\', "/");
    if relative_display.starts_with("bin/") || relative_display.contains("/bin/") {
        return true;
    }

    let Some(file_name) = relative.file_name().and_then(|name| name.to_str()) else {
        return false;
    };

    matches!(
        file_name,
        "main.rs"
            | "main.ts"
            | "main.tsx"
            | "main.js"
            | "main.jsx"
            | "main.py"
            | "main.go"
            | "index.ts"
            | "index.tsx"
            | "index.js"
            | "index.jsx"
    ) || (file_name == "lib.rs" && project_root.join("Cargo.toml").exists())
}

fn symbol_kind_name(kind: &SymbolKind) -> &'static str {
    match kind {
        SymbolKind::Function => "function",
        SymbolKind::Class => "class",
        SymbolKind::Method => "method",
        SymbolKind::Struct => "struct",
        SymbolKind::Interface => "interface",
        SymbolKind::Enum => "enum",
        SymbolKind::TypeAlias => "type_alias",
        SymbolKind::Variable => "variable",
        SymbolKind::Heading => "heading",
        SymbolKind::FileSummary => "file_summary",
    }
}

fn parse_top_k(params: &Value) -> Result<usize, String> {
    let Some(value) = params.get("topK").or_else(|| params.get("top_k")) else {
        return Ok(DEFAULT_TOP_K);
    };
    if value.is_null() || empty_string(value) {
        return Ok(DEFAULT_TOP_K);
    }
    let Some(top_k) = value.as_u64() else {
        return Err("inspect: topK must be a positive integer".to_string());
    };
    if top_k == 0 {
        return Err("inspect: topK must be greater than 0".to_string());
    }
    Ok((top_k as usize).min(MAX_TOP_K))
}

fn parse_sections(value: Option<&Value>) -> Result<Sections, String> {
    let Some(value) = value else {
        return Ok(Sections::summary_only());
    };
    if value.is_null() || empty_string(value) || empty_array(value) {
        return Ok(Sections::summary_only());
    }

    let mut categories = BTreeSet::new();
    match value {
        Value::String(section) => add_section(section, &mut categories)?,
        Value::Array(sections) => {
            for section in sections {
                if section.is_null() || empty_string(section) {
                    continue;
                }
                let Some(section) = section.as_str() else {
                    return Err("inspect: sections array entries must be strings".to_string());
                };
                add_section(section, &mut categories)?;
            }
        }
        _ => return Err("inspect: sections must be a string or string array".to_string()),
    }

    if categories.len() == InspectCategory::active().len() {
        Ok(Sections::all())
    } else {
        Ok(Sections {
            detail_categories: categories,
        })
    }
}

fn add_section(section: &str, categories: &mut BTreeSet<InspectCategory>) -> Result<(), String> {
    let section = section.trim();
    if section.is_empty() {
        return Ok(());
    }
    if section == "all" {
        categories.extend(InspectCategory::active().iter().copied());
        return Ok(());
    }
    let category = section
        .parse::<InspectCategory>()
        .map_err(|error| format!("inspect: {error}"))?;
    if !category.is_active() {
        return Err(format!(
            "inspect: category '{category}' is registered but disabled in v0.33"
        ));
    }
    categories.insert(category);
    Ok(())
}

fn parse_tier2_categories(value: Option<&Value>) -> Result<Vec<InspectCategory>, String> {
    let sections = parse_sections(value)?.detail_categories;
    let categories = if sections.is_empty() {
        InspectCategory::active()
            .iter()
            .copied()
            .filter(|category| category.is_tier2())
            .collect::<Vec<_>>()
    } else {
        sections
            .into_iter()
            .filter(|category| category.is_tier2())
            .collect::<Vec<_>>()
    };
    Ok(categories)
}

fn parse_scope(
    req: &RawRequest,
    ctx: &AppContext,
    project_root: &Path,
) -> Result<JobScope, Response> {
    let Some(value) = req.params.get("scope") else {
        return Ok(JobScope::for_project(project_root.to_path_buf()));
    };
    if value.is_null() || empty_string(value) || empty_array(value) {
        return Ok(JobScope::for_project(project_root.to_path_buf()));
    }

    let raw_scopes = match value {
        Value::String(scope) => vec![scope.clone()],
        Value::Array(scopes) => {
            let mut values = Vec::new();
            for scope in scopes {
                if scope.is_null() || empty_string(scope) {
                    continue;
                }
                let Some(scope) = scope.as_str() else {
                    return Err(Response::error(
                        &req.id,
                        "invalid_request",
                        "inspect: scope array entries must be strings",
                    ));
                };
                values.push(scope.to_string());
            }
            values
        }
        _ => {
            return Err(Response::error(
                &req.id,
                "invalid_request",
                "inspect: scope must be a string or string array",
            ));
        }
    };

    let mut roots = Vec::new();
    for scope in raw_scopes {
        let raw_path = PathBuf::from(scope);
        let candidate = if raw_path.is_absolute() {
            raw_path
        } else {
            project_root.join(raw_path)
        };
        let validated = ctx.validate_path(&req.id, &candidate)?;
        roots.push(std::fs::canonicalize(&validated).unwrap_or(validated));
    }

    Ok(JobScope::from_roots(project_root.to_path_buf(), roots))
}

fn build_inspect_payload(
    snapshot: &InspectSnapshot,
    outcomes: &BTreeMap<InspectCategory, JobOutcome>,
    sections: &Sections,
    top_k: usize,
    manager: &crate::inspect::InspectManager,
) -> Value {
    let mut summary = Map::new();
    let mut details = Map::new();
    let mut stale_categories = Vec::new();
    let mut pending_categories = Vec::new();
    let mut failed_categories = Vec::new();

    for category in InspectCategory::active() {
        let outcome = outcomes.get(category);
        if outcome.is_some_and(JobOutcome::is_stale) {
            stale_categories.push(category.as_str().to_string());
        }
        if outcome.is_some_and(JobOutcome::is_pending) {
            pending_categories.push(category.as_str().to_string());
        }
        if let Some(JobOutcome::Failed { message }) = outcome {
            failed_categories.push(serde_json::json!({
                "category": category.as_str(),
                "message": message,
            }));
        }

        let payload = outcome.and_then(JobOutcome::payload);
        summary.insert(
            category.as_str().to_string(),
            summary_for(*category, payload),
        );
        if sections.includes(*category) {
            details.insert(
                category.as_str().to_string(),
                details_for(*category, payload, top_k),
            );
        }
    }

    let disabled_categories = InspectCategory::disabled()
        .iter()
        .map(|category| category.as_str())
        .collect::<Vec<_>>();
    let tier2_last_run = tier2_last_run(snapshot, manager);

    let mut payload = serde_json::json!({
        "summary": Value::Object(summary),
        "scanner_state": {
            "tier2_last_run": tier2_last_run,
            "stale_categories": stale_categories,
            "disabled_categories": disabled_categories,
            "pending_categories": pending_categories,
            "failed_categories": failed_categories,
        }
    });
    if !details.is_empty() {
        payload["details"] = Value::Object(details);
    }
    payload
}

fn summary_for(category: InspectCategory, payload: Option<&Value>) -> Value {
    match category {
        InspectCategory::Diagnostics => serde_json::json!({
            "errors": payload.and_then(|p| p.get("errors")).and_then(Value::as_u64).unwrap_or(0),
            "warnings": payload.and_then(|p| p.get("warnings")).and_then(Value::as_u64).unwrap_or(0),
            "pending_servers": payload.and_then(|p| p.get("pending_servers")).cloned().unwrap_or_else(|| serde_json::json!([])),
        }),
        InspectCategory::Metrics => serde_json::json!({
            "files": payload.and_then(|p| p.get("files").or_else(|| p.pointer("/totals/file_count"))).and_then(Value::as_u64).unwrap_or(0),
            "symbols": payload.and_then(|p| p.get("symbols").or_else(|| p.pointer("/totals/symbol_count"))).and_then(Value::as_u64).unwrap_or(0),
            "loc": payload.and_then(|p| p.get("loc").or_else(|| p.pointer("/totals/loc"))).and_then(Value::as_u64).unwrap_or(0),
        }),
        InspectCategory::Todos => serde_json::json!({
            "count": count_from_payload(payload),
            "by_kind": payload.and_then(|p| p.get("by_kind").or_else(|| p.get("by_marker"))).cloned().unwrap_or_else(|| serde_json::json!({})),
        }),
        InspectCategory::DeadCode => serde_json::json!({
            "count": count_from_payload(payload),
            "by_language": payload.and_then(|p| p.get("by_language")).cloned().unwrap_or_else(|| serde_json::json!({})),
        }),
        InspectCategory::UnusedExports => serde_json::json!({
            "count": count_from_payload(payload),
        }),
        InspectCategory::Duplicates => serde_json::json!({
            "count": count_from_payload(payload),
            "total_groups": payload.and_then(|p| p.get("total_groups").or_else(|| p.get("groups_count"))).and_then(Value::as_u64).unwrap_or_else(|| count_from_payload(payload)),
        }),
        _ => serde_json::json!({ "count": count_from_payload(payload) }),
    }
}

fn details_for(category: InspectCategory, payload: Option<&Value>, top_k: usize) -> Value {
    if category == InspectCategory::Metrics {
        return summary_for(category, payload);
    }
    let Some(payload) = payload else {
        return serde_json::json!([]);
    };
    let items = payload
        .get("items")
        .or_else(|| payload.get("groups"))
        .and_then(Value::as_array);
    match items {
        Some(items) => Value::Array(items.iter().take(top_k).cloned().collect()),
        None => serde_json::json!([]),
    }
}

fn count_from_payload(payload: Option<&Value>) -> u64 {
    payload
        .and_then(|payload| payload.get("count"))
        .and_then(Value::as_u64)
        .unwrap_or(0)
}

fn tier2_last_run(
    snapshot: &InspectSnapshot,
    manager: &crate::inspect::InspectManager,
) -> Option<i64> {
    let cache = manager.cache_for_snapshot(snapshot).ok()?;
    InspectCategory::active()
        .iter()
        .copied()
        .filter(|category| category.is_tier2())
        .filter_map(|category| cache.last_full_run(category).ok().flatten())
        .max()
}

fn empty_string(value: &Value) -> bool {
    value.as_str().is_some_and(|value| value.trim().is_empty())
}

fn empty_array(value: &Value) -> bool {
    value.as_array().is_some_and(|value| value.is_empty())
}

fn invalid_request(id: &str, message: String) -> Response {
    Response::error(id, "invalid_request", message)
}
