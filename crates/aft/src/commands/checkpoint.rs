use std::path::PathBuf;

use crate::context::AppContext;
use crate::protocol::{RawRequest, Response};

/// Handle the `checkpoint` command: create a named workspace checkpoint.
///
/// Params:
/// - `name` (string, required) — checkpoint name.
/// - `files` (array of strings, optional) — files to include. If omitted, uses
///   all files tracked by the backup store.
///
/// Returns: `{ name, file_count, created_at }`. When some tracked files have
/// been deleted since their last edit the checkpoint still succeeds for the
/// remaining files and adds a `skipped: [{ file, error }, ...]` array so the
/// caller can surface which paths were dropped.
pub fn handle_checkpoint(req: &RawRequest, ctx: &AppContext) -> Response {
    match handle_checkpoint_impl(req, ctx) {
        Ok(resp) | Err(resp) => resp,
    }
}

fn handle_checkpoint_impl(req: &RawRequest, ctx: &AppContext) -> Result<Response, Response> {
    let name = match req.params.get("name").and_then(|v| v.as_str()) {
        Some(n) => n,
        None => {
            return Ok(Response::error(
                &req.id,
                "invalid_request",
                "checkpoint: missing required param 'name'",
            ));
        }
    };

    let files: Vec<PathBuf> = req
        .params
        .get("files")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(PathBuf::from))
                .collect()
        })
        .unwrap_or_default();

    let file_list = if files.is_empty() {
        let backup = ctx.backup().lock();
        backup.tracked_files(req.session())
    } else {
        files
    };

    let validated_files = validate_checkpoint_files(&req.id, ctx, file_list)?;

    let backup = ctx.backup().lock();
    let mut checkpoint_store = ctx.checkpoint().lock();

    match checkpoint_store.create(req.session(), name, validated_files, &backup) {
        Ok(info) => {
            // Only surface `skipped` when we actually skipped something. Keeps
            // happy-path responses compact and backward-compatible for callers
            // that only read `name` / `file_count` / `created_at`.
            let mut payload = serde_json::json!({
                "name": info.name,
                "file_count": info.file_count,
                "created_at": info.created_at,
            });
            if !info.skipped.is_empty() {
                let skipped: Vec<_> = info
                    .skipped
                    .iter()
                    .map(|(p, err)| {
                        serde_json::json!({
                            "file": p.display().to_string(),
                            "error": err,
                        })
                    })
                    .collect();
                payload["skipped"] = serde_json::Value::Array(skipped);
            }
            Ok(Response::success(&req.id, payload))
        }
        Err(e) => Ok(Response::error(&req.id, e.code(), e.to_string())),
    }
}

fn validate_checkpoint_files(
    req_id: &str,
    ctx: &AppContext,
    files: Vec<PathBuf>,
) -> Result<Vec<PathBuf>, Response> {
    let mut validated = Vec::with_capacity(files.len());
    for path in files {
        validated.push(ctx.validate_path(req_id, &path)?);
    }
    Ok(validated)
}

/// Handle the `checkpoint_paths` command: return paths a checkpoint restore would write.
///
/// Params: `name` (string, required) — checkpoint name.
/// Returns: `{ name, paths, file_count }` without mutating checkpoint or filesystem state.
pub fn handle_checkpoint_paths(req: &RawRequest, ctx: &AppContext) -> Response {
    let name = match req.params.get("name").and_then(|v| v.as_str()) {
        Some(n) => n,
        None => {
            return Response::error(
                &req.id,
                "invalid_request",
                "checkpoint_paths: missing required param 'name'",
            );
        }
    };

    let checkpoint_store = ctx.checkpoint().lock();
    match checkpoint_store.absolute_file_paths(req.session(), name) {
        Ok(paths) => Response::success(
            &req.id,
            serde_json::json!({
                "name": name,
                "paths": paths.iter().map(|path| path.display().to_string()).collect::<Vec<_>>(),
                "file_count": paths.len(),
            }),
        ),
        Err(e) => Response::error(&req.id, e.code(), e.to_string()),
    }
}
