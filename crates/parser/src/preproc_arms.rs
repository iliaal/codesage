//! Reduce a C/C++ conditional group whose arms each hold part of one
//! statement, before tree-sitter sees the file.
//!
//! tree-sitter parses every arm of `#if`/`#else` as if both were compiled.
//! When each arm opens the same loop or switch header
//! (`#ifdef X\n for (a) {\n#else\n for (b) {\n#endif`), the parse sees two
//! unclosed braces, and error recovery drops the enclosing function
//! definition with every call in it. An arm that starts with `else` and has
//! no `if` of its own in the arm is the same shape.
//!
//! For such a group the copy handed to the parser keeps the first live arm
//! as written and blanks the directive lines. Every other arm stays in place
//! with only what ties it to code outside the arm blanked: braces it opens
//! or closes without a partner, and a leading `else`. What remains of it is
//! complete statements, so its calls are still indexed inside the enclosing
//! function. An arm no build reaches (`#if 0`, or after `#if 1`), and an arm
//! that leaves a parenthesis unbalanced (a piece of an expression), is
//! blanked whole. Blanking writes spaces and keeps line breaks, so the
//! tree's byte offsets and lines are those of the original source;
//! extractors read node text from the original.
//!
//! Only groups whose innermost enclosing brace is a function or statement
//! body are reduced, and only when no arm closes more braces than the
//! statement blocks open around the group. Groups at namespace, class,
//! `extern "C"`, or file scope, groups whose arms close the function body
//! (`}` / `} catch (...) { }` around a function-try-block), groups whose
//! arms are each balanced, and groups whose arms disagree on how many braces
//! or parentheses they leave open are left exactly as written.

use crate::export_macro::{Kind, Lexer};
use crate::preproc::DeadRegions;
use anyhow::Result;

/// Tokens between cancellation checks; directives always check.
const CHECK_EVERY: u32 = 4096;

/// Return a same-length copy of `source` with split conditional groups
/// reduced, or `None` when no group qualifies.
pub(crate) fn select_split_arms(source: &[u8]) -> Result<Option<Vec<u8>>> {
    if !has_conditional_directive(source) {
        return Ok(None);
    }
    let control = codesage_protocol::work::current();
    let check = || -> Result<()> {
        if let Some(control) = &control {
            control.check()?;
        }
        Ok(())
    };
    let mut scan = Scan::default();
    let mut since_check = 0u32;
    for token in Lexer::new(source) {
        if token.kind == Kind::Directive {
            check()?;
            scan.directive(token.start, token.end, &source[token.start..token.end]);
            continue;
        }
        since_check += 1;
        if since_check == CHECK_EVERY {
            since_check = 0;
            check()?;
        }
        scan.token(
            token.kind,
            token.start,
            token.end,
            &source[token.start..token.end],
        );
    }
    if scan.blanks.is_empty() {
        return Ok(None);
    }
    let mut out = source.to_vec();
    for (start, end) in scan.blanks {
        for byte in &mut out[start..end] {
            if *byte != b'\n' && *byte != b'\r' {
                *byte = b' ';
            }
        }
    }
    Ok(Some(out))
}

/// Cheap gate: a line whose first non-blank byte is `#` followed by `if`.
fn has_conditional_directive(source: &[u8]) -> bool {
    let mut line_start = true;
    for (i, &b) in source.iter().enumerate() {
        if b == b'\n' {
            line_start = true;
        } else if line_start && b == b'#' {
            let rest = &source[i + 1..];
            let skip = rest
                .iter()
                .take_while(|c| matches!(c, b' ' | b'\t'))
                .count();
            if rest[skip..].starts_with(b"if") {
                return true;
            }
            line_start = false;
        } else if !matches!(b, b' ' | b'\t' | b'\r') {
            line_start = false;
        }
    }
    false
}

