use crate::compress::generic::{ceil_char_boundary, floor_char_boundary};

pub const FINAL_OUTPUT_CAP_BYTES: usize = 16 * 1024;
pub const FINAL_OUTPUT_HEAD_BYTES: usize = 6 * 1024;
pub const FINAL_OUTPUT_TAIL_BYTES: usize = 10 * 1024;

pub const RUNNING_OUTPUT_PREVIEW_BYTES: usize = 8 * 1024;
pub const COMPLETION_OUTPUT_PREVIEW_BYTES: usize = 4 * 1024;
pub const COMPLETION_OUTPUT_HEAD_BYTES: usize = 2 * 1024;
pub const COMPLETION_OUTPUT_TAIL_BYTES: usize = 2 * 1024;

pub const RAW_PASSTHROUGH_CAP_BYTES: usize = 50 * 1024;
pub const RAW_PASSTHROUGH_HEAD_BYTES: usize = 20 * 1024;
pub const RAW_PASSTHROUGH_TAIL_BYTES: usize = 30 * 1024;

pub const STRUCTURED_OUTPUT_CAP_BYTES: usize = 50 * 1024;

pub const COMPRESS_INPUT_CAP_BYTES: usize = 10 * 1024 * 1024;
pub const COMPRESS_INPUT_HEAD_BYTES: usize = 4 * 1024 * 1024;
pub const COMPRESS_INPUT_TAIL_BYTES: usize = 6 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CappedText {
    pub text: String,
    pub truncated: bool,
}

pub fn cap_final_output(input: &str) -> CappedText {
    cap_head_tail(
        input,
        FINAL_OUTPUT_CAP_BYTES,
        FINAL_OUTPUT_HEAD_BYTES,
        FINAL_OUTPUT_TAIL_BYTES,
    )
}

pub fn cap_final_output_with_marker(input: &str, marker: &str) -> CappedText {
    cap_head_tail_with_marker(
        input,
        FINAL_OUTPUT_CAP_BYTES,
        FINAL_OUTPUT_HEAD_BYTES,
        FINAL_OUTPUT_TAIL_BYTES,
        marker,
    )
}

pub fn cap_completion_output(input: &str) -> CappedText {
    cap_head_tail(
        input,
        COMPLETION_OUTPUT_PREVIEW_BYTES,
        COMPLETION_OUTPUT_HEAD_BYTES,
        COMPLETION_OUTPUT_TAIL_BYTES,
    )
}

pub fn cap_completion_output_with_marker(input: &str, marker: &str) -> CappedText {
    cap_head_tail_with_marker(
        input,
        COMPLETION_OUTPUT_PREVIEW_BYTES,
        COMPLETION_OUTPUT_HEAD_BYTES,
        COMPLETION_OUTPUT_TAIL_BYTES,
        marker,
    )
}

pub fn cap_head_tail(
    input: &str,
    threshold_bytes: usize,
    keep_head: usize,
    keep_tail: usize,
) -> CappedText {
    if input.len() <= threshold_bytes {
        return CappedText {
            text: input.to_string(),
            truncated: false,
        };
    }

    let head_end = floor_char_boundary(input, keep_head.min(input.len()));
    let mut tail_start = ceil_char_boundary(input, input.len().saturating_sub(keep_tail));

    if head_end >= tail_start {
        return CappedText {
            text: input.to_string(),
            truncated: false,
        };
    }

    let marker_prefix_len = if head_end == 0 || input[..head_end].ends_with('\n') {
        0
    } else {
        1
    };
    loop {
        let truncated_bytes = tail_start - head_end;
        let marker_len = marker_prefix_len
            + "...<truncated ".len()
            + truncated_bytes.to_string().len()
            + " bytes>...\n".len();
        let max_tail = threshold_bytes.saturating_sub(head_end + marker_len);
        let adjusted_tail_start = ceil_char_boundary(input, input.len().saturating_sub(max_tail));
        if adjusted_tail_start <= tail_start {
            break;
        }
        tail_start = adjusted_tail_start;
        if head_end >= tail_start {
            return CappedText {
                text: input.to_string(),
                truncated: false,
            };
        }
    }

    let truncated_bytes = tail_start - head_end;
    let mut output = String::with_capacity(threshold_bytes.min(input.len()));
    output.push_str(&input[..head_end]);
    if !output.ends_with('\n') {
        output.push('\n');
    }
    output.push_str("...<truncated ");
    output.push_str(&truncated_bytes.to_string());
    output.push_str(" bytes>...\n");
    output.push_str(&input[tail_start..]);

    CappedText {
        text: output,
        truncated: true,
    }
}

