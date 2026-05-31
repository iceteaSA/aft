use std::collections::HashSet;
use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use rayon::prelude::*;

use crate::commands::multi_path::{
    canonical_key, dedupe_nested_paths, resolve_path_or_multi, SearchPathResolution,
};
use crate::context::AppContext;
use crate::pattern_compile::{CompiledPattern, LiteralSearch};
use crate::protocol::Response;
use crate::search_index::{
    build_path_filters, read_searchable_text, resolve_search_scope,
    sort_grep_matches_by_mtime_desc, walk_project_files_from, GrepMatch, GrepResult, IndexStatus,
};

#[derive(Clone, Debug)]
pub struct GrepParams {
    pub include: Vec<String>,
    pub exclude: Vec<String>,
    pub max_results: usize,
}

#[derive(Clone, Debug)]
pub struct GrepScope {
    pub roots: Vec<ResolvedRoot>,
    pub multi_root: bool,
    pub per_root_max: usize,
}

#[derive(Clone, Debug)]
pub struct ResolvedRoot {
    pub search_root: PathBuf,
    pub filter_root: PathBuf,
    pub use_index: bool,
    pub is_external: bool,
}

pub fn project_root(ctx: &AppContext) -> PathBuf {
    let project_root = ctx
        .config()
        .project_root
        .clone()
        .unwrap_or_else(|| env::current_dir().unwrap_or_default());
    std::fs::canonicalize(&project_root).unwrap_or(project_root)
}

pub fn resolve_grep_scope(
    ctx: &AppContext,
    paths: Option<&serde_json::Value>,
    max_results: usize,
    req_id: &str,
) -> Result<GrepScope, Response> {
    let project_root = project_root(ctx);
    let search_roots = resolve_roots(ctx, paths, &project_root, req_id)?;

    if let Some(missing_root) = search_roots.iter().find(|root| !root.exists()) {
        return Err(Response::error(
            req_id,
            "path_not_found",
            format!(
                "grep: search path does not exist: {}",
                missing_root.display()
            ),
        ));
    }

    let roots = search_roots
        .into_iter()
        .map(|search_root| {
            let scope = resolve_search_scope(&project_root, Some(&search_root.to_string_lossy()));
            let is_external = !scope.use_index;
            let filter_root =
                compute_filter_root(&project_root, &scope.root, scope.use_index, is_external);
            ResolvedRoot {
                search_root: scope.root,
                filter_root,
                use_index: scope.use_index,
                is_external,
            }
        })
        .collect::<Vec<_>>();

    let multi_root = roots.len() > 1;
    let per_root_max = if multi_root {
        max_results.saturating_mul(2).max(max_results)
    } else {
        max_results
    };

    Ok(GrepScope {
        roots,
        multi_root,
        per_root_max,
    })
}

pub fn compute_filter_root(
    project_root: &Path,
    search_root: &Path,
    use_index: bool,
    is_external: bool,
) -> PathBuf {
    if is_external && !use_index {
        search_root.to_path_buf()
    } else {
        project_root.to_path_buf()
    }
}

pub fn scope_has_files(project_root: &Path, scope: &GrepScope) -> bool {
    scope.roots.iter().any(|root| {
        // An explicitly-named existing file is always in scope (it's searched
        // directly even if gitignored / .aftignored), so don't report it as
        // "no files matched scope".
        if root.search_root.is_file() {
            return true;
        }
        walk_project_files_from(
            &root.filter_root,
            &root.search_root,
            &build_path_filters(&["**/*".to_string()], &[]).expect("valid catch-all glob"),
        )
        .into_iter()
        .next()
        .is_some()
            || walk_project_files_from(
                project_root,
                &root.search_root,
                &build_path_filters(&["**/*".to_string()], &[]).expect("valid catch-all glob"),
            )
            .into_iter()
            .next()
            .is_some()
    })
}

pub fn execute(
    ctx: &AppContext,
    pattern: &CompiledPattern,
    scope: &GrepScope,
    params: &GrepParams,
) -> GrepResult {
    let project_root = project_root(ctx);
    if scope.roots.len() == 1 {
        return execute_root(
            ctx,
            pattern,
            &scope.roots[0],
            params,
            params.max_results,
            &project_root,
        );
    }

    let mut results = Vec::new();
    for root in &scope.roots {
        results.push(execute_root(
            ctx,
            pattern,
            root,
            params,
            scope.per_root_max,
            &project_root,
        ));
    }
    merge_grep_results(results, &project_root, params.max_results)
}

