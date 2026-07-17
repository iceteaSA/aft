use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::commands::outline::symbol_to_entry;
use crate::commands::symbol_render::{
    build_container_outline, format_qualified_entry, might_have_container_members,
    qualified_symbol_name, render_container_member_menu, should_return_member_menu,
    symbol_kind_string,
};
use crate::context::AppContext;
use crate::edit::line_col_to_byte;
use crate::language::LanguageProvider;
use crate::lsp_hints;
use crate::parser::{detect_language, FileParser, LangId};
use crate::protocol::{RawRequest, Response};
use crate::symbols::{Range, Symbol, SymbolKind, SymbolMatch};
use crate::url_fetch::{fetch_url_to_cache, is_http_url, UrlFetchOptions};

/// A reference to a called/calling function.
#[derive(Debug, Clone, Serialize)]
pub struct CallRef {
    pub name: String,
    /// 1-based line number of the call reference.
    pub line: u32,
    /// Number of later call sites with the same callee or caller name merged into this entry.
    #[serde(skip_serializing_if = "is_zero")]
    pub extra_count: u32,
}

fn is_zero(value: &u32) -> bool {
    *value == 0
}

fn dedupe_call_refs_by_name(calls: Vec<CallRef>) -> Vec<CallRef> {
    let mut index_by_name: HashMap<String, usize> = HashMap::new();
    let mut deduped: Vec<CallRef> = Vec::new();

    for call in calls {
        if let Some(index) = index_by_name.get(&call.name).copied() {
            deduped[index].extra_count = deduped[index]
                .extra_count
                .saturating_add(call.extra_count.saturating_add(1));
        } else {
            index_by_name.insert(call.name.clone(), deduped.len());
            deduped.push(call);
        }
    }

    deduped
}

/// Annotations describing file-scoped call relationships.
#[derive(Debug, Clone, Serialize)]
pub struct Annotations {
    pub calls_out: Vec<CallRef>,
    pub called_by: Vec<CallRef>,
}

/// Response payload for the zoom command.
#[derive(Debug, Clone, Serialize)]
pub struct ZoomResponse {
    pub name: String,
    pub kind: String,
    pub range: Range,
    pub content: String,
    pub context_before: Vec<String>,
    pub context_after: Vec<String>,
    pub annotations: Annotations,
}

struct RawCall {
    name: String,
    line: u32,
    start_byte: usize,
    end_byte: usize,
}

fn resolve_file_or_url(
    req: &RawRequest,
    ctx: &AppContext,
    file: &str,
) -> Result<PathBuf, Response> {
    if is_http_url(file) {
        let storage_dir = crate::bash_background::storage_dir(ctx.config().storage_dir.as_deref());
        let allow_private = ctx.config().url_fetch_allow_private
            || req
                .params
                .get("allow_private")
                .and_then(|value| value.as_bool())
                .unwrap_or(false);
        return fetch_url_to_cache(
            file,
            &storage_dir,
            UrlFetchOptions {
                allow_private,
                ..UrlFetchOptions::default()
            },
        )
        .map_err(|error| Response::error(&req.id, "url_fetch_failed", error.to_string()));
    }

    ctx.validate_path(&req.id, Path::new(file))
}

fn zoom_one_target_response(
    req: &RawRequest,
    ctx: &AppContext,
    file: &str,
    symbol: &str,
    context_lines: usize,
    include_callgraph: bool,
) -> Response {
    let path = match resolve_file_or_url(req, ctx, file) {
        Ok(path) => path,
        Err(resp) => return resp,
    };
    if !path.exists() {
        return Response::error(
            &req.id,
            "file_not_found",
            format!("file not found: {}", file),
        );
    }

    let source = match std::fs::read_to_string(&path) {
        Ok(source) => source,
        Err(error) => {
            return Response::error(&req.id, "file_not_found", format!("{}: {}", file, error));
        }
    };
    let lines: Vec<String> = source.lines().map(|line| line.to_string()).collect();

    zoom_one_symbol(
        req,
        ctx,
        &path,
        file,
        &source,
        &lines,
        symbol,
        context_lines,
        include_callgraph,
    )
}

fn serialize_zoom_target_response(req: &RawRequest, response: Response) -> serde_json::Value {
    serde_json::to_value(&response).unwrap_or_else(|error| {
        serde_json::to_value(Response::error(
            &req.id,
            "internal_error",
            format!("zoom: failed to serialize target response: {error}"),
        ))
        .expect("serializing Response::error should not fail")
    })
}

fn handle_zoom_targets(
    req: &RawRequest,
    ctx: &AppContext,
    targets: &[serde_json::Value],
    context_lines: usize,
    include_callgraph: bool,
) -> Response {
    if targets.is_empty() {
        return Response::error(
            &req.id,
            "invalid_request",
            "zoom: 'targets' must be a non-empty array",
        );
    }

    let mut entries = Vec::with_capacity(targets.len());
    for (index, target) in targets.iter().enumerate() {
        let obj = target.as_object();
        let Some(file) = obj
            .and_then(|obj| obj.get("file"))
            .and_then(|value| value.as_str())
            .filter(|file| !file.is_empty())
        else {
            return Response::error(
                &req.id,
                "invalid_request",
                format!("zoom: targets[{index}].file must be a non-empty string"),
            );
        };
        let Some(symbol) = obj
            .and_then(|obj| obj.get("symbol"))
            .and_then(|value| value.as_str())
            .filter(|symbol| !symbol.is_empty())
        else {
            return Response::error(
                &req.id,
                "invalid_request",
                format!("zoom: targets[{index}].symbol must be a non-empty string"),
            );
        };
        let target_label = obj
            .and_then(|obj| obj.get("target_label").or_else(|| obj.get("targetLabel")))
            .and_then(|value| value.as_str())
            .filter(|label| !label.is_empty())
            .unwrap_or(file);

        let response =
            zoom_one_target_response(req, ctx, file, symbol, context_lines, include_callgraph);
        entries.push(serde_json::json!({
            "targetLabel": target_label,
            "name": symbol,
            "response": serialize_zoom_target_response(req, response),
        }));
    }

    Response::success(
        &req.id,
        serde_json::json!({
            "targets": entries,
        }),
    )
}

