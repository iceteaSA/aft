use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::{Map, Value};

use crate::context::AppContext;
use crate::inspect::diagnostics_category::run_diagnostics_category;
use crate::inspect::{InspectCache, InspectCategory, InspectSnapshot, JobOutcome, JobScope};
use crate::protocol::{RawRequest, Response};

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

    let scope_was_provided = scope_was_provided(req.params.get("scope"));
    let snapshot = match build_snapshot(ctx) {
        Ok(snapshot) => snapshot,
        Err(response) => return response.with_id(&req.id),
    };
    let scope = match parse_scope(req, ctx, &snapshot.project_root) {
        Ok(scope) => scope,
        Err(response) => return response,
    };

    // Callgraph snapshots are only built inside background Tier 2 jobs.
    // handle_inspect itself is fully read-only for Tier 2 (cache hit only), and
    // Tier 1 (todos, metrics) does not consume the callgraph snapshot.
    let manager = ctx.inspect_manager();
    let mut outcomes = BTreeMap::new();
    let mut tier2_refresh_needed = false;
    for category in InspectCategory::active() {
        let outcome = if *category == InspectCategory::Diagnostics {
            // Diagnostics are backed by the AppContext LSP manager (RefCell, not
            // Send/Sync), so they must be computed on this main dispatch thread.
            // Do not send them through InspectManager's rayon worker path.
            run_diagnostics_category(ctx, &snapshot, &scope, scope_was_provided)
        } else if category.is_tier2() {
            // Tier 2 (dead_code, unused_exports, duplicates) are NEVER scanned
            // synchronously here. handle_inspect just returns whatever aggregate
            // the last Tier 2 run persisted, or Pending if nothing is cached yet.
            let outcome =
                manager.tier2_read_cached_readonly(snapshot.clone(), *category, scope.clone());
            if !ctx.is_worktree_bridge()
                && matches!(
                    outcome,
                    JobOutcome::Stale { .. } | JobOutcome::Pending { .. }
                )
            {
                tier2_refresh_needed = true;
            }
            outcome
        } else {
            manager.submit_category_with_callgraph(snapshot.clone(), *category, scope.clone(), None)
        };
        outcomes.insert(*category, outcome);
    }

    if tier2_refresh_needed {
        ctx.request_tier2_refresh_pull();
    }

    let payload = build_inspect_payload(&snapshot, &outcomes, &sections, top_k, ctx);
    Response::success(&req.id, payload)
}