/// The conditional directives the pre-pass blanked, on a same-length copy
/// of `source` that is otherwise spaces, or `None` when no blanked `#if` /
/// `#elif` condition could carry a call. Without the pre-pass a condition
/// such as `#if !FEATURE(x)` is parsed as an expression, so its calls are
/// references; reference extraction parses this copy to keep them. Every
/// blanked directive of the group is copied, so each condition parses in the
/// same structure as in the original. A directive counts as blanked when the
/// tree has no directive token where its `#` stands.
///
/// A raw line scan finds candidates cheaply; only directive tokens from the
/// lexer are copied, so a `#if` line inside a comment, string, or raw string
/// never becomes code.
pub(crate) fn blanked_conditions(source: &[u8], dead: &DeadRegions) -> Option<Vec<u8>> {
    if !has_blanked_call_condition_line(source, dead) {
        return None;
    }
    let mut spans: Vec<(usize, usize)> = Vec::new();
    let mut calls = false;
    for token in Lexer::new(source) {
        if token.kind != Kind::Directive || dead.has_directive_at(token.start) {
            continue;
        }
        let text = &source[token.start..token.end];
        if directive(text).is_some() {
            spans.push((token.start, token.end));
            calls |= matches!(keyword(text), b"if" | b"elif") && text.contains(&b'(');
        }
    }
    if !calls {
        return None;
    }
    let mut out: Vec<u8> = source
        .iter()
        .map(|&b| if b == b'\n' || b == b'\r' { b } else { b' ' })
        .collect();
    for (start, end) in spans {
        out[start..end].copy_from_slice(&source[start..end]);
    }
    Some(out)
}

/// A line that looks like `#if ...(` or `#elif ...(` with no directive token
/// in the tree at its `#`. May match inside comments and strings; the lexer
/// pass decides.
fn has_blanked_call_condition_line(source: &[u8], dead: &DeadRegions) -> bool {
    let mut i = 0;
    while i < source.len() {
        let mut line_end = memchr_newline(source, i);
        while line_end < source.len() && continues_line(source, line_end) {
            line_end = memchr_newline(source, line_end + 1);
        }
        let hash = i + source[i..line_end]
            .iter()
            .take_while(|b| matches!(b, b' ' | b'\t'))
            .count();
        if source.get(hash) == Some(&b'#')
            && matches!(keyword(&source[hash..line_end]), b"if" | b"elif")
            && source[hash..line_end].contains(&b'(')
            && !dead.has_directive_at(hash)
        {
            return true;
        }
        i = line_end + 1;
    }
    false
}

/// True when the newline at `nl` is escaped by a trailing `\\` (optionally
/// followed by `\r`), so the directive continues on the next line.
fn continues_line(source: &[u8], nl: usize) -> bool {
    let mut end = nl;
    if end > 0 && source[end - 1] == b'\r' {
        end -= 1;
    }
    end > 0 && source[end - 1] == b'\\'
}

fn memchr_newline(source: &[u8], from: usize) -> usize {
    source[from.min(source.len())..]
        .iter()
        .position(|&b| b == b'\n')
        .map_or(source.len(), |p| from + p)
}

enum Directive {
    /// `#if`, `#ifdef`, `#ifndef`, with the literal value of a `#if 0` or `#if 1`.
    If(Option<bool>),
    /// `#elif`, `#elifdef`, `#elifndef`, `#else`.
    Else(Option<bool>),
    Endif,
}

/// The directive name after `#`, or empty when `text` is not a directive.
fn keyword(text: &[u8]) -> &[u8] {
    let Some(rest) = text.strip_prefix(b"#") else {
        return b"";
    };
    let rest = trim_start(rest);
    let len = rest
        .iter()
        .take_while(|b| b.is_ascii_alphanumeric() || **b == b'_')
        .count();
    &rest[..len]
}

fn directive(text: &[u8]) -> Option<Directive> {
    let rest = text.strip_prefix(b"#")?;
    let rest = trim_start(rest);
    let len = rest
        .iter()
        .take_while(|b| b.is_ascii_alphanumeric() || **b == b'_')
        .count();
    let (keyword, condition) = rest.split_at(len);
    Some(match keyword {
        b"if" => Directive::If(literal(condition)),
        b"ifdef" | b"ifndef" => Directive::If(None),
        b"elif" => Directive::Else(literal(condition)),
        b"elifdef" | b"elifndef" | b"else" => Directive::Else(None),
        b"endif" => Directive::Endif,
        _ => return None,
    })
}

/// `Some(false)` for a condition spelled `0` or `(0)`, `Some(true)` for `1`
/// or `(1)`, matching the literals `preproc::DeadRegions` treats as decided.
fn literal(condition: &[u8]) -> Option<bool> {
    let end = condition
        .windows(2)
        .position(|w| w == b"/*" || w == b"//")
        .unwrap_or(condition.len());
    let mut text = trim(&condition[..end]);
    while let Some(inner) = text.strip_prefix(b"(").and_then(|t| t.strip_suffix(b")")) {
        text = trim(inner);
    }
    match text {
        b"0" => Some(false),
        b"1" => Some(true),
        _ => None,
    }
}

