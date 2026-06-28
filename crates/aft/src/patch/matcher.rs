//! Patch-specific line-sequence matcher ported from the TypeScript apply_patch engine.
//!
//! This module intentionally does not reuse `fuzzy_match`: edit matching works in byte
//! ranges, while apply_patch needs line indexes, EOF anchoring, and unique-only reflow.

use std::collections::HashSet;

/// Allow candidate reflow windows to differ by up to eight non-whitespace characters before exact normalized comparison.
pub const REFLOW_NON_WS_TOLERANCE: usize = 8;

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
}
