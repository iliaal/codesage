//! Map a pasted stack trace onto indexed symbols.
//!
//! Rust module paths are matched against the workspace's `Cargo.toml`
//! packages under the conventional `<crate>/src/` layout; a `[lib]` or
//! `[[bin]]` target with a `path` outside `src/` is not modelled, so frames
//! from it fall back to bare-name leads.

use std::collections::HashMap;
use std::path::Path;
use std::sync::LazyLock;

use anyhow::Result;
use codesage_protocol::{
    FromTraceReport, FromTraceRequest, Language, Symbol, SymbolKind, TraceFrame, TraceFrameStatus,
    TraceSymbol,
};
use codesage_storage::Database;
use regex::Regex;

/// Order in `frames` of every report; Python and Xdebug input is reversed to
/// match.
pub const TRACE_ORDER: &str = "innermost-first";

/// A frame as printed, before any index lookup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawFrame {
    pub raw: String,
    pub function: Option<String>,
    pub file: Option<String>,
    pub line: Option<u32>,
}

/// Where a format's `file:line` points relative to the named function.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Location {
    /// Inside the named function's body (every format but PHP).
    InBody,
    /// The call site of the named function, in the caller's body. PHP's
    /// `#0 file(123): Foo->bar()` says `bar` was invoked at `file:123`, so the
    /// symbol spanning that line is the caller, not `bar`.
    CallSite,
}

/// How a multi-stack report orders its stacks so that `frames[0]` is the
/// innermost frame of the root cause.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StackOrder {
    /// The first stack printed is the root cause (Python chained tracebacks,
    /// ASan access → freed → allocated, PHP `Next` exception chains).
    AsPrinted,
    /// The last stack printed is the root cause (Java `Caused by:` chains).
    Reversed,
    /// The `[running]` goroutine first, the rest as printed.
    RunningFirst,
}

struct Format {
    name: &'static str,
    location: Location,
    /// Frames of one stack, in the order the runtime printed them.
    extract: fn(&[&str]) -> Vec<RawFrame>,
    /// Frames printed outermost-first, so each stack is reversed.
    reversed: bool,
    /// A line matching this starts a new stack; `None` means one stack.
    separator: Option<&'static LazyLock<Regex>>,
    stack_order: StackOrder,
    /// Definition languages a bare function name may be matched against.
    languages: &'static [Language],
    /// The index stores absolute fully qualified names for this language
    /// (PHP `Ns\Class\method`, Java `pkg.Class.method`), so a stored name
    /// shorter than the printed one is a different, shallower namespace,
    /// not a partial spelling of the same definition.
    qualified_names_absolute: bool,
}

/// Longest `candidates` list on a frame; `candidates_total` keeps the count.
const MAX_CANDIDATES: usize = 10;
/// Weak name-only leads get a smaller cap to leave room for other frames.
const MAX_CANDIDATES_NAME_ONLY: usize = 5;

const FORMATS: &[Format] = &[
    Format {
        name: "python",
        location: Location::InBody,
        extract: extract_python,
        reversed: true,
        separator: Some(&PY_SEP),
        stack_order: StackOrder::AsPrinted,
        languages: &[Language::Python],
        qualified_names_absolute: false,
    },
    Format {
        name: "php",
        location: Location::CallSite,
        extract: extract_php,
        reversed: false,
        separator: Some(&PHP_SEP),
        stack_order: StackOrder::AsPrinted,
        languages: &[Language::Php],
        qualified_names_absolute: true,
    },
    Format {
        name: "php-xdebug",
        location: Location::CallSite,
        extract: extract_xdebug,
        reversed: true,
        separator: None,
        stack_order: StackOrder::AsPrinted,
        languages: &[Language::Php],
        qualified_names_absolute: true,
    },
    Format {
        name: "rust",
        location: Location::InBody,
        extract: extract_rust,
        reversed: false,
        separator: Some(&RUST_SEP),
        stack_order: StackOrder::AsPrinted,
        languages: &[Language::Rust],
        qualified_names_absolute: false,
    },
    Format {
        name: "java",
        location: Location::InBody,
        extract: extract_java,
        reversed: false,
        separator: Some(&JAVA_SEP),
        stack_order: StackOrder::Reversed,
        languages: &[Language::Java],
        qualified_names_absolute: true,
    },
    Format {
        name: "go",
        location: Location::InBody,
        extract: extract_go,
        reversed: false,
        separator: Some(&GO_SEP),
        stack_order: StackOrder::RunningFirst,
        languages: &[Language::Go],
        qualified_names_absolute: false,
    },
    Format {
        name: "node",
        location: Location::InBody,
        extract: extract_node,
        reversed: false,
        separator: None,
        stack_order: StackOrder::AsPrinted,
        languages: &[Language::JavaScript, Language::TypeScript],
        qualified_names_absolute: false,
    },
    Format {
        name: "gdb-asan",
        location: Location::InBody,
        extract: extract_gdb_asan,
        reversed: false,
        separator: Some(&ASAN_SEP),
        stack_order: StackOrder::AsPrinted,
        languages: &[Language::C, Language::Cpp],
        qualified_names_absolute: false,
    },
];

macro_rules! re {
    ($name:ident, $pat:literal) => {
        static $name: LazyLock<Regex> = LazyLock::new(|| Regex::new($pat).expect("static regex"));
    };
}

// Optional Windows drive letter ahead of a path, so `C:\src\a.js:1:2` keeps
// its drive instead of splitting at the first colon.
re!(
    PY_FRAME,
    r#"^\s*File "(?P<file>[^"]+)", line (?P<line>\d+)(?:, in (?P<func>\S+))?"#
);
re!(PY_SEP, r"^\s*Traceback \(most recent call last\):");
re!(
    PHP_FRAME,
    r"^\s*#\d+\s+(?P<file>[^\s(]+)\((?P<line>\d+)\): (?P<func>[^(]+)\("
);
re!(
    PHP_INTERNAL,
    r"^\s*#\d+\s+\[internal function\]: (?P<func>[^(]+)\("
);
re!(PHP_MAIN, r"^\s*#\d+\s+\{main\}\s*$");
// `.* in ` is greedy so a message that itself contains " in " does not
// truncate the path; the last " in " is the one PHP appends.
re!(
    PHP_FATAL,
    r"^\s*(?:PHP )?(?:Fatal error|Warning|Notice|Deprecated|Parse error|Uncaught [A-Za-z\\_]+|Next [A-Za-z\\_]+).* in (?P<file>(?:[A-Za-z]:)?[^\s:(]+)(?::(?P<line1>\d+)| on line (?P<line2>\d+))"
);
re!(PHP_SEP, r"^\s*Next [A-Za-z\\_]+");
// Xdebug `Call Stack:` table: `N  time  memory  func()  file:line`, or the
// compact `N. func() file:line` form.
re!(
    XDEBUG_FRAME,
    r"^\s*(?:[\d.]+\s+\d+\s+)?\d+[.)]?\s+(?P<func>[^\s(]+)\((?:[^()]|\([^()]*\))*\)\s+(?P<file>(?:[A-Za-z]:)?[^\s:]+):(?P<line>\d+)\s*$"
);
re!(
    RUST_FRAME,
    r"^\s*(?P<n>\d+):\s+(?:0x[0-9a-fA-F]+ - )?(?P<func>\S.*?)\s*$"
);
re!(RUST_AT, r"^\s+at (?P<file>\S+?):(?P<line>\d+)(?::\d+)?\s*$");
re!(RUST_SEP, r"^thread '.*' panicked at ");
re!(
    RUST_PANIC,
    r"panicked at (?P<file>\S+?):(?P<line>\d+)(?::\d+)?:?\s*$"
);
re!(
    JAVA_FRAME,
    r"^\s*at (?P<func>[^\s(]+)\((?P<file>[^:)]+)(?::(?P<line>\d+))?\)\s*$"
);
re!(JAVA_SEP, r"^\s*(?:Caused by|Suppressed):");
re!(JAVA_SUPPRESSED, r"^\s*Suppressed:");
re!(
    GO_FUNC,
    r"^(?P<func>[A-Za-z_][\w./\-]*?(?:\.\(\*?\w+\))?(?:\.\w+)+)\((?:[^()]|\([^()]*\))*\)\s*$"
);
re!(
    GO_LOC,
    r"^\s+(?P<file>\S+\.go):(?P<line>\d+)(?:\s+\+0x[0-9a-fA-F]+)?\s*$"
);
re!(GO_SEP, r"^goroutine \d+ \[");
re!(
    NODE_FRAME,
    r"^\s*at (?:async )?(?:new )?(?:(?P<func>[^\s(]+(?: \[as \w+\])?) \()?(?P<file>(?:node:|[A-Za-z]:)?[^\s():]+):(?P<line>\d+):\d+\)?\s*$"
);
// `func` admits C++ `operator delete` / `operator()` / `operator[]`; the
// repeated paren group swallows a signature `(int, char const*)`, one level
// of nested function-pointer parens, and gdb's `(this=0x0)` argument list.
re!(
    GDB_FRAME,
    r"^\s*#\d+\s+(?:0x[0-9a-fA-F]+ in )?(?P<func>operator\s*(?:\(\)|\[\]|[^\s(]+)|[^\s(]+)(?:\s*\((?:[^()]|\([^()]*\))*\))*\s+(?:at\s+)?(?P<file>(?:[A-Za-z]:)?[^\s:]+):(?P<line>\d+)"
);
re!(
    GDB_LIB,
    r"^\s*#\d+\s+0x[0-9a-fA-F]+ in (?P<func>[^\s(]+)\s*\((?P<lib>[^)]*\+0x[0-9a-fA-F]+)\)"
);
re!(
    GDB_UNKNOWN,
    r"^\s*#\d+\s+(?:0x[0-9a-fA-F]+ in )?\?\?\s*\(\)"
);
re!(
    ASAN_SEP,
    r"^\s*(?:freed by thread|previously allocated by thread|(?:Direct|Indirect) leak of)"
);
re!(RUST_HASH, r"::h[0-9a-f]{16}$");

fn cap_u32(caps: &regex::Captures<'_>, name: &str) -> Option<u32> {
    caps.name(name).and_then(|m| m.as_str().parse().ok())
}

fn cap_str(caps: &regex::Captures<'_>, name: &str) -> Option<String> {
    caps.name(name).map(|m| m.as_str().to_string())
}

fn extract_python(lines: &[&str]) -> Vec<RawFrame> {
    lines
        .iter()
        .filter_map(|l| {
            let caps = PY_FRAME.captures(l)?;
            Some(RawFrame {
                raw: l.trim_end().to_string(),
                function: cap_str(&caps, "func"),
                file: cap_str(&caps, "file"),
                line: cap_u32(&caps, "line"),
            })
        })
        .collect()
}

fn extract_php(lines: &[&str]) -> Vec<RawFrame> {
    let mut out = Vec::new();
    for l in lines {
        if let Some(caps) = PHP_FRAME.captures(l) {
            out.push(RawFrame {
                raw: l.trim_end().to_string(),
                function: cap_str(&caps, "func").map(|f| f.trim().to_string()),
                file: cap_str(&caps, "file"),
                line: cap_u32(&caps, "line"),
            });
        } else if let Some(caps) = PHP_INTERNAL.captures(l) {
            out.push(RawFrame {
                raw: l.trim_end().to_string(),
                function: cap_str(&caps, "func").map(|f| f.trim().to_string()),
                file: None,
                line: None,
            });
        } else if PHP_MAIN.is_match(l) {
            out.push(RawFrame {
                raw: l.trim_end().to_string(),
                function: Some("{main}".to_string()),
                file: None,
                line: None,
            });
        } else if let Some(caps) = PHP_FATAL.captures(l) {
            out.push(RawFrame {
                raw: l.trim_end().to_string(),
                function: None,
                file: cap_str(&caps, "file"),
                line: cap_u32(&caps, "line1").or_else(|| cap_u32(&caps, "line2")),
            });
        }
    }
    out
}

fn extract_xdebug(lines: &[&str]) -> Vec<RawFrame> {
    lines
        .iter()
        .filter_map(|l| {
            let caps = XDEBUG_FRAME.captures(l)?;
            Some(RawFrame {
                raw: l.trim_end().to_string(),
                function: cap_str(&caps, "func"),
                file: cap_str(&caps, "file"),
                line: cap_u32(&caps, "line"),
            })
        })
        .collect()
}

