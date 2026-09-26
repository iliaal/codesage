//! Blank C++ export/visibility macros before tree-sitter sees them.
//!
//! `class MYLIB_API Widget {}` is valid C++ once the preprocessor has run, but
//! tree-sitter parses the raw text: it takes `MYLIB_API` as the class name and
//! recovers `Widget` as an error, so the class and every member declared in
//! it vanish from the index. CodeSage never runs the preprocessor, so the
//! macro token (and a parenthesized argument list such as
//! `MYLIB_DEPRECATED("use x")`) is overwritten instead: with spaces in a class
//! head, and with a same-length `[[a   ]]` attribute at the start of a
//! declaration (see `Form`). Only bytes other than `\n` / `\r` change, so
//! every byte offset, line, and column the tree reports stays valid for the
//! original source, which is what every extractor reads node text from.
//!
//! A token is blanked only when it is macro-shaped and sits where a
//! declaration head can take an attribute, never where a value can appear.

/// Name suffixes of the export-macro convention (`CORE_API`, `Q_DECL_EXPORT`,
/// CMake `generate_export_header`'s `FOO_EXPORT` / `FOO_DEPRECATED_EXPORT`).
const SUFFIXES: [&[u8]; 5] = [
    b"_API",
    b"_EXPORT",
    b"_IMPORT",
    b"_DLLEXPORT",
    b"_DEPRECATED",
];

/// Longest argument list a macro may carry (`FOO_DEPRECATED("...")`); a
/// longer parenthesized run is not treated as macro arguments.
const MAX_ARGS_BYTES: usize = 512;

const CLASS_KEYS: [&[u8]; 4] = [b"class", b"struct", b"union", b"enum"];

const SPECIFIER_KEYS: [&[u8]; 8] = [
    b"static",
    b"inline",
    b"virtual",
    b"explicit",
    b"friend",
    b"constexpr",
    b"extern",
    b"template",
];

/// Longest run of adjacent export macros looked through to find the head;
/// bounds the per-candidate lookahead so a pathological run stays linear.
const MAX_MACRO_RUN: usize = 8;

/// Declaration-head scan window, template arguments included.
const MAX_HEAD_TOKENS: usize = 64;

const QUALIFIER_KEYS: [&[u8]; 19] = [
    b"const",
    b"volatile",
    b"static",
    b"inline",
    b"virtual",
    b"explicit",
    b"friend",
    b"constexpr",
    b"consteval",
    b"constinit",
    b"extern",
    b"mutable",
    b"typename",
    b"thread_local",
    b"register",
    b"struct",
    b"class",
    b"enum",
    b"union",
];

const PRIMITIVE_KEYS: [&[u8]; 15] = [
    b"void",
    b"bool",
    b"char",
    b"wchar_t",
    b"char8_t",
    b"char16_t",
    b"char32_t",
    b"short",
    b"int",
    b"long",
    b"float",
    b"double",
    b"signed",
    b"unsigned",
    b"auto",
];

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    Ident,
    Str,
    Other,
    /// A preprocessor directive line: a declaration may start after it.
    Directive,
    Punct(u8),
    Scope,
}

/// How a qualifying macro is overwritten.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Form {
    /// Spaces. Used in class heads, where the class node already starts at
    /// the `class` keyword, and after a specifier keyword, where the
    /// declaration already starts at that keyword. An attribute would add
    /// nothing there and breaks some heads (`friend class [[a]] W;`).
    Spaces,
    /// `[[a   ]]`: a C++11 attribute of the same length. At the start of a
    /// declaration the attribute keeps the definition node starting where
    /// the macro starts, so symbol spans, `col_start`, leading-comment
    /// rationale (which needs the comment on the row above the node), and
    /// `edit_check`'s replaced byte range all still cover the macro, as
    /// they do for any other attribute. `[[ ]]` with no name does not parse.
    Attribute,
}

#[derive(Clone, Copy, Debug)]
struct Token {
    kind: Kind,
    start: usize,
    end: usize,
}