fn resolve_roots(
    ctx: &AppContext,
    paths: Option<&serde_json::Value>,
    project_root: &Path,
    req_id: &str,
) -> Result<Vec<PathBuf>, Response> {
    let Some(paths) = paths else {
        return Ok(vec![resolve_search_scope(project_root, None).root]);
    };
    if paths.is_null() {
        return Ok(vec![resolve_search_scope(project_root, None).root]);
    }
    if let Some(path) = paths.as_str() {
        return match resolve_path_or_multi(
            path,
            project_root,
            |candidate| ctx.validate_path(req_id, candidate),
            req_id,
        )? {
            SearchPathResolution::Single(root) => Ok(vec![root]),
            SearchPathResolution::Multi(roots) => Ok(roots),
        };
    }
    if let Some(items) = paths.as_array() {
        let mut roots = Vec::with_capacity(items.len());
        for item in items {
            let Some(path) = item.as_str() else {
                return Err(Response::error(
                    req_id,
                    "invalid_request",
                    "grep: path array entries must be strings",
                ));
            };
            let validated = ctx.validate_path(req_id, Path::new(path))?;
            let raw = validated.to_string_lossy();
            roots.push(resolve_search_scope(project_root, Some(raw.as_ref())).root);
        }
        let roots = dedupe_nested_paths(roots);
        if roots.is_empty() {
            Ok(vec![resolve_search_scope(project_root, None).root])
        } else {
            Ok(roots)
        }
    } else {
        Err(Response::error(
            req_id,
            "invalid_request",
            "grep: path must be a string, array of strings, or null",
        ))
    }
}

fn execute_root(
    ctx: &AppContext,
    pattern: &CompiledPattern,
    root: &ResolvedRoot,
    params: &GrepParams,
    max_results: usize,
    project_root: &Path,
) -> GrepResult {
    // Explicit single-file scope: search the named file directly, bypassing the
    // trigram index and the gitignore/.aftignore-aware walk. Matches ripgrep,
    // where naming a file explicitly searches it even when it is gitignored,
    // .aftignored, or not yet indexed. Binary + UTF-8 guards still apply.
    if root.search_root.is_file() {
        let index_status = if root.use_index {
            current_index_status(ctx)
        } else {
            IndexStatus::Fallback
        };
        return grep_explicit_file(&root.search_root, pattern, max_results, index_status);
    }

    let search_index = ctx.search_index().borrow();
    match search_index.as_ref() {
        Some(index) if index.ready && root.use_index => index.search_grep(
            pattern,
            &params.include,
            &params.exclude,
            &root.search_root,
            max_results,
        ),
        _ => {
            if !root.use_index {
                if let Some(result) = ripgrep_grep(
                    &root.search_root,
                    pattern,
                    &params.include,
                    &params.exclude,
                    max_results,
                ) {
                    return result;
                }
            }
            let index_status = if root.use_index {
                current_index_status(ctx)
            } else {
                IndexStatus::Fallback
            };
            fallback_grep(
                project_root,
                &root.search_root,
                &root.filter_root,
                pattern,
                &params.include,
                &params.exclude,
                max_results,
                index_status,
            )
        }
    }
}

/// Grep a single explicitly-named file directly, bypassing the trigram index
/// and the gitignore/.aftignore-aware walk. Used when the caller's `path`
/// resolves to one existing file — ripgrep semantics: an explicitly-named file
/// is searched even when it is gitignored, `.aftignore`d, or not yet indexed.
/// Binary detection and UTF-8 guards still apply (via `read_searchable_text`
/// inside `fallback_search_file`).
fn grep_explicit_file(
    file: &Path,
    pattern: &CompiledPattern,
    max_results: usize,
    index_status: IndexStatus,
) -> GrepResult {
    let total_matches = AtomicUsize::new(0);
    let files_searched = AtomicUsize::new(0);
    let files_with_matches = AtomicUsize::new(0);
    let truncated = AtomicBool::new(false);
    let engine_capped = AtomicBool::new(false);
    let stop_after = max_results.saturating_mul(2);

    let matches = fallback_search_file(
        &file.to_path_buf(),
        pattern,
        max_results,
        stop_after,
        &total_matches,
        &files_searched,
        &files_with_matches,
        &truncated,
        &engine_capped,
    );

    GrepResult {
        total_matches: total_matches.load(Ordering::Relaxed),
        matches,
        files_searched: files_searched.load(Ordering::Relaxed),
        files_with_matches: files_with_matches.load(Ordering::Relaxed),
        index_status,
        truncated: truncated.load(Ordering::Relaxed),
        fully_degraded: false,
        engine_capped: engine_capped.load(Ordering::Relaxed),
    }
}