fn extract_rust(lines: &[&str]) -> Vec<RawFrame> {
    let mut out: Vec<RawFrame> = Vec::new();
    let mut pending: Option<RawFrame> = None;
    let mut panic_at: Vec<usize> = Vec::new();
    for l in lines {
        if let Some(caps) = RUST_FRAME.captures(l) {
            if let Some(p) = pending.take() {
                out.push(p);
            }
            let func = cap_str(&caps, "func").map(|f| RUST_HASH.replace(&f, "").into_owned());
            pending = Some(RawFrame {
                raw: l.trim_end().to_string(),
                function: func,
                file: None,
                line: None,
            });
        } else if let Some(caps) = RUST_AT.captures(l) {
            if let Some(p) = pending.as_mut() {
                p.raw.push('\n');
                p.raw.push_str(l.trim_end());
                p.file = cap_str(&caps, "file");
                p.line = cap_u32(&caps, "line");
                out.push(pending.take().expect("pending checked"));
            }
        } else if let Some(caps) = RUST_PANIC.captures(l) {
            if let Some(p) = pending.take() {
                out.push(p);
            }
            panic_at.push(out.len());
            out.push(RawFrame {
                raw: l.trim_end().to_string(),
                function: None,
                file: cap_str(&caps, "file"),
                line: cap_u32(&caps, "line"),
            });
        }
    }
    if let Some(p) = pending {
        out.push(p);
    }
    // The `panicked at file:line` header repeats the numbered frame that
    // carries the panic; keep the frame, which also names the function.
    let mut drop: Vec<usize> = panic_at
        .into_iter()
        .filter(|&i| {
            // The header prints the path relative to the build root while
            // the frame prints it absolute, so compare by `/`-bounded suffix.
            let p = &out[i];
            out.iter().enumerate().any(|(j, f)| {
                j != i
                    && f.function.is_some()
                    && f.line == p.line
                    && match (&f.file, &p.file) {
                        (Some(a), Some(b)) => {
                            let (a, b) = (normalize_path(a), normalize_path(b));
                            a == b
                                || a.strip_suffix(b.as_str()).is_some_and(|r| r.ends_with('/'))
                                || b.strip_suffix(a.as_str()).is_some_and(|r| r.ends_with('/'))
                        }
                        _ => false,
                    }
            })
        })
        .collect();
    drop.sort_unstable_by(|a, b| b.cmp(a));
    for i in drop {
        out.remove(i);
    }
    out
}

fn extract_java(lines: &[&str]) -> Vec<RawFrame> {
    lines
        .iter()
        .filter_map(|l| {
            let caps = JAVA_FRAME.captures(l)?;
            let file = cap_str(&caps, "file")
                .filter(|f| f != "Native Method" && f != "Unknown Source" && f.contains('.'));
            Some(RawFrame {
                raw: l.trim_end().to_string(),
                function: cap_str(&caps, "func"),
                line: if file.is_some() {
                    cap_u32(&caps, "line")
                } else {
                    None
                },
                file,
            })
        })
        .collect()
}

fn extract_go(lines: &[&str]) -> Vec<RawFrame> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let l = lines[i];
        if l.starts_with("goroutine ") || l.starts_with("created by ") {
            i += 1;
            continue;
        }
        if let Some(caps) = GO_FUNC.captures(l)
            && let Some(next) = lines.get(i + 1)
            && let Some(loc) = GO_LOC.captures(next)
        {
            out.push(RawFrame {
                raw: format!("{}\n{}", l.trim_end(), next.trim_end()),
                function: cap_str(&caps, "func"),
                file: cap_str(&loc, "file"),
                line: cap_u32(&loc, "line"),
            });
            i += 2;
            continue;
        }
        i += 1;
    }
    out
}

fn extract_node(lines: &[&str]) -> Vec<RawFrame> {
    lines
        .iter()
        .enumerate()
        .filter_map(|(i, l)| {
            // A Rust `at path:line:col` line has this exact shape; it is never
            // a Node frame when the line above it is a numbered Rust frame.
            if i > 0 && RUST_FRAME.is_match(lines[i - 1]) {
                return None;
            }
            let caps = NODE_FRAME.captures(l)?;
            Some(RawFrame {
                raw: l.trim_end().to_string(),
                function: cap_str(&caps, "func"),
                file: cap_str(&caps, "file"),
                line: cap_u32(&caps, "line"),
            })
        })
        .collect()
}

fn extract_gdb_asan(lines: &[&str]) -> Vec<RawFrame> {
    let mut out = Vec::new();
    for l in lines {
        if let Some(caps) = GDB_FRAME.captures(l) {
            out.push(RawFrame {
                raw: l.trim_end().to_string(),
                function: cap_str(&caps, "func"),
                file: cap_str(&caps, "file"),
                line: cap_u32(&caps, "line"),
            });
        } else if let Some(caps) = GDB_LIB.captures(l) {
            out.push(RawFrame {
                raw: l.trim_end().to_string(),
                function: cap_str(&caps, "func"),
                file: None,
                line: None,
            });
        } else if GDB_UNKNOWN.is_match(l) {
            out.push(RawFrame {
                raw: l.trim_end().to_string(),
                function: Some("??".to_string()),
                file: None,
                line: None,
            });
        }
    }
    out
}

/// Frames of one detected format, grouped by stack in root-cause-first
/// order, each stack innermost-first.
pub struct ParsedTrace {
    pub format: &'static str,
    pub stacks: Vec<Vec<RawFrame>>,
    /// Stack 0 is a cause, not merely the first printed: a Java cause chain,
    /// or a Go dump whose `[running]` goroutine was found.
    pub root_cause_first: bool,
}

/// One separator-delimited chunk of the input with the line that opened it
/// (`None` for the text before the first separator).
struct Chunk<'a> {
    header: Option<&'a str>,
    /// Leading whitespace of the header line: Java nests a suppressed
    /// exception's own `Caused by:` under the `Suppressed:` indent.
    indent: usize,
    lines: Vec<&'a str>,
}

/// Split `lines` at every separator match; the separator line opens the
/// stack it belongs to (Go needs its `goroutine N [running]:` header, Java
/// its `Caused by:` / `Suppressed:` kind).
fn split_stacks<'a>(lines: &[&'a str], sep: Option<&LazyLock<Regex>>) -> Vec<Chunk<'a>> {
    let Some(sep) = sep else {
        return vec![Chunk {
            header: None,
            indent: 0,
            lines: lines.to_vec(),
        }];
    };
    let mut chunks: Vec<Chunk<'a>> = vec![Chunk {
        header: None,
        indent: 0,
        lines: Vec::new(),
    }];
    for l in lines {
        if sep.is_match(l) {
            chunks.push(Chunk {
                header: Some(l),
                indent: l.len() - l.trim_start().len(),
                lines: Vec::new(),
            });
        }
        chunks.last_mut().expect("never empty").lines.push(l);
    }
    chunks
}

struct Parsed {
    stacks: Vec<Vec<RawFrame>>,
    root_cause_first: bool,
}

fn parse_with(f: &Format, lines: &[&str]) -> Parsed {
    let chunks = split_stacks(lines, f.separator);
    let stacks: Vec<(Option<&str>, usize, Vec<RawFrame>)> = chunks
        .iter()
        .map(|chunk| {
            let mut frames = (f.extract)(&chunk.lines);
            if f.reversed {
                frames.reverse();
            }
            (chunk.header, chunk.indent, frames)
        })
        .filter(|(_, _, frames)| !frames.is_empty())
        .collect();
    let (ordered, root_cause_first): (Vec<Vec<RawFrame>>, bool) = match f.stack_order {
        StackOrder::AsPrinted => (stacks.into_iter().map(|(_, _, s)| s).collect(), false),
        StackOrder::Reversed => {
            // Reverse the primary cause chain. Suppressed exceptions and their
            // nested causes remain last in printed order.
            let mut chain: Vec<Vec<RawFrame>> = Vec::new();
            let mut side: Vec<Vec<RawFrame>> = Vec::new();
            // Open suppressed-block indents, innermost last. A cause joins the
            // primary chain only after every suppressed block closes.
            let mut open: Vec<usize> = Vec::new();
            for (header, indent, frames) in stacks {
                let Some(h) = header else {
                    chain.push(frames);
                    continue;
                };
                while open.last().is_some_and(|&last| last > indent) {
                    open.pop();
                }
                if JAVA_SUPPRESSED.is_match(h) {
                    open.push(indent);
                    side.push(frames);
                } else if !open.is_empty() {
                    side.push(frames);
                } else {
                    chain.push(frames);
                }
            }
            chain.reverse();
            chain.extend(side);
            (chain, true)
        }
        StackOrder::RunningFirst => {
            let is_running = |h: &Option<&str>| {
                h.is_some_and(|h| h.starts_with("goroutine ") && h.contains("[running]"))
            };
            let any_running = stacks.iter().any(|(h, _, _)| is_running(h));
            let mut s = stacks;
            s.sort_by_key(|(h, _, _)| !is_running(h));
            (s.into_iter().map(|(_, _, s)| s).collect(), any_running)
        }
    };
    Parsed {
        stacks: ordered,
        root_cause_first,
    }
}

/// Detect the format with the most recognized frames. `None` when no line
/// parsed.
pub fn parse_trace(trace: &str) -> Option<ParsedTrace> {
    let lines: Vec<&str> = trace.lines().collect();
    let mut best: Option<(&Format, Parsed, usize)> = None;
    for f in FORMATS {
        let parsed = parse_with(f, &lines);
        let count: usize = parsed.stacks.iter().map(Vec::len).sum();
        if count == 0 {
            continue;
        }
        if best.as_ref().is_none_or(|(_, _, n)| count > *n) {
            best = Some((f, parsed, count));
        }
    }
    let (f, parsed, _) = best?;
    Some(ParsedTrace {
        format: f.name,
        stacks: parsed.stacks,
        root_cause_first: parsed.root_cause_first,
    })
}

fn format_of(name: &str) -> Option<&'static Format> {
    FORMATS.iter().find(|f| f.name == name)
}

/// Names runtimes print where no definition exists.
fn is_placeholder(seg: &str) -> bool {
    matches!(
        seg,
        "" | "<anonymous>" | "<lambda>" | "<module>" | "<listcomp>" | "<genexpr>" | "??" | "{main}"
    )
}

/// Rust closure/shim segments; the enclosing function is the definition.
fn is_rust_shim(seg: &str) -> bool {
    matches!(seg, "{{closure}}" | "{{vtable.shim}}")
}

/// The path segments of a printed function name, separators normalized away:
/// `crate::mod::func` → `[crate, mod, func]`, `Ns\Class->method` →
/// `[Ns, Class, method]`, `main.(*T).m` → `[main, T, m]`. Rust closure
/// segments are dropped, a leading `<T as Trait>` group is stripped, and a
/// trailing generic list is cut. Empty when the name is a placeholder
/// (`<module>`, `{main}`, `??`, `Object.<anonymous>`) or a Java lambda.
pub fn name_segments(function: &str) -> Vec<String> {
    let f = function.trim().trim_end_matches("()");
    let f = strip_generic_prefix(f);
    let normalized = f.replace("->", "::").replace(['\\', '.'], "::");
    let mut segs: Vec<String> = normalized
        .split("::")
        .map(|s| {
            s.trim_matches(|c| c == '(' || c == ')' || c == '*')
                .to_string()
        })
        .collect();
    while segs.last().is_some_and(|s| is_rust_shim(s)) {
        segs.pop();
    }
    let Some(last) = segs.last().cloned() else {
        return Vec::new();
    };
    if is_placeholder(&last) || last.starts_with("lambda$") {
        return Vec::new();
    }
    if last == "<init>" || last == "<clinit>" {
        segs.pop();
        return segs;
    }
    let cut = last.split('<').next().unwrap_or(&last).to_string();
    if cut.is_empty()
        || !cut
            .chars()
            .all(|c| c.is_alphanumeric() || c == '_' || c == '$')
    {
        return Vec::new();
    }
    *segs.last_mut().expect("checked non-empty") = cut;
    segs.retain(|s| !s.is_empty());
    segs
}