pub fn handle_inspect_tier2_run(req: &RawRequest, ctx: &AppContext) -> Response {
    let categories = match parse_tier2_categories(req.params.get("categories")) {
        Ok(categories) => categories,
        Err(message) => return invalid_request(&req.id, message),
    };

    if ctx.is_worktree_bridge() {
        let skipped = categories
            .iter()
            .map(|category| {
                serde_json::json!({
                    "category": category.as_str(),
                    "reason": "worktree_bridge_read_only",
                })
            })
            .collect::<Vec<_>>();
        return Response::success(
            &req.id,
            serde_json::json!({
                "queued_categories": [],
                "in_flight_categories": [],
                "errors": [],
                "skipped_categories": skipped,
            }),
        );
    }

    let snapshot = match build_snapshot(ctx) {
        Ok(snapshot) => snapshot,
        Err(response) => return response.with_id(&req.id),
    };
    let manager = ctx.inspect_manager();
    let submission = manager.submit_tier2_run_with_reuse_serial_background(snapshot, categories);
    if submission.has_new_work() {
        ctx.note_tier2_refresh_started();
    }

    let queued = submission
        .queued_categories
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    let errors = submission
        .errors
        .iter()
        .map(|error| {
            serde_json::json!({
                "category": error.category.as_str(),
                "message": error.message.as_str(),
            })
        })
        .collect::<Vec<_>>();

    Response::success(
        &req.id,
        serde_json::json!({
            "queued_categories": queued.clone(),
            "in_flight_categories": queued,
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

fn scope_was_provided(value: Option<&Value>) -> bool {
    let Some(value) = value else {
        return false;
    };
    !(value.is_null() || empty_string(value) || empty_array(value))
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
    ctx: &AppContext,
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
        if *category == InspectCategory::Diagnostics
            && diagnostics_payload_status(payload) == Some("pending")
            && !pending_categories
                .iter()
                .any(|value| value == category.as_str())
        {
            pending_categories.push(category.as_str().to_string());
        }
        summary.insert(
            category.as_str().to_string(),
            summary_for(*category, outcome),
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
    let tier2_last_run = tier2_last_run(snapshot);

    let mut payload = serde_json::json!({
        "summary": Value::Object(summary),
        "scanner_state": {
            "tier2_last_run": tier2_last_run,
            "tier2_trigger_reason": ctx.tier2_trigger_reason(),
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

fn summary_for(category: InspectCategory, outcome: Option<&JobOutcome>) -> Value {
    let Some(outcome) = outcome else {
        return status_summary("pending");
    };
    if let Some(status) = outcome.summary_status() {
        return status_summary(status);
    }

    computed_summary_for(category, outcome.payload())
}

fn status_summary(status: &'static str) -> Value {
    serde_json::json!({ "status": status })
}

fn computed_summary_for(category: InspectCategory, payload: Option<&Value>) -> Value {
    match category {
        InspectCategory::Diagnostics => diagnostics_summary_for(payload),
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

fn diagnostics_payload_status(payload: Option<&Value>) -> Option<&str> {
    payload
        .and_then(|payload| payload.get("status"))
        .and_then(Value::as_str)
}

fn diagnostics_summary_for(payload: Option<&Value>) -> Value {
    let Some(payload) = payload else {
        return status_summary("pending");
    };

    let complete = payload
        .get("complete")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let server_ran = payload
        .get("server_ran")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    if complete && server_ran {
        return serde_json::json!({
            "errors": payload.get("errors").and_then(Value::as_u64).unwrap_or(0),
            "warnings": payload.get("warnings").and_then(Value::as_u64).unwrap_or(0),
            "info": payload.get("info").and_then(Value::as_u64).unwrap_or(0),
            "hints": payload.get("hints").and_then(Value::as_u64).unwrap_or(0),
        });
    }

    // Public diagnostics summary contract for the plugin layer:
    //   complete: { errors, warnings, info, hints }
    //   partial:  { errors, warnings, info, hints,
    //               status: "pending"|"incomplete", servers_pending, servers_not_installed }
    //
    // The partial shape ALWAYS carries the counts found SO FAR alongside the
    // status/gap fields. Hiding already-collected diagnostics behind a bare
    // "pending" sentinel was dishonest the other direction: a scoped pull could
    // have real errors from one server while another server is still pending,
    // and an agent reading only the summary would miss them. The presence of
    // `status` tells the agent the counts are not yet the full picture, so a
    // `0` count here is never misread as "clean".
    serde_json::json!({
        "errors": payload.get("errors").and_then(Value::as_u64).unwrap_or(0),
        "warnings": payload.get("warnings").and_then(Value::as_u64).unwrap_or(0),
        "info": payload.get("info").and_then(Value::as_u64).unwrap_or(0),
        "hints": payload.get("hints").and_then(Value::as_u64).unwrap_or(0),
        "status": payload
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("pending"),
        "servers_pending": payload
            .get("servers_pending")
            .cloned()
            .unwrap_or_else(|| serde_json::json!([])),
        "servers_not_installed": payload
            .get("servers_not_installed")
            .cloned()
            .unwrap_or_else(|| serde_json::json!([])),
        "files_without_server": payload
            .get("files_without_server")
            .and_then(Value::as_u64)
            .unwrap_or(0),
    })
}

fn details_for(category: InspectCategory, payload: Option<&Value>, top_k: usize) -> Value {
    if category == InspectCategory::Metrics {
        return computed_summary_for(category, payload);
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

fn tier2_last_run(snapshot: &InspectSnapshot) -> Option<i64> {
    let cache =
        InspectCache::open_readonly(snapshot.inspect_dir.clone(), snapshot.project_root.clone())
            .ok()
            .flatten()?;
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
