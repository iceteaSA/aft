use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::context::AppContext;
use crate::edit;
use crate::protocol::{RawRequest, Response};

const MAX_PATCH_SIZE: usize = 1024 * 1024;
const MAX_HUNKS: usize = 500;

#[derive(Debug, Clone)]
enum Hunk {
    Add {
        path: String,
        contents: String,
    },
    Delete {
        path: String,
    },
    Update {
        path: String,
        move_path: Option<String>,
        chunks: Vec<UpdateFileChunk>,
    },
}

#[derive(Debug, Clone)]
struct UpdateFileChunk {
    old_lines: Vec<String>,
    new_lines: Vec<String>,
    change_context: Option<String>,
    is_end_of_file: bool,
}

/// Internal apply_patch preview command.
///
/// OpenCode permission asks need a unified-diff string before the mutation is
/// approved. The public AFT tool surface deliberately does not expose dry-run or
/// preview fields; this wire command is used only by the host plugin with
/// `preview: true` to run the same patch transform without writing, backing up,
/// formatting, or collecting diagnostics.
pub fn handle_apply_patch(req: &RawRequest, ctx: &AppContext) -> Response {
    if !edit::wants_preview(&req.params) {
        return Response::error(
            &req.id,
            "invalid_request",
            "apply_patch: only internal preview mode is supported by this command",
        );
    }

    let patch_text = match req
        .params
        .get("patch_text")
        .or_else(|| req.params.get("patchText"))
        .and_then(|v| v.as_str())
    {
        Some(text) if !text.is_empty() => text,
        _ => {
            return Response::error(
                &req.id,
                "invalid_request",
                "apply_patch: missing required param 'patch_text'",
            )
        }
    };

    let hunks = match parse_patch(patch_text) {
        Ok(hunks) => hunks,
        Err(message) => return Response::error(&req.id, "invalid_request", message),
    };

    if hunks.is_empty() {
        return Response::error(
            &req.id,
            "invalid_request",
            "Empty patch: no file operations found",
        );
    }

    preview_hunks(req, ctx, &hunks)
}

