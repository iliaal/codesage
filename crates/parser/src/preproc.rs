//! Preprocessor groups no build can reach, for C and C++.
//!
//! `find_references` publishes `counts_floor`: the true count is at least the
//! reported one. A call parked in `#if 0` breaks that promise in the other
//! direction, so the row must not exist rather than carry a flag — a flagged
//! row still counts, and the count is the wrong number.
//!
//! Only *provably* dead groups qualify. `#ifdef X`, `#ifndef X` and `#if EXPR`
//! stay live: "this build does not define X" is a build-configuration fact,
//! and CodeSage indexes a file once, with no configuration, so dropping those
//! bodies would trade a bounded over-count for a much larger under-count.
//! The `#if 0` arm dead / `#elif`-or-`#else` arm live split matches
//! `codesage_features::mappers::php::strip_if_zero_blocks`.

use codesage_protocol::Language;
use tree_sitter::Node;

/// Byte ranges of preprocessor groups every configuration skips. Sorted and
/// non-overlapping after construction, so a lookup is one binary search.
#[derive(Debug, Default)]
pub(crate) struct DeadRegions {
    ranges: Vec<(usize, usize)>,
}

impl DeadRegions {
    /// Empty for every language without the C preprocessor, so extraction
    /// paths can build this unconditionally.
    ///
    /// One iterative pre-order pass over the tree, rather than an ancestor
    /// walk per query match: `ts_node_parent` re-descends from the root, which
    /// would make the per-match check quadratic in nesting depth.
    pub(crate) fn scan(root: Node, source: &[u8], language: Language) -> Self {
        if !matches!(language, Language::C | Language::Cpp) {
            return Self::default();
        }
        let mut ranges: Vec<(usize, usize)> = Vec::new();
        let mut cursor = root.walk();
        loop {
            collect_dead_spans(&cursor.node(), source, &mut ranges);
            if cursor.goto_first_child() {
                continue;
            }
            loop {
                if cursor.goto_next_sibling() {
                    break;
                }
                if !cursor.goto_parent() {
                    return Self::merged(ranges);
                }
            }
        }
    }

    /// True when `node` starts inside a dead group. Start position alone
    /// decides: a captured `@ref` or `@def` node lies wholly within one group
    /// unless the directives are unbalanced, and an unbalanced region is
    /// already a degraded parse.
    pub(crate) fn covers(&self, node: &Node) -> bool {
        let byte = node.start_byte();
        let idx = self.ranges.partition_point(|(start, _)| *start <= byte);
        idx > 0 && self.ranges[idx - 1].1 > byte
    }

    fn merged(mut ranges: Vec<(usize, usize)>) -> Self {
        ranges.sort_unstable();
        let mut merged: Vec<(usize, usize)> = Vec::with_capacity(ranges.len());
        for (start, end) in ranges {
            match merged.last_mut() {
                Some(last) if start <= last.1 => last.1 = last.1.max(end),
                _ => merged.push((start, end)),
            }
        }
        Self { ranges: merged }
    }
}

/// Record the dead spans `node` itself establishes. Nested conditionals are
/// reached by the caller's walk and merge into the enclosing span.
fn collect_dead_spans(node: &Node, source: &[u8], out: &mut Vec<(usize, usize)>) {
    if std::env::var_os("ZZ_MASK_OFF").is_some() {
        return;
    }
    let (own_group_dead, else_chain_dead) = match node.kind() {
        "preproc_if" => match literal_condition(node, source) {
            // `#if 0`: the if-group is skipped in every configuration. A
            // following `#elif`/`#else` selects an arm the build decides, so
            // the chain below stays live.
            Some(false) => (true, false),
            // `#if 1`: the if-group is taken in every configuration, and the
            // preprocessor skips every later group of a conditional once one
            // is taken — so the whole `#elif`/`#else` chain is dead.
            Some(true) => (false, true),
            None => return,
        },
        // `#elif 0` is false in every configuration. Its own chain below is
        // reached only when the earlier conditions failed, which is a build
        // fact, so that chain stays live. `#elif 1` proves nothing: it is
        // evaluated only if an earlier condition failed.
        "preproc_elif" if literal_condition(node, source) == Some(false) => (true, false),
        _ => return,
    };

    if std::env::var_os("ZZ_GUARD_OFF").is_none() && !terminated_by_endif(node) {
        return;
    }

    let alternative = node.child_by_field_name("alternative");
    if own_group_dead {
        let end = alternative.map_or_else(|| node.end_byte(), |alt| alt.start_byte());
        out.push((node.start_byte(), end));
    }
    if else_chain_dead && let Some(alt) = alternative {
        out.push((alt.start_byte(), node.end_byte()));
    }
}