/// Return a same-length copy of `source` with export macros blanked, or
/// `None` when nothing qualified (the common case, decided by a substring
/// scan before any tokenizing).
pub(crate) fn neutralize(source: &[u8]) -> Option<Vec<u8>> {
    if !SUFFIXES
        .iter()
        .any(|s| source.windows(s.len()).any(|w| w == *s))
    {
        return None;
    }
    let tokens = tokenize(source);
    let spans = blank_spans(&tokens, source);
    if spans.is_empty() {
        return None;
    }
    let mut out = source.to_vec();
    for (start, end, form) in spans {
        // A candidate is at least six bytes (`XX_API`), so `[[a` always
        // overwrites its own name; `]]` falls back to spaces when an argument
        // list ends on a line break, which the attribute cannot absorb.
        let attribute = form == Form::Attribute
            && end - start >= 5
            && !source[end - 2..end]
                .iter()
                .any(|b| matches!(b, b'\n' | b'\r'));
        let span = &mut out[start..end];
        for b in span.iter_mut() {
            if *b != b'\n' && *b != b'\r' {
                *b = b' ';
            }
        }
        if attribute {
            let n = span.len();
            span[..3].copy_from_slice(b"[[a");
            span[n - 2..].copy_from_slice(b"]]");
        }
    }
    Some(out)
}

fn is_candidate(text: &[u8]) -> bool {
    if !text.first().is_some_and(u8::is_ascii_uppercase)
        || !text
            .iter()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || *b == b'_')
    {
        return false;
    }
    SUFFIXES
        .iter()
        .any(|s| text.len() >= s.len() + 2 && text.ends_with(s))
}

fn text<'a>(t: &Token, source: &'a [u8]) -> &'a [u8] {
    &source[t.start..t.end]
}

fn ident_is(t: Option<&Token>, source: &[u8], words: &[&[u8]]) -> bool {
    t.is_some_and(|t| t.kind == Kind::Ident && words.contains(&text(t, source)))
}

/// Index just past a candidate's optional `( ... )` argument list, or `None`
/// when the list is unbalanced or longer than [`MAX_ARGS_BYTES`].
fn after_args(tokens: &[Token], i: usize) -> Option<usize> {
    let open = i + 1;
    if tokens.get(open).map(|t| t.kind) != Some(Kind::Punct(b'(')) {
        return Some(open);
    }
    let limit = tokens[open].start + MAX_ARGS_BYTES;
    let mut depth = 0usize;
    for (j, t) in tokens.iter().enumerate().skip(open) {
        if t.start > limit {
            return None;
        }
        match t.kind {
            Kind::Punct(b'(') => depth += 1,
            Kind::Punct(b')') => {
                depth -= 1;
                if depth == 0 {
                    return Some(j + 1);
                }
            }
            Kind::Punct(b';' | b'{' | b'}') | Kind::Directive => return None,
            _ => {}
        }
    }
    None
}

/// Skip a run of macro-shaped tokens (with arguments) starting at `j`,
/// at most [`MAX_MACRO_RUN`] of them.
fn skip_candidates(tokens: &[Token], source: &[u8], mut j: usize) -> usize {
    for _ in 0..MAX_MACRO_RUN {
        let Some(t) = tokens.get(j) else {
            break;
        };
        if t.kind != Kind::Ident || !is_candidate(text(t, source)) {
            break;
        }
        match after_args(tokens, j) {
            Some(next) => j = next,
            None => break,
        }
    }
    j
}

fn blank_spans(tokens: &[Token], source: &[u8]) -> Vec<(usize, usize, Form)> {
    let mut spans = Vec::new();
    // The last token that was not itself blanked: a run of export macros
    // (`FOO_API FOO_DEPRECATED void f();`) is judged against what precedes
    // the whole run.
    let mut prev: Option<usize> = None;
    let mut i = 0;
    while i < tokens.len() {
        let t = tokens[i];
        if t.kind == Kind::Ident
            && is_candidate(text(&t, source))
            && let Some(next) = after_args(tokens, i)
            && let Some(form) = qualifies(tokens, source, prev.map(|p| &tokens[p]), prev, next)
        {
            spans.push((t.start, tokens[next - 1].end, form));
            i = next;
            continue;
        }
        prev = Some(i);
        i += 1;
    }
    spans
}

