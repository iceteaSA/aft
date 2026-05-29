use std::path::{Path, PathBuf};

use aft::commands::semantic_search::fuse_hybrid_results;
use aft::query_shape::classify;
use aft::search_index::SearchIndex;
use aft::semantic_index::SemanticResult;
use aft::symbols::SymbolKind;

fn semantic(file: &str, name: &str, score: f32) -> SemanticResult {
    SemanticResult {
        file: PathBuf::from(file),
        name: name.to_string(),
        kind: SymbolKind::Function,
        start_line: 0,
        end_line: 2,
        exported: true,
        snippet: format!("fn {name}() {{}}"),
        score,
        source: "semantic",
    }
}

fn fingerprint(results: &[aft::commands::semantic_search::HybridResult]) -> Vec<String> {
    results
        .iter()
        .map(|result| {
            format!(
                "{}|{}|{}|{:.3}|{:?}|{:?}",
                result.file.display(),
                result.name,
                result.source,
                result.score,
                result.semantic_score,
                result.lexical_score
            )
        })
        .collect()
}

#[test]
fn identifier_file_in_both_lanes_gets_hybrid_boost() {
    let shape = classify("useState");
    let results = fuse_hybrid_results(
        vec![semantic("/project/src/hooks.ts", "useState", 0.4)],
        vec![(PathBuf::from("/project/src/hooks.ts"), 2.0)],
        &shape,
        10,
    );

    // v0.32 contract: a file in both lanes keeps source "semantic" and is flagged
    // hybrid_boosted (source is no longer overloaded with "hybrid").
    assert_eq!(results[0].source, "semantic");
    assert!(results[0].hybrid_boosted);
    assert_eq!(results[0].semantic_score, Some(0.4));
    assert_eq!(results[0].lexical_score, Some(2.0));
    assert!((results[0].score - 0.44).abs() < 0.0001);
}

#[test]
fn identifier_file_only_in_lexical_top_twenty_surfaces() {
    let shape = classify("useState");
    let results = fuse_hybrid_results(
        vec![semantic("/project/src/other.ts", "other", 0.3)],
        vec![(PathBuf::from("/project/src/hooks.ts"), 2.0)],
        &shape,
        10,
    );

    let lexical = results
        .iter()
        .find(|result| result.source == "lexical")
        .expect("lexical-only result");
    assert_eq!(lexical.file, PathBuf::from("/project/src/hooks.ts"));
    assert!(matches!(lexical.kind, SymbolKind::FileSummary));
    assert_eq!(lexical.name, "");
    assert_eq!(lexical.start_line, 0);
    assert_eq!(lexical.end_line, 0);
    assert!((lexical.score - 0.25).abs() < 0.0001);
}

#[test]
fn lexical_candidate_beyond_old_twenty_cap_still_surfaces() {
    // Regression for the silent `.take(20)` sub-cap: with no semantic overlap,
    // a lexical candidate ranked beyond the 20th position used to be dropped
    // from fusion entirely (and the loss was not reflected in
    // more_available/engine_capped). All collected lexical candidates must now
    // be eligible — final bounding is cap_per_file + truncate(top_k) only.
    let shape = classify("useState");
    // 25 distinct lexical-only files (no semantic input), descending scores.
    let lexical_files: Vec<(PathBuf, f32)> = (0..25)
        .map(|i| {
            (
                PathBuf::from(format!("/project/src/file{i:02}.ts")),
                2.5 - (i as f32) * 0.05,
            )
        })
        .collect();
    let target = PathBuf::from("/project/src/file23.ts"); // 24th-ranked (index 23)

    let results = fuse_hybrid_results(Vec::new(), lexical_files, &shape, 100);

    assert!(
        results.iter().any(|result| result.file == target),
        "lexical candidate beyond the old 20-cap must surface: {:?}",
        results
            .iter()
            .map(|r| r.file.display().to_string())
            .collect::<Vec<_>>()
    );
}

