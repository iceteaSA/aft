use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Instant;

use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tree_sitter::{Node, Parser, Tree};

use super::duplicates_classifier::{is_anonymizable, node_cost, AnonymizeAs};
use crate::cache_freshness;
use crate::inspect::entry_points::TOP_PREVIEW_ITEMS;
use crate::inspect::{
    FileContribution, InspectCategory, InspectJob, InspectResult, InspectScanSuccess,
};
use crate::parser::{detect_language, grammar_for, LangId};

const LOWER_BOUND: u32 = 20;
const MAX_COST: u32 = 7_000;
// Defensive recursion bound for `collect_fragments`. Hand-written code nests
// only tens of levels deep, but minified bundles, generated code, and very long
// operator/promise chains can produce trees thousands of nodes deep. The inspect
// rayon pool uses bounded worker stacks, so unbounded AST recursion here can
// overflow the stack and SIGABRT the entire bridge. Past this depth we stop
// descending and treat the node as an opaque leaf — duplicate detection inside
// such pathological nesting is noise anyway. Kept well under the pool's stack
// budget (see dispatch.rs stack_size).
const MAX_FRAGMENT_DEPTH: u32 = 1_500;
const MAX_GROUP_ITEMS: usize = 100;
const VARIABLE_SENTINEL: &str = "_var";
const FIELD_SENTINEL: &str = "_field";