fn qualifies(
    tokens: &[Token],
    source: &[u8],
    prev: Option<&Token>,
    prev_idx: Option<usize>,
    next: usize,
) -> Option<Form> {
    let head = skip_candidates(tokens, source, next);
    let first = tokens.get(head)?;
    let after = tokens.get(head + 1).map(|t| t.kind);

    // `class MACRO Name {` / `: base` / `final` / `<args>` / `ns::Name`.
    // Anything else after the name (`struct MY_API value;`) means the
    // macro-shaped word is itself the type name, and so does a "name" that
    // is really the `final` specifier (`class RENDER_API final {`).
    if ident_is(prev, source, &CLASS_KEYS) {
        let named = first.kind == Kind::Ident
            && !ident_is(Some(first), source, &[b"final"])
            && (matches!(after, Some(Kind::Punct(b'{' | b':' | b'<') | Kind::Scope))
                || ident_is(tokens.get(head + 1), source, &[b"final"]));
        return named.then_some(Form::Spaces);
    }

    let declaration_start = match prev.map(|t| t.kind) {
        None | Some(Kind::Directive) => true,
        Some(Kind::Punct(b';' | b'{' | b'}' | b':' | b'>' | b']')) => true,
        Some(Kind::Ident) => ident_is(prev, source, &SPECIFIER_KEYS),
        // `extern "C" MACRO int f();`
        Some(Kind::Str) => prev_idx
            .and_then(|p| p.checked_sub(1))
            .is_some_and(|p| ident_is(tokens.get(p), source, &[b"extern"])),
        _ => false,
    };
    if !declaration_start {
        return None;
    }
    let form = if prev.map(|t| t.kind) == Some(Kind::Ident) {
        Form::Spaces
    } else {
        Form::Attribute
    };
    let shaped = match first.kind {
        Kind::Punct(b'~') => true,
        // `MACRO type name ...`: a following `;`, `=`, `,`, `[`, `)`, brace,
        // or bit-field colon means `MACRO type` was itself a declaration of
        // `type`, so the macro-shaped word is a type name, not an attribute.
        Kind::Ident => {
            !matches!(
                after,
                None | Some(Kind::Punct(
                    b';' | b'=' | b',' | b'[' | b')' | b'{' | b'}' | b':'
                ))
            ) && declarator_head(tokens, source, head)
        }
        _ => false,
    };
    shaped.then_some(form)
}

/// What blanking leaves must be one declaration head: a type and a
/// declarator name (`int f(`, `const Foo& get(`, `A& operator=(`), or a bare
/// name opening a parameter list (a constructor, `Foo(int)`), or a
/// destructor. A head that keeps a second type is refused:
/// `ZEND_API void ZEND_FASTCALL f()` blanked to `void ZEND_FASTCALL f()`
/// reads the calling-convention macro as a type, which tree-sitter-cpp
/// recovers worse than the original run. So is a head with no declarator
/// name: in `MY_TYPE_API const x;` the macro-shaped word is the type.
fn declarator_head(tokens: &[Token], source: &[u8], head: usize) -> bool {
    let mut idents = 0usize;
    let mut primitive = false;
    let mut after_scope = false;
    let mut j = head;
    let stop = loop {
        if j >= head + MAX_HEAD_TOKENS {
            return false;
        }
        let Some(t) = tokens.get(j) else {
            return false;
        };
        match t.kind {
            Kind::Ident => {
                let word = text(t, source);
                if word == b"operator" {
                    idents += 1;
                    break Kind::Punct(b'(');
                } else if PRIMITIVE_KEYS.contains(&word) {
                    primitive = true;
                } else if !QUALIFIER_KEYS.contains(&word) && !after_scope {
                    idents += 1;
                }
                after_scope = false;
            }
            Kind::Scope => after_scope = true,
            Kind::Punct(b'~') => return idents + usize::from(primitive) == 0,
            Kind::Punct(b'*' | b'&') => {}
            Kind::Punct(b'<') => {
                let mut depth = 0usize;
                while let Some(t) = tokens.get(j) {
                    match t.kind {
                        Kind::Punct(b'<') => depth += 1,
                        Kind::Punct(b'>') => depth -= 1,
                        Kind::Punct(b';' | b'{' | b'}') => return false,
                        _ => {}
                    }
                    if depth == 0 || j >= head + MAX_HEAD_TOKENS {
                        break;
                    }
                    j += 1;
                }
                if depth != 0 {
                    return false;
                }
            }
            kind => break kind,
        }
        j += 1;
    };
    match idents + usize::from(primitive) {
        2 => true,
        1 => stop == Kind::Punct(b'('),
        _ => false,
    }
}