fn trim_start(text: &[u8]) -> &[u8] {
    let skip = text.iter().take_while(|b| b.is_ascii_whitespace()).count();
    &text[skip..]
}

fn trim(text: &[u8]) -> &[u8] {
    let text = trim_start(text);
    let keep = text.len()
        - text
            .iter()
            .rev()
            .take_while(|b| b.is_ascii_whitespace())
            .count();
    &text[..keep]
}

/// Bracket accounting for one arm: net depth and the lowest depth reached,
/// so an arm that closes a brace before opening one (`} else {`) is not
/// mistaken for a balanced one.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
struct Depth {
    net: i32,
    low: i32,
}

impl Depth {
    fn step(&mut self, delta: i32) {
        self.net += delta;
        self.low = self.low.min(self.net);
    }

    fn then(&mut self, next: Depth) {
        self.low = self.low.min(self.net + next.low);
        self.net += next.net;
    }

    fn balanced(self) -> bool {
        self.net == 0 && self.low == 0
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum First {
    Unseen,
    Else,
    Other,
}

/// What a `{` opens, judged from the token before it and the brace around it.
#[derive(Clone, Copy, PartialEq, Eq)]
enum BraceKind {
    Function,
    /// A statement block inside a function: `if`/`for`/`while`/`switch`,
    /// `else`, `do`, `try`, a lambda, or a bare block.
    Block,
    /// Namespace, `extern "C"`, class/struct/union/enum, or an initializer.
    Other,
}

#[derive(Clone, Copy)]
struct Open {
    at: usize,
    kind: BraceKind,
    /// Consecutive `Block` braces ending with this one.
    blocks: i32,
}

/// The last token, as far as classifying a following `{` needs.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Prev {
    /// Starts a statement: file start, `{`, `}`, `;`, or `:`.
    Boundary,
    /// Ends a head whose `{` opens a body: `)`, `]`, `else`, `do`, `try`,
    /// or a trailing function qualifier.
    BodyHead,
    Other,
}

/// Where an `else` stands: at the start of an arm, right after a closer with
/// no opener in the arm, or anywhere else.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Position {
    Start,
    AfterCloser,
    Inside,
}

/// A `}` or `)` with no opener in its arm, plus an `else` right after it.
#[derive(Clone, Copy)]
struct Closer {
    at: usize,
    else_after: Option<(usize, usize)>,
}

/// Where tokens land: the file itself, or a non-kept arm scanned in
/// isolation. A kept arm writes straight into its enclosing sink, which is
/// what the parser sees once the group is reduced.
struct Sink {
    brace_base: usize,
    paren_base: usize,
    closers: Vec<Closer>,
    paren_closed: bool,
    leading_else: Option<(usize, usize)>,
    position: Position,
    prev: Prev,
    /// A nested group was left as written while not balanced, so the sink's
    /// offsets no longer describe what the parser sees.
    opaque: bool,
}

impl Sink {
    fn new(brace_base: usize, paren_base: usize, prev: Prev) -> Self {
        Self {
            brace_base,
            paren_base,
            closers: Vec::new(),
            paren_closed: false,
            leading_else: None,
            position: Position::Start,
            prev,
            opaque: false,
        }
    }
}

struct Arm {
    directive: (usize, usize),
    literal: Option<bool>,
    braces: Depth,
    parens: Depth,
    first: First,
    /// Scanned in its own sink; dropped from the parser's view if reduced.
    sandboxed: bool,
}

impl Arm {
    fn new(directive: (usize, usize), literal: Option<bool>, sandboxed: bool) -> Self {
        Self {
            directive,
            literal,
            braces: Depth::default(),
            parens: Depth::default(),
            first: First::Unseen,
            sandboxed,
        }
    }

    /// Parses as a unit on its own: brackets balanced and not a dangling
    /// `else` that needs an `if` outside the arm.
    fn whole(&self) -> bool {
        self.braces.balanced() && self.parens.balanced() && self.first != First::Else
    }
}

struct Group {
    arms: Vec<Arm>,
    kept: Option<usize>,
    /// The kept arm is a literal `#if 1` / `#elif 1`: later arms are dead.
    taken: bool,
    /// The innermost brace around the group is a function or block body.
    in_body: bool,
    /// Statement blocks open around the group inside the function body;
    /// an arm may close at most this many.
    blocks: i32,
    prev_at_open: Prev,
    /// Blanking to apply if the group is reduced.
    pending: Vec<(usize, usize)>,
}

