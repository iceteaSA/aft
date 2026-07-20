use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::context::AppContext;
use crate::protocol::{RawRequest, Response};

pub type DispatchFn<'a> = dyn Fn(RawRequest, &AppContext) -> Response + 'a;
pub type FinalizeFn<'a> = dyn Fn(&mut Response) + 'a;

/// Monotonic timestamps for one subc tool call. The recorder stays on the
/// request path and only takes an `Instant::now()` at each phase boundary.
#[derive(Debug)]
pub struct PhaseTrace {
    frame_decoded: Instant,
    executor_submitted: Option<Instant>,
    job_admitted: Option<Instant>,
    translate_done: Option<Instant>,
    execute_done: Option<Instant>,
    format_done: Option<Instant>,
    finalize_done: Option<Instant>,
}

#[derive(Debug, Clone, Copy)]
pub struct ToolCallEgressTiming {
    pub enqueued: Instant,
    pub dequeued: Instant,
    pub write_started: Instant,
    pub write_finished: Instant,
    pub frame_bytes: usize,
    pub queue_depth: usize,
    pub writer_active_at_enqueue: bool,
    pub writer_queue_was_full: bool,
    pub reserve_timeouts: u32,
}

#[derive(Debug, Clone, Copy)]
pub struct ToolCallPhaseDurations {
    pub queue: Duration,
    pub translate: Duration,
    pub execute: Duration,
    pub format: Duration,
    pub finalize: Duration,
    pub egress_enqueue: Duration,
    pub egress_queue: Duration,
    pub egress_prepare: Duration,
    pub egress_write: Duration,
    pub egress: Duration,
    pub frame_bytes: usize,
    pub writer_queue_depth: usize,
    pub writer_active_at_enqueue: bool,
    pub writer_queue_was_full: bool,
    pub writer_reserve_timeouts: u32,
    pub total: Duration,
}

impl PhaseTrace {
    pub fn new(frame_decoded: Instant) -> Self {
        Self {
            frame_decoded,
            executor_submitted: None,
            job_admitted: None,
            translate_done: None,
            execute_done: None,
            format_done: None,
            finalize_done: None,
        }
    }

    pub fn mark_executor_submitted(&mut self) {
        self.executor_submitted = Some(Instant::now());
    }

    pub fn mark_job_admitted(&mut self) {
        self.job_admitted = Some(Instant::now());
    }

    fn mark_translate_done(&mut self) {
        self.translate_done = Some(Instant::now());
    }

    fn mark_execute_done(&mut self) {
        self.execute_done = Some(Instant::now());
    }

    fn mark_format_done(&mut self) {
        self.format_done = Some(Instant::now());
    }

    fn mark_finalize_done(&mut self) {
        self.finalize_done = Some(Instant::now());
    }