fn tokenize(src: &[u8]) -> Vec<Token> {
    let mut tokens = Vec::new();
    let mut i = 0;
    let mut line_start = true;
    while i < src.len() {
        let b = src[i];
        if b == b'\n' {
            line_start = true;
            i += 1;
            continue;
        }
        if b.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        let start = i;
        if b == b'/' && src.get(i + 1) == Some(&b'/') {
            i = skip_line(src, i);
            continue;
        }
        if b == b'/' && src.get(i + 1) == Some(&b'*') {
            i = skip_block_comment(src, i);
            continue;
        }
        let at_line_start = std::mem::replace(&mut line_start, false);
        let kind = if b == b'#' && at_line_start {
            i = skip_directive(src, i);
            Kind::Directive
        } else if b == b'"' {
            i = skip_quoted(src, i, b'"');
            Kind::Str
        } else if b == b'\'' {
            i = skip_quoted(src, i, b'\'');
            Kind::Str
        } else if b.is_ascii_digit()
            || (b == b'.' && src.get(i + 1).is_some_and(u8::is_ascii_digit))
        {
            i = skip_number(src, i);
            Kind::Other
        } else if b.is_ascii_alphabetic() || b == b'_' || b >= 0x80 {
            while i < src.len()
                && (src[i].is_ascii_alphanumeric() || src[i] == b'_' || src[i] >= 0x80)
            {
                i += 1;
            }
            if src.get(i) == Some(&b'"')
                && matches!(&src[start..i], b"R" | b"LR" | b"uR" | b"UR" | b"u8R")
            {
                i = skip_raw_string(src, i);
                Kind::Str
            } else {
                Kind::Ident
            }
        } else if b == b':' && src.get(i + 1) == Some(&b':') {
            i += 2;
            Kind::Scope
        } else {
            i += 1;
            Kind::Punct(b)
        };
        tokens.push(Token {
            kind,
            start,
            end: i,
        });
    }
    tokens
}

fn skip_line(src: &[u8], mut i: usize) -> usize {
    while i < src.len() && src[i] != b'\n' {
        i += 1;
    }
    i
}

fn skip_block_comment(src: &[u8], i: usize) -> usize {
    src[i + 2..]
        .windows(2)
        .position(|w| w == b"*/")
        .map_or(src.len(), |p| i + 2 + p + 2)
}

/// A directive runs to the first newline not escaped by a trailing
/// backslash; a block comment inside it may span lines.
fn skip_directive(src: &[u8], mut i: usize) -> usize {
    while i < src.len() {
        match src[i] {
            b'\n' => return i,
            b'\\' if src.get(i + 1) == Some(&b'\n') => i += 2,
            b'\\' if src.get(i + 1) == Some(&b'\r') && src.get(i + 2) == Some(&b'\n') => i += 3,
            b'/' if src.get(i + 1) == Some(&b'*') => i = skip_block_comment(src, i),
            b'/' if src.get(i + 1) == Some(&b'/') => return skip_line(src, i),
            _ => i += 1,
        }
    }
    i
}

/// An unterminated literal stops at the end of its line, as the compiler's
/// lexer would report it, so one stray quote cannot swallow the file.
fn skip_quoted(src: &[u8], mut i: usize, quote: u8) -> usize {
    i += 1;
    while i < src.len() {
        match src[i] {
            b'\\' => i += 2,
            b'\n' => return i,
            c if c == quote => return i + 1,
            _ => i += 1,
        }
    }
    src.len()
}