/// Handle a `zoom` request.
///
/// Expects either `file` plus `symbol`/`symbols`, or a cross-file `targets` array,
/// with optional `context_lines` (default 3). Resolves the symbol, extracts body +
/// context, and walks ASTs for call annotations. For code files, a whitespace-separated
/// top-level `symbol`/`symbols` string is split into multiple same-file lookups.
pub fn handle_zoom(req: &RawRequest, ctx: &AppContext) -> Response {
    let context_lines = req
        .params
        .get("context_lines")
        .and_then(|v| v.as_u64())
        .unwrap_or(3) as usize;
    let include_callgraph = req
        .params
        .get("callgraph")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    if let Some(targets_value) = req.params.get("targets") {
        let Some(targets) = targets_value.as_array() else {
            return Response::error(
                &req.id,
                "invalid_request",
                "zoom: 'targets' must be a non-empty array",
            );
        };
        return handle_zoom_targets(req, ctx, targets, context_lines, include_callgraph);
    }

    let file = match req
        .params
        .get("file")
        .or_else(|| req.params.get("url"))
        .and_then(|v| v.as_str())
    {
        Some(f) => f,
        None => {
            return Response::error(
                &req.id,
                "invalid_request",
                "zoom: missing required param 'file'",
            );
        }
    };

    let start_line = req
        .params
        .get("start_line")
        .and_then(|v| v.as_u64())
        .map(|v| v as usize);
    let end_line = req
        .params
        .get("end_line")
        .and_then(|v| v.as_u64())
        .map(|v| v as usize);

    let path = match resolve_file_or_url(req, ctx, file) {
        Ok(path) => path,
        Err(resp) => return resp,
    };
    if !path.exists() {
        return Response::error(
            &req.id,
            "file_not_found",
            format!("file not found: {}", file),
        );
    }

    // Read source file early because both symbol mode and line-range mode need it.
    let source = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) => {
            return Response::error(&req.id, "file_not_found", format!("{}: {}", file, e));
        }
    };

    let lines: Vec<String> = source.lines().map(|l| l.to_string()).collect();

    // Line-range mode: read arbitrary lines without requiring a symbol.
    match (start_line, end_line) {
        (Some(start), Some(end)) => {
            if zoom_symbol_param(&req.params).is_some() {
                return Response::error(
                    &req.id,
                    "invalid_request",
                    "zoom: provide either 'symbol' OR ('start_line' and 'end_line'), not both",
                );
            }
            if start == 0 || end == 0 {
                return Response::error(
                    &req.id,
                    "invalid_request",
                    "zoom: 'start_line' and 'end_line' are 1-based and must be >= 1",
                );
            }
            if end < start {
                return Response::error(
                    &req.id,
                    "invalid_request",
                    format!("zoom: end_line {} must be >= start_line {}", end, start),
                );
            }
            if lines.is_empty() {
                return Response::error(
                    &req.id,
                    "invalid_request",
                    format!("zoom: {} is empty", file),
                );
            }

            let start_idx = start - 1;
            // Clamp end_line to file length (same as batch edits)
            let clamped_end = end.min(lines.len());
            let end_idx = clamped_end - 1;
            if start_idx >= lines.len() {
                return Response::error(
                    &req.id,
                    "invalid_request",
                    format!(
                        "zoom: start_line {} is past end of {} ({} lines)",
                        start,
                        file,
                        lines.len()
                    ),
                );
            }

            let content = lines[start_idx..=end_idx].join("\n");
            let ctx_start = start_idx.saturating_sub(context_lines);
            let context_before: Vec<String> = if ctx_start < start_idx {
                lines[ctx_start..start_idx]
                    .iter()
                    .map(|l| l.to_string())
                    .collect()
            } else {
                vec![]
            };
            let ctx_end = (end_idx + 1 + context_lines).min(lines.len());
            let context_after: Vec<String> = if end_idx + 1 < lines.len() {
                lines[(end_idx + 1)..ctx_end]
                    .iter()
                    .map(|l| l.to_string())
                    .collect()
            } else {
                vec![]
            };
            let end_col = lines[end_idx].chars().count() as u32;

            return Response::success(
                &req.id,
                serde_json::json!({
                    "name": format!("lines {}-{}", start, clamped_end),
                    "kind": "lines",
                    "range": {
                        "start_line": start,  // already 1-based from user input
                        "start_col": 1,
                        "end_line": clamped_end,
                        "end_col": end_col + 1,
                    },
                    "content": content,
                    "context_before": context_before,
                    "context_after": context_after,
                    "annotations": {
                        "calls_out": [],
                        "called_by": [],
                    },
                }),
            );
        }
        (Some(_), None) | (None, Some(_)) => {
            return Response::error(
                &req.id,
                "invalid_request",
                "zoom: provide both 'start_line' and 'end_line' for line-range mode",
            );
        }
        (None, None) => {}
    }

    let lang = detect_language(&path);
    let symbol_names = match parse_zoom_symbol_names(&req.params, lang) {
        Ok(names) => names,
        Err(resp) => return resp,
    };

    if symbol_names.is_empty() {
        return Response::error(
            &req.id,
            "invalid_request",
            "zoom: missing required param 'symbol'",
        );
    }

    if symbol_names.len() == 1 {
        return zoom_one_symbol(
            req,
            ctx,
            &path,
            file,
            &source,
            &lines,
            &symbol_names[0],
            context_lines,
            include_callgraph,
        );
    }

    zoom_batch_symbols(
        req,
        ctx,
        &path,
        file,
        &source,
        &lines,
        &symbol_names,
        context_lines,
        include_callgraph,
    )
}

/// Raw `symbol` or `symbols` param before language-aware splitting.
fn zoom_symbol_param(params: &serde_json::Value) -> Option<&str> {
    params
        .get("symbol")
        .or_else(|| params.get("symbols"))
        .and_then(|v| v.as_str())
}

fn is_heading_zoom_language(lang: Option<LangId>) -> bool {
    matches!(lang, Some(LangId::Markdown | LangId::Html))
}

/// Normalize `symbol` / `symbols` into one or more lookup names.
///
/// For code files, a single string containing internal whitespace is split on `\s+`.
/// Markdown/HTML headings keep the full string (headings may contain spaces).
fn parse_zoom_symbol_names(
    params: &serde_json::Value,
    lang: Option<LangId>,
) -> Result<Vec<String>, Response> {
    if let Some(arr) = params.get("symbols").and_then(|v| v.as_array()) {
        let names: Vec<String> = arr
            .iter()
            .filter_map(|v| v.as_str().map(str::trim))
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect();
        return Ok(names);
    }

    let Some(raw) = zoom_symbol_param(params) else {
        return Ok(Vec::new());
    };

    if is_heading_zoom_language(lang) {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Ok(Vec::new());
        }
        return Ok(vec![trimmed.to_string()]);
    }

    if raw.split_whitespace().count() <= 1 {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Ok(Vec::new());
        }
        return Ok(vec![trimmed.to_string()]);
    }

    Ok(raw.split_whitespace().map(str::to_string).collect())
}

fn zoom_batch_symbols(
    req: &RawRequest,
    ctx: &AppContext,
    path: &Path,
    file: &str,
    source: &str,
    lines: &[String],
    symbol_names: &[String],
    context_lines: usize,
    include_callgraph: bool,
) -> Response {
    let mut entries = Vec::with_capacity(symbol_names.len());
    let mut all_ok = true;

    for name in symbol_names {
        let resp = zoom_one_symbol(
            req,
            ctx,
            path,
            file,
            source,
            lines,
            name,
            context_lines,
            include_callgraph,
        );
        let json = match serde_json::to_value(&resp) {
            Ok(v) => v,
            Err(err) => {
                return Response::error(
                    &req.id,
                    "internal_error",
                    format!("zoom: failed to serialize batch entry: {err}"),
                );
            }
        };
        if json.get("success").and_then(|v| v.as_bool()) != Some(true) {
            all_ok = false;
        }
        entries.push(serde_json::json!({
            "name": name,
            "response": json,
        }));
    }

    Response::success(
        &req.id,
        serde_json::json!({
            "complete": all_ok,
            "symbols": entries,
        }),
    )
}