/// The bare definition name: the last segment of [`name_segments`].
#[cfg(test)]
fn leaf_name(function: &str) -> Option<String> {
    name_segments(function).pop()
}

fn strip_generic_prefix(f: &str) -> &str {
    if !f.starts_with('<') {
        return f;
    }
    let mut depth = 0usize;
    for (i, c) in f.char_indices() {
        match c {
            '<' => depth += 1,
            '>' => {
                depth -= 1;
                if depth == 0 {
                    return &f[i + 1..];
                }
            }
            _ => {}
        }
    }
    f
}

/// Segments of an indexed qualified name, which the parser stores with the
/// language's own separator (`Ns\Class\method`, `Type::method`,
/// `pkg.Class.method`).
fn qualified_segments(q: &str) -> Vec<&str> {
    q.split(['\\', '.', ':'])
        .filter(|s| !s.is_empty())
        .collect()
}

/// Require agreement beyond the bare name. A shorter printed name may match
/// a stored suffix; a shorter stored name is allowed only for relative-name
/// languages. Rust free functions can instead agree through their crate path.
fn qualified_agrees(
    def: &Symbol,
    frame_segs: &[String],
    format: &Format,
    crates: &CrateMap,
) -> bool {
    if frame_segs.len() < 2 {
        return false;
    }
    let def_segs = qualified_segments(&def.qualified_name);
    let frame: Vec<&str> = frame_segs.iter().map(String::as_str).collect();
    if def_segs.len() < 2 {
        return !format.qualified_names_absolute && crates.path_agrees(&def.file_path, &frame);
    }
    if def_segs.len() >= frame.len() {
        return def_segs.ends_with(&frame);
    }
    !format.qualified_names_absolute && frame.ends_with(&def_segs)
}

/// Cargo package names → crate root directory (project-relative, `""` for a
/// root package), read from the project's `Cargo.toml` and its workspace
/// members. Empty when the project has no `Cargo.toml`, in which case no
/// printed module path can agree with a bare-stored definition.
#[derive(Debug, Default)]
struct CrateMap {
    dirs: HashMap<String, String>,
}

re!(TOML_SECTION, r"(?m)^\s*\[(?P<name>[^\]]+)\]\s*$");
re!(TOML_NAME, r#"(?m)^\s*name\s*=\s*"(?P<name>[^"]+)""#);
re!(TOML_MEMBERS, r"(?ms)^\s*members\s*=\s*\[(?P<inside>.*?)\]");
re!(TOML_STRING, r#""(?P<s>[^"]+)""#);

/// Bound reads of repository-supplied manifests.
const MAX_MANIFEST_BYTES: u64 = 1 << 20;

fn read_manifest(path: &Path) -> Option<String> {
    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_file() || meta.len() > MAX_MANIFEST_BYTES {
        return None;
    }
    let raw = std::fs::read_to_string(path).ok()?;
    Some(
        raw.lines()
            .map(|l| l.split_once('#').map_or(l, |(code, _)| code))
            .collect::<Vec<_>>()
            .join("\n"),
    )
}

/// Body of the first `[section]` in `toml`, up to the next section header.
fn toml_section<'a>(toml: &'a str, section: &str) -> Option<&'a str> {
    let mut headers = TOML_SECTION.captures_iter(toml).peekable();
    while let Some(h) = headers.next() {
        if h.name("name")?.as_str().trim() != section {
            continue;
        }
        let start = h.get(0)?.end();
        let end = headers
            .peek()
            .and_then(|n| n.get(0))
            .map_or(toml.len(), |m| m.start());
        return Some(&toml[start..end]);
    }
    None
}

fn package_name(toml: &str) -> Option<String> {
    let body = toml_section(toml, "package")?;
    Some(TOML_NAME.captures(body)?.name("name")?.as_str().to_string())
}

fn crate_key(name: &str) -> String {
    name.replace('-', "_")
}

impl CrateMap {
    fn load(root: &Path) -> Self {
        let mut dirs = HashMap::new();
        let Some(root_toml) = read_manifest(&root.join("Cargo.toml")) else {
            return Self { dirs };
        };
        if let Some(name) = package_name(&root_toml) {
            dirs.insert(crate_key(&name), String::new());
        }
        let members: Vec<String> = toml_section(&root_toml, "workspace")
            .and_then(|ws| TOML_MEMBERS.captures(ws))
            .and_then(|c| c.name("inside").map(|m| m.as_str().to_string()))
            .map(|inside| {
                TOML_STRING
                    .captures_iter(&inside)
                    .filter_map(|c| {
                        c.name("s")
                            .map(|m| m.as_str().trim_end_matches('/').to_string())
                    })
                    .collect()
            })
            .unwrap_or_default();
        for member in members {
            // Members stay under the project root: no parent traversal and
            // no absolute paths (`/etc/*`).
            if member.contains("..") || member.starts_with('/') || member.contains(':') {
                continue;
            }
            // `crates/*` style globs: one level of directories under the prefix.
            let candidates: Vec<String> = match member.strip_suffix("/*") {
                Some(prefix) if !prefix.contains(['*', '?']) => {
                    std::fs::read_dir(root.join(prefix))
                        .map(|rd| {
                            rd.filter_map(|e| e.ok())
                                .filter(|e| e.path().is_dir())
                                .map(|e| format!("{prefix}/{}", e.file_name().to_string_lossy()))
                                .collect()
                        })
                        .unwrap_or_default()
                }
                Some(_) => Vec::new(),
                None if member.contains(['*', '?']) => Vec::new(),
                None => vec![member],
            };
            for dir in candidates {
                if let Some(toml) = read_manifest(&root.join(&dir).join("Cargo.toml"))
                    && let Some(name) = package_name(&toml)
                {
                    dirs.insert(crate_key(&name), normalize_path(&dir));
                }
            }
        }
        Self { dirs }
    }

    /// Whether the module path printed ahead of a bare-stored definition is
    /// exactly the file it lives in: the first segment must be a known
    /// package name (`-` ↔ `_`), and the remaining segments must be the
    /// ordered path from that crate's `src/` to the file stem, with
    /// `mod.rs`, `lib.rs`, and `main.rs` as silent terminals
    /// (`codesage::commands::query::f` ↔ `crates/cli/src/commands/query.rs`,
    /// `codesage::main` ↔ `crates/cli/src/main.rs`).
    fn path_agrees(&self, file_path: &str, frame: &[&str]) -> bool {
        let Some((_leaf, modules)) = frame.split_last() else {
            return false;
        };
        let Some((crate_seg, module_segs)) = modules.split_first() else {
            return false;
        };
        let Some(dir) = self.dirs.get(&crate_key(crate_seg)) else {
            return false;
        };
        let file = normalize_path(file_path);
        let src_prefix = if dir.is_empty() {
            "src/".to_string()
        } else {
            format!("{dir}/src/")
        };
        let Some(rel) = file.strip_prefix(src_prefix.as_str()) else {
            return false;
        };
        let rel = rel.rsplit_once('.').map_or(rel, |(stem, _)| stem);
        let mut components: Vec<&str> = rel.split('/').filter(|c| !c.is_empty()).collect();
        if matches!(components.last(), Some(&"mod" | &"lib" | &"main")) {
            components.pop();
        }
        components == module_segs
    }
}

fn normalize_path(p: &str) -> String {
    let p = p.replace('\\', "/");
    p.strip_prefix("./").map_or(p.clone(), |s| s.to_string())
}

/// Indexed paths that share a suffix with `file`: equal, or one is a `/`-
/// bounded suffix of the other (absolute trace path vs project-relative index
/// path, or a bare `main.c` from gdb vs `src/main.c`). An exact match wins
/// alone.
fn match_files<'a>(indexed: &'a [String], file: &str) -> Vec<&'a String> {
    let f = normalize_path(file);
    if let Some(exact) = indexed.iter().find(|p| **p == f) {
        return vec![exact];
    }
    indexed
        .iter()
        .filter(|p| {
            f.strip_suffix(p.as_str())
                .is_some_and(|rest| rest.ends_with('/'))
                || p.strip_suffix(f.as_str())
                    .is_some_and(|rest| rest.ends_with('/'))
        })
        .collect()
}

fn kind_rank(k: SymbolKind) -> u8 {
    match k {
        SymbolKind::Method | SymbolKind::Function | SymbolKind::Macro => 0,
        SymbolKind::Constant => 1,
        SymbolKind::Class
        | SymbolKind::Trait
        | SymbolKind::Interface
        | SymbolKind::Struct
        | SymbolKind::Enum => 2,
        SymbolKind::Module | SymbolKind::Namespace => 3,
    }
}

/// The innermost symbol spanning `line`: smallest range, then callable kinds
/// over containers.
fn containing_symbol(symbols: &[Symbol], line: u32) -> Option<&Symbol> {
    symbols
        .iter()
        .filter(|s| s.line_start <= line && line <= s.line_end)
        .min_by_key(|s| (s.line_end - s.line_start, kind_rank(s.kind)))
}

fn trace_symbol(s: &Symbol) -> TraceSymbol {
    TraceSymbol {
        name: s.name.clone(),
        qualified_name: s.qualified_name.clone(),
        kind: s.kind,
        path: s.file_path.clone(),
        line_start: s.line_start,
        line_end: s.line_end,
    }
}

fn candidate_of(s: &Symbol) -> String {
    format!("{}:{}", s.file_path, s.line_start)
}

fn dedupe_symbols(mut syms: Vec<Symbol>) -> Vec<Symbol> {
    syms.sort_by(|a, b| {
        (&a.file_path, a.line_start, &a.qualified_name).cmp(&(
            &b.file_path,
            b.line_start,
            &b.qualified_name,
        ))
    });
    syms.dedup_by(|a, b| {
        a.file_path == b.file_path && a.line_start == b.line_start && a.name == b.name
    });
    syms
}

/// Everything the resolver needs about the index, loaded once per request.
struct Index {
    paths: Vec<String>,
    language: HashMap<String, Language>,
    crates: CrateMap,
}

impl Index {
    fn load(db: &Database, root: &Path) -> Result<Self> {
        let files = db.all_files_with_id_and_language()?;
        let mut paths = Vec::with_capacity(files.len());
        let mut language = HashMap::with_capacity(files.len());
        for (_, path, lang) in files {
            language.insert(path.clone(), lang);
            paths.push(path);
        }
        Ok(Self {
            paths,
            language,
            crates: CrateMap::load(root),
        })
    }

    fn compatible(&self, def: &Symbol, format: &Format) -> bool {
        self.language
            .get(&def.file_path)
            .is_some_and(|l| format.languages.contains(l))
    }
}

/// Definitions sharing the frame's bare name, in a language the detected
/// format can produce, split into those whose qualified name agrees with the
/// printed one and the rest.
struct NamePool {
    qualified: Vec<Symbol>,
    bare: Vec<Symbol>,
}

fn name_pool(db: &Database, index: &Index, format: &Format, segs: &[String]) -> Result<NamePool> {
    let Some(leaf) = segs.last() else {
        return Ok(NamePool {
            qualified: Vec::new(),
            bare: Vec::new(),
        });
    };
    let hits: Vec<Symbol> = dedupe_symbols(db.find_symbols(leaf, None)?)
        .into_iter()
        .filter(|s| index.compatible(s, format))
        .collect();
    let (qualified, bare): (Vec<Symbol>, Vec<Symbol>) = hits
        .into_iter()
        .partition(|s| qualified_agrees(s, segs, format, &index.crates));
    Ok(NamePool { qualified, bare })
}

/// Cap ambiguous candidates while preserving their total count.
fn set_ambiguous(frame: &mut TraceFrame, candidates: Vec<String>) {
    frame.status = TraceFrameStatus::Ambiguous;
    frame.symbol = None;
    frame.candidates_total = candidates.len();
    let cap = if frame.file.is_none() {
        MAX_CANDIDATES_NAME_ONLY
    } else {
        MAX_CANDIDATES
    };
    frame.candidates = candidates.into_iter().take(cap).collect();
}

/// Place `frame` on a unique qualified match, or mark it ambiguous with the
/// candidates. Returns `false` when the pool offered nothing qualified.
fn apply_qualified(frame: &mut TraceFrame, pool: &NamePool) -> bool {
    match pool.qualified.as_slice() {
        [] => false,
        [one] => {
            frame.status = TraceFrameStatus::Resolved;
            frame.file.get_or_insert_with(|| one.file_path.clone());
            frame.symbol = Some(trace_symbol(one));
            true
        }
        many => {
            set_ambiguous(frame, many.iter().map(candidate_of).collect());
            true
        }
    }
}

