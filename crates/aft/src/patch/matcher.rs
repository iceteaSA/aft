//! Patch-specific line-sequence matcher ported from the TypeScript apply_patch engine.
//!
//! This module intentionally does not reuse `fuzzy_match`: edit matching works in byte
//! ranges, while apply_patch needs line indexes, EOF anchoring, and unique-only reflow.

use std::collections::{HashMap, HashSet};

/// Allow candidate reflow windows to differ by up to eight non-whitespace characters before exact normalized comparison.
pub const REFLOW_NON_WS_TOLERANCE: usize = 8;
/// Avoid spending diagnostic work or memory on files too large to render usefully in an error.
pub const NEAREST_MISS_MAX_FILE_BYTES: usize = 2 * 1024 * 1024;

const NEAREST_MISS_ANCHOR_COUNT: usize = 3;
const NEAREST_MISS_MAX_CANDIDATES: usize = 512;
const NEAREST_MISS_MAX_POSITIONS_PER_ANCHOR: usize = 192;
const NEAREST_MISS_MAX_LINE_COMPARISONS: usize = 100_000;
const NEAREST_MISS_MAX_FUZZY_CHARS: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NearestMiss {
    /// Zero-based first line of the candidate window.
    pub start: usize,
    /// Zero-based exclusive end of the available candidate window.
    pub end: usize,
    pub matched_lines: usize,
    /// Zero-based wanted-line offset of the first mismatch.
    pub first_divergence: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NearestMissSearch {
    Found(NearestMiss),
    NoSimilarRegion,
    SkippedLargeFile,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatchTier {
    Exact,
    Rstrip,
    Trim,
    Indent,
    Unicode,
    Reflow,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SequenceMatch {
    pub found: usize,
    pub tier: MatchTier,
    pub line_count: usize,
}

/// Convert smart quotes, dash variants, ellipsis, and NBSP to their ASCII forms; mirrors `patch-parser.ts:207-214`.
pub fn normalize_unicode(input: &str) -> String {
    let mut normalized = String::with_capacity(input.len());
    for ch in input.chars() {
        match ch {
            '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}' => normalized.push('\''),
            '\u{201C}' | '\u{201D}' | '\u{201E}' | '\u{201F}' => normalized.push('"'),
            '\u{2010}' | '\u{2011}' | '\u{2012}' | '\u{2013}' | '\u{2014}' | '\u{2015}' => {
                normalized.push('-');
            }
            '\u{2026}' => normalized.push_str("..."),
            '\u{00A0}' => normalized.push(' '),
            _ => normalized.push(ch),
        }
    }
    normalized
}

/// Replace a leading run of tabs and spaces with the same number of plain spaces; mirrors `patch-parser.ts:227-229`.
pub fn normalize_indent(input: &str) -> String {
    let mut leading_chars = 0;
    let mut leading_bytes = 0;

    for ch in input.chars() {
        if ch != '\t' && ch != ' ' {
            break;
        }
        leading_chars += 1;
        leading_bytes += ch.len_utf8();
    }

    if leading_chars == 0 {
        return input.to_owned();
    }

    let mut normalized = String::with_capacity(input.len());
    normalized.push_str(&" ".repeat(leading_chars));
    normalized.push_str(&input[leading_bytes..]);
    normalized
}

/// Collapse every Unicode whitespace run to one space and trim the ends; mirrors `patch-parser.ts:233-235`.
pub fn normalize_reflow_whitespace(input: &str) -> String {
    let mut collapsed = String::with_capacity(input.len());
    let mut in_whitespace = false;

    for ch in input.chars() {
        if ch.is_whitespace() {
            if !in_whitespace {
                collapsed.push(' ');
                in_whitespace = true;
            }
        } else {
            collapsed.push(ch);
            in_whitespace = false;
        }
    }

    collapsed.trim().to_owned()
}

/// Remove every Unicode whitespace character; mirrors `patch-parser.ts:237-239`.
pub fn strip_reflow_whitespace(input: &str) -> String {
    input.chars().filter(|ch| !ch.is_whitespace()).collect()
}

/// Return true when a line has any non-whitespace content; mirrors `patch-parser.ts:241-243`.
pub fn has_reflow_content(input: &str) -> bool {
    input.chars().any(|ch| !ch.is_whitespace())
}

fn matches_at<F>(lines: &[&str], pattern: &[&str], start: usize, compare: &F) -> bool
where
    F: Fn(&str, &str) -> bool,
{
    pattern
        .iter()
        .enumerate()
        .all(|(offset, expected)| compare(lines[start + offset], expected))
}

/// Search for a full pattern with a caller-supplied comparator, optionally anchored at EOF; mirrors `patch-parser.ts:247-281`.
pub fn try_match<F>(
    lines: &[&str],
    pattern: &[&str],
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
        if from_end >= start_index && matches_at(lines, pattern, from_end, &compare) {
            return Some(from_end);
        }
        return None;
    }

    let last_start = lines.len() - pattern.len();
    if start_index > last_start {
        return None;
    }

    (start_index..=last_start).find(|&start| matches_at(lines, pattern, start, &compare))
}

fn non_whitespace_unit_count(input: &str) -> usize {
    // TypeScript uses UTF-16 code units for `.length`; Rust has no direct equivalent on `str`.
    // The length check only bounds candidate windows before exact string equality, so counting
    // Unicode scalar values keeps non-ASCII text from being over-weighted by UTF-8 byte length.
    strip_reflow_whitespace(input).chars().count()
}

/// Find one unique whitespace-reflowed window, returning `(found_line, line_count)`; mirrors `patch-parser.ts:310-351`.
pub fn find_reflow_match(
    lines: &[&str],
    pattern: &[&str],
    start_index: usize,
) -> Option<(usize, usize)> {
    let needle_text = pattern.join("\n");
    let normalized_needle = normalize_reflow_whitespace(&needle_text);
    let needle_non_whitespace = strip_reflow_whitespace(&needle_text);
    if normalized_needle.is_empty() || needle_non_whitespace.is_empty() {
        return None;
    }

    let needle_non_whitespace_len = needle_non_whitespace.chars().count();
    let min_non_whitespace = needle_non_whitespace_len.saturating_sub(REFLOW_NON_WS_TOLERANCE);
    let max_non_whitespace = needle_non_whitespace_len + REFLOW_NON_WS_TOLERANCE;
    let mut matches = Vec::new();
    let mut seen = HashSet::new();

    for start in start_index..lines.len() {
        if !has_reflow_content(lines[start]) {
            continue;
        }

        let mut window_non_whitespace_len = 0;
        for end in (start + 1)..=lines.len() {
            let line = lines[end - 1];
            window_non_whitespace_len += non_whitespace_unit_count(line);

            if window_non_whitespace_len > max_non_whitespace {
                break;
            }
            if window_non_whitespace_len < min_non_whitespace {
                continue;
            }
            if !has_reflow_content(line) {
                continue;
            }

            let window_text = lines[start..end].join("\n");
            let window_non_whitespace = strip_reflow_whitespace(&window_text);
            if window_non_whitespace != needle_non_whitespace {
                continue;
            }
            if normalize_reflow_whitespace(&window_text) != normalized_needle {
                continue;
            }

            if seen.insert((start, end)) {
                matches.push((start, end - start));
            }
        }
    }

    if matches.len() == 1 {
        Some(matches[0])
    } else {
        None
    }
}

/// Run the first-hit-wins Exact/Rstrip/Trim/Indent/Unicode/Reflow ladder; mirrors `patch-parser.ts:353-399`.
pub fn seek_sequence_tiered(
    lines: &[&str],
    pattern: &[&str],
    start_index: usize,
    eof: bool,
) -> Option<SequenceMatch> {
    if pattern.is_empty() {
        return None;
    }

    if let Some(found) = try_match(lines, pattern, start_index, |a, b| a == b, eof) {
        return Some(SequenceMatch {
            found,
            tier: MatchTier::Exact,
            line_count: pattern.len(),
        });
    }

    if let Some(found) = try_match(
        lines,
        pattern,
        start_index,
        |a, b| a.trim_end() == b.trim_end(),
        eof,
    ) {
        return Some(SequenceMatch {
            found,
            tier: MatchTier::Rstrip,
            line_count: pattern.len(),
        });
    }

    if let Some(found) = try_match(
        lines,
        pattern,
        start_index,
        |a, b| a.trim() == b.trim(),
        eof,
    ) {
        return Some(SequenceMatch {
            found,
            tier: MatchTier::Trim,
            line_count: pattern.len(),
        });
    }

    if let Some(found) = try_match(
        lines,
        pattern,
        start_index,
        |a, b| normalize_indent(a).trim_end() == normalize_indent(b).trim_end(),
        eof,
    ) {
        return Some(SequenceMatch {
            found,
            tier: MatchTier::Indent,
            line_count: pattern.len(),
        });
    }

    if let Some(found) = try_match(
        lines,
        pattern,
        start_index,
        |a, b| normalize_unicode(a.trim()) == normalize_unicode(b.trim()),
        eof,
    ) {
        return Some(SequenceMatch {
            found,
            tier: MatchTier::Unicode,
            line_count: pattern.len(),
        });
    }

    if eof {
        return None;
    }

    find_reflow_match(lines, pattern, start_index).map(|(found, line_count)| SequenceMatch {
        found,
        tier: MatchTier::Reflow,
        line_count,
    })
}

fn add_sampled_candidates(
    candidates: &mut HashSet<usize>,
    positions: &[usize],
    wanted_offset: usize,
    candidate_limit: usize,
) {
    let remaining = candidate_limit.saturating_sub(candidates.len());
    let sample_count = positions
        .len()
        .min(NEAREST_MISS_MAX_POSITIONS_PER_ANCHOR)
        .min(remaining);
    if sample_count == 0 {
        return;
    }

    for sample in 0..sample_count {
        let position_index = if sample_count == 1 {
            0
        } else {
            sample * (positions.len() - 1) / (sample_count - 1)
        };
        let file_position = positions[position_index];
        if let Some(start) = file_position.checked_sub(wanted_offset) {
            candidates.insert(start);
        }
    }
}

fn score_nearest_miss(lines: &[&str], pattern: &[&str], start: usize) -> NearestMiss {
    let end = (start + pattern.len()).min(lines.len());
    let matched_lines = pattern
        .iter()
        .enumerate()
        .filter(|(offset, expected)| {
            lines
                .get(start + offset)
                .is_some_and(|actual| actual.trim() == expected.trim())
        })
        .count();
    let first_divergence = pattern
        .iter()
        .enumerate()
        .find(|(offset, expected)| {
            lines
                .get(start + offset)
                .is_none_or(|actual| actual.trim() != expected.trim())
        })
        .map_or(pattern.len(), |(offset, _)| offset);

    NearestMiss {
        start,
        end,
        matched_lines,
        first_divergence,
    }
}

fn is_better_nearest_miss(candidate: NearestMiss, current: NearestMiss) -> bool {
    candidate.matched_lines > current.matched_lines
        || (candidate.matched_lines == current.matched_lines
            && (candidate.first_divergence > current.first_divergence
                || (candidate.first_divergence == current.first_divergence
                    && candidate.start < current.start)))
}

fn best_scored_candidate(
    lines: &[&str],
    pattern: &[&str],
    candidates: HashSet<usize>,
) -> Option<NearestMiss> {
    candidates
        .into_iter()
        .filter(|start| *start < lines.len())
        .map(|start| score_nearest_miss(lines, pattern, start))
        .fold(None, |best, candidate| match best {
            Some(current) if !is_better_nearest_miss(candidate, current) => Some(current),
            _ => Some(candidate),
        })
}

fn normalize_fuzzy_line(line: &str) -> String {
    normalize_unicode(&normalize_reflow_whitespace(line))
}

fn normalized_prefix_score(normalized_wanted: &str, actual: &str) -> Option<usize> {
    let actual = normalize_fuzzy_line(actual);
    let wanted_len = normalized_wanted
        .chars()
        .take(NEAREST_MISS_MAX_FUZZY_CHARS)
        .count();
    if wanted_len < 4 {
        return None;
    }

    let common = normalized_wanted
        .chars()
        .zip(actual.chars())
        .take(NEAREST_MISS_MAX_FUZZY_CHARS)
        .take_while(|(wanted_char, actual_char)| wanted_char == actual_char)
        .count();
    (common >= 4 && common * 2 >= wanted_len).then_some(common)
}

fn rarest_wanted_line<'a>(pattern: &'a [&'a str]) -> Option<(usize, &'a str)> {
    let mut frequencies: HashMap<&str, usize> = HashMap::new();
    for line in pattern
        .iter()
        .map(|line| line.trim())
        .filter(|line| !line.is_empty())
    {
        *frequencies.entry(line).or_default() += 1;
    }

    pattern
        .iter()
        .enumerate()
        .map(|(offset, line)| (offset, line.trim()))
        .filter(|(_, line)| !line.is_empty())
        .min_by_key(|(offset, line)| {
            (
                frequencies.get(line).copied().unwrap_or(usize::MAX),
                std::cmp::Reverse(line.chars().count()),
                *offset,
            )
        })
}

/// Find a bounded best-effort candidate after every accepted match tier has failed.
pub fn find_nearest_miss(
    lines: &[&str],
    pattern: &[&str],
    file_size_bytes: usize,
) -> NearestMissSearch {
    if file_size_bytes > NEAREST_MISS_MAX_FILE_BYTES {
        return NearestMissSearch::SkippedLargeFile;
    }
    if lines.is_empty() || pattern.is_empty() {
        return NearestMissSearch::NoSimilarRegion;
    }

    let mut line_index: HashMap<&str, Vec<usize>> = HashMap::new();
    for (position, line) in lines.iter().enumerate() {
        let trimmed = line.trim();
        if !trimmed.is_empty() {
            line_index.entry(trimmed).or_default().push(position);
        }
    }

    let candidate_limit = NEAREST_MISS_MAX_CANDIDATES
        .min((NEAREST_MISS_MAX_LINE_COMPARISONS / pattern.len().max(1)).max(1));
    let mut anchor_positions: Vec<(usize, &[usize])> = pattern
        .iter()
        .enumerate()
        .filter(|(_, line)| !line.trim().is_empty())
        .take(NEAREST_MISS_ANCHOR_COUNT)
        .filter_map(|(offset, line)| {
            line_index
                .get(line.trim())
                .map(|positions| (offset, positions.as_slice()))
        })
        .collect();
    anchor_positions.sort_by_key(|(offset, positions)| (positions.len(), *offset));

    let mut candidates = HashSet::new();
    for (wanted_offset, positions) in anchor_positions {
        add_sampled_candidates(&mut candidates, positions, wanted_offset, candidate_limit);
        if candidates.len() >= candidate_limit {
            break;
        }
    }
    if let Some(best) = best_scored_candidate(lines, pattern, candidates) {
        return NearestMissSearch::Found(best);
    }

    let Some((wanted_offset, wanted_line)) = rarest_wanted_line(pattern) else {
        return NearestMissSearch::NoSimilarRegion;
    };
    let normalized_wanted = normalize_fuzzy_line(wanted_line);
    let mut best_prefix = 0;
    let mut fuzzy_candidates = HashSet::new();
    for (file_position, actual_line) in lines.iter().enumerate() {
        let Some(start) = file_position.checked_sub(wanted_offset) else {
            continue;
        };
        let Some(prefix_score) = normalized_prefix_score(&normalized_wanted, actual_line) else {
            continue;
        };
        if prefix_score > best_prefix {
            best_prefix = prefix_score;
            fuzzy_candidates.clear();
        }
        if prefix_score == best_prefix && fuzzy_candidates.len() < candidate_limit {
            fuzzy_candidates.insert(start);
        }
    }

    best_scored_candidate(lines, pattern, fuzzy_candidates)
        .map_or(NearestMissSearch::NoSimilarRegion, NearestMissSearch::Found)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_match(
        actual: Option<SequenceMatch>,
        found: usize,
        tier: MatchTier,
        line_count: usize,
    ) {
        assert_eq!(
            actual,
            Some(SequenceMatch {
                found,
                tier,
                line_count,
            })
        );
    }

    #[test]
    fn normalization_helpers_match_patch_parser_sources() {
        assert_eq!(
            normalize_unicode("‘’‚‛“”„‟‐‑‒–—―…\u{00A0}"),
            "''''\"\"\"\"------... "
        );
        assert_eq!(normalize_indent("\t  alpha\t beta  "), "   alpha\t beta  ");
        assert_eq!(normalize_indent(""), "");
        assert_eq!(
            normalize_reflow_whitespace(" \talpha\n\u{00A0} beta  "),
            "alpha beta"
        );
        assert_eq!(
            strip_reflow_whitespace(" \talpha\n\u{00A0} beta  "),
            "alphabeta"
        );
        assert!(has_reflow_content("\u{00A0}x"));
        assert!(!has_reflow_content(" \t\n"));
    }

    #[test]
    fn exact_tier_wins_without_upgrading_to_later_tiers() {
        assert_match(
            seek_sequence_tiered(&["alpha", "beta"], &["beta"], 0, false),
            1,
            MatchTier::Exact,
            1,
        );
    }

    #[test]
    fn rstrip_tier_wins_before_trim() {
        assert_match(
            seek_sequence_tiered(&["alpha   "], &["alpha"], 0, false),
            0,
            MatchTier::Rstrip,
            1,
        );
    }

    #[test]
    fn trim_tier_wins_before_indent_and_unicode() {
        assert_match(
            seek_sequence_tiered(&["  alpha  "], &["alpha"], 0, false),
            0,
            MatchTier::Trim,
            1,
        );
    }

    #[test]
    fn indent_normalization_matches_tab_space_drift_but_trim_shadows_the_tier() {
        assert_eq!(normalize_indent("\treturn 42;"), " return 42;");
        assert_eq!(normalize_indent(" return 42;"), " return 42;");
        assert_eq!(
            try_match(
                &["\treturn 42;"],
                &[" return 42;"],
                0,
                |a, b| normalize_indent(a).trim_end() == normalize_indent(b).trim_end(),
                false,
            ),
            Some(0)
        );
        // Expect Trim, not Indent, for tab-vs-space input: a leading tab-vs-space drift
        // is already accepted by the earlier trim tier, so the nominal indent tier is shadowed.
        assert_match(
            seek_sequence_tiered(&["\treturn 42;"], &["    return 42;"], 0, false),
            0,
            MatchTier::Trim,
            1,
        );
    }

    #[test]
    fn unicode_tier_normalizes_smart_punctuation_after_stricter_tiers_fail() {
        assert_match(
            seek_sequence_tiered(
                &["const label = “alpha”—beta…;"],
                &["const label = \"alpha\"-beta...;"],
                0,
                false,
            ),
            0,
            MatchTier::Unicode,
            1,
        );
    }

    #[test]
    fn reflow_tier_matches_one_line_hunk_against_three_line_formatter_split() {
        let lines = [
            "function demo() {",
            "  const value = alpha +",
            "    beta +",
            "    gamma;",
            "  return value;",
            "}",
        ];
        let pattern = ["  const value = alpha + beta + gamma;"];

        assert_match(
            seek_sequence_tiered(&lines, &pattern, 0, false),
            1,
            MatchTier::Reflow,
            3,
        );
    }

    #[test]
    fn rejects_ambiguous_reflow_matches_instead_of_choosing_a_window() {
        // Reject a reflow match when the pattern could match more than one distinct window.
        let lines = [
            "const value = alpha +",
            "  beta +",
            "  gamma;",
            "",
            "const value = alpha +",
            "  beta +",
            "  gamma;",
        ];
        let pattern = ["const value = alpha + beta + gamma;"];

        assert_eq!(find_reflow_match(&lines, &pattern, 0), None);
        assert_eq!(seek_sequence_tiered(&lines, &pattern, 0, false), None);
    }

    #[test]
    fn uses_line_contiguous_match_before_considering_reflow_candidate() {
        // A line-contiguous match wins before any reflow candidate is considered.
        let lines = [
            "const value = alpha +",
            "  beta +",
            "  gamma;",
            "const value = alpha + beta + gamma;",
        ];
        let pattern = ["const value = alpha + beta + gamma;"];

        assert_match(
            seek_sequence_tiered(&lines, &pattern, 0, false),
            3,
            MatchTier::Exact,
            1,
        );
    }

    #[test]
    fn eof_hunk_only_matches_the_tail_and_never_forward_scans() {
        // EOF-anchored hunks match only the tail and never forward-scan.
        let pattern = ["marker", "old"];

        assert_match(
            seek_sequence_tiered(
                &["header", "marker", "old", "middle", "marker", "old"],
                &pattern,
                0,
                true,
            ),
            4,
            MatchTier::Exact,
            2,
        );
        assert_eq!(
            seek_sequence_tiered(
                &["header", "marker", "old", "middle", "marker", "changed"],
                &pattern,
                0,
                true,
            ),
            None
        );
    }

    #[test]
    fn eof_hunk_skips_reflow_even_when_the_tail_would_reflow_match() {
        let lines = ["header", "const value = alpha +", "  beta +", "  gamma;"];
        let pattern = ["const value = alpha + beta + gamma;"];

        assert_eq!(find_reflow_match(&lines, &pattern, 0), Some((1, 3)));
        assert_eq!(seek_sequence_tiered(&lines, &pattern, 0, true), None);
    }

    #[test]
    fn try_match_honors_start_index_for_forward_scans_and_eof_anchor() {
        assert_eq!(
            try_match(&["a", "b", "a", "b"], &["a", "b"], 1, |a, b| a == b, false),
            Some(2)
        );
        assert_eq!(
            try_match(&["a", "b", "a", "b"], &["a", "b"], 3, |a, b| a == b, false),
            None
        );
        assert_eq!(
            try_match(&["a", "b", "a", "b"], &["a", "b"], 3, |a, b| a == b, true),
            None
        );
    }

    #[test]
    fn nearest_miss_scores_matching_lines_across_the_candidate_window() {
        let lines = [
            "header",
            "  const first = 1;",
            "  const actual = 2;",
            "  return first;",
            "separator",
            "  const first = 1;",
            "  unrelated",
            "  unrelated",
        ];
        let pattern = [
            "  const first = 1;",
            "  const expected = 2;",
            "  return first;",
        ];

        assert_eq!(
            find_nearest_miss(&lines, &pattern, 128),
            NearestMissSearch::Found(NearestMiss {
                start: 1,
                end: 4,
                matched_lines: 2,
                first_divergence: 1,
            })
        );
    }

    #[test]
    fn nearest_miss_uses_a_strong_prefix_when_no_anchor_matches_exactly() {
        assert_eq!(
            find_nearest_miss(
                &["header", "const expected_value = 2;", "footer"],
                &["const expected_value = 1;"],
                42,
            ),
            NearestMissSearch::Found(NearestMiss {
                start: 1,
                end: 2,
                matched_lines: 0,
                first_divergence: 0,
            })
        );
    }

    #[test]
    fn nearest_miss_reports_no_region_when_anchors_and_prefixes_are_absent() {
        assert_eq!(
            find_nearest_miss(
                &["alpha", "beta", "gamma"],
                &["completely unrelated line"],
                17,
            ),
            NearestMissSearch::NoSimilarRegion
        );
    }

    #[test]
    fn nearest_miss_skips_files_larger_than_the_diagnostic_limit() {
        let synthetic_file = "x".repeat(NEAREST_MISS_MAX_FILE_BYTES + 1);
        assert_eq!(
            find_nearest_miss(
                &[synthetic_file.as_str()],
                &["wanted content"],
                synthetic_file.len(),
            ),
            NearestMissSearch::SkippedLargeFile
        );
    }
}
