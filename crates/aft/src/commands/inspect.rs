use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::{Map, Value};

use crate::context::AppContext;
use crate::inspect::diagnostics_category::run_diagnostics_category;
use crate::inspect::{
    DirectTier2RunOutcome, InspectCache, InspectCategory, InspectSnapshot, JobOutcome, JobScope,
};
use crate::protocol::{RawRequest, Response};

const DEFAULT_TOP_K: usize = 20;
const MAX_TOP_K: usize = 100;
const DIRECT_TIER2_WAIT_BUDGET: Duration = Duration::from_secs(25);

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

    let manager = ctx.inspect_manager();
    let tier2_deadline = Instant::now() + direct_tier2_wait_budget(&snapshot.project_root);
    let pending_tier2_paths = ctx.pending_tier2_paths();
    let mut tier2_receivers = BTreeMap::new();
    for category in [
        InspectCategory::DeadCode,
        InspectCategory::UnusedExports,
        InspectCategory::Duplicates,
        InspectCategory::Cycles,
    ] {
        if !ctx.inspect_writer() {
            continue;
        }
        let manager = manager.clone();
        let snapshot = snapshot.clone();
        let scope = scope.clone();
        let force_paths = pending_tier2_paths.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(manager.tier2_run_with_reuse_direct(
                snapshot,
                category,
                scope,
                tier2_deadline,
                force_paths,
            ));
        });
        tier2_receivers.insert(category, rx);
    }

    let mut force_paths_completed = BTreeSet::new();
    let mut outcomes = BTreeMap::new();
    let mut tier2_refresh_needed = false;
    for category in InspectCategory::active() {
        let outcome = if *category == InspectCategory::Diagnostics {
            // Diagnostics are backed by the AppContext LSP manager and must stay
            // on the serial LSP/status lane. Do not send them through
            // InspectManager's rayon worker path.
            run_diagnostics_category(ctx, &snapshot, &scope, scope_was_provided)
        } else if category.is_tier2() {
            let direct = if let Some(rx) = tier2_receivers.remove(category) {
                receive_direct_tier2(rx, tier2_deadline)
            } else {
                DirectTier2RunOutcome {
                    outcome: manager.tier2_read_cached_readonly(
                        snapshot.clone(),
                        *category,
                        scope.clone(),
                    ),
                    force_paths_completed: false,
                }
            };
            if direct.force_paths_completed && matches!(direct.outcome, JobOutcome::Fresh { .. }) {
                force_paths_completed.insert(*category);
            }
            if matches!(direct.outcome, JobOutcome::Pending { .. }) {
                tier2_refresh_needed = true;
            }
            direct.outcome
        } else {
            manager.submit_category_with_callgraph(snapshot.clone(), *category, scope.clone(), None)
        };
        outcomes.insert(*category, outcome);
    }

    if !pending_tier2_paths.is_empty()
        && [
            InspectCategory::DeadCode,
            InspectCategory::UnusedExports,
            InspectCategory::Duplicates,
            InspectCategory::Cycles,
        ]
        .iter()
        .all(|category| force_paths_completed.contains(category))
    {
        ctx.remove_pending_tier2_paths(pending_tier2_paths);
    }

    if tier2_refresh_needed {
        ctx.request_tier2_refresh_pull();
    }

    refresh_status_bar_counts(ctx, &outcomes);

    let payload = build_inspect_payload(&snapshot, &outcomes, &sections, top_k, ctx);
    Response::success(&req.id, payload)
}

/// Refresh the agent status-bar's last-known Tier-2 + todos counts from the
/// outcomes just computed. Tier-2 counts come from whatever the read-only cache
/// returned (Fresh or Stale-cached); a Stale/Pending outcome marks them stale
/// so the bar renders the `~` marker. Errors/warnings are NOT touched here —
/// they're read live from the LSP store at attach time.
fn refresh_status_bar_counts(ctx: &AppContext, outcomes: &BTreeMap<InspectCategory, JobOutcome>) {
    // Per-category count: `Some` only when the category actually produced data
    // (Fresh, or Stale with a cached aggregate — `JobOutcome::payload()` returns
    // `None` for Pending/Failed/Stale-without-cache). A `None` category is left
    // untouched downstream rather than overwritten with a fabricated `0`, and
    // the bar stays suppressed until all three categories hold a real value, so
    // a partially-completed cold scan never lies about project health (#1).
    let count_of = |category: InspectCategory| -> Option<usize> {
        outcomes
            .get(&category)
            .and_then(JobOutcome::payload)
            .and_then(|payload| available_count_from_payload(category, payload))
    };
    let any_tier2_stale = [
        InspectCategory::DeadCode,
        InspectCategory::UnusedExports,
        InspectCategory::Duplicates,
    ]
    .iter()
    .any(|category| {
        matches!(
            outcomes.get(category),
            Some(JobOutcome::Stale { .. } | JobOutcome::Pending { .. })
        )
    });
    let todos = outcomes
        .get(&InspectCategory::Todos)
        .and_then(JobOutcome::payload)
        .and_then(|payload| payload.get("count"))
        .and_then(Value::as_u64)
        .map(|count| count as usize);

    ctx.update_status_bar_tier2(
        count_of(InspectCategory::DeadCode),
        count_of(InspectCategory::UnusedExports),
        count_of(InspectCategory::Duplicates),
        todos,
        any_tier2_stale,
    );
}

