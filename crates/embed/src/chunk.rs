use codesage_protocol::Chunk;

pub const DEFAULT_CHUNK_SIZE: usize = 1000;
pub const DEFAULT_MIN_CHUNK_SIZE: usize = 250;
pub const DEFAULT_CHUNK_OVERLAP: usize = 150;

#[derive(Debug, Clone)]
pub struct ChunkConfig {
    pub chunk_size: usize,
    pub min_chunk_size: usize,
    pub overlap: usize,
}

impl Default for ChunkConfig {
    fn default() -> Self {
        Self {
            chunk_size: DEFAULT_CHUNK_SIZE,
            min_chunk_size: DEFAULT_MIN_CHUNK_SIZE,
            overlap: DEFAULT_CHUNK_OVERLAP,
        }
    }
}

static SEPARATORS: &[&str] = &["\n\n", "\n", ". ", " "];

pub fn chunk_text(content: &str, config: &ChunkConfig) -> Vec<Chunk> {
    if content.is_empty() {
        return Vec::new();
    }

    let raw = split_recursive(content, config.chunk_size, 0);

    let mut merged = merge_small_chunks(raw, config.min_chunk_size);

    apply_overlap(&mut merged, content, config.overlap);

    merged
        .into_iter()
        .map(|seg| {
            let start = snap_to_char_boundary(content, seg.start);
            let end = find_char_boundary(content, seg.end);
            let start_line = 1 + content[..start].matches('\n').count() as u32;
            let end_line = 1 + content[..end].matches('\n').count() as u32;
            Chunk {
                text: content[start..end].to_string(),
                start_line,
                end_line,
                start_byte: start,
                end_byte: end,
            }
        })
        .collect()
}

#[derive(Debug, Clone)]
struct Segment {
    start: usize,
    end: usize,
}

fn split_recursive(text: &str, max_size: usize, sep_idx: usize) -> Vec<Segment> {
    if text.len() <= max_size {
        return vec![Segment {
            start: 0,
            end: text.len(),
        }];
    }

    if sep_idx >= SEPARATORS.len() {
        let mut segments = Vec::new();
        let mut pos = 0;
        while pos < text.len() {
            let end = (pos + max_size).min(text.len());
            let end = find_char_boundary(text, end);
            segments.push(Segment { start: pos, end });
            pos = end;
        }
        return segments;
    }

    let sep = SEPARATORS[sep_idx];
    let parts = split_keeping_offsets(text, sep);

    let mut segments = Vec::new();
    let mut current_start = 0;
    let mut current_end = 0;

    for (part_start, part_end) in parts {
        let candidate_len = if current_start == current_end {
            part_end - part_start
        } else {
            part_end - current_start
        };

        if candidate_len <= max_size {
            current_end = part_end;
        } else {
            if current_start < current_end {
                segments.push(Segment {
                    start: current_start,
                    end: current_end,
                });
            }

            let part_text = &text[part_start..part_end];
            if part_text.len() > max_size {
                let sub = split_recursive(part_text, max_size, sep_idx + 1);
                for s in sub {
                    segments.push(Segment {
                        start: part_start + s.start,
                        end: part_start + s.end,
                    });
                }
                current_start = part_end;
                current_end = part_end;
            } else {
                current_start = part_start;
                current_end = part_end;
            }
        }
    }

    if current_start < current_end {
        segments.push(Segment {
            start: current_start,
            end: current_end,
        });
    }

    segments
}

fn split_keeping_offsets(text: &str, sep: &str) -> Vec<(usize, usize)> {
    let mut parts = Vec::new();
    let mut start = 0;

    for (idx, _) in text.match_indices(sep) {
        let end = idx + sep.len();
        if start < end {
            parts.push((start, end));
        }
        start = end;
    }

    if start < text.len() {
        parts.push((start, text.len()));
    }

    parts
}

fn merge_small_chunks(segments: Vec<Segment>, min_size: usize) -> Vec<Segment> {
    if segments.is_empty() {
        return segments;
    }

    let mut merged: Vec<Segment> = Vec::new();
    for seg in segments {
        if let Some(last) = merged.last_mut()
            && (last.end - last.start) < min_size {
                last.end = seg.end;
                continue;
            }
        merged.push(seg);
    }

    if merged.len() > 1 {
        let last_idx = merged.len() - 1;
        if (merged[last_idx].end - merged[last_idx].start) < min_size {
            let last_end = merged[last_idx].end;
            merged[last_idx - 1].end = last_end;
            merged.pop();
        }
    }

    merged
}