fn zoom_one_symbol(
    req: &RawRequest,
    ctx: &AppContext,
    path: &Path,
    _file: &str,
    source: &str,
    lines: &[String],
    symbol_name: &str,
    context_lines: usize,
    include_callgraph: bool,
) -> Response {
    // Keep raw heading labels for outline display. Zoom resolves heading names in tiers:
    // exact raw text, normalized text, case-insensitive normalized text, then anchor slugs.
    // Code symbols continue through the provider's exact resolver.
    let is_heading = is_heading_zoom_language(detect_language(path));
    let matches = match resolve_zoom_symbol(ctx.provider(), path, symbol_name, is_heading) {
        Ok(matches) => matches,
        Err(e) => return Response::error(&req.id, e.code(), e.to_string()),
    };

    // LSP-enhanced disambiguation (S03)
    let matches = if let Some(hints) = lsp_hints::parse_lsp_hints(req) {
        lsp_hints::apply_lsp_disambiguation(matches, &hints)
    } else {
        matches
    };

    if matches.len() > 1 {
        let content = render_ambiguous_symbol_menu(symbol_name, &matches);
        let candidates = matches
            .iter()
            .map(|candidate| {
                let sym = &candidate.symbol;
                serde_json::json!({
                    "name": sym.name.clone(),
                    "qualified_name": qualified_symbol_name(sym),
                    "kind": symbol_kind_string(&sym.kind),
                    "range": sym.range.clone(),
                    "signature": sym.signature.clone(),
                })
            })
            .collect::<Vec<_>>();

        return Response::success(
            &req.id,
            serde_json::json!({
                "name": symbol_name,
                "kind": "ambiguous_symbol",
                "content": content,
                "context_before": [],
                "context_after": [],
                "annotations": empty_annotations(),
                "candidates": candidates,
            }),
        );
    }

    if matches.is_empty() {
        let mut msg = format!("symbol '{}' not found", symbol_name);
        if let Ok(all_symbols) = ctx.provider().list_symbols(path) {
            let suggestions = if is_heading {
                suggest_heading_symbols(symbol_name, &all_symbols, 5)
            } else {
                let available: Vec<String> = all_symbols.into_iter().map(|s| s.name).collect();
                suggest_close_symbols(symbol_name, &available, 5)
            };
            if !suggestions.is_empty() {
                msg.push_str(&format!(", did you mean: [{}]", suggestions.join(", ")));
            }
        }
        return Response::error(&req.id, "symbol_not_found", msg);
    }

    let target = &matches[0].symbol;
    let start = target.range.start_line as usize;
    let end = target.range.end_line as usize;

    // When re-export following resolved to a different file, re-read that file's lines
    let resolved_file_path = std::path::Path::new(&matches[0].file);
    let resolved_lines: Vec<String>;
    let effective_lines: &[String] = if resolved_file_path != path {
        resolved_lines = match std::fs::read_to_string(resolved_file_path) {
            Ok(src) => src.lines().map(|l| l.to_string()).collect(),
            Err(_) => lines.to_vec(),
        };
        &resolved_lines
    } else {
        lines
    };

    // Extract symbol body (0-based line indices)
    let content = if end < effective_lines.len() {
        effective_lines[start..=end].join("\n")
    } else {
        effective_lines[start..].join("\n")
    };

    let resolved_lang = detect_language(resolved_file_path);
    let container_outline = if might_have_container_members(target) {
        match build_container_outline(ctx, resolved_file_path, target) {
            Ok(outline) => Some(outline),
            Err(e) => {
                return Response::error(&req.id, e.code(), e.to_string());
            }
        }
    } else {
        None
    };

    if should_return_member_menu(target, resolved_lang, container_outline.as_ref()) {
        let kind_str = symbol_kind_string(&target.kind);
        let menu = render_container_member_menu(target, container_outline.as_ref().unwrap());
        let resp = ZoomResponse {
            name: target.name.clone(),
            kind: kind_str,
            range: target.range.clone(),
            content: menu,
            context_before: Vec::new(),
            context_after: Vec::new(),
            annotations: Annotations {
                calls_out: Vec::new(),
                called_by: Vec::new(),
            },
        };
        return match serde_json::to_value(&resp) {
            Ok(resp_json) => Response::success(&req.id, resp_json),
            Err(err) => Response::error(
                &req.id,
                "internal_error",
                format!("zoom: failed to serialize response: {err}"),
            ),
        };
    }

    // Context before
    let ctx_start = start.saturating_sub(context_lines);
    let context_before: Vec<String> = if ctx_start < start {
        effective_lines[ctx_start..start]
            .iter()
            .map(|l| l.to_string())
            .collect()
    } else {
        vec![]
    };

    // Context after
    let ctx_end = (end + 1 + context_lines).min(effective_lines.len());
    let context_after: Vec<String> = if end + 1 < effective_lines.len() {
        effective_lines[(end + 1)..ctx_end]
            .iter()
            .map(|l| l.to_string())
            .collect()
    } else {
        vec![]
    };

    let (calls_out, called_by) = if include_callgraph {
        // Get all symbols in the resolved file for call matching
        let all_symbols = match ctx.provider().list_symbols(resolved_file_path) {
            Ok(s) => s,
            Err(e) => {
                return Response::error(&req.id, e.code(), e.to_string());
            }
        };

        let known_names: Vec<&str> = all_symbols.iter().map(|s| s.name.as_str()).collect();

        // Parse AST for call extraction (use resolved file for cross-file re-exports)
        let mut parser = FileParser::with_symbol_cache(ctx.symbol_cache());
        let (tree, lang) = match parser.parse(resolved_file_path) {
            Ok(r) => r,
            Err(e) => {
                return Response::error(&req.id, e.code(), e.to_string());
            }
        };

        // calls_out: calls within the target symbol's byte range
        let resolved_source = if resolved_file_path != path {
            std::fs::read_to_string(resolved_file_path).unwrap_or_else(|_| source.to_string())
        } else {
            source.to_string()
        };
        let signature_byte_start = line_col_to_byte(
            &resolved_source,
            target.range.start_line,
            target.range.start_col,
        );
        let signature_byte_end = line_col_to_byte(
            &resolved_source,
            target.range.end_line,
            target.range.end_col,
        );
        let (target_byte_start, target_byte_end) =
            symbol_body_byte_range(tree.root_node(), signature_byte_start, signature_byte_end)
                .unwrap_or((signature_byte_start, signature_byte_end));

        let all_file_calls = extract_calls_with_ranges(&resolved_source, tree.root_node(), lang);

        let raw_calls = all_file_calls.iter().filter(|call| {
            call.start_byte >= target_byte_start && call.end_byte <= target_byte_end
        });
        let calls_out = dedupe_call_refs_by_name(
            raw_calls
                .filter(|call| {
                    known_names.contains(&call.name.as_str()) && call.name != target.name
                })
                .map(|call| CallRef {
                    name: call.name.clone(),
                    line: call.line,
                    extra_count: 0,
                })
                .collect(),
        );

        // called_by: bucket the single file-wide call extraction by enclosing symbol range
        let mut called_by: Vec<CallRef> = Vec::new();
        for sym in &all_symbols {
            if sym.name == target.name && sym.range.start_line == target.range.start_line {
                continue; // skip self
            }
            let sym_byte_start =
                line_col_to_byte(&resolved_source, sym.range.start_line, sym.range.start_col);
            let sym_byte_end =
                line_col_to_byte(&resolved_source, sym.range.end_line, sym.range.end_col);
            for call in &all_file_calls {
                if call.name == target.name
                    && call.start_byte >= sym_byte_start
                    && call.end_byte <= sym_byte_end
                {
                    called_by.push(CallRef {
                        name: sym.name.clone(),
                        line: call.line,
                        extra_count: 0,
                    });
                }
            }
        }

        let called_by = dedupe_call_refs_by_name(called_by);

        (calls_out, called_by)
    } else {
        (Vec::new(), Vec::new())
    };

    let kind_str = symbol_kind_string(&target.kind);

    let resp = ZoomResponse {
        name: target.name.clone(),
        kind: kind_str,
        range: target.range.clone(),
        content,
        context_before,
        context_after,
        annotations: Annotations {
            calls_out,
            called_by,
        },
    };

    match serde_json::to_value(&resp) {
        Ok(resp_json) => Response::success(&req.id, resp_json),
        Err(err) => Response::error(
            &req.id,
            "internal_error",
            format!("zoom: failed to serialize response: {err}"),
        ),
    }
}