fn preview_hunks(req: &RawRequest, ctx: &AppContext, hunks: &[Hunk]) -> Response {
    let mut virtual_files: HashMap<PathBuf, Option<String>> = HashMap::new();
    let mut files = Vec::new();
    let mut preview_diff = String::new();
    let mut total_additions = 0usize;
    let mut total_deletions = 0usize;

    for hunk in hunks {
        match hunk {
            Hunk::Add { path, contents } => {
                let file_path = match ctx.validate_path(&req.id, &resolve_patch_path(ctx, path)) {
                    Ok(path) => path,
                    Err(resp) => return resp,
                };
                if virtual_read(&virtual_files, &file_path).is_some() || file_path.exists() {
                    return Response::error(
                        &req.id,
                        "invalid_request",
                        format!(
                            "Failed to create {}: file already exists. Use *** Update File: to modify, or *** Delete File: first if you want to replace it entirely.",
                            path
                        ),
                    );
                }

                let after = if contents.ends_with('\n') {
                    contents.clone()
                } else {
                    format!("{contents}\n")
                };
                let display = file_path.display().to_string();
                let diff = edit::compute_diff_info("", &after);
                accumulate_counts(&diff, &mut total_additions, &mut total_deletions);
                push_patch(&mut preview_diff, &display, "", &after);
                files.push(serde_json::json!({
                    "file": display,
                    "type": "add",
                    "diff": diff,
                }));
                virtual_files.insert(file_path, Some(after));
            }
            Hunk::Delete { path } => {
                let file_path = match ctx.validate_path(&req.id, &resolve_patch_path(ctx, path)) {
                    Ok(path) => path,
                    Err(resp) => return resp,
                };
                let before = match virtual_read_or_disk(&virtual_files, &file_path) {
                    Ok(content) => content,
                    Err(message) => {
                        return Response::error(
                            &req.id,
                            "file_not_found",
                            format!("Failed to delete {}: {}", path, message),
                        )
                    }
                };
                let display = file_path.display().to_string();
                let diff = edit::compute_diff_info(&before, "");
                accumulate_counts(&diff, &mut total_additions, &mut total_deletions);
                push_patch(&mut preview_diff, &display, &before, "");
                files.push(serde_json::json!({
                    "file": display,
                    "type": "delete",
                    "diff": diff,
                }));
                virtual_files.insert(file_path, None);
            }
            Hunk::Update {
                path,
                move_path,
                chunks,
            } => {
                let file_path = match ctx.validate_path(&req.id, &resolve_patch_path(ctx, path)) {
                    Ok(path) => path,
                    Err(resp) => return resp,
                };
                let original = match virtual_read_or_disk(&virtual_files, &file_path) {
                    Ok(content) => content,
                    Err(message) => {
                        return Response::error(
                            &req.id,
                            "file_not_found",
                            format!("Failed to update {}: {}", path, message),
                        )
                    }
                };
                let new_content = match apply_update_chunks(&original, path, chunks) {
                    Ok(content) => content,
                    Err(message) => {
                        return Response::error(
                            &req.id,
                            "patch_failed",
                            format!("Failed to update {}: {}", path, message),
                        )
                    }
                };

                let target_path = if let Some(move_path) = move_path {
                    match ctx.validate_path(&req.id, &resolve_patch_path(ctx, move_path)) {
                        Ok(path) => path,
                        Err(resp) => return resp,
                    }
                } else {
                    file_path.clone()
                };
                let display = target_path.display().to_string();
                let diff = edit::compute_diff_info(&original, &new_content);
                accumulate_counts(&diff, &mut total_additions, &mut total_deletions);
                push_patch(&mut preview_diff, &display, &original, &new_content);
                files.push(serde_json::json!({
                    "file": display,
                    "type": if move_path.is_some() { "move" } else { "update" },
                    "source": file_path.display().to_string(),
                    "diff": diff,
                }));

                if move_path.is_some() {
                    virtual_files.insert(file_path, None);
                }
                virtual_files.insert(target_path, Some(new_content));
            }
        }
    }

    Response::success(
        &req.id,
        serde_json::json!({
            "ok": true,
            "preview": true,
            "files": files,
            "diff": {
                "additions": total_additions,
                "deletions": total_deletions,
            },
            "preview_diff": preview_diff,
        }),
    )
}

fn resolve_patch_path(ctx: &AppContext, raw: &str) -> PathBuf {
    let path = Path::new(raw);
    if path.is_absolute() {
        return path.to_path_buf();
    }
    let config = ctx.config();
    let root = config
        .project_root
        .clone()
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    drop(config);
    root.join(path)
}

fn virtual_read(files: &HashMap<PathBuf, Option<String>>, path: &Path) -> Option<String> {
    files.get(path).and_then(|entry| entry.clone())
}

fn virtual_read_or_disk(
    files: &HashMap<PathBuf, Option<String>>,
    path: &Path,
) -> Result<String, String> {
    if let Some(entry) = files.get(path) {
        return entry
            .clone()
            .ok_or_else(|| format!("file not found: {}", path.display()));
    }
    std::fs::read_to_string(path).map_err(|error| format!("{}: {}", path.display(), error))
}

fn accumulate_counts(diff: &serde_json::Value, additions: &mut usize, deletions: &mut usize) {
    *additions += diff.get("additions").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
    *deletions += diff.get("deletions").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
}

fn push_patch(out: &mut String, file: &str, before: &str, after: &str) {
    out.push_str(&edit::build_unified_diff(file, before, after));
    if !out.ends_with('\n') {
        out.push('\n');
    }
}

fn strip_heredoc(input: &str) -> String {
    let trimmed = input.trim();
    let lines = trimmed.lines().collect::<Vec<_>>();
    if lines.len() < 3 {
        return trimmed.to_string();
    }
    let first = lines[0].trim();
    let marker = first
        .strip_prefix("cat ")
        .unwrap_or(first)
        .strip_prefix("<<")
        .map(|s| s.trim_matches(['\'', '"', ' '].as_ref()));
    let Some(marker) = marker else {
        return trimmed.to_string();
    };
    if lines.last().map(|line| line.trim()) != Some(marker) {
        return trimmed.to_string();
    }
    lines[1..lines.len() - 1].join("\n")
}

