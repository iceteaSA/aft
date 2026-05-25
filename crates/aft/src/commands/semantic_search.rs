use std::collections::{BTreeMap, HashMap, HashSet};
use std::env;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::context::{AppContext, SemanticIndexStatus};
use crate::protocol::{RawRequest, Response};
use crate::query_shape::{self, QueryKind, QueryShape};
use crate::search_index::SearchIndex;
use crate::semantic_index::{
    is_onnx_runtime_unavailable, is_semantic_indexed_extension, EmbeddingModel, SemanticResult,
};
use crate::symbols::SymbolKind;

const DEFAULT_TOP_K: usize = 10;
const MAX_TOP_K: usize = 100;
const HYBRID_LEXICAL_BOOST: f32 = 1.1;
const LEXICAL_ONLY_SCORE_CEILING: f32 = 0.25;

#[derive(Debug, Clone)]
pub struct HybridResult {
    pub file: PathBuf,
    pub name: String,
    pub kind: SymbolKind,
    pub start_line: u32,
    pub end_line: u32,
    pub exported: bool,
    pub score: f32,
    pub source: &'static str,
    pub semantic_score: Option<f32>,
    pub lexical_score: Option<f32>,
    pub snippet: String,
}

#[derive(Debug, Deserialize)]
struct SemanticSearchParams {
    query: String,
    #[serde(default = "default_top_k")]
    top_k: usize,
}

pub fn handle_semantic_search(req: &RawRequest, ctx: &AppContext) -> Response {
    let params = match serde_json::from_value::<SemanticSearchParams>(req.params.clone()) {
        Ok(params) => params,
        Err(error) => {
            return Response::error(
                &req.id,
                "invalid_request",
                format!("semantic_search: invalid params: {error}"),
            );
        }
    };

    let status = ctx.semantic_index_status().borrow().clone();
    match &status {
        SemanticIndexStatus::Disabled => {
            return Response::success(
                &req.id,
                serde_json::json!({
                    "status": "disabled",
                    "text": "Semantic search is not enabled.",
                }),
            );
        }
        SemanticIndexStatus::Failed(error) => {
            return semantic_error_response(&req.id, error);
        }
        SemanticIndexStatus::Building { .. } | SemanticIndexStatus::Ready => {}
    }

    let project_root = ctx
        .config()
        .project_root
        .clone()
        .unwrap_or_else(|| env::current_dir().unwrap_or_default());
    let project_root = std::fs::canonicalize(&project_root).unwrap_or(project_root);

    let shape = query_shape::classify(&params.query);
    let (lexical_files, lexical_index_ready) = collect_lexical_files(ctx, &params.query, &shape);

    if let SemanticIndexStatus::Building {
        stage,
        files,
        entries_done,
        entries_total,
    } = status
    {
        let mut detail = format!("Semantic index is still building (stage: {}).", stage);
        if let Some(files) = files {
            detail.push_str(&format!(" files: {}", files));
        }
        if let Some(entries_done) = entries_done {
            detail.push_str(&format!(" entries done: {}", entries_done));
        }
        if let Some(entries_total) = entries_total {
            detail.push_str(&format!(" / {}", entries_total));
        }

        let results = fuse_hybrid_results(
            Vec::new(),
            lexical_files,
            &shape,
            params.top_k.min(MAX_TOP_K),
        );
        return Response::success(
            &req.id,
            serde_json::json!({
                "status": "building",
                "text": format_building_lexical_text(&detail, &results, &project_root, lexical_index_ready),
                "stage": stage,
                "files": files,
                "entries_done": entries_done,
                "entries_total": entries_total,
                "note": building_lexical_note(lexical_index_ready),
                "results": results.iter().map(result_to_json).collect::<Vec<_>>(),
            }),
        );
    }

    let query_vector = match embed_query(&params.query, ctx) {
        Ok(query_vector) => query_vector,
        Err(error) => return semantic_error_response(&req.id, &error),
    };

    let semantic_results = {
        let semantic_index = ctx.semantic_index().borrow();
        let Some(index) = semantic_index.as_ref() else {
            return Response::success(
                &req.id,
                serde_json::json!({
                    "status": "not_ready",
                    "text": "Semantic index is not ready yet.",
                }),
            );
        };
        index.search(&query_vector, params.top_k.clamp(50, MAX_TOP_K))
    };

    let results = fuse_hybrid_results(
        semantic_results,
        lexical_files,
        &shape,
        params.top_k.min(MAX_TOP_K),
    );

    // No score threshold: silent filtering produced "0 results" even when the
    // model had reasonable matches the agent could have judged. Surface every
    // hit with its score so the caller can decide.

    *ctx.semantic_index_status().borrow_mut() = SemanticIndexStatus::Ready;

    Response::success(
        &req.id,
        serde_json::json!({
            "status": "ready",
            "text": format_semantic_text(&results, &project_root),
            "results": results.iter().map(result_to_json).collect::<Vec<_>>(),
        }),
    )
}