/// A bare-name hit outside the frame's own file is a lead, not a location:
/// with only a leaf to go on, even a single same-named definition is
/// reported as `ambiguous` with that one candidate.
fn apply_bare_lead(frame: &mut TraceFrame, pool: &NamePool) -> bool {
    if pool.bare.is_empty() {
        return false;
    }
    set_ambiguous(frame, pool.bare.iter().map(candidate_of).collect());
    true
}

fn resolve_frame(
    db: &Database,
    index: &Index,
    format: &Format,
    index_in_report: usize,
    stack: u32,
    raw: RawFrame,
) -> Result<TraceFrame> {
    let mut frame = TraceFrame {
        index: index_in_report,
        stack,
        raw: raw.raw,
        function: raw.function,
        file: raw.file,
        line: raw.line,
        status: TraceFrameStatus::Unresolved,
        symbol: None,
        candidates: Vec::new(),
        candidates_total: 0,
    };
    let segs: Vec<String> = frame
        .function
        .as_deref()
        .map(name_segments)
        .unwrap_or_default();
    let has_qualifier = segs.len() >= 2;

    let Some(file) = frame.file.clone() else {
        if !segs.is_empty() {
            let pool = name_pool(db, index, format, &segs)?;
            if !apply_qualified(&mut frame, &pool) {
                apply_bare_lead(&mut frame, &pool);
            }
        }
        return Ok(frame);
    };

    let matches = match_files(&index.paths, &file);
    let path = match matches.as_slice() {
        // Vendor PHP call sites may name project methods; require qualified evidence.
        [] => {
            if format.location == Location::CallSite && has_qualifier {
                let pool = name_pool(db, index, format, &segs)?;
                apply_qualified(&mut frame, &pool);
            }
            return Ok(frame);
        }
        [one] => (*one).clone(),
        many => {
            set_ambiguous(&mut frame, many.iter().map(|p| (*p).clone()).collect());
            return Ok(frame);
        }
    };
    frame.file = Some(path.clone());
    frame.status = TraceFrameStatus::Resolved;

    let symbols = db.symbols_for_file(&path)?;
    let leaf = segs.last().cloned();
    let container = frame.line.and_then(|l| containing_symbol(&symbols, l));

    match format.location {
        Location::InBody => {
            if let Some(c) = container
                && leaf.as_deref().is_none_or(|n| n == c.name)
            {
                frame.symbol = Some(trace_symbol(c));
                return Ok(frame);
            }
            // Resolve conflicting names within the printed file, not project-wide.
            if let Some(n) = &leaf {
                let in_file: Vec<&Symbol> = symbols.iter().filter(|s| s.name == *n).collect();
                match in_file.as_slice() {
                    [] => {}
                    [one] => {
                        frame.symbol = Some(trace_symbol(one));
                        return Ok(frame);
                    }
                    many => {
                        set_ambiguous(&mut frame, many.iter().map(|s| candidate_of(s)).collect());
                        return Ok(frame);
                    }
                }
            }
            if let Some(c) = container {
                frame.symbol = Some(trace_symbol(c));
            }
        }
        Location::CallSite => {
            // `file:line` is where the named function was called, so its
            // definition can live anywhere.
            if segs.is_empty() {
                // A fatal-error line names the throw point itself.
                if frame.function.is_none()
                    && let Some(c) = container
                {
                    frame.symbol = Some(trace_symbol(c));
                }
                return Ok(frame);
            }
            let pool = name_pool(db, index, format, &segs)?;
            if has_qualifier {
                if !apply_qualified(&mut frame, &pool) {
                    apply_bare_lead(&mut frame, &pool);
                }
                return Ok(frame);
            }
            // No class or module printed: a plain function. One defined in
            // the calling file is what was called; elsewhere is a lead.
            let in_file: Vec<&Symbol> = pool.bare.iter().filter(|s| s.file_path == path).collect();
            match in_file.as_slice() {
                [one] => frame.symbol = Some(trace_symbol(one)),
                [] => {
                    apply_bare_lead(&mut frame, &pool);
                }
                many => {
                    set_ambiguous(&mut frame, many.iter().map(|s| candidate_of(s)).collect());
                }
            }
        }
    }
    Ok(frame)
}

fn empty_report(note: String) -> FromTraceReport {
    FromTraceReport {
        frames: Vec::new(),
        format: "unknown".to_string(),
        order: TRACE_ORDER.to_string(),
        stacks: 0,
        root_cause_first: false,
        parsed: 0,
        resolved: 0,
        with_symbol: 0,
        ambiguous: 0,
        unresolved: 0,
        note: Some(note),
    }
}