fn skip_raw_string(src: &[u8], quote: usize) -> usize {
    let delim_start = quote + 1;
    let Some(open) = src[delim_start..].iter().take(17).position(|&b| b == b'(') else {
        return skip_quoted(src, quote, b'"');
    };
    let delim = &src[delim_start..delim_start + open];
    let body = delim_start + open + 1;
    let mut close = Vec::with_capacity(delim.len() + 2);
    close.push(b')');
    close.extend_from_slice(delim);
    close.push(b'"');
    src[body..]
        .windows(close.len())
        .position(|w| w == close.as_slice())
        .map_or(src.len(), |p| body + p + close.len())
}

/// Numbers include digit separators (`1'000`) and exponent signs, so the
/// `'` inside one never opens a character literal.
fn skip_number(src: &[u8], mut i: usize) -> usize {
    while i < src.len() {
        let c = src[i];
        let exponent_sign =
            (c == b'+' || c == b'-') && matches!(src[i - 1], b'e' | b'E' | b'p' | b'P');
        if !(c.is_ascii_alphanumeric() || matches!(c, b'_' | b'.' | b'\'') || exponent_sign) {
            break;
        }
        i += 1;
    }
    i
}

#[cfg(test)]
mod tests {
    use super::neutralize;

    fn blanked(src: &str) -> String {
        let out = neutralize(src.as_bytes())
            .map_or_else(|| src.to_string(), |b| String::from_utf8(b).unwrap());
        assert_eq!(out.len(), src.len());
        let newlines = |s: &str| s.match_indices('\n').map(|(i, _)| i).collect::<Vec<_>>();
        assert_eq!(newlines(&out), newlines(src));
        out
    }

    /// `src` with each listed macro text overwritten, newlines kept. A
    /// leading `@` marks the attribute form (`[[a` + spaces + `]]`).
    fn by_hand(src: &str, macros: &[&str]) -> String {
        let mut out = src.to_string();
        for m in macros {
            let (text, attribute) = match m.strip_prefix('@') {
                Some(text) => (text, true),
                None => (*m, false),
            };
            let mut blank: Vec<u8> = text
                .bytes()
                .map(|c| if c == b'\n' { c } else { b' ' })
                .collect();
            if attribute {
                let n = blank.len();
                blank[..3].copy_from_slice(b"[[a");
                blank[n - 2..].copy_from_slice(b"]]");
            }
            out = out.replacen(text, &String::from_utf8(blank).unwrap(), 1);
        }
        out
    }