fn parse_patch(patch_text: &str) -> Result<Vec<Hunk>, String> {
    if patch_text.len() > MAX_PATCH_SIZE {
        return Err(format!(
            "Patch too large: {} bytes exceeds limit of {} bytes",
            patch_text.len(),
            MAX_PATCH_SIZE
        ));
    }

    let cleaned = strip_heredoc(patch_text);
    let lines = cleaned.split('\n').collect::<Vec<_>>();
    let begin_idx = lines
        .iter()
        .position(|line| line.trim() == "*** Begin Patch")
        .ok_or_else(|| {
            "Invalid patch format: missing *** Begin Patch / *** End Patch markers".to_string()
        })?;
    let end_idx = lines
        .iter()
        .position(|line| line.trim() == "*** End Patch")
        .ok_or_else(|| {
            "Invalid patch format: missing *** Begin Patch / *** End Patch markers".to_string()
        })?;
    if begin_idx >= end_idx {
        return Err(
            "Invalid patch format: missing *** Begin Patch / *** End Patch markers".to_string(),
        );
    }

    let mut hunks = Vec::new();
    let mut i = begin_idx + 1;
    while i < end_idx {
        if let Some(path) = lines[i].strip_prefix("*** Add File:") {
            if hunks.len() >= MAX_HUNKS {
                return Err(format!(
                    "Patch exceeds maximum of {MAX_HUNKS} file operations"
                ));
            }
            let path = path.trim().to_string();
            if path.is_empty() {
                i += 1;
                continue;
            }
            let (contents, next_idx) = parse_add_file_content(&lines, i + 1);
            hunks.push(Hunk::Add { path, contents });
            i = next_idx;
        } else if let Some(path) = lines[i].strip_prefix("*** Delete File:") {
            if hunks.len() >= MAX_HUNKS {
                return Err(format!(
                    "Patch exceeds maximum of {MAX_HUNKS} file operations"
                ));
            }
            let path = path.trim().to_string();
            if !path.is_empty() {
                hunks.push(Hunk::Delete { path });
            }
            i += 1;
        } else if let Some(path) = lines[i].strip_prefix("*** Update File:") {
            if hunks.len() >= MAX_HUNKS {
                return Err(format!(
                    "Patch exceeds maximum of {MAX_HUNKS} file operations"
                ));
            }
            let path = path.trim().to_string();
            if path.is_empty() {
                i += 1;
                continue;
            }
            let mut next_idx = i + 1;
            let mut move_path = None;
            if next_idx < lines.len() {
                if let Some(raw_move) = lines[next_idx].strip_prefix("*** Move to:") {
                    move_path = Some(raw_move.trim().to_string());
                    next_idx += 1;
                }
            }
            let (chunks, after_chunks) = parse_update_file_chunks(&lines, next_idx);
            hunks.push(Hunk::Update {
                path,
                move_path,
                chunks,
            });
            i = after_chunks;
        } else {
            i += 1;
        }
    }

    Ok(hunks)
}

fn parse_add_file_content(lines: &[&str], start_idx: usize) -> (String, usize) {
    let mut content = String::new();
    let mut i = start_idx;
    while i < lines.len() && !lines[i].starts_with("***") {
        if let Some(line) = lines[i].strip_prefix('+') {
            content.push_str(line);
            content.push('\n');
        }
        i += 1;
    }
    if content.ends_with('\n') {
        content.pop();
    }
    (content, i)
}

fn parse_update_file_chunks(lines: &[&str], start_idx: usize) -> (Vec<UpdateFileChunk>, usize) {
    let mut chunks = Vec::new();
    let mut i = start_idx;
    while i < lines.len() && !lines[i].starts_with("***") {
        if lines[i].starts_with("@@") {
            let context = lines[i][2..].trim();
            i += 1;
            let mut old_lines = Vec::new();
            let mut new_lines = Vec::new();
            let mut is_end_of_file = false;
            while i < lines.len() && !lines[i].starts_with("@@") && !lines[i].starts_with("***") {
                let line = lines[i];
                if line == "*** End of File" {
                    is_end_of_file = true;
                    i += 1;
                    break;
                }
                if let Some(content) = line.strip_prefix(' ') {
                    old_lines.push(content.to_string());
                    new_lines.push(content.to_string());
                } else if let Some(content) = line.strip_prefix('-') {
                    old_lines.push(content.to_string());
                } else if let Some(content) = line.strip_prefix('+') {
                    new_lines.push(content.to_string());
                }
                i += 1;
            }
            chunks.push(UpdateFileChunk {
                old_lines,
                new_lines,
                change_context: if context.is_empty() {
                    None
                } else {
                    Some(context.to_string())
                },
                is_end_of_file,
            });
        } else {
            i += 1;
        }
    }
    (chunks, i)
}