pub fn handle_inspect_tier2_run(req: &RawRequest, ctx: &AppContext) -> Response {
    let categories = match parse_tier2_categories(req.params.get("categories")) {
        Ok(categories) => categories,
        Err(message) => return invalid_request(&req.id, message),
    };

    if !ctx.inspect_writer() {
        let skipped = categories
            .iter()
            .map(|category| {
                serde_json::json!({
                    "category": category.as_str(),
                    "reason": "inspect_read_only",
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

    let config = ctx.config();
    let project_root = config
        .project_root
        .clone()
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    let project_root = std::fs::canonicalize(&project_root).unwrap_or(project_root);
    Ok(InspectSnapshot::new_with_capabilities(
        project_root,
        ctx.inspect_dir(),
        config,
        ctx.symbol_cache(),
        ctx.inspect_writer(),
        ctx.callgraph_writer(),
    ))
}

fn direct_tier2_wait_budget(project_root: &Path) -> Duration {
    if !env_project_root_matches("AFT_INSPECT_DIRECT_TIER2_DEADLINE_ROOT", project_root) {
        return DIRECT_TIER2_WAIT_BUDGET;
    }
    std::env::var("AFT_INSPECT_DIRECT_TIER2_DEADLINE_MS")
        .ok()
        .and_then(|raw| raw.parse::<u64>().ok())
        .map(Duration::from_millis)
        .unwrap_or(DIRECT_TIER2_WAIT_BUDGET)
}

fn env_project_root_matches(var: &str, project_root: &Path) -> bool {
    let Some(raw) = std::env::var_os(var) else {
        return true;
    };
    let expected = PathBuf::from(raw);
    let expected = std::fs::canonicalize(&expected).unwrap_or(expected);
    let actual = std::fs::canonicalize(project_root).unwrap_or_else(|_| project_root.to_path_buf());
    expected == actual
}

fn receive_direct_tier2(
    rx: std::sync::mpsc::Receiver<DirectTier2RunOutcome>,
    deadline: Instant,
) -> DirectTier2RunOutcome {
    let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
        return DirectTier2RunOutcome {
            outcome: JobOutcome::Pending { in_flight: true },
            force_paths_completed: false,
        };
    };
    if remaining.is_zero() {
        return DirectTier2RunOutcome {
            outcome: JobOutcome::Pending { in_flight: true },
            force_paths_completed: false,
        };
    }

    rx.recv_timeout(remaining).unwrap_or(DirectTier2RunOutcome {
        outcome: JobOutcome::Pending { in_flight: true },
        force_paths_completed: false,
    })
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
            if matches!(
                *category,
                InspectCategory::DeadCode | InspectCategory::UnusedExports
            ) {
                let test_only_detail = test_only_details_for(payload, top_k);
                if test_only_detail
                    .as_array()
                    .is_some_and(|items| !items.is_empty())
                {
                    details.insert(format!("{}_test_only", category.as_str()), test_only_detail);
                }
            }
            if matches!(
                *category,
                InspectCategory::DeadCode
                    | InspectCategory::UnusedExports
                    | InspectCategory::Duplicates
            ) {
                let generated_detail = generated_details_for(payload, top_k);
                if generated_detail
                    .as_array()
                    .is_some_and(|items| !items.is_empty())
                {
                    details.insert(format!("{}_generated", category.as_str()), generated_detail);
                }
            }
        } else if *category == InspectCategory::Diagnostics {
            // Diagnostics detail is always actionable — a bare count ("1 error")
            // can't be fixed without the message + location, and this category
            // is the replacement for the removed `lsp_diagnostics` tool. So
            // include its drill-down even without an explicit `sections` request.
            // Self-suppressing: only inserted when there's something to show, so
            // the clean (E0/W0) payload stays identical to before (no `details`).
            // Other Tier-2 categories stay `sections`-gated (their detail can be
            // hundreds of rows and a count is a meaningful at-a-glance signal).
            let detail = details_for(*category, payload, top_k);
            if detail.as_array().is_some_and(|items| !items.is_empty()) {
                details.insert(category.as_str().to_string(), detail);
            }
        }
    }

    let complete = pending_categories.is_empty();
    let incomplete_categories = pending_categories.clone();
    let disabled_categories = InspectCategory::disabled()
        .iter()
        .map(|category| category.as_str())
        .collect::<Vec<_>>();
    let tier2_last_run = tier2_last_run(snapshot);

    // Compact, line-oriented agent text (single source for both harnesses; the
    // plugins prefer `response.text` and fall back to JSON only when absent).
    // Renders the Tier-2 findings + todos + an honesty note. Diagnostics are
    // appended by the plugin layer (it owns the partial/pending honesty logic),
    // and the status bar is appended by the plugin's global hook — so neither
    // is rendered here. Metrics and scanner_state stay in the JSON wire payload
    // for the sidebar but are intentionally omitted from the agent text. Built
    // before `summary`/`details` are moved into the payload below.
    let text = render_inspect_text(
        &summary,
        &details,
        &stale_categories,
        &pending_categories,
        &failed_categories,
    );

    let mut payload = serde_json::json!({
        "complete": complete,
        "summary": Value::Object(summary),
        "text": text,
        "scanner_state": {
            "complete": complete,
            "incomplete_categories": incomplete_categories,
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

/// Render the compact agent-facing body. One source of truth for OpenCode + Pi.
fn render_inspect_text(
    summary: &Map<String, Value>,
    details: &Map<String, Value>,
    stale: &[String],
    pending: &[String],
    failed: &[Value],
) -> String {
    let mut lines: Vec<String> = Vec::new();

    // Honesty note first, only when there's something incomplete to flag.
    let mut notes: Vec<String> = Vec::new();
    for cat in stale {
        notes.push(format!("{cat} stale"));
    }
    for cat in pending {
        // Diagnostics pending is surfaced by the plugin's diagnostics line.
        if cat != "diagnostics" {
            notes.push(format!("{cat} pending"));
        }
    }
    for entry in failed {
        if let Some(cat) = entry.get("category").and_then(Value::as_str) {
            notes.push(format!("{cat} failed"));
        }
    }
    if !notes.is_empty() {
        lines.push(format!("note: {}", notes.join(", ")));
    }

    // Tier-2 findings, highest-signal first.
    render_group_category(&mut lines, "Duplicates", summary, details, "duplicates");
    render_cycles_category(&mut lines, summary, details);
    render_symbol_category(&mut lines, "Dead code", summary, details, "dead_code");
    render_symbol_category(
        &mut lines,
        "Unused exports",
        summary,
        details,
        "unused_exports",
    );
    render_todos(&mut lines, summary, details);

    lines.join("\n")
}

fn render_cycles_category(
    lines: &mut Vec<String>,
    summary: &Map<String, Value>,
    details: &Map<String, Value>,
) {
    if !details.contains_key("cycles") {
        return;
    }
    let Some(section) = summary.get("cycles") else {
        return;
    };
    if let Some(status) = section.get("status").and_then(Value::as_str) {
        lines.push(format!("Import cycles: {status}"));
        return;
    }
    let count = section.get("count").and_then(Value::as_u64).unwrap_or(0);
    if count == 0 {
        lines.push("Import cycles: 0".to_string());
        return;
    }
    let largest = section.get("largest").and_then(Value::as_u64).unwrap_or(0);
    let cycle_word = if count == 1 { "cycle" } else { "cycles" };
    let file_word = if largest == 1 { "file" } else { "files" };
    lines.push(format!(
        "Import cycles: {count} import {cycle_word} (largest: {largest} {file_word})"
    ));
    let Some(items) = details.get("cycles").and_then(Value::as_array) else {
        return;
    };
    for item in items {
        let cycle = item.get("cycle").and_then(Value::as_str).unwrap_or("?");
        let edge_kind = item
            .get("edge_kind")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        lines.push(format!("  {cycle} [{edge_kind}]"));
        if let Some(edges) = item.get("edges").and_then(Value::as_array) {
            for edge in edges {
                let from = edge.get("from").and_then(Value::as_str).unwrap_or("?");
                let to = edge.get("to").and_then(Value::as_str).unwrap_or("?");
                let imports = edge
                    .get("imports")
                    .and_then(Value::as_array)
                    .map(|imports| {
                        imports
                            .iter()
                            .map(render_cycle_import)
                            .collect::<Vec<_>>()
                            .join(", ")
                    })
                    .unwrap_or_default();
                if imports.is_empty() {
                    lines.push(format!("    {from} -> {to}"));
                } else {
                    lines.push(format!("    {from} -> {to} via {imports}"));
                }
            }
        }
    }
}

fn render_cycle_import(import: &Value) -> String {
    let specifier = import
        .get("specifier")
        .and_then(Value::as_str)
        .unwrap_or("?");
    let kind = import
        .get("kind")
        .and_then(Value::as_str)
        .unwrap_or("import");
    let line = import.get("line").and_then(Value::as_u64).unwrap_or(0);
    if line == 0 {
        format!("{kind} '{specifier}'")
    } else {
        format!("{kind} '{specifier}' line {line}")
    }
}

/// Pick the fuller drill-down list when present (sections requested), else the
/// summary's ranked `top` preview.
fn category_items<'a>(
    summary: &'a Map<String, Value>,
    details: &'a Map<String, Value>,
    key: &str,
) -> Option<&'a Vec<Value>> {
    details
        .get(key)
        .and_then(Value::as_array)
        .filter(|items| !items.is_empty())
        .or_else(|| {
            summary
                .get(key)
                .and_then(|s| s.get("top"))
                .and_then(Value::as_array)
        })
}

/// Categories whose findings are `{file, symbol}` (dead_code, unused_exports).
fn render_symbol_category(
    lines: &mut Vec<String>,
    label: &str,
    summary: &Map<String, Value>,
    details: &Map<String, Value>,
    key: &str,
) {
    let Some(section) = summary.get(key) else {
        return;
    };
    if let Some(status) = section.get("status").and_then(Value::as_str) {
        if let Some(reason) = section.get("reason").and_then(Value::as_str) {
            lines.push(format!("{label}: {status} ({reason})"));
        } else {
            lines.push(format!("{label}: {status}"));
        }
        return;
    }
    let count = section.get("count").and_then(Value::as_u64).unwrap_or(0);
    let suffix = dead_code_language_suffix(section);
    let skipped_suffix = dead_code_skipped_language_suffix(section);
    let generated_suffix = generated_count_suffix(section);
    if count == 0 {
        lines.push(format!("{label}: 0{generated_suffix}{skipped_suffix}"));
    } else {
        lines.push(format!(
            "{label}: {count}{suffix}{generated_suffix}{skipped_suffix}:"
        ));
        if let Some(items) = category_items(summary, details, key) {
            for item in items.iter().filter(|item| !item_is_generated(item)) {
                let file = item.get("file").and_then(Value::as_str).unwrap_or("?");
                let symbol = item.get("symbol").and_then(Value::as_str).unwrap_or("?");
                lines.push(format!("  {file}::{symbol}"));
            }
        }
    }
    render_generated_symbol_usage(lines, summary, details, key);
    render_test_only_usage(lines, summary, details, key);
}

fn generated_count_suffix(section: &Value) -> String {
    let generated_count = section
        .get("generated_count")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    if generated_count == 0 {
        String::new()
    } else {
        format!(" (generated: {generated_count})")
    }
}

fn item_is_generated(item: &Value) -> bool {
    item.get("generated").and_then(Value::as_bool) == Some(true)
}

fn render_generated_symbol_usage(
    lines: &mut Vec<String>,
    summary: &Map<String, Value>,
    details: &Map<String, Value>,
    key: &str,
) {
    let generated_count = summary
        .get(key)
        .and_then(|section| section.get("generated_count"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    if generated_count == 0 {
        return;
    }
    lines.push(format!("  generated: {generated_count}:"));
    if let Some(items) = generated_items(summary, details, key) {
        for item in items {
            let file = item.get("file").and_then(Value::as_str).unwrap_or("?");
            let symbol = item.get("symbol").and_then(Value::as_str).unwrap_or("?");
            lines.push(format!("    {file}::{symbol}"));
        }
    }
}

fn render_test_only_usage(
    lines: &mut Vec<String>,
    summary: &Map<String, Value>,
    details: &Map<String, Value>,
    key: &str,
) {
    let test_only_count = summary
        .get(key)
        .and_then(|section| section.get("test_only_count"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    if test_only_count == 0 {
        return;
    }
    lines.push(format!("  test-only usage: {test_only_count}:"));
    if let Some(items) = test_only_items(summary, details, key) {
        for item in items {
            let file = item.get("file").and_then(Value::as_str).unwrap_or("?");
            let symbol = item.get("symbol").and_then(Value::as_str).unwrap_or("?");
            let used_by = format_used_by_tests(item.get("used_by"));
            lines.push(format!("    {file}::{symbol} — used by {used_by}"));
        }
    }
}

fn test_only_items<'a>(
    summary: &'a Map<String, Value>,
    details: &'a Map<String, Value>,
    key: &str,
) -> Option<&'a Vec<Value>> {
    details
        .get(&format!("{key}_test_only"))
        .and_then(Value::as_array)
        .filter(|items| !items.is_empty())
        .or_else(|| {
            summary
                .get(key)
                .and_then(|s| s.get("test_only_top"))
                .and_then(Value::as_array)
        })
}

fn generated_items<'a>(
    summary: &'a Map<String, Value>,
    details: &'a Map<String, Value>,
    key: &str,
) -> Option<&'a Vec<Value>> {
    details
        .get(&format!("{key}_generated"))
        .and_then(Value::as_array)
        .filter(|items| !items.is_empty())
        .or_else(|| {
            summary
                .get(key)
                .and_then(|s| s.get("generated_top"))
                .and_then(Value::as_array)
        })
}

fn format_used_by_tests(value: Option<&Value>) -> String {
    let names = value
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(Value::as_str).collect::<Vec<_>>())
        .unwrap_or_default();
    if names.is_empty() {
        "test file".to_string()
    } else {
        names.join(", ")
    }
}

/// `(rust 214, ts 143)` language breakdown for dead_code; empty for others.
fn dead_code_language_suffix(section: &Value) -> String {
    let Some(by_lang) = section.get("by_language").and_then(Value::as_object) else {
        return String::new();
    };
    if by_lang.is_empty() {
        return String::new();
    }
    let mut pairs: Vec<(&String, u64)> = by_lang
        .iter()
        .map(|(k, v)| (k, v.as_u64().unwrap_or(0)))
        .collect();
    pairs.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));
    let rendered = pairs
        .iter()
        .map(|(lang, n)| format!("{} {n}", short_lang(lang)))
        .collect::<Vec<_>>()
        .join(", ");
    format!(" ({rendered})")
}

fn dead_code_skipped_language_suffix(section: &Value) -> String {
    let Some(languages) = section.get("languages_skipped").and_then(Value::as_array) else {
        return String::new();
    };
    if languages.is_empty() {
        return String::new();
    }
    let mut languages = languages
        .iter()
        .filter_map(Value::as_str)
        .map(short_lang)
        .collect::<Vec<_>>();
    languages.sort_unstable();
    languages.dedup();
    if languages.is_empty() {
        String::new()
    } else {
        format!(" ({} not analyzed)", languages.join(", "))
    }
}

fn short_lang(lang: &str) -> &str {
    match lang {
        "typescript" => "ts",
        "javascript" => "js",
        "python" => "py",
        other => other,
    }
}

/// Duplicates: `{cost, files: [a, b, ...]}`.
fn render_group_category(
    lines: &mut Vec<String>,
    label: &str,
    summary: &Map<String, Value>,
    details: &Map<String, Value>,
    key: &str,
) {
    if key == "duplicates" {
        render_duplicates_category(lines, label, summary, details, key);
        return;
    }

    let Some(section) = summary.get(key) else {
        return;
    };
    if let Some(status) = section.get("status").and_then(Value::as_str) {
        lines.push(format!("{label}: {status}"));
        return;
    }
    let count = section.get("count").and_then(Value::as_u64).unwrap_or(0);
    if count == 0 {
        lines.push(format!("{label}: 0"));
        return;
    }
    lines.push(format!("{label}: {count} (top by cost):"));
    if let Some(items) = category_items(summary, details, key) {
        for item in items.iter().filter(|item| !item_is_generated(item)) {
            let cost = item.get("cost").and_then(Value::as_u64).unwrap_or(0);
            let files: Vec<&str> = item
                .get("files")
                .and_then(Value::as_array)
                .map(|arr| arr.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            lines.push(format!("  {cost}  {}", files.join(" == ")));
        }
    }
}

fn render_duplicates_category(
    lines: &mut Vec<String>,
    label: &str,
    summary: &Map<String, Value>,
    details: &Map<String, Value>,
    key: &str,
) {
    let Some(section) = summary.get(key) else {
        return;
    };
    if let Some(status) = section.get("status").and_then(Value::as_str) {
        lines.push(format!("{label}: {status}"));
        return;
    }

    let count = section.get("count").and_then(Value::as_u64).unwrap_or(0);
    let generated_count = section
        .get("generated_count")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let generated_suffix = if generated_count == 0 {
        String::new()
    } else {
        format!(" (generated: {generated_count})")
    };
    let Some(duplicated_lines) = section.get("duplicated_lines").and_then(Value::as_u64) else {
        if count == 0 {
            lines.push(format!("{label}: 0{generated_suffix}"));
            render_generated_duplicate_usage(lines, summary, details, key);
            return;
        }
        lines.push(format!("{label}: {count}{generated_suffix} (top by cost):"));
        render_duplicate_rows(lines, summary, details, key);
        render_generated_duplicate_usage(lines, summary, details, key);
        return;
    };

    let total_lines = section
        .get("total_analyzed_lines")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let percent = section
        .get("duplicated_percent")
        .and_then(Value::as_f64)
        .unwrap_or_else(|| duplicate_percent(duplicated_lines, total_lines));
    let file_count = section
        .get("duplicated_file_count")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let group_count = count;
    let suffix = if count > 0 { " (top by cost):" } else { "" };
    // A zero denominator means analyzed-line counts are missing (pre-v0.44
    // cached contributions); print no percentage rather than a false "0.0%".
    let percent_clause = if total_lines > 0 {
        format!(
            " ({}% of {total_lines} analyzed lines)",
            format_percent(percent)
        )
    } else {
        String::new()
    };
    lines.push(format!(
        "{label}: {duplicated_lines} duplicated lines{percent_clause} across {file_count} files, {group_count} {}{generated_suffix}{suffix}",
        plural_group(group_count),
    ));
    render_duplicate_suppression(lines, section);
    if count > 0 {
        render_duplicate_rows(lines, summary, details, key);
    }
    render_generated_duplicate_usage(lines, summary, details, key);
}

fn render_duplicate_rows(
    lines: &mut Vec<String>,
    summary: &Map<String, Value>,
    details: &Map<String, Value>,
    key: &str,
) {
    if let Some(items) = category_items(summary, details, key) {
        for item in items.iter().filter(|item| !item_is_generated(item)) {
            let cost = item.get("cost").and_then(Value::as_u64).unwrap_or(0);
            let files: Vec<&str> = item
                .get("files")
                .and_then(Value::as_array)
                .map(|arr| arr.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            lines.push(format!("  {cost}  {}", files.join(" == ")));
            if duplicate_group_file_count(&files) >= 3 {
                lines
                    .push("      suggestion: consider extracting into a shared module".to_string());
            }
        }
    }
}

fn render_generated_duplicate_usage(
    lines: &mut Vec<String>,
    summary: &Map<String, Value>,
    details: &Map<String, Value>,
    key: &str,
) {
    let generated_count = summary
        .get(key)
        .and_then(|section| section.get("generated_count"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    if generated_count == 0 {
        return;
    }
    lines.push(format!("  generated: {generated_count}:"));
    if let Some(items) = generated_items(summary, details, key) {
        for item in items {
            let cost = item.get("cost").and_then(Value::as_u64).unwrap_or(0);
            let files: Vec<&str> = item
                .get("files")
                .and_then(Value::as_array)
                .map(|arr| arr.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            lines.push(format!("    {cost}  {}", files.join(" == ")));
        }
    }
}

fn render_duplicate_suppression(lines: &mut Vec<String>, section: &Value) {
    let mirror = section
        .get("mirror_suppressed_groups")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    if mirror > 0 {
        lines.push(format!(
            "  {mirror} mirror {} suppressed by expected_mirrors",
            plural_group(mirror)
        ));
    }
    let marker = section
        .get("marker_suppressed_groups")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    if marker > 0 {
        lines.push(format!(
            "  {marker} marker {} suppressed by aft:expected-duplicate",
            plural_group(marker)
        ));
    }
}

fn plural_group(count: u64) -> &'static str {
    if count == 1 {
        "group"
    } else {
        "groups"
    }
}

fn duplicate_group_file_count(files: &[&str]) -> usize {
    files
        .iter()
        .map(|file| display_file_from_duplicate_occurrence(file))
        .collect::<std::collections::BTreeSet<_>>()
        .len()
}

fn display_file_from_duplicate_occurrence(value: &str) -> &str {
    let Some((file, range)) = value.rsplit_once(':') else {
        return value;
    };
    let Some((start, end)) = range.split_once('-') else {
        return value;
    };
    if start.chars().all(|char| char.is_ascii_digit())
        && end.chars().all(|char| char.is_ascii_digit())
    {
        file
    } else {
        value
    }
}

fn duplicate_percent(duplicated_lines: u64, total_lines: u64) -> f64 {
    if total_lines == 0 {
        0.0
    } else {
        (duplicated_lines as f64 * 100.0) / total_lines as f64
    }
}

fn format_percent(percent: f64) -> String {
    format!("{percent:.1}")
}

fn render_todos(
    lines: &mut Vec<String>,
    summary: &Map<String, Value>,
    details: &Map<String, Value>,
) {
    let Some(section) = summary.get("todos") else {
        return;
    };
    let count = section.get("count").and_then(Value::as_u64).unwrap_or(0);
    if count == 0 {
        return;
    }
    let by_kind = section
        .get("by_kind")
        .and_then(Value::as_object)
        .map(|map| {
            let mut pairs: Vec<(&String, u64)> = map
                .iter()
                .map(|(k, v)| (k, v.as_u64().unwrap_or(0)))
                .collect();
            pairs.sort_by(|a, b| a.0.cmp(b.0));
            pairs
                .iter()
                .map(|(kind, n)| format!("{kind} {n}"))
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default();
    if by_kind.is_empty() {
        lines.push(format!("TODOs: {count}"));
    } else {
        lines.push(format!("TODOs: {count} ({by_kind})"));
    }
    // Detail rows only when explicitly drilled into (sections: ["todos"]) — the
    // scanner populates details["todos"] only then, keeping the default summary
    // compact while honoring an explicit request for the items.
    if let Some(items) = details.get("todos").and_then(Value::as_array) {
        for item in items {
            let file = item.get("file").and_then(Value::as_str).unwrap_or("?");
            let line = item.get("line").and_then(Value::as_u64).unwrap_or(0);
            let marker = item.get("marker").and_then(Value::as_str).unwrap_or("?");
            let text = item.get("text").and_then(Value::as_str).unwrap_or("");
            lines.push(format!("  {file}:{line} {marker} {text}"));
        }
    }
}

fn summary_for(category: InspectCategory, outcome: Option<&JobOutcome>) -> Value {
    let Some(outcome) = outcome else {
        return status_summary("pending");
    };
    // Stale WITH a cached payload: surface the real last-known counts (the same
    // numbers the status bar shows with its `~` marker) flagged `stale: true`,
    // instead of a bare `{status:"stale"}` that throws the counts away and makes
    // the body disagree with the bar. Staleness is still signaled — by the flag
    // and the `note:` line. Pending / Failed / stale-without-cache carry no
    // payload, so they keep the bare status sentinel.
    if let JobOutcome::Stale {
        cached: Some(payload),
        ..
    } = outcome
    {
        // Defensive: only surface counts when the cached payload actually has a
        // real `count`. All Tier-2 stale categories are count-based, so a
        // payload without one is malformed — fall through to the sentinel rather
        // than render a fabricated `0` that would read as "clean".
        if payload.get("count").and_then(Value::as_u64).is_some()
            || payload.get("total_count").and_then(Value::as_u64).is_some()
            || payload.get("top").is_some()
        {
            let mut summary = computed_summary_for(category, Some(payload));
            if let Some(obj) = summary.as_object_mut() {
                obj.insert("stale".to_string(), Value::Bool(true));
            }
            return summary;
        }
    }
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
        InspectCategory::DeadCode => {
            if payload
                .and_then(|payload| payload.get("callgraph_available"))
                .and_then(Value::as_bool)
                == Some(false)
            {
                serde_json::json!({
                    "status": "unavailable",
                    "reason": "call graph building/retrying",
                    "callgraph_available": false,
                })
            } else {
                serde_json::json!({
                    "count": count_from_payload(payload),
                    "generated_count": generated_count_from_payload(payload),
                    "total_count": total_count_from_payload(payload),
                    "test_only_count": test_only_count_from_payload(payload),
                    "by_language": payload.and_then(|p| p.get("by_language")).cloned().unwrap_or_else(|| serde_json::json!({})),
                    "languages_skipped": payload.and_then(|p| p.get("languages_skipped")).cloned().unwrap_or_else(|| serde_json::json!([])),
                    "top": top_preview_from_payload(payload),
                    "generated_top": generated_top_from_payload(payload),
                    "test_only_top": test_only_top_from_payload(payload),
                })
            }
        }
        InspectCategory::UnusedExports => serde_json::json!({
            "count": count_from_payload(payload),
            "generated_count": generated_count_from_payload(payload),
            "total_count": total_count_from_payload(payload),
            "test_only_count": test_only_count_from_payload(payload),
            "top": top_preview_from_payload(payload),
            "generated_top": generated_top_from_payload(payload),
            "test_only_top": test_only_top_from_payload(payload),
        }),
        InspectCategory::Duplicates => {
            let mut section = Map::new();
            section.insert(
                "count".to_string(),
                serde_json::json!(count_from_payload(payload)),
            );
            section.insert(
                "total_groups".to_string(),
                serde_json::json!(payload
                    .and_then(|p| p.get("total_groups").or_else(|| p.get("groups_count")))
                    .and_then(Value::as_u64)
                    .unwrap_or_else(|| count_from_payload(payload))),
            );
            for key in [
                "generated_count",
                "total_count",
                "duplicated_lines",
                "duplicated_percent",
                "duplicated_file_count",
                "generated_duplicated_lines",
                "generated_duplicated_file_count",
                "total_duplicated_lines",
                "total_duplicated_file_count",
                "total_analyzed_lines",
                "suppressed_groups",
                "mirror_suppressed_groups",
                "marker_suppressed_groups",
            ] {
                if let Some(value) = payload.and_then(|payload| payload.get(key)).cloned() {
                    section.insert(key.to_string(), value);
                }
            }
            section.insert("top".to_string(), top_preview_from_payload(payload));
            section.insert(
                "generated_top".to_string(),
                generated_top_from_payload(payload),
            );
            Value::Object(section)
        }
        InspectCategory::Cycles => serde_json::json!({
            "count": count_from_payload(payload),
            "largest": payload.and_then(|p| p.get("largest")).and_then(Value::as_u64).unwrap_or(0),
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

fn test_only_details_for(payload: Option<&Value>, top_k: usize) -> Value {
    let Some(payload) = payload else {
        return serde_json::json!([]);
    };
    match payload.get("test_only_items").and_then(Value::as_array) {
        Some(items) => Value::Array(items.iter().take(top_k).cloned().collect()),
        None => serde_json::json!([]),
    }
}

fn generated_details_for(payload: Option<&Value>, top_k: usize) -> Value {
    let Some(payload) = payload else {
        return serde_json::json!([]);
    };
    match payload.get("generated_items").and_then(Value::as_array) {
        Some(items) => Value::Array(items.iter().take(top_k).cloned().collect()),
        None => serde_json::json!([]),
    }
}

fn available_count_from_payload(category: InspectCategory, payload: &Value) -> Option<usize> {
    if category == InspectCategory::DeadCode
        && payload.get("callgraph_available").and_then(Value::as_bool) == Some(false)
    {
        return None;
    }
    payload
        .get("count")
        .and_then(Value::as_u64)
        .map(|count| count as usize)
}

fn count_from_payload(payload: Option<&Value>) -> u64 {
    payload
        .and_then(|payload| payload.get("count"))
        .and_then(Value::as_u64)
        .unwrap_or(0)
}

/// Pass through the scanner's already-ranked `top` preview (highest-signal
/// findings) into the summary view. Omitted (empty array) when absent so the
/// summary stays compact for empty/legacy payloads.
fn top_preview_from_payload(payload: Option<&Value>) -> Value {
    payload
        .and_then(|payload| payload.get("top"))
        .filter(|top| top.is_array())
        .cloned()
        .unwrap_or_else(|| serde_json::json!([]))
}

fn test_only_count_from_payload(payload: Option<&Value>) -> u64 {
    payload
        .and_then(|payload| payload.get("test_only_count"))
        .and_then(Value::as_u64)
        .unwrap_or(0)
}

fn generated_count_from_payload(payload: Option<&Value>) -> u64 {
    payload
        .and_then(|payload| payload.get("generated_count"))
        .and_then(Value::as_u64)
        .unwrap_or(0)
}

fn total_count_from_payload(payload: Option<&Value>) -> u64 {
    payload
        .and_then(|payload| payload.get("total_count"))
        .and_then(Value::as_u64)
        .unwrap_or_else(|| {
            count_from_payload(payload)
                + test_only_count_from_payload(payload)
                + generated_count_from_payload(payload)
        })
}

fn test_only_top_from_payload(payload: Option<&Value>) -> Value {
    payload
        .and_then(|payload| payload.get("test_only_top"))
        .filter(|top| top.is_array())
        .cloned()
        .unwrap_or_else(|| serde_json::json!([]))
}

fn generated_top_from_payload(payload: Option<&Value>) -> Value {
    payload
        .and_then(|payload| payload.get("generated_top"))
        .filter(|top| top.is_array())
        .cloned()
        .unwrap_or_else(|| serde_json::json!([]))
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

#[cfg(test)]
mod status_bar_refresh_tests {
    use super::*;
    use crate::parser::TreeSitterProvider;

    fn ctx() -> AppContext {
        AppContext::new(Box::new(TreeSitterProvider::new()), Default::default())
    }

    fn outcomes(
        entries: Vec<(InspectCategory, JobOutcome)>,
    ) -> BTreeMap<InspectCategory, JobOutcome> {
        entries.into_iter().collect()
    }

    // #1: a Pending-only Tier-2 (no scan has ever produced counts) must NOT
    // populate the status bar — otherwise it renders fabricated `~D0 U0 C0`
    // zeros that lie about project health.
    #[test]
    fn pending_tier2_does_not_populate_status_bar() {
        let ctx = ctx();
        assert!(ctx.status_bar_counts().is_none());

        refresh_status_bar_counts(
            &ctx,
            &outcomes(vec![
                (
                    InspectCategory::DeadCode,
                    JobOutcome::Pending { in_flight: true },
                ),
                (
                    InspectCategory::UnusedExports,
                    JobOutcome::Pending { in_flight: true },
                ),
                (
                    InspectCategory::Duplicates,
                    JobOutcome::Pending { in_flight: true },
                ),
            ]),
        );

        assert!(
            ctx.status_bar_counts().is_none(),
            "Pending Tier-2 must leave the bar unpopulated (no fabricated zeros)"
        );
    }

    // Stale-without-cache is equally untrustworthy — also must not populate.
    #[test]
    fn stale_without_cache_does_not_populate_status_bar() {
        let ctx = ctx();
        refresh_status_bar_counts(
            &ctx,
            &outcomes(vec![(
                InspectCategory::DeadCode,
                JobOutcome::Stale {
                    cached: None,
                    in_flight: true,
                },
            )]),
        );
        assert!(ctx.status_bar_counts().is_none());
    }

    // A real Fresh outcome populates the bar with the actual counts.
    #[test]
    fn fresh_tier2_populates_status_bar() {
        let ctx = ctx();
        refresh_status_bar_counts(
            &ctx,
            &outcomes(vec![
                (
                    InspectCategory::DeadCode,
                    JobOutcome::Fresh {
                        payload: serde_json::json!({ "count": 7 }),
                    },
                ),
                (
                    InspectCategory::UnusedExports,
                    JobOutcome::Fresh {
                        payload: serde_json::json!({ "count": 3 }),
                    },
                ),
                (
                    InspectCategory::Duplicates,
                    JobOutcome::Fresh {
                        payload: serde_json::json!({ "count": 1 }),
                    },
                ),
            ]),
        );
        let counts = ctx.status_bar_counts().expect("populated");
        assert_eq!(counts.dead_code, 7);
        assert_eq!(counts.unused_exports, 3);
        assert_eq!(counts.duplicates, 1);
        assert!(!counts.tier2_stale);
    }

    // Stale-WITH-cache populates (last-known counts) and marks the bar stale.
    // All three categories must carry a cached value — the bar stays suppressed
    // until every Tier-2 category is real, never fabricating a 0 (#1).
    #[test]
    fn stale_with_cache_populates_and_marks_stale() {
        let ctx = ctx();
        let stale_cache = |count: i64| JobOutcome::Stale {
            cached: Some(serde_json::json!({ "count": count })),
            in_flight: true,
        };
        refresh_status_bar_counts(
            &ctx,
            &outcomes(vec![
                (InspectCategory::DeadCode, stale_cache(12)),
                (InspectCategory::UnusedExports, stale_cache(4)),
                (InspectCategory::Duplicates, stale_cache(2)),
            ]),
        );
        let counts = ctx.status_bar_counts().expect("populated");
        assert_eq!(counts.dead_code, 12);
        assert_eq!(counts.unused_exports, 4);
        assert_eq!(counts.duplicates, 2);
        assert!(counts.tier2_stale);
    }

    // A single category (others Pending) must NOT surface the bar — the core
    // partial-completion fabrication guard at the sync refresh path (#1).
    #[test]
    fn single_category_does_not_populate_status_bar() {
        let ctx = ctx();
        refresh_status_bar_counts(
            &ctx,
            &outcomes(vec![(
                InspectCategory::DeadCode,
                JobOutcome::Fresh {
                    payload: serde_json::json!({ "count": 9 }),
                },
            )]),
        );
        assert!(
            ctx.status_bar_counts().is_none(),
            "one real category must not surface a bar with fabricated U0 C0"
        );
    }
}

#[cfg(test)]
mod render_text_tests {
    use super::*;

    fn summary_map(value: Value) -> Map<String, Value> {
        value.as_object().cloned().unwrap_or_default()
    }

    fn render(summary: Value) -> String {
        render_inspect_text(&summary_map(summary), &Map::new(), &[], &[], &[])
    }

    fn render_with_details(summary: Value, details: Value) -> String {
        render_inspect_text(&summary_map(summary), &summary_map(details), &[], &[], &[])
    }

    #[test]
    fn renders_todo_detail_rows_when_drilled_into() {
        let text = render_with_details(
            serde_json::json!({ "todos": { "count": 2, "by_kind": { "BUG": 1, "TODO": 1 } } }),
            serde_json::json!({
                "todos": [
                    { "file": "src/a.ts", "line": 10, "marker": "BUG", "text": "leak here" },
                    { "file": "src/b.ts", "line": 4, "marker": "TODO", "text": "wire it" },
                ]
            }),
        );
        // Summary line still present, plus per-item rows.
        assert!(
            text.contains("TODOs: 2 (BUG 1, TODO 1)"),
            "summary:\n{text}"
        );
        assert!(
            text.contains("  src/a.ts:10 BUG leak here"),
            "row a:\n{text}"
        );
        assert!(text.contains("  src/b.ts:4 TODO wire it"), "row b:\n{text}");
    }

    #[test]
    fn omits_todo_detail_rows_without_drill_in() {
        // No details → count/by_kind only, no per-item rows (default compact).
        let text = render(serde_json::json!({
            "todos": { "count": 2, "by_kind": { "BUG": 1, "TODO": 1 } }
        }));
        assert!(
            text.contains("TODOs: 2 (BUG 1, TODO 1)"),
            "summary:\n{text}"
        );
        assert!(!text.contains("\n  "), "no detail rows expected:\n{text}");
    }

    #[test]
    fn renders_populated_categories_highest_signal_first() {
        let text = render(serde_json::json!({
            "duplicates": {
                "count": 2,
                "top": [
                    { "cost": 1083, "files": ["a/x.ts:1-9", "b/x.ts:1-9"] },
                    { "cost": 500, "files": ["a/y.ts:1-3", "b/y.ts:1-3"] },
                ],
            },
            "dead_code": {
                "count": 357,
                "by_language": { "rust": 214, "typescript": 143 },
                "top": [ { "file": "crates/aft/src/x.rs", "symbol": "foo" } ],
            },
            "unused_exports": {
                "count": 1,
                "top": [ { "file": "packages/aft-bridge/src/log.ts", "symbol": "sessionLog" } ],
            },
            "todos": { "count": 8, "by_kind": { "BUG": 2, "TODO": 3 } },
        }));

        // Order: duplicates → dead_code → unused_exports → todos.
        let dup = text.find("Duplicates:").expect("duplicates");
        let dead = text.find("Dead code:").expect("dead code");
        let unused = text.find("Unused exports:").expect("unused");
        let todos = text.find("TODOs:").expect("todos");
        assert!(
            dup < dead && dead < unused && unused < todos,
            "wrong order:\n{text}"
        );

        // Cost-ranked duplicate rows with `==` separator between the file pair.
        assert!(
            text.contains("1083  a/x.ts:1-9 == b/x.ts:1-9"),
            "dup row:\n{text}"
        );
        // dead_code language breakdown uses short names, count-desc.
        assert!(
            text.contains("Dead code: 357 (rust 214, ts 143):"),
            "dead head:\n{text}"
        );
        assert!(
            text.contains("  crates/aft/src/x.rs::foo"),
            "dead row:\n{text}"
        );
        assert!(
            text.contains("  packages/aft-bridge/src/log.ts::sessionLog"),
            "unused row:\n{text}"
        );
        assert!(text.contains("TODOs: 8 (BUG 2, TODO 3)"), "todos:\n{text}");

        // Metrics + scanner_state are NOT in the agent text.
        assert!(!text.contains("loc"), "metrics leaked into text:\n{text}");
        assert!(
            !text.contains("scanner_state"),
            "scanner_state leaked:\n{text}"
        );
        // Diagnostics + status bar are appended by the plugin layer, not here.
        assert!(
            !text.contains("diagnostics"),
            "diagnostics must be plugin-rendered:\n{text}"
        );
        assert!(
            !text.contains("[AFT"),
            "status bar must be plugin-appended:\n{text}"
        );
    }

    #[test]
    fn renders_test_only_usage_after_headline_items() {
        let text = render_with_details(
            serde_json::json!({
                "dead_code": {
                    "count": 1,
                    "top": [ { "file": "src/api.ts", "symbol": "plantedDead" } ],
                    "test_only_count": 2,
                    "test_only_top": [
                        { "file": "src/api.ts", "symbol": "testOnly", "used_by": ["api.test.ts"] },
                    ],
                },
                "unused_exports": {
                    "count": 0,
                    "top": [],
                    "test_only_count": 1,
                    "test_only_top": [
                        { "file": "src/barrel-target.ts", "symbol": "throughBarrel", "used_by": ["barrel.test.ts"] },
                    ],
                }
            }),
            serde_json::json!({
                "dead_code": [ { "file": "src/api.ts", "symbol": "plantedDead" } ],
                "dead_code_test_only": [
                    { "file": "src/api.ts", "symbol": "testOnly", "used_by": ["api.test.ts"] },
                    { "file": "src/barrel-target.ts", "symbol": "throughBarrel", "used_by": ["barrel.test.ts"] },
                ],
            }),
        );

        assert!(text.contains("Dead code: 1:"), "{text}");
        assert!(text.contains("  src/api.ts::plantedDead"), "{text}");
        assert!(text.contains("  test-only usage: 2:"), "{text}");
        assert!(
            text.contains("    src/api.ts::testOnly — used by api.test.ts"),
            "{text}"
        );
        assert!(
            text.contains("    src/barrel-target.ts::throughBarrel — used by barrel.test.ts"),
            "{text}"
        );
        assert!(text.contains("Unused exports: 0"), "{text}");
        assert!(
            text.contains("    src/barrel-target.ts::throughBarrel — used by barrel.test.ts"),
            "{text}"
        );
    }

    #[test]
    fn renders_dead_code_skipped_languages_as_not_analyzed() {
        let text = render(serde_json::json!({
            "dead_code": {
                "count": 0,
                "by_language": {},
                "languages_skipped": ["kotlin", "java"],
                "top": [],
            }
        }));

        assert!(
            text.contains("Dead code: 0 (java, kotlin not analyzed)"),
            "dead-code skipped language note missing:\n{text}"
        );
    }

    #[test]
    fn renders_generated_usage_after_headline_items() {
        let text = render_with_details(
            serde_json::json!({
                "duplicates": {
                    "count": 1,
                    "generated_count": 1,
                    "total_groups": 2,
                    "duplicated_lines": 6,
                    "duplicated_percent": 3.0,
                    "duplicated_file_count": 2,
                    "total_analyzed_lines": 200,
                    "top": [
                        { "cost": 10, "files": ["src/a.ts:1-3", "src/b.ts:1-3"] },
                    ],
                    "generated_top": [
                        { "cost": 100, "files": ["gen/a.ts:1-9", "gen/b.ts:1-9"], "generated": true },
                    ],
                },
                "dead_code": {
                    "count": 1,
                    "generated_count": 2,
                    "total_count": 3,
                    "top": [ { "file": "src/hand.ts", "symbol": "handDead" } ],
                    "generated_top": [
                        { "file": "gen/schema_pb.ts", "symbol": "generatedPathDead", "generated": true },
                    ],
                },
                "unused_exports": {
                    "count": 0,
                    "generated_count": 1,
                    "total_count": 1,
                    "top": [],
                    "generated_top": [
                        { "file": "src/banner.ts", "symbol": "bannerUnused", "generated": true },
                    ],
                }
            }),
            serde_json::json!({
                "duplicates": [
                    { "cost": 10, "files": ["src/a.ts:1-3", "src/b.ts:1-3"] },
                    { "cost": 100, "files": ["gen/a.ts:1-9", "gen/b.ts:1-9"], "generated": true },
                ],
                "duplicates_generated": [
                    { "cost": 100, "files": ["gen/a.ts:1-9", "gen/b.ts:1-9"], "generated": true },
                ],
                "dead_code": [
                    { "file": "src/hand.ts", "symbol": "handDead" },
                    { "file": "gen/schema_pb.ts", "symbol": "generatedPathDead", "generated": true },
                ],
                "dead_code_generated": [
                    { "file": "gen/schema_pb.ts", "symbol": "generatedPathDead", "generated": true },
                    { "file": "src/banner.ts", "symbol": "bannerDead", "generated": true },
                ],
            }),
        );

        assert!(
            text.contains("Duplicates: 6 duplicated lines (3.0% of 200 analyzed lines) across 2 files, 1 group (generated: 1) (top by cost):"),
            "{text}"
        );
        assert!(
            text.contains("  10  src/a.ts:1-3 == src/b.ts:1-3"),
            "{text}"
        );
        assert!(text.contains("  generated: 1:"), "{text}");
        assert!(
            text.contains("    100  gen/a.ts:1-9 == gen/b.ts:1-9"),
            "{text}"
        );

        assert!(text.contains("Dead code: 1 (generated: 2):"), "{text}");
        assert!(text.contains("  src/hand.ts::handDead"), "{text}");
        assert!(
            text.contains("    gen/schema_pb.ts::generatedPathDead"),
            "{text}"
        );
        assert!(text.contains("    src/banner.ts::bannerDead"), "{text}");

        assert!(text.contains("Unused exports: 0 (generated: 1)"), "{text}");
        assert!(text.contains("    src/banner.ts::bannerUnused"), "{text}");
    }

    #[test]
    fn renders_duplicate_framing_suppression_and_extraction_suggestions() {
        let text = render(serde_json::json!({
            "duplicates": {
                "count": 1,
                "total_groups": 1,
                "duplicated_lines": 42,
                "duplicated_percent": 10.4,
                "duplicated_file_count": 3,
                "total_analyzed_lines": 404,
                "mirror_suppressed_groups": 2,
                "marker_suppressed_groups": 1,
                "top": [
                    { "cost": 1083, "files": ["a/x.ts:1-9", "b/x.ts:1-9", "c/x.ts:1-9"] }
                ]
            }
        }));

        assert!(
            text.contains(
                "Duplicates: 42 duplicated lines (10.4% of 404 analyzed lines) across 3 files, 1 group (top by cost):"
            ),
            "{text}"
        );
        assert!(
            text.contains("2 mirror groups suppressed by expected_mirrors"),
            "{text}"
        );
        assert!(
            text.contains("1 marker group suppressed by aft:expected-duplicate"),
            "{text}"
        );
        assert!(
            text.contains("suggestion: consider extracting into a shared module"),
            "{text}"
        );
    }

    #[test]
    fn zero_counts_render_as_clean_zero() {
        let text = render(serde_json::json!({
            "duplicates": { "count": 0 },
            "dead_code": { "count": 0, "by_language": {} },
            "unused_exports": { "count": 0 },
            "todos": { "count": 0 },
        }));
        assert!(text.contains("Duplicates: 0"), "{text}");
        assert!(text.contains("Dead code: 0"), "{text}");
        assert!(text.contains("Unused exports: 0"), "{text}");
        // Zero todos are omitted entirely (no noise).
        assert!(
            !text.contains("TODOs:"),
            "zero todos should be omitted:\n{text}"
        );
    }

    #[test]
    fn pending_status_renders_status_not_count() {
        let text = render(serde_json::json!({
            "duplicates": { "status": "pending" },
            "dead_code": { "status": "stale" },
        }));
        assert!(text.contains("Duplicates: pending"), "{text}");
        assert!(text.contains("Dead code: stale"), "{text}");
    }

    #[test]
    fn honesty_note_lists_incomplete_categories_only_when_present() {
        let none = render_inspect_text(&Map::new(), &Map::new(), &[], &[], &[]);
        assert!(!none.contains("note:"), "no note when all clear:\n{none}");

        let text = render_inspect_text(
            &summary_map(serde_json::json!({ "duplicates": { "count": 1, "top": [] } })),
            &Map::new(),
            &["dead_code".to_string()],
            &["unused_exports".to_string(), "diagnostics".to_string()],
            &[serde_json::json!({ "category": "duplicates", "message": "boom" })],
        );
        // diagnostics-pending is surfaced by the plugin diagnostics line, not here.
        assert!(
            text.contains("note: dead_code stale, unused_exports pending, duplicates failed"),
            "{text}"
        );
        assert!(
            !text.contains("diagnostics pending"),
            "diagnostics excluded from note:\n{text}"
        );
        // Note comes first.
        assert!(text.starts_with("note:"), "note must lead:\n{text}");
    }

    // Regression: a stale outcome WITH a cached payload must surface the real
    // last-known counts (matching the status bar's `~D…` numbers) rather than a
    // bare {status:"stale"} that drops them — body and bar must agree.
    #[test]
    fn stale_with_cache_summary_keeps_counts_and_flags_stale() {
        let stale = JobOutcome::Stale {
            cached: Some(serde_json::json!({ "count": 357, "by_language": { "rust": 214 } })),
            in_flight: true,
        };
        let summary = summary_for(InspectCategory::DeadCode, Some(&stale));
        assert_eq!(summary.get("count").and_then(Value::as_u64), Some(357));
        assert_eq!(summary.get("stale").and_then(Value::as_bool), Some(true));
        // Not the bare sentinel.
        assert!(
            summary.get("status").is_none(),
            "stale-with-cache must not be a status sentinel: {summary}"
        );

        // And the rendered body shows the count, not "stale".
        let text = render_inspect_text(
            &summary_map(serde_json::json!({ "dead_code": summary })),
            &Map::new(),
            &["dead_code".to_string()],
            &[],
            &[],
        );
        assert!(
            text.contains("Dead code: 357"),
            "body must show cached count:\n{text}"
        );
        assert!(
            text.contains("note: dead_code stale"),
            "staleness still flagged:\n{text}"
        );
    }

    // Stale WITHOUT a cache (never scanned, just invalidated) keeps the bare
    // sentinel — there are no real counts to show.
    #[test]
    fn stale_without_cache_summary_is_status_sentinel() {
        let stale = JobOutcome::Stale {
            cached: None,
            in_flight: true,
        };
        let summary = summary_for(InspectCategory::DeadCode, Some(&stale));
        assert_eq!(summary.get("status").and_then(Value::as_str), Some("stale"));
        assert!(summary.get("count").is_none());
    }
}