fn empty_annotations() -> serde_json::Value {
    serde_json::json!({
        "calls_out": [],
        "called_by": [],
    })
}

fn render_ambiguous_symbol_menu(
    symbol_name: &str,
    matches: &[crate::symbols::SymbolMatch],
) -> String {
    let mut lines = vec![format!(
        "symbol '{symbol_name}' is ambiguous ({} candidates) — zoom a qualified name for its body",
        matches.len()
    )];

    for candidate in matches {
        let entry = symbol_to_entry(&candidate.symbol);
        lines.push(format!(
            "- {}",
            format_qualified_entry(&entry, Some(&candidate.symbol))
        ));
    }

    lines.join("\n")
}

fn levenshtein_distance(s1: &str, s2: &str) -> usize {
    let s1_chars: Vec<char> = s1.chars().collect();
    let s2_chars: Vec<char> = s2.chars().collect();
    let len1 = s1_chars.len();
    let len2 = s2_chars.len();

    let mut dp = vec![vec![0; len2 + 1]; len1 + 1];

    for i in 0..=len1 {
        dp[i][0] = i;
    }
    for j in 0..=len2 {
        dp[0][j] = j;
    }

    for i in 1..=len1 {
        for j in 1..=len2 {
            if s1_chars[i - 1] == s2_chars[j - 1] {
                dp[i][j] = dp[i - 1][j - 1];
            } else {
                dp[i][j] =
                    1 + std::cmp::min(dp[i - 1][j], std::cmp::min(dp[i][j - 1], dp[i - 1][j - 1]));
            }
        }
    }

    dp[len1][len2]
}

fn suggest_close_symbols(query: &str, available: &[String], k: usize) -> Vec<String> {
    let mut unique: Vec<&String> = available.iter().collect();
    unique.sort();
    unique.dedup();

    let query_lower = query.to_lowercase();
    let query_len = query_lower.chars().count();
    let max_dist = std::cmp::max(2, query_len / 3);

    let mut scored: Vec<(bool, usize, &String)> = unique
        .into_iter()
        .map(|name| {
            let name_lower = name.to_lowercase();
            let is_substring =
                name_lower.contains(&query_lower) || query_lower.contains(&name_lower);
            let is_wildcard = if let (Some(first_idx), Some(last_idx)) =
                (query_lower.find('_'), query_lower.rfind('_'))
            {
                let prefix = &query_lower[..=first_idx];
                let suffix = &query_lower[last_idx..];
                name_lower.starts_with(prefix) && name_lower.ends_with(suffix)
            } else {
                false
            };
            let is_match = is_substring || is_wildcard;
            let dist = levenshtein_distance(&query_lower, &name_lower);
            (is_match, dist, name)
        })
        .filter(|&(is_match, dist, _)| is_match || dist <= max_dist)
        .collect();

    scored.sort_by(|a, b| {
        let a_match = a.0;
        let b_match = b.0;
        (!a_match)
            .cmp(&(!b_match))
            .then_with(|| a.1.cmp(&b.1))
            .then_with(|| a.2.cmp(b.2))
    });

    scored
        .into_iter()
        .take(k)
        .map(|(_, _, name)| name.clone())
        .collect()
}

fn resolve_zoom_symbol(
    provider: &dyn LanguageProvider,
    path: &Path,
    query: &str,
    is_heading: bool,
) -> Result<Vec<SymbolMatch>, crate::error::AftError> {
    if is_heading {
        return resolve_heading_symbols(provider, path, query);
    }

    match provider.resolve_symbol(path, query) {
        Err(crate::error::AftError::SymbolNotFound { .. }) => Ok(Vec::new()),
        result => result,
    }
}

/// Keep document headings' raw labels for outline fidelity while allowing zoom to use
/// human-readable labels, section prefixes, or anchors without affecting code symbols.
fn resolve_heading_symbols(
    provider: &dyn LanguageProvider,
    path: &Path,
    query: &str,
) -> Result<Vec<SymbolMatch>, crate::error::AftError> {
    let headings: Vec<SymbolMatch> = provider
        .list_symbols(path)?
        .into_iter()
        .filter(|symbol| symbol.kind == SymbolKind::Heading)
        .map(|symbol| SymbolMatch {
            file: path.display().to_string(),
            symbol,
        })
        .collect();

    Ok(match_heading_identity(&headings, query))
}

fn match_heading_identity(headings: &[SymbolMatch], query: &str) -> Vec<SymbolMatch> {
    let exact: Vec<_> = headings
        .iter()
        .filter(|candidate| heading_identity_is_exact(&candidate.symbol, query))
        .cloned()
        .collect();
    if !exact.is_empty() {
        return exact;
    }

    let normalized_query = normalize_heading_label(query);
    if normalized_query.is_empty() {
        return Vec::new();
    }

    let normalized: Vec<_> = headings
        .iter()
        .filter(|candidate| heading_identity_is_normalized(&candidate.symbol, &normalized_query))
        .cloned()
        .collect();
    if !normalized.is_empty() {
        return normalized;
    }

    let folded_query = normalized_query.to_lowercase();
    let case_insensitive: Vec<_> = headings
        .iter()
        .filter(|candidate| heading_identity_is_case_insensitive(&candidate.symbol, &folded_query))
        .cloned()
        .collect();
    if !case_insensitive.is_empty() {
        return case_insensitive;
    }

    let query_slug = slugify_heading_label(&normalized_query);
    if query_slug.is_empty() {
        return Vec::new();
    }

    headings
        .iter()
        .filter(|candidate| heading_identity_has_slug(&candidate.symbol, &query_slug))
        .cloned()
        .collect()
}