pub fn cap_head_tail_with_marker(
    input: &str,
    threshold_bytes: usize,
    keep_head: usize,
    keep_tail: usize,
    marker: &str,
) -> CappedText {
    if marker.is_empty() {
        return cap_head_tail(input, threshold_bytes, keep_head, keep_tail);
    }
    if input.len() <= threshold_bytes {
        let with_marker = append_marker_line(input, marker);
        if with_marker.len() <= threshold_bytes {
            return CappedText {
                text: with_marker,
                truncated: true,
            };
        }
    }

    let mut head_budget = keep_head.min(input.len());
    let mut tail_budget = keep_tail.min(input.len());
    let mut seen_budgets = Vec::new();

    for _ in 0..8 {
        let head_end = floor_char_boundary(input, head_budget);
        let marker_prefix_len = if head_end == 0 || input[..head_end].ends_with('\n') {
            0
        } else {
            1
        };
        let marker_len = marker_prefix_len + marker.len() + 1;
        let available = threshold_bytes.saturating_sub(marker_len);
        let next_head = keep_head.min(available).min(input.len());
        let next_tail = keep_tail
            .min(available.saturating_sub(next_head))
            .min(input.len().saturating_sub(next_head));

        if next_head == head_budget && next_tail == tail_budget {
            break;
        }
        if seen_budgets.contains(&(next_head, next_tail)) {
            if next_head.saturating_add(next_tail) < head_budget.saturating_add(tail_budget) {
                head_budget = next_head;
                tail_budget = next_tail;
            }
            break;
        }
        seen_budgets.push((head_budget, tail_budget));
        head_budget = next_head;
        tail_budget = next_tail;
    }

    let head_end = floor_char_boundary(input, head_budget);
    let tail_start = ceil_char_boundary(input, input.len().saturating_sub(tail_budget));
    let tail_start = if head_end >= tail_start {
        input.len()
    } else {
        tail_start
    };

    CappedText {
        text: marker_capped_output(input, head_end, tail_start, marker, threshold_bytes),
        truncated: true,
    }
}

fn append_marker_line(input: &str, marker: &str) -> String {
    if input.is_empty() {
        return marker.to_string();
    }
    let mut output = input.trim_end().to_string();
    output.push('\n');
    output.push_str(marker);
    output
}

fn marker_capped_output(
    input: &str,
    head_end: usize,
    tail_start: usize,
    marker: &str,
    threshold_bytes: usize,
) -> String {
    let output = marker_output(input, head_end, tail_start, marker);
    if output.len() <= threshold_bytes {
        return output;
    }

    let separator_len = usize::from(head_end > 0 && !input[..head_end].ends_with('\n'));
    let marker_line_len = marker.len().saturating_add(1);
    let tail_budget = threshold_bytes.saturating_sub(head_end + separator_len + marker_line_len);
    let adjusted_tail_start = ceil_char_boundary(input, input.len().saturating_sub(tail_budget));
    let output = marker_output(input, head_end, tail_start.max(adjusted_tail_start), marker);
    if output.len() <= threshold_bytes {
        return output;
    }

    // If the marker itself still fits, prefer preserving it and trimming all head
    // content before resorting to a marker-only hard cap.
    let tail_budget = threshold_bytes.saturating_sub(marker_line_len);
    let adjusted_tail_start = ceil_char_boundary(input, input.len().saturating_sub(tail_budget));
    let output = marker_output(input, 0, adjusted_tail_start, marker);
    if output.len() <= threshold_bytes {
        return output;
    }

    marker[..floor_char_boundary(marker, threshold_bytes.min(marker.len()))].to_string()
}

fn marker_output(input: &str, head_end: usize, tail_start: usize, marker: &str) -> String {
    let mut output = String::new();
    output.push_str(&input[..head_end]);
    if head_end > 0 && !output.ends_with('\n') {
        output.push('\n');
    }
    output.push_str(marker);
    if tail_start < input.len() {
        output.push('\n');
        output.push_str(&input[tail_start..]);
    }
    output
}

pub fn quote_path(path: &str) -> String {
    let escaped = path.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{escaped}\"")
}

pub fn json_output_pointer(total_bytes: u64, path: &str) -> String {
    let kb = total_bytes.div_ceil(1024);
    format!(
        "[JSON output {kb} KB; full output: read {}]",
        quote_path(path)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn head_tail_cap_respects_utf8_boundaries() {
        let input = format!("{}{}", "🦀".repeat(4_000), "tail");
        let capped = cap_head_tail(&input, 128, 64, 64);
        assert!(capped.truncated);
        assert!(capped.text.is_char_boundary(capped.text.len()));
        assert!(capped.text.contains("...<truncated "));
        assert!(capped.text.len() <= 128);
        assert!(capped.text.ends_with("tail"));
    }

    #[test]
    fn marker_cap_respects_newline_boundary_oscillation() {
        let capped = cap_head_tail_with_marker("01234567\nabcdef", 20, 15, 0, "123456789");

        assert!(capped.truncated);
        assert!(capped.text.len() <= 20, "{}", capped.text.len());
        assert!(capped.text.is_char_boundary(capped.text.len()));
        assert!(!capped.text.starts_with('\n'));
    }

    #[test]
    fn marker_cap_respects_tiny_threshold_and_long_marker() {
        let capped = cap_head_tail_with_marker("abcdef", 5, 0, 0, "[very long recovery marker]");

        assert!(capped.truncated);
        assert!(capped.text.len() <= 5, "{}", capped.text.len());
        assert!(capped.text.is_char_boundary(capped.text.len()));
        assert!(!capped.text.starts_with('\n'));
    }

    #[test]
    fn marker_cap_accounts_for_marker_when_body_is_under_threshold() {
        let capped = cap_head_tail_with_marker("0123456789", 10, 10, 0, "[x]");

        assert!(capped.truncated);
        assert!(capped.text.len() <= 10, "{}", capped.text.len());
        assert!(capped.text.contains("[x]"));
    }
}