    pub fn finish(self, egress: ToolCallEgressTiming) -> Option<ToolCallPhaseDurations> {
        let executor_submitted = self.executor_submitted?;
        let job_admitted = self.job_admitted?;
        let translate_done = self.translate_done?;
        let execute_done = self.execute_done?;
        let format_done = self.format_done?;
        let finalize_done = self.finalize_done?;
        Some(ToolCallPhaseDurations {
            queue: job_admitted.duration_since(executor_submitted),
            translate: translate_done.duration_since(job_admitted),
            execute: execute_done.duration_since(translate_done),
            format: format_done.duration_since(execute_done),
            finalize: finalize_done.duration_since(format_done),
            egress_enqueue: egress.enqueued.duration_since(finalize_done),
            egress_queue: egress.dequeued.duration_since(egress.enqueued),
            egress_prepare: egress.write_started.duration_since(egress.dequeued),
            egress_write: egress.write_finished.duration_since(egress.write_started),
            egress: egress.write_finished.duration_since(finalize_done),
            frame_bytes: egress.frame_bytes,
            writer_queue_depth: egress.queue_depth,
            writer_active_at_enqueue: egress.writer_active_at_enqueue,
            writer_queue_was_full: egress.writer_queue_was_full,
            writer_reserve_timeouts: egress.reserve_timeouts,
            total: egress.write_finished.duration_since(self.frame_decoded),
        })
    }
}

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
    args: Value,
    format_context: &crate::subc_format::FormatContext,
    ctx: &ToolCallContext,
    app_ctx: &AppContext,
    dispatch: &DispatchFn<'_>,
    finalizer: Option<&FinalizeFn<'_>>,
    mut phase_trace: Option<&mut PhaseTrace>,
) -> ToolCallOutcome {
    let sanitized_args = strip_agent_preview_arg_owned(args);
    let translate_context = crate::subc_translate::TranslateContext {
        diagnostics_on_edit: ctx.diagnostics_on_edit,
        preview: ctx.preview,
    };
    let (command, translated_args) = if crate::subc_translate::supports_tool(bare_name) {
        match crate::subc_translate::subc_translate_owned_with_context(
            bare_name,
            sanitized_args,
            ctx.project_root.as_path(),
            translate_context,
        ) {
            Ok(translated) => (translated.command, translated.args),
            Err(err) => {
                if let Some(trace) = phase_trace.as_mut() {
                    trace.mark_translate_done();
                    trace.mark_execute_done();
                }
                let response = Response::error(ctx.request_id.clone(), err.code, err.message);
                let result = tool_call_result_from_response(bare_name, format_context, response);
                if let Some(trace) = phase_trace.as_mut() {
                    trace.mark_format_done();
                    trace.mark_finalize_done();
                }
                return ToolCallOutcome::Unary(result);
            }
        }
    } else {
        let map = match sanitized_args {
            Value::Object(map) => map,
            _ => serde_json::Map::new(),
        };
        (bare_name.to_string(), map)
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
            if let Some(trace) = phase_trace.as_mut() {
                trace.mark_translate_done();
                trace.mark_execute_done();
            }
            let response = Response::error(
                ctx.request_id.clone(),
                "invalid_request",
                format!("failed to build request from tool call: {error}"),
            );
            let result = tool_call_result_from_response(bare_name, format_context, response);
            if let Some(trace) = phase_trace.as_mut() {
                trace.mark_format_done();
                trace.mark_finalize_done();
            }
            return ToolCallOutcome::Unary(result);
        }
    };
    if let Some(trace) = phase_trace.as_mut() {
        trace.mark_translate_done();
    }

    let mut response = dispatch(raw_req, app_ctx);
    if let Some(trace) = phase_trace.as_mut() {
        trace.mark_execute_done();
    }
    let text =
        crate::subc_format::format_response_with_context(bare_name, &response, format_context);
    if let Some(trace) = phase_trace.as_mut() {
        trace.mark_format_done();
    }
    if let Some(finalizer) = finalizer {
        finalizer(&mut response);
    }
    if let Some(trace) = phase_trace.as_mut() {
        trace.mark_finalize_done();
    }

    ToolCallOutcome::Unary(ToolCallResult { text, response })
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phase_trace_reports_execution_and_writer_egress_subphases() {
        let t0 = Instant::now();
        let trace = PhaseTrace {
            frame_decoded: t0,
            executor_submitted: Some(t0 + Duration::from_millis(1)),
            job_admitted: Some(t0 + Duration::from_millis(3)),
            translate_done: Some(t0 + Duration::from_millis(6)),
            execute_done: Some(t0 + Duration::from_millis(10)),
            format_done: Some(t0 + Duration::from_millis(15)),
            finalize_done: Some(t0 + Duration::from_millis(21)),
        };

        let phases = trace
            .finish(ToolCallEgressTiming {
                enqueued: t0 + Duration::from_millis(28),
                dequeued: t0 + Duration::from_millis(35),
                write_started: t0 + Duration::from_millis(37),
                write_finished: t0 + Duration::from_millis(48),
                frame_bytes: 262_144,
                queue_depth: 17,
                writer_active_at_enqueue: true,
                writer_queue_was_full: true,
                reserve_timeouts: 2,
            })
            .unwrap();

        assert_eq!(phases.queue, Duration::from_millis(2));
        assert_eq!(phases.translate, Duration::from_millis(3));
        assert_eq!(phases.execute, Duration::from_millis(4));
        assert_eq!(phases.format, Duration::from_millis(5));
        assert_eq!(phases.finalize, Duration::from_millis(6));
        assert_eq!(phases.egress_enqueue, Duration::from_millis(7));
        assert_eq!(phases.egress_queue, Duration::from_millis(7));
        assert_eq!(phases.egress_prepare, Duration::from_millis(2));
        assert_eq!(phases.egress_write, Duration::from_millis(11));
        assert_eq!(phases.egress, Duration::from_millis(27));
        assert_eq!(phases.frame_bytes, 262_144);
        assert_eq!(phases.writer_queue_depth, 17);
        assert!(phases.writer_active_at_enqueue);
        assert!(phases.writer_queue_was_full);
        assert_eq!(phases.writer_reserve_timeouts, 2);
        assert_eq!(phases.total, Duration::from_millis(48));
    }
}