fn qualified_heading_name(symbol: &Symbol) -> String {
    if symbol.scope_chain.is_empty() {
        return symbol.name.clone();
    }
    format!("{}.{}", symbol.scope_chain.join("."), symbol.name)
}

fn heading_identity_is_exact(symbol: &Symbol, query: &str) -> bool {
    symbol.name == query || qualified_heading_name(symbol) == query
}

fn heading_identity_is_normalized(symbol: &Symbol, query: &str) -> bool {
    normalize_heading_label(&symbol.name) == query
        || normalize_heading_label(&qualified_heading_name(symbol)) == query
}

fn heading_identity_is_case_insensitive(symbol: &Symbol, query: &str) -> bool {
    normalize_heading_label(&symbol.name).to_lowercase() == query
        || normalize_heading_label(&qualified_heading_name(symbol)).to_lowercase() == query
}

fn heading_identity_has_slug(symbol: &Symbol, query_slug: &str) -> bool {
    slugify_heading_label(&normalize_heading_label(&symbol.name)) == query_slug
        || slugify_heading_label(&normalize_heading_label(&qualified_heading_name(symbol)))
            == query_slug
}

fn suggest_heading_symbols(query: &str, symbols: &[Symbol], k: usize) -> Vec<String> {
    let available: Vec<String> = symbols
        .iter()
        .filter(|symbol| symbol.kind == SymbolKind::Heading)
        .map(|symbol| normalize_heading_label(&symbol.name))
        .filter(|name| !name.is_empty())
        .collect();
    let normalized_query = normalize_heading_label(query);
    if normalized_query.is_empty() {
        return Vec::new();
    }
    suggest_close_symbols(&normalized_query, &available, k)
}

fn normalize_heading_label(input: &str) -> String {
    let mut value = collapse_heading_whitespace(&strip_markdown_links(input));

    // A label can contain both a document prefix and a decorative symbol cluster.
    // Repeat the small cleanup sequence so either order is handled consistently.
    for _ in 0..4 {
        let mut next = value.as_str();
        let without_heading_markers = next.trim_start_matches('#').trim_start();
        if without_heading_markers != next {
            next = without_heading_markers;
        }
        if let Some(rest) = strip_heading_html_prefix(next) {
            next = rest;
        }
        if let Some(rest) = strip_leading_section_prefix(next) {
            next = rest;
        }
        if let Some(rest) = strip_leading_symbol_cluster(next) {
            next = rest;
        }

        let collapsed = collapse_heading_whitespace(next);
        if collapsed == value {
            break;
        }
        value = collapsed;
    }

    value
}

fn collapse_heading_whitespace(input: &str) -> String {
    input.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn strip_markdown_links(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    let mut cursor = 0;

    while let Some(relative_open) = input[cursor..].find('[') {
        let open = cursor + relative_open;
        let Some(close) = find_matching_delimiter(input, open, b'[', b']') else {
            output.push_str(&input[cursor..]);
            return output;
        };

        if input.as_bytes().get(close + 1) != Some(&b'(') {
            output.push_str(&input[cursor..=close]);
            cursor = close + 1;
            continue;
        }

        let target_open = close + 1;
        let Some(target_close) = find_matching_delimiter(input, target_open, b'(', b')') else {
            output.push_str(&input[cursor..]);
            return output;
        };

        output.push_str(&input[cursor..open]);
        output.push_str(&input[open + 1..close]);
        cursor = target_close + 1;
    }

    output.push_str(&input[cursor..]);
    output
}

fn find_matching_delimiter(input: &str, start: usize, open: u8, close: u8) -> Option<usize> {
    let mut depth = 0;
    for (index, byte) in input.as_bytes().iter().enumerate().skip(start) {
        if *byte == open {
            depth += 1;
        } else if *byte == close {
            depth -= 1;
            if depth == 0 {
                return Some(index);
            }
        }
    }
    None
}

fn strip_heading_html_prefix(input: &str) -> Option<&str> {
    let input = input.trim_start();
    let bytes = input.as_bytes();
    if bytes.first() != Some(&b'<') {
        return None;
    }

    let mut index = 1;
    if bytes.get(index) == Some(&b'/') {
        index += 1;
    }
    if !matches!(bytes.get(index), Some(b'h' | b'H')) {
        return None;
    }
    index += 1;
    if !matches!(bytes.get(index), Some(b'1'..=b'6')) {
        return None;
    }

    let end = input.find('>')?;
    let rest = input[end + 1..].trim_start();
    if rest.chars().any(|character| character.is_alphanumeric()) {
        Some(rest)
    } else {
        None
    }
}

fn strip_leading_section_prefix(input: &str) -> Option<&str> {
    let bytes = input.as_bytes();
    let mut index = 0;
    let mut saw_dot = false;

    if !bytes
        .first()
        .is_some_and(|byte| byte.is_ascii_alphanumeric())
    {
        return None;
    }

    while index < bytes.len() {
        while index < bytes.len() && bytes[index].is_ascii_alphanumeric() {
            index += 1;
        }
        if bytes.get(index) != Some(&b'.') {
            break;
        }
        saw_dot = true;
        index += 1;
        if bytes
            .get(index)
            .is_some_and(|byte| byte.is_ascii_whitespace())
        {
            let rest = input[index..].trim_start();
            return if rest.chars().any(|character| character.is_alphanumeric()) {
                Some(rest)
            } else {
                None
            };
        }
        if !bytes
            .get(index)
            .is_some_and(|byte| byte.is_ascii_alphanumeric())
        {
            return None;
        }
    }

    if saw_dot
        && bytes
            .get(index)
            .is_some_and(|byte| byte.is_ascii_whitespace())
    {
        let rest = input[index..].trim_start();
        if rest.chars().any(|character| character.is_alphanumeric()) {
            return Some(rest);
        }
    }
    None
}

fn strip_leading_symbol_cluster(input: &str) -> Option<&str> {
    let first_text = input
        .char_indices()
        .find(|(_, character)| character.is_alphanumeric())
        .map(|(index, _)| index)?;
    if first_text == 0 {
        return None;
    }

    let rest = &input[first_text..];
    if rest.chars().any(|character| character.is_alphanumeric()) {
        Some(rest)
    } else {
        None
    }
}

fn slugify_heading_label(label: &str) -> String {
    let mut slug = String::new();
    let mut pending_separator = false;

    for character in label.chars() {
        if character.is_alphanumeric() {
            if pending_separator && !slug.is_empty() {
                slug.push('-');
            }
            for lowercase in character.to_lowercase() {
                slug.push(lowercase);
            }
            pending_separator = false;
        } else if !slug.is_empty() {
            pending_separator = true;
        }
    }

    slug
}

/// Extract call expression names within a byte range of the AST.
///
/// Delegates to `crate::calls::extract_calls_in_range`.
#[cfg(test)]
fn extract_calls_in_range(
    source: &str,
    root: tree_sitter::Node,
    byte_start: usize,
    byte_end: usize,
    lang: LangId,
) -> Vec<(String, u32)> {
    crate::calls::extract_calls_in_range(source, root, byte_start, byte_end, lang)
}

fn symbol_body_byte_range(
    root: tree_sitter::Node,
    byte_start: usize,
    byte_end: usize,
) -> Option<(usize, usize)> {
    let node = smallest_node_covering_range(root, byte_start, byte_end)?;
    let mut current = Some(node);
    while let Some(node) = current {
        if is_symbol_body_node(node.kind()) {
            return Some((node.start_byte(), node.end_byte()));
        }
        current = node.parent();
    }
    Some((node.start_byte(), node.end_byte()))
}

fn smallest_node_covering_range<'tree>(
    node: tree_sitter::Node<'tree>,
    byte_start: usize,
    byte_end: usize,
) -> Option<tree_sitter::Node<'tree>> {
    if node.start_byte() > byte_start || node.end_byte() < byte_end {
        return None;
    }

    let mut cursor = node.walk();
    if cursor.goto_first_child() {
        loop {
            let child = cursor.node();
            if let Some(found) = smallest_node_covering_range(child, byte_start, byte_end) {
                return Some(found);
            }
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }

    Some(node)
}