fn default_top_k() -> usize {
    DEFAULT_TOP_K
}

fn collect_lexical_files(
    ctx: &AppContext,
    query: &str,
    shape: &QueryShape,
) -> (Vec<(PathBuf, f32)>, bool) {
    let search_index = ctx.search_index().borrow();
    let Some(index) = search_index.as_ref().filter(|index| index.ready) else {
        return (Vec::new(), false);
    };

    if !shape.weights.should_use_lexical {
        return (Vec::new(), true);
    }

    let tokens = query_shape::extract_tokens(query, shape);
    let token_refs = tokens.iter().map(String::as_str).collect::<Vec<_>>();
    let query_trigrams = SearchIndex::query_trigrams_from_tokens(&token_refs);
    (
        index.lexical_rank(&query_trigrams, Some(&is_semantic_indexed_extension), 50),
        true,
    )
}

fn embed_query(query: &str, ctx: &AppContext) -> Result<Vec<f32>, String> {
    let mut model_ref = ctx.semantic_embedding_model().borrow_mut();
    let semantic_config = ctx.config().semantic.clone();

    if model_ref.is_none() {
        *model_ref = Some(EmbeddingModel::from_config(&semantic_config)?);
    }

    let model = model_ref
        .as_mut()
        .ok_or_else(|| "embedding model was not initialized".to_string())?;
    let query_vector = model
        .embed_query_cached(query)
        .map_err(|error| format!("failed to embed query: {error}"))?;

    if let Some(index) = ctx.semantic_index().borrow().as_ref() {
        if index.len() > 0 && index.dimension() != query_vector.len() {
            return Err(format!(
                "semantic embedding dimension mismatch: query backend returned {}, index expects {}. Rebuild the semantic index for the active backend/model.",
                query_vector.len(),
                index.dimension()
            ));
        }
    }

    Ok(query_vector)
}

pub fn fuse_hybrid_results(
    semantic: Vec<SemanticResult>,
    lexical_files: Vec<(PathBuf, f32)>,
    shape: &QueryShape,
    top_k: usize,
) -> Vec<HybridResult> {
    if top_k == 0 {
        return Vec::new();
    }

    if lexical_files.is_empty() {
        return semantic
            .into_iter()
            .map(|result| hybrid_from_semantic(result, "semantic", None))
            .take(top_k)
            .collect();
    }

    if semantic.is_empty() {
        return lexical_files
            .into_iter()
            .take(top_k)
            .map(|(file, score)| lexical_only_result(file, score, shape))
            .collect();
    }

    let lexical_top_files: HashMap<PathBuf, f32> = lexical_files.iter().take(20).cloned().collect();
    let mut results: Vec<HybridResult> = semantic
        .into_iter()
        .map(|result| {
            if let Some(&lexical_score) = lexical_top_files.get(&result.file) {
                hybrid_from_semantic(result, "hybrid", Some(lexical_score))
            } else {
                hybrid_from_semantic(result, "semantic", None)
            }
        })
        .collect();

    let semantic_files: HashSet<PathBuf> =
        results.iter().map(|result| result.file.clone()).collect();
    for (file, score) in lexical_files.iter().take(20) {
        if !semantic_files.contains(file) {
            results.push(lexical_only_result(file.clone(), *score, shape));
        }
    }

    results.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.file.cmp(&b.file))
            .then_with(|| a.name.cmp(&b.name))
    });
    let mut results = cap_per_file(results, 2);
    results.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.file.cmp(&b.file))
            .then_with(|| a.name.cmp(&b.name))
    });
    results.truncate(top_k);
    results
}

