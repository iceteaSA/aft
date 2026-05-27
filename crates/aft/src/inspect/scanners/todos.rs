use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Instant;

use rayon::prelude::*;

use crate::inspect::{InspectJob, InspectResult, InspectScanSuccess};

const MAX_LINES_PER_FILE: usize = 100_000;
const MAX_ITEMS: usize = 100;
const MAX_TEXT_CHARS: usize = 200;
const MARKERS: [&str; 5] = ["TODO", "FIXME", "HACK", "XXX", "BUG"];

#[derive(Debug, Clone)]
struct TodoItem {
    file: String,
    line: usize,
    marker: &'static str,
    author: Option<String>,
    text: String,
}

#[derive(Debug)]
struct FileScan {
    scanned_file: Option<PathBuf>,
    items: Vec<TodoItem>,
}

pub fn run_todos_scan(job: &InspectJob) -> InspectResult {
    let started = Instant::now();
    let per_file: Vec<FileScan> = job
        .scope_files
        .par_iter()
        .map(|path| scan_file(path, &job.project_root))
        .collect();

    let mut scanned_files = Vec::new();
    let mut all_items = Vec::new();
    for scan in per_file {
        if let Some(path) = scan.scanned_file {
            scanned_files.push(path);
        }
        all_items.extend(scan.items);
    }

    let mut by_kind = BTreeMap::new();
    for marker in MARKERS {
        by_kind.insert(marker.to_string(), 0usize);
    }
    for item in &all_items {
        if let Some(count) = by_kind.get_mut(item.marker) {
            *count += 1;
        }
    }

    let total_count = all_items.len();
    let drill_down_capped = total_count > MAX_ITEMS;
    let items = all_items
        .into_iter()
        .take(MAX_ITEMS)
        .map(|item| {
            serde_json::json!({
                "file": item.file,
                "line": item.line,
                "marker": item.marker,
                "author": item.author,
                "text": item.text,
            })
        })
        .collect::<Vec<_>>();

    let aggregate = serde_json::json!({
        "count": total_count,
        "by_kind": by_kind,
        "items": items,
        "drill_down_capped": drill_down_capped,
    });
    let success = InspectScanSuccess {
        scanned_files,
        contributions: Vec::new(),
        aggregate,
    };
    InspectResult::success(job, success, started.elapsed())
}

fn scan_file(path: &Path, project_root: &Path) -> FileScan {
    let Some(source) = read_text_file(path) else {
        return FileScan {
            scanned_file: None,
            items: Vec::new(),
        };
    };

    let file = display_file_path(project_root, path);
    let mut items = Vec::new();
    let mut in_block_comment = false;
    for (line_index, line) in source.lines().take(MAX_LINES_PER_FILE).enumerate() {
        if let Some(item) = scan_line(line, line_index + 1, &file, &mut in_block_comment) {
            items.push(item);
        }
    }

    FileScan {
        scanned_file: Some(path.to_path_buf()),
        items,
    }
}

fn read_text_file(path: &Path) -> Option<String> {
    let bytes = std::fs::read(path).ok()?;
    if bytes.contains(&0) {
        return None;
    }
    String::from_utf8(bytes).ok()
}

