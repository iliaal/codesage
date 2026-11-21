//! Repair C/C++ macro invocations that tree-sitter reads as declarators.
//!
//! CodeSage never runs the preprocessor, so a macro invocation is parsed as
//! whatever C or C++ syntax it resembles. Four shapes mint a function named
//! after the macro and drop, or swallow, the real definition:
//!
//! - an annotation after a declarator, `void f(int a) LOCKS_REQUIRED(mu) {`
//!   or `int g_ GUARDED_BY(mu_);`, reads as a second function declarator
//!   and takes the body;
//! - an attribute macro between the return type and the name,
//!   `void printflike(1, 2) log_debug(...) {`, reads as the declarator;
//! - a statement or declaration macro written without a semicolon, a run of
//!   `Z_PARAM_LONG(x)` lines in a body or `EXPORT_FUNC(PyA)` lines at file
//!   scope, joins the following line into one declaration;
//! - a file-scope invocation with a brace initializer,
//!   `REGISTER(Factory, Base){ "name" };`, reads as a function body.
//!
//! [`find`] locates these lexically: annotations are blanked to spaces, and a
//! `;` is inserted after a semicolonless invocation (before the brace of an
//! initializer), so the invocation stays a call of the macro. Blanking keeps
//! every byte offset; `crate::parse` removes the inserted bytes from the tree
//! after parsing, so offsets, lines, and columns are those of the original.
//!
//! A function-defining macro followed by a body (`TEST_F(A, B) {`,
//! `PHP_FUNCTION(f)\n{`) is never changed, and neither is a declaration whose
//! head is a single all-caps function name (`static zend_long SEED(void)`).

use crate::export_macro::{Kind, Lexer, Token};
use anyhow::Result;

/// Tokens between cancellation checks.
const CHECK_EVERY: usize = 4096;
/// Byte ranges to blank and byte offsets at which to insert a `;`, both
/// ascending and disjoint.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Repairs {
    pub(crate) blanks: Vec<(usize, usize)>,
    pub(crate) insertions: Vec<usize>,
}

impl Repairs {
    pub(crate) fn is_empty(&self) -> bool {
        self.blanks.is_empty() && self.insertions.is_empty()
    }

    /// Whether the repairs take away the definition whose head (definition
    /// start to body) and name span these ranges: its name lies in a blanked
    /// annotation, or a `;` is inserted inside its head, splitting it.
    pub(crate) fn removes(
        &self,
        head: std::ops::Range<usize>,
        name: std::ops::Range<usize>,
    ) -> bool {
        self.blanks
            .iter()
            .any(|&(start, end)| name.start < end && start < name.end)
            || self
                .insertions
                .iter()
                .any(|offset| head.start <= *offset && *offset <= head.end)
    }

