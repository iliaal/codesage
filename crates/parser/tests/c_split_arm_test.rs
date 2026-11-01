use codesage_parser::{
    extract::extract_symbols, parse::parse_file, references::extract_references,
};
use codesage_protocol::{Language, SymbolKind};

const SPLIT_HEADERS: &str = "void f1(int n)
{
#ifdef X
    for (int i = 0; i < n; i++) {
#else
    for (int i = n; i > 0; i--) {
#endif
        b2(n);
    }
}

void s1(int n)
{
#if defined(X)
    switch (n) {
#elif defined(Y)
    switch (n + 2) {
#else
    switch (n + 1) {
#endif
    case 1:
        b3(n);
        break;
    }
}

void ws(int n)
{
#ifdef X
    while (n > 0) {
#else
    while (n > 1) {
#endif
        b4(n);
    }
}

void after(void) { b5(); }
";

fn callables(source: &str, language: Language, path: &str) -> Vec<(String, u32, u32)> {
    let tree = parse_file(source.as_bytes(), language).unwrap();
    extract_symbols(&tree, source.as_bytes(), language, path)
        .unwrap()
        .into_iter()
        .filter(|s| matches!(s.kind, SymbolKind::Function | SymbolKind::Method))
        .map(|s| (s.name, s.line_start, s.line_end))
        .collect()
}

fn calls(source: &str, language: Language, path: &str) -> Vec<(String, u32)> {
    let tree = parse_file(source.as_bytes(), language).unwrap();
    extract_references(&tree, source.as_bytes(), language, path)
        .unwrap()
        .into_iter()
        .filter(|r| r.kind == codesage_protocol::ReferenceKind::Call)
        .map(|r| (r.to_name, r.line))
        .collect()
}

fn owned(rows: &[(&str, u32, u32)]) -> Vec<(String, u32, u32)> {
    rows.iter()
        .map(|&(name, start, end)| (name.to_string(), start, end))
        .collect()
}

#[test]
fn loop_and_switch_headers_split_across_arms_keep_their_function() {
    for (language, path) in [(Language::C, "split.c"), (Language::Cpp, "split.cpp")] {
        assert_eq!(
            callables(SPLIT_HEADERS, language, path),
            owned(&[
                ("f1", 1, 10),
                ("s1", 12, 25),
                ("ws", 27, 36),
                ("after", 38, 38)
            ]),
            "{language:?}"
        );
        assert_eq!(
            calls(SPLIT_HEADERS, language, path),
            [
                ("b2".to_string(), 8),
                ("b3".to_string(), 22),
                ("b4".to_string(), 34),
                ("b5".to_string(), 38),
            ],
            "{language:?}"
        );
    }
}

/// At file scope the arms are alternative declaration heads; keeping one would
/// choose which name is indexed, so the group parses as written.
#[test]
fn file_scope_split_function_header_parses_as_written() {
    let source = "#ifdef WIDE
static int pick(long a)
{
#else
static int pick(int a)
{
#endif
    return c1(a);
}
";
    assert_eq!(
        callables(source, Language::C, "pick.c"),
        owned(&[("pick", 5, 9)])
    );
}

#[test]
fn literal_zero_first_arm_yields_to_the_else_arm() {
    let source = "void z(int n)
{
#if 0
    while (n > 0) { old(n);
#else
    while (n > 1) { live(n);
#endif
    }
}
";
    assert_eq!(callables(source, Language::C, "z.c"), owned(&[("z", 1, 9)]));
    assert_eq!(calls(source, Language::C, "z.c"), [("live".to_string(), 6)]);
}

#[test]
fn dangling_else_constexpr_arm_joins_its_if() {
    let source = "void k1(int x)
{
    if (x) {
        a(x);
    }
#if defined(A)
    else if constexpr (X > 2) {
        b(x);
    }
#endif
}

void k2(void) { c(); }
";
    assert_eq!(
        callables(source, Language::Cpp, "k.cpp"),
        owned(&[("k1", 1, 11), ("k2", 13, 13)])
    );
    assert_eq!(
        calls(source, Language::Cpp, "k.cpp"),
        [
            ("a".to_string(), 4),
            ("b".to_string(), 8),
            ("c".to_string(), 13)
        ]
    );
}

/// Balanced arms are parsed exactly as written: each arm's definition stays
/// a separate symbol and both arms' calls stay indexed.
#[test]
fn balanced_arms_keep_every_alternative() {
    let source = "#ifdef X
int pick(int n)
{
    return a1(n);
}
#else
int pick(int n)
{
    return a2(n);
}
#endif

void body(int n)
{
#ifdef X
    a3(n);
#else
    a4(n);
#endif
}
";
    for (language, path) in [(Language::C, "balanced.c"), (Language::Cpp, "balanced.cpp")] {
        assert_eq!(
            callables(source, language, path),
            owned(&[("pick", 2, 5), ("pick", 7, 10), ("body", 13, 20)]),
            "{language:?}"
        );
        assert_eq!(
            calls(source, language, path),
            [
                ("a1".to_string(), 4),
                ("a2".to_string(), 9),
                ("a3".to_string(), 16),
                ("a4".to_string(), 18),
            ],
            "{language:?}"
        );
    }
}

/// Arms that leave different depths cannot be reduced to one arm without
/// unbalancing the file, and a lone arm that opens a brace is closed by a
/// group elsewhere; both parse as written.
#[test]
fn unequal_and_lone_opening_arms_are_left_as_written() {
    let source = "#ifdef __cplusplus
extern \"C\" {
#endif
int g(void) { return h(); }
#ifdef __cplusplus
}
#endif
";
    assert_eq!(
        callables(source, Language::Cpp, "g.h"),
        owned(&[("g", 4, 4)])
    );
}

/// The arms the parser does not keep stay in place as complete statements,
/// so a call written in any live arm is still indexed.
#[test]
fn calls_in_every_live_arm_stay_indexed() {
    let source = "void g(int x)
{
#ifdef SLOW
    while ((x = next_a(x)) != 0) {
#else
    while ((x = next_b(x)) != 0) {
#endif
        body(x);
    }
    if (x) {
        one(x);
#ifdef WIN
    } else if (win(x)) {
        two(x);
#else
    } else {
        three(x);
#endif
    }
}

void h(void) { tail(); }
";
    for (language, path) in [(Language::C, "g.c"), (Language::Cpp, "g.cpp")] {
        assert_eq!(
            callables(source, language, path),
            owned(&[("g", 1, 20), ("h", 22, 22)]),
            "{language:?}"
        );
        assert_eq!(
            calls(source, language, path),
            [
                ("next_a".to_string(), 4),
                ("next_b".to_string(), 6),
                ("body".to_string(), 8),
                ("one".to_string(), 11),
                ("win".to_string(), 13),
                ("two".to_string(), 14),
                ("three".to_string(), 17),
                ("tail".to_string(), 22),
            ],
            "{language:?}"
        );
    }
}

/// A split group nested in an arm of another split group: the inner group is
/// reduced first and the outer arm still keeps its calls.
#[test]
fn nested_split_groups_keep_every_live_call() {
    let source = "int put(int p)
{
#ifdef HAVE_UNSETENV
    if (!p || setv(p) == 0) {
#else
# ifndef WIN
    if (setv(p) == 0) {
# else
    if (!wide(p)) {
        bail(p);
    }
    if (setw(p) != 0
#  ifndef ZTS
        && wput(p) == 0
#  endif
    ) {
# endif
#endif
        store(p);
    }
    return p;
}

void next(void) { tail(); }
";
    assert_eq!(
        callables(source, Language::C, "put.c"),
        owned(&[("put", 1, 22), ("next", 24, 24)])
    );
    assert_eq!(
        calls(source, Language::C, "put.c"),
        [
            ("setv".to_string(), 4),
            ("setv".to_string(), 7),
            ("wide".to_string(), 9),
            ("bail".to_string(), 10),
            ("setw".to_string(), 12),
            ("wput".to_string(), 14),
            ("store".to_string(), 19),
            ("tail".to_string(), 24),
        ]
    );
}

/// An arm that leaves a parenthesis open is part of an expression; it is not
/// spliced into the kept arm's condition as a stray statement.
#[test]
fn split_condition_arm_does_not_become_a_call() {
    let source = "int r(int j1, int dig)
{
    if (j1 > 0) {
#ifdef BIASED
        if (j1 >= 0
#else
        if ((j1 > 0 || (j1 == 0 && odd(dig)))
#endif
            && dig++ == '9')
            up(dig);
    }
    return dig;
}
";
    assert_eq!(
        callables(source, Language::C, "r.c"),
        owned(&[("r", 1, 13)])
    );
    assert_eq!(calls(source, Language::C, "r.c"), [("up".to_string(), 10)]);
}

/// Joining a lone `else` arm to its `if` also recovers calls in the arm's
/// own condition, which a recovered phantom definition used to swallow.
#[test]
fn call_in_a_joined_else_if_condition_is_indexed() {
    let source = "void handle(int id)
{
    if (id == 1) {
        a(id);
    }
#if defined(USE_V)
    else if (foo(id)) {
        b(id);
    }
#endif
}
";
    for (language, path) in [(Language::C, "cond.c"), (Language::Cpp, "cond.cpp")] {
        assert_eq!(
            callables(source, language, path),
            owned(&[("handle", 1, 11)]),
            "{language:?}"
        );
        assert_eq!(
            calls(source, language, path),
            [
                ("a".to_string(), 4),
                ("foo".to_string(), 7),
                ("b".to_string(), 8)
            ],
            "{language:?}"
        );
    }
}

/// Alternative function heads at namespace scope are declarations, not
/// statements in a body: both stay indexed, as without the pre-pass.
#[test]
fn split_heads_at_namespace_scope_parse_as_written() {
    let source = "namespace ns {
#ifdef WIDE
int pick_wide(long a)
{
#else
int pick(int a)
{
#endif
    return c1(a);
}
}
";
    assert_eq!(
        callables(source, Language::Cpp, "ns.cpp"),
        owned(&[("pick_wide", 3, 11), ("pick", 6, 10)])
    );
}

/// The arms around a function-try-block close the function body itself
/// (`}` / `} catch (...) { }`); reducing them would leave the handler at
/// namespace scope as a function named `catch`.
#[test]
fn function_try_block_arms_parse_as_written() {
    let plain = "#ifdef NO_EXC
bool parse(int s) {
#else
bool parse(int s) try {
#endif
  return check(s);
#ifdef NO_EXC
}
#else
} catch (...) { return fallback(); }
#endif

int after() { return c2(); }
";
    let namespaced = format!("namespace ns {{\n{plain}}}\n");
    assert_eq!(
        callables(plain, Language::Cpp, "ftry.cpp"),
        owned(&[("parse", 4, 10), ("after", 13, 13)])
    );
    assert_eq!(
        callables(&namespaced, Language::Cpp, "ftry_ns.cpp"),
        owned(&[("parse", 3, 15), ("parse", 5, 11), ("after", 14, 14)])
    );
    let closes_function = "bool parse(int s) {
  if (s) {
    go(s);
#ifdef A
  }
}
#else
  } else { other(); }
}
#endif
int after() { return c2(); }
";
    assert_eq!(
        callables(closes_function, Language::Cpp, "ftry2.cpp"),
        owned(&[("parse", 1, 6), ("after", 11, 11)])
    );
}

/// Inside a function nested in a namespace or `extern "C"` block, a split
/// loop header is still reduced; the block around the function is not
/// mistaken for a body.
#[test]
fn split_loop_inside_namespace_and_extern_c_functions_keeps_the_function() {
    let body = "void f1(int n)
{
#ifdef X
    for (int i = 0; i < n; i++) {
#else
    for (int i = n; i > 0; i--) {
#endif
        b2(n);
    }
}
";
    let namespaced = format!("namespace ns {{\n{body}}}\n");
    assert_eq!(
        callables(&namespaced, Language::Cpp, "ns.cpp"),
        owned(&[("f1", 2, 11)])
    );
    let extern_c = format!(
        "#ifdef __cplusplus\nextern \"C\" {{\n#endif\n{body}#ifdef __cplusplus\n}}\n#endif\n"
    );
    for (language, path) in [(Language::C, "ext.c"), (Language::Cpp, "ext.cpp")] {
        assert_eq!(
            callables(&extern_c, language, path),
            owned(&[("f1", 4, 13)]),
            "{language:?}"
        );
    }
}

/// A call in the condition of a blanked `#if` stays a reference, as it is
/// when the group is parsed as written.
#[test]
fn call_in_a_blanked_condition_stays_indexed() {
    let source = "void scatter(int t)
{
    if (t == 1) {
        constant(t);
#if !SMALLER_BINARY(flat)
    } else if (t == 2) {
        flat(t);
#endif
    } else {
        generic(t);
    }
}
";
    for (language, path) in [(Language::C, "s.c"), (Language::Cpp, "s.cpp")] {
        assert_eq!(
            callables(source, language, path),
            owned(&[("scatter", 1, 12)]),
            "{language:?}"
        );
        assert_eq!(
            calls(source, language, path),
            [
                ("constant".to_string(), 4),
                ("SMALLER_BINARY".to_string(), 5),
                ("flat".to_string(), 7),
                ("generic".to_string(), 10),
            ],
            "{language:?}"
        );
    }
}

/// A `#if` line inside a comment, string, or raw string is not a directive:
/// its calls never become references, whether or not a group was reduced.
#[test]
fn directive_lines_in_comments_and_strings_add_no_references() {
    for (path, language, source, expected) in [
        (
            "cmt.c",
            Language::C,
            "/*\n#if defined(FOO) && phantom_cmt(x)\n*/\nint f(void) { return g(); }\n",
            &[("g", 4)][..],
        ),
        (
            "raw.cpp",
            Language::Cpp,
            "const char *k = R\"(\n#if raw_phantom(2)\n)\";\nint f() { return g(); }\n",
            &[("g", 4)][..],
        ),
        (
            "cmt2.c",
            Language::C,
            "void f(int n)\n{\n#ifdef X\n    for (;;) {\n#else\n    for (;n;) {\n#endif\n        c(n);\n    }\n}\n/* Example:\n#if HAVE(phantom_feature)\n    use();\n#endif\n*/\nstatic const char *s = \"abc\\\n#if bogus_str(1)\\\n\";\n",
            &[("c", 8)][..],
        ),
    ] {
        let expected: Vec<_> = expected
            .iter()
            .map(|&(name, line)| (name.to_string(), line))
            .collect();
        assert_eq!(calls(source, language, path), expected, "{path}");
    }
}

/// Merging condition references stays linear in the reference count, and a
/// commented-out run of `#if` lines is never parsed as code.
#[test]
fn many_references_with_commented_conditions_stay_fast_and_exact() {
    let n = 50_000;
    let mut source = String::from(
        "void f(int x)\n{\n#ifdef X\n    for (;;) {\n#else\n    for (;x;) {\n#endif\n",
    );
    for _ in 0..n {
        source.push_str("a();\n");
    }
    source.push_str("    }\n}\n/*\n");
    for _ in 0..n {
        source.push_str("#if f(x)\n");
    }
    source.push_str("*/\n");
    let started = std::time::Instant::now();
    let found = calls(&source, Language::C, "dedupe.c");
    let elapsed = started.elapsed();
    assert_eq!(found.len(), n);
    assert!(found.iter().all(|(name, _)| name == "a"));
    assert!(
        elapsed < std::time::Duration::from_secs(30),
        "took {elapsed:?}"
    );

    let groups = 2_000;
    let mut source = String::from("void g(int x)\n{\n");
    for _ in 0..n {
        source.push_str("a();\n");
    }
    for _ in 0..groups {
        source.push_str("    if (x) {\n        a();\n#if FEAT(x)\n    } else if (x) {\n        b();\n#endif\n    }\n");
    }
    source.push_str("}\n");
    let started = std::time::Instant::now();
    let found = calls(&source, Language::C, "merge.c");
    let elapsed = started.elapsed();
    let count = |name: &str| found.iter().filter(|(n, _)| n == name).count();
    assert_eq!(
        (count("a"), count("FEAT"), count("b")),
        (n + groups, groups, groups)
    );
    assert!(
        elapsed < std::time::Duration::from_secs(30),
        "took {elapsed:?}"
    );
}