thread_local! {
    static DUPLICATES_PARSERS: RefCell<HashMap<LangId, Parser>> = RefCell::new(HashMap::new());
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct DuplicateFragment {
    hash: String,
    start_line: u32,
    end_line: u32,
    cost: u32,
}

#[derive(Debug)]
struct FileScan {
    path: PathBuf,
    display_path: String,
    language_skipped: Option<&'static str>,
    freshness: cache_freshness::FileFreshness,
    fragments: Vec<DuplicateFragment>,
}

#[derive(Debug, Clone)]
struct FragmentOccurrence {
    file: Rc<str>,
    start_line: u32,
    end_line: u32,
    cost: u32,
}

#[derive(Debug, Clone, Serialize)]
struct DuplicateGroup {
    files: Vec<String>,
    cost: u32,
    sample_file: String,
    sample_start_line: u32,
    sample_end_line: u32,
}

#[derive(Debug, Clone, Deserialize)]
struct DuplicateContribution {
    file: String,
    fragments: Vec<DuplicateFragment>,
}

#[derive(Debug, Clone, Copy)]
struct NodeDigest {
    hash: blake3::Hash,
    cost: u32,
}

pub fn run_duplicates_scan(job: &InspectJob) -> InspectResult {
    let started = Instant::now();
    let file_scans = job
        .scope_files
        .par_iter()
        .map(|path| scan_file(job, path))
        .collect::<Result<Vec<_>, _>>();

    let file_scans = match file_scans {
        Ok(file_scans) => file_scans,
        Err(message) => return InspectResult::failed(job, message, started.elapsed()),
    };

    let aggregate =
        aggregate_file_scans(&file_scans, skipped_languages_from_file_scans(&file_scans));
    let contributions = file_scans
        .iter()
        .map(file_scan_to_contribution)
        .collect::<Vec<_>>();
    let scanned_files = file_scans
        .iter()
        .map(|scan| scan.path.clone())
        .collect::<Vec<_>>();

    let success = InspectScanSuccess {
        scanned_files,
        contributions,
        aggregate,
    };
    InspectResult::success(job, success, started.elapsed())
}

fn scan_file(job: &InspectJob, path: &Path) -> Result<FileScan, String> {
    let display_path = display_path(&job.project_root, path);
    let freshness = cache_freshness::collect(path)
        .map_err(|error| format!("freshness failed for {}: {error}", path.display()))?;
    let Some(lang) = detect_language(path) else {
        return Ok(FileScan {
            path: path.to_path_buf(),
            display_path,
            language_skipped: Some("unknown"),
            freshness,
            fragments: Vec::new(),
        });
    };

    if !is_supported_language(lang) {
        return Ok(FileScan {
            path: path.to_path_buf(),
            display_path,
            language_skipped: Some(language_name(lang)),
            freshness,
            fragments: Vec::new(),
        });
    }

    let source = fs::read_to_string(path)
        .map_err(|error| format!("read failed for {}: {error}", path.display()))?;
    let tree = parse_source(path, lang, &source)?;
    let mut fragments = Vec::new();
    let mut hash_scratch = Vec::new();
    collect_fragments(
        tree.root_node(),
        &source,
        lang,
        &mut fragments,
        &mut hash_scratch,
        0,
    );
    fragments.sort_by(|left, right| {
        left.start_line
            .cmp(&right.start_line)
            .then(left.end_line.cmp(&right.end_line))
            .then(left.hash.cmp(&right.hash))
    });

    Ok(FileScan {
        path: path.to_path_buf(),
        display_path,
        language_skipped: None,
        freshness,
        fragments,
    })
}

fn parse_source(path: &Path, lang: LangId, source: &str) -> Result<Tree, String> {
    DUPLICATES_PARSERS.with(|parsers| {
        let mut parsers = parsers.borrow_mut();
        if let std::collections::hash_map::Entry::Vacant(entry) = parsers.entry(lang) {
            let grammar = grammar_for(lang);
            let mut parser = Parser::new();
            parser
                .set_language(&grammar)
                .map_err(|error| format!("grammar init failed for {:?}: {error}", lang))?;
            entry.insert(parser);
        }

        parsers
            .get_mut(&lang)
            .expect("parser inserted for language")
            .parse(source, None)
            .ok_or_else(|| format!("tree-sitter parse returned None for {}", path.display()))
    })
}

fn collect_fragments(
    node: Node<'_>,
    source: &str,
    lang: LangId,
    fragments: &mut Vec<DuplicateFragment>,
    hash_scratch: &mut Vec<blake3::Hash>,
    depth: u32,
) -> NodeDigest {
    let mut cost = node_cost(lang, node.kind());
    // Stop descending past MAX_FRAGMENT_DEPTH to avoid overflowing the bounded
    // inspect worker stack on pathologically deep trees (minified bundles,
    // generated code, long chains). Hash the node as an opaque leaf so the
    // parent digest stays deterministic; we simply do not emit fragments from
    // the truncated subtree.
    if depth >= MAX_FRAGMENT_DEPTH {
        let leaf = leaf_hash_text(node, source);
        return NodeDigest {
            hash: hash_node(node.kind(), &[], Some(leaf)),
            cost,
        };
    }
    let child_start = hash_scratch.len();
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.is_extra() {
            continue;
        }
        let child_digest =
            collect_fragments(child, source, lang, fragments, hash_scratch, depth + 1);
        cost = cost.saturating_add(child_digest.cost);
        hash_scratch.push(child_digest.hash);
    }

    let child_hashes = &hash_scratch[child_start..];
    let leaf_text = if child_hashes.is_empty() {
        Some(leaf_hash_text(node, source))
    } else {
        None
    };
    let hash = hash_node(node.kind(), child_hashes, leaf_text);
    hash_scratch.truncate(child_start);

    if node.is_named() && (LOWER_BOUND..=MAX_COST).contains(&cost) {
        fragments.push(DuplicateFragment {
            hash: hash.to_hex().to_string(),
            start_line: node.start_position().row as u32 + 1,
            end_line: node.end_position().row as u32 + 1,
            cost,
        });
    }

    NodeDigest { hash, cost }
}

fn leaf_hash_text<'source>(node: Node<'_>, source: &'source str) -> &'source str {
    match anonymize_leaf(node) {
        AnonymizeAs::Variable => VARIABLE_SENTINEL,
        AnonymizeAs::Field => FIELD_SENTINEL,
        // Anonymous tree-sitter leaves are usually fixed tokens whose kind is
        // their source text. Reuse the kind only after comparing the source
        // bytes, so generated/external zero-width tokens keep the old hash.
        AnonymizeAs::None if !node.is_named() => {
            let source_text = source
                .as_bytes()
                .get(node.start_byte()..node.end_byte())
                .unwrap_or_default();
            if source_text == node.kind().as_bytes() {
                node.kind()
            } else {
                node.utf8_text(source.as_bytes()).unwrap_or("")
            }
        }
        AnonymizeAs::None => node.utf8_text(source.as_bytes()).unwrap_or(""),
    }
}