    /// `source` with the blanks applied and the `;` bytes inserted.
    pub(crate) fn apply(&self, source: &[u8]) -> Vec<u8> {
        let mut blanked = source.to_vec();
        for &(start, end) in &self.blanks {
            for byte in &mut blanked[start..end] {
                if *byte != b'\n' && *byte != b'\r' {
                    *byte = b' ';
                }
            }
        }
        let mut out = Vec::with_capacity(blanked.len() + self.insertions.len());
        let mut copied = 0;
        for &offset in &self.insertions {
            out.extend_from_slice(&blanked[copied..offset]);
            out.push(b';');
            copied = offset;
        }
        out.extend_from_slice(&blanked[copied..]);
        out
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Scope {
    /// File, namespace, `extern "C"`, or class body.
    Declarations,
    /// Function body or a statement block inside one.
    Statements,
    /// Initializers, enum bodies, and braces whose role is unknown.
    Other,
}

struct Frame {
    scope: Scope,
    /// First token of the current declaration or statement.
    start: usize,
    parens: Vec<usize>,
}

const STATEMENT_KEYS: [&[u8]; 14] = [
    b"return",
    b"break",
    b"continue",
    b"goto",
    b"if",
    b"for",
    b"while",
    b"switch",
    b"do",
    b"case",
    b"default",
    b"throw",
    b"try",
    b"co_return",
];

const DECLARATION_KEYS: [&[u8]; 38] = [
    b"void",
    b"bool",
    b"_Bool",
    b"char",
    b"short",
    b"int",
    b"long",
    b"float",
    b"double",
    b"signed",
    b"unsigned",
    b"auto",
    b"wchar_t",
    b"char8_t",
    b"char16_t",
    b"char32_t",
    b"const",
    b"volatile",
    b"static",
    b"inline",
    b"extern",
    b"register",
    b"_Noreturn",
    b"_Thread_local",
    b"thread_local",
    b"constexpr",
    b"typedef",
    b"struct",
    b"union",
    b"enum",
    b"class",
    b"namespace",
    b"template",
    b"using",
    b"virtual",
    b"explicit",
    b"friend",
    b"typename",
];

/// Words that open a parenthesized type or attribute operand rather than a
/// macro invocation.
const OPERAND_KEYS: [&[u8]; 16] = [
    b"decltype",
    b"typeof",
    b"__typeof__",
    b"__typeof",
    b"_Atomic",
    b"__attribute__",
    b"__attribute",
    b"__declspec",
    b"alignas",
    b"_Alignas",
    b"noexcept",
    b"throw",
    b"sizeof",
    b"asm",
    b"__asm__",
    b"operator",
];

const QUALIFIER_KEYS: [&[u8]; 17] = [
    b"const",
    b"volatile",
    b"restrict",
    b"static",
    b"inline",
    b"extern",
    b"register",
    b"_Noreturn",
    b"_Atomic",
    b"_Thread_local",
    b"thread_local",
    b"constexpr",
    b"struct",
    b"union",
    b"enum",
    b"class",
    b"typename",
];

const PRIMITIVE_KEYS: [&[u8]; 16] = [
    b"void",
    b"bool",
    b"_Bool",
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

/// Lexically locate the repairs described in the module documentation.
pub(crate) fn find(source: &[u8]) -> Result<Repairs> {
    let tokens: Vec<Token> = Lexer::new(source).collect();
    let mut scan = Scan {
        tokens: &tokens,
        source,
        defines: function_like_defines(&tokens, source),
        frames: vec![Frame {
            scope: Scope::Declarations,
            start: 0,
            parens: Vec::new(),
        }],
        prev: None,
        boundary: true,
        repairs: Repairs::default(),
    };
    let mut i = 0;
    while i < tokens.len() {
        if i.is_multiple_of(CHECK_EVERY) {
            codesage_protocol::work::checkpoint()?;
        }
        i = scan.step(i);
    }
    Ok(scan.repairs)
}

struct Scan<'a> {
    tokens: &'a [Token],
    source: &'a [u8],
    defines: Vec<&'a [u8]>,
    frames: Vec<Frame>,
    /// Last significant token that was not blanked; directives are skipped.
    prev: Option<usize>,
    /// A statement or declaration starts at the next significant token.
    boundary: bool,
    repairs: Repairs,
}

impl<'a> Scan<'a> {
    fn step(&mut self, i: usize) -> usize {
        let t = self.tokens[i];
        if t.kind == Kind::Directive {
            return i + 1;
        }
        let at_start = self.boundary;
        self.boundary = false;
        let frame = self.frames.last_mut().expect("file frame");
        match t.kind {
            Kind::Punct(b'(') => frame.parens.push(i),
            Kind::Punct(b')') => {
                frame.parens.pop();
            }
            Kind::Punct(b'{') => {
                let scope = if frame.parens.is_empty() {
                    self.classify_brace(i)
                } else {
                    Scope::Other
                };
                self.frames.push(Frame {
                    scope,
                    start: i + 1,
                    parens: Vec::new(),
                });
                self.boundary = true;
            }
            Kind::Punct(b'}') => {
                if self.frames.len() > 1 {
                    self.frames.pop();
                }
                let frame = self.frames.last_mut().expect("file frame");
                if frame.parens.is_empty() {
                    frame.start = i + 1;
                    self.boundary = true;
                }
            }
            Kind::Punct(b';') if frame.parens.is_empty() => {
                frame.start = i + 1;
                self.boundary = true;
            }
            Kind::Punct(b':') if frame.parens.is_empty() => {
                let scope = frame.scope;
                let access = self
                    .prev
                    .is_some_and(|p| matches!(self.text(p), b"public" | b"private" | b"protected"));
                if scope == Scope::Statements || (scope == Scope::Declarations && access) {
                    self.frames.last_mut().expect("file frame").start = i + 1;
                    self.boundary = true;
                }
            }
            Kind::Ident if frame.parens.is_empty() => {
                let scope = frame.scope;
                let next = match scope {
                    Scope::Declarations => self
                        .semicolonless_declaration(i, at_start)
                        .or_else(|| (!at_start).then(|| self.annotation(i)).flatten()),
                    Scope::Statements => self.semicolonless_statement(i, at_start),
                    Scope::Other => None,
                };
                if let Some(next) = next {
                    return next;
                }
            }
            _ => {}
        }
        self.prev = Some(i);
        i + 1
    }

    fn text(&self, i: usize) -> &'a [u8] {
        let t = self.tokens[i];
        &self.source[t.start..t.end]
    }

    fn is_ident(&self, i: usize, words: &[&[u8]]) -> bool {
        self.tokens
            .get(i)
            .is_some_and(|t| t.kind == Kind::Ident && words.contains(&self.text(i)))
    }

    fn kind(&self, i: usize) -> Option<Kind> {
        self.tokens.get(i).map(|t| t.kind)
    }

    fn macro_shaped(&self, i: usize) -> bool {
        self.kind(i) == Some(Kind::Ident) && macro_shaped(self.text(i))
    }

    /// The next significant token at or after `i`, skipping directives.
    fn next_significant(&self, mut i: usize) -> Option<usize> {
        while self.kind(i)? == Kind::Directive {
            i += 1;
        }
        Some(i)
    }

    fn line_break_between(&self, end: usize, start: usize) -> bool {
        self.source[end..start].contains(&b'\n')
    }

    fn at_line_start(&self, i: usize) -> bool {
        let start = self.tokens[i].start;
        self.source[..start]
            .iter()
            .rev()
            .take_while(|b| **b != b'\n')
            .all(u8::is_ascii_whitespace)
    }

    fn classify_brace(&self, i: usize) -> Scope {
        let frame = self.frames.last().expect("file frame");
        let Some(prev) = self.prev else {
            return match frame.scope {
                Scope::Other => Scope::Other,
                _ => frame.scope,
            };
        };
        let prev_kind = self.tokens[prev].kind;
        match frame.scope {
            Scope::Declarations => {
                if prev_kind == Kind::Punct(b')') {
                    return Scope::Statements;
                }
                let head = frame.start.min(i)..i;
                if head.clone().any(|j| self.is_ident(j, &[b"enum"])) {
                    return Scope::Other;
                }
                if head
                    .clone()
                    .any(|j| self.is_ident(j, &[b"class", b"struct", b"union", b"namespace"]))
                {
                    return Scope::Declarations;
                }
                if prev_kind == Kind::Str
                    && prev
                        .checked_sub(1)
                        .is_some_and(|p| self.is_ident(p, &[b"extern"]))
                {
                    return Scope::Declarations;
                }
                if self.is_ident(
                    prev,
                    &[
                        b"const",
                        b"override",
                        b"final",
                        b"noexcept",
                        b"mutable",
                        b"volatile",
                        b"try",
                    ],
                ) || matches!(prev_kind, Kind::Punct(b'}' | b'&'))
                {
                    return Scope::Statements;
                }
                Scope::Other
            }
            Scope::Statements => {
                if matches!(
                    prev_kind,
                    Kind::Punct(b')' | b';' | b'{' | b'}' | b':' | b']')
                ) || self.is_ident(prev, &[b"else", b"do", b"try"])
                {
                    Scope::Statements
                } else {
                    Scope::Other
                }
            }
            Scope::Other => Scope::Other,
        }
    }

    /// Index just past the `( ... )` that opens at `open`, when it does.
    fn after_args(&self, open: usize) -> Option<usize> {
        if self.kind(open) != Some(Kind::Punct(b'(')) {
            return None;
        }
        crate::export_macro::after_args(self.tokens, open - 1)
    }

    fn insert_after(&mut self, last: usize) {
        self.repairs.insertions.push(self.tokens[last].end);
        self.frames.last_mut().expect("file frame").start = last + 1;
        self.boundary = true;
        self.prev = Some(last);
    }

    /// A statement-position macro invocation, `NAME(args)` or a bare
    /// `NAME`, alone on its line and followed on a later line by another
    /// statement macro, a statement keyword, or (after an argument list) the
    /// closing brace. A following lowercase word may be the declarator of a
    /// macro-wrapped type (`TYPE(int)\nvalue;`), and a following `{` is the
    /// body of a loop macro; both are left alone, and so is an unbraced
    /// control body, which never starts a statement here.
    fn semicolonless_statement(&mut self, i: usize, at_start: bool) -> Option<usize> {
        if !at_start || !self.macro_shaped(i) || !self.at_line_start(i) {
            return None;
        }
        let (end, has_args) = match self.after_args(i + 1) {
            Some(end) => (end, true),
            None if self.kind(i + 1) == Some(Kind::Punct(b'(')) => return None,
            None => (i + 1, false),
        };
        let next = self.next_significant(end)?;
        if !self.line_break_between(self.tokens[end - 1].end, self.tokens[next].start) {
            return None;
        }
        let follows = self.is_ident(next, &STATEMENT_KEYS)
            || (self.macro_shaped(next)
                && (has_args || self.kind(next + 1) == Some(Kind::Punct(b'('))))
            || (has_args && self.kind(next) == Some(Kind::Punct(b'}')));
        if !follows {
            return None;
        }
        self.insert_after(end - 1);
        Some(end)
    }

    /// A declaration-position invocation `NAME(args)` alone on its line and
    /// followed on a later line by what starts a declaration of its own, or a
    /// brace initializer `NAME(args){ "a", "b" };`.
    fn semicolonless_declaration(&mut self, i: usize, at_start: bool) -> Option<usize> {
        if !at_start || !self.macro_shaped(i) || !self.at_line_start(i) {
            return None;
        }
        let end = self.after_args(i + 1)?;
        if self.kind(end) == Some(Kind::Punct(b'{')) {
            let close = self.initializer_close(end)?;
            self.repairs.insertions.push(self.tokens[end - 1].end);
            self.frames.last_mut().expect("file frame").start = close + 2;
            self.boundary = true;
            self.prev = Some(close + 1);
            return Some(close + 2);
        }
        let next = self.next_significant(end)?;
        if !self.line_break_between(self.tokens[end - 1].end, self.tokens[next].start)
            || !self.starts_declaration(next)
        {
            return None;
        }
        self.insert_after(end - 1);
        Some(end)
    }

    /// The closing brace of `{ ... };` holding no statement terminator and
    /// no nested brace, or `None`. A function-defining macro's body holds
    /// statements or nothing.
    fn initializer_close(&self, open: usize) -> Option<usize> {
        let mut j = open + 1;
        loop {
            match self.kind(j)? {
                Kind::Punct(b'}') => break,
                Kind::Punct(b';' | b'{') | Kind::Directive => return None,
                _ => j += 1,
            }
        }
        (j > open + 1 && self.kind(j + 1) == Some(Kind::Punct(b';'))).then_some(j)
    }

    fn starts_declaration(&self, i: usize) -> bool {
        match self.kind(i) {
            Some(Kind::Punct(b'}')) => true,
            Some(Kind::Punct(b'[')) => self.kind(i + 1) == Some(Kind::Punct(b'[')),
            Some(Kind::Ident) => {
                self.macro_shaped(i)
                    || self.is_ident(i, &DECLARATION_KEYS)
                    || matches!(
                        self.kind(i + 1),
                        Some(Kind::Ident | Kind::Scope | Kind::Punct(b'*' | b'<'))
                    )
            }
            _ => false,
        }
    }

    /// An annotation invocation inside a declaration head, blanked to spaces:
    /// `NAME(args)` after a declarator, or `name(args)` between the return
    /// type and the declarator it precedes.
    fn annotation(&mut self, i: usize) -> Option<usize> {
        if self.is_ident(i, &OPERAND_KEYS) {
            return None;
        }
        let end = self.after_args(i + 1)?;
        let frame_start = self.frames.last().expect("file frame").start;
        let prev = self.prev.filter(|p| *p >= frame_start)?;
        let qualifies = if self.macro_shaped(i) {
            self.trails_declarator(prev, end)
        } else {
            false
        } || self.precedes_declarator(i, prev, end, frame_start);
        if !qualifies {
            return None;
        }
        let start = self.tokens[i].start;
        let stop = self.tokens[end - 1].end;
        self.repairs.blanks.push((start, stop));
        Some(end)
    }

    /// `) NAME(args)` after a declarator's parameter list, or
    /// `type name NAME(args)` after a declarator name, before what can
    /// follow a declarator.
    fn trails_declarator(&self, prev: usize, end: usize) -> bool {
        let follows = |allow_body: bool| match self.kind(end) {
            Some(Kind::Punct(b';' | b'=' | b',')) => true,
            Some(Kind::Punct(b'{' | b':')) => allow_body,
            Some(Kind::Ident) => {
                allow_body
                    && (self.macro_shaped(end)
                        || self.is_ident(end, &[b"const", b"override", b"final", b"noexcept"]))
            }
            _ => false,
        };
        match self.tokens[prev].kind {
            Kind::Punct(b')') => self.closes_declarator_parameters(prev) && follows(true),
            Kind::Ident => {
                let frame_start = self.frames.last().expect("file frame").start;
                !self.macro_shaped(prev)
                    && !self.is_ident(prev, &QUALIFIER_KEYS)
                    && !self.is_ident(prev, &PRIMITIVE_KEYS)
                    && prev > frame_start
                    && self.is_type_word(prev - 1)
                    && follows(false)
            }
            _ => false,
        }
    }

    fn is_type_word(&self, i: usize) -> bool {
        match self.kind(i) {
            Some(Kind::Ident) => {
                !self.is_ident(i, &QUALIFIER_KEYS) || self.is_ident(i, &PRIMITIVE_KEYS)
            }
            Some(Kind::Punct(b'*' | b'&' | b'>')) => true,
            _ => false,
        }
    }

    /// Whether the `)` at `close` ends `type name(...)`: a declarator's
    /// parameter list, not a typeless invocation such as `DEFINE(a, b)`.
    fn closes_declarator_parameters(&self, close: usize) -> bool {
        let Some(open) = matching_open(self.tokens, close) else {
            return false;
        };
        let Some(mut name) = open.checked_sub(1) else {
            return false;
        };
        if self.kind(name) != Some(Kind::Ident) || self.is_ident(name, &OPERAND_KEYS) {
            return false;
        }
        while name >= 2
            && self.kind(name - 1) == Some(Kind::Scope)
            && self.kind(name - 2) == Some(Kind::Ident)
        {
            name -= 2;
        }
        let frame_start = self.frames.last().expect("file frame").start;
        name > frame_start && self.is_type_word(name - 1)
    }

    /// `type name(args) declarator(`: two declarators in a row are never C,
    /// so the first is an annotation when it is macro-shaped, `#define`d as
    /// a function-like macro earlier in the file, or given a literal argument
    /// that no parameter list holds.
    fn precedes_declarator(&self, i: usize, prev: usize, end: usize, frame_start: usize) -> bool {
        let mut declarator = end;
        while self.kind(declarator) == Some(Kind::Punct(b'*')) {
            declarator += 1;
        }
        if self.kind(declarator) != Some(Kind::Ident)
            || self.macro_shaped(declarator)
            || self.is_ident(declarator, &DECLARATION_KEYS)
            || self.is_ident(declarator, &OPERAND_KEYS)
            || self.kind(declarator + 1) != Some(Kind::Punct(b'('))
            || prev < frame_start
            || !self.is_type_word(prev)
        {
            return false;
        }
        let name = self.text(i);
        macro_shaped(name)
            || self.defines.contains(&name)
            || (i + 2..end - 1).any(|j| {
                self.kind(j) == Some(Kind::Str)
                    || (self.kind(j) == Some(Kind::Other)
                        && self.text(j).first().is_some_and(u8::is_ascii_digit))
            })
    }
}

fn matching_open(tokens: &[Token], close: usize) -> Option<usize> {
    let mut depth = 0usize;
    for j in (0..=close).rev() {
        match tokens[j].kind {
            Kind::Punct(b')') => depth += 1,
            Kind::Punct(b'(') => {
                depth -= 1;
                if depth == 0 {
                    return Some(j);
                }
            }
            Kind::Punct(b';' | b'{' | b'}') | Kind::Directive => return None,
            _ => {}
        }
    }
    None
}

/// Names of function-like macros a `#define NAME(` directive introduces.
fn function_like_defines<'a>(tokens: &[Token], source: &'a [u8]) -> Vec<&'a [u8]> {
    let mut names = Vec::new();
    for t in tokens.iter().filter(|t| t.kind == Kind::Directive) {
        let text = &source[t.start..t.end];
        let rest = trim_blanks(&text[1..]);
        let Some(rest) = rest.strip_prefix(b"define") else {
            continue;
        };
        let rest = trim_blanks(rest);
        let len = rest
            .iter()
            .take_while(|b| b.is_ascii_alphanumeric() || **b == b'_')
            .count();
        if len > 0 && rest.get(len) == Some(&b'(') {
            names.push(&rest[..len]);
        }
    }
    names
}

fn trim_blanks(text: &[u8]) -> &[u8] {
    let skip = text
        .iter()
        .take_while(|b| matches!(b, b' ' | b'\t'))
        .count();
    &text[skip..]
}

/// `FOO`, `Z_PARAM_LONG`, `_ASSERT`: uppercase letters, digits, and
/// underscores, with at least one letter.
pub(crate) fn macro_shaped(text: &[u8]) -> bool {
    text.iter().any(u8::is_ascii_uppercase)
        && text
            .iter()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || *b == b'_')
        && !text.first().is_some_and(u8::is_ascii_digit)
}