/// Parse `req.trace` and resolve every frame against the index.
/// `root` is the project root; its `Cargo.toml` (if any) tells which printed
/// Rust module paths name which indexed files.
pub fn from_trace(db: &Database, root: &Path, req: &FromTraceRequest) -> Result<FromTraceReport> {
    let Some(parsed) = parse_trace(&req.trace) else {
        return Ok(empty_report(
            "no stack frames recognized; supported formats: python, php (fatal, stack trace, Xdebug), rust, java, go, node, gdb/asan/ubsan"
                .to_string(),
        ));
    };
    let format = format_of(parsed.format).expect("parsed format is registered");
    let total: usize = parsed.stacks.iter().map(Vec::len).sum();
    // Zero selects the default limit.
    let keep = req.limit.filter(|&l| l > 0).map_or(total, |l| l.min(total));
    let stack_count = parsed.stacks.len();
    let note = (keep < total).then(|| {
        // Distinguish a partial stack from entirely dropped trailing stacks.
        let mut seen = 0usize;
        let last_kept_stack = parsed
            .stacks
            .iter()
            .position(|s| {
                seen += s.len();
                seen >= keep
            })
            .unwrap_or(stack_count.saturating_sub(1));
        if stack_count > 1 && last_kept_stack + 1 < stack_count {
            format!(
                "showing the first {keep} of {total} parsed frames, covering stacks 0..={last_kept_stack} of {stack_count} (stacks {}..={} dropped entirely); raise `limit` for the rest",
                last_kept_stack + 1,
                stack_count - 1
            )
        } else if stack_count > 1 {
            format!(
                "showing the first {keep} of {total} parsed frames across all {stack_count} stacks (stack {last_kept_stack} cut short); raise `limit` for the rest"
            )
        } else {
            format!(
                "showing the innermost {keep} of {total} parsed frames; raise `limit` for the rest"
            )
        }
    });

    let index = Index::load(db, root)?;
    let mut frames = Vec::with_capacity(keep);
    'outer: for (stack, raws) in parsed.stacks.iter().enumerate() {
        for raw in raws {
            if frames.len() >= keep {
                break 'outer;
            }
            let i = frames.len();
            frames.push(resolve_frame(
                db,
                &index,
                format,
                i,
                stack as u32,
                raw.clone(),
            )?);
        }
    }
    let count = |st: TraceFrameStatus| frames.iter().filter(|f| f.status == st).count();
    Ok(FromTraceReport {
        resolved: count(TraceFrameStatus::Resolved),
        with_symbol: frames.iter().filter(|f| f.symbol.is_some()).count(),
        ambiguous: count(TraceFrameStatus::Ambiguous),
        unresolved: count(TraceFrameStatus::Unresolved),
        stacks: stack_count,
        root_cause_first: parsed.root_cause_first,
        frames,
        format: format.name.to_string(),
        order: TRACE_ORDER.to_string(),
        parsed: total,
        note,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use codesage_protocol::FileInfo;

    fn frames_of(trace: &str) -> (&'static str, Vec<(u32, RawFrame)>) {
        let p = parse_trace(trace).expect("trace should parse");
        let flat = p
            .stacks
            .into_iter()
            .enumerate()
            .flat_map(|(i, s)| s.into_iter().map(move |f| (i as u32, f)))
            .collect();
        (p.format, flat)
    }

    fn brief(f: &RawFrame) -> (Option<&str>, Option<&str>, Option<u32>) {
        (f.function.as_deref(), f.file.as_deref(), f.line)
    }

    #[test]
    fn python_traceback_is_reversed_to_innermost_first() {
        let trace = r#"Traceback (most recent call last):
  File "/srv/app/main.py", line 12, in <module>
    run()
  File "/srv/app/app/runner.py", line 40, in run
    handle(req)
  File "/srv/app/app/handlers.py", line 88, in handle
    return int(payload["n"])
  File "<frozen importlib._bootstrap>", line 1027, in _find_and_load
ValueError: invalid literal for int() with base 10: 'x'
"#;
        let (fmt, frames) = frames_of(trace);
        assert_eq!(fmt, "python");
        assert_eq!(frames.len(), 4);
        assert_eq!(
            brief(&frames[0].1),
            (
                Some("_find_and_load"),
                Some("<frozen importlib._bootstrap>"),
                Some(1027)
            )
        );
        assert_eq!(
            brief(&frames[1].1),
            (Some("handle"), Some("/srv/app/app/handlers.py"), Some(88))
        );
        assert_eq!(
            brief(&frames[3].1),
            (Some("<module>"), Some("/srv/app/main.py"), Some(12))
        );
        assert_eq!(leaf_name("<module>"), None);
    }

    #[test]
    fn python_chained_tracebacks_keep_the_first_as_root_cause() {
        let trace = r#"Traceback (most recent call last):
  File "/srv/app/db.py", line 30, in connect
    sock.connect(addr)
ConnectionRefusedError: [Errno 111] Connection refused

During handling of the above exception, another exception occurred:

Traceback (most recent call last):
  File "/srv/app/main.py", line 5, in <module>
    boot()
  File "/srv/app/boot.py", line 9, in boot
    raise StartupError("db") from exc
StartupError: db
"#;
        let (fmt, frames) = frames_of(trace);
        assert_eq!(fmt, "python");
        assert_eq!(frames.len(), 3);
        assert_eq!(frames[0].0, 0);
        assert_eq!(brief(&frames[0].1).0, Some("connect"));
        assert_eq!(frames[1].0, 1);
        assert_eq!(
            brief(&frames[1].1).0,
            Some("boot"),
            "second stack innermost-first"
        );
        assert_eq!(frames[2].0, 1);
        assert_eq!(brief(&frames[2].1).0, Some("<module>"));
    }

    #[test]
    fn php_fatal_with_numbered_frames_keeps_text_order_and_emits_main() {
        let trace = r#"PHP Fatal error:  Uncaught RuntimeException: boom in payload in /var/www/src/Service/Mailer.php:57
Stack trace:
#0 /var/www/src/Http/Controller.php(120): App\Service\Mailer->send(Object(App\Mail))
#1 /var/www/vendor/laravel/framework/src/Router.php(700): App\Http\Controller->store()
#2 [internal function]: Closure->__invoke()
#3 {main}
  thrown in /var/www/src/Service/Mailer.php on line 57
"#;
        let (fmt, frames) = frames_of(trace);
        assert_eq!(fmt, "php");
        assert_eq!(frames.len(), 5, "{frames:#?}");
        assert_eq!(
            brief(&frames[0].1),
            (None, Some("/var/www/src/Service/Mailer.php"), Some(57))
        );
        assert_eq!(
            brief(&frames[1].1),
            (
                Some("App\\Service\\Mailer->send"),
                Some("/var/www/src/Http/Controller.php"),
                Some(120)
            )
        );
        assert_eq!(brief(&frames[3].1), (Some("Closure->__invoke"), None, None));
        assert_eq!(brief(&frames[4].1), (Some("{main}"), None, None));
        assert_eq!(
            name_segments("App\\Service\\Mailer->send"),
            vec!["App", "Service", "Mailer", "send"]
        );
        assert!(name_segments("{main}").is_empty());
    }

    #[test]
    fn php_next_exception_chain_is_as_printed() {
        let trace = "PHP Fatal error:  Uncaught PDOException: gone in /srv/app/src/Db.php:20\nStack trace:\n#0 /srv/app/src/Repo.php(8): App\\Db->query()\n#1 {main}\n\nNext App\\RepoException: wrapped in /srv/app/src/Repo.php:10\nStack trace:\n#0 /srv/app/src/Main.php(3): App\\Repo->load()\n#1 {main}\n";
        let (fmt, frames) = frames_of(trace);
        assert_eq!(fmt, "php");
        assert_eq!(frames.len(), 6, "{frames:#?}");
        assert_eq!(frames[0].0, 0);
        assert_eq!(
            brief(&frames[0].1),
            (None, Some("/srv/app/src/Db.php"), Some(20))
        );
        assert_eq!(frames[3].0, 1);
        assert_eq!(
            brief(&frames[3].1),
            (None, Some("/srv/app/src/Repo.php"), Some(10))
        );
    }

    #[test]
    fn php_warning_on_line_form_parses() {
        let trace = "PHP Warning:  Undefined variable $x in /srv/app/lib/util.php on line 9\n";
        let (fmt, frames) = frames_of(trace);
        assert_eq!(fmt, "php");
        assert_eq!(
            brief(&frames[0].1),
            (None, Some("/srv/app/lib/util.php"), Some(9))
        );
    }

    #[test]
    fn xdebug_call_stack_table_is_reversed_to_innermost_first() {
        let trace = "PHP Fatal error:  Uncaught Error: nope in /srv/app/src/Foo.php on line 12\nCall Stack:\n    0.0001     393512   1. {main}() /srv/app/public/index.php:0\n    0.0020     512000   2. App\\Kernel->handle($req = class Request) /srv/app/public/index.php:17\n    0.0031     600000   3. App\\Foo->bar(1, 'x') /srv/app/src/Kernel.php:40\n";
        let (fmt, frames) = frames_of(trace);
        assert_eq!(fmt, "php-xdebug", "{frames:#?}");
        assert_eq!(frames.len(), 3);
        assert_eq!(
            brief(&frames[0].1),
            (
                Some("App\\Foo->bar"),
                Some("/srv/app/src/Kernel.php"),
                Some(40)
            )
        );
        assert_eq!(
            brief(&frames[2].1),
            (Some("{main}"), Some("/srv/app/public/index.php"), Some(0))
        );
        let compact = "1. App\\Foo->bar() /srv/app/src/Kernel.php:40\n2. {main}() /srv/app/public/index.php:0\n";
        let (fmt, frames) = frames_of(compact);
        assert_eq!(fmt, "php-xdebug");
        assert_eq!(frames.len(), 2);
    }

    #[test]
    fn rust_backtrace_attaches_at_lines_strips_hashes_and_dedupes_the_panic_line() {
        let trace = r#"thread 'main' panicked at crates/cli/src/main.rs:812:9:
bad input
stack backtrace:
   0: rust_begin_unwind
             at /rustc/abc123/library/std/src/panicking.rs:665:5
   1: core::panicking::panic_fmt
             at /rustc/abc123/library/core/src/panicking.rs:74:14
   2: codesage::run_from_trace::{{closure}}::h1a2b3c4d5e6f7a8b
             at /home/u/codesage/crates/cli/src/main.rs:812:9
   3: <alloc::vec::Vec<T> as core::clone::Clone>::clone
   4: codesage::main
             at /home/u/codesage/crates/cli/src/main.rs:900:5
note: Some details are omitted, run with `RUST_BACKTRACE=full` for a verbose backtrace.
"#;
        let (fmt, frames) = frames_of(trace);
        assert_eq!(fmt, "rust");
        assert_eq!(
            frames.len(),
            5,
            "the panic header repeats frame 2 and is dropped: {frames:#?}"
        );
        assert_eq!(
            brief(&frames[0].1),
            (
                Some("rust_begin_unwind"),
                Some("/rustc/abc123/library/std/src/panicking.rs"),
                Some(665)
            )
        );
        assert_eq!(
            brief(&frames[2].1),
            (
                Some("codesage::run_from_trace::{{closure}}"),
                Some("/home/u/codesage/crates/cli/src/main.rs"),
                Some(812)
            )
        );
        assert!(frames[2].1.raw.contains("\n             at "));
        assert_eq!(
            brief(&frames[3].1),
            (
                Some("<alloc::vec::Vec<T> as core::clone::Clone>::clone"),
                None,
                None
            )
        );
        assert_eq!(
            leaf_name("codesage::run_from_trace::{{closure}}").as_deref(),
            Some("run_from_trace")
        );
        assert_eq!(
            name_segments("<alloc::vec::Vec<T> as core::clone::Clone>::clone"),
            vec!["clone"]
        );

        // A panic header whose location no numbered frame repeats is kept.
        let lone = "thread 'main' panicked at src/lib.rs:3:5:\nboom\n   0: std::rt::lang_start\n             at /rustc/x/library/std/src/rt.rs:1:1\n";
        let (_, frames) = frames_of(lone);
        assert_eq!(frames.len(), 2);
        assert_eq!(brief(&frames[0].1), (None, Some("src/lib.rs"), Some(3)));
    }

    #[test]
    fn java_frames_skip_native_and_unknown_source() {
        let trace = r#"Exception in thread "main" java.lang.NullPointerException: x
	at com.acme.billing.Invoice.total(Invoice.java:88)
	at com.acme.billing.Invoice.<init>(Invoice.java:20)
	at java.base/jdk.internal.reflect.NativeMethodAccessorImpl.invoke0(Native Method)
	at com.acme.Main.lambda$run$0(Main.java:14)
	at com.acme.Main.main(Unknown Source)
"#;
        let (fmt, frames) = frames_of(trace);
        assert_eq!(fmt, "java");
        assert_eq!(frames.len(), 5);
        assert_eq!(
            brief(&frames[0].1),
            (
                Some("com.acme.billing.Invoice.total"),
                Some("Invoice.java"),
                Some(88)
            )
        );
        assert_eq!(
            name_segments("com.acme.billing.Invoice.<init>"),
            vec!["com", "acme", "billing", "Invoice"]
        );
        assert_eq!(brief(&frames[2].1).1, None);
        assert_eq!(leaf_name("com.acme.Main.lambda$run$0"), None);
        assert_eq!(
            brief(&frames[4].1),
            (Some("com.acme.Main.main"), None, None)
        );
    }

    #[test]
    fn java_caused_by_chain_puts_the_root_cause_first() {
        let trace = r#"javax.persistence.PersistenceException: could not execute statement
	at org.hibernate.internal.ExceptionConverterImpl.convert(ExceptionConverterImpl.java:154)
	at com.acme.OrderService.place(OrderService.java:44)
Caused by: org.hibernate.exception.ConstraintViolationException: could not execute statement
	at org.hibernate.exception.internal.SQLStateConversionDelegate.convert(SQLStateConversionDelegate.java:112)
	... 12 more
Caused by: org.postgresql.util.PSQLException: ERROR: duplicate key value
	at org.postgresql.core.v3.QueryExecutorImpl.receiveErrorResponse(QueryExecutorImpl.java:2676)
	at com.acme.OrderRepo.insert(OrderRepo.java:19)
	... 20 more
"#;
        let (fmt, frames) = frames_of(trace);
        assert_eq!(fmt, "java");
        assert_eq!(frames.len(), 5, "{frames:#?}");
        // Root cause (last `Caused by`) is stack 0 and its innermost frame is first.
        assert_eq!(frames[0].0, 0);
        assert_eq!(
            brief(&frames[0].1).0,
            Some("org.postgresql.core.v3.QueryExecutorImpl.receiveErrorResponse")
        );
        assert_eq!(brief(&frames[1].1).0, Some("com.acme.OrderRepo.insert"));
        assert_eq!(frames[2].0, 1);
        assert_eq!(
            brief(&frames[2].1).0,
            Some("org.hibernate.exception.internal.SQLStateConversionDelegate.convert")
        );
        assert_eq!(frames[3].0, 2);
        assert_eq!(
            brief(&frames[3].1).0,
            Some("org.hibernate.internal.ExceptionConverterImpl.convert")
        );
    }

    #[test]
    fn go_panic_pairs_function_and_location_lines_and_puts_running_goroutine_first() {
        let trace = "panic: runtime error: index out of range [3] with length 3\n\ngoroutine 18 [chan receive]:\nmain.worker(0xc000010000)\n\t/home/u/svc/internal/worker.go:9 +0x35\n\ngoroutine 1 [running]:\nmain.(*Server).handle(0xc000010000, {0x4b, 0x2})\n\t/home/u/svc/internal/server.go:42 +0x1a5\nmain.main()\n\t/home/u/svc/cmd/svc/main.go:15 +0x25\ncreated by net/http.(*Server).Serve\n";
        let (fmt, frames) = frames_of(trace);
        assert_eq!(fmt, "go");
        assert_eq!(frames.len(), 3, "{frames:#?}");
        assert_eq!(frames[0].0, 0);
        assert_eq!(
            brief(&frames[0].1),
            (
                Some("main.(*Server).handle"),
                Some("/home/u/svc/internal/server.go"),
                Some(42)
            )
        );
        assert_eq!(frames[2].0, 1);
        assert_eq!(brief(&frames[2].1).0, Some("main.worker"));
        assert_eq!(
            name_segments("main.(*Server).handle"),
            vec!["main", "Server", "handle"]
        );
        assert_eq!(leaf_name("main.main").as_deref(), Some("main"));
    }

    #[test]
    fn node_frames_with_and_without_function_and_windows_paths() {
        let trace = r#"TypeError: Cannot read properties of undefined (reading 'id')
    at Object.<anonymous> (/srv/api/src/users.js:12:18)
    at async handleRequest (/srv/api/src/server.js:40:5)
    at /srv/api/node_modules/express/lib/router/index.js:280:10
    at node:internal/main/run_main_module:28:49
    at render (C:\Users\dev\api\src\view.js:7:3)
"#;
        let (fmt, frames) = frames_of(trace);
        assert_eq!(fmt, "node");
        assert_eq!(frames.len(), 5, "{frames:#?}");
        assert_eq!(
            brief(&frames[0].1),
            (
                Some("Object.<anonymous>"),
                Some("/srv/api/src/users.js"),
                Some(12)
            )
        );
        assert_eq!(leaf_name("Object.<anonymous>"), None);
        assert_eq!(
            brief(&frames[1].1),
            (
                Some("handleRequest"),
                Some("/srv/api/src/server.js"),
                Some(40)
            )
        );
        assert_eq!(
            brief(&frames[3].1),
            (None, Some("node:internal/main/run_main_module"), Some(28))
        );
        assert_eq!(
            brief(&frames[4].1),
            (
                Some("render"),
                Some(r"C:\Users\dev\api\src\view.js"),
                Some(7)
            )
        );
        assert_eq!(
            normalize_path(r"C:\Users\dev\api\src\view.js"),
            "C:/Users/dev/api/src/view.js"
        );
    }

    #[test]
    fn asan_report_splits_access_freed_and_allocated_stacks_in_that_order() {
        let trace = r#"==4242==ERROR: AddressSanitizer: heap-use-after-free on address 0x602000000014
READ of size 4 at 0x602000000014 thread T0
    #0 0x55d3f0 in read_header /home/u/proj/src/parse.c:123:12
    #1 0x55d590 in main /home/u/proj/src/main.c:31:9
    #2 0x7f1c2a029d8f in __libc_start_call_main (/lib/x86_64-linux-gnu/libc.so.6+0x29d8f)
0x602000000014 is located 4 bytes inside of 16-byte region
freed by thread T0 here:
    #0 0x4f0a2d in free ../../../../src/libsanitizer/asan/asan_malloc_linux.cpp:52
    #1 0x55d4a1 in release_header /home/u/proj/src/parse.c:210
previously allocated by thread T0 here:
    #0 0x4f0e6d in malloc ../../../../src/libsanitizer/asan/asan_malloc_linux.cpp:69
    #1 0x55d3a0 in alloc_header /home/u/proj/src/parse.c:100
SUMMARY: AddressSanitizer: heap-use-after-free /home/u/proj/src/parse.c:123:12 in read_header
"#;
        let (fmt, frames) = frames_of(trace);
        assert_eq!(fmt, "gdb-asan");
        assert_eq!(frames.len(), 7, "{frames:#?}");
        assert_eq!(frames[0].0, 0);
        assert_eq!(
            brief(&frames[0].1),
            (
                Some("read_header"),
                Some("/home/u/proj/src/parse.c"),
                Some(123)
            )
        );
        assert_eq!(
            brief(&frames[2].1),
            (Some("__libc_start_call_main"), None, None)
        );
        assert_eq!(frames[3].0, 1);
        assert_eq!(brief(&frames[4].1).0, Some("release_header"));
        assert_eq!(frames[5].0, 2);
        assert_eq!(brief(&frames[6].1).0, Some("alloc_header"));
    }

    #[test]
    fn gdb_backtrace_with_cpp_signatures_args_operators_and_unknown_frames() {
        let trace = "#0  0x00005555555551a9 in read_header (buf=0x5555555592a0 \"x\", n=4) at src/parse.c:123\n#1  0x000055555555520f in Foo::bar(int, char const*) (this=0x0, n=1, s=0x0) at src/foo.cpp:12\n#2  0x0000555555555300 in ns::f(void (*)(int)) (cb=0x555555555189) at src/ns.cpp:5\n#3  0x0000555555555400 in operator delete(void*) (p=0x0) at src/mem.cpp:3\n#4  0x0000555555555500 in main () at C:\\src\\main.c:31\n#5  0x00007ffff7c29d90 in ?? () from /lib/x86_64-linux-gnu/libc.so.6\n";
        let (fmt, frames) = frames_of(trace);
        assert_eq!(fmt, "gdb-asan");
        assert_eq!(frames.len(), 6, "{frames:#?}");
        assert_eq!(
            brief(&frames[0].1),
            (Some("read_header"), Some("src/parse.c"), Some(123))
        );
        assert_eq!(
            brief(&frames[1].1),
            (Some("Foo::bar"), Some("src/foo.cpp"), Some(12))
        );
        assert_eq!(
            brief(&frames[2].1),
            (Some("ns::f"), Some("src/ns.cpp"), Some(5))
        );
        assert_eq!(
            brief(&frames[3].1),
            (Some("operator delete"), Some("src/mem.cpp"), Some(3))
        );
        assert_eq!(
            brief(&frames[4].1),
            (Some("main"), Some(r"C:\src\main.c"), Some(31))
        );
        assert_eq!(brief(&frames[5].1), (Some("??"), None, None));
        assert!(name_segments("operator delete").is_empty());
        assert_eq!(name_segments("Foo::bar"), vec!["Foo", "bar"]);
    }

    #[test]
    fn unknown_text_parses_nothing() {
        assert!(parse_trace("nothing to see here\njust logs\n").is_none());
        assert!(parse_trace("").is_none());
    }

    fn sym(name: &str, qualified: &str, kind: SymbolKind, path: &str, a: u32, b: u32) -> Symbol {
        Symbol {
            name: name.to_string(),
            qualified_name: qualified.to_string(),
            kind,
            file_path: path.to_string(),
            line_start: a,
            line_end: b,
            col_start: 0,
            col_end: 0,
            rationale: Vec::new(),
        }
    }

    fn seed(db: &Database, path: &str, lang: Language, symbols: &[Symbol]) {
        let id = db
            .upsert_file(&FileInfo {
                path: path.to_string(),
                language: lang,
                content_hash: format!("h-{path}"),
            })
            .unwrap();
        db.insert_symbols(id, symbols).unwrap();
    }

    const MODEL: &str = "src/Illuminate/Database/Eloquent/Model.php";
    const HAS_ONE_OR_MANY: &str = "src/Illuminate/Database/Eloquent/Relations/HasOneOrMany.php";
    const SESSION_HANDLER: &str = "src/Illuminate/Session/DatabaseSessionHandler.php";

    /// Qualified names as the parser stores them: PHP `Ns\Class\method`
    /// (sampled from the laravel-framework index), Rust `Type::method`.
    fn fixture_db() -> Database {
        let db = Database::open_in_memory().unwrap();
        seed(
            &db,
            "crates/graph/src/search.rs",
            Language::Rust,
            &[
                sym(
                    "search",
                    "search",
                    SymbolKind::Function,
                    "crates/graph/src/search.rs",
                    10,
                    80,
                ),
                sym(
                    "rerank",
                    "search::rerank",
                    SymbolKind::Function,
                    "crates/graph/src/search.rs",
                    30,
                    50,
                ),
            ],
        );
        seed(
            &db,
            "crates/cli/src/util.rs",
            Language::Rust,
            &[sym(
                "helper",
                "helper",
                SymbolKind::Function,
                "crates/cli/src/util.rs",
                1,
                5,
            )],
        );
        seed(
            &db,
            "crates/graph/src/util.rs",
            Language::Rust,
            &[sym(
                "helper",
                "helper",
                SymbolKind::Function,
                "crates/graph/src/util.rs",
                1,
                5,
            )],
        );
        seed(
            &db,
            "crates/cli/src/mcp/mod.rs",
            Language::Rust,
            &[sym(
                "capped_limit_tracked",
                "capped_limit_tracked",
                SymbolKind::Function,
                "crates/cli/src/mcp/mod.rs",
                104,
                125,
            )],
        );
        seed(
            &db,
            MODEL,
            Language::Php,
            &[
                sym(
                    "Model",
                    "Illuminate\\Database\\Eloquent\\Model",
                    SymbolKind::Class,
                    MODEL,
                    10,
                    2000,
                ),
                sym(
                    "save",
                    "Illuminate\\Database\\Eloquent\\Model\\save",
                    SymbolKind::Method,
                    MODEL,
                    1345,
                    1380,
                ),
                sym(
                    "performUpdate",
                    "Illuminate\\Database\\Eloquent\\Model\\performUpdate",
                    SymbolKind::Method,
                    MODEL,
                    1459,
                    1490,
                ),
            ],
        );
        seed(
            &db,
            HAS_ONE_OR_MANY,
            Language::Php,
            &[
                sym(
                    "HasOneOrMany",
                    "Illuminate\\Database\\Eloquent\\Relations\\HasOneOrMany",
                    SymbolKind::Class,
                    HAS_ONE_OR_MANY,
                    21,
                    629,
                ),
                sym(
                    "updateOrCreate",
                    "Illuminate\\Database\\Eloquent\\Relations\\HasOneOrMany\\updateOrCreate",
                    SymbolKind::Method,
                    HAS_ONE_OR_MANY,
                    298,
                    305,
                ),
                sym(
                    "save",
                    "Illuminate\\Database\\Eloquent\\Relations\\HasOneOrMany\\save",
                    SymbolKind::Method,
                    HAS_ONE_OR_MANY,
                    334,
                    340,
                ),
            ],
        );
        seed(
            &db,
            SESSION_HANDLER,
            Language::Php,
            &[sym(
                "performUpdate",
                "Illuminate\\Session\\DatabaseSessionHandler\\performUpdate",
                SymbolKind::Method,
                SESSION_HANDLER,
                170,
                180,
            )],
        );
        // A shallow, absolute PHP name that shares only its leaf and class
        // segment with a deeper namespace's frame.
        seed(
            &db,
            "types/Support/Collection.php",
            Language::Php,
            &[sym(
                "__invoke",
                "Invokable\\__invoke",
                SymbolKind::Method,
                "types/Support/Collection.php",
                28,
                30,
            )],
        );
        seed(
            &db,
            "src/com/acme/Foo.java",
            Language::Java,
            &[sym(
                "run",
                "com.acme.Foo.run",
                SymbolKind::Method,
                "src/com/acme/Foo.java",
                12,
                20,
            )],
        );
        seed(
            &db,
            "crates/cli/src/commands/query.rs",
            Language::Rust,
            &[sym(
                "cmd_search",
                "cmd_search",
                SymbolKind::Function,
                "crates/cli/src/commands/query.rs",
                100,
                160,
            )],
        );
        seed(
            &db,
            "crates/cli/src/main.rs",
            Language::Rust,
            &[sym(
                "main",
                "main",
                SymbolKind::Function,
                "crates/cli/src/main.rs",
                800,
                900,
            )],
        );
        seed(
            &db,
            "crates/parser/src/parse.rs",
            Language::Rust,
            &[sym(
                "ParsedTree",
                "ParsedTree",
                SymbolKind::Struct,
                "crates/parser/src/parse.rs",
                3,
                30,
            )],
        );
        // Rust free functions are stored with a bare qualified name.
        seed(
            &db,
            "crates/graph/src/mention.rs",
            Language::Rust,
            &[sym(
                "apply_mention_anchor",
                "apply_mention_anchor",
                SymbolKind::Function,
                "crates/graph/src/mention.rs",
                5,
                40,
            )],
        );
        db
    }

    /// A project root whose `Cargo.toml` mirrors this workspace's shape:
    /// package `codesage` at `crates/cli`, `codesage-graph` at
    /// `crates/graph`, plus a `libs/*` glob member.
    static ROOT: LazyLock<tempfile::TempDir> = LazyLock::new(|| {
        let dir = tempfile::tempdir().unwrap();
        let w = |rel: &str, body: &str| {
            let p = dir.path().join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, body).unwrap();
        };
        w(
            "Cargo.toml",
            "[workspace]\n# members = [\"crates/ghost\"]\nmembers = [\n    \"crates/graph\",\n    \"crates/cli\",\n    \"libs/*\",\n]\n\n[workspace.package]\nversion = \"0.1.0\"\n",
        );
        w(
            "crates/graph/Cargo.toml",
            "[package]\nname = \"codesage-graph\"\nversion.workspace = true\n\n[dependencies]\nname = \"not-a-package-name\"\n",
        );
        w(
            "crates/cli/Cargo.toml",
            "[package]\nname = \"codesage\"\n\n[[bin]]\nname = \"codesage\"\n",
        );
        w(
            "libs/parser/Cargo.toml",
            "[package]\nname = \"toml-parser\"\n",
        );
        dir
    });

    fn run_limit(db: &Database, trace: &str, limit: Option<usize>) -> FromTraceReport {
        from_trace(
            db,
            ROOT.path(),
            &FromTraceRequest {
                trace: trace.to_string(),
                limit,
            },
        )
        .unwrap()
    }

    fn run(db: &Database, trace: &str) -> FromTraceReport {
        run_limit(db, trace, None)
    }

    #[test]
    fn crate_map_reads_packages_members_and_globs() {
        let m = CrateMap::load(ROOT.path());
        assert_eq!(
            m.dirs.get("codesage_graph").map(String::as_str),
            Some("crates/graph")
        );
        assert_eq!(
            m.dirs.get("codesage").map(String::as_str),
            Some("crates/cli")
        );
        assert_eq!(
            m.dirs.get("toml_parser").map(String::as_str),
            Some("libs/parser")
        );
        assert!(
            !m.dirs.contains_key("ghost"),
            "commented-out member ignored"
        );
        assert!(!m.dirs.contains_key("not_a_package_name"));
        assert_eq!(m.dirs.len(), 3);

        let empty = tempfile::tempdir().unwrap();
        assert!(CrateMap::load(empty.path()).dirs.is_empty());
        // Place real manifests at escaped paths so the guard test is non-vacuous.
        let hostile = tempfile::tempdir().unwrap();
        let root = hostile.path().join("outside/root");
        let sibling = hostile.path().join("outside/sibling");
        let abs = hostile.path().join("abs");
        for d in [&root, &sibling, &abs] {
            std::fs::create_dir_all(d).unwrap();
        }
        std::fs::write(
            sibling.join("Cargo.toml"),
            "[package]\nname = \"escapee\"\n",
        )
        .unwrap();
        std::fs::write(abs.join("Cargo.toml"), "[package]\nname = \"absolutee\"\n").unwrap();
        std::fs::write(
            root.join("Cargo.toml"),
            format!(
                "[workspace]\nmembers = [\"../sibling\", \"{}\", \"{}/*\"]\n",
                abs.display(),
                hostile.path().display()
            ),
        )
        .unwrap();
        assert_eq!(
            package_name(&read_manifest(&sibling.join("Cargo.toml")).unwrap()).as_deref(),
            Some("escapee")
        );
        assert_eq!(
            package_name(&read_manifest(&abs.join("Cargo.toml")).unwrap()).as_deref(),
            Some("absolutee")
        );
        let escaped = CrateMap::load(&root);
        assert!(
            escaped.dirs.is_empty(),
            "members outside the root were followed: {:?}",
            escaped.dirs
        );
        let frame = ["codesage_graph", "search", "f"];
        assert!(
            !CrateMap::load(empty.path()).path_agrees("crates/graph/src/search.rs", &frame),
            "no Cargo.toml: no module path can agree"
        );

        assert!(m.path_agrees("crates/graph/src/search.rs", &frame));
        assert!(m.path_agrees("crates/graph/src/search/mod.rs", &frame));
        assert!(m.path_agrees("crates/cli/src/main.rs", &["codesage", "main"]));
        assert!(m.path_agrees("crates/cli/src/lib.rs", &["codesage", "f"]));
        assert!(m.path_agrees(
            "crates/cli/src/commands/query.rs",
            &["codesage", "commands", "query", "cmd_search"]
        ));
        assert!(!m.path_agrees(
            "crates/cli/src/commands/query.rs",
            &["codesage", "query", "cmd_search"],
        ));
        assert!(!m.path_agrees("crates/graph/src/search.rs", &["foo_graph", "search", "f"]));
        assert!(!m.path_agrees("crates/cli/src/main.rs", &["foo_cli", "main"]));
        assert!(!m.path_agrees("crates/parser/src/parse.rs", &["toml_parser", "parse", "T"]));
    }

    #[test]
    fn rust_frame_resolves_to_innermost_spanning_symbol() {
        let db = fixture_db();
        let r = run(
            &db,
            "   0: codesage_graph::search::rerank::{{closure}}\n             at /home/u/codesage/crates/graph/src/search.rs:35:9\n   1: codesage_graph::search::search\n             at /home/u/codesage/crates/graph/src/search.rs:70:5\n   2: std::rt::lang_start\n             at /rustc/abc/library/std/src/rt.rs:100:5\n",
        );
        assert_eq!(r.format, "rust");
        assert_eq!(r.order, "innermost-first");
        assert_eq!(r.stacks, 1);
        assert_eq!(
            (
                r.parsed,
                r.resolved,
                r.with_symbol,
                r.ambiguous,
                r.unresolved
            ),
            (3, 2, 2, 0, 1)
        );
        let f0 = &r.frames[0];
        assert_eq!(f0.status, TraceFrameStatus::Resolved);
        assert_eq!(f0.stack, 0);
        assert_eq!(f0.file.as_deref(), Some("crates/graph/src/search.rs"));
        assert_eq!(f0.symbol.as_ref().unwrap().name, "rerank");
        assert_eq!(r.frames[1].symbol.as_ref().unwrap().name, "search");
        let f2 = &r.frames[2];
        assert_eq!(f2.status, TraceFrameStatus::Unresolved);
        assert!(f2.candidates.is_empty());
        assert!(f2.symbol.is_none());
        assert_eq!(f2.file.as_deref(), Some("/rustc/abc/library/std/src/rt.rs"));
    }

    #[test]
    fn same_suffix_in_two_files_is_ambiguous_with_candidates() {
        let db = fixture_db();
        let r = run(&db, "#0 0x1 in helper src/util.rs:3\n");
        assert_eq!(r.format, "gdb-asan");
        let f = &r.frames[0];
        assert_eq!(f.status, TraceFrameStatus::Ambiguous);
        assert!(f.symbol.is_none());
        assert_eq!(
            f.candidates,
            vec!["crates/cli/src/util.rs", "crates/graph/src/util.rs"]
        );
        assert_eq!(r.ambiguous, 1);
    }

    #[test]
    fn name_only_frames_resolve_only_on_qualified_agreement() {
        let db = fixture_db();
        // Two bare definitions: ambiguous with both.
        let r = run(&db, "   0: codesage::helper\n");
        let f = &r.frames[0];
        assert_eq!(f.status, TraceFrameStatus::Ambiguous);
        assert_eq!(
            f.candidates,
            vec!["crates/cli/src/util.rs:1", "crates/graph/src/util.rs:1"]
        );
        // The index's `search::rerank` is a suffix of the printed path.
        let r = run(&db, "   0: codesage_graph::search::rerank\n");
        let f = &r.frames[0];
        assert_eq!(f.status, TraceFrameStatus::Resolved);
        assert_eq!(f.file.as_deref(), Some("crates/graph/src/search.rs"));
        assert_eq!(f.symbol.as_ref().unwrap().qualified_name, "search::rerank");
        // A bare-stored Rust free function agrees through its file path:
        // `codesage_graph` ↔ `crates/graph`, `search` ↔ `search.rs`.
        let r = run(&db, "   0: codesage_graph::search::search\n");
        let f = &r.frames[0];
        assert_eq!(f.status, TraceFrameStatus::Resolved, "{f:#?}");
        assert_eq!(f.file.as_deref(), Some("crates/graph/src/search.rs"));
        let r = run(&db, "   0: codesage_graph::mention::apply_mention_anchor\n");
        let f = &r.frames[0];
        assert_eq!(f.status, TraceFrameStatus::Resolved, "{f:#?}");
        assert_eq!(
            f.symbol.as_ref().unwrap().path,
            "crates/graph/src/mention.rs"
        );
        // The module path names a different file: a lead, not a location.
        let r = run(&db, "   0: codesage_graph::index::apply_mention_anchor\n");
        let f = &r.frames[0];
        assert_eq!(f.status, TraceFrameStatus::Ambiguous, "{f:#?}");
        assert_eq!(f.candidates, vec!["crates/graph/src/mention.rs:5"]);
        assert_eq!(f.candidates_total, 1);
        // Another crate's module of the same name does not agree either.
        let r = run(&db, "   0: codesage_cli::search::search\n");
        assert_eq!(r.frames[0].status, TraceFrameStatus::Ambiguous);
        // The binary crate is package `codesage`, whose directory is
        // `crates/cli`: only the manifest knows that.
        let r = run(&db, "   0: codesage::commands::query::cmd_search\n");
        let f = &r.frames[0];
        assert_eq!(f.status, TraceFrameStatus::Resolved, "{f:#?}");
        assert_eq!(f.file.as_deref(), Some("crates/cli/src/commands/query.rs"));
        // Crates that merely end in a path component, or are not in the
        // workspace at all, never agree.
        for frame in [
            "   0: toml_parser::parse::ParsedTree\n",
            "   0: foo_graph::search::search\n",
            "   0: foo_cli::main\n",
            "   0: anstyle_query::commands::query::cmd_search\n",
        ] {
            let r = run(&db, frame);
            assert_ne!(
                r.frames[0].status,
                TraceFrameStatus::Resolved,
                "{frame:?} must not resolve: {:#?}",
                r.frames[0]
            );
        }
    }

    #[test]
    fn name_only_frames_never_cross_languages() {
        let db = fixture_db();
        // A Java frame on an index that only has a Rust `helper`.
        let r = run(&db, "\tat com.acme.Util.helper(Unknown Source)\n");
        assert_eq!(r.format, "java");
        let f = &r.frames[0];
        assert_eq!(f.status, TraceFrameStatus::Unresolved, "{f:#?}");
        assert!(f.candidates.is_empty());
        // A PHP internal-function frame naming a Rust function.
        let r = run(
            &db,
            "#0 [internal function]: capped_limit_tracked()\n#1 {main}\n",
        );
        assert_eq!(r.format, "php");
        assert_eq!(
            r.frames[0].status,
            TraceFrameStatus::Unresolved,
            "{:#?}",
            r.frames[0]
        );
        assert!(r.frames[0].candidates.is_empty());
        assert_eq!(r.frames[1].function.as_deref(), Some("{main}"));
        assert_eq!(r.frames[1].status, TraceFrameStatus::Unresolved);
        assert_eq!(r.unresolved, 2);
    }

    #[test]
    fn vendor_path_is_unresolved_without_candidates() {
        let db = fixture_db();
        let r = run(
            &db,
            "  File \"/srv/app/vendor/lib/thing.py\", line 3, in helper\n",
        );
        let f = &r.frames[0];
        assert_eq!(f.status, TraceFrameStatus::Unresolved);
        assert!(
            f.candidates.is_empty(),
            "no guessing by name for out-of-index files"
        );
        assert!(f.symbol.is_none());
    }

    #[test]
    fn php_call_site_frames_resolve_the_named_method_by_qualified_agreement() {
        let db = fixture_db();
        // `Model->save()` called from inside HasOneOrMany, which has its own
        // `save`: the qualified form must win over the in-file bare name.
        let r = run(
            &db,
            "#0 /var/www/src/Illuminate/Database/Eloquent/Relations/HasOneOrMany.php(300): Illuminate\\Database\\Eloquent\\Model->save()\n",
        );
        let f = &r.frames[0];
        assert_eq!(f.status, TraceFrameStatus::Resolved);
        assert_eq!(f.file.as_deref(), Some(HAS_ONE_OR_MANY));
        assert_eq!(f.line, Some(300));
        let s = f.symbol.as_ref().unwrap();
        assert_eq!(
            s.qualified_name,
            "Illuminate\\Database\\Eloquent\\Model\\save"
        );
        assert_eq!(s.path, MODEL);

        // Short class name: the printed `Model\save` is a suffix of the stored
        // name, and only one definition satisfies it.
        let r = run(
            &db,
            "#0 /var/www/src/Illuminate/Database/Eloquent/Relations/HasOneOrMany.php(300): Model->save()\n",
        );
        assert_eq!(r.frames[0].symbol.as_ref().unwrap().path, MODEL);

        // Vendor call site naming a project method: two bare `performUpdate`
        // definitions, one qualified agreement.
        let r = run(
            &db,
            "#0 /var/www/vendor/foo/Bootstrap.php(9): Illuminate\\Database\\Eloquent\\Model->performUpdate()\n",
        );
        let f = &r.frames[0];
        assert_eq!(f.status, TraceFrameStatus::Resolved, "{f:#?}");
        assert_eq!(
            f.file.as_deref(),
            Some("/var/www/vendor/foo/Bootstrap.php"),
            "the call site stays as printed"
        );
        assert_eq!(f.symbol.as_ref().unwrap().path, MODEL);

        // A bare function name from a vendor call site is not enough.
        let r = run(&db, "#0 /var/www/vendor/x.php(5): performUpdate()\n");
        assert_eq!(r.frames[0].status, TraceFrameStatus::Unresolved);
        assert!(r.frames[0].symbol.is_none());

        // A qualified name nothing agrees with falls back to a lead: both
        // bare definitions as candidates, no symbol.
        let r = run(
            &db,
            "#0 /var/www/src/Illuminate/Database/Eloquent/Model.php(1350): App\\Legacy\\Thing->performUpdate()\n",
        );
        let f = &r.frames[0];
        assert_eq!(f.status, TraceFrameStatus::Ambiguous, "{f:#?}");
        assert_eq!(
            f.candidates,
            vec![format!("{MODEL}:1459"), format!("{SESSION_HANDLER}:170")]
        );

        // The fatal line names the throw point: the spanning method.
        let r = run(
            &db,
            "PHP Fatal error:  Uncaught RuntimeException: boom in /var/www/src/Illuminate/Database/Eloquent/Model.php:1360\n",
        );
        assert_eq!(r.frames[0].symbol.as_ref().unwrap().name, "save");
    }

    #[test]
    fn limit_truncates_and_notes_and_zero_means_default() {
        let db = fixture_db();
        let r = run_limit(&db, "   0: a\n   1: b\n   2: c\n", Some(2));
        assert_eq!(r.parsed, 3);
        assert_eq!(r.frames.len(), 2);
        assert!(r.note.as_deref().unwrap().contains("2 of 3"));

        let r = run_limit(&db, "   0: a\n   1: b\n   2: c\n", Some(0));
        assert_eq!(r.frames.len(), 3);
        assert!(r.note.is_none());
    }

    #[test]
    fn absolute_qualified_names_never_agree_with_a_shallower_stored_name() {
        let db = fixture_db();
        // The stored `Invokable\__invoke` is shorter than the printed
        // `App\Domain\Rules\Invokable\__invoke`; in PHP that is a different
        // namespace, so it is at most a lead.
        let r = run(
            &db,
            "#0 /var/www/src/Illuminate/Database/Eloquent/Model.php(1350): App\\Domain\\Rules\\Invokable->__invoke()\n",
        );
        let f = &r.frames[0];
        assert_ne!(f.status, TraceFrameStatus::Resolved, "{f:#?}");
        assert!(f.symbol.is_none());
        assert_eq!(f.candidates, vec!["types/Support/Collection.php:28"]);
        // The frame-shorter direction still agrees: `Model->save()` and a
        // Java `Foo.run(Native Method)` pin their absolute definitions.
        let r = run(
            &db,
            "#0 /var/www/src/Illuminate/Database/Eloquent/Relations/HasOneOrMany.php(300): Model->save()\n",
        );
        assert_eq!(r.frames[0].status, TraceFrameStatus::Resolved);
        assert_eq!(r.frames[0].symbol.as_ref().unwrap().path, MODEL);
        let r = run(&db, "\tat Foo.run(Native Method)\n");
        assert_eq!(r.format, "java");
        let f = &r.frames[0];
        assert_eq!(f.status, TraceFrameStatus::Resolved, "{f:#?}");
        assert_eq!(
            f.symbol.as_ref().unwrap().qualified_name,
            "com.acme.Foo.run"
        );
        assert!(r.root_cause_first, "a Java trace is a cause chain");
    }

    #[test]
    fn java_suppressed_blocks_are_siblings_never_the_root_cause() {
        // try-with-resources: primary plus a Suppressed close failure.
        let trace = "java.io.IOException: read failed\n\tat com.acme.Reader.read(Reader.java:10)\n\tSuppressed: java.io.IOException: close failed\n\t\tat com.acme.Reader.close(Reader.java:30)\n";
        let (fmt, frames) = frames_of(trace);
        assert_eq!(fmt, "java");
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].0, 0);
        assert_eq!(brief(&frames[0].1).0, Some("com.acme.Reader.read"));
        assert_eq!(frames[1].0, 1);
        assert_eq!(brief(&frames[1].1).0, Some("com.acme.Reader.close"));

        // A suppressed exception with its own `Caused by:` (printed at the
        // Suppressed indent) stays in the suppressed subtree; the primary
        // chain's `Caused by:` at indent 0 is the root cause.
        let trace = "java.lang.RuntimeException: wrapped\n\tat com.acme.Service.run(Service.java:5)\n\tSuppressed: java.io.IOException: close failed\n\t\tat com.acme.Reader.close(Reader.java:30)\n\tCaused by: java.io.IOException: flush failed\n\t\tat com.acme.Reader.flush(Reader.java:44)\nCaused by: java.io.IOException: read failed\n\tat com.acme.Reader.read(Reader.java:10)\n";
        let (_, frames) = frames_of(trace);
        assert_eq!(frames.len(), 4, "{frames:#?}");
        assert_eq!(
            (frames[0].0, brief(&frames[0].1).0),
            (0, Some("com.acme.Reader.read"))
        );
        assert_eq!(
            (frames[1].0, brief(&frames[1].1).0),
            (1, Some("com.acme.Service.run"))
        );
        assert_eq!(
            (frames[2].0, brief(&frames[2].1).0),
            (2, Some("com.acme.Reader.close"))
        );
        assert_eq!(
            (frames[3].0, brief(&frames[3].1).0),
            (3, Some("com.acme.Reader.flush"))
        );

        // Three levels: a `Caused by:` printed after a deeper nested
        // `Suppressed:` but at the outer suppressed block's indent still
        // belongs to that outer block, not the primary chain. Primary A,
        // \tSuppressed B, \t\tSuppressed C, \tCaused by D → A, B, C, D.
        let trace = "java.lang.RuntimeException: A\n\tat com.acme.A.a(A.java:1)\n\tSuppressed: java.io.IOException: B\n\t\tat com.acme.B.b(B.java:2)\n\t\tSuppressed: java.io.IOException: C\n\t\t\tat com.acme.C.c(C.java:3)\n\tCaused by: java.io.IOException: D\n\t\tat com.acme.D.d(D.java:4)\n";
        let (_, frames) = frames_of(trace);
        let order: Vec<(u32, Option<&str>)> =
            frames.iter().map(|(s, f)| (*s, brief(f).0)).collect();
        assert_eq!(
            order,
            vec![
                (0, Some("com.acme.A.a")),
                (1, Some("com.acme.B.b")),
                (2, Some("com.acme.C.c")),
                (3, Some("com.acme.D.d")),
            ],
            "{frames:#?}"
        );
        // With a real outer cause after the suppressed subtree: F, A, B, C, D.
        let trace =
            format!("{trace}Caused by: java.io.IOException: F\n\tat com.acme.F.f(F.java:6)\n");
        let (_, frames) = frames_of(&trace);
        let order: Vec<(u32, Option<&str>)> =
            frames.iter().map(|(s, f)| (*s, brief(f).0)).collect();
        assert_eq!(
            order,
            vec![
                (0, Some("com.acme.F.f")),
                (1, Some("com.acme.A.a")),
                (2, Some("com.acme.B.b")),
                (3, Some("com.acme.C.c")),
                (4, Some("com.acme.D.d")),
            ],
            "{frames:#?}"
        );

        // A cause with its own Suppressed block: the cause is stack 0, the
        // primary stack 1, the suppressed block last.
        let trace = "java.lang.RuntimeException: wrapped\n\tat com.acme.Service.run(Service.java:5)\nCaused by: java.io.IOException: read failed\n\tat com.acme.Reader.read(Reader.java:10)\n\tSuppressed: java.io.IOException: close failed\n\t\tat com.acme.Reader.close(Reader.java:30)\n";
        let (_, frames) = frames_of(trace);
        assert_eq!(frames.len(), 3);
        assert_eq!(
            (frames[0].0, brief(&frames[0].1).0),
            (0, Some("com.acme.Reader.read"))
        );
        assert_eq!(
            (frames[1].0, brief(&frames[1].1).0),
            (1, Some("com.acme.Service.run"))
        );
        assert_eq!(
            (frames[2].0, brief(&frames[2].1).0),
            (2, Some("com.acme.Reader.close"))
        );
    }

    #[test]
    fn candidates_are_capped_with_the_total_kept() {
        let db = Database::open_in_memory().unwrap();
        for i in 0..57 {
            let path = format!(
                "src/Illuminate/Console/View/Components/Mutators/EnsureDynamicContentIsHighlighted{i:02}.php"
            );
            seed(
                &db,
                &path,
                Language::Php,
                &[sym(
                    "__invoke",
                    &format!(
                        "Illuminate\\Console\\View\\Components\\Mutators\\Highlighted{i:02}\\__invoke"
                    ),
                    SymbolKind::Method,
                    &path,
                    13,
                    19,
                )],
            );
        }
        let r = run(
            &db,
            "#0 /srv/app/src/Illuminate/Console/View/Components/Mutators/EnsureDynamicContentIsHighlighted00.php(5): Closure->__invoke()\n",
        );
        let f = &r.frames[0];
        assert_eq!(f.status, TraceFrameStatus::Ambiguous);
        assert_eq!(f.candidates.len(), MAX_CANDIDATES);
        assert_eq!(f.candidates_total, 57);
        let r = run(&db, "#0 [internal function]: Closure->__invoke()\n");
        let f = &r.frames[0];
        assert_eq!(f.status, TraceFrameStatus::Ambiguous);
        assert_eq!(f.candidates.len(), MAX_CANDIDATES_NAME_ONLY);
        assert_eq!(f.candidates_total, 57);
        assert!(f.candidates[0].ends_with("Highlighted00.php:13"));

        // Long name-only traces must leave enough MCP budget to retain every frame.
        let mut trace = String::new();
        for i in 0..42 {
            trace.push_str(&format!("#{i} [internal function]: Closure->__invoke()\n"));
        }
        let r = run(&db, &trace);
        assert_eq!(r.frames.len(), 42);
        assert_eq!(r.ambiguous, 42);
        let json = serde_json::to_string(&r).unwrap();
        let candidate_chars: usize = r
            .frames
            .iter()
            .flat_map(|f| f.candidates.iter())
            .map(|c| c.len() + 4)
            .sum();
        let uncapped_estimate = json.len() - candidate_chars + candidate_chars * 57 / 5;
        assert!(
            json.len() <= 32_000,
            "42 capped name-only frames serialize to {} chars (uncapped would be ~{uncapped_estimate})",
            json.len()
        );
        assert!(
            json.len() * 3 < uncapped_estimate,
            "cap should cut the payload by more than 3x: {} vs ~{uncapped_estimate}",
            json.len()
        );
        eprintln!(
            "from_trace: 42 name-only frames = {} chars capped, ~{uncapped_estimate} uncapped",
            json.len()
        );

        let db = Database::open_in_memory().unwrap();
        for i in 0..12 {
            seed(&db, &format!("pkg{i:02}/src/util.rs"), Language::Rust, &[]);
        }
        let r = run(&db, "#0 0x1 in helper src/util.rs:3\n");
        let f = &r.frames[0];
        assert_eq!(f.status, TraceFrameStatus::Ambiguous);
        assert_eq!(f.candidates.len(), MAX_CANDIDATES);
        assert_eq!(f.candidates_total, 12);
    }

    #[test]
    fn rust_panics_on_several_threads_split_into_stacks() {
        let trace = "thread 'worker' panicked at src/worker.rs:9:5:\nboom\nstack backtrace:\n   0: app::worker::run\n             at /home/u/app/src/worker.rs:9:5\nthread 'main' panicked at src/main.rs:20:9:\nworker died\nstack backtrace:\n   0: app::main\n             at /home/u/app/src/main.rs:20:9\n";
        let (fmt, frames) = frames_of(trace);
        assert_eq!(fmt, "rust");
        assert_eq!(frames.len(), 2, "{frames:#?}");
        assert_eq!(
            (frames[0].0, brief(&frames[0].1).0),
            (0, Some("app::worker::run"))
        );
        assert_eq!((frames[1].0, brief(&frames[1].1).0), (1, Some("app::main")));
        let db = fixture_db();
        let r = run(&db, trace);
        assert_eq!(r.stacks, 2);
        assert!(
            !r.root_cause_first,
            "thread order is positional, not causal"
        );
    }

    #[test]
    fn multi_stack_truncation_note_names_the_dropped_stacks() {
        let db = fixture_db();
        let trace = "a.X: outer\n\tat com.acme.A.a(A.java:1)\nCaused by: b.X: mid\n\tat com.acme.B.b(B.java:2)\nCaused by: c.X: deep\n\tat com.acme.C.c(C.java:3)\nCaused by: d.X: deepest\n\tat com.acme.D.d(D.java:4)\n\tat com.acme.D.e(D.java:5)\n";
        let r = run_limit(&db, trace, Some(2));
        assert_eq!(r.stacks, 4);
        assert_eq!(r.frames.len(), 2);
        assert!(r.frames.iter().all(|f| f.stack == 0));
        let note = r.note.as_deref().unwrap();
        assert!(note.contains("stacks 0..=0 of 4"), "{note}");
        assert!(note.contains("stacks 1..=3 dropped"), "{note}");
        assert!(!note.contains("innermost"), "{note}");

        let r = run_limit(&db, trace, Some(4));
        let note = r.note.as_deref().unwrap();
        assert!(note.contains("stacks 0..=2 of 4"), "{note}");
        assert!(note.contains("stacks 3..=3 dropped"), "{note}");
    }

    #[test]
    fn indented_python_chained_traceback_splits_into_stacks() {
        let trace = "    Traceback (most recent call last):\n      File \"/srv/app/db.py\", line 30, in connect\n        sock.connect(addr)\n    ConnectionRefusedError: refused\n\n    The above exception was the direct cause of the following exception:\n\n    Traceback (most recent call last):\n      File \"/srv/app/boot.py\", line 9, in boot\n        raise StartupError(\"db\") from exc\n    StartupError: db\n";
        let (fmt, frames) = frames_of(trace);
        assert_eq!(fmt, "python");
        assert_eq!(frames.len(), 2);
        assert_eq!((frames[0].0, brief(&frames[0].1).0), (0, Some("connect")));
        assert_eq!((frames[1].0, brief(&frames[1].1).0), (1, Some("boot")));
    }

    #[test]
    fn unknown_input_reports_unknown_format() {
        let db = fixture_db();
        let r = run(&db, "just some log line\n");
        assert_eq!(r.format, "unknown");
        assert_eq!(r.parsed, 0);
        assert_eq!(r.stacks, 0);
        assert!(r.frames.is_empty());
        assert!(r.note.is_some());
    }
}
