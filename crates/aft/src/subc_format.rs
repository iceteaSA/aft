//! Agent-facing text formatters for subc-mode tool results (parity with TS plugins).

use std::path::Path;

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FormatContext {
    pub agent_specified_range: bool,
    pub outline_mode: OutlineMode,
}

impl Default for FormatContext {
    fn default() -> Self {
        Self {
            agent_specified_range: false,
            outline_mode: OutlineMode::Text,
        }
    }
}

impl FormatContext {
    pub fn from_tool_call(bare_name: &str, arguments: &Value, project_root: &Path) -> Self {
        Self {
            agent_specified_range: agent_specified_read_range(arguments),
            outline_mode: outline_mode_for_call(bare_name, arguments, project_root),
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

fn is_core_agent_tool(bare_name: &str) -> bool {
    matches!(
        bare_name,
        "status" | "read" | "write" | "edit" | "grep" | "search" | "outline" | "inspect"
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
        return format_error(bare_name, data);
    }

    match bare_name {
        "edit" => format_edit_response(data),
        "write" => format_write_response(data),
        "read" => format_read(data, ctx.agent_specified_range),
        "grep" => format_grep(data),
        "search" => format_search(data),
        "outline" => format_outline(response, ctx.outline_mode),
        "inspect" => format_inspect(response),
        "status" => format_status(data),
        _ => unreachable!("core agent tools are exhaustive"),
    }
}

// Mirrors per-tool OpenCode wrapper error handling in packages/opencode-plugin/src/tools/*.ts.
fn format_error(bare_name: &str, data: &Value) -> String {
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
    let extra_count = attachments.len().saturating_sub(1);
    let suffix = if extra_count > 0 {
        format!("; +{extra_count} more")
    } else {
        String::new()
    };

    if kind == "image" || mime.starts_with("image/") {
        let dimensions = match (
            first.get("width").and_then(Value::as_u64),
            first.get("height").and_then(Value::as_u64),
        ) {
            (Some(width), Some(height)) => format!(", {width}×{height}"),
            _ => String::new(),
        };
        let size = size.map(|size| format!(", {size}")).unwrap_or_default();
        return Some(format!(
            "[image attachment: {mime}{dimensions}{size}{suffix} — inline delivery pending MCP image support]"
        ));
    }

    if kind == "pdf" || mime == "application/pdf" {
        let size = size.map(|size| format!(", {size}")).unwrap_or_default();
        return Some(format!(
            "[pdf attachment: {mime}{size}{suffix} — inline delivery pending MCP file support]"
        ));
    }

    let size = size.map(|size| format!(", {size}")).unwrap_or_default();
    Some(format!(
        "[attachment: {mime}{size}{suffix} — inline delivery pending MCP file support]"
    ))
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
