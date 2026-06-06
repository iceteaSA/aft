use std::path::Path;

use crate::commands::callgraph_store_adapter::{
    building_response, call_tree_result, store_error_response, unavailable_response,
};
use crate::context::{AppContext, CallgraphStoreAccess};
use crate::protocol::{RawRequest, Response};

/// Handle a `call_tree` request.
pub fn handle_call_tree(req: &RawRequest, ctx: &AppContext) -> Response {
    let file = match req.params.get("file").and_then(|v| v.as_str()) {
        Some(f) => f,
        None => {
            return Response::error(
                &req.id,
                "invalid_request",
                "call_tree: missing required param 'file'",
            );
        }
    };

    let symbol = match req.params.get("symbol").and_then(|v| v.as_str()) {
        Some(s) => s,
        None => {
            return Response::error(
                &req.id,
                "invalid_request",
                "call_tree: missing required param 'symbol'",
            );
        }
    };

    let depth = req
        .params
        .get("depth")
        .and_then(|v| v.as_u64())
        .unwrap_or(5)
        .min(100) as usize;

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

    let store = match ctx.callgraph_store_for_ops() {
        CallgraphStoreAccess::Ready(store) => store,
        CallgraphStoreAccess::Building => return building_response(&req.id, "call_tree"),
        CallgraphStoreAccess::Unavailable => {
            return unavailable_response(&req.id, "call_tree", ctx.is_worktree_bridge())
        }
        CallgraphStoreAccess::Error(error) => {
            return store_error_response(&req.id, "call_tree", error)
        }
    };

    match call_tree_result(&store, &file_path, symbol, depth) {
        Ok(tree) => {
            let tree_json = serde_json::to_value(&tree).unwrap_or_default();
            Response::success(&req.id, tree_json)
        }
        Err(error) => store_error_response(&req.id, "call_tree", error),
    }
}
