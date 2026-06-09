use crate::compress::{CompressionResult, Compressor};

pub fn strip_ansi(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut output = String::with_capacity(input.len());
    let mut index = 0;
    let mut last_kept = 0;

    while index < bytes.len() {
        if bytes[index] != 0x1b {
            index += 1;
            continue;
        }

        let Some(next) = bytes.get(index + 1).copied() else {
            break;
        };

        let end = if next == b'[' {
            let mut cursor = index + 2;
            while cursor < bytes.len() {
                if (0x40..=0x7e).contains(&bytes[cursor]) {
                    cursor += 1;
                    break;
                }
                cursor += 1;
            }
            cursor
        } else if (0x40..=0x5f).contains(&next) {
            index + 2
        } else {
            index += 1;
            continue;
        };

        output.push_str(&input[last_kept..index]);
        index = end.min(bytes.len());
        last_kept = index;
    }

    output.push_str(&input[last_kept..]);
    output
}

pub fn dedup_consecutive(input: &str) -> String {
    let had_trailing_newline = input.ends_with('\n');
    let mut output = String::with_capacity(input.len());
    let mut lines = input.lines();

    let Some(mut current) = lines.next() else {
        return String::new();
    };
    let mut count = 1usize;

    for line in lines {
        if line == current {
            count += 1;
        } else {
            push_dedup_run(&mut output, current, count);
            current = line;
            count = 1;
        }
    }
    push_dedup_run(&mut output, current, count);

    if !had_trailing_newline {
        output.pop();
    }

    output
}

fn push_dedup_run(output: &mut String, line: &str, count: usize) {
    output.push_str(line);
    output.push('\n');
    if count >= 4 {
        output.push_str("... (");
        output.push_str(&(count - 1).to_string());
        output.push_str(" more)\n");
    } else {
        for _ in 1..count {
            output.push_str(line);
            output.push('\n');
        }
    }
}

pub fn middle_truncate(
    input: &str,
    threshold_bytes: usize,
    keep_head: usize,
    keep_tail: usize,
) -> String {
    if input.len() <= threshold_bytes {
        return input.to_string();
    }

    let head_end = floor_char_boundary(input, keep_head.min(input.len()));
    let tail_start = ceil_char_boundary(input, input.len().saturating_sub(keep_tail));

    if head_end >= tail_start {
        return input.to_string();
    }

    let truncated_bytes = tail_start - head_end;
    let mut output = String::with_capacity(head_end + keep_tail + 64);
    output.push_str(&input[..head_end]);
    if !output.ends_with('\n') {
        output.push('\n');
    }
    output.push_str("...<truncated ");
    output.push_str(&truncated_bytes.to_string());
    output.push_str(" bytes>...\n");
    output.push_str(&input[tail_start..]);
    output
}

pub(crate) fn floor_char_boundary(input: &str, mut index: usize) -> usize {
    while index > 0 && !input.is_char_boundary(index) {
        index -= 1;
    }
    index
}

pub(crate) fn ceil_char_boundary(input: &str, mut index: usize) -> usize {
    while index < input.len() && !input.is_char_boundary(index) {
        index += 1;
    }
    index
}

pub struct GenericCompressor;

impl GenericCompressor {
    pub fn compress_output(output: &str) -> String {
        let stripped = strip_ansi(output);
        dedup_consecutive(&stripped)
    }
}

impl Compressor for GenericCompressor {
    fn matches(&self, _command: &str) -> bool {
        true
    }

    fn compress_with_exit_code(
        &self,
        _command: &str,
        output: &str,
        _exit_code: Option<i32>,
    ) -> CompressionResult {
        Self::compress_output(output).into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generic_does_not_pretruncate_above_old_five_kib_threshold() {
        let input = (0..900)
            .map(|idx| format!("unique-line-{idx:04}"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(input.len() > 5 * 1024);

        let compressed = GenericCompressor::compress_output(&input);

        assert!(!compressed.contains("...<truncated "));
        assert!(compressed.len() > 5 * 1024);
        assert!(compressed.contains("unique-line-0000"));
        assert!(compressed.contains("unique-line-0899"));
    }
}