pub fn merge_grep_results(
    results: Vec<GrepResult>,
    project_root: &Path,
    max_results: usize,
) -> GrepResult {
    let mut matches = Vec::new();
    let mut total_matches = 0usize;
    let mut files_searched = 0usize;
    let mut files_with_matches = 0usize;
    let mut index_status = IndexStatus::Ready;
    let mut any_child_truncated = false;
    let mut fully_degraded = false;
    let mut engine_capped = false;
    let mut seen_match_keys = HashSet::new();

    for result in results {
        total_matches += result.total_matches;
        files_searched += result.files_searched;
        files_with_matches += result.files_with_matches;
        index_status = weakest_index_status(index_status, result.index_status);
        any_child_truncated |= result.truncated;
        fully_degraded |= result.fully_degraded;
        engine_capped |= result.engine_capped;

        for grep_match in result.matches {
            let file_key = canonical_key(&grep_match.file);
            let match_key = (file_key, grep_match.line, grep_match.column);
            if seen_match_keys.insert(match_key) {
                matches.push(grep_match);
            }
        }
    }

    sort_grep_matches_by_mtime_desc(&mut matches, project_root);
    if matches.len() > max_results {
        matches.truncate(max_results);
    }

    GrepResult {
        matches,
        total_matches,
        files_searched,
        files_with_matches,
        index_status,
        truncated: any_child_truncated || total_matches > max_results,
        fully_degraded,
        engine_capped,
    }
}

pub fn weakest_index_status(left: IndexStatus, right: IndexStatus) -> IndexStatus {
    match (left, right) {
        (IndexStatus::Disabled, _) | (_, IndexStatus::Disabled) => IndexStatus::Disabled,
        (IndexStatus::Fallback, _) | (_, IndexStatus::Fallback) => IndexStatus::Fallback,
        (IndexStatus::Building, _) | (_, IndexStatus::Building) => IndexStatus::Building,
        (IndexStatus::Ready, IndexStatus::Ready) => IndexStatus::Ready,
    }
}

fn fallback_grep(
    project_root: &Path,
    search_root: &Path,
    filter_root: &Path,
    pattern: &CompiledPattern,
    include: &[String],
    exclude: &[String],
    max_results: usize,
    index_status: IndexStatus,
) -> GrepResult {
    let filters = build_path_filters(include, exclude).unwrap_or_default();
    let files = walk_project_files_from(filter_root, search_root, &filters);

    let total_matches = AtomicUsize::new(0);
    let files_searched = AtomicUsize::new(0);
    let files_with_matches = AtomicUsize::new(0);
    let truncated = AtomicBool::new(false);
    let engine_capped = AtomicBool::new(false);
    let stop_after = max_results.saturating_mul(2);

    let mut matches = files
        .par_iter()
        .map(|file| {
            fallback_search_file(
                file,
                pattern,
                max_results,
                stop_after,
                &total_matches,
                &files_searched,
                &files_with_matches,
                &truncated,
                &engine_capped,
            )
        })
        .reduce(Vec::new, |mut left, mut right| {
            left.append(&mut right);
            left
        });

    sort_grep_matches_by_mtime_desc(&mut matches, project_root);

    GrepResult {
        total_matches: total_matches.load(Ordering::Relaxed),
        matches,
        files_searched: files_searched.load(Ordering::Relaxed),
        files_with_matches: files_with_matches.load(Ordering::Relaxed),
        index_status,
        truncated: truncated.load(Ordering::Relaxed),
        fully_degraded: true,
        engine_capped: engine_capped.load(Ordering::Relaxed),
    }
}

