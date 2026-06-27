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

/// Single owner of per-call context (Oracle #4). diagnostics_on_edit has ONE source here.
#[derive(Debug, Clone)]
pub struct ToolCallContext {
    pub project_root: PathBuf,
    pub session_id: Option<String>,
    pub request_id: String,
    pub diagnostics_on_edit: bool,
}

pub fn run_tool_call(
    bare_name: &str,
    args: &Value,
    ctx: &ToolCallContext,
    app_ctx: &AppContext,
    dispatch: &DispatchFn<'_>,
) -> ToolCallOutcome {
    let format_context = crate::subc_format::FormatContext::from_tool_call(
        bare_name,
        args,
        ctx.project_root.as_path(),
    );
    let translate_context = crate::subc_translate::TranslateContext {
        diagnostics_on_edit: ctx.diagnostics_on_edit,
    };
    let (command, translated_args) = match crate::subc_translate::subc_translate_with_context(
        bare_name,
        args,
        ctx.project_root.as_path(),
        translate_context,
    ) {
        Ok(translated) => (translated.command, translated.args),
        Err(err) if is_subc_agent_core_tool(bare_name) => {
            let response = Response::error(ctx.request_id.clone(), err.code, err.message);
            return ToolCallOutcome::Unary(tool_call_result_from_response(
                bare_name,
                &format_context,
                response,
            ));
        }
        Err(_) => {
            let map = args.as_object().cloned().unwrap_or_default();
            (bare_name.to_string(), map)
        }
    };

    let mut map = translated_args;
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

fn tool_call_result_from_response(
    bare_name: &str,
    format_context: &crate::subc_format::FormatContext,
    response: Response,
) -> ToolCallResult {
    let text =
        crate::subc_format::format_response_with_context(bare_name, &response, format_context);
    ToolCallResult { text, response }
}

fn is_subc_agent_core_tool(name: &str) -> bool {
    matches!(
        name,
        "status" | "read" | "write" | "edit" | "grep" | "search" | "outline" | "inspect"
    )
}
