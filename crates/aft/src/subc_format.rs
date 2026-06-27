//! Agent-facing text formatters for subc-mode tool results (parity with TS plugins).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::protocol::Response;
use crate::subc_translate::resolve_path_from_project_root;
use serde_json::Value;

const MAX_UNCHECKED_FILES_IN_FOOTER: usize = 10;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutlineMode {
    Text,
    Files,
    DirectoryJson,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FormatContext {
    pub agent_specified_range: bool,
    pub outline_mode: OutlineMode,
    pub callgraph_op: Option<String>,
    pub callgraph_include_unresolved: bool,
    pub zoom_target_label: Option<String>,
}

impl Default for FormatContext {
    fn default() -> Self {
        Self {
            agent_specified_range: false,
            outline_mode: OutlineMode::Text,
            callgraph_op: None,
            callgraph_include_unresolved: false,
            zoom_target_label: None,
        }
    }
}

impl FormatContext {
    pub fn from_tool_call(bare_name: &str, arguments: &Value, project_root: &Path) -> Self {
        Self {
            agent_specified_range: agent_specified_read_range(arguments),
            outline_mode: outline_mode_for_call(bare_name, arguments, project_root),
            callgraph_op: callgraph_op_for_call(bare_name, arguments),
            callgraph_include_unresolved: callgraph_include_unresolved_for_call(
                bare_name, arguments,
            ),
            zoom_target_label: zoom_target_label_for_call(bare_name, arguments),
        }
    }
}

fn agent_specified_read_range(arguments: &Value) -> bool {
    let Some(obj) = arguments.as_object() else {
        return false;
    };
    obj.contains_key("startLine")
        || obj.contains_key("endLine")
        || obj.contains_key("offset")
        || obj.contains_key("limit")
}

fn outline_mode_for_call(bare_name: &str, arguments: &Value, project_root: &Path) -> OutlineMode {
    if bare_name != "outline" {
        return OutlineMode::Text;
    }
    let Some(obj) = arguments.as_object() else {
        return OutlineMode::Text;
    };
    if obj.get("files").and_then(Value::as_bool) == Some(true) {
        return OutlineMode::Files;
    }
    let Some(target) = obj.get("target").and_then(Value::as_str) else {
        return OutlineMode::Text;
    };
    if target.starts_with("http://") || target.starts_with("https://") {
        return OutlineMode::Text;
    }
    let resolved = resolve_path_from_project_root(project_root, target);
    if std::fs::metadata(resolved)
        .map(|m| m.is_dir())
        .unwrap_or(false)
    {
        OutlineMode::DirectoryJson
    } else {
        OutlineMode::Text
    }
}

fn callgraph_op_for_call(bare_name: &str, arguments: &Value) -> Option<String> {
    if bare_name != "callgraph" {
        return None;
    }
    arguments
        .as_object()
        .and_then(|obj| obj.get("op"))
        .and_then(Value::as_str)
        .filter(|op| !op.is_empty())
        .map(str::to_string)
}

fn callgraph_include_unresolved_for_call(bare_name: &str, arguments: &Value) -> bool {
    if bare_name != "callgraph" {
        return false;
    }
    arguments
        .as_object()
        .and_then(|obj| obj.get("includeUnresolved"))
        .is_some_and(coerce_boolean)
}

fn zoom_target_label_for_call(bare_name: &str, arguments: &Value) -> Option<String> {
    if bare_name != "zoom" {
        return None;
    }
    let obj = arguments.as_object()?;
    obj.get("filePath")
        .or_else(|| obj.get("url"))
        .and_then(Value::as_str)
        .filter(|label| !label.is_empty())
        .map(str::to_string)
}

fn coerce_boolean(value: &Value) -> bool {
    match value {
        Value::Bool(value) => *value,
        Value::Number(num) => num.as_i64() == Some(1) || num.as_u64() == Some(1),
        Value::String(raw) => {
            let normalized = raw.trim().to_ascii_lowercase();
            normalized == "true" || normalized == "1"
        }
        _ => false,
    }
}

// Return true for tools whose text output is formatted on the server for the
// agent. This list is larger than the subc manifest in subc.rs because zoom and
// callgraph are routed through NDJSON tool_call today, but their responses still
// need Rust formatting here.
fn is_core_agent_tool(bare_name: &str) -> bool {
    matches!(
        bare_name,
        "status"
            | "read"
            | "write"
            | "edit"
            | "grep"
            | "search"
            | "outline"
            | "zoom"
            | "inspect"
            | "callgraph"
    )
}

/// Render the text block for a tool `CallToolResult` from the structured AFT `Response`.
pub fn format_response(
    bare_name: &str,
    response: &Response,
    agent_specified_range: bool,
) -> String {
    let ctx = FormatContext {
        agent_specified_range,
        ..FormatContext::default()
    };
    format_response_with_context(bare_name, response, &ctx)
}

/// Render the text block for a tool `CallToolResult` from the structured AFT `Response`.
pub fn format_response_with_context(
    bare_name: &str,
    response: &Response,
    ctx: &FormatContext,
) -> String {
    if !is_core_agent_tool(bare_name) {
        return serde_json::to_string(response).unwrap_or_else(|_| "{}".to_string());
    }

    let data = &response.data;
    if !response.success {
        return format_error(bare_name, data, ctx);
    }

    match bare_name {
        "edit" => format_edit_response(data),
        "write" => format_write_response(data),
        "read" => format_read(data, ctx.agent_specified_range),
        "grep" => format_grep(data),
        "search" => format_search(data),
        "outline" => format_outline(response, ctx.outline_mode),
        "zoom" => format_zoom(data, ctx),
        "inspect" => format_inspect(response),
        "status" => format_status(data),
        "callgraph" => format_callgraph(
            ctx.callgraph_op.as_deref().unwrap_or("callgraph"),
            data,
            ctx.callgraph_include_unresolved,
        ),
        _ => unreachable!("core agent tools are exhaustive"),
    }
}

// Mirrors per-tool OpenCode wrapper error handling in packages/opencode-plugin/src/tools/*.ts.
fn format_error(bare_name: &str, data: &Value, ctx: &FormatContext) -> String {
    if bare_name == "callgraph" {
        return format_callgraph_error(ctx.callgraph_op.as_deref().unwrap_or("callgraph"), data);
    }
    let code = data
        .get("code")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty());
    let message = data
        .get("message")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .unwrap_or("request failed");
    match (bare_name, code) {
        ("search", Some(c)) => format!("semantic_search: {c} — {message}"),
        _ => message.to_string(),
    }
}