fn fallback_search_file(
    file: &PathBuf,
    pattern: &CompiledPattern,
    max_results: usize,
    stop_after: usize,
    total_matches: &AtomicUsize,
    files_searched: &AtomicUsize,
    files_with_matches: &AtomicUsize,
    truncated: &AtomicBool,
    engine_capped: &AtomicBool,
) -> Vec<GrepMatch> {
    if should_stop_fallback_search(truncated, total_matches, stop_after) {
        engine_capped.store(true, Ordering::Relaxed);
        return Vec::new();
    }

    let Some(content) = read_searchable_text(file) else {
        return Vec::new();
    };
    files_searched.fetch_add(1, Ordering::Relaxed);

    let line_starts = line_starts(&content);
    let mut seen_lines = HashSet::new();
    let mut matched_this_file = false;
    let mut matches = Vec::new();

    match pattern {
        CompiledPattern::Literal(literal) => search_literal_in_text(
            file,
            &content,
            &line_starts,
            literal,
            max_results,
            stop_after,
            total_matches,
            &mut seen_lines,
            truncated,
            engine_capped,
            &mut matched_this_file,
            &mut matches,
        ),
        CompiledPattern::Regex { compiled, .. } => {
            for matched in compiled.find_iter(content.as_bytes()) {
                if should_stop_fallback_search(truncated, total_matches, stop_after) {
                    engine_capped.store(true, Ordering::Relaxed);
                    break;
                }

                let (line, column, line_text) =
                    line_details(&content, &line_starts, matched.start());
                if !seen_lines.insert(line) {
                    continue;
                }

                matched_this_file = true;
                let match_number = total_matches.fetch_add(1, Ordering::Relaxed) + 1;
                if match_number > max_results {
                    truncated.store(true, Ordering::Relaxed);
                    break;
                }

                matches.push(GrepMatch {
                    file: file.clone(),
                    line,
                    column,
                    line_text,
                    match_text: String::from_utf8_lossy(matched.as_bytes()).into_owned(),
                });
            }
        }
    }

    if matched_this_file {
        files_with_matches.fetch_add(1, Ordering::Relaxed);
    }

    matches
}

fn search_literal_in_text(
    file: &Path,
    content: &str,
    line_starts: &[usize],
    literal: &LiteralSearch,
    max_results: usize,
    stop_after: usize,
    total_matches: &AtomicUsize,
    seen_lines: &mut HashSet<u32>,
    truncated: &AtomicBool,
    engine_capped: &AtomicBool,
    matched_this_file: &mut bool,
    matches: &mut Vec<GrepMatch>,
) {
    let content_bytes = content.as_bytes();
    let search_content;
    let haystack = if literal.case_insensitive_ascii {
        search_content = content_bytes.to_ascii_lowercase();
        search_content.as_slice()
    } else {
        content_bytes
    };
    let finder = memchr::memmem::Finder::new(&literal.needle);
    let mut start = 0usize;

    while let Some(position) = finder.find(&haystack[start..]) {
        if should_stop_fallback_search(truncated, total_matches, stop_after) {
            engine_capped.store(true, Ordering::Relaxed);
            break;
        }

        let offset = start + position;
        start = offset + 1;
        let (line, column, line_text) = line_details(content, line_starts, offset);
        if !seen_lines.insert(line) {
            continue;
        }

        *matched_this_file = true;
        let match_number = total_matches.fetch_add(1, Ordering::Relaxed) + 1;
        if match_number > max_results {
            truncated.store(true, Ordering::Relaxed);
            break;
        }

        let end = offset + literal.needle.len();
        matches.push(GrepMatch {
            file: file.to_path_buf(),
            line,
            column,
            line_text,
            match_text: String::from_utf8_lossy(&content_bytes[offset..end]).into_owned(),
        });
    }
}

fn should_stop_fallback_search(
    truncated: &AtomicBool,
    total_matches: &AtomicUsize,
    stop_after: usize,
) -> bool {
    truncated.load(Ordering::Relaxed) && total_matches.load(Ordering::Relaxed) >= stop_after
}

