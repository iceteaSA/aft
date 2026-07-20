//! Handler for the `organize_imports` command: re-group, sort, deduplicate, and
//! optionally merge imports in a file.
//!
//! For all languages: extracts imports, groups by convention, sorts alphabetically
//! within groups, deduplicates, and regenerates the import block with blank-line
//! separators between groups.
//!
//! For Rust: merges separate `use` declarations sharing a common prefix into
//! `use` trees (e.g. `use std::path::Path;` + `use std::path::PathBuf;` →
//! `use std::path::{Path, PathBuf};`). This implements D045's deferred merging.

use std::collections::BTreeMap;
use std::ops::Range;
use std::path::Path;

use crate::context::AppContext;
use crate::edit;
use crate::imports::{self, ImportForm, ImportGroup, ImportKind, ImportStatement};
use crate::parser::{detect_language, LangId};
use crate::protocol::{RawRequest, Response};

/// Handle an `organize_imports` request.
///
/// Params:
///   - `file` (string, required) — target file path
///
/// Returns: `{ file, groups: [{name, count}], removed_duplicates, syntax_valid?, backup_id? }`
pub fn handle_organize_imports(req: &RawRequest, ctx: &AppContext) -> Response {
    let op_id = crate::backup::new_op_id();
    // --- Extract params ---
    let file = match req.params.get("file").and_then(|v| v.as_str()) {
        Some(f) => f,
        None => {
            return Response::error(
                &req.id,
                "invalid_request",
                "organize_imports: missing required param 'file'",
            );
        }
    };

    // --- Validate ---
    let path = match ctx.validate_path(&req.id, Path::new(file)) {
        Ok(path) => path,
        Err(resp) => return resp,
    };
    if !path.exists() {
        return Response::error(
            &req.id,
            "file_not_found",
            format!("organize_imports: file not found: {}", file),
        );
    }

    let lang = match detect_language(&path) {
        Some(l) => l,
        None => {
            return Response::error(
                &req.id,
                "unsupported_language",
                format!(
                    "organize_imports: unsupported file extension: {}",
                    path.extension()
                        .and_then(|e| e.to_str())
                        .unwrap_or("<none>")
                ),
            );
        }
    };

    if !imports::is_supported(lang) {
        return Response::error(
            &req.id,
            "unsupported_language",
            format!(
                "organize_imports: import management not yet supported for {:?}",
                lang
            ),
        );
    }

    // --- Parse file and imports ---
    let (source, _tree, block) = match imports::parse_file_imports(&path, lang) {
        Ok(result) => result,
        Err(e) => {
            return Response::error(&req.id, e.code(), e.to_string());
        }
    };

    if lang == LangId::Vue {
        if let Err(err) = imports::vue_single_script_content_range(&_tree) {
            return Response::error(&req.id, err.code(), err.message("organize_imports"));
        }
    }

    if block.imports.is_empty() {
        log::debug!("organize_imports: {} (no imports)", file);
        return Response::success(
            &req.id,
            serde_json::json!({
                "file": file,
                "groups": [],
                "removed_duplicates": 0,
                "no_op": true,
            }),
        );
    }

    let spans_multiple_regions = if lang == LangId::Go {
        go_import_declarations_span_multiple_code_regions(&source, &_tree)
    } else {
        imports_span_multiple_code_regions(&source, lang, &block.imports)
    };

    if spans_multiple_regions {
        return Response::error_with_data(
            &req.id,
            "multi_region_imports",
            format!(
                "organize_imports: imports in {file} span multiple code regions; refusing to organize because replacing the combined import range would corrupt intervening code"
            ),
            serde_json::json!({ "file": file }),
        );
    }

    if lang == LangId::Go && go_grouped_import_block_has_comments(&_tree) {
        return Response::error_with_data(
            &req.id,
            "unsupported_import_comments",
            format!(
                "organize_imports: Go grouped import block in {file} contains comments; refusing to organize because regrouping would drop or detach them"
            ),
            serde_json::json!({ "file": file }),
        );
    }

    // --- Auto-backup ---
    let backup_id = match edit::auto_backup(
        ctx,
        req.session(),
        &path,
        "organize_imports: pre-edit backup",
        Some(&op_id),
    ) {
        Ok(id) => id,
        Err(e) => {
            return Response::error(&req.id, e.code(), e.to_string());
        }
    };

    // --- Organize: group, sort, dedup ---
    let original_count = block.imports.len();
    let comment_gaps = import_gaps_contain_comments(&source, lang, &block.imports);
    let (mut grouped, mut removed_duplicates) = organize(&block.imports, lang);

    // --- Generate new import block ---
    let grouped_go_range = if matches!(lang, LangId::Go) {
        imports::go_has_grouped_import(&source, &_tree)
    } else {
        None
    };
    let go_import_declarations_range = if matches!(lang, LangId::Go) && grouped_go_range.is_some() {
        imports::go_import_declarations_range(&source, &_tree)
    } else {
        None
    };
    let new_import_text = if matches!(lang, LangId::Go) && grouped_go_range.is_some() {
        generate_go_grouped_block(&grouped)
    } else if comment_gaps {
        let (preserved_grouped, preserved_removed, preserved_text) =
            organize_preserving_comment_gaps(&source, lang, &block.imports);
        grouped = preserved_grouped;
        removed_duplicates = preserved_removed;
        preserved_text
    } else {
        generate_organized_block(&grouped, lang)
    };

    // --- Replace import region ---
    let import_range = match go_import_declarations_range
        .as_ref()
        .or(grouped_go_range.as_ref())
        .or(block.byte_range.as_ref())
    {
        Some(range) => range,
        None => {
            return Response::error(
                &req.id,
                "parse_error",
                format!(
                    "organize_imports: missing import byte range for {} despite parsed imports",
                    file
                ),
            );
        }
    };
    let new_source = format!(
        "{}{}{}",
        &source[..import_range.start],
        new_import_text,
        &source[import_range.end..],
    );

    // --- Write, format, and validate ---
    let mut write_result =
        match edit::write_format_validate(&path, &new_source, &ctx.config(), &req.params) {
            Ok(r) => r,
            Err(e) => {
                return Response::error(&req.id, e.code(), e.to_string());
            }
        };

    if let Ok(final_content) = std::fs::read_to_string(&path) {
        write_result.lsp_outcome = ctx.lsp_post_write(&path, &final_content, &req.params);
    }

    // A rollback means post-write syntax validation failed and the file was
    // restored — imports were NOT reorganized. Report that honestly with an
    // error instead of claiming `organized: true`.
    if write_result.rolled_back {
        return Response::error(
            &req.id,
            "generated_invalid_syntax",
            format!(
                "organize_imports: reorganizing imports in {file} would produce invalid syntax; file left unchanged"
            ),
        );
    }

    log::debug!("organize_imports: {}", file);

    // --- Build response ---
    let groups_info: Vec<serde_json::Value> = grouped
        .iter()
        .map(|(group, imps)| {
            serde_json::json!({
                "name": group.label(),
                "count": imps.len(),
            })
        })
        .collect();

    let _ = original_count; // used for removed_duplicates calculation above

    let mut result = serde_json::json!({
        "file": file,
        "groups": groups_info,
        "removed_duplicates": removed_duplicates,
        "formatted": write_result.formatted,
    });

    if let Some(valid) = write_result.syntax_valid {
        result["syntax_valid"] = serde_json::json!(valid);
    }

    if let Some(ref reason) = write_result.format_skipped_reason {
        result["format_skipped_reason"] = serde_json::json!(reason);
    }

    if write_result.validate_requested {
        result["validation_errors"] = serde_json::json!(write_result.validation_errors);
    }
    if let Some(ref reason) = write_result.validate_skipped_reason {
        result["validate_skipped_reason"] = serde_json::json!(reason);
    }

    if let Some(ref id) = backup_id {
        result["backup_id"] = serde_json::json!(id);
    }

    write_result.append_lsp_diagnostics_to(&mut result);
    write_result.append_reformatted_excerpt_to(&mut result);
    Response::success(&req.id, result)
}