fn normalize_unicode(str_value: &str) -> String {
    str_value
        .chars()
        .map(|ch| match ch {
            '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}' => '\'',
            '\u{201C}' | '\u{201D}' | '\u{201E}' | '\u{201F}' => '"',
            '\u{2010}' | '\u{2011}' | '\u{2012}' | '\u{2013}' | '\u{2014}' | '\u{2015}' => '-',
            '\u{2026}' => '…',
            '\u{00A0}' => ' ',
            other => other,
        })
        .collect::<String>()
        .replace('…', "...")
}

fn normalize_indent(str_value: &str) -> String {
    let leading = str_value
        .bytes()
        .take_while(|byte| *byte == b' ' || *byte == b'\t')
        .count();
    if leading == 0 {
        str_value.to_string()
    } else {
        format!("{}{}", " ".repeat(leading), &str_value[leading..])
    }
}

fn try_match<F>(
    lines: &[String],
    pattern: &[String],
    start_index: usize,
    compare: F,
    eof: bool,
) -> Option<usize>
where
    F: Fn(&str, &str) -> bool,
{
    if pattern.is_empty() || pattern.len() > lines.len() {
        return None;
    }

    if eof {
        let from_end = lines.len() - pattern.len();
        if from_end >= start_index
            && pattern
                .iter()
                .enumerate()
                .all(|(idx, expected)| compare(&lines[from_end + idx], expected))
        {
            return Some(from_end);
        }
    }

    let last_start = lines.len() - pattern.len();
    (start_index..=last_start).find(|&index| {
        pattern
            .iter()
            .enumerate()
            .all(|(idx, expected)| compare(&lines[index + idx], expected))
    })
}

fn seek_sequence(
    lines: &[String],
    pattern: &[String],
    start_index: usize,
    eof: bool,
) -> Option<usize> {
    try_match(lines, pattern, start_index, |a, b| a == b, eof)
        .or_else(|| {
            try_match(
                lines,
                pattern,
                start_index,
                |a, b| a.trim_end() == b.trim_end(),
                eof,
            )
        })
        .or_else(|| {
            try_match(
                lines,
                pattern,
                start_index,
                |a, b| a.trim() == b.trim(),
                eof,
            )
        })
        .or_else(|| {
            try_match(
                lines,
                pattern,
                start_index,
                |a, b| normalize_indent(a).trim_end() == normalize_indent(b).trim_end(),
                eof,
            )
        })
        .or_else(|| {
            try_match(
                lines,
                pattern,
                start_index,
                |a, b| normalize_unicode(a.trim()) == normalize_unicode(b.trim()),
                eof,
            )
        })
}

fn compare_any(a: &str, b: &str) -> bool {
    a == b
        || a.trim_end() == b.trim_end()
        || a.trim() == b.trim()
        || normalize_indent(a).trim_end() == normalize_indent(b).trim_end()
        || normalize_unicode(a.trim()) == normalize_unicode(b.trim())
}

fn find_closest_partial_match(
    lines: &[String],
    pattern: &[String],
) -> Option<(usize, usize, usize)> {
    if pattern.is_empty() || lines.is_empty() {
        return None;
    }

    let mut candidates = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        if compare_any(line, &pattern[0]) {
            candidates.push(index);
            if candidates.len() >= 16 {
                break;
            }
        }
    }

    let mut best: Option<(usize, usize, usize)> = None;
    for start in candidates {
        let mut matched = 0usize;
        for idx in 0..pattern.len() {
            if start + idx >= lines.len() || !compare_any(&lines[start + idx], &pattern[idx]) {
                break;
            }
            matched += 1;
        }
        if matched > best.map(|(_, matched, _)| matched).unwrap_or(0) {
            best = Some((start + 1, matched, matched));
        }
    }
    best
}