fn anonymize_leaf(node: Node<'_>) -> AnonymizeAs {
    // Most leaves (punctuation, keywords, literals, type identifiers) are not
    // anonymizable. Avoid parent/field callee checks unless the kind can be
    // anonymized in the first place.
    let anonymize_as = is_anonymizable(node.kind());
    if anonymize_as == AnonymizeAs::None || node.kind() == "type_identifier" {
        return AnonymizeAs::None;
    }
    if is_method_or_callee_name(node) {
        AnonymizeAs::None
    } else {
        anonymize_as
    }
}

fn is_method_or_callee_name(node: Node<'_>) -> bool {
    if is_direct_callee(node) || is_member_call_name(node) {
        return true;
    }

    let Some(parent) = node.parent() else {
        return false;
    };
    matches!(
        parent.kind(),
        "method_definition" | "method_declaration" | "method_signature" | "method_invocation"
    ) && is_name_field(parent, node)
}

fn is_direct_callee(node: Node<'_>) -> bool {
    let Some(parent) = node.parent() else {
        return false;
    };
    matches!(parent.kind(), "call_expression" | "call" | "function_call")
        && is_field_node(parent, "function", node)
}

fn is_member_call_name(node: Node<'_>) -> bool {
    let Some(parent) = node.parent() else {
        return false;
    };
    if !matches!(
        parent.kind(),
        "member_expression" | "field_expression" | "selector_expression" | "attribute"
    ) || !(is_field_node(parent, "property", node)
        || is_field_node(parent, "field", node)
        || is_field_node(parent, "attribute", node))
    {
        return false;
    }

    let Some(grandparent) = parent.parent() else {
        return false;
    };
    matches!(
        grandparent.kind(),
        "call_expression" | "call" | "function_call"
    ) && is_field_node(grandparent, "function", parent)
}

fn is_name_field(parent: Node<'_>, node: Node<'_>) -> bool {
    is_field_node(parent, "name", node)
        || is_field_node(parent, "property", node)
        || is_field_node(parent, "field", node)
}

fn is_field_node(parent: Node<'_>, field_name: &str, node: Node<'_>) -> bool {
    parent
        .child_by_field_name(field_name)
        .is_some_and(|field_node| same_node(field_node, node))
}

fn same_node(left: Node<'_>, right: Node<'_>) -> bool {
    left.kind() == right.kind()
        && left.start_byte() == right.start_byte()
        && left.end_byte() == right.end_byte()
}

fn hash_node(kind: &str, child_hashes: &[blake3::Hash], leaf_text: Option<&str>) -> blake3::Hash {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"aft-duplicates-v1");
    hash_bytes(&mut hasher, kind.as_bytes());
    hasher.update(&(child_hashes.len() as u64).to_le_bytes());
    for child_hash in child_hashes {
        hasher.update(child_hash.as_bytes());
    }
    match leaf_text {
        Some(text) => {
            hasher.update(&[1]);
            hash_bytes(&mut hasher, text.as_bytes());
        }
        None => {
            hasher.update(&[0]);
        }
    };
    hasher.finalize()
}

fn hash_bytes(hasher: &mut blake3::Hasher, bytes: &[u8]) {
    hasher.update(&(bytes.len() as u64).to_le_bytes());
    hasher.update(bytes);
}

fn file_scan_to_contribution(scan: &FileScan) -> FileContribution {
    FileContribution::new(
        InspectCategory::Duplicates,
        scan.path.clone(),
        scan.freshness,
        json!({
            "file": scan.display_path,
            "fragments": scan.fragments,
        }),
    )
}

