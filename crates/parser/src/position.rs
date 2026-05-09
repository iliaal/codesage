//! Convert tree-sitter byte-based column offsets to UTF-8 character columns.
//!
//! Tree-sitter's `Node::start_position()` / `end_position()` return a `Point`
//! whose `column` field is bytes from the start of the line, not codepoints.
//! For ASCII source that's the same number, but a single CJK ideograph or
//! emoji at the start of a line shifts the byte column past where any editor
//! would render the cursor — goto-definition jumps land in the wrong place.
//!
//! The 0.26 Rust binding doesn't expose `ts_node_*_point_utf8`, so we do the
//! conversion in-tree: walk back to the line start, decode the prefix as
//! UTF-8, count the chars.

use tree_sitter::Node;

/// UTF-8 character column for `byte_offset` within `source`.
///
/// `byte_offset` is the absolute byte index in `source` (typically
/// `node.start_byte()` or `node.end_byte()`). The function locates the
/// containing line by scanning back to the previous `\n`, then counts UTF-8
/// `char` codepoints in the prefix between the line start and `byte_offset`.
///
/// Falls back to the byte count when the prefix isn't valid UTF-8 — better
/// to return the wrong-but-monotonic byte column than to panic on a binary
/// blob that slipped past the discovery filter.
fn utf8_column_for_byte(source: &[u8], byte_offset: usize) -> u32 {
    let byte_offset = byte_offset.min(source.len());
    let line_start = source[..byte_offset]
        .iter()
        .rposition(|&b| b == b'\n')
        .map(|i| i + 1)
        .unwrap_or(0);
    let prefix = &source[line_start..byte_offset];
    match std::str::from_utf8(prefix) {
        Ok(s) => s.chars().count() as u32,
        Err(_) => prefix.len() as u32,
    }
}

/// (row, column) for the start of `node`. Row is zero-based (matches
/// tree-sitter's `Point.row`); column is a UTF-8 character index, not bytes.
pub fn node_start_utf8(node: &Node, source: &[u8]) -> (u32, u32) {
    let row = node.start_position().row as u32;
    let col = utf8_column_for_byte(source, node.start_byte());
    (row, col)
}

/// (row, column) for the end of `node`. Same semantics as `node_start_utf8`.
pub fn node_end_utf8(node: &Node, source: &[u8]) -> (u32, u32) {
    let row = node.end_position().row as u32;
    let col = utf8_column_for_byte(source, node.end_byte());
    (row, col)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_column_matches_byte_column() {
        let source = b"fn main() {}\n";
        assert_eq!(utf8_column_for_byte(source, 0), 0);
        assert_eq!(utf8_column_for_byte(source, 3), 3);
        assert_eq!(utf8_column_for_byte(source, 12), 12);
    }

    #[test]
    fn cjk_codepoints_count_as_one_column() {
        // Each CJK char is 3 UTF-8 bytes. Byte column would be 9 at the end of
        // 「日本語」; codepoint column should be 3.
        let source = "日本語 = 1;\n".as_bytes();
        let after_cjk = "日本語".len(); // 9 bytes
        assert_eq!(utf8_column_for_byte(source, after_cjk), 3);
    }

    #[test]
    fn emoji_counts_as_one_column() {
        // A single 🦀 is 4 UTF-8 bytes but one char.
        let source = "🦀 crab\n".as_bytes();
        let after_crab = "🦀".len(); // 4 bytes
        assert_eq!(utf8_column_for_byte(source, after_crab), 1);
    }

    #[test]
    fn column_resets_at_each_line() {
        // Second line starts after the newline; columns count from there.
        let source = b"alpha\nbeta\n";
        let beta_offset = "alpha\n".len(); // 6
        assert_eq!(utf8_column_for_byte(source, beta_offset), 0);
        let after_beta = "alpha\nbeta".len(); // 10
        assert_eq!(utf8_column_for_byte(source, after_beta), 4);
    }

    #[test]
    fn invalid_utf8_prefix_falls_back_to_bytes() {
        let mut source = b"abc".to_vec();
        source.push(0xFF); // not a valid UTF-8 start byte
        source.push(b'd');
        // byte_offset 5 (after the invalid byte and one more letter)
        // prefix "abc\xFFd" is not valid UTF-8 — fallback returns byte length.
        assert_eq!(utf8_column_for_byte(&source, 5), 5);
    }
}