fn apply_update_chunks(
    original_content: &str,
    file_path: &str,
    chunks: &[UpdateFileChunk],
) -> Result<String, String> {
    let mut original_lines = original_content
        .split('\n')
        .map(str::to_string)
        .collect::<Vec<_>>();
    if original_lines.last().is_some_and(|line| line.is_empty()) {
        original_lines.pop();
    }

    let mut replacements: Vec<(usize, usize, Vec<String>)> = Vec::new();
    let mut line_index = 0usize;

    for chunk in chunks {
        if let Some(context) = &chunk.change_context {
            let context_pattern = vec![context.clone()];
            let Some(context_idx) =
                seek_sequence(&original_lines, &context_pattern, line_index, false)
            else {
                return Err(format!(
                    "Failed to find context '{}' in {}",
                    context, file_path
                ));
            };
            line_index = context_idx + 1;
        }

        if chunk.old_lines.is_empty() {
            let insertion_idx = if chunk.change_context.is_some() {
                line_index
            } else {
                original_lines.len()
            };
            replacements.push((insertion_idx, 0, chunk.new_lines.clone()));
            continue;
        }

        let mut pattern = chunk.old_lines.clone();
        let mut new_slice = chunk.new_lines.clone();
        let mut found = seek_sequence(&original_lines, &pattern, line_index, chunk.is_end_of_file);

        if found.is_none() && pattern.last().is_some_and(|line| line.is_empty()) {
            pattern.pop();
            if new_slice.last().is_some_and(|line| line.is_empty()) {
                new_slice.pop();
            }
            found = seek_sequence(&original_lines, &pattern, line_index, chunk.is_end_of_file);
        }

        if let Some(found_idx) = found {
            replacements.push((found_idx, pattern.len(), new_slice));
            line_index = found_idx + pattern.len();
        } else {
            let already_applied = {
                let new_slice_trimmed = new_slice
                    .iter()
                    .filter(|line| !line.trim().is_empty())
                    .cloned()
                    .collect::<Vec<_>>();
                !new_slice_trimmed.is_empty()
                    && seek_sequence(&original_lines, &new_slice_trimmed, 0, chunk.is_end_of_file)
                        .is_some()
            };

            let closest_hint = if let Some((line_number, matched_lines, first_divergence)) =
                find_closest_partial_match(&original_lines, &pattern)
            {
                let file_line_no = line_number + first_divergence;
                let expected = pattern
                    .get(first_divergence)
                    .cloned()
                    .unwrap_or_else(|| "<EOF>".to_string());
                let actual = original_lines
                    .get(file_line_no.saturating_sub(1))
                    .cloned()
                    .unwrap_or_else(|| "<EOF>".to_string());
                format!(
                    "\n\nClosest match starts at line {} ({} of {} lines matched).\nFirst divergence at line {}:\n  expected: {:?}\n  actual:   {:?}",
                    line_number,
                    matched_lines,
                    pattern.len(),
                    file_line_no,
                    expected,
                    actual
                )
            } else {
                String::new()
            };

            let already_applied_hint = if already_applied {
                "\n\nHint: the replacement content for this hunk already appears in the file. The patch may have been partially applied in a prior turn — re-read the file to confirm which hunks still need to apply."
            } else {
                ""
            };

            return Err(format!(
                "Failed to find expected lines in {}:\n{}\n\nTried match tiers: exact, trimEnd, trim, indent (tab/space), unicode.{}{}",
                file_path,
                chunk.old_lines.join("\n"),
                closest_hint,
                already_applied_hint
            ));
        }
    }

    replacements.sort_by_key(|(start, _, _)| *start);
    let mut result = original_lines;
    for (start, old_len, new_segment) in replacements.into_iter().rev() {
        result.splice(start..start + old_len, new_segment);
    }
    if result.last().is_none_or(|line| !line.is_empty()) {
        result.push(String::new());
    }
    Ok(result.join("\n"))
}