fn go_import_declarations_span_multiple_code_regions(
    source: &str,
    tree: &tree_sitter::Tree,
) -> bool {
    let root = tree.root_node();
    let mut cursor = root.walk();
    let mut ranges: Vec<Range<usize>> = Vec::new();
    if cursor.goto_first_child() {
        loop {
            let node = cursor.node();
            if node.kind() == "import_declaration" {
                ranges.push(node.byte_range());
            }
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }

    ranges.windows(2).any(|pair| {
        let previous = &pair[0];
        let next = &pair[1];
        previous.end > next.start
            || !import_gap_is_trivia(source, LangId::Go, previous.end..next.start)
    })
}

fn go_grouped_import_block_has_comments(tree: &tree_sitter::Tree) -> bool {
    let root = tree.root_node();
    let mut cursor = root.walk();
    if cursor.goto_first_child() {
        loop {
            let node = cursor.node();
            if node.kind() == "import_declaration"
                && go_import_declaration_is_grouped(&node)
                && node_contains_comment(node)
            {
                return true;
            }
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }

    false
}

fn go_import_declaration_is_grouped(node: &tree_sitter::Node<'_>) -> bool {
    let mut cursor = node.walk();
    if cursor.goto_first_child() {
        loop {
            if cursor.node().kind() == "import_spec_list" {
                return true;
            }
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }

    false
}

fn node_contains_comment(node: tree_sitter::Node<'_>) -> bool {
    if node.kind() == "comment" {
        return true;
    }

    let mut cursor = node.walk();
    if cursor.goto_first_child() {
        loop {
            if node_contains_comment(cursor.node()) {
                return true;
            }
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }

    false
}

pub(crate) fn imports_span_multiple_code_regions(
    source: &str,
    lang: LangId,
    imports: &[ImportStatement],
) -> bool {
    imports.windows(2).any(|pair| {
        let previous = &pair[0];
        let next = &pair[1];
        if previous.byte_range.end > next.byte_range.start {
            return true;
        }

        !import_gap_is_trivia(source, lang, previous.byte_range.end..next.byte_range.start)
    })
}

#[derive(Debug, Clone, Copy)]
struct ImportGapTrivia {
    has_comment: bool,
}

fn import_gap_is_trivia(source: &str, lang: LangId, range: Range<usize>) -> bool {
    scan_import_gap(source, lang, range).is_some()
}

fn import_gap_has_comment(source: &str, lang: LangId, range: Range<usize>) -> bool {
    scan_import_gap(source, lang, range)
        .map(|gap| gap.has_comment)
        .unwrap_or(false)
}

fn scan_import_gap(source: &str, lang: LangId, range: Range<usize>) -> Option<ImportGapTrivia> {
    let gap = source.get(range)?;

    let mut offset = 0;
    let mut has_comment = false;
    while offset < gap.len() {
        let rest = &gap[offset..];
        let ch = rest
            .chars()
            .next()
            .expect("offset is within the trivia gap");

        if ch.is_whitespace() {
            offset += ch.len_utf8();
            continue;
        }

        if lang == LangId::Lua && rest.starts_with("--[[") {
            let end = rest.find("]]")?;
            offset += end + 2;
            has_comment = true;
            continue;
        }

        if lang == LangId::Lua && rest.starts_with("--") {
            offset += line_comment_len(rest);
            has_comment = true;
            continue;
        }

        if supports_slash_line_comments(lang) && rest.starts_with("//") {
            offset += line_comment_len(rest);
            has_comment = true;
            continue;
        }

        if supports_block_comments(lang) && rest.starts_with("/*") {
            let end = rest.find("*/")?;
            offset += end + 2;
            has_comment = true;
            continue;
        }

        if supports_hash_line_comments(lang) && rest.starts_with('#') {
            offset += line_comment_len(rest);
            has_comment = true;
            continue;
        }

        return None;
    }

    Some(ImportGapTrivia { has_comment })
}

fn line_comment_len(rest: &str) -> usize {
    rest.find('\n').unwrap_or(rest.len())
}

fn supports_slash_line_comments(lang: LangId) -> bool {
    matches!(
        lang,
        LangId::TypeScript
            | LangId::Tsx
            | LangId::JavaScript
            | LangId::Go
            | LangId::Rust
            | LangId::Solidity
            | LangId::Java
            | LangId::Kotlin
            | LangId::Scala
            | LangId::CSharp
            | LangId::Php
            | LangId::Swift
            | LangId::C
            | LangId::Cpp
            | LangId::Vue
    )
}

fn supports_block_comments(lang: LangId) -> bool {
    supports_slash_line_comments(lang)
}

fn supports_hash_line_comments(lang: LangId) -> bool {
    matches!(
        lang,
        LangId::Python | LangId::Ruby | LangId::Perl | LangId::Php
    )
}

fn import_gaps_contain_comments(source: &str, lang: LangId, imports: &[ImportStatement]) -> bool {
    imports.windows(2).any(|pair| {
        import_gap_has_comment(
            source,
            lang,
            pair[0].byte_range.end..pair[1].byte_range.start,
        )
    })
}

fn organize_preserving_comment_gaps(
    source: &str,
    lang: LangId,
    imports: &[ImportStatement],
) -> (Vec<(ImportGroup, Vec<OrganizedImport>)>, usize, String) {
    let mut grouped = Vec::new();
    let mut removed_duplicates = 0;
    let mut output = String::new();
    let mut segment_start = 0;

    for idx in 0..imports.len() {
        let next_gap = imports.get(idx + 1).map(|next| {
            let range = imports[idx].byte_range.end..next.byte_range.start;
            (import_gap_has_comment(source, lang, range.clone()), range)
        });
        let is_boundary = next_gap
            .as_ref()
            .map(|(has_comment, _)| *has_comment)
            .unwrap_or(true);

        if !is_boundary {
            continue;
        }

        let mut refs: Vec<&ImportStatement> = imports[segment_start..=idx].iter().collect();
        refs.sort_by_key(|imp| imp.byte_range.start);
        let (segment_grouped, segment_removed) = organize_ordered_import_refs(&refs, lang);
        output.push_str(&generate_organized_block(&segment_grouped, lang));
        grouped.extend(segment_grouped);
        removed_duplicates += segment_removed;

        if let Some((true, range)) = next_gap {
            if let Some(gap) = source.get(range) {
                output.push_str(gap);
            }
        }

        segment_start = idx + 1;
    }

    (grouped, removed_duplicates, output)
}

/// Organize imports: group by convention, sort within groups, deduplicate.
/// Returns (grouped imports in order, count of removed duplicates).
fn organize(
    imports: &[ImportStatement],
    lang: LangId,
) -> (Vec<(ImportGroup, Vec<OrganizedImport>)>, usize) {
    let mut refs: Vec<&ImportStatement> = imports.iter().collect();
    refs.sort_by_key(|imp| imp.byte_range.start);
    organize_ordered_import_refs(&refs, lang)
}

fn organize_ordered_import_refs(
    refs: &[&ImportStatement],
    lang: LangId,
) -> (Vec<(ImportGroup, Vec<OrganizedImport>)>, usize) {
    if preserves_side_effect_order(lang)
        && refs.iter().any(|imp| imp.kind == ImportKind::SideEffect)
    {
        return organize_preserving_side_effect_order(refs, lang);
    }

    organize_import_refs(refs, lang)
}

fn organize_import_refs(
    imports: &[&ImportStatement],
    lang: LangId,
) -> (Vec<(ImportGroup, Vec<OrganizedImport>)>, usize) {
    // Group imports
    let mut groups: BTreeMap<ImportGroup, Vec<&ImportStatement>> = BTreeMap::new();
    for imp in imports {
        groups.entry(imp.group).or_default().push(*imp);
    }

    let mut result: Vec<(ImportGroup, Vec<OrganizedImport>)> = Vec::new();
    let mut total_removed = 0;

    for (group, imps) in &groups {
        let (organized, removed) = if matches!(lang, LangId::Rust) {
            organize_rust_group(imps)
        } else if should_preserve_raw_on_organize(lang) {
            organize_raw_preserving_group(imps)
        } else {
            organize_generic_group(imps, lang)
        };
        total_removed += removed;
        if !organized.is_empty() {
            result.push((*group, organized));
        }
    }

    (result, total_removed)
}

fn preserves_side_effect_order(lang: LangId) -> bool {
    matches!(
        lang,
        LangId::TypeScript | LangId::Tsx | LangId::JavaScript | LangId::Vue | LangId::Lua
    )
}

fn organize_preserving_side_effect_order(
    imports: &[&ImportStatement],
    lang: LangId,
) -> (Vec<(ImportGroup, Vec<OrganizedImport>)>, usize) {
    let mut result = Vec::new();
    let mut total_removed = 0;
    let mut segment: Vec<&ImportStatement> = Vec::new();

    for imp in imports {
        if imp.kind == ImportKind::SideEffect {
            let (mut grouped, removed) = organize_import_refs(&segment, lang);
            result.append(&mut grouped);
            total_removed += removed;
            segment.clear();

            result.push((imp.group, vec![organized_from_statement(imp, lang)]));
        } else {
            segment.push(*imp);
        }
    }

    let (mut grouped, removed) = organize_import_refs(&segment, lang);
    result.append(&mut grouped);
    total_removed += removed;

    (result, total_removed)
}

fn organized_from_statement(imp: &ImportStatement, lang: LangId) -> OrganizedImport {
    let mut names = imp.names.clone();
    sort_named_specifiers(&mut names);
    let raw_override = should_preserve_raw_on_organize(lang)
        .then(|| imp.raw_text.trim().to_string())
        .filter(|raw| !raw.is_empty());

    OrganizedImport {
        module_path: imp.module_path.clone(),
        names,
        default_import: imp.default_import.clone(),
        namespace_import: imp.namespace_import.clone(),
        kind: imp.kind,
        raw_override,
    }
}

fn should_preserve_raw_on_organize(lang: LangId) -> bool {
    matches!(
        lang,
        LangId::Scala
            | LangId::Java
            | LangId::CSharp
            | LangId::Php
            | LangId::Kotlin
            | LangId::Solidity
            | LangId::Swift
            | LangId::Ruby
            | LangId::Lua
            | LangId::Perl
            | LangId::C
            | LangId::Cpp
            | LangId::Vue
            // Python: the structured parser captures only the FIRST module of a
            // multi-module `import os, sys` statement, so the generic regenerate
            // path dropped `sys` (silent data loss). Raw-preserve keeps each
            // statement's source text verbatim — no merge of `from X import a` +
            // `from X import b`, but zero corruption. Grouping/sort/dedup still
            // apply per group.
            | LangId::Python
    )
}

/// An organized import ready for code generation.
#[derive(Debug, Clone)]
struct OrganizedImport {
    module_path: String,
    names: Vec<String>,
    default_import: Option<String>,
    namespace_import: Option<String>,
    kind: ImportKind,
    /// When set, the import is rendered verbatim from this string instead of
    /// being regenerated from the structured fields. Used by dialect-sensitive
    /// languages (e.g. Scala) where re-rendering would normalize across
    /// incompatible syntax variants and corrupt the source.
    raw_override: Option<String>,
}

/// Organize a group of non-Rust imports: sort by module path, deduplicate.
fn organize_generic_group(
    imps: &[&ImportStatement],
    _lang: LangId,
) -> (Vec<OrganizedImport>, usize) {
    use std::collections::HashSet;

    let mut seen: HashSet<String> = HashSet::new();
    let mut organized: Vec<OrganizedImport> = Vec::new();
    let mut removed = 0;

    let mut side_effects: Vec<&&ImportStatement> = imps
        .iter()
        .filter(|imp| imp.kind == ImportKind::SideEffect)
        .collect();
    let mut sorted: Vec<&&ImportStatement> = imps
        .iter()
        .filter(|imp| imp.kind != ImportKind::SideEffect)
        .collect();
    sorted.sort_by(|a, b| a.module_path.cmp(&b.module_path));

    // Side-effect imports are evaluation-order sensitive. Keep their original
    // relative source order as a pinned subgroup before value/type imports.
    side_effects.extend(sorted);

    for imp in side_effects {
        // Build dedup key: module_path + kind + sorted names + default + namespace.
        // Namespace imports introduce local bindings, so different aliases are
        // distinct and side-effect imports are not duplicates of namespace
        // imports from the same module.
        let names_key = {
            let mut n = imp.names.clone();
            sort_named_specifiers(&mut n);
            n.join(",")
        };
        let dedup_key = format!(
            "{}|{:?}|{}|{}|{}",
            imp.module_path,
            imp.kind,
            names_key,
            imp.default_import.as_deref().unwrap_or(""),
            imp.namespace_import.as_deref().unwrap_or("")
        );

        if seen.contains(&dedup_key) {
            removed += 1;
            continue;
        }
        seen.insert(dedup_key);

        let mut names = imp.names.clone();
        sort_named_specifiers(&mut names);

        organized.push(OrganizedImport {
            module_path: imp.module_path.clone(),
            names,
            default_import: imp.default_import.clone(),
            namespace_import: imp.namespace_import.clone(),
            kind: imp.kind,
            raw_override: None,
        });
    }

    (organized, removed)
}

fn organize_raw_preserving_group(imps: &[&ImportStatement]) -> (Vec<OrganizedImport>, usize) {
    use std::collections::HashSet;

    let mut seen: HashSet<String> = HashSet::new();
    let mut side_effects: Vec<&ImportStatement> = Vec::new();
    let mut sorted: Vec<&ImportStatement> = Vec::new();
    let mut removed = 0;

    for imp in imps {
        let raw = imp.raw_text.trim();
        if raw.is_empty() {
            continue;
        }

        let key = raw_preserving_dedup_key(imp);
        if !seen.insert(key) {
            removed += 1;
            continue;
        }

        if imp.kind == ImportKind::SideEffect {
            side_effects.push(*imp);
        } else {
            sorted.push(*imp);
        }
    }

    sorted.sort_by(|a, b| a.raw_text.trim().cmp(b.raw_text.trim()));
    side_effects.extend(sorted);

    let organized = side_effects
        .into_iter()
        .map(|imp| OrganizedImport {
            module_path: imp.module_path.clone(),
            names: imp.names.clone(),
            default_import: imp.default_import.clone(),
            namespace_import: imp.namespace_import.clone(),
            kind: imp.kind,
            raw_override: Some(imp.raw_text.trim().to_string()),
        })
        .collect();

    (organized, removed)
}

fn raw_preserving_dedup_key(imp: &ImportStatement) -> String {
    let mut form = imp.form.clone();
    match &mut form {
        ImportForm::Structured { named, .. }
        | ImportForm::Solidity { named, .. }
        | ImportForm::Es { named, .. }
        | ImportForm::Python { named, .. }
        | ImportForm::RustUse { named, .. } => sort_named_specifiers(named),
        ImportForm::Go { .. } | ImportForm::Php { .. } => {}
    }

    format!("{}|{:?}|{:?}", imp.module_path, imp.kind, form)
}

fn sort_named_specifiers(names: &mut [String]) {
    names.sort_by(|a, b| {
        imports::specifier_imported_name(a)
            .cmp(imports::specifier_imported_name(b))
            .then_with(|| a.cmp(b))
    });
}

/// Organize Rust use declarations: sort, deduplicate, and merge common prefixes.
fn organize_rust_group(imps: &[&ImportStatement]) -> (Vec<OrganizedImport>, usize) {
    use std::collections::BTreeMap as BMap;

    // First pass: collect all use paths. For items like `use std::path::Path;`,
    // extract prefix `std::path` and item `Path`. For items like `use serde::{Deserialize, Serialize}`,
    // keep as-is (already a tree).
    #[derive(Debug)]
    struct UsePath {
        /// Full original module_path (e.g. "std::path::Path" or "serde::{Deserialize, Serialize}")
        full_path: String,
        /// Prefix for merging (e.g. "std::path")
        prefix: Option<String>,
        /// Leaf item(s) for merging (e.g. ["Path"])
        items: Vec<String>,
        kind: ImportKind,
        /// Exact visibility text (`pub`, `pub(crate)`, …) or None for private.
        /// Part of the merge key so different visibilities don't merge, and
        /// re-emitted verbatim so `pub(crate) use` isn't widened to `pub use`.
        visibility: Option<String>,
    }

    let mut paths: Vec<UsePath> = Vec::new();
    let mut removed = 0;

    for imp in imps {
        let visibility = imp.default_import.clone();
        let mp = &imp.module_path;

        // Check if this already has a use list (contains '{')
        if mp.contains('{') {
            // Already a tree like "serde::{Deserialize, Serialize}"
            // Extract prefix and items
            if let Some(brace_pos) = mp.find("::{") {
                let prefix = mp[..brace_pos].to_string();
                let items_str = &mp[brace_pos + 3..mp.len() - 1]; // strip ::{ and }
                                                                  // Split on TOP-LEVEL commas only. A naive split(',') corrupts
                                                                  // nested use trees like `hash_map::{Entry, HashMap}, BTreeMap`
                                                                  // into `hash_map::{Entry` / `HashMap}` / `BTreeMap`, which then
                                                                  // sort and regroup into invalid Rust. Brace-aware splitting keeps
                                                                  // each nested subtree intact as one opaque item, so re-emitting
                                                                  // `prefix::{items}` stays syntactically valid.
                let items: Vec<String> = split_top_level_commas(items_str)
                    .into_iter()
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect();
                paths.push(UsePath {
                    full_path: mp.clone(),
                    prefix: Some(prefix),
                    items,
                    kind: imp.kind,
                    visibility: visibility.clone(),
                });
            } else {
                paths.push(UsePath {
                    full_path: mp.clone(),
                    prefix: None,
                    items: vec![],
                    kind: imp.kind,
                    visibility: visibility.clone(),
                });
            }
        } else if let Some(last_sep) = mp.rfind("::") {
            // Simple path like "std::path::Path" → prefix "std::path", item "Path"
            let prefix = mp[..last_sep].to_string();
            let item = mp[last_sep + 2..].to_string();
            paths.push(UsePath {
                full_path: mp.clone(),
                prefix: Some(prefix),
                items: vec![item],
                kind: imp.kind,
                visibility: visibility.clone(),
            });
        } else {
            // Single-segment like "serde" — no prefix to merge on
            paths.push(UsePath {
                full_path: mp.clone(),
                prefix: None,
                items: vec![],
                kind: imp.kind,
                visibility: visibility.clone(),
            });
        }
    }

    // Group by (prefix, kind, visibility) for merging. Visibility is part of the
    // key so `pub(crate) use a::X` never merges with `pub use a::Y`, and it is
    // re-emitted verbatim (was: widened to bare `pub`).
    let mut merge_groups: BMap<(String, u8, Option<String>), Vec<String>> = BMap::new();
    let mut no_prefix: Vec<OrganizedImport> = Vec::new();

    for up in &paths {
        if let Some(ref prefix) = up.prefix {
            let kind_d = match up.kind {
                ImportKind::Value => 0,
                ImportKind::Type => 1,
                ImportKind::SideEffect => 2,
            };
            let key = (prefix.clone(), kind_d, up.visibility.clone());
            let entry = merge_groups.entry(key).or_default();
            for item in &up.items {
                if !entry.contains(item) {
                    entry.push(item.clone());
                } else {
                    removed += 1;
                }
            }
        } else {
            // Check for duplicate
            let already = no_prefix.iter().any(|o| {
                o.module_path == up.full_path
                    && o.kind == up.kind
                    && o.default_import == up.visibility
            });
            if already {
                removed += 1;
            } else {
                no_prefix.push(OrganizedImport {
                    module_path: up.full_path.clone(),
                    names: vec![],
                    default_import: up.visibility.clone(),
                    namespace_import: None,
                    kind: up.kind,
                    raw_override: None,
                });
            }
        }
    }

    // Convert merge groups into OrganizedImport entries
    let mut organized: Vec<OrganizedImport> = Vec::new();

    for ((prefix, kind_d, visibility), mut items) in merge_groups {
        items.sort();
        let kind = match kind_d {
            1 => ImportKind::Type,
            2 => ImportKind::SideEffect,
            _ => ImportKind::Value,
        };

        let module_path = if items.len() == 1 {
            // Single item — no braces needed
            format!("{}::{}", prefix, items[0])
        } else {
            // Multiple items — use tree
            format!("{}::{{{}}}", prefix, items.join(", "))
        };

        organized.push(OrganizedImport {
            module_path,
            names: vec![],
            default_import: visibility,
            namespace_import: None,
            kind,
            raw_override: None,
        });
    }

    // Add no-prefix items and sort everything by module_path
    organized.extend(no_prefix);
    organized.sort_by(|a, b| a.module_path.cmp(&b.module_path));

    // Track how many original imports were merged away
    let final_count = organized.len();
    let original_count = imps.len();
    if original_count > final_count + removed {
        removed = original_count - final_count;
    }

    (organized, removed)
}

/// Split a Rust use-list body on TOP-LEVEL commas only, treating nested
/// `{...}` (and defensively `[...]`/`(...)`) as opaque so commas inside a
/// nested subtree do not split it.
///
/// `"hash_map::{Entry, HashMap}, BTreeMap"` -> `["hash_map::{Entry, HashMap}", "BTreeMap"]`
/// `"Deserialize, Serialize"`               -> `["Deserialize", "Serialize"]`
fn split_top_level_commas(s: &str) -> Vec<String> {
    let mut items = Vec::new();
    let mut depth: i32 = 0;
    let mut start = 0usize;
    for (i, ch) in s.char_indices() {
        match ch {
            '{' | '[' | '(' => depth += 1,
            '}' | ']' | ')' => depth -= 1,
            ',' if depth == 0 => {
                items.push(s[start..i].to_string());
                start = i + 1;
            }
            _ => {}
        }
    }
    items.push(s[start..].to_string());
    items
}

/// Generate the full organized import block text.
fn generate_organized_block(
    grouped: &[(ImportGroup, Vec<OrganizedImport>)],
    lang: LangId,
) -> String {
    let mut output = String::new();
    let mut previous_group: Option<ImportGroup> = None;

    for (group, imps) in grouped {
        let mut lines: Vec<String> = Vec::new();
        for imp in imps {
            let line = generate_organized_line(imp, lang);
            lines.push(line);
        }
        if lines.is_empty() {
            continue;
        }

        if !output.is_empty() {
            if previous_group == Some(*group) {
                output.push('\n');
            } else {
                output.push_str("\n\n");
            }
        }
        output.push_str(&lines.join("\n"));
        previous_group = Some(*group);
    }

    output
}

fn generate_go_grouped_block(grouped: &[(ImportGroup, Vec<OrganizedImport>)]) -> String {
    let mut lines = Vec::new();
    lines.push("import (".to_string());
    for (group_idx, (_, imps)) in grouped.iter().enumerate() {
        if group_idx > 0 {
            lines.push(String::new());
        }
        for imp in imps {
            if let Some(ref alias) = imp.default_import {
                lines.push(format!("\t{} \"{}\"", alias, imp.module_path));
            } else {
                lines.push(format!("\t\"{}\"", imp.module_path));
            }
        }
    }
    lines.push(")".to_string());
    lines.join("\n")
}

/// Generate a single import line from an OrganizedImport.
fn generate_organized_line(imp: &OrganizedImport, lang: LangId) -> String {
    if let Some(ref raw) = imp.raw_override {
        return raw.clone();
    }
    match lang {
        LangId::Rust => {
            // default_import carries the exact visibility text (`pub`,
            // `pub(crate)`, `pub(super)`, …); emit it verbatim so restricted
            // visibilities aren't widened to bare `pub`.
            match imp.default_import.as_deref() {
                Some(vis) if !vis.is_empty() => format!("{} use {};", vis, imp.module_path),
                _ => format!("use {};", imp.module_path),
            }
        }
        LangId::Go => {
            // Go organize: regenerate as standalone imports
            // (organize_imports for Go would need grouped import rewrite — keep simple for now)
            if let Some(ref alias) = imp.default_import {
                format!("import {} \"{}\"", alias, imp.module_path)
            } else {
                format!("import \"{}\"", imp.module_path)
            }
        }
        LangId::TypeScript | LangId::Tsx | LangId::JavaScript
            if imp.names.is_empty()
                && imp.default_import.is_none()
                && imp.namespace_import.is_some() =>
        {
            let namespace = imp.namespace_import.as_deref().unwrap_or_default();
            format!("import * as {} from '{}';", namespace, imp.module_path)
        }
        _ => {
            // TS/JS/TSX/Python — use the standard generator
            imports::generate_import_line_with_namespace(
                lang,
                &imp.module_path,
                &imp.names,
                imp.default_import.as_deref(),
                imp.namespace_import.as_deref(),
                imp.kind == ImportKind::Type,
            )
        }
    }
}