#[cfg(test)]
pub(crate) fn aggregate_duplicate_contributions(
    contributions: &[FileContribution],
    languages_skipped: Vec<String>,
) -> serde_json::Value {
    aggregate_duplicate_contributions_with_limit(
        contributions,
        languages_skipped,
        Some(MAX_GROUP_ITEMS),
    )
}

pub(crate) fn aggregate_duplicate_contributions_with_limit(
    contributions: &[FileContribution],
    languages_skipped: Vec<String>,
    drill_down_limit: Option<usize>,
) -> serde_json::Value {
    let parsed = contributions
        .iter()
        .filter_map(|contribution| {
            serde_json::from_value::<DuplicateContribution>(contribution.contribution.clone()).ok()
        })
        .collect::<Vec<_>>();
    let mut by_hash = BTreeMap::<String, Vec<FragmentOccurrence>>::new();

    for scan in &parsed {
        let file = Rc::<str>::from(scan.file.as_str());
        for fragment in &scan.fragments {
            push_occurrence(
                &mut by_hash,
                fragment.hash.as_str(),
                FragmentOccurrence {
                    file: Rc::clone(&file),
                    start_line: fragment.start_line,
                    end_line: fragment.end_line,
                    cost: fragment.cost,
                },
            );
        }
    }

    aggregate_duplicate_occurrences(by_hash, parsed.len(), languages_skipped, drill_down_limit)
}

fn aggregate_file_scans(
    file_scans: &[FileScan],
    languages_skipped: Vec<String>,
) -> serde_json::Value {
    aggregate_file_scans_with_limit(file_scans, languages_skipped, Some(MAX_GROUP_ITEMS))
}

fn aggregate_file_scans_with_limit(
    file_scans: &[FileScan],
    languages_skipped: Vec<String>,
    drill_down_limit: Option<usize>,
) -> serde_json::Value {
    let mut by_hash = BTreeMap::<String, Vec<FragmentOccurrence>>::new();

    for scan in file_scans {
        let file = Rc::<str>::from(scan.display_path.as_str());
        for fragment in &scan.fragments {
            push_occurrence(
                &mut by_hash,
                fragment.hash.as_str(),
                FragmentOccurrence {
                    file: Rc::clone(&file),
                    start_line: fragment.start_line,
                    end_line: fragment.end_line,
                    cost: fragment.cost,
                },
            );
        }
    }

    aggregate_duplicate_occurrences(
        by_hash,
        file_scans.len(),
        languages_skipped,
        drill_down_limit,
    )
}

fn push_occurrence(
    by_hash: &mut BTreeMap<String, Vec<FragmentOccurrence>>,
    hash: &str,
    occurrence: FragmentOccurrence,
) {
    if let Some(occurrences) = by_hash.get_mut(hash) {
        occurrences.push(occurrence);
    } else {
        by_hash.insert(hash.to_string(), vec![occurrence]);
    }
}