fn is_symbol_body_node(kind: &str) -> bool {
    matches!(
        kind,
        "function_declaration"
            | "generator_function_declaration"
            | "function_expression"
            | "generator_function"
            | "arrow_function"
            | "method_definition"
            | "class_declaration"
            | "abstract_class_declaration"
            | "class"
            | "lexical_declaration"
            | "function_definition"
            | "class_definition"
            | "decorated_definition"
            | "function_item"
            | "impl_item"
            | "method_declaration"
    )
}

fn extract_calls_with_ranges(source: &str, root: tree_sitter::Node, lang: LangId) -> Vec<RawCall> {
    let mut results = Vec::new();
    let call_kinds = crate::calls::call_node_kinds(lang);
    collect_calls_with_ranges(root, source, &call_kinds, &mut results);
    results
}

fn collect_calls_with_ranges(
    node: tree_sitter::Node,
    source: &str,
    call_kinds: &[&str],
    results: &mut Vec<RawCall>,
) {
    if call_kinds.contains(&node.kind()) {
        if let Some(name) = crate::calls::extract_callee_name(&node, source) {
            results.push(RawCall {
                name,
                line: node.start_position().row as u32 + 1,
                start_byte: node.start_byte(),
                end_byte: node.end_byte(),
            });
        }
    }

    let mut cursor = node.walk();
    if cursor.goto_first_child() {
        loop {
            collect_calls_with_ranges(cursor.node(), source, call_kinds, results);
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::context::AppContext;
    use crate::parser::TreeSitterProvider;
    use std::path::PathBuf;

    fn fixture_path(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
            .join(name)
    }

    fn make_ctx() -> AppContext {
        AppContext::new(Box::new(TreeSitterProvider::new()), Config::default())
    }

    #[test]
    fn parse_zoom_symbol_names_splits_whitespace_for_code() {
        let params = serde_json::json!({ "symbol": "InspectCategory active is_active" });
        let names = parse_zoom_symbol_names(&params, Some(LangId::Rust)).expect("parse");
        assert_eq!(names, vec!["InspectCategory", "active", "is_active"]);
    }

    #[test]
    fn parse_zoom_symbol_names_does_not_split_markdown_headings() {
        let params = serde_json::json!({ "symbols": "Getting Started" });
        let names = parse_zoom_symbol_names(&params, Some(LangId::Markdown)).expect("parse");
        assert_eq!(names, vec!["Getting Started"]);
    }

    #[test]
    fn parse_zoom_symbol_names_does_not_split_html_headings() {
        let params = serde_json::json!({ "symbol": "Last Heading" });
        let names = parse_zoom_symbol_names(&params, Some(LangId::Html)).expect("parse");
        assert_eq!(names, vec!["Last Heading"]);
    }

    #[test]
    fn parse_zoom_symbol_names_single_token_unchanged() {
        let params = serde_json::json!({ "symbol": "compute" });
        let names = parse_zoom_symbol_names(&params, Some(LangId::TypeScript)).expect("parse");
        assert_eq!(names, vec!["compute"]);
    }

    #[test]
    fn parse_zoom_symbol_names_symbols_array_unchanged() {
        let params = serde_json::json!({ "symbols": ["A", "B", "C"] });
        let names = parse_zoom_symbol_names(&params, Some(LangId::Rust)).expect("parse");
        assert_eq!(names, vec!["A", "B", "C"]);
    }

    // --- Call extraction tests ---

    #[test]
    fn extract_calls_finds_direct_calls() {
        let source = std::fs::read_to_string(fixture_path("calls.ts")).unwrap();
        let mut parser = FileParser::new();
        let path = fixture_path("calls.ts");
        let (tree, lang) = parser.parse(&path).unwrap();

        // `compute` calls `helper` — find compute's range from symbols
        let ctx = make_ctx();
        let symbols = ctx.provider().list_symbols(&path).unwrap();
        let compute = symbols.iter().find(|s| s.name == "compute").unwrap();

        let byte_start =
            line_col_to_byte(&source, compute.range.start_line, compute.range.start_col);
        let byte_end = line_col_to_byte(&source, compute.range.end_line, compute.range.end_col);

        let calls = extract_calls_in_range(&source, tree.root_node(), byte_start, byte_end, lang);
        let names: Vec<&str> = calls.iter().map(|(n, _)| n.as_str()).collect();

        assert!(
            names.contains(&"helper"),
            "compute should call helper, got: {:?}",
            names
        );
    }

    #[test]
    fn extract_calls_finds_member_calls() {
        let source = std::fs::read_to_string(fixture_path("calls.ts")).unwrap();
        let mut parser = FileParser::new();
        let path = fixture_path("calls.ts");
        let (tree, lang) = parser.parse(&path).unwrap();

        let ctx = make_ctx();
        let symbols = ctx.provider().list_symbols(&path).unwrap();
        let run_all = symbols.iter().find(|s| s.name == "runAll").unwrap();

        let byte_start =
            line_col_to_byte(&source, run_all.range.start_line, run_all.range.start_col);
        let byte_end = line_col_to_byte(&source, run_all.range.end_line, run_all.range.end_col);

        let calls = extract_calls_in_range(&source, tree.root_node(), byte_start, byte_end, lang);
        let names: Vec<&str> = calls.iter().map(|(n, _)| n.as_str()).collect();

        assert!(
            names.contains(&"add"),
            "runAll should call this.add, got: {:?}",
            names
        );
        assert!(
            names.contains(&"helper"),
            "runAll should call helper, got: {:?}",
            names
        );
    }

    #[test]
    fn extract_calls_unused_function_has_no_calls() {
        let source = std::fs::read_to_string(fixture_path("calls.ts")).unwrap();
        let mut parser = FileParser::new();
        let path = fixture_path("calls.ts");
        let (tree, lang) = parser.parse(&path).unwrap();

        let ctx = make_ctx();
        let symbols = ctx.provider().list_symbols(&path).unwrap();
        let unused = symbols.iter().find(|s| s.name == "unused").unwrap();

        let byte_start = line_col_to_byte(&source, unused.range.start_line, unused.range.start_col);
        let byte_end = line_col_to_byte(&source, unused.range.end_line, unused.range.end_col);

        let calls = extract_calls_in_range(&source, tree.root_node(), byte_start, byte_end, lang);
        // console.log is the only call, but "log" or "console" aren't known symbols
        let known_names = [
            "helper",
            "compute",
            "orchestrate",
            "unused",
            "format",
            "display",
        ];
        let filtered: Vec<&str> = calls
            .iter()
            .map(|(n, _)| n.as_str())
            .filter(|n| known_names.contains(n))
            .collect();
        assert!(
            filtered.is_empty(),
            "unused should not call known symbols, got: {:?}",
            filtered
        );
    }

    // --- Context line tests ---

    #[test]
    fn context_lines_clamp_at_file_start() {
        // helper() is at the top of the file (line 2) — context_before should be clamped
        let ctx = make_ctx();
        let path = fixture_path("calls.ts");
        let symbols = ctx.provider().list_symbols(&path).unwrap();
        let helper = symbols.iter().find(|s| s.name == "helper").unwrap();

        let source = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = source.lines().collect();
        let start = helper.range.start_line as usize;

        // With context_lines=5, ctx_start should clamp to 0
        let ctx_start = start.saturating_sub(5);
        let context_before: Vec<&str> = lines[ctx_start..start].to_vec();
        // Should have at most `start` lines (not panic)
        assert!(context_before.len() <= start);
    }

    #[test]
    fn context_lines_clamp_at_file_end() {
        let ctx = make_ctx();
        let path = fixture_path("calls.ts");
        let symbols = ctx.provider().list_symbols(&path).unwrap();
        let display = symbols.iter().find(|s| s.name == "display").unwrap();

        let source = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = source.lines().collect();
        let end = display.range.end_line as usize;

        // With context_lines=20, should clamp to file length
        let ctx_end = (end + 1 + 20).min(lines.len());
        let context_after: Vec<&str> = if end + 1 < lines.len() {
            lines[(end + 1)..ctx_end].to_vec()
        } else {
            vec![]
        };
        // Should not panic regardless of context_lines size
        assert!(context_after.len() <= 20);
    }

    // --- Body extraction test ---

    #[test]
    fn body_extraction_matches_source() {
        let ctx = make_ctx();
        let path = fixture_path("calls.ts");
        let symbols = ctx.provider().list_symbols(&path).unwrap();
        let compute = symbols.iter().find(|s| s.name == "compute").unwrap();

        let source = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = source.lines().collect();
        let start = compute.range.start_line as usize;
        let end = compute.range.end_line as usize;
        let body = lines[start..=end].join("\n");

        assert!(
            body.contains("function compute"),
            "body should contain function declaration"
        );
        assert!(
            body.contains("helper(a)"),
            "body should contain call to helper"
        );
        assert!(
            body.contains("doubled + b"),
            "body should contain return expression"
        );
    }

    // --- Full zoom response tests ---

    #[test]
    fn body_range_expands_signature_range_to_include_body_calls() {
        let source = r#"function compute(
  value: number,
): number {
  return helper(value);
}

function helper(value: number): number {
  return value * 2;
}
"#;
        let grammar = crate::parser::grammar_for(LangId::TypeScript);
        let mut parser = tree_sitter::Parser::new();
        parser.set_language(&grammar).unwrap();
        let tree = parser.parse(source, None).unwrap();
        let signature_end = source.find('{').expect("function has body");

        let (body_start, body_end) =
            symbol_body_byte_range(tree.root_node(), 0, signature_end).expect("body range");
        let calls = extract_calls_in_range(
            source,
            tree.root_node(),
            body_start,
            body_end,
            LangId::TypeScript,
        );
        let names = calls
            .iter()
            .map(|(name, _)| name.as_str())
            .collect::<Vec<_>>();

        assert!(
            names.contains(&"helper"),
            "call inside the function body should be included: {names:?}"
        );
    }

    #[test]
    fn zoom_leaf_returns_full_body_without_budget_marker() {
        let ctx = make_ctx();
        let path = fixture_path("calls.ts");
        let req = make_zoom_request(
            "z-leaf-full",
            path.to_str().unwrap(),
            "repeatedOutgoing",
            None,
        );
        let resp = handle_zoom(&req, &ctx);
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["success"], true, "zoom should succeed: {json:?}");

        let symbols = ctx.provider().list_symbols(&path).unwrap();
        let target = symbols
            .iter()
            .find(|symbol| symbol.name == "repeatedOutgoing")
            .unwrap();
        let source = std::fs::read_to_string(&path).unwrap();
        let lines = source.lines().collect::<Vec<_>>();
        let expected =
            lines[target.range.start_line as usize..=target.range.end_line as usize].join("\n");

        assert_eq!(json["content"].as_str().unwrap(), expected);
        assert!(
            !json["content"]
                .as_str()
                .unwrap()
                .contains("more lines — zoom"),
            "explicit zoom must not budget-cap leaf bodies"
        );
    }

    #[test]
    fn zoom_response_has_calls_out_and_called_by() {
        let ctx = make_ctx();
        let path = fixture_path("calls.ts");

        let req = make_zoom_request_cg("z-1", path.to_str().unwrap(), "compute");
        let resp = handle_zoom(&req, &ctx);

        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["success"], true, "zoom should succeed: {:?}", json);

        let calls_out = json["annotations"]["calls_out"]
            .as_array()
            .expect("calls_out array");
        let out_names: Vec<&str> = calls_out
            .iter()
            .map(|c| c["name"].as_str().unwrap())
            .collect();
        assert!(
            out_names.contains(&"helper"),
            "compute calls helper: {:?}",
            out_names
        );

        let called_by = json["annotations"]["called_by"]
            .as_array()
            .expect("called_by array");
        let by_names: Vec<&str> = called_by
            .iter()
            .map(|c| c["name"].as_str().unwrap())
            .collect();
        assert!(
            by_names.contains(&"orchestrate"),
            "orchestrate calls compute: {:?}",
            by_names
        );
    }

    #[test]
    fn zoom_callgraph_dedupes_repeated_call_sites_by_name() {
        let ctx = make_ctx();
        let path = fixture_path("calls.ts");

        let req = make_zoom_request_cg("z-dedupe-out", path.to_str().unwrap(), "repeatedOutgoing");
        let resp = handle_zoom(&req, &ctx);
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["success"], true, "zoom should succeed: {json:?}");

        let calls_out = json["annotations"]["calls_out"]
            .as_array()
            .expect("calls_out array");
        let helper_refs = calls_out
            .iter()
            .filter(|call| call["name"] == "helper")
            .collect::<Vec<_>>();
        assert_eq!(
            helper_refs.len(),
            1,
            "helper should be folded once: {calls_out:?}"
        );
        assert_eq!(helper_refs[0]["extra_count"], 1);
        assert!(
            calls_out.iter().any(|call| call["name"] == "format"),
            "distinct callee must not be folded into helper: {calls_out:?}"
        );

        let req = make_zoom_request_cg("z-dedupe-by", path.to_str().unwrap(), "compute");
        let resp = handle_zoom(&req, &ctx);
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["success"], true, "zoom should succeed: {json:?}");

        let called_by = json["annotations"]["called_by"]
            .as_array()
            .expect("called_by array");
        let repeat_refs = called_by
            .iter()
            .filter(|call| call["name"] == "repeatCompute")
            .collect::<Vec<_>>();
        assert_eq!(
            repeat_refs.len(),
            1,
            "repeatCompute should be folded once: {called_by:?}"
        );
        assert_eq!(repeat_refs[0]["extra_count"], 1);
        assert!(
            called_by.iter().any(|call| call["name"] == "orchestrate"),
            "distinct caller must not be folded into repeatCompute: {called_by:?}"
        );
    }

    #[test]
    fn zoom_response_empty_annotations_for_unused() {
        let ctx = make_ctx();
        let path = fixture_path("calls.ts");

        let req = make_zoom_request_cg("z-2", path.to_str().unwrap(), "unused");
        let resp = handle_zoom(&req, &ctx);

        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["success"], true);

        let _calls_out = json["annotations"]["calls_out"].as_array().unwrap();
        let called_by = json["annotations"]["called_by"].as_array().unwrap();

        // calls_out exists (may contain console.log but no known symbols)
        // called_by should be empty — nobody calls unused
        assert!(
            called_by.is_empty(),
            "unused should not be called by anyone: {:?}",
            called_by
        );
    }

    #[test]
    fn zoom_default_omits_callgraph_annotations() {
        let ctx = make_ctx();
        let path = fixture_path("calls.ts");

        let req = make_zoom_request("z-1-default", path.to_str().unwrap(), "compute", None);
        let resp = handle_zoom(&req, &ctx);

        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["success"], true, "zoom should succeed: {:?}", json);

        let calls_out = json["annotations"]["calls_out"]
            .as_array()
            .expect("calls_out array");
        let called_by = json["annotations"]["called_by"]
            .as_array()
            .expect("called_by array");
        assert!(
            calls_out.is_empty(),
            "default zoom should omit calls_out: {:?}",
            calls_out
        );
        assert!(
            called_by.is_empty(),
            "default zoom should omit called_by: {:?}",
            called_by
        );
    }

    #[test]
    fn zoom_symbol_not_found() {
        let ctx = make_ctx();
        let path = fixture_path("calls.ts");

        let req = make_zoom_request("z-3", path.to_str().unwrap(), "nonexistent", None);
        let resp = handle_zoom(&req, &ctx);

        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["success"], false);
        assert_eq!(json["code"], "symbol_not_found");
    }

    #[test]
    fn zoom_custom_context_lines() {
        let ctx = make_ctx();
        let path = fixture_path("calls.ts");

        let req = make_zoom_request("z-4", path.to_str().unwrap(), "compute", Some(1));
        let resp = handle_zoom(&req, &ctx);

        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["success"], true);

        let ctx_before = json["context_before"].as_array().unwrap();
        let ctx_after = json["context_after"].as_array().unwrap();
        // With context_lines=1, we get at most 1 line before and after
        assert!(
            ctx_before.len() <= 1,
            "context_before should be ≤1: {:?}",
            ctx_before
        );
        assert!(
            ctx_after.len() <= 1,
            "context_after should be ≤1: {:?}",
            ctx_after
        );
    }

    #[test]
    fn zoom_missing_file_param() {
        let ctx = make_ctx();
        let req = make_raw_request("z-5", r#"{"id":"z-5","command":"zoom","symbol":"foo"}"#);
        let resp = handle_zoom(&req, &ctx);

        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["success"], false);
        assert_eq!(json["code"], "invalid_request");
    }

    #[test]
    fn zoom_missing_symbol_param() {
        let ctx = make_ctx();
        let path = fixture_path("calls.ts");
        // Build the JSON via serde_json so Windows paths (with backslashes)
        // are escaped correctly. Hand-formatted JSON would treat `C:\path`
        // backslashes as escape sequences and fail to parse.
        let req_value = serde_json::json!({
            "id": "z-6",
            "command": "zoom",
            "file": path.to_string_lossy(),
        });
        let req_str = req_value.to_string();
        let req: RawRequest = serde_json::from_str(&req_str).unwrap();
        let resp = handle_zoom(&req, &ctx);

        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["success"], false);
        assert_eq!(json["code"], "invalid_request");
    }

    #[test]
    fn test_suggest_close_symbols_unit() {
        let available = vec![
            "handle_grep_search".to_string(),
            "handle_semantic_search".to_string(),
            "handle_semantic_or_hybrid_search".to_string(),
            "compute_total".to_string(),
            "search".to_string(),
            "handle_search".to_string(),
        ];

        let suggestions = suggest_close_symbols("handle_search", &available, 5);
        assert!(suggestions.contains(&"handle_grep_search".to_string()));
        assert!(suggestions.contains(&"handle_semantic_search".to_string()));
        assert!(suggestions.contains(&"handle_semantic_or_hybrid_search".to_string()));
        assert!(suggestions.contains(&"search".to_string()));
        assert!(!suggestions.contains(&"compute_total".to_string()));

        let suggestions_caps = suggest_close_symbols("HANDLE_SEARCH", &available, 5);
        assert_eq!(suggestions, suggestions_caps);

        let available2 = vec![
            "total".to_string(),
            "compute_total".to_string(),
            "unrelated".to_string(),
        ];
        let suggestions2 = suggest_close_symbols("totol", &available2, 5);
        assert_eq!(suggestions2, vec!["total".to_string()]);
    }

    // --- Helpers ---

    fn make_zoom_request(
        id: &str,
        file: &str,
        symbol: &str,
        context_lines: Option<u64>,
    ) -> RawRequest {
        let mut json = serde_json::json!({
            "id": id,
            "command": "zoom",
            "file": file,
            "symbol": symbol,
        });
        if let Some(cl) = context_lines {
            json["context_lines"] = serde_json::json!(cl);
        }
        serde_json::from_value(json).unwrap()
    }

    fn make_zoom_request_cg(id: &str, file: &str, symbol: &str) -> RawRequest {
        let mut req = make_zoom_request(id, file, symbol, None);
        req.params["callgraph"] = serde_json::json!(true);
        req
    }

    fn make_raw_request(_id: &str, json_str: &str) -> RawRequest {
        serde_json::from_str(json_str).unwrap()
    }
}