/// True only when the parse confirms a real `#endif` closing the conditional
/// `group` belongs to.
///
/// A body that leaves a brace or paren unbalanced makes error recovery consume
/// the real `#endif` into the broken construct and insert a zero-width MISSING
/// one at end of file; the group node then reaches EOF, so its span covers
/// every later symbol and reference in the file. `#endif` absent entirely lands
/// in the same shape. Masking either would be silent data loss, which is worse
/// than the bounded over-count the masking removes — so any shape that cannot
/// be confirmed terminated is left live.
fn terminated_by_endif(group: &Node) -> bool {
    // `#endif` closes the whole chain, so it is a child of the opening
    // `#if`/`#ifdef`, never of the `#elif` that named the dead arm. Walking up
    // re-descends from the root, but only for a literal-false `#elif`, and the
    // chain is as short as the directives the author wrote.
    let mut opener = *group;
    while matches!(
        opener.kind(),
        "preproc_elif" | "preproc_elifdef" | "preproc_else"
    ) {
        match opener.parent() {
            Some(parent) => opener = parent,
            None => return false,
        }
    }
    let mut cursor = opener.walk();
    if !cursor.goto_last_child() {
        return false;
    }
    let last = cursor.node();
    last.kind() == "#endif" && !last.is_missing()
}