#[derive(Default)]
struct Scan {
    braces: Vec<Open>,
    parens: Vec<usize>,
    sinks: Vec<Sink>,
    groups: Vec<Group>,
    blanks: Vec<(usize, usize)>,
}

impl Scan {
    fn sink(&mut self) -> &mut Sink {
        if self.sinks.is_empty() {
            self.sinks.push(Sink::new(0, 0, Prev::Boundary));
        }
        self.sinks.last_mut().expect("root sink exists")
    }

    fn token(&mut self, kind: Kind, start: usize, end: usize, text: &[u8]) {
        if let Some(arm) = self.groups.last_mut().and_then(|g| g.arms.last_mut()) {
            if arm.first == First::Unseen {
                arm.first = if text == b"else" {
                    First::Else
                } else {
                    First::Other
                };
            }
            match kind {
                Kind::Punct(b'{') => arm.braces.step(1),
                Kind::Punct(b'}') => arm.braces.step(-1),
                Kind::Punct(b'(') => arm.parens.step(1),
                Kind::Punct(b')') => arm.parens.step(-1),
                _ => {}
            }
        }
        match kind {
            Kind::Ident => {
                let sink = self.sink();
                if text == b"else" {
                    match sink.position {
                        Position::Start => sink.leading_else = Some((start, end)),
                        Position::AfterCloser => {
                            if let Some(closer) = sink.closers.last_mut() {
                                closer.else_after = Some((start, end));
                            }
                        }
                        Position::Inside => {}
                    }
                }
                sink.position = Position::Inside;
                sink.prev = if matches!(
                    text,
                    b"else"
                        | b"do"
                        | b"try"
                        | b"const"
                        | b"override"
                        | b"final"
                        | b"noexcept"
                        | b"mutable"
                ) {
                    Prev::BodyHead
                } else {
                    Prev::Other
                };
            }
            Kind::Punct(b'{') => {
                let prev = self.sink().prev;
                let outer = self.braces.last().copied();
                let in_body = outer.is_some_and(|o| o.kind != BraceKind::Other);
                let kind = match prev {
                    Prev::BodyHead if in_body => BraceKind::Block,
                    Prev::BodyHead => BraceKind::Function,
                    Prev::Boundary if in_body => BraceKind::Block,
                    _ => BraceKind::Other,
                };
                let blocks = match (kind, outer) {
                    (BraceKind::Block, Some(o)) if o.kind == BraceKind::Block => o.blocks + 1,
                    (BraceKind::Block, _) => 1,
                    _ => 0,
                };
                self.braces.push(Open {
                    at: start,
                    kind,
                    blocks,
                });
                let sink = self.sink();
                sink.position = Position::Inside;
                sink.prev = Prev::Boundary;
            }
            Kind::Punct(b'}') => {
                let base = self.sink().brace_base;
                if self.braces.len() > base {
                    self.braces.pop();
                    self.sink().position = Position::Inside;
                } else {
                    let sink = self.sink();
                    sink.closers.push(Closer {
                        at: start,
                        else_after: None,
                    });
                    sink.position = Position::AfterCloser;
                }
                self.sink().prev = Prev::Boundary;
            }
            Kind::Punct(b'(') => {
                self.parens.push(start);
                let sink = self.sink();
                sink.position = Position::Inside;
                sink.prev = Prev::Other;
            }
            Kind::Punct(b')') => {
                let base = self.sink().paren_base;
                if self.parens.len() > base {
                    self.parens.pop();
                } else {
                    self.sink().paren_closed = true;
                }
                let sink = self.sink();
                sink.position = Position::Inside;
                sink.prev = Prev::BodyHead;
            }
            Kind::Punct(b) => {
                let sink = self.sink();
                sink.position = Position::Inside;
                sink.prev = match b {
                    b';' | b':' => Prev::Boundary,
                    b']' => Prev::BodyHead,
                    _ => Prev::Other,
                };
            }
            _ => {
                let sink = self.sink();
                sink.position = Position::Inside;
                sink.prev = Prev::Other;
            }
        }
    }