#[test]
fn natural_language_query_with_no_lexical_lane_stays_semantic() {
    let shape = classify("how does auth work");
    let results = fuse_hybrid_results(
        vec![semantic("/project/src/auth.ts", "authorize", 0.7)],
        Vec::new(),
        &shape,
        10,
    );

    assert!(results.iter().all(|result| result.source == "semantic"));
}

#[test]
fn per_file_cap_keeps_top_two_results() {
    let shape = classify("useState");
    let results = fuse_hybrid_results(
        vec![
            semantic("/project/src/hooks.ts", "one", 0.9),
            semantic("/project/src/hooks.ts", "two", 0.8),
            semantic("/project/src/hooks.ts", "three", 0.7),
        ],
        vec![(PathBuf::from("/project/src/elsewhere.ts"), 0.5)],
        &shape,
        10,
    );
    let hooks_file = PathBuf::from("/project/src/hooks.ts");

    assert_eq!(
        results
            .iter()
            .filter(|result| result.file == hooks_file)
            .count(),
        2
    );
    assert!(results.iter().any(|result| result.name == "one"));
    assert!(results.iter().any(|result| result.name == "two"));
    assert!(!results.iter().any(|result| result.name == "three"));
}

#[test]
fn same_inputs_produce_stable_results() {
    let shape = classify("useState");
    let semantic_results = vec![
        semantic("/project/src/a.ts", "alpha", 0.5),
        semantic("/project/src/b.ts", "beta", 0.5),
    ];
    let lexical = vec![
        (PathBuf::from("/project/src/b.ts"), 1.0),
        (PathBuf::from("/project/src/c.ts"), 0.9),
    ];

    let first = fuse_hybrid_results(semantic_results.clone(), lexical.clone(), &shape, 10);
    let second = fuse_hybrid_results(semantic_results, lexical, &shape, 10);

    assert_eq!(fingerprint(&first), fingerprint(&second));
}

const LEXICAL_CAP_QUERY: &str = "lexicalcapneedle";

fn code_file_filter(path: &Path) -> bool {
    path.extension().and_then(|extension| extension.to_str()) == Some("rs")
}

fn index_matching_files(index: &mut SearchIndex, extension: &str, start: usize, count: usize) {
    for offset in 0..count {
        let file_id = start + offset;
        let path = PathBuf::from(format!("/project/{extension}/{file_id:03}.{extension}"));
        index.index_file(&path, LEXICAL_CAP_QUERY.as_bytes());
    }
}

#[test]
fn lexical_candidate_cap_filters_extensions_before_truncating() {
    let mut index = SearchIndex::new();
    index_matching_files(&mut index, "lock", 0, 200);
    index_matching_files(&mut index, "rs", 200, 201);

    let query_trigrams = SearchIndex::query_trigrams_from_tokens(&[LEXICAL_CAP_QUERY]);
    let result = index.lexical_rank_with_stats(
        &query_trigrams,
        Some(&code_file_filter as &dyn Fn(&Path) -> bool),
        25,
    );

    assert_eq!(result.files.len(), 25);
    assert!(result
        .files
        .iter()
        .all(|(path, _)| code_file_filter(path.as_path())));
    assert!(result.engine_capped);
}

#[test]
fn lexical_engine_capped_reports_pre_filter_candidate_pressure() {
    let mut index = SearchIndex::new();
    index_matching_files(&mut index, "lock", 0, 201);
    index_matching_files(&mut index, "rs", 201, 1);

    let query_trigrams = SearchIndex::query_trigrams_from_tokens(&[LEXICAL_CAP_QUERY]);
    let result = index.lexical_rank_with_stats(
        &query_trigrams,
        Some(&code_file_filter as &dyn Fn(&Path) -> bool),
        10,
    );

    assert_eq!(result.files.len(), 1);
    assert!(code_file_filter(result.files[0].0.as_path()));
    assert!(result.engine_capped);
}
