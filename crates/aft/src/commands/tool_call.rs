use serde_json::{json, Value};
use std::sync::OnceLock;

use crate::context::AppContext;
use crate::protocol::{RawRequest, Response};
use crate::run_tool_call::{
    run_tool_call, strip_agent_preview_arg, DispatchFn, ToolCallContext, ToolCallOutcome,
};

type StandaloneDispatch = fn(RawRequest, &AppContext) -> Response;

static STANDALONE_DISPATCH: OnceLock<StandaloneDispatch> = OnceLock::new();

pub fn register_dispatch(dispatch: StandaloneDispatch) {
    let _ = STANDALONE_DISPATCH.set(dispatch);
}

pub fn handle(req: &RawRequest, ctx: &AppContext) -> Response {
    let Some(dispatch) = STANDALONE_DISPATCH.get().copied() else {
        return Response::error(
            &req.id,
            "internal_error",
            "tool_call: standalone dispatcher is not registered",
        );
    };
    handle_with_dispatch(req, ctx, &dispatch)
}

fn handle_with_dispatch(req: &RawRequest, ctx: &AppContext, dispatch: &DispatchFn<'_>) -> Response {
    let Some(name) = req
        .params
        .get("name")
        .and_then(Value::as_str)
        .filter(|name| !name.is_empty())
    else {
        return Response::error(
            &req.id,
            "invalid_request",
            "tool_call: missing or invalid required string field 'name'",
        );
    };

    if name == "tool_call" {
        return Response::error(
            &req.id,
            "invalid_request",
            "tool_call: recursive tool_call requests are not supported",
        );
    }

    let arguments = req
        .params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let preview = req
        .params
        .get("preview")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let config = ctx.config();
    let project_root = config
        .project_root
        .clone()
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    let tool_ctx = ToolCallContext {
        project_root,
        session_id: Some(req.session().to_string()),
        request_id: req.id.clone(),
        diagnostics_on_edit: config.diagnostics_on_edit,
        preview,
    };

    let sanitized_arguments = strip_agent_preview_arg(&arguments);
    let format_context = crate::subc_format::FormatContext::from_tool_call(
        name,
        &sanitized_arguments,
        tool_ctx.project_root.as_path(),
    );

    match run_tool_call(
        name,
        &sanitized_arguments,
        &format_context,
        &tool_ctx,
        ctx,
        dispatch,
        None,
        None,
    ) {
        ToolCallOutcome::Unary(result) => response_with_text(result.response, result.text),
    }
}

fn response_with_text(mut response: Response, text: String) -> Response {
    if let Some(data) = response.data.as_object_mut() {
        data.insert("text".to_string(), Value::String(text));
    } else {
        response.data = json!({ "text": text });
    }
    response
}