fn apply_overlap(segments: &mut [Segment], text: &str, overlap: usize) {
    if segments.len() < 2 || overlap == 0 {
        return;
    }

    for i in 1..segments.len() {
        let prev_end = segments[i - 1].end;
        let target_start = prev_end.saturating_sub(overlap);
        let new_start = find_line_boundary_after(text, target_start);
        if new_start < segments[i].start {
            segments[i].start = new_start;
        }
    }
}

fn find_line_boundary_after(text: &str, pos: usize) -> usize {
    if pos == 0 {
        return 0;
    }
    let pos = snap_to_char_boundary(text, pos);
    match text[pos..].find('\n') {
        Some(offset) => {
            let nl = pos + offset + 1;
            if nl < text.len() { nl } else { pos }
        }
        None => pos,
    }
}

fn snap_to_char_boundary(text: &str, pos: usize) -> usize {
    if pos >= text.len() {
        return text.len();
    }
    let mut p = pos;
    while !text.is_char_boundary(p) && p > 0 {
        p -= 1;
    }
    p
}

fn find_char_boundary(text: &str, pos: usize) -> usize {
    if pos >= text.len() {
        return text.len();
    }
    let mut p = pos;
    while !text.is_char_boundary(p) && p < text.len() {
        p += 1;
    }
    p
}

#[cfg(test)]
mod tests {
    use super::*;

    fn default_config() -> ChunkConfig {
        ChunkConfig::default()
    }

    #[test]
    fn empty_input() {
        let chunks = chunk_text("", &default_config());
        assert!(chunks.is_empty());
    }

    #[test]
    fn small_input_single_chunk() {
        let text = "fn main() { println!(\"hello\"); }";
        let chunks = chunk_text(text, &default_config());
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].text, text);
        assert_eq!(chunks[0].start_line, 1);
        assert_eq!(chunks[0].end_line, 1);
    }

    #[test]
    fn paragraph_splitting() {
        let para = "x".repeat(600);
        let text = format!("{para}\n\n{para}");
        let config = ChunkConfig {
            chunk_size: 800,
            min_chunk_size: 100,
            overlap: 0,
        };
        let chunks = chunk_text(&text, &config);
        assert_eq!(chunks.len(), 2);
    }

    #[test]
    fn line_splitting_fallback() {
        let line = "x".repeat(400);
        let text = format!("{line}\n{line}\n{line}");
        let config = ChunkConfig {
            chunk_size: 500,
            min_chunk_size: 100,
            overlap: 0,
        };
        let chunks = chunk_text(&text, &config);
        assert!(chunks.len() >= 2);
        for chunk in &chunks {
            assert!(chunk.text.len() <= 500 + 10);
        }
    }

    #[test]
    fn overlap_between_chunks() {
        let lines: Vec<String> = (0..20).map(|i| format!("line {i}: {}", "x".repeat(80))).collect();
        let text = lines.join("\n");
        let config = ChunkConfig {
            chunk_size: 500,
            min_chunk_size: 100,
            overlap: 150,
        };
        let chunks = chunk_text(&text, &config);
        assert!(chunks.len() >= 2);
        if chunks.len() >= 2 {
            let c0_end_byte = chunks[0].end_byte;
            let c1_start_byte = chunks[1].start_byte;
            assert!(
                c1_start_byte < c0_end_byte,
                "expected overlap: c1 starts at {c1_start_byte} but c0 ends at {c0_end_byte}"
            );
        }
    }

    #[test]
    fn min_chunk_merging() {
        let text = "short\n\nmedium content here\n\ntiny";
        let config = ChunkConfig {
            chunk_size: 1000,
            min_chunk_size: 20,
            overlap: 0,
        };
        let chunks = chunk_text(text, &config);
        for chunk in &chunks {
            assert!(
                chunk.text.len() >= 20 || chunks.len() == 1,
                "chunk too small: {} chars",
                chunk.text.len()
            );
        }
    }

    #[test]
    fn line_numbers_accurate() {
        let text = "line1\nline2\nline3\n\nline5\nline6";
        let config = ChunkConfig {
            chunk_size: 15,
            min_chunk_size: 5,
            overlap: 0,
        };
        let chunks = chunk_text(text, &config);
        assert!(chunks[0].start_line == 1);
        if chunks.len() > 1 {
            assert!(chunks.last().unwrap().end_line >= 5);
        }
    }

    #[test]
    fn multiline_code_chunk() {
        let lines: Vec<String> = (1..=50)
            .map(|i| format!("    let x{i} = compute_value({i});"))
            .collect();
        let text = format!("fn large_function() {{\n{}\n}}", lines.join("\n"));
        let config = default_config();
        let chunks = chunk_text(&text, &config);
        assert!(!chunks.is_empty());
        let total_coverage: usize = chunks.iter().map(|c| c.text.len()).sum();
        assert!(total_coverage >= text.len());
    }
}