fn ripgrep_grep(
    search_root: &Path,
    pattern: &CompiledPattern,
    include: &[String],
    exclude: &[String],
    max_results: usize,
) -> Option<GrepResult> {
    let rg = which_rg()?;
    let mut cmd = Command::new(rg);
    cmd.args(["-nH", "--hidden", "--no-messages", "--json"]);
    if pattern.case_insensitive() {
        cmd.arg("-i");
    }
    if pattern.is_literal() {
        cmd.arg("-F");
    }
    for inc in include {
        cmd.args(["--glob", inc]);
    }
    for exc in exclude {
        let negated = if exc.starts_with('!') {
            exc.clone()
        } else {
            format!("!{exc}")
        };
        cmd.args(["--glob", &negated]);
    }
    cmd.arg(format!("--regexp={}", pattern.ripgrep_pattern()))
        .arg(search_root);

    let output = cmd.output().ok()?;
    let stdout = String::from_utf8_lossy(&output.stdout);

    let mut matches = Vec::new();
    let mut total_matches = 0usize;
    let mut files_with_matches_set: HashSet<PathBuf> = HashSet::new();
    let mut truncated = false;
    let mut engine_capped = false;
    let stop_after = max_results.saturating_mul(2);

    for line in stdout.lines() {
        let parsed: serde_json::Value = serde_json::from_str(line).ok()?;
        if parsed.get("type").and_then(|value| value.as_str()) != Some("match") {
            continue;
        }

        let data = parsed.get("data")?;
        let file_str = data
            .get("path")
            .and_then(|value| value.get("text"))
            .and_then(|value| value.as_str())?;
        let line_num = data
            .get("line_number")
            .and_then(|value| value.as_u64())
            .and_then(|value| u32::try_from(value).ok())?;
        let line_text = data
            .get("lines")
            .and_then(|value| value.get("text"))
            .and_then(|value| value.as_str())?
            .trim_end_matches(['\r', '\n'])
            .to_string();
        let file_path = PathBuf::from(file_str);

        total_matches += 1;
        files_with_matches_set.insert(file_path.clone());

        if matches.len() < max_results {
            matches.push(GrepMatch {
                file: file_path,
                line: line_num,
                column: 0,
                line_text,
                match_text: String::new(),
            });
        } else {
            truncated = true;
        }
        if truncated && total_matches >= stop_after {
            engine_capped = true;
            break;
        }
    }

    Some(GrepResult {
        total_matches,
        matches,
        files_searched: 0,
        files_with_matches: files_with_matches_set.len(),
        index_status: IndexStatus::Fallback,
        truncated,
        fully_degraded: true,
        engine_capped,
    })
}

pub(crate) fn ripgrep_glob(
    search_root: &Path,
    pattern: &str,
    max_results: usize,
) -> Option<Vec<PathBuf>> {
    let rg = which_rg()?;
    let mut cmd = Command::new(rg);
    cmd.args(["--files", "--hidden", "--glob=!.git/*"])
        .arg(format!("--glob={pattern}"))
        .arg(search_root);

    let output = cmd.output().ok()?;
    let stdout = String::from_utf8_lossy(&output.stdout);

    let files: Vec<PathBuf> = stdout
        .lines()
        .take(max_results)
        .map(PathBuf::from)
        .collect();

    Some(files)
}

fn which_rg() -> Option<PathBuf> {
    if let Some(path_var) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path_var) {
            let candidate = dir.join(if cfg!(windows) { "rg.exe" } else { "rg" });
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }

    None
}

fn current_index_status(ctx: &AppContext) -> IndexStatus {
    if ctx
        .search_index()
        .borrow()
        .as_ref()
        .is_some_and(|index| index.ready)
    {
        IndexStatus::Ready
    } else if ctx.search_index_rx().borrow().is_some() || ctx.search_index().borrow().is_some() {
        IndexStatus::Building
    } else {
        IndexStatus::Fallback
    }
}

pub fn line_starts(content: &str) -> Vec<usize> {
    let mut starts = vec![0usize];
    for (index, byte) in content.bytes().enumerate() {
        if byte == b'\n' {
            starts.push(index + 1);
        }
    }
    starts
}