    fn parses_cleanly(src: &str) -> bool {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&crate::parse::ts_language(codesage_protocol::Language::Cpp))
            .unwrap();
        !parser.parse(src, None).unwrap().root_node().has_error()
    }

    #[test]
    fn class_heads_and_declaration_specifiers_are_blanked() {
        for (src, macros) in [
            ("class MYLIB_API Widget {};", &["MYLIB_API"][..]),
            ("struct Q_DECL_EXPORT P : B {};", &["Q_DECL_EXPORT"]),
            ("class FOO_API final_t final {};", &["FOO_API"]),
            ("template <> class FOO_API Box<int> {};", &["FOO_API"]),
            ("friend class FOO_API W;", &[][..]),
            ("MYLIB_API int f(int);", &["@MYLIB_API"]),
            ("MYLIB_API\nint g(int a) { return a; }", &["@MYLIB_API"]),
            ("MYLIB_API std::string name();", &["@MYLIB_API"]),
            ("extern \"C\" CORE_API void g();", &["@CORE_API"]),
            (
                "template <typename T> ENGINE_API T get() { return T(); }",
                &["@ENGINE_API"],
            ),
            ("template <> ENGINE_API int get<int>();", &["@ENGINE_API"]),
            (
                "class A { public: LIB_API ~A(); LIB_API static A make(); };",
                &["@LIB_API", "@LIB_API"],
            ),
            ("static LIB_API int local() { return 0; }", &["LIB_API"]),
            (
                "FOO_API FOO_DEPRECATED void h();",
                &["@FOO_API", "@FOO_DEPRECATED"],
            ),
            (
                "class MYLIB_API MYLIB_DEPRECATED W {};",
                &["MYLIB_API", "MYLIB_DEPRECATED"],
            ),
            (
                "#include \"x.h\"\nclass FOO_DEPRECATED(\"a\\\"b\",\n  2) W {};",
                &["FOO_DEPRECATED(\"a\\\"b\",\n  2)"],
            ),
            (
                "FOO_DEPRECATED(\"a\",\n  2) void f();",
                &["@FOO_DEPRECATED(\"a\",\n  2)"],
            ),
            (
                "FOO_DEPRECATED(\"a\"\n) void f();",
                &["FOO_DEPRECATED(\"a\"\n)"],
            ),
            ("int n = 1'000; class MYLIB_API W {};", &["MYLIB_API"]),
            ("char q = '\\''; class MYLIB_API W {};", &["MYLIB_API"]),
            (
                "auto r = R\"x(\")x\"; class MYLIB_API W {};",
                &["MYLIB_API"],
            ),
            ("/* a */ class MYLIB_API W {};", &["MYLIB_API"]),
            (
                "MYLIB_API const std::vector<std::pair<int, int>>& all() const;",
                &["@MYLIB_API"],
            ),
            ("MYLIB_API unsigned long long count();", &["@MYLIB_API"]),
            (
                "MYLIB_API bool operator==(const A&, const A&);",
                &["@MYLIB_API"],
            ),
            (
                "class A { LIB_API A(int); LIB_API virtual ~A(); };",
                &["@LIB_API", "@LIB_API"],
            ),
            ("CORE_API ns::Foo::Foo() {}", &["@CORE_API"]),
        ] {
            let expected = by_hand(src, macros);
            let out = blanked(src);
            assert_eq!(out, expected, "{src:?}");
            if !macros.is_empty() {
                assert_ne!(expected, src, "fixture {src:?} names no macro text");
                assert!(parses_cleanly(&out), "{out:?} does not parse cleanly");
            }
        }
    }

    #[test]
    fn values_type_names_directives_comments_and_strings_are_untouched() {
        for src in [
            "#define MYLIB_API __attribute__((visibility(\"default\")))\n",
            "#define WRAP(x) \\\n  class MYLIB_API x {}\n",
            "struct MY_API { int x; };",
            "struct MY_API value;",
            "struct NET_EXPORT *handle;",
            "enum E { FOO_API, BAR_EXPORT = 2 };",
            "int v = LIB_EXPORT;",
            "void f() { CONFIG_DEPRECATED(\"m\"); return LIB_EXPORT; }",
            "void f() { if (a > FOO_API) {} }",
            "MY_TYPE_API x;",
            "MY_TYPE_API x = 1;",
            "// class MYLIB_API Widget {};",
            "/* class MYLIB_API Widget {}; */",
            "const char *s = \"class MYLIB_API Widget {};\";",
            "auto r = R\"x(class MYLIB_API W {};)x\";",
            "class X_API W {};",
            "class Lib_Api W {};",
            "int n = 1'000; class MYLIB_API",
            "class Beta {}; MYLIB_API;",
            "ZEND_API void ZEND_FASTCALL f(int x) {}",
            "ZEND_API ZEND_COLD void g(int x) {}",
            "MYLIB_API BOOL WINAPI h();",
            "class RENDER_API final { void m(); };",
            "class RENDER_API final : public Base {};",
            "MY_TYPE_API const x;",
            "MY_TYPE_API const x = 1;",
            "MY_TYPE_API volatile y;",
            "MY_TYPE_API const *p;",
        ] {
            assert_eq!(blanked(src), src, "{src:?} was modified");
            assert!(neutralize(src.as_bytes()).is_none(), "{src:?}");
        }
    }

    #[test]
    fn unbalanced_or_oversized_arguments_are_left_alone() {
        let long = format!("class FOO_DEPRECATED(\"{}\") W {{}};", "x".repeat(600));
        assert_eq!(blanked(&long), long);
        let open = "class FOO_DEPRECATED( W {};";
        assert_eq!(blanked(open), open);
    }
}