/// `Some(false)` for `#if 0`, `Some(true)` for `#if 1`, `None` for everything
/// else. `0L`, `0x0`, `00` and `!0` are left live on purpose: the rule covers
/// only the two spellings a reader can confirm at a glance, and erring live
/// keeps the pre-existing behavior instead of risking an under-count.
fn literal_condition(group: &Node, source: &[u8]) -> Option<bool> {
    let mut condition = group.child_by_field_name("condition")?;
    // A parenthesized constant is the same constant.
    while condition.kind() == "parenthesized_expression" {
        condition = condition.named_child(0)?;
    }
    if condition.kind() != "number_literal" {
        return None;
    }
    match condition.utf8_text(source).ok()? {
        "0" => Some(false),
        "1" => Some(true),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::parse_file;

    fn dead_ranges(src: &str, language: Language) -> Vec<(usize, usize)> {
        let tree = parse_file(src.as_bytes(), language).unwrap();
        DeadRegions::scan(tree.root_node(), src.as_bytes(), language).ranges
    }

    #[test]
    fn non_c_languages_scan_to_nothing() {
        assert!(dead_ranges("fn main() {}\n", Language::Rust).is_empty());
    }

    #[test]
    fn live_conditionals_record_no_dead_span() {
        let src = "void d(void){\n#ifdef X\na();\n#elif Y\nb();\n#else\nc();\n#endif\n}\n";
        assert!(dead_ranges(src, Language::C).is_empty());
        let expr = "void d(void){\n#if defined(X) && Y > 2\na();\n#endif\n}\n";
        assert!(dead_ranges(expr, Language::C).is_empty());
    }

    #[test]
    fn nested_dead_spans_merge_into_one_range() {
        let src = "#if 0\n#ifdef Q\n#if 0\n#endif\n#endif\n#endif\n";
        let ranges = dead_ranges(src, Language::C);
        assert_eq!(ranges.len(), 1, "nested spans should merge: {ranges:?}");
        // The outer node ends with the last `#endif`, before its newline.
        assert_eq!(ranges[0], (0, src.trim_end().len()));
    }

    #[test]
    fn parenthesized_zero_is_dead_but_suffixed_zero_is_live() {
        assert_eq!(dead_ranges("#if (0)\n#endif\n", Language::C).len(), 1);
        assert!(dead_ranges("#if 0L\n#endif\n", Language::C).is_empty());
        assert!(dead_ranges("#if 00\n#endif\n", Language::C).is_empty());
    }

    #[test]
    fn a_conditional_without_a_confirmed_endif_records_no_dead_span() {
        // Each body unbalances a brace or paren, so error recovery eats the
        // real `#endif` and inserts a zero-width MISSING one at EOF.
        for (name, src, language) in [
            (
                "c brace",
                "#if 0\nstruct S {\n#endif\nint x;\n};\nvoid live(void){ live_t(); }\n",
                Language::C,
            ),
            (
                "c paren",
                "#if 0\nvoid f(\n#endif\nint x);\nvoid live(void){ live_t(); }\n",
                Language::C,
            ),
            ("c no endif", "#if 0\nvoid dead(void){}\n", Language::C),
            (
                "c elif",
                "#if FOO\nvoid a(void){}\n#elif 0\nstruct S {\n#endif\nint x; };\n",
                Language::C,
            ),
            (
                "c if one else",
                "#if 1\nvoid a(void){}\n#else\nstruct S {\n#endif\nint x; };\n",
                Language::C,
            ),
            (
                "cpp brace",
                "#if 0\nclass C {\n#endif\nint x;\n};\nvoid live(){ live_t(); }\n",
                Language::Cpp,
            ),
            (
                "cpp no endif",
                "#if 0\nclass C { int x; };\n",
                Language::Cpp,
            ),
        ] {
            let ranges = dead_ranges(src, language);
            assert!(ranges.is_empty(), "{name} should mask nothing: {ranges:?}");
        }
    }

    #[test]
    fn a_damaged_body_that_still_reaches_its_endif_stays_dead() {
        // Balanced-but-unparseable bodies keep the real `#endif`, so the span
        // ends where the author wrote it and the group is provably skipped.
        for (name, src) in [
            ("prose", "#if 0\nthis is prose not code\n#endif\n"),
            ("unterminated string", "#if 0\nchar *s = \"oops;\n#endif\n"),
            ("stray close brace", "#if 0\n}\n#endif\n"),
            ("partial declaration", "#if 0\nint\n#endif\n"),
            ("nested ifdef", "#if 0\n#ifdef Q\na();\n#endif\n#endif\n"),
            ("nested if one", "#if 0\n#if 1\na();\n#endif\n#endif\n"),
            ("directive spacing", "#  if 0\na();\n#  endif\n"),
            ("trailing comment", "#if 0 /* why */\na();\n#endif\n"),
        ] {
            let ranges = dead_ranges(src, Language::C);
            assert_eq!(ranges.len(), 1, "{name} should stay dead: {ranges:?}");
        }
    }

    #[test]
    fn deeply_nested_c_tree_does_not_overflow_the_stack() {
        // Match a small rayon worker stack to expose recursive traversal.
        let depth = 4000;
        let mut body = String::from("1");
        for _ in 0..depth {
            body = format!("({body})");
        }
        let src = format!("int deep(void) {{ return {body}; }}\n");
        let count = std::thread::Builder::new()
            .stack_size(512 * 1024)
            .spawn(move || dead_ranges(&src, Language::C).len())
            .unwrap()
            .join()
            .unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn zz_corpus_dump() {
        let Some(dir) = std::env::var_os("ZZ_CORPUS") else {
            return;
        };
        let out = std::env::var("ZZ_OUT").unwrap();
        let mut rows: Vec<String> = Vec::new();
        let mut stack = vec![std::path::PathBuf::from(dir)];
        let mut files = 0usize;
        while let Some(d) = stack.pop() {
            for e in std::fs::read_dir(&d).unwrap() {
                let path = e.unwrap().path();
                if path.is_dir() {
                    stack.push(path);
                    continue;
                }
                let lang = match path.extension().and_then(|e| e.to_str()) {
                    Some("c") => Language::C,
                    Some("h" | "hpp" | "hh" | "hxx" | "cpp" | "cc" | "cxx") => Language::Cpp,
                    _ => continue,
                };
                let Ok(bytes) = std::fs::read(&path) else {
                    continue;
                };
                let Ok(tree) = parse_file(&bytes, lang) else {
                    continue;
                };
                let rel = path.to_string_lossy().into_owned();
                files += 1;
                for s in crate::extract::extract_symbols(&tree, &bytes, lang, &rel).unwrap() {
                    rows.push(format!(
                        "S\t{rel}\t{}\t{:?}\t{}",
                        s.line_start, s.kind, s.name
                    ));
                }
                for r in crate::references::extract_references(&tree, &bytes, lang, &rel).unwrap() {
                    rows.push(format!("R\t{rel}\t{}\t{:?}\t{}", r.line, r.kind, r.to_name));
                }
            }
        }
        rows.sort();
        std::fs::write(&out, format!("# files={files}\n") + &rows.join("\n")).unwrap();
    }
}