fn display_file_path(project_root: &Path, path: &Path) -> String {
    path.strip_prefix(project_root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

fn scan_line(
    line: &str,
    line_number: usize,
    file: &str,
    in_block_comment: &mut bool,
) -> Option<TodoItem> {
    if *in_block_comment {
        let item = parse_todo_body(strip_block_comment_prefix(line), line_number, file);
        if line.contains("*/") {
            *in_block_comment = false;
        }
        if item.is_some() {
            return item;
        }
    }

    let mut search_start = 0usize;
    let mut found_item = None;
    while search_start < line.len() {
        let Some(prefix_match) = find_next_comment_prefix(line, search_start) else {
            break;
        };
        let body = &line[prefix_match.body_start..];
        if prefix_match.starts_block_comment && !body.contains("*/") {
            *in_block_comment = true;
        }
        let body = if prefix_match.starts_block_comment {
            strip_block_comment_prefix(body)
        } else {
            body.trim_start()
        };
        if let Some(item) = parse_todo_body(body, line_number, file) {
            found_item = Some(item);
            break;
        }
        search_start = prefix_match.body_start;
    }

    found_item
}

#[derive(Debug, Clone, Copy)]
struct CommentPrefixMatch {
    body_start: usize,
    starts_block_comment: bool,
}

fn find_next_comment_prefix(line: &str, search_start: usize) -> Option<CommentPrefixMatch> {
    let prefixes = ["<!--", "//", "/*", "#", "--"];
    prefixes
        .iter()
        .filter_map(|prefix| find_prefix_match(line, search_start, prefix))
        .min_by_key(|prefix_match| prefix_match.body_start)
}

fn find_prefix_match(line: &str, search_start: usize, prefix: &str) -> Option<CommentPrefixMatch> {
    let mut cursor = search_start;
    while cursor < line.len() {
        let offset = line[cursor..].find(prefix)?;
        let prefix_start = cursor + offset;
        if is_comment_prefix_boundary(line, prefix_start, prefix) {
            return Some(CommentPrefixMatch {
                body_start: prefix_start + prefix.len(),
                starts_block_comment: prefix == "/*",
            });
        }
        cursor = prefix_start + prefix.len();
    }
    None
}

fn is_comment_prefix_boundary(line: &str, prefix_start: usize, prefix: &str) -> bool {
    if prefix == "<!--" {
        return true;
    }
    prefix_start == 0
        || line[..prefix_start]
            .chars()
            .next_back()
            .is_some_and(char::is_whitespace)
}

fn strip_block_comment_prefix(body: &str) -> &str {
    let mut trimmed = body.trim_start();
    while let Some(rest) = trimmed.strip_prefix('*') {
        trimmed = rest.trim_start();
    }
    trimmed
}

fn parse_todo_body(body: &str, line_number: usize, file: &str) -> Option<TodoItem> {
    let body = body.trim_start();
    for marker in MARKERS {
        let Some(rest) = body.strip_prefix(marker) else {
            continue;
        };
        let Some((author, text_start)) = parse_marker_suffix(marker, rest) else {
            continue;
        };
        return Some(TodoItem {
            file: file.to_string(),
            line: line_number,
            marker,
            author,
            text: truncate_text(strip_comment_closer(text_start)),
        });
    }
    None
}

fn parse_marker_suffix<'a>(
    marker: &'static str,
    rest: &'a str,
) -> Option<(Option<String>, &'a str)> {
    if rest.is_empty() {
        return Some((None, rest));
    }

    let trimmed = rest.trim_start();
    if let Some(after_colon) = trimmed.strip_prefix(':') {
        return Some((None, after_colon.trim_start()));
    }
    if let Some(after_author_start) = trimmed.strip_prefix('(') {
        if !matches!(marker, "TODO" | "FIXME") {
            return None;
        }
        let author_end = after_author_start.find(')')?;
        let author = after_author_start[..author_end].trim();
        if author.is_empty() {
            return None;
        }
        let after_author = &after_author_start[author_end + 1..];
        let after_author = after_author.trim_start();
        let text_start = after_author
            .strip_prefix(':')
            .map(str::trim_start)
            .unwrap_or(after_author);
        return Some((Some(author.to_string()), text_start));
    }
    if rest.chars().next().is_some_and(char::is_whitespace) {
        return Some((None, rest.trim_start()));
    }
    if rest.starts_with("*/") || rest.starts_with("-->") {
        return Some((None, rest));
    }
    None
}

fn strip_comment_closer(text: &str) -> &str {
    let mut trimmed = text.trim();
    loop {
        let without_closer = trimmed
            .strip_suffix("*/")
            .or_else(|| trimmed.strip_suffix("-->"));
        let Some(next) = without_closer else {
            break;
        };
        trimmed = next.trim_end();
    }
    trimmed
}

fn truncate_text(text: &str) -> String {
    text.chars().take(MAX_TEXT_CHARS).collect()
}
