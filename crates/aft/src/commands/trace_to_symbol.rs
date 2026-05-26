use std::path::{Path, PathBuf};

use crate::context::AppContext;
use crate::error::AftError;
use crate::protocol::{RawRequest, Response};

/// Handle a `trace_to_symbol` request.
///
/// Traces forward from one symbol to another symbol using breadth-first search,
/// returning the first (shortest) resolved call path from origin to target.
///
/// Expects:
/// - `file` (string, required) — path to the source file containing the FROM symbol
/// - `symbol` (string, required) — name of the FROM symbol
/// - `toSymbol` (string, required) — name of the TO symbol
/// - `toFile` (string, optional) — file containing the TO symbol, required when ambiguous
/// - `depth` (number, optional, default 10, max 16) — maximum forward BFS depth
///
/// Returns `TraceToSymbolResult` with fields: `path` (array of hops or null),
/// `complete`, and `reason` when no path is returned.
pub fn handle_trace_to_symbol(req: &RawRequest, ctx: &AppContext) -> Response {
    let file = match req.params.get("file").and_then(|v| v.as_str()) {
        Some(f) => f,
        None => {
            return Response::error(
                &req.id,
                "invalid_request",
                "trace_to_symbol: missing required param 'file'",
            );
        }
    };

    let symbol = match req.params.get("symbol").and_then(|v| v.as_str()) {
        Some(s) => s,
        None => {
            return Response::error(
                &req.id,
                "invalid_request",
                "trace_to_symbol: missing required param 'symbol'",
            );
        }
    };

    let to_symbol = match req.params.get("toSymbol").and_then(|v| v.as_str()) {
        Some(s) => s,
        None => {
            return Response::error(
                &req.id,
                "invalid_request",
                "trace_to_symbol: missing required param 'toSymbol'",
            );
        }
    };

    let depth = req
        .params
        .get("depth")
        .and_then(|v| v.as_u64())
        .unwrap_or(10)
        .min(16) as usize;

    let (max_files, project_root) = {
        let config = ctx.config();
        (config.max_callgraph_files, config.project_root.clone())
    };

    let mut cg_ref = ctx.callgraph().borrow_mut();
    let graph = match cg_ref.as_mut() {
        Some(g) => g,
        None => {
            return Response::error(
                &req.id,
                "not_configured",
                "trace_to_symbol: project not configured — send 'configure' first",
            );
        }
    };

    let file_path = match validate_callgraph_path(req, ctx, file) {
        Ok(path) => path,
        Err(resp) => return resp,
    };

    let to_file_arg = req.params.get("toFile").and_then(|v| v.as_str());
    if let Some(to_file) = to_file_arg {
        let requested_path = resolve_request_path(project_root.as_deref(), to_file);
        if !requested_path.exists() {
            return Response::error(
                &req.id,
                "to_file_not_found",
                format!("trace_to_symbol: toFile not found: {}", to_file),
            );
        }
    }

    let to_file_path = match to_file_arg {
        Some(to_file) => match validate_callgraph_path(req, ctx, to_file) {
            Ok(path) => Some(path),
            Err(resp) => return resp,
        },
        None => None,
    };

    let symbol = match graph.resolve_symbol_query(&file_path, symbol) {
        Ok(symbol) => symbol,
        Err(e) => return Response::error(&req.id, e.code(), e.to_string()),
    };

    let target_candidates = match graph.trace_to_symbol_candidates(to_symbol, max_files) {
        Ok(candidates) => candidates,
        Err(err @ AftError::ProjectTooLarge { .. }) => {
            return Response::error(&req.id, "project_too_large", format!("{}", err));
        }
        Err(e) => return Response::error(&req.id, e.code(), e.to_string()),
    };

    if let Some(to_file_path) = to_file_path.as_deref() {
        if !target_candidates.iter().any(|candidate| {
            trace_candidate_matches_file(project_root.as_deref(), &candidate.file, to_file_path)
        }) {
            let candidates_json = serde_json::to_value(&target_candidates).unwrap_or_default();
            return Response::error_with_data(
                &req.id,
                "target_symbol_not_in_file",
                format!(
                    "trace_to_symbol: target symbol '{}' is not defined in toFile: {}",
                    to_symbol,
                    to_file_arg.unwrap_or("<unknown>")
                ),
                serde_json::json!({ "candidates": candidates_json }),
            );
        }
    } else {
        match target_candidates.len() {
            0 => {
                return Response::error(
                    &req.id,
                    "target_symbol_not_found",
                    format!("trace_to_symbol: target symbol '{}' not found", to_symbol),
                );
            }
            1 => {}
            _ => {
                let candidates_json = serde_json::to_value(&target_candidates).unwrap_or_default();
                return Response::error_with_data(
                    &req.id,
                    "ambiguous_target",
                    format!(
                        "trace_to_symbol: target symbol '{}' exists in multiple files; pass 'toFile' to disambiguate",
                        to_symbol
                    ),
                    serde_json::json!({ "candidates": candidates_json }),
                );
            }
        }
    }

    match graph.trace_to_symbol(
        &file_path,
        &symbol,
        to_symbol,
        to_file_path.as_deref(),
        depth,
        max_files,
    ) {
        Ok(result) => {
            let result_json = serde_json::to_value(&result).unwrap_or_default();
            Response::success(&req.id, result_json)
        }
        Err(err @ AftError::ProjectTooLarge { .. }) => {
            Response::error(&req.id, "project_too_large", format!("{}", err))
        }
        Err(e) => Response::error(&req.id, e.code(), e.to_string()),
    }
}

fn resolve_request_path(project_root: Option<&Path>, file: &str) -> PathBuf {
    let path = Path::new(file);
    if path.is_relative() {
        project_root
            .map(|root| root.join(path))
            .unwrap_or_else(|| path.to_path_buf())
    } else {
        path.to_path_buf()
    }
}

fn trace_candidate_matches_file(
    project_root: Option<&Path>,
    candidate_file: &str,
    target_file: &Path,
) -> bool {
    let candidate_path = resolve_request_path(project_root, candidate_file);
    canonicalize_for_compare(&candidate_path) == canonicalize_for_compare(target_file)
}

fn canonicalize_for_compare(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn validate_callgraph_path(
    req: &RawRequest,
    ctx: &AppContext,
    file: &str,
) -> Result<PathBuf, Response> {
    let file_path = ctx.validate_path(&req.id, Path::new(file))?;

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
            return Err(Response::error(
                &req.id,
                "path_outside_project_root",
                format!(
                    "Callgraph operations require paths inside project_root. Got: {} (project_root: {})",
                    file_path.display(),
                    project_root.display(),
                ),
            ));
        }
    }

    Ok(file_path)
}