pub fn line_details(content: &str, line_starts: &[usize], offset: usize) -> (u32, u32, String) {
    let line_index = match line_starts.binary_search(&offset) {
        Ok(index) => index,
        Err(index) => index.saturating_sub(1),
    };
    let line_start = line_starts.get(line_index).copied().unwrap_or(0);
    let line_end = content[line_start..]
        .find('\n')
        .map(|length| line_start + length)
        .unwrap_or(content.len());
    let line_text = content[line_start..line_end]
        .trim_end_matches('\r')
        .to_string();
    let column = content[line_start..offset].chars().count() as u32 + 1;
    (line_index as u32 + 1, column, line_text)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grep_match(file: &Path, line: u32, column: u32) -> GrepMatch {
        GrepMatch {
            file: file.to_path_buf(),
            line,
            column,
            line_text: "needle".to_string(),
            match_text: "needle".to_string(),
        }
    }

    fn result(matches: Vec<GrepMatch>, truncated: bool, status: IndexStatus) -> GrepResult {
        GrepResult {
            total_matches: matches.len(),
            files_searched: matches.len(),
            files_with_matches: matches.len(),
            matches,
            index_status: status,
            truncated,
            fully_degraded: false,
            engine_capped: false,
        }
    }

    #[test]
    fn single_root_uses_requested_max() {
        let scope = GrepScope {
            roots: vec![ResolvedRoot {
                search_root: PathBuf::from("/project"),
                filter_root: PathBuf::from("/project"),
                use_index: true,
                is_external: false,
            }],
            multi_root: false,
            per_root_max: 10,
        };
        assert!(!scope.multi_root);
        assert_eq!(scope.per_root_max, 10);
    }

    #[test]
    fn multi_root_uses_double_per_root_max() {
        let project = tempfile::tempdir().expect("project");
        let ctx = AppContext::new(
            Box::new(crate::parser::TreeSitterProvider::new()),
            crate::config::Config {
                project_root: Some(project.path().to_path_buf()),
                ..crate::config::Config::default()
            },
        );
        let left = project.path().join("left");
        let right = project.path().join("right");
        std::fs::create_dir_all(&left).expect("left");
        std::fs::create_dir_all(&right).expect("right");
        let paths = serde_json::json!([left.display().to_string(), right.display().to_string()]);

        let scope = resolve_grep_scope(&ctx, Some(&paths), 10, "test").expect("scope");

        assert!(scope.multi_root);
        assert_eq!(scope.per_root_max, 20);
    }

    #[test]
    fn filter_root_is_project_for_in_project_and_search_root_for_external_unindexed() {
        let project = PathBuf::from("/project");
        let in_project = compute_filter_root(&project, Path::new("/project/src"), true, false);
        let external = compute_filter_root(&project, Path::new("/tmp/external"), false, true);
        assert_eq!(in_project, project);
        assert_eq!(external, PathBuf::from("/tmp/external"));
    }

    #[test]
    fn weakest_status_orders_disabled_fallback_building_ready() {
        assert_eq!(
            weakest_index_status(IndexStatus::Ready, IndexStatus::Building),
            IndexStatus::Building
        );
        assert_eq!(
            weakest_index_status(IndexStatus::Building, IndexStatus::Fallback),
            IndexStatus::Fallback
        );
        assert_eq!(
            weakest_index_status(IndexStatus::Fallback, IndexStatus::Disabled),
            IndexStatus::Disabled
        );
    }

    #[test]
    fn merge_dedupes_by_canonical_file_line_column() {
        let temp = tempfile::tempdir().expect("temp");
        let file = temp.path().join("file.rs");
        std::fs::write(&file, "needle").expect("write");
        let symlink = temp.path().join("link.rs");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&file, &symlink).expect("symlink");
        #[cfg(windows)]
        std::os::windows::fs::symlink_file(&file, &symlink).expect("symlink");

        let merged = merge_grep_results(
            vec![
                result(vec![grep_match(&file, 1, 1)], false, IndexStatus::Ready),
                result(vec![grep_match(&symlink, 1, 1)], false, IndexStatus::Ready),
            ],
            temp.path(),
            10,
        );

        assert_eq!(merged.matches.len(), 1);
    }

    #[test]
    fn merge_truncated_when_child_truncated_or_pre_merge_exceeds_max() {
        let root = Path::new("/project");
        let child = merge_grep_results(
            vec![result(
                vec![grep_match(Path::new("/project/a.rs"), 1, 1)],
                true,
                IndexStatus::Ready,
            )],
            root,
            10,
        );
        assert!(child.truncated);

        let many = merge_grep_results(
            vec![
                result(
                    vec![grep_match(Path::new("/project/a.rs"), 1, 1)],
                    false,
                    IndexStatus::Ready,
                ),
                result(
                    vec![grep_match(Path::new("/project/b.rs"), 1, 1)],
                    false,
                    IndexStatus::Ready,
                ),
            ],
            root,
            1,
        );
        assert!(many.truncated);
    }
}