fn aggregate_duplicate_occurrences(
    by_hash: BTreeMap<String, Vec<FragmentOccurrence>>,
    scanned_files: usize,
    languages_skipped: Vec<String>,
    drill_down_limit: Option<usize>,
) -> serde_json::Value {
    // Collapse nested/overlapping duplicate fragments. tree-sitter records a
    // duplicate hash for EVERY named subtree, so one duplicated block emits the
    // outer node PLUS every nested descendant as a separate group — inflating
    // the count by an order of magnitude (e.g. one shared file → dozens of
    // overlapping groups). A duplicate fragment whose every occurrence is
    // spatially enclosed by a LARGER duplicate fragment in the same file is
    // redundant: its duplication is already implied by the enclosing block.
    // Keep only "maximal" groups — those with at least one occurrence that is
    // NOT enclosed by any other duplicate fragment. A group that is sometimes
    // standalone (a shared idiom duplicated widely on its own AND nested inside
    // a larger dup elsewhere) is preserved, because that standalone occurrence
    // is unenclosed.
    let surfaced_hashes = surfaced_duplicate_hashes(&by_hash);

    let mut groups = by_hash
        .iter()
        .filter(|(hash, occurrences)| {
            occurrences.len() >= 2 && surfaced_hashes.contains(hash.as_str())
        })
        .map(|(_, occurrences)| {
            let mut occurrences = occurrences.clone();
            occurrences.sort_by(|left, right| {
                left.file
                    .cmp(&right.file)
                    .then(left.start_line.cmp(&right.start_line))
                    .then(left.end_line.cmp(&right.end_line))
            });
            occurrences_to_group(&occurrences)
        })
        .collect::<Vec<_>>();

    groups.sort_by(|left, right| {
        right
            .cost
            .cmp(&left.cost)
            .then(left.sample_file.cmp(&right.sample_file))
            .then(left.sample_start_line.cmp(&right.sample_start_line))
    });

    let groups_count = groups.len();
    let drill_down_capped = drill_down_limit.is_some_and(|limit| groups_count > limit);
    let items = match drill_down_limit {
        Some(limit) => groups.into_iter().take(limit).collect::<Vec<_>>(),
        None => groups,
    };

    // Cost-ranked top-N preview for the summary view (groups are already sorted
    // by cost). One `aft_inspect` call surfaces the biggest duplications without
    // a drill-down.
    let top = items
        .iter()
        .take(TOP_PREVIEW_ITEMS)
        .map(|group| {
            json!({
                "files": group.files,
                "cost": group.cost,
            })
        })
        .collect::<Vec<_>>();

    json!({
        "count": groups_count,
        "total_groups": groups_count,
        "groups_count": groups_count,
        "items": items,
        "top": top,
        "drill_down_capped": drill_down_capped,
        "scanned_files": scanned_files,
        "languages_skipped": languages_skipped,
    })
}

/// Hashes of duplicate fragments that are "maximal" — i.e. have at least one
/// occurrence NOT spatially enclosed by another duplicate fragment in the same
/// file. A fragment whose every occurrence is enclosed by a larger duplicate
/// fragment is collapsed away (its duplication is implied by the enclosing
/// block). tree-sitter node spans nest properly, so within a file a stack of
/// currently-open intervals yields each fragment's enclosing ancestors.
fn surfaced_duplicate_hashes(
    by_hash: &BTreeMap<String, Vec<FragmentOccurrence>>,
) -> BTreeSet<&str> {
    // Per-file intervals, restricted to fragments whose hash is actually
    // duplicated (>= 2 occurrences project-wide). Only such fragments can
    // subsume — and only such fragments are eligible to surface.
    let mut by_file: BTreeMap<&str, Vec<(u32, u32, &str)>> = BTreeMap::new();
    for (hash, occurrences) in by_hash {
        if occurrences.len() < 2 {
            continue;
        }
        for occ in occurrences {
            by_file.entry(occ.file.as_ref()).or_default().push((
                occ.start_line,
                occ.end_line,
                hash.as_str(),
            ));
        }
    }

    let mut surfaced = BTreeSet::new();
    for intervals in by_file.values_mut() {
        // Enclosing-first: a parent (smaller start, then larger end) precedes
        // its descendants. Equal spans (a wrapper node and its sole child) sort
        // adjacently; the first surfaces and the rest are treated as enclosed,
        // collapsing the pair to one region.
        intervals.sort_by(|a, b| a.0.cmp(&b.0).then(b.1.cmp(&a.1)));
        let mut open: Vec<(u32, u32)> = Vec::new();
        for &(start, end, hash) in intervals.iter() {
            while let Some(&(_, top_end)) = open.last() {
                if top_end < start {
                    open.pop();
                } else {
                    break;
                }
            }
            // Unenclosed by any larger duplicate fragment in this file → this
            // occurrence is maximal here, so the group surfaces.
            if open.is_empty() {
                surfaced.insert(hash);
            }
            open.push((start, end));
        }
    }

    surfaced
}