fn hybrid_from_semantic(
    result: SemanticResult,
    source: &'static str,
    lexical_score: Option<f32>,
) -> HybridResult {
    let semantic_score = result.score;
    let score = if source == "hybrid" {
        semantic_score * HYBRID_LEXICAL_BOOST
    } else {
        semantic_score
    };

    HybridResult {
        file: result.file,
        name: result.name,
        kind: result.kind,
        start_line: result.start_line,
        end_line: result.end_line,
        exported: result.exported,
        snippet: result.snippet,
        score,
        source,
        semantic_score: Some(semantic_score),
        lexical_score,
    }
}

fn lexical_only_result(file: PathBuf, lexical_score: f32, shape: &QueryShape) -> HybridResult {
    HybridResult {
        file,
        name: String::new(),
        kind: SymbolKind::FileSummary,
        start_line: 0,
        end_line: 0,
        exported: false,
        // Lexical scores are not cosine-normalized and can exceed the semantic
        // lane's score scale. Keep lexical-only files visible without letting
        // broad trigram overlaps evict strong semantic matches.
        score: (lexical_score * shape_dependent_lexical_only_weight(shape))
            .min(LEXICAL_ONLY_SCORE_CEILING),
        source: "lexical",
        semantic_score: None,
        lexical_score: Some(lexical_score),
        snippet: "[lexical match — use aft_zoom or read for context]".to_string(),
    }
}

fn shape_dependent_lexical_only_weight(shape: &QueryShape) -> f32 {
    match shape.kind {
        QueryKind::Identifier => 0.8,
        QueryKind::Path | QueryKind::ErrorCode | QueryKind::Mixed => 0.5,
        QueryKind::NaturalLanguage => 0.0,
    }
}

fn cap_per_file(results: Vec<HybridResult>, cap: usize) -> Vec<HybridResult> {
    let mut counts: HashMap<PathBuf, usize> = HashMap::new();
    let mut capped = Vec::new();
    for result in results {
        let count = counts.entry(result.file.clone()).or_insert(0);
        if *count < cap {
            *count += 1;
            capped.push(result);
        }
    }
    capped
}

fn semantic_error_response(request_id: &str, error: &str) -> Response {
    if is_onnx_runtime_unavailable(error) {
        return Response::error(
            request_id,
            "semantic_search_unavailable",
            format!("Semantic search unavailable: {error}"),
        );
    }

    Response::error(
        request_id,
        "semantic_search_failed",
        format!("semantic_search: {error}"),
    )
}

fn building_lexical_note(lexical_index_ready: bool) -> &'static str {
    if lexical_index_ready {
        "Semantic index is rebuilding; results are lexical-only fallback results from the trigram index."
    } else {
        "Semantic index is rebuilding; lexical fallback is unavailable because the trigram index is not ready."
    }
}

fn format_building_lexical_text(
    detail: &str,
    results: &[HybridResult],
    project_root: &Path,
    lexical_index_ready: bool,
) -> String {
    let note = building_lexical_note(lexical_index_ready);
    if results.is_empty() {
        return format!(
            "{detail}\n{note}\nFound 0 lexical fallback result(s). [semantic: rebuilding]"
        );
    }

    format!(
        "{detail}\n{note}\n\n{}\n\nFound {} lexical fallback result(s). [semantic: rebuilding]",
        format_result_sections(results, project_root),
        results.len()
    )
}

fn format_semantic_text(results: &[HybridResult], project_root: &Path) -> String {
    if results.is_empty() {
        return "Found 0 semantic result(s). [index: ready]".to_string();
    }

    format!(
        "{}\n\nFound {} semantic result(s). [index: ready]",
        format_result_sections(results, project_root),
        results.len()
    )
}