// Mirrors packages/opencode-plugin/src/tools/hoisted.ts createWriteTool.
fn format_write_response(data: &Value) -> String {
    if data.get("rolled_back").and_then(Value::as_bool) == Some(true) {
        return "Write rolled back: the content produced invalid syntax, so the file was left unchanged."
            .to_string();
    }

    let mut output = if data.get("created").and_then(Value::as_bool) == Some(true) {
        "Created new file.".to_string()
    } else {
        "File updated.".to_string()
    };
    if is_truthy_formatted(data) {
        output.push_str(" Auto-formatted.");
    }
    if data.get("no_op").and_then(Value::as_bool) == Some(true) {
        output.push_str(
            " No net change — the written content is byte-identical to what was already on disk.",
        );
    }
    append_lsp_error_lines(&mut output, data, true);
    append_lsp_server_notes(&mut output, data);
    output
}

// Mirrors packages/opencode-plugin/src/tools/hoisted.ts createEditTool.
fn format_edit_response(data: &Value) -> String {
    let mut result = format_edit_summary(data);

    if let Some(note) = format_glob_skip_reasons_note(data.get("format_skip_reasons")) {
        result.push_str("\n\n");
        result.push_str(&note);
    }
    if data.get("no_op").and_then(Value::as_bool) == Some(true) {
        result.push_str(
            "\n\nNote: no net file change — the match was found and applied, but the file content is byte-identical to before. Likely causes: oldString and newString are identical, or a formatter normalized the change away.",
        );
    }
    append_lsp_error_lines(&mut result, data, false);
    append_lsp_server_notes(&mut result, data);
    result
}

fn format_glob_skip_reasons_note(reasons: Option<&Value>) -> Option<String> {
    let actionable = reasons?
        .as_array()?
        .iter()
        .filter_map(Value::as_str)
        .filter(|reason| {
            matches!(
                *reason,
                "formatter_not_installed" | "formatter_excluded_path" | "timeout" | "error"
            )
        })
        .collect::<std::collections::BTreeSet<_>>();
    if actionable.is_empty() {
        None
    } else {
        Some(format!(
            "Note: formatter skipped some glob edit result file(s): {}. See per-file format_skipped_reason values for details.",
            actionable.into_iter().collect::<Vec<_>>().join(", ")
        ))
    }
}

fn append_lsp_error_lines(output: &mut String, data: &Value, trailing_newline: bool) {
    let errors = data
        .get("lsp_diagnostics")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter(|d| d.get("severity").and_then(Value::as_str) == Some("error"))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    if errors.is_empty() {
        return;
    }

    output.push_str("\n\nLSP errors detected, please fix:\n");
    let lines = errors
        .iter()
        .map(|d| {
            let line = d
                .get("line")
                .and_then(Value::as_u64)
                .map(|n| n.to_string())
                .unwrap_or_else(|| "undefined".to_string());
            let message = d
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("undefined");
            format!("  Line {line}: {message}")
        })
        .collect::<Vec<_>>();
    output.push_str(&lines.join("\n"));
    if trailing_newline {
        output.push('\n');
    }
}

fn append_lsp_server_notes(output: &mut String, data: &Value) {
    let pending = string_array(data.get("lsp_pending_servers"));
    if !pending.is_empty() {
        output.push_str(&format!(
            "\n\nNote: LSP server(s) did not respond in time: {}. Diagnostics may be incomplete; call aft_inspect for a checkpoint diagnostics snapshot.",
            pending.join(", ")
        ));
    }
    let exited = string_array(data.get("lsp_exited_servers"));
    if !exited.is_empty() {
        output.push_str(&format!(
            "\n\nNote: LSP server(s) exited during this edit: {}. Their diagnostics could not be collected.",
            exited.join(", ")
        ));
    }
}

// Mirrors packages/aft-bridge/src/edit-summary.ts formatEditSummary.
fn format_edit_summary(data: &Value) -> String {
    if data.get("rolled_back").and_then(Value::as_bool) == Some(true) {
        return "Edit rolled back: the change produced invalid syntax, so the file was left unchanged."
            .to_string();
    }

    if let Some(n) = data.get("files_modified").and_then(Value::as_u64) {
        let n = n as usize;
        return format!(
            "Applied edits to {} file{}.",
            n,
            if n == 1 { "" } else { "s" }
        );
    }

    if let Some(files) = data.get("total_files").and_then(Value::as_u64) {
        let files = files as usize;
        let reps = data
            .get("total_replacements")
            .and_then(Value::as_u64)
            .unwrap_or(0) as usize;
        return format!(
            "Edited {} file{} ({} replacement{}).",
            files,
            if files == 1 { "" } else { "s" },
            reps,
            if reps == 1 { "" } else { "s" }
        );
    }

    let additions = data
        .get("diff")
        .and_then(Value::as_object)
        .and_then(|d| d.get("additions"))
        .and_then(Value::as_u64)
        .unwrap_or(0) as usize;
    let deletions = data
        .get("diff")
        .and_then(Value::as_object)
        .and_then(|d| d.get("deletions"))
        .and_then(Value::as_u64)
        .unwrap_or(0) as usize;
    let counts = format!("+{additions}/-{deletions}");

    if data.get("created").and_then(Value::as_bool) == Some(true) {
        let mut s = format!("Created file ({counts}).");
        if is_truthy_formatted(data) {
            s.push_str(&format_auto_formatted_suffix(data));
        }
        return s;
    }

    let mut detail = counts.clone();
    if let Some(n) = data.get("edits_applied").and_then(Value::as_u64) {
        if n > 1 {
            detail = format!("{counts}, {n} edits");
        }
    } else if let Some(n) = data.get("replacements").and_then(Value::as_u64) {
        if n > 1 {
            detail = format!("{counts}, {n} replacements");
        }
    }

    let mut s = format!("Edited ({detail}).");
    if is_truthy_formatted(data) {
        s.push_str(&format_auto_formatted_suffix(data));
    }
    s
}

