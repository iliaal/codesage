use codesage_protocol::Chunk;

// 1500 chars ≈ 380–500 tokens for typical code (3–4 chars/token), which
// stays under the 512-token embed-time cap (MAX_SEQ_LENGTH) with slack
// for the ~30-token augmentation header and special tokens. The earlier
// 1000-char value paired with a 256-token cap caused silent truncation
// on dense chunks; the cap-and-chunk pair was raised together. See
// `bench/history/cap512-1500-2026-05-04.md` for the validating bench.
//
// The budgets below are character counts, not bytes: byte accounting
// over-chunks multibyte text ~2-3x (a 1500-byte CJK slice is only ~500
// chars). Byte offsets are still used for slicing and line numbers —
// only the size comparisons count characters.
pub const DEFAULT_CHUNK_SIZE: usize = 1500;
pub const DEFAULT_MIN_CHUNK_SIZE: usize = 350;
pub const DEFAULT_CHUNK_OVERLAP: usize = 200;

/// Version of the splitting algorithm below. Bump it when a change to
/// `chunk_text` can produce different chunk texts for the same input; it is
/// part of the semantic fingerprint that gates stored-vector reuse.
///
/// Version 2 switched the budgets from bytes to characters: ASCII chunking
/// is unchanged (one byte per char), multibyte text now chunks at the
/// designed granularity instead of ~2-3x finer.
pub const CHUNKER_VERSION: u32 = 2;

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

    // A zero chunk_size would make the char-fallback loop in
    // `split_recursive` compute `end == pos` and never advance — an infinite
    // loop on any non-empty input. Clamp once at the entry so every consumer
    // (split, merge, overlap) sees the same floor.
    let chunk_size = config.chunk_size.max(1);

    let raw = split_recursive(content, chunk_size, 0);

    let mut merged = merge_small_chunks(raw, content, config.min_chunk_size, chunk_size);

    apply_overlap(&mut merged, content, config.overlap, chunk_size);

    // Rescanning content[..pos] per chunk boundary is O(file × chunks);
    // one newline-offset pass plus a binary search per boundary keeps large
    // files linear. Overlap can move a chunk's start behind its
    // predecessor's, so boundaries aren't monotonic and a running cursor
    // wouldn't be safe.
    let newline_offsets: Vec<usize> = content
        .bytes()
        .enumerate()
        .filter_map(|(i, b)| (b == b'\n').then_some(i))
        .collect();
    // Line number at a byte offset = 1 + newlines strictly before it.
    let line_at = |pos: usize| 1 + newline_offsets.partition_point(|&nl| nl < pos) as u32;

    merged
        .into_iter()
        .map(|seg| {
            let start = snap_to_char_boundary(content, seg.start);
            let end = find_char_boundary(content, seg.end);
            let start_line = line_at(start);
            // A chunk's own trailing newline terminates its last line rather
            // than starting a new one; counting it would report an end_line
            // one past the chunk's content (a "aaa\n" chunk is line 1, not
            // lines 1-2) and misattribute symbols at chunk boundaries.
            let count_end = if end > start && content.as_bytes()[end - 1] == b'\n' {
                end - 1
            } else {
                end
            };
            let end_line = line_at(count_end);
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
    // Character budget, byte offsets: `text.len() <= max_size` in bytes
    // implies the same in chars, so the cheap check runs first and the
    // O(n) count only pays for inputs that might actually split.
    if text.len() <= max_size || text.chars().count() <= max_size {
        return vec![Segment {
            start: 0,
            end: text.len(),
        }];
    }

    if sep_idx >= SEPARATORS.len() {
        let mut segments = Vec::new();
        let mut pos = 0;
        while pos < text.len() {
            // `max_size >= 1` (clamped at the entry) advances at least one
            // char, so this loop always terminates.
            let end = advance_by_chars(text, pos, max_size);
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
    // Characters in `text[current_start..current_end]`, tracked
    // incrementally: recounting the whole candidate per part would be
    // quadratic in the part count. Parts tile the input contiguously, so
    // each part's chars are counted exactly once.
    let mut current_len = 0;

    for (part_start, part_end) in parts {
        let part_len = text[part_start..part_end].chars().count();
        let candidate_len = if current_start == current_end {
            part_len
        } else {
            current_len + part_len
        };

        if candidate_len <= max_size {
            current_end = part_end;
            current_len = candidate_len;
        } else {
            if current_start < current_end {
                segments.push(Segment {
                    start: current_start,
                    end: current_end,
                });
            }

            let part_text = &text[part_start..part_end];
            if part_len > max_size {
                let sub = split_recursive(part_text, max_size, sep_idx + 1);
                for s in sub {
                    segments.push(Segment {
                        start: part_start + s.start,
                        end: part_start + s.end,
                    });
                }
                current_start = part_end;
                current_end = part_end;
                current_len = 0;
            } else {
                current_start = part_start;
                current_end = part_end;
                current_len = part_len;
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

fn segment_chars(text: &str, seg: &Segment) -> usize {
    text[seg.start..seg.end].chars().count()
}

fn merge_small_chunks(
    segments: Vec<Segment>,
    text: &str,
    min_size: usize,
    max_size: usize,
) -> Vec<Segment> {
    if segments.is_empty() {
        return segments;
    }

    let mut merged: Vec<Segment> = Vec::new();
    for seg in segments {
        // Merge when either neighbor is sub-`min_size` and the result still
        // fits `max_size` — so a small segment following a large one is also
        // absorbed, not just a small predecessor.
        if let Some(last) = merged.last_mut()
            && (segment_chars(text, last) < min_size || segment_chars(text, &seg) < min_size)
            && segment_chars(text, last) + segment_chars(text, &seg) <= max_size
        {
            last.end = seg.end;
            continue;
        }
        merged.push(seg);
    }

    if merged.len() > 1 {
        let last_idx = merged.len() - 1;
        if segment_chars(text, &merged[last_idx]) < min_size {
            let combined =
                segment_chars(text, &merged[last_idx - 1]) + segment_chars(text, &merged[last_idx]);
            if combined <= max_size {
                let last_end = merged[last_idx].end;
                merged[last_idx - 1].end = last_end;
                merged.pop();
            }
        }
    }

    merged
}

fn apply_overlap(segments: &mut [Segment], text: &str, overlap: usize, chunk_size: usize) {
    if segments.len() < 2 || overlap == 0 {
        return;
    }

    for i in 1..segments.len() {
        let prev_end = segments[i - 1].end;
        let target_start = retreat_by_chars(text, prev_end, overlap);
        let new_start = find_line_boundary_after(text, target_start);
        // The overlap must not push the chunk over budget: the earliest
        // start that still fits `chunk_size` characters in `[start, end]`.
        let min_start = retreat_by_chars(text, segments[i].end, chunk_size);
        let new_start = new_start.max(min_start);
        if new_start < segments[i].start {
            segments[i].start = new_start;
        }
    }
}

/// Byte offset `n` characters after `pos`, clamped to `text.len()`. Always a
/// char boundary; advances at least one character while `pos` is inside the
/// text and `n >= 1`.
fn advance_by_chars(text: &str, pos: usize, n: usize) -> usize {
    text[pos..]
        .char_indices()
        .nth(n)
        .map_or(text.len(), |(off, _)| pos + off)
}

/// Byte offset `n` characters before `pos`, clamped to 0. Walks back over
/// UTF-8 continuation bytes, so the result is always a char boundary.
fn retreat_by_chars(text: &str, pos: usize, n: usize) -> usize {
    let bytes = text.as_bytes();
    let mut p = pos.min(text.len());
    let mut remaining = n;
    while remaining > 0 && p > 0 {
        p -= 1;
        if (bytes[p] & 0xC0) != 0x80 {
            remaining -= 1;
        }
    }
    p
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
    fn overlap_respects_chunk_size_ceiling() {
        let line = "x".repeat(80);
        let text = (0..30)
            .map(|_| line.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        let config = ChunkConfig {
            chunk_size: 800,
            min_chunk_size: 100,
            overlap: 300,
        };
        let chunks = chunk_text(&text, &config);
        assert!(chunks.len() >= 2);
        for chunk in &chunks {
            assert!(
                chunk.text.len() <= config.chunk_size,
                "chunk length {} exceeds cap {}",
                chunk.text.len(),
                config.chunk_size
            );
        }
    }

    #[test]
    fn overlap_between_chunks() {
        let lines: Vec<String> = (0..20)
            .map(|i| format!("line {i}: {}", "x".repeat(80)))
            .collect();
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
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[0].text, "line1\nline2\n");
        assert_eq!((chunks[0].start_line, chunks[0].end_line), (1, 2));
        assert_eq!(chunks[1].text, "line3\n\n");
        assert_eq!((chunks[1].start_line, chunks[1].end_line), (3, 4));
        assert_eq!(chunks[2].text, "line5\nline6");
        assert_eq!((chunks[2].start_line, chunks[2].end_line), (5, 6));
    }

    #[test]
    fn end_line_excludes_chunk_trailing_newline() {
        // First chunk is exactly "aaa\n": its content is line 1 only, and the
        // second chunk ("bbb", no trailing newline) is line 2 only.
        let config = ChunkConfig {
            chunk_size: 4,
            min_chunk_size: 1,
            overlap: 0,
        };
        let chunks = chunk_text("aaa\nbbb", &config);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].text, "aaa\n");
        assert_eq!((chunks[0].start_line, chunks[0].end_line), (1, 1));
        assert_eq!(chunks[1].text, "bbb");
        assert_eq!((chunks[1].start_line, chunks[1].end_line), (2, 2));
    }

    #[test]
    fn newline_terminated_file_single_chunk() {
        let chunks = chunk_text("aaa\nbbb\n", &default_config());
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].text, "aaa\nbbb\n");
        assert_eq!((chunks[0].start_line, chunks[0].end_line), (1, 2));
    }

    #[test]
    fn single_line_file_without_trailing_newline() {
        let chunks = chunk_text("aaa", &default_config());
        assert_eq!(chunks.len(), 1);
        assert_eq!((chunks[0].start_line, chunks[0].end_line), (1, 1));
    }

    #[test]
    fn merge_small_chunks_respects_chunk_size_cap() {
        let seg_a = "x".repeat(349);
        let seg_b = "y".repeat(349);
        let text = format!("{seg_a}\n{seg_b}");
        let config = ChunkConfig {
            chunk_size: 500,
            min_chunk_size: 350,
            overlap: 0,
        };
        let chunks = chunk_text(&text, &config);
        for chunk in &chunks {
            assert!(
                chunk.text.len() <= 500,
                "merged chunk exceeded chunk_size: {} bytes",
                chunk.text.len()
            );
        }
    }

    /// Reference implementation for line numbering: full rescan of
    /// `content[..pos]` per boundary, with the same trailing-newline
    /// end_line exclusion as production.
    fn naive_line_numbers(content: &str, start: usize, end: usize) -> (u32, u32) {
        let start_line = 1 + content[..start].matches('\n').count() as u32;
        let count_end = if end > start && content.as_bytes()[end - 1] == b'\n' {
            end - 1
        } else {
            end
        };
        let end_line = 1 + content[..count_end].matches('\n').count() as u32;
        (start_line, end_line)
    }

    fn mixed_line_fixture(lines: usize, trailing_newline: bool) -> String {
        let mut s = String::new();
        for i in 0..lines {
            if i % 9 == 0 {
                s.push('\n');
                continue;
            }
            let pad = "x".repeat((i * 37) % 110);
            s.push_str(&format!("line{i:03} é中λ {pad}\n"));
        }
        if !trailing_newline {
            s.pop();
        }
        s
    }

    #[test]
    fn line_numbers_match_naive_full_rescan() {
        let configs = [
            ChunkConfig {
                chunk_size: 300,
                min_chunk_size: 50,
                overlap: 80,
            },
            ChunkConfig::default(),
        ];
        for trailing_newline in [true, false] {
            let text = mixed_line_fixture(120, trailing_newline);
            assert!(
                text.len() > 2 * DEFAULT_CHUNK_SIZE,
                "fixture must span multiple default-size chunks"
            );
            for config in &configs {
                let chunks = chunk_text(&text, config);
                assert!(
                    chunks.len() >= 3,
                    "fixture must produce a multi-chunk split (got {})",
                    chunks.len()
                );
                for (i, chunk) in chunks.iter().enumerate() {
                    let (want_start, want_end) =
                        naive_line_numbers(&text, chunk.start_byte, chunk.end_byte);
                    assert_eq!(
                        (chunk.start_line, chunk.end_line),
                        (want_start, want_end),
                        "chunk {i} [{}..{}] (trailing_newline={trailing_newline}, chunk_size={})",
                        chunk.start_byte,
                        chunk.end_byte,
                        config.chunk_size
                    );
                }
            }
        }
    }

    #[test]
    fn zero_chunk_size_terminates_with_sane_output() {
        // Regression: chunk_size = 0 used to hang the char-fallback splitting
        // loop. The entry clamp treats it as 1.
        let config = ChunkConfig {
            chunk_size: 0,
            min_chunk_size: DEFAULT_MIN_CHUNK_SIZE,
            overlap: DEFAULT_CHUNK_OVERLAP,
        };
        let text = "fn a() {}\nfn b() {}";
        let chunks = chunk_text(text, &config);
        assert!(!chunks.is_empty());
        let total: usize = chunks.iter().map(|c| c.text.len()).sum();
        assert!(
            total >= text.len(),
            "chunks must cover the input: {total} < {}",
            text.len()
        );
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

    #[test]
    fn budgets_count_characters_not_bytes() {
        // 600 CJK chars are 1800 bytes: over a 1500-byte budget but under
        // a 1500-char one. Byte accounting hard-split this input in two.
        let text = "中".repeat(600);
        let config = ChunkConfig {
            chunk_size: 1500,
            min_chunk_size: 100,
            overlap: 0,
        };
        let chunks = chunk_text(&text, &config);
        assert_eq!(
            chunks.len(),
            1,
            "600 chars must fit one 1500-char chunk despite being 1800 bytes"
        );
        assert_eq!(chunks[0].text.chars().count(), 600);

        // Overlapped multibyte chunks stay within the char budget on
        // char boundaries (slicing would panic otherwise).
        let text = "あ".repeat(1000);
        let config = ChunkConfig {
            chunk_size: 400,
            min_chunk_size: 50,
            overlap: 100,
        };
        let chunks = chunk_text(&text, &config);
        assert!(chunks.len() >= 2, "1000 chars need several 400-char chunks");
        for chunk in &chunks {
            assert!(
                chunk.text.chars().count() <= 400,
                "chunk exceeds char budget: {} chars",
                chunk.text.chars().count()
            );
            assert!(
                text.is_char_boundary(chunk.start_byte) && text.is_char_boundary(chunk.end_byte),
                "chunk boundary splits a character"
            );
        }
    }
}