fn format_result_sections(results: &[HybridResult], project_root: &Path) -> String {
    let mut groups: BTreeMap<String, Vec<&HybridResult>> = BTreeMap::new();

    for result in results {
        let display_path = result
            .file
            .strip_prefix(project_root)
            .unwrap_or(&result.file)
            .display()
            .to_string();
        groups.entry(display_path).or_default().push(result);
    }

    groups
        .into_iter()
        .map(|(file, file_results)| {
            let mut section = file;

            for result in file_results {
                if result.source == "lexical" {
                    section.push_str(&format!(" [lexical match — score: {:.3}]", result.score));
                } else if matches!(result.kind, SymbolKind::FileSummary) {
                    section.push_str(&format!(
                        "\n{} [{}] [file summary] score {:.3} source {}",
                        result.name,
                        symbol_kind_label(&result.kind),
                        result.score,
                        result.source
                    ));
                } else {
                    section.push_str(&format!(
                        "\n{} [{}] lines {}-{} score {:.3} source {}",
                        result.name,
                        symbol_kind_label(&result.kind),
                        display_line_number(result.start_line),
                        display_line_number(result.end_line),
                        result.score,
                        result.source
                    ));
                }

                if !result.snippet.trim().is_empty() {
                    for line in result.snippet.lines() {
                        section.push_str("\n    ");
                        section.push_str(line);
                    }
                }
            }

            section
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn result_to_json(result: &HybridResult) -> serde_json::Value {
    let is_file_level = matches!(result.kind, SymbolKind::FileSummary);
    let (start_line, end_line) = if is_file_level {
        (serde_json::Value::Null, serde_json::Value::Null)
    } else {
        (
            serde_json::json!(display_line_number(result.start_line)),
            serde_json::json!(display_line_number(result.end_line)),
        )
    };

    serde_json::json!({
        "file": result.file.display().to_string(),
        "name": result.name,
        "kind": result.kind,
        "start_line": start_line,
        "end_line": end_line,
        "location": if result.source == "lexical" { "[lexical match]" } else if is_file_level { "[file summary]" } else { "line range" },
        "score": result.score,
        "source": result.source,
        "semantic_score": result.semantic_score,
        "lexical_score": result.lexical_score,
        "snippet": result.snippet,
    })
}

fn display_line_number(line: u32) -> u32 {
    line.saturating_add(1)
}

fn symbol_kind_label(kind: &SymbolKind) -> &'static str {
    match kind {
        SymbolKind::Function => "function",
        SymbolKind::Class => "class",
        SymbolKind::Method => "method",
        SymbolKind::Struct => "struct",
        SymbolKind::Interface => "interface",
        SymbolKind::Enum => "enum",
        SymbolKind::TypeAlias => "type_alias",
        SymbolKind::Variable => "variable",
        SymbolKind::Heading => "heading",
        SymbolKind::FileSummary => "file-summary",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, SemanticBackend, SemanticBackendConfig};
    use crate::context::AppContext;
    use crate::parser::TreeSitterProvider;
    use crate::semantic_index::SemanticIndex;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::path::{Path, PathBuf};
    use std::thread;

    fn semantic_request(query: &str, top_k: usize) -> RawRequest {
        serde_json::from_value(serde_json::json!({
            "id": "semantic-search-test",
            "command": "semantic_search",
            "query": query,
            "top_k": top_k,
        }))
        .expect("build semantic search request")
    }

    fn response_value(response: Response) -> serde_json::Value {
        serde_json::to_value(response).expect("serialize response")
    }

    fn test_context(project_root: &Path) -> AppContext {
        AppContext::new(
            Box::new(TreeSitterProvider::new()),
            Config {
                project_root: Some(project_root.to_path_buf()),
                ..Config::default()
            },
        )
    }

    fn start_mock_embedding_server() -> (String, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind embedding server");
        let addr = listener.local_addr().expect("embedding server addr");
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept embedding request");
            let mut buf = Vec::new();
            let mut chunk = [0u8; 4096];
            let mut header_end = None;
            let mut content_length = 0usize;
            loop {
                let n = stream.read(&mut chunk).expect("read embedding request");
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&chunk[..n]);
                if header_end.is_none() {
                    if let Some(pos) = buf.windows(4).position(|window| window == b"\r\n\r\n") {
                        header_end = Some(pos + 4);
                        for line in String::from_utf8_lossy(&buf[..pos + 4]).lines() {
                            if let Some(value) = line.strip_prefix("Content-Length:") {
                                content_length = value.trim().parse::<usize>().unwrap_or(0);
                            }
                        }
                    }
                }
                if let Some(end) = header_end {
                    if buf.len() >= end + content_length {
                        break;
                    }
                }
            }

            let body = r#"{"data":[{"embedding":[0.1,0.2,0.3],"index":0}]}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            stream
                .write_all(response.as_bytes())
                .expect("write embedding response");
        });

        (format!("http://{}", addr), handle)
    }

    #[test]
    fn building_status_returns_lexical_fallback_results() {
        let project = tempfile::tempdir().expect("create project dir");
        let source_file = project.path().join("src/lib.rs");
        std::fs::create_dir_all(source_file.parent().expect("source parent"))
            .expect("create source dir");
        let source = "pub fn needle_symbol() -> bool { true }\n";
        std::fs::write(&source_file, source).expect("write source file");

        let ctx = test_context(project.path());
        let mut index = SearchIndex::new();
        index.index_file(&source_file, source.as_bytes());
        index.ready = true;
        *ctx.search_index().borrow_mut() = Some(index);
        *ctx.semantic_index_status().borrow_mut() = SemanticIndexStatus::Building {
            stage: "embedding".to_string(),
            files: Some(1),
            entries_done: Some(0),
            entries_total: Some(1),
        };

        let response = response_value(handle_semantic_search(
            &semantic_request("needle_symbol", 5),
            &ctx,
        ));

        assert_eq!(response["success"], true);
        assert_eq!(response["status"], "building");
        assert!(response["note"]
            .as_str()
            .expect("note")
            .contains("lexical-only fallback"));
        assert!(response["text"]
            .as_str()
            .expect("text")
            .contains("lexical fallback"));
        let results = response["results"].as_array().expect("results array");
        assert!(
            results.iter().any(|result| {
                result["source"] == "lexical"
                    && result["file"]
                        .as_str()
                        .expect("file")
                        .ends_with("src/lib.rs")
            }),
            "expected lexical fallback result, got {results:?}"
        );
    }

    #[test]
    fn empty_semantic_index_skips_query_dimension_check() {
        let project = tempfile::tempdir().expect("create project dir");
        let (base_url, handle) = start_mock_embedding_server();
        let ctx = AppContext::new(
            Box::new(TreeSitterProvider::new()),
            Config {
                project_root: Some(project.path().to_path_buf()),
                semantic: SemanticBackendConfig {
                    backend: SemanticBackend::OpenAiCompatible,
                    model: "test-embedding".to_string(),
                    base_url: Some(base_url),
                    api_key_env: None,
                    timeout_ms: 5_000,
                    max_batch_size: 64,
                },
                ..Config::default()
            },
        );
        *ctx.semantic_index_status().borrow_mut() = SemanticIndexStatus::Ready;
        *ctx.semantic_index().borrow_mut() =
            Some(SemanticIndex::new(project.path().to_path_buf(), 384));

        let response = response_value(handle_semantic_search(
            &semantic_request("anything", 5),
            &ctx,
        ));

        assert_eq!(
            response["success"], true,
            "response should not fail: {response:?}"
        );
        assert_eq!(response["status"], "ready");
        assert!(response["results"].as_array().expect("results").is_empty());
        handle.join().expect("embedding server thread");
    }

    #[test]
    fn file_summary_text_uses_summary_location_instead_of_line_range() {
        let project_root = Path::new("/project");
        let results = vec![HybridResult {
            file: PathBuf::from("/project/src/index.ts"),
            name: "index".to_string(),
            kind: SymbolKind::FileSummary,
            start_line: 0,
            end_line: 0,
            exported: false,
            snippet: String::new(),
            score: 0.75,
            source: "semantic",
            semantic_score: Some(0.75),
            lexical_score: None,
        }];

        let text = format_semantic_text(&results, project_root);

        assert!(text.contains("index [file-summary] [file summary] score 0.750 source semantic"));
        assert!(!text.contains("lines 1-1"));
    }

    #[test]
    fn file_summary_json_uses_summary_location_instead_of_line_numbers() {
        let result = HybridResult {
            file: PathBuf::from("/project/src/index.ts"),
            name: "index".to_string(),
            kind: SymbolKind::FileSummary,
            start_line: 0,
            end_line: 0,
            exported: false,
            snippet: String::new(),
            score: 0.75,
            source: "semantic",
            semantic_score: Some(0.75),
            lexical_score: None,
        };

        let json = result_to_json(&result);

        assert_eq!(json["kind"], "file_summary");
        assert_eq!(json["location"], "[file summary]");
        assert!(json["start_line"].is_null());
        assert!(json["end_line"].is_null());
        assert_eq!(json["source"], "semantic");
        assert_eq!(json["semantic_score"], 0.75);
        assert!(json["lexical_score"].is_null());
    }
}