fn is_truthy_formatted(data: &Value) -> bool {
    data.get("formatted")
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

fn format_auto_formatted_suffix(data: &Value) -> String {
    let reformatted = data.get("reformatted").and_then(Value::as_object);
    if let Some(text) = reformatted
        .and_then(|r| r.get("text"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    {
        return format!(
            "\nAuto-formatted — the formatter reflowed your edit. On disk now:\n{text}"
        );
    }
    if reformatted
        .and_then(|r| r.get("extensive"))
        .and_then(Value::as_bool)
        == Some(true)
    {
        return " Auto-formatted — extensive reflow; re-read the file before your next anchored edit."
            .to_string();
    }
    " Auto-formatted.".to_string()
}

// Mirrors packages/opencode-plugin/src/tools/hoisted.ts createReadTool.
fn format_read(data: &Value, agent_specified_range: bool) -> String {
    if let Some(entries) = data.get("entries").and_then(Value::as_array) {
        return entries
            .iter()
            .filter_map(|e| e.as_str())
            .collect::<Vec<_>>()
            .join("\n");
    }

    if let Some(attachment_line) = format_read_attachments(data) {
        return attachment_line;
    }

    if data.get("binary").and_then(Value::as_bool).unwrap_or(false) {
        return data
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("Binary file")
            .to_string();
    }

    let mut text = data
        .get("content")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    text.push_str(&format_read_footer(agent_specified_range, data));
    text
}

fn format_read_attachments(data: &Value) -> Option<String> {
    let attachments = data.get("attachments")?.as_array()?;
    let has_host_attachment = attachments.iter().any(|attachment| {
        attachment.get("mime").and_then(Value::as_str).is_some()
            && attachment.get("data").and_then(Value::as_str).is_some()
    });
    if !has_host_attachment {
        return None;
    }

    if let Some(content) = data
        .get("content")
        .and_then(Value::as_str)
        .filter(|content| !content.is_empty())
    {
        return Some(content.to_string());
    }

    let first = attachments.first()?.as_object()?;
    let kind = first.get("kind").and_then(Value::as_str).unwrap_or("file");
    let mime = first
        .get("mime")
        .and_then(Value::as_str)
        .unwrap_or("application/octet-stream");
    let size = first
        .get("bytes")
        .and_then(Value::as_u64)
        .map(format_attachment_size);

    if kind == "image" || mime.starts_with("image/") {
        let dimensions = match (
            first.get("width").and_then(Value::as_u64),
            first.get("height").and_then(Value::as_u64),
        ) {
            (Some(width), Some(height)) => format!(", {width}×{height}"),
            _ => String::new(),
        };
        let resized = if first.get("resized").and_then(Value::as_bool) == Some(true) {
            ", resized"
        } else {
            ""
        };
        let size = size.map(|size| format!(", {size}")).unwrap_or_default();
        return Some(format!("Read image ({mime}{dimensions}{resized}{size})."));
    }

    if kind == "pdf" || mime == "application/pdf" {
        let size = size.map(|size| format!(" ({size})")).unwrap_or_default();
        return Some(format!("Read PDF{size}."));
    }

    let size = size.map(|size| format!(", {size}")).unwrap_or_default();
    Some(format!("Read attachment ({mime}{size})."))
}

fn format_attachment_size(bytes: u64) -> String {
    if bytes >= 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    } else if bytes >= 1024 {
        format!("{} KB", bytes.div_ceil(1024))
    } else {
        format!("{bytes} bytes")
    }
}

fn format_read_footer(agent_specified_range: bool, data: &Value) -> String {
    if agent_specified_range {
        return String::new();
    }
    if !data
        .get("truncated")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return String::new();
    }
    let start = data.get("start_line").and_then(Value::as_u64);
    let end = data.get("end_line").and_then(Value::as_u64);
    let total = data.get("total_lines").and_then(Value::as_u64);
    match (start, end, total) {
        (Some(start), Some(end), Some(total)) => format!(
            "\n(Showing lines {start}-{end} of {total}. Use startLine/endLine to read other sections.)"
        ),
        _ => String::new(),
    }
}

// Mirrors packages/opencode-plugin/src/tools/search.ts formatGrepOutput.
fn format_grep(data: &Value) -> String {
    if let Some(text) = data.get("text").and_then(Value::as_str) {
        return text.to_string();
    }

    let matches = data
        .get("matches")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let total_matches = data
        .get("total_matches")
        .and_then(Value::as_u64)
        .unwrap_or(matches.len() as u64);
    let files_with_matches = data
        .get("files_with_matches")
        .and_then(Value::as_u64)
        .unwrap_or_else(|| {
            matches
                .iter()
                .filter_map(|m| m.get("file").and_then(Value::as_str))
                .collect::<std::collections::BTreeSet<_>>()
                .len() as u64
        });

    if matches.is_empty() {
        return format!("Found {total_matches} match across {files_with_matches} file");
    }

    let body = matches
        .iter()
        .map(|m| {
            let file = m.get("file").and_then(Value::as_str).unwrap_or("unknown");
            let line = m.get("line").and_then(Value::as_u64).unwrap_or(0);
            let text = m
                .get("line_text")
                .or_else(|| m.get("text"))
                .and_then(Value::as_str)
                .unwrap_or("");
            format!("{file}:{line}: {text}")
        })
        .collect::<Vec<_>>()
        .join("\n");
    format!("{body}\n\nFound {total_matches} match across {files_with_matches} file")
}

// Mirrors packages/opencode-plugin/src/tools/semantic.ts semanticTools.
fn format_search(data: &Value) -> String {
    let note = extra_honesty_note(data);
    if let Some(text) = data
        .get("text")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    {
        return match note {
            Some(n) => format!("{text}\n{n}"),
            None => text.to_string(),
        };
    }
    semantic_honesty_note(data).unwrap_or_else(|| "No results.".to_string())
}

fn semantic_honesty_note(data: &Value) -> Option<String> {
    let mut notes = Vec::new();
    if data.get("more_available").and_then(Value::as_bool) == Some(true) {
        notes.push("more results available");
    }
    if data.get("engine_capped").and_then(Value::as_bool) == Some(true) {
        notes.push("enumeration capped");
    }
    if data.get("fully_degraded").and_then(Value::as_bool) == Some(true) {
        notes.push("fully degraded");
    }
    if data.get("complete").and_then(Value::as_bool) == Some(false) {
        notes.push("partial/incomplete");
    }
    if notes.is_empty() {
        None
    } else {
        Some(format!("Search status: {}.", notes.join("; ")))
    }
}

fn extra_honesty_note(data: &Value) -> Option<String> {
    let mut notes = Vec::new();
    if data.get("fully_degraded").and_then(Value::as_bool) == Some(true) {
        notes.push("fully degraded");
    }
    if data.get("complete").and_then(Value::as_bool) == Some(false) {
        notes.push("partial/incomplete");
    }
    if notes.is_empty() {
        None
    } else {
        Some(format!("Search status: {}.", notes.join("; ")))
    }
}

// Mirrors packages/opencode-plugin/src/tools/reading.ts aft_outline dispatch.
fn format_outline(response: &Response, mode: OutlineMode) -> String {
    match mode {
        OutlineMode::Text => format_outline_text(&response.data),
        OutlineMode::Files => format_outline_files_text(&response.data),
        OutlineMode::DirectoryJson => {
            serde_json::to_string_pretty(response).unwrap_or_else(|_| "{}".to_string())
        }
    }
}

// Mirrors packages/opencode-plugin/src/tools/reading.ts formatOutlineFilesText.
fn format_outline_files_text(data: &Value) -> String {
    let text = format_outline_text(data);
    let unchecked: Vec<String> = data
        .get("unchecked_files")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();

    let is_partial = data.get("complete").and_then(Value::as_bool) == Some(false)
        || data.get("walk_truncated").and_then(Value::as_bool) == Some(true)
        || !unchecked.is_empty();

    if !is_partial {
        return text;
    }

    let mut footer = Vec::new();
    if data.get("walk_truncated").and_then(Value::as_bool) == Some(true) {
        let suffix = if !unchecked.is_empty() {
            format!(
                " {} additional files in this directory were not indexed.",
                unchecked.len()
            )
        } else {
            " Some files in this directory were not indexed.".to_string()
        };
        footer.push(format!(
            "⚠ Partial result: walk truncated at 200 files.{suffix}"
        ));
    } else {
        let suffix = if !unchecked.is_empty() {
            format!(
                " {} files in this directory were not indexed.",
                unchecked.len()
            )
        } else {
            " Some files in this directory were not indexed.".to_string()
        };
        footer.push(format!("⚠ Partial result:{suffix}"));
    }

    if !unchecked.is_empty() {
        footer.push("Unchecked files:".to_string());
        for file in unchecked.iter().take(MAX_UNCHECKED_FILES_IN_FOOTER) {
            footer.push(format!("  {file}"));
        }
        let remaining = unchecked
            .len()
            .saturating_sub(MAX_UNCHECKED_FILES_IN_FOOTER);
        if remaining > 0 {
            footer.push(format!("  ... +{remaining} more"));
        }
    }

    if text.is_empty() {
        footer.join("\n")
    } else {
        format!("{text}\n\n{}", footer.join("\n"))
    }
}

fn format_outline_text(data: &Value) -> String {
    let text = data.get("text").and_then(Value::as_str).unwrap_or("");
    let skipped = data.get("skipped_files").and_then(Value::as_array);
    let Some(skipped) = skipped.filter(|s| !s.is_empty()) else {
        return text.to_string();
    };

    let lines: Vec<String> = skipped
        .iter()
        .filter_map(|item| {
            let obj = item.as_object()?;
            let file = obj.get("file").and_then(Value::as_str)?;
            let reason = obj
                .get("reason")
                .and_then(Value::as_str)
                .unwrap_or("skipped");
            Some(format!("  {file} — {reason}"))
        })
        .collect();
    if lines.is_empty() {
        return text.to_string();
    }
    let header = if text.is_empty() { "" } else { "\n\n" };
    format!(
        "{text}{header}Skipped {} file(s):\n{}",
        lines.len(),
        lines.join("\n")
    )
}

// Format zoom responses as plain text so direct calls and server-side calls
// produce identical output.
fn format_zoom(data: &Value, ctx: &FormatContext) -> String {
    if let Some(entries) = data.get("targets").and_then(Value::as_array) {
        return format_zoom_multi_target_result(entries);
    }

    let target_label = ctx.zoom_target_label.as_deref().unwrap_or("(no target)");
    if let Some((names, responses)) = unwrap_rust_zoom_batch_envelope(data) {
        return format_zoom_batch_result(target_label, &names, &responses);
    }
    format_zoom_text(target_label, data)
}

fn format_zoom_multi_target_result(entries: &[Value]) -> String {
    let rendered = entries
        .iter()
        .map(|entry| {
            let target_label = entry
                .get("targetLabel")
                .and_then(Value::as_str)
                .filter(|label| !label.is_empty())
                .unwrap_or("(no target)");
            let name = entry.get("name").and_then(Value::as_str).unwrap_or("");
            let response = entry.get("response");
            if response
                .and_then(|response| response.get("success"))
                .and_then(Value::as_bool)
                == Some(false)
            {
                let message = response
                    .and_then(|response| response.get("message"))
                    .and_then(Value::as_str)
                    .filter(|message| !message.is_empty())
                    .unwrap_or("zoom failed");
                return (
                    false,
                    format!("Symbol \"{name}\" not found in {target_label}: {message}"),
                );
            }
            match response {
                Some(response) => (true, format_zoom_text(target_label, response)),
                None => (
                    false,
                    format!("Symbol \"{name}\" not found in {target_label}: missing zoom response"),
                ),
            }
        })
        .collect::<Vec<_>>();

    let complete = rendered.iter().all(|(success, _)| *success);
    let mut sections = Vec::new();
    if !complete {
        sections.push("Incomplete zoom results: one or more symbols failed.".to_string());
    }
    sections.extend(rendered.into_iter().map(|(_, content)| content));
    sections.join("\n\n")
}

fn unwrap_rust_zoom_batch_envelope(data: &Value) -> Option<(Vec<String>, Vec<Value>)> {
    let symbols = data.get("symbols")?.as_array()?;
    if symbols.is_empty() {
        return None;
    }

    let mut names = Vec::with_capacity(symbols.len());
    let mut responses = Vec::with_capacity(symbols.len());
    for entry in symbols {
        let row = entry.as_object()?;
        let name = row.get("name")?.as_str()?;
        let response = row.get("response")?;
        if response.is_null() {
            return None;
        }
        names.push(name.to_string());
        responses.push(response.clone());
    }
    Some((names, responses))
}

fn format_zoom_batch_result(target_label: &str, symbols: &[String], responses: &[Value]) -> String {
    let entries = symbols
        .iter()
        .enumerate()
        .map(|(index, name)| {
            let response = responses.get(index);
            if response
                .and_then(|r| r.get("success"))
                .and_then(Value::as_bool)
                == Some(false)
            {
                let message = response
                    .and_then(|r| r.get("message"))
                    .and_then(Value::as_str)
                    .filter(|message| !message.is_empty())
                    .unwrap_or("zoom failed");
                return (false, format!("Symbol \"{name}\" not found: {message}"));
            }
            match response {
                Some(response) => (true, format_zoom_text(target_label, response)),
                None => (
                    false,
                    format!("Symbol \"{name}\" not found: missing zoom response"),
                ),
            }
        })
        .collect::<Vec<_>>();

    let complete = entries.iter().all(|(success, _)| *success);
    let mut sections = Vec::new();
    if !complete {
        sections.push("Incomplete zoom results: one or more symbols failed.".to_string());
    }
    sections.extend(entries.into_iter().map(|(_, content)| content));
    sections.join("\n\n")
}

fn format_zoom_text(target_label: &str, response: &Value) -> String {
    let range = response.get("range");
    let start_line = range
        .and_then(|range| range.get("start_line"))
        .and_then(Value::as_i64)
        .unwrap_or(1);
    let end_line = range
        .and_then(|range| range.get("end_line"))
        .and_then(Value::as_i64)
        .unwrap_or(start_line);
    let kind = response
        .get("kind")
        .and_then(Value::as_str)
        .unwrap_or("symbol");
    let name = response.get("name").and_then(Value::as_str).unwrap_or("");
    let content_text = response
        .get("content")
        .and_then(Value::as_str)
        .unwrap_or("");
    let context_before = string_array(response.get("context_before"));
    let context_after = string_array(response.get("context_after"));

    let header = if kind == "lines" {
        format!("{target_label}:{start_line}-{end_line}")
    } else {
        format!("{target_label}:{start_line}-{end_line} [{kind} {name}]")
            .trim_end()
            .to_string()
    };

    let mut content_lines = content_text.split('\n').collect::<Vec<_>>();
    if content_lines.last() == Some(&"") {
        content_lines.pop();
    }

    let last_displayed_line = end_line + context_after.len() as i64;
    let gutter_width = last_displayed_line.max(1).to_string().len();
    let mut out = vec![header, String::new()];

    let mut line_no = start_line - context_before.len() as i64;
    for text in &context_before {
        out.push(format_zoom_line(line_no, gutter_width, text));
        line_no += 1;
    }
    for text in content_lines {
        out.push(format_zoom_line(line_no, gutter_width, text));
        line_no += 1;
    }
    for text in &context_after {
        out.push(format_zoom_line(line_no, gutter_width, text));
        line_no += 1;
    }

    let annotations = response.get("annotations");
    let calls_out = annotations
        .and_then(|annotations| annotations.get("calls_out"))
        .and_then(Value::as_array);
    if let Some(calls_out) = calls_out.filter(|calls| !calls.is_empty()) {
        out.push(String::new());
        out.push("──── calls_out".to_string());
        for call in calls_out {
            out.push(format_zoom_call_ref(call));
        }
    }

    let called_by = annotations
        .and_then(|annotations| annotations.get("called_by"))
        .and_then(Value::as_array);
    if let Some(called_by) = called_by.filter(|calls| !calls.is_empty()) {
        out.push(String::new());
        out.push("──── called_by".to_string());
        for call in called_by {
            out.push(format_zoom_call_ref(call));
        }
    }

    out.join("\n")
}

fn format_zoom_line(line_no: i64, gutter_width: usize, text: &str) -> String {
    format!("{line_no:>gutter_width$}: {text}")
}

fn format_zoom_call_ref(call: &Value) -> String {
    let name = call.get("name").and_then(Value::as_str).unwrap_or("");
    let line = call.get("line").and_then(Value::as_i64).unwrap_or(0);
    let extra = call
        .get("extra_count")
        .and_then(Value::as_i64)
        .filter(|count| *count > 0)
        .map(|count| format!(" +{count}"))
        .unwrap_or_default();
    format!("  {name} (line {line}){extra}")
}

// Mirrors packages/opencode-plugin/src/tools/inspect.ts inspectTools.
fn format_inspect(response: &Response) -> String {
    if let Some(text) = response.data.get("text").and_then(Value::as_str) {
        return append_rendered_diagnostics(text, &response.data);
    }
    let json = serde_json::to_string_pretty(response).unwrap_or_else(|_| "{}".to_string());
    append_rendered_diagnostics(&json, &response.data)
}

// Mirrors packages/opencode-plugin/src/tools/inspect.ts appendRenderedDiagnostics.
fn append_rendered_diagnostics(text: &str, data: &Value) -> String {
    if text.lines().any(|line| {
        let lower = line.to_lowercase();
        lower.starts_with("diagnostics:") || lower.starts_with("diagnostics ")
    }) {
        return text.to_string();
    }
    let diagnostics = render_inspect_diagnostics(data);
    if diagnostics.is_empty() {
        return text.to_string();
    }
    if text.is_empty() {
        diagnostics
    } else {
        format!("{text}\n\n{diagnostics}")
    }
}

fn render_inspect_diagnostics(data: &Value) -> String {
    let mut lines = Vec::new();
    if let Some(summary_line) = format_diagnostics_summary(data.get("summary")) {
        lines.push(summary_line);
    }

    let detail_lines = format_diagnostics_details(data.get("details"));
    if !detail_lines.is_empty() {
        lines.push("diagnostics details:".to_string());
        for line in detail_lines {
            lines.push(format!("- {line}"));
        }
    }

    lines.join("\n")
}

fn format_diagnostics_summary(summary: Option<&Value>) -> Option<String> {
    let section = summary?.get("diagnostics")?.as_object()?;
    let errors = section.get("errors").and_then(Value::as_u64);
    let warnings = section.get("warnings").and_then(Value::as_u64);
    let info = section.get("info").and_then(Value::as_u64);
    let hints = section.get("hints").and_then(Value::as_u64);
    let has_counts = [errors, warnings, info, hints].iter().any(|v| v.is_some());
    let counts = format!(
        "{} errors, {} warnings, {} info, {} hints",
        errors.unwrap_or(0),
        warnings.unwrap_or(0),
        info.unwrap_or(0),
        hints.unwrap_or(0)
    );
    let status = section.get("status").and_then(Value::as_str);

    match status {
        Some("pending") => {
            if has_counts {
                Some(format!(
                    "diagnostics: {counts} so far — still pending (servers: {})",
                    diagnostics_server_summary(section)
                ))
            } else {
                Some(format!(
                    "diagnostics: pending (servers: {})",
                    diagnostics_server_summary(section)
                ))
            }
        }
        Some("incomplete") => {
            if has_counts {
                Some(format!(
                    "diagnostics: {counts} (incomplete — servers: {})",
                    diagnostics_server_summary(section)
                ))
            } else {
                Some(format!(
                    "diagnostics: unavailable (status incomplete; servers: {})",
                    diagnostics_server_summary(section)
                ))
            }
        }
        _ => {
            if has_counts {
                Some(format!("diagnostics: {counts}"))
            } else {
                None
            }
        }
    }
}

fn diagnostics_server_summary(section: &serde_json::Map<String, Value>) -> String {
    let pending = string_array(section.get("servers_pending"));
    let not_installed = string_array(section.get("servers_not_installed"));
    let mut parts = Vec::new();
    if !pending.is_empty() {
        parts.push(format!("pending: {}", pending.join(", ")));
    }
    if !not_installed.is_empty() {
        parts.push(format!("not installed: {}", not_installed.join(", ")));
    }
    if parts.is_empty() {
        "none reported".to_string()
    } else {
        parts.join("; ")
    }
}

fn string_array(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

fn format_diagnostics_details(details: Option<&Value>) -> Vec<String> {
    let Some(details) = details.and_then(Value::as_object) else {
        return Vec::new();
    };
    let Some(diagnostics) = details.get("diagnostics").and_then(Value::as_array) else {
        return Vec::new();
    };
    diagnostics
        .iter()
        .filter_map(|item| {
            let d = item.as_object()?;
            let severity = d
                .get("severity")
                .and_then(Value::as_str)
                .unwrap_or("information");
            let message = d
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("(no message)");
            let source = d.get("source").and_then(Value::as_str);
            let suffix = source.map(|s| format!(" [{s}]")).unwrap_or_default();
            Some(format!(
                "{} {} {}{}",
                format_diagnostic_location(d),
                severity,
                message,
                suffix
            ))
        })
        .collect()
}

fn format_diagnostic_location(d: &serde_json::Map<String, Value>) -> String {
    let file = d
        .get("file")
        .and_then(Value::as_str)
        .unwrap_or("(unknown file)");
    let line = d.get("line").and_then(Value::as_u64);
    let column = d.get("column").and_then(Value::as_u64);
    match (line, column) {
        (None, _) => file.to_string(),
        (Some(line), None) => format!("{file}:{line}"),
        (Some(line), Some(col)) => format!("{file}:{line}:{col}"),
    }
}

const UNRESOLVED_SUMMARY_NAME_LIMIT: usize = 10;

pub fn format_callgraph(op: &str, response_data: &Value, include_unresolved: bool) -> String {
    let Some(record) = response_data.as_object() else {
        return "No navigation result.".to_string();
    };

    let sections = match op {
        "call_tree" => format_call_tree_sections(record, include_unresolved),
        "callers" => format_callers_sections(record),
        "trace_to_symbol" => format_trace_to_symbol_sections(record),
        "trace_to" => format_trace_to_sections(record),
        "impact" => format_impact_sections(record),
        _ => format_trace_data_sections(record),
    };
    sections.join("\n")
}

fn format_callgraph_error(command: &str, data: &Value) -> String {
    let code = data
        .get("code")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty());
    let message = data
        .get("message")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .unwrap_or("callgraph failed");

    if matches!(
        code,
        Some("ambiguous_target") | Some("target_symbol_not_in_file")
    ) {
        let candidates = callgraph_candidates(data);
        if !candidates.is_empty() {
            let symbol =
                callgraph_error_symbol(data).or_else(|| symbol_from_callgraph_message(message));
            let target = symbol
                .map(|symbol| format!("multiple symbols named \"{symbol}\""))
                .unwrap_or_else(|| strip_terminal_punctuation(message));
            let action = if code == Some("ambiguous_target") {
                "Pass toFile to disambiguate"
            } else {
                "Try one of these files for toFile"
            };
            let mut lines = vec![format!(
                "{command}: {} — {target}. {action}:",
                code.unwrap_or_default()
            )];
            lines.extend(
                candidates
                    .into_iter()
                    .map(|candidate| format!("  - {candidate}")),
            );
            return lines.join("\n");
        }
    }

    let Some(code) = code else {
        return message.to_string();
    };
    let mut lines = vec![format!("{command}: {code} — {message}")];
    if let Some(extras) = collect_callgraph_error_extras(data) {
        lines.push(format!("data: {extras}"));
    }
    lines.join("\n")
}

fn callgraph_candidates(data: &Value) -> Vec<String> {
    data.get("candidates")
        .and_then(Value::as_array)
        .or_else(|| {
            data.get("data")
                .and_then(Value::as_object)
                .and_then(|nested| nested.get("candidates"))
                .and_then(Value::as_array)
        })
        .map(|items| {
            items
                .iter()
                .filter_map(|candidate| {
                    let candidate = candidate.as_object()?;
                    let file = string_field(candidate, "file")?;
                    let line = number_field(candidate, "line");
                    Some(match line {
                        Some(line) => format!("{file}:{line}"),
                        None => file.to_string(),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

fn callgraph_error_symbol(data: &Value) -> Option<String> {
    data.get("symbol")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .or_else(|| {
            data.get("data")
                .and_then(Value::as_object)
                .and_then(|nested| nested.get("symbol"))
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
        })
        .map(str::to_string)
}

fn symbol_from_callgraph_message(message: &str) -> Option<String> {
    extract_between(message, "target symbol '", "'")
        .or_else(|| extract_between(message, "multiple symbols named \"", "\""))
}

fn extract_between(message: &str, prefix: &str, suffix: &str) -> Option<String> {
    let start = message.find(prefix)? + prefix.len();
    let rest = &message[start..];
    let end = rest.find(suffix)?;
    let value = &rest[..end];
    (!value.is_empty()).then(|| value.to_string())
}

fn strip_terminal_punctuation(message: &str) -> String {
    message.trim_end_matches(['.', '!', '?']).to_string()
}

fn collect_callgraph_error_extras(data: &Value) -> Option<String> {
    let obj = data.as_object()?;
    let mut extras = serde_json::Map::new();
    for (key, value) in obj {
        if matches!(
            key.as_str(),
            "id" | "success" | "code" | "message" | "data" | "status_bar" | "bg_completions"
        ) {
            continue;
        }
        extras.insert(key.clone(), value.clone());
    }
    if extras.is_empty() {
        data.get("data").map(stringify_json_pretty)
    } else {
        if let Some(nested) = data.get("data") {
            extras.insert("data".to_string(), nested.clone());
        }
        Some(stringify_json_pretty(&Value::Object(extras)))
    }
}

fn stringify_json_pretty(value: &Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string())
}

fn format_call_tree_sections(
    record: &serde_json::Map<String, Value>,
    include_unresolved: bool,
) -> Vec<String> {
    let mut lines = Vec::new();
    render_call_tree_node(record, 0, &mut lines, include_unresolved);
    let warning = depth_warning(record, "depth_limited", "truncated");
    if !warning.is_empty() {
        lines.push(warning);
    }
    if lines.is_empty() {
        vec!["No call tree available.".to_string()]
    } else {
        lines
    }
}

fn render_call_tree_node(
    node: &serde_json::Map<String, Value>,
    depth: usize,
    lines: &mut Vec<String>,
    include_unresolved: bool,
) {
    let name = string_field(node, "name").unwrap_or("(unknown)");
    let file = shorten_path(string_field(node, "file").unwrap_or("(unknown file)"));
    let line = number_field(node, "line");
    let unresolved = if node.get("resolved").and_then(Value::as_bool) == Some(false) {
        " [unresolved]"
    } else {
        ""
    };
    let name_match = name_match_edge_marker(node);
    let location = match line {
        Some(line) => format!("[{file}:{line}]"),
        None => format!("[{file}]"),
    };
    lines.push(tree_line(
        depth,
        &format!("{name} {location}{unresolved}{name_match}"),
    ));

    let children = records_field(node, "children");
    if include_unresolved {
        for child in children {
            render_call_tree_node(child, depth + 1, lines, include_unresolved);
        }
        return;
    }

    let unresolved_indices = children
        .iter()
        .enumerate()
        .filter_map(|(index, child)| is_unresolved_leaf(child).then_some(index))
        .collect::<Vec<_>>();
    if unresolved_indices.is_empty() {
        for child in children {
            render_call_tree_node(child, depth + 1, lines, include_unresolved);
        }
        return;
    }

    let mut summary_inserted = false;
    for (index, child) in children.iter().enumerate() {
        if unresolved_indices.contains(&index) {
            if !summary_inserted {
                let unresolved_leaves = unresolved_indices
                    .iter()
                    .filter_map(|idx| children.get(*idx).copied())
                    .collect::<Vec<_>>();
                lines.push(tree_line(
                    depth + 1,
                    &unresolved_summary_text(&unresolved_leaves),
                ));
                summary_inserted = true;
            }
            continue;
        }
        render_call_tree_node(child, depth + 1, lines, include_unresolved);
    }
}

fn is_unresolved_leaf(node: &serde_json::Map<String, Value>) -> bool {
    node.get("resolved").and_then(Value::as_bool) == Some(false)
        && records_field(node, "children").is_empty()
}

fn unresolved_summary_text(nodes: &[&serde_json::Map<String, Value>]) -> String {
    let mut distinct_names = Vec::new();
    for node in nodes {
        let name = string_field(node, "name").unwrap_or("(unknown)");
        if !distinct_names.iter().any(|seen| seen == name) {
            distinct_names.push(name.to_string());
        }
    }

    let displayed = distinct_names
        .iter()
        .take(UNRESOLVED_SUMMARY_NAME_LIMIT)
        .cloned()
        .collect::<Vec<_>>();
    let hidden = distinct_names.len().saturating_sub(displayed.len());
    let names = if hidden > 0 {
        format!("{}, … (+{hidden} more)", displayed.join(", "))
    } else {
        displayed.join(", ")
    };
    let noun = if nodes.len() == 1 { "call" } else { "calls" };
    format!("+ {} unresolved external {noun}: {names}", nodes.len())
}

fn format_callers_sections(record: &serde_json::Map<String, Value>) -> Vec<String> {
    let groups = records_field(record, "callers");
    let warning = depth_warning(record, "depth_limited", "truncated");
    let hub_summary = hub_summary_line(record);
    let total = number_field(record, "total_callers").unwrap_or(0);
    let mut sections = vec![join_non_empty(&[
        Some(format!(
            "{total} caller{}",
            if total == 1 { "" } else { "s" }
        )),
        Some(format!(
            "{} file group{}",
            groups.len(),
            if groups.len() == 1 { "" } else { "s" }
        )),
        (!warning.is_empty()).then_some(warning),
    ])];
    if let Some(summary) = hub_summary {
        sections.push(summary);
    }
    for group in groups {
        sections.push(render_callers_group_lines(group).join("\n"));
    }
    sections
}

fn render_callers_group_lines(group: &serde_json::Map<String, Value>) -> Vec<String> {
    let file = shorten_path(string_field(group, "file").unwrap_or("(unknown file)"));
    let mut lines = vec![file];
    let callers = records_field(group, "callers");
    let mut by_symbol_provenance: BTreeMap<String, Vec<i64>> = BTreeMap::new();
    for caller in callers {
        let symbol = string_field(caller, "symbol").unwrap_or("(unknown)");
        let provenance = if string_field(caller, "resolved_by") == Some("name_match") {
            "name_match"
        } else {
            "exact"
        };
        let key = format!("{symbol}\0{provenance}");
        let bucket = by_symbol_provenance.entry(key).or_default();
        if let Some(line) = number_field(caller, "line") {
            bucket.push(line);
        }
    }
    for (key, mut line_nums) in by_symbol_provenance {
        let symbol = key.split('\0').next().unwrap_or("(unknown)");
        let is_name_match = key.ends_with("\0name_match");
        line_nums.sort_unstable();
        let line_part = if line_nums.is_empty() {
            "?".to_string()
        } else {
            line_nums
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        };
        let marker = if is_name_match { " ~" } else { "" };
        lines.push(format!("  ↳ {symbol}:{line_part}{marker}"));
    }
    lines
}

fn format_trace_to_symbol_sections(record: &serde_json::Map<String, Value>) -> Vec<String> {
    let path = records_field(record, "path");
    let complete = record.get("complete").and_then(Value::as_bool);
    let reason = string_field(record, "reason");
    if path.is_empty() {
        let prefix = if complete == Some(false) {
            "No complete path"
        } else {
            "No path"
        };
        return vec![match reason {
            Some(reason) => format!("{prefix} ({reason})"),
            None => prefix.to_string(),
        }];
    }

    let mut lines = vec![format!(
        "{} hop{}",
        path.len(),
        if path.len() == 1 { "" } else { "s" }
    )];
    for (index, hop) in path.iter().enumerate() {
        let symbol = string_field(hop, "symbol").unwrap_or("(unknown)");
        let file = shorten_path(string_field(hop, "file").unwrap_or("(unknown file)"));
        let line = number_field(hop, "line");
        let name_match = name_match_edge_marker(hop);
        let location = match line {
            Some(line) => format!("[{file}:{line}]"),
            None => format!("[{file}]"),
        };
        lines.push(tree_line(
            index + 1,
            &format!("{symbol} {location}{name_match}"),
        ));
    }
    lines
}

fn format_trace_to_sections(record: &serde_json::Map<String, Value>) -> Vec<String> {
    let paths = records_field(record, "paths");
    let warning = depth_warning(record, "max_depth_reached", "truncated_paths");
    let hub_summary = hub_summary_line(record);
    let total_paths = number_field(record, "total_paths").unwrap_or(paths.len() as i64);
    let entry_points = number_field(record, "entry_points_found").unwrap_or(0);
    let mut sections = vec![join_non_empty(&[
        Some(format!(
            "{total_paths} path{}",
            if total_paths == 1 { "" } else { "s" }
        )),
        Some(format!(
            "{entry_points} entry point{}",
            if entry_points == 1 { "" } else { "s" }
        )),
        (!warning.is_empty()).then_some(warning),
    ])];
    if let Some(summary) = hub_summary {
        sections.push(summary);
    }
    if paths.is_empty() {
        sections.push("No entry paths found.".to_string());
    }
    for (index, path) in paths.iter().enumerate() {
        let mut lines = Vec::new();
        render_trace_path(path, index, &mut lines);
        sections.push(lines.join("\n"));
    }
    sections
}

fn render_trace_path(path: &serde_json::Map<String, Value>, index: usize, lines: &mut Vec<String>) {
    lines.push(format!("Path {}", index + 1));
    for (hop_index, hop) in records_field(path, "hops").iter().enumerate() {
        let symbol = string_field(hop, "symbol").unwrap_or("(unknown)");
        let file = shorten_path(string_field(hop, "file").unwrap_or("(unknown file)"));
        let line = number_field(hop, "line");
        let entry = if hop.get("is_entry_point").and_then(Value::as_bool) == Some(true) {
            " [entry]"
        } else {
            ""
        };
        let name_match = name_match_edge_marker(hop);
        let location = match line {
            Some(line) => format!("[{file}:{line}]"),
            None => format!("[{file}]"),
        };
        lines.push(tree_line(
            hop_index + 1,
            &format!("{symbol}{entry} {location}{name_match}"),
        ));
    }
}

fn format_impact_sections(record: &serde_json::Map<String, Value>) -> Vec<String> {
    let callers = records_field(record, "callers");
    let warning = depth_warning(record, "depth_limited", "truncated");
    let hub_summary = hub_summary_line(record);
    let total_affected = number_field(record, "total_affected").unwrap_or(callers.len() as i64);
    let affected_files = number_field(record, "affected_files").unwrap_or(0);
    let mut sections = vec![join_non_empty(&[
        Some(format!(
            "{total_affected} affected call site{}",
            if total_affected == 1 { "" } else { "s" }
        )),
        Some(format!(
            "{affected_files} file{}",
            if affected_files == 1 { "" } else { "s" }
        )),
        (!warning.is_empty()).then_some(warning),
    ])];
    if let Some(summary) = hub_summary {
        sections.push(summary);
    }
    if callers.is_empty() {
        sections.push("No impacted callers found.".to_string());
    }
    for caller in callers {
        let file = shorten_path(string_field(caller, "caller_file").unwrap_or("(unknown file)"));
        let symbol = string_field(caller, "caller_symbol").unwrap_or("(unknown)");
        let line = number_field(caller, "line").unwrap_or(0);
        let entry = if caller.get("is_entry_point").and_then(Value::as_bool) == Some(true) {
            " [entry]"
        } else {
            ""
        };
        let name_match = name_match_edge_marker(caller);
        let expression = string_field(caller, "call_expression");
        let params = caller
            .get("parameters")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .map(value_to_plain_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .unwrap_or_default();
        let mut lines = vec![
            format!("{file}:{line}"),
            format!("  ↳ {symbol}{entry}{name_match}"),
        ];
        if let Some(expression) = expression {
            lines.push(format!("  {expression}"));
        }
        if !params.is_empty() {
            lines.push(format!("  params: {params}"));
        }
        sections.push(lines.join("\n"));
    }
    sections
}

fn format_trace_data_sections(record: &serde_json::Map<String, Value>) -> Vec<String> {
    let hops = records_field(record, "hops");
    let mut sections = vec![join_non_empty(&[
        Some(format!(
            "{} hop{}",
            hops.len(),
            if hops.len() == 1 { "" } else { "s" }
        )),
        (record.get("depth_limited").and_then(Value::as_bool) == Some(true))
            .then_some("(depth limited)".to_string()),
    ])];
    if hops.is_empty() {
        sections.push("No data-flow hops found.".to_string());
    }
    for (index, hop) in hops.iter().enumerate() {
        let file = shorten_path(string_field(hop, "file").unwrap_or("(unknown file)"));
        let symbol = string_field(hop, "symbol").unwrap_or("(unknown)");
        let variable = string_field(hop, "variable").unwrap_or("(unknown)");
        let line = number_field(hop, "line").unwrap_or(0);
        let approximate = if hop.get("approximate").and_then(Value::as_bool) == Some(true) {
            " [approx]"
        } else {
            ""
        };
        let name_match = name_match_edge_marker(hop);
        let flow_type = string_field(hop, "flow_type").unwrap_or("flow");
        sections.push(tree_line(
            index,
            &format!("{variable} {flow_type} {symbol} [{file}:{line}]{approximate}{name_match}"),
        ));
    }
    sections
}

fn records_field<'a>(
    record: &'a serde_json::Map<String, Value>,
    key: &str,
) -> Vec<&'a serde_json::Map<String, Value>> {
    record
        .get(key)
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(Value::as_object).collect())
        .unwrap_or_default()
}

fn string_field<'a>(record: &'a serde_json::Map<String, Value>, key: &str) -> Option<&'a str> {
    record.get(key).and_then(Value::as_str)
}

fn number_field(record: &serde_json::Map<String, Value>, key: &str) -> Option<i64> {
    let value = record.get(key)?;
    value
        .as_i64()
        .or_else(|| value.as_u64().and_then(|n| i64::try_from(n).ok()))
}

fn shorten_path(path: &str) -> String {
    let Some(home) = home_dir() else {
        return path.to_string();
    };
    let home = home.to_string_lossy().to_string();
    if path.starts_with(&home) {
        format!("~{}", &path[home.len()..])
    } else {
        path.to_string()
    }
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

fn tree_line(depth: usize, text: &str) -> String {
    format!(
        "{}{}{}",
        "  ".repeat(depth),
        if depth == 0 { "" } else { "↳ " },
        text
    )
}

fn name_match_edge_marker(record: &serde_json::Map<String, Value>) -> &'static str {
    if string_field(record, "resolved_by") == Some("name_match") {
        " ~"
    } else {
        ""
    }
}

fn depth_warning(
    response: &serde_json::Map<String, Value>,
    depth_field: &str,
    truncated_field: &str,
) -> String {
    let limited = response.get(depth_field).and_then(Value::as_bool);
    let truncated = number_field(response, truncated_field).unwrap_or(0);
    if limited != Some(true) && truncated == 0 {
        return String::new();
    }
    let detail = if truncated > 0 {
        format!(", {truncated} truncated")
    } else {
        String::new()
    };
    format!("(depth limited{detail})")
}

fn hub_summary_line(response: &serde_json::Map<String, Value>) -> Option<String> {
    response
        .get("hub_summary")
        .and_then(Value::as_object)
        .and_then(|summary| string_field(summary, "message"))
        .map(str::to_string)
}

fn join_non_empty(parts: &[Option<String>]) -> String {
    parts
        .iter()
        .filter_map(|part| part.as_deref())
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" · ")
}

fn value_to_plain_string(value: &Value) -> String {
    value
        .as_str()
        .map(str::to_string)
        .unwrap_or_else(|| value.to_string())
}

// Status has no TypeScript wrapper; this mirrors the subc bare status fallback.
fn format_status(data: &Value) -> String {
    if let Some(text) = data
        .get("text")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    {
        return text.to_string();
    }
    serde_json::to_string_pretty(data).unwrap_or_else(|_| "{}".to_string())
}
