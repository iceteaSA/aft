use std::path::Path;
use std::time::Instant;

use crate::context::AppContext;
use crate::error::AftError;
use crate::protocol::{RawRequest, Response};
use crate::{slog_info, slog_warn};

/// Handle a `callers` request.
///
/// Expects:
/// - `file` (string, required) — path to the source file containing the target symbol
/// - `symbol` (string, required) — name of the symbol to find callers for
/// - `depth` (number, optional, default 1) — recursive depth (1 = direct callers only)
///
/// Returns callers grouped by file with fields: `symbol`, `file`,
/// `callers` (array of `{ file, callers: [{ symbol, line }] }`),
/// `total_callers`, `scanned_files`.
///
/// Returns error if:
/// - required params missing
/// - call graph not initialized (configure not called)
/// - symbol not found in the file
pub fn handle_callers(req: &RawRequest, ctx: &AppContext) -> Response {
    let file = match req.params.get("file").and_then(|v| v.as_str()) {
        Some(f) => f,
        None => {
            return Response::error(
                &req.id,
                "invalid_request",
                "callers: missing required param 'file'",
            );
        }
    };

    let symbol = match req.params.get("symbol").and_then(|v| v.as_str()) {
        Some(s) => s,
        None => {
            return Response::error(
                &req.id,
                "invalid_request",
                "callers: missing required param 'symbol'",
            );
        }
    };

    let depth = req
        .params
        .get("depth")
        .and_then(|v| v.as_u64())
        .unwrap_or(1)
        .min(100) as usize;

    let mut cg_ref = ctx.callgraph().borrow_mut();
    let graph = match cg_ref.as_mut() {
        Some(g) => g,
        None => {
            return Response::error(
                &req.id,
                "not_configured",
                "callers: project not configured — send 'configure' first",
            );
        }
    };

    let file_path = match ctx.validate_path(&req.id, Path::new(file)) {
        Ok(path) => path,
        Err(resp) => return resp,
    };

    let project_root = ctx.config().project_root.clone();
    if let Some(project_root) = project_root {
        let canonical_root = std::fs::canonicalize(&project_root).unwrap_or(project_root.clone());
        let input_for_resolution = if file_path.is_relative() {
            project_root.join(&file_path)
        } else {
            file_path.clone()
        };
        let canonical_input =
            std::fs::canonicalize(&input_for_resolution).unwrap_or(input_for_resolution);
        if !canonical_input.starts_with(&canonical_root) {
            return Response::error(
                &req.id,
                "path_outside_project_root",
                format!(
                    "Callgraph operations require paths inside project_root. Got: {} (project_root: {})",
                    file_path.display(),
                    project_root.display(),
                ),
            );
        }
    }

    let symbol = match graph.resolve_symbol_query(&file_path, symbol) {
        Ok(symbol) => symbol,
        Err(e) => return Response::error(&req.id, e.code(), e.to_string()),
    };

    let max_files = ctx.config().max_callgraph_files;

    // Time the whole query. A cold call-graph build on a large repo can run
    // close to the bridge request timeout (which the host then renders as a
    // failed/"red" tool call). Logging the duration + outcome makes a slow or
    // rejected call attributable instead of looking like an opaque hang (#84).
    let started = Instant::now();
    let outcome = graph.callers_of(&file_path, &symbol, depth, max_files);
    let elapsed_ms = started.elapsed().as_millis();

    match outcome {
        Ok(result) => {
            slog_info!(
                "callers: '{}' in {} → {} sites in {}ms",
                symbol,
                file_path.display(),
                result.total_callers,
                elapsed_ms
            );
            let result_json = serde_json::to_value(&result).unwrap_or_default();
            Response::success(&req.id, result_json)
        }
        Err(err @ AftError::ProjectTooLarge { .. }) => {
            slog_warn!(
                "callers: rejected project_too_large after {}ms (raise lsp.max_callgraph_files or scope the repo)",
                elapsed_ms
            );
            Response::error(&req.id, "project_too_large", format!("{}", err))
        }
        Err(e) => {
            slog_warn!("callers: '{}' failed after {}ms: {}", symbol, elapsed_ms, e);
            Response::error(&req.id, e.code(), e.to_string())
        }
    }
}