    fn directive(&mut self, start: usize, end: usize, text: &[u8]) {
        match directive(text) {
            Some(Directive::If(literal)) => {
                let prev_at_open = self.sink().prev;
                let outer = self.braces.last().copied();
                let in_body = outer.is_some_and(|o| o.kind != BraceKind::Other);
                let blocks = outer.map_or(0, |o| o.blocks);
                self.groups.push(Group {
                    arms: Vec::new(),
                    kept: None,
                    taken: false,
                    in_body,
                    blocks,
                    prev_at_open,
                    pending: Vec::new(),
                });
                self.start_arm((start, end), literal);
            }
            Some(Directive::Else(literal)) => {
                if self.groups.is_empty() {
                    return;
                }
                self.end_arm(start);
                self.start_arm((start, end), literal);
            }
            Some(Directive::Endif) => {
                if self.groups.is_empty() {
                    return;
                }
                self.end_arm(start);
                self.resolve((start, end));
            }
            None => {}
        }
    }

    fn start_arm(&mut self, directive: (usize, usize), literal: Option<bool>) {
        let (braces, parens) = (self.braces.len(), self.parens.len());
        let group = self
            .groups
            .last_mut()
            .expect("arm belongs to an open group");
        let index = group.arms.len();
        let kept = group.kept.is_none() && !group.taken && literal != Some(false);
        if kept {
            group.kept = Some(index);
            group.taken = literal == Some(true);
        }
        let prev = group.prev_at_open;
        group.arms.push(Arm::new(directive, literal, !kept));
        if !kept {
            self.sinks.push(Sink::new(braces, parens, prev));
        }
    }

    /// Close the current arm. A non-kept arm's offsets are turned into the
    /// blanking it needs if the group is reduced, then dropped, so each
    /// offset is visited once however deep the nesting.
    fn end_arm(&mut self, end: usize) {
        let group = self
            .groups
            .last_mut()
            .expect("arm belongs to an open group");
        let arm = group.arms.last().expect("group has an arm");
        if !arm.sandboxed {
            return;
        }
        let sink = self.sinks.pop().expect("sandboxed arm has a sink");
        let dead = arm.literal == Some(false) || group.kept.is_some_and(|_| group.taken);
        // An unbalanced parenthesis means the arm is a piece of an
        // expression (`if (a >= 0` / `if ((a > 0 || b)`); with its bracket
        // removed it would be spliced into the kept arm's expression.
        let expression = sink.paren_closed || self.parens.len() > sink.paren_base;
        if dead || sink.opaque || expression {
            group.pending.push((arm.directive.1, end));
        } else {
            group.pending.extend(sink.leading_else);
            for closer in &sink.closers {
                group.pending.push((closer.at, closer.at + 1));
                group.pending.extend(closer.else_after);
            }
            group.pending.extend(
                self.braces[sink.brace_base..]
                    .iter()
                    .map(|open| (open.at, open.at + 1)),
            );
        }
        self.braces.truncate(sink.brace_base);
        self.parens.truncate(sink.paren_base);
    }

