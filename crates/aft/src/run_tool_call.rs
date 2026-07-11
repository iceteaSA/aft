use std::borrow::Cow;
use std::path::PathBuf;

use serde_json::{json, Value};

use crate::context::AppContext;
use crate::protocol::{RawRequest, Response};

pub type DispatchFn<'a> = dyn Fn(RawRequest, &AppContext) -> Response + 'a;

/// The full result of a tool call: the COMPLETE dispatch Response carried VERBATIM,
/// plus the server-rendered agent-facing text (what the deleted TS formatters used to produce).
/// Oracle #1: carry the WHOLE Response — promote nothing, drop nothing (preview_diff, attachments,
/// status_bar, bg_completions, lsp_diagnostics, code, message, candidates, … all ride inside `response`).
#[derive(Debug)]
pub struct ToolCallResult {
    pub text: String,
    pub response: crate::protocol::Response,
}

/// Reserve a discriminated seam so bash/PTY/streaming (P3) doesn't force a signature rewrite.
/// Only `Unary` is constructed today. Do NOT build `Stream`.
#[derive(Debug)]
pub enum ToolCallOutcome {
    Unary(ToolCallResult),
}

/// Server-owned settings for a single `tool_call` request.
/// These fields cannot be supplied through the agent's arguments object.
#[derive(Debug, Clone)]
pub struct ToolCallContext {
    pub project_root: PathBuf,
    pub session_id: Option<String>,
    pub request_id: String,
    pub diagnostics_on_edit: bool,
    pub preview: bool,
}

pub fn run_tool_call(
    bare_name: &str,
    args: &Value,
    format_context: &crate::subc_format::FormatContext,
    ctx: &ToolCallContext,
    app_ctx: &AppContext,
    dispatch: &DispatchFn<'_>,
) -> ToolCallOutcome {
    let sanitized_args = strip_agent_preview_arg(args);
    let translate_context = crate::subc_translate::TranslateContext {
        diagnostics_on_edit: ctx.diagnostics_on_edit,
        preview: ctx.preview,
    };
    let (command, translated_args) = match crate::subc_translate::subc_translate_with_context(
        bare_name,
        &sanitized_args,
        ctx.project_root.as_path(),
        translate_context,
    ) {
        Ok(translated) => (translated.command, translated.args),
        // Return validation errors from the translation step immediately. Only
        // the special unsupported_tool error can fall through, allowing native
        // NDJSON commands such as configure/undo to reach dispatch unchanged.
        Err(err) if err.code != "unsupported_tool" => {
            let response = Response::error(ctx.request_id.clone(), err.code, err.message);
            return ToolCallOutcome::Unary(tool_call_result_from_response(
                bare_name,
                &format_context,
                response,
            ));
        }
        Err(_) => {
            let map = sanitized_args.as_object().cloned().unwrap_or_default();
            (bare_name.to_string(), map)
        }
    };

    let mut map = translated_args;
    if ctx.preview {
        map.insert("preview".to_string(), json!(true));
    }
    map.insert("id".to_string(), json!(ctx.request_id.clone()));
    map.insert("command".to_string(), json!(command));
    map.insert("session_id".to_string(), json!(ctx.session_id.clone()));

    let raw_req = match serde_json::from_value::<RawRequest>(Value::Object(map)) {
        Ok(req) => req,
        Err(error) => {
            let response = Response::error(
                ctx.request_id.clone(),
                "invalid_request",
                format!("failed to build request from tool call: {error}"),
            );
            return ToolCallOutcome::Unary(tool_call_result_from_response(
                bare_name,
                &format_context,
                response,
            ));
        }
    };

    let response = dispatch(raw_req, app_ctx);
    ToolCallOutcome::Unary(tool_call_result_from_response(
        bare_name,
        &format_context,
        response,
    ))
}

pub(crate) fn strip_agent_preview_arg(args: &Value) -> Cow<'_, Value> {
    let Some(map) = args.as_object() else {
        return Cow::Borrowed(args);
    };
    if !map.contains_key("preview") {
        return Cow::Borrowed(args);
    }

    let mut sanitized = map.clone();
    sanitized.remove("preview");
    Cow::Owned(Value::Object(sanitized))
}

pub(crate) fn strip_agent_preview_arg_owned(mut args: Value) -> Value {
    if let Some(map) = args.as_object_mut() {
        map.remove("preview");
    }
    args
}

fn tool_call_result_from_response(
    bare_name: &str,
    format_context: &crate::subc_format::FormatContext,
    response: Response,
) -> ToolCallResult {
    let text =
        crate::subc_format::format_response_with_context(bare_name, &response, format_context);
    ToolCallResult { text, response }
}
