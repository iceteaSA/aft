use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

use rayon::prelude::*;
use serde::Serialize;

use crate::parser::{detect_language, LangId};

#[derive(Debug, Clone, Default, Serialize)]
struct MetricCounts {
    file_count: usize,
    symbol_count: usize,
    loc: usize,
}

impl MetricCounts {
    fn add_file(&mut self, file: &FileMetric) {
        self.file_count += 1;
        self.symbol_count += file.symbol_count;
        self.loc += file.loc;
    }
}

#[derive(Debug, Clone)]
struct FileMetric {
    path: PathBuf,
    language: &'static str,
    symbol_count: usize,
    loc: usize,
}

#[derive(Debug, Clone, Serialize)]
struct TopFileMetric {
    file: String,
    loc: usize,
    symbol_count: usize,
}

pub fn run_metrics_scan(job: &crate::inspect::InspectJob) -> crate::inspect::InspectResult {
    let started = Instant::now();
    let per_file = job
        .scope_files
        .par_iter()
        .map(|path| scan_file(path, job))
        .collect::<Vec<_>>();
    let aggregate = aggregate_metrics(&job.project_root, &per_file);
    let success = crate::inspect::InspectScanSuccess {
        scanned_files: job.scope_files.clone(),
        contributions: Vec::new(),
        aggregate,
    };

    crate::inspect::InspectResult::success(job, success, started.elapsed())
}

fn scan_file(path: &Path, job: &crate::inspect::InspectJob) -> FileMetric {
    FileMetric {
        path: path.to_path_buf(),
        language: language_key(path),
        symbol_count: cached_symbol_count(path, job),
        loc: line_count(path),
    }
}

fn cached_symbol_count(path: &Path, job: &crate::inspect::InspectJob) -> usize {
    let Ok(metadata) = fs::metadata(path) else {
        return 0;
    };
    let Ok(mtime) = metadata.modified() else {
        return 0;
    };
    let Ok(cache) = job.symbol_cache.read() else {
        return 0;
    };

    cache
        .symbol_count_if_metadata_matches(path, mtime, metadata.len())
        .unwrap_or(0)
}

fn line_count(path: &Path) -> usize {
    fs::read(path)
        .map(|content| content.iter().filter(|byte| **byte == b'\n').count() + 1)
        .unwrap_or(0)
}

fn aggregate_metrics(project_root: &Path, per_file: &[FileMetric]) -> serde_json::Value {
    let mut totals = MetricCounts::default();
    let mut by_language = BTreeMap::<&'static str, MetricCounts>::new();
    let mut top_files = Vec::with_capacity(per_file.len());

    for file in per_file {
        totals.add_file(file);
        by_language.entry(file.language).or_default().add_file(file);
        top_files.push(TopFileMetric {
            file: display_path(project_root, &file.path),
            loc: file.loc,
            symbol_count: file.symbol_count,
        });
    }

    top_files.sort_by(|left, right| {
        right
            .loc
            .cmp(&left.loc)
            .then_with(|| left.file.cmp(&right.file))
    });
    top_files.truncate(20);

    serde_json::json!({
        "files": totals.file_count,
        "symbols": totals.symbol_count,
        "loc": totals.loc,
        "totals": totals,
        "by_language": by_language,
        "top_files_by_loc": top_files,
    })
}

fn display_path(project_root: &Path, path: &Path) -> String {
    path.strip_prefix(project_root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

fn language_key(path: &Path) -> &'static str {
    match detect_language(path) {
        Some(LangId::TypeScript) => "typescript",
        Some(LangId::Tsx) => "tsx",
        Some(LangId::JavaScript) => "javascript",
        Some(LangId::Python) => "python",
        Some(LangId::Rust) => "rust",
        Some(LangId::Go) => "go",
        Some(LangId::C) => "c",
        Some(LangId::Cpp) => "cpp",
        Some(LangId::Zig) => "zig",
        Some(LangId::CSharp) => "csharp",
        Some(LangId::Bash) => "bash",
        Some(LangId::Html) => "html",
        Some(LangId::Markdown) => "markdown",
        Some(LangId::Solidity) => "solidity",
        Some(LangId::Vue) => "vue",
        Some(LangId::Json) => "json",
        Some(LangId::Scala) => "scala",
        Some(LangId::Java) => "java",
        Some(LangId::Ruby) => "ruby",
        Some(LangId::Kotlin) => "kotlin",
        Some(LangId::Swift) => "swift",
        Some(LangId::Php) => "php",
        Some(LangId::Lua) => "lua",
        Some(LangId::Perl) => "perl",
        None => "unknown",
    }
}