fn skipped_languages_from_file_scans(file_scans: &[FileScan]) -> Vec<String> {
    file_scans
        .iter()
        .filter_map(|scan| scan.language_skipped)
        .map(ToString::to_string)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn occurrences_to_group(occurrences: &[FragmentOccurrence]) -> DuplicateGroup {
    let sample = &occurrences[0];
    DuplicateGroup {
        files: occurrences
            .iter()
            .map(|occurrence| {
                format!(
                    "{}:{}-{}",
                    occurrence.file.as_ref(),
                    occurrence.start_line,
                    occurrence.end_line
                )
            })
            .collect(),
        cost: sample.cost,
        sample_file: sample.file.to_string(),
        sample_start_line: sample.start_line,
        sample_end_line: sample.end_line,
    }
}

fn display_path(project_root: &Path, path: &Path) -> String {
    path.strip_prefix(project_root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

fn is_supported_language(lang: LangId) -> bool {
    !matches!(
        lang,
        LangId::Bash
            | LangId::Html
            | LangId::Json
            | LangId::Scala
            | LangId::Solidity
            | LangId::Scss
            | LangId::Vue
            | LangId::Markdown
            | LangId::Java
            | LangId::Ruby
            | LangId::Kotlin
            | LangId::Swift
            | LangId::Php
            | LangId::Lua
            | LangId::Perl
            | LangId::Pascal
    )
}
fn language_name(lang: LangId) -> &'static str {
    match lang {
        LangId::TypeScript => "typescript",
        LangId::Tsx => "tsx",
        LangId::JavaScript => "javascript",
        LangId::Python => "python",
        LangId::Rust => "rust",
        LangId::Go => "go",
        LangId::C => "c",
        LangId::Cpp => "cpp",
        LangId::Zig => "zig",
        LangId::CSharp => "csharp",
        LangId::Bash => "bash",
        LangId::Html => "html",
        LangId::Markdown => "markdown",
        LangId::Yaml => "yaml",
        LangId::Solidity => "solidity",
        LangId::Scss => "scss",
        LangId::Vue => "vue",
        LangId::Json => "json",
        LangId::Scala => "scala",
        LangId::Java => "java",
        LangId::Ruby => "ruby",
        LangId::Kotlin => "kotlin",
        LangId::Swift => "swift",
        LangId::Php => "php",
        LangId::Lua => "lua",
        LangId::Perl => "perl",
        LangId::Pascal => "pascal",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache_freshness::FileFreshness;
    use serde_json::json;

    fn freshness() -> FileFreshness {
        FileFreshness {
            mtime: std::time::SystemTime::UNIX_EPOCH,
            size: 0,
            content_hash: blake3::hash(b""),
        }
    }

    /// Build a duplicates FileContribution from (start, end, cost, hash) fragments.
    fn contribution(file: &str, fragments: &[(u32, u32, u32, &str)]) -> FileContribution {
        let frag_json = fragments
            .iter()
            .map(|(start, end, cost, hash)| {
                json!({
                    "hash": hash,
                    "start_line": start,
                    "end_line": end,
                    "cost": cost,
                })
            })
            .collect::<Vec<_>>();
        FileContribution::new(
            InspectCategory::Duplicates,
            file,
            freshness(),
            json!({ "file": file, "fragments": frag_json }),
        )
    }

    fn group_count(aggregate: &serde_json::Value) -> u64 {
        aggregate["count"].as_u64().unwrap()
    }

    #[test]
    fn nested_fragments_of_one_duplicated_block_collapse_to_one_group() {
        // A duplicated block (hash "block", lines 1-20) whose nested child
        // (hash "child", lines 3-8) is ALSO duplicated across the same files.
        // Without collapse this is 2 groups; the child is enclosed by the block
        // in every occurrence, so it must collapse to 1 maximal group.
        let contributions = vec![
            contribution("src/a.ts", &[(1, 20, 1000, "block"), (3, 8, 400, "child")]),
            contribution("src/b.ts", &[(1, 20, 1000, "block"), (3, 8, 400, "child")]),
        ];

        let aggregate = aggregate_duplicate_contributions(&contributions, Vec::new());
        assert_eq!(group_count(&aggregate), 1, "aggregate: {aggregate:#}");
        assert_eq!(aggregate["items"][0]["cost"], 1000);
    }

    #[test]
    fn widely_duplicated_idiom_survives_even_when_nested_elsewhere() {
        // "idiom" is nested inside "block" in a.ts/b.ts, BUT also appears
        // standalone (unenclosed) in c.ts/d.ts. Its standalone occurrences are
        // maximal, so the idiom group must be preserved alongside the block.
        let contributions = vec![
            contribution("src/a.ts", &[(1, 20, 1000, "block"), (3, 8, 400, "idiom")]),
            contribution("src/b.ts", &[(1, 20, 1000, "block"), (3, 8, 400, "idiom")]),
            contribution("src/c.ts", &[(40, 45, 400, "idiom")]),
            contribution("src/d.ts", &[(70, 75, 400, "idiom")]),
        ];

        let aggregate = aggregate_duplicate_contributions(&contributions, Vec::new());
        assert_eq!(group_count(&aggregate), 2, "aggregate: {aggregate:#}");
    }

    #[test]
    fn disjoint_sibling_duplications_are_both_kept() {
        // Two unrelated duplicated blocks, side by side (no enclosure). Both
        // are maximal → both kept.
        let contributions = vec![
            contribution("src/a.ts", &[(1, 10, 300, "alpha"), (20, 30, 300, "beta")]),
            contribution("src/b.ts", &[(1, 10, 300, "alpha"), (20, 30, 300, "beta")]),
        ];

        let aggregate = aggregate_duplicate_contributions(&contributions, Vec::new());
        assert_eq!(group_count(&aggregate), 2, "aggregate: {aggregate:#}");
    }

    #[test]
    fn non_duplicated_fragment_does_not_subsume() {
        // The enclosing "block" appears only once (NOT duplicated), so it must
        // not subsume the genuinely-duplicated nested "child". The child is the
        // only real duplication and must surface.
        let contributions = vec![
            contribution(
                "src/a.ts",
                &[(1, 20, 1000, "uniqueA"), (3, 8, 400, "child")],
            ),
            contribution(
                "src/b.ts",
                &[(1, 20, 1000, "uniqueB"), (3, 8, 400, "child")],
            ),
        ];

        let aggregate = aggregate_duplicate_contributions(&contributions, Vec::new());
        assert_eq!(group_count(&aggregate), 1, "aggregate: {aggregate:#}");
        // The surfaced group is the child, not either unique block.
        assert_eq!(aggregate["items"][0]["cost"], 400);
    }

    #[test]
    fn deeply_nested_tree_does_not_overflow_stack() {
        // Regression for the inspect-thread stack overflow / SIGABRT: a
        // pathologically deep expression (here ~6000 nested parentheses, far
        // past MAX_FRAGMENT_DEPTH) must not recurse unbounded. Before the depth
        // guard this overflowed the bounded inspect worker stack and aborted the
        // whole process. We run it on the current (main) thread, which has a
        // larger stack than rayon workers, but the guard is what makes it safe
        // on the bounded pool. Reaching the assert at all proves no overflow.
        let depth = 6_000usize;
        let mut source = String::with_capacity(depth * 2 + 32);
        source.push_str("const x = ");
        for _ in 0..depth {
            source.push('(');
        }
        source.push('1');
        for _ in 0..depth {
            source.push(')');
        }
        source.push_str(";\n");

        let lang = LangId::TypeScript;
        let tree = parse_source(Path::new("deep.ts"), lang, &source).expect("parse deep source");
        let mut fragments = Vec::new();
        // Must return instead of overflowing. Run in a bounded-stack thread to
        // mirror the inspect pool and prove the guard (not just a big stack)
        // is doing the work.
        std::thread::Builder::new()
            .stack_size(2 * 1024 * 1024)
            .spawn(move || {
                let mut hash_scratch = Vec::new();
                collect_fragments(
                    tree.root_node(),
                    &source,
                    lang,
                    &mut fragments,
                    &mut hash_scratch,
                    0,
                );
            })
            .expect("spawn bounded-stack worker")
            .join()
            .expect("deep-tree scan must not overflow the bounded stack");
    }
}