    fn resolve(&mut self, endif: (usize, usize)) {
        let group = self.groups.pop().expect("endif closes an open group");
        let Some(kept) = group.kept else {
            // Every arm is a literal `#if 0` / `#elif 0`: no build parses
            // any of it, and `preproc::DeadRegions` masks it.
            return;
        };
        let whole = group.arms.iter().all(Arm::whole);
        let first = &group.arms[0];
        let split = !whole
            && group.in_body
            && group.arms.iter().all(|arm| arm.braces.low >= -group.blocks)
            && if group.arms.len() == 1 {
                // Without `#else`, the configuration that skips the arm
                // keeps nothing, so the arm may only be joined to the code
                // around it when it leaves the depth unchanged (`else ...
                // { }`, `} else {`). A lone arm that opens a brace is closed
                // by a matching group elsewhere and parses as written.
                first.braces.net == 0 && first.parens.net == 0
            } else {
                group.arms.iter().all(|arm| {
                    arm.braces.net == first.braces.net && arm.parens.net == first.parens.net
                })
            };
        if split {
            self.blanks.extend(group.pending);
            self.blanks
                .extend(group.arms.iter().map(|arm| arm.directive));
            self.blanks.push(endif);
        } else if !whole {
            self.sink().opaque = true;
        }
        let kept = &group.arms[kept];
        if let Some(parent) = self.groups.last_mut().and_then(|g| g.arms.last_mut()) {
            if parent.first == First::Unseen {
                parent.first = kept.first;
            }
            parent.braces.then(kept.braces);
            parent.parens.then(kept.parens);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::select_split_arms;

    fn reduced(src: &str) -> Option<String> {
        let out = select_split_arms(src.as_bytes()).unwrap()?;
        assert_eq!(out.len(), src.len());
        for (a, b) in src.bytes().zip(out.iter().copied()) {
            assert!(a == b || b == b' ', "only spaces may be written");
            assert_eq!(a == b'\n', b == b'\n', "newlines must stay put");
        }
        Some(String::from_utf8(out).unwrap())
    }

    #[test]
    fn split_loop_header_keeps_the_first_arm_and_detaches_the_other() {
        let src = "void f(int n)\n{\n#ifdef X\n    for (a(); n; n--) {\n#else\n    for (b(); n; n++) {\n#endif\n        c();\n    }\n}\n";
        let out = reduced(src).unwrap();
        assert_eq!(
            out,
            "void f(int n)\n{\n        \n    for (a(); n; n--) {\n     \n    for (b(); n; n++)  \n      \n        c();\n    }\n}\n"
        );
    }

    #[test]
    fn balanced_arms_and_file_scope_groups_are_untouched() {
        for src in [
            "#ifdef X\nint f(void) { return a(); }\n#else\nint f(void) { return b(); }\n#endif\n",
            "void g(void)\n{\n#ifdef X\n    a();\n#elif Y\n    b();\n#else\n    c();\n#endif\n}\n",
            "#ifdef W\nstatic int p(long a)\n{\n#else\nstatic int p(int a)\n{\n#endif\n    return a;\n}\n",
            "#ifdef __cplusplus\nextern \"C\" {\n#endif\nint h(void);\n#ifdef __cplusplus\n}\n#endif\n",
            "void u(int n)\n{\n#ifdef X\n    while (n) {\n#else\n    n++;\n#endif\n    }\n}\n",
            "int plain(void) { return 0; }\n",
        ] {
            assert_eq!(reduced(src), None, "{src}");
        }
    }

    #[test]
    fn dangling_else_in_a_lone_arm_loses_only_its_directives() {
        let src = "void k(int x)\n{\n    if (x) {\n    }\n#if defined(A)\n    else if (x > 2) {\n        b();\n    }\n#endif\n}\n";
        let out = reduced(src).unwrap();
        assert_eq!(
            out,
            src.replace("#if defined(A)", "              ")
                .replace("#endif", "      ")
        );
    }

    #[test]
    fn dead_arms_are_blanked_whole() {
        let src = "void z(int n)\n{\n#if 0\n    while (n) { old();\n#else\n    while (n) { live();\n#endif\n    }\n}\n";
        let out = reduced(src).unwrap();
        assert!(!out.contains("old"), "{out}");
        assert!(out.contains("while (n) { live();"), "{out}");
    }

    #[test]
    fn directives_inside_comments_and_strings_are_not_groups() {
        let src = "void s(int n)\n{\n    const char *t = \"#ifdef X {\";\n    /*\n#else\n    } {\n    */\n    a(t);\n}\n";
        assert_eq!(reduced(src), None);
    }

    #[test]
    fn deep_nesting_is_scanned_in_linear_time() {
        // 50,000 levels: a scan that revisits enclosing levels per directive
        // takes seconds even in release builds; a linear one takes
        // milliseconds. The bound is loose enough for a loaded debug run.
        let depth = 50_000;
        let mut plain = String::from("void f(void)\n{\n");
        let mut split = plain.clone();
        for _ in 0..depth {
            plain.push_str("#ifdef X\n");
            split.push_str("#ifdef X\nfor(;;){\n#else\n");
        }
        for _ in 0..depth {
            plain.push_str("#endif\n");
            split.push_str("for(;;){\n#endif\n");
        }
        plain.push_str("}\n");
        for _ in 0..depth {
            split.push_str("}\n");
        }
        split.push_str("}\n");
        let started = std::time::Instant::now();
        assert!(select_split_arms(plain.as_bytes()).unwrap().is_none());
        assert!(select_split_arms(split.as_bytes()).unwrap().is_some());
        let elapsed = started.elapsed();
        assert!(
            elapsed < std::time::Duration::from_secs(10),
            "deep nesting took {elapsed:?}"
        );
    }

    #[test]
    fn a_stopped_scan_reports_the_stop() {
        use codesage_protocol::work::{StopReason, WorkControl};
        let control = WorkControl::new(None);
        control.cancel(StopReason::ClientCancelled);
        let _scope = control.enter();
        let src = "void f(void)\n{\n#ifdef X\n    a();\n#endif\n}\n";
        assert!(select_split_arms(src.as_bytes()).is_err());
    }
}
