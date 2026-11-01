//! Pick one arm of a C/C++ conditional group whose arms each hold part of a
//! statement, before tree-sitter sees the file.
//!
//! tree-sitter parses every arm of `#if`/`#else` as if both were compiled.
//! When each arm opens the same loop or switch header,
//! (`#ifdef X\n for (a) {\n#else\n for (b) {\n#endif`), the parse sees two
//! unclosed braces, and error recovery drops the enclosing function
//! definition with every call in it. An arm that starts with `else` and has
//! no `if` of its own in the arm is the same shape.
//!
//! For such a group the copy handed to the parser keeps one arm as written
//! and blanks the directive lines. Every other arm stays in place with only
//! what ties it to code outside the arm blanked: braces it opens or closes
//! without a partner, and a leading `else`. What remains of it is complete
//! statements, so its calls are still indexed inside the enclosing function.
//! An arm no build reaches (`#if 0`, or after `#if 1`), and an arm that
//! leaves a parenthesis unbalanced (a piece of an expression), is blanked
//! whole.
//! Blanking writes spaces and keeps newlines, so the tree's byte offsets and
//! lines are those of the original source; extractors read node text from
//! the original. Groups at file scope, groups whose arms are each balanced,
//! and groups whose arms disagree on how many braces or parentheses they
//! leave open are left exactly as written.

use crate::export_macro::{Kind, Lexer};

/// Return a same-length copy of `source` with split conditional groups
/// reduced, or `None` when no group qualifies.
pub(crate) fn select_split_arms(source: &[u8]) -> Option<Vec<u8>> {
    if !has_conditional_directive(source) {
        return None;
    }
    let mut stack: Vec<Group> = Vec::new();
    let mut blanks: Vec<(usize, usize)> = Vec::new();
    // Brace depth of the code outside every open group, as the parser will
    // see it once resolved groups are reduced.
    let mut outer_depth = 0i32;
    for token in Lexer::new(source) {
        if token.kind == Kind::Directive {
            match directive(&source[token.start..token.end]) {
                Some(Directive::If(literal)) => {
                    let depth = outer_depth
                        + stack
                            .iter()
                            .filter_map(|group| group.arms.last())
                            .map(|arm| arm.braces.net)
                            .sum::<i32>();
                    stack.push(Group::open(token.start, token.end, literal, depth));
                }
                Some(Directive::Else(literal)) => {
                    if let Some(group) = stack.last_mut() {
                        group.arms.push(Arm::new(token.start, token.end, literal));
                    }
                }
                Some(Directive::Endif) => {
                    if let Some(mut group) = stack.pop() {
                        group.endif = (token.start, token.end);
                        let arm = group.resolve(&mut blanks);
                        match stack.last_mut().and_then(|g| g.arms.last_mut()) {
                            Some(parent) => parent.absorb(arm),
                            None => outer_depth += arm.braces.net,
                        }
                    }
                }
                None => {}
            }
            continue;
        }
        let Some(arm) = stack.last_mut().and_then(|group| group.arms.last_mut()) else {
            match token.kind {
                Kind::Punct(b'{') => outer_depth += 1,
                Kind::Punct(b'}') => outer_depth -= 1,
                _ => {}
            }
            continue;
        };
        match token.kind {
            Kind::Ident => arm.push_ident(&source[token.start..token.end], token.start, token.end),
            Kind::Punct(b) => arm.push_punct(b, token.start),
            _ => arm.push_other(),
        }
    }
    if blanks.is_empty() {
        return None;
    }
    let mut out = source.to_vec();
    for (start, end) in blanks {
        for byte in &mut out[start..end] {
            if *byte != b'\n' && *byte != b'\r' {
                *byte = b' ';
            }
        }
    }
    Some(out)
}

/// Cheap gate: a line whose first non-blank byte is `#` followed by `if`.
fn has_conditional_directive(source: &[u8]) -> bool {
    let mut line_start = true;
    let mut i = 0;
    while i < source.len() {
        let b = source[i];
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
        i += 1;
    }
    false
}

enum Directive {
    /// `#if`, `#ifdef`, `#ifndef`, with the literal value of a `#if 0` or `#if 1`.
    If(Option<bool>),
    /// `#elif`, `#elifdef`, `#elifndef`, `#else`.
    Else(Option<bool>),
    Endif,
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

/// Where the arm's token stream stands for deciding whether an `else` is
/// dangling: at the arm's start, right after a closer with no opener in the
/// arm, or anywhere else.
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
    brace: bool,
    else_after: Option<(usize, usize)>,
}

#[derive(Default)]
struct Brackets {
    /// Offsets of `{` and `(` not yet closed.
    open_braces: Vec<usize>,
    open_parens: Vec<usize>,
    closers: Vec<Closer>,
    leading_else: Option<(usize, usize)>,
}

struct Arm {
    directive: (usize, usize),
    literal: Option<bool>,
    braces: Depth,
    parens: Depth,
    first: First,
    position: Position,
    brackets: Brackets,
    /// A nested group was left as written while not balanced, so the arm's
    /// bracket offsets no longer describe what the parser sees.
    opaque: bool,
}

impl Arm {
    fn new(start: usize, end: usize, literal: Option<bool>) -> Self {
        Self {
            directive: (start, end),
            literal,
            braces: Depth::default(),
            parens: Depth::default(),
            first: First::Unseen,
            position: Position::Start,
            brackets: Brackets::default(),
            opaque: false,
        }
    }

    fn see(&mut self, first: First) {
        if self.first == First::Unseen {
            self.first = first;
        }
    }

    fn take_else(&mut self, span: (usize, usize)) {
        match self.position {
            Position::Start => self.brackets.leading_else = Some(span),
            Position::AfterCloser => {
                if let Some(closer) = self.brackets.closers.last_mut() {
                    closer.else_after = Some(span);
                }
            }
            Position::Inside => {}
        }
    }

    fn push_ident(&mut self, ident: &[u8], start: usize, end: usize) {
        let is_else = ident == b"else";
        if is_else {
            self.take_else((start, end));
        }
        self.position = Position::Inside;
        self.see(if is_else { First::Else } else { First::Other });
    }

    fn push_punct(&mut self, b: u8, at: usize) {
        self.see(First::Other);
        self.position = Position::Inside;
        match b {
            b'{' => {
                self.braces.step(1);
                self.brackets.open_braces.push(at);
            }
            b'(' => {
                self.parens.step(1);
                self.brackets.open_parens.push(at);
            }
            b'}' => {
                self.braces.step(-1);
                self.close(at, true);
            }
            b')' => {
                self.parens.step(-1);
                self.close(at, false);
            }
            _ => {}
        }
    }

    fn close(&mut self, at: usize, brace: bool) {
        let opener = if brace {
            self.brackets.open_braces.pop()
        } else {
            self.brackets.open_parens.pop()
        };
        if opener.is_none() {
            self.brackets.closers.push(Closer {
                at,
                brace,
                else_after: None,
            });
            self.position = Position::AfterCloser;
        }
    }

    fn push_other(&mut self) {
        self.see(First::Other);
        self.position = Position::Inside;
    }

    /// Fold in the arm a nested group resolved to, as if its tokens had been
    /// written in this arm.
    fn absorb(&mut self, child: Arm) {
        self.opaque |= child.opaque;
        self.braces.then(child.braces);
        self.parens.then(child.parens);
        if child.first == First::Unseen {
            return;
        }
        self.see(child.first);
        if let Some(span) = child.brackets.leading_else {
            self.take_else(span);
        }
        if child.first != First::Else || self.position == Position::Start {
            self.position = Position::Inside;
        }
        for closer in child.brackets.closers {
            self.position = Position::Inside;
            let opener = if closer.brace {
                self.brackets.open_braces.pop()
            } else {
                self.brackets.open_parens.pop()
            };
            if opener.is_none() {
                self.brackets.closers.push(closer);
                self.position = Position::AfterCloser;
            }
        }
        if child.position != Position::AfterCloser {
            self.position = Position::Inside;
        }
        self.brackets.open_braces.extend(child.brackets.open_braces);
        self.brackets.open_parens.extend(child.brackets.open_parens);
    }

    fn balanced(&self) -> bool {
        self.braces.balanced() && self.parens.balanced()
    }

    /// Parses as a unit on its own: brackets balanced and not a dangling
    /// `else` that needs an `if` outside the arm.
    fn whole(&self) -> bool {
        self.balanced() && self.first != First::Else
    }

    /// Blank what ties a non-kept arm to the code around it, leaving its
    /// remaining statements, and their calls, in place. Returns false when
    /// the arm must be blanked whole instead.
    fn detach(&self, blanks: &mut Vec<(usize, usize)>) -> bool {
        // An unbalanced parenthesis means the arm is a piece of an
        // expression (`if (a >= 0` / `if ((a > 0 || b)`); with its bracket
        // removed it would be spliced into the kept arm's expression.
        if self.opaque || !self.parens.balanced() {
            return false;
        }
        let b = &self.brackets;
        blanks.extend(b.leading_else);
        for closer in &b.closers {
            blanks.push((closer.at, closer.at + 1));
            blanks.extend(closer.else_after);
        }
        blanks.extend(
            b.open_braces
                .iter()
                .chain(&b.open_parens)
                .map(|&at| (at, at + 1)),
        );
        true
    }
}

struct Group {
    arms: Vec<Arm>,
    endif: (usize, usize),
    /// Brace depth where the group opens. Only groups inside a body are
    /// reduced: at file scope the split shapes are alternative declaration
    /// heads, where keeping one arm would also pick which name is indexed,
    /// and tree-sitter already recovers those without help.
    depth: i32,
}

impl Group {
    fn open(start: usize, end: usize, literal: Option<bool>, depth: i32) -> Self {
        Self {
            arms: vec![Arm::new(start, end, literal)],
            endif: (end, end),
            depth,
        }
    }

    /// Record the blanking this group needs and return the arm that stands
    /// for it in the enclosing arm's accounting. A group left as written
    /// with arms that are not each balanced returns its first live arm
    /// marked opaque: its depth still counts, its offsets do not.
    fn resolve(mut self, blanks: &mut Vec<(usize, usize)>) -> Arm {
        let Some(kept) = self.arms.iter().position(|arm| arm.literal != Some(false)) else {
            // Every arm is a literal `#if 0` / `#elif 0`: nothing is parsed
            // as code in any build, and `preproc::DeadRegions` masks it.
            return Arm::new(0, 0, None);
        };
        if self.arms.iter().all(Arm::whole) {
            return self.arms.swap_remove(kept);
        }
        let split = self.depth > 0
            && if self.arms.len() == 1 {
                // Without `#else`, the configuration that skips the arm keeps
                // nothing, so the arm may only be joined to the surrounding
                // code when it leaves the depth unchanged (`else ... { }`,
                // `} else {`). A lone arm that opens a brace (`extern "C" {`
                // under `#ifdef __cplusplus`) is closed by a matching group
                // elsewhere and parses as written.
                let arm = &self.arms[0];
                arm.braces.net == 0 && arm.parens.net == 0
            } else {
                let first = &self.arms[0];
                self.arms.iter().all(|arm| {
                    arm.braces.net == first.braces.net && arm.parens.net == first.parens.net
                })
            };
        if !split {
            let mut arm = self.arms.swap_remove(kept);
            arm.opaque = true;
            return arm;
        }
        // After a literal `#if 1` / `#elif 1` arm, no later arm is reachable.
        let taken = self.arms[kept].literal == Some(true);
        for (i, arm) in self.arms.iter().enumerate() {
            blanks.push(arm.directive);
            if i == kept {
                continue;
            }
            let dead = arm.literal == Some(false) || (taken && i > kept);
            if dead || !arm.detach(blanks) {
                let end = self
                    .arms
                    .get(i + 1)
                    .map_or(self.endif.0, |next| next.directive.0);
                blanks.push((arm.directive.1, end));
            }
        }
        blanks.push(self.endif);
        self.arms.swap_remove(kept)
    }
}

#[cfg(test)]
mod tests {
    use super::select_split_arms;

    fn reduced(src: &str) -> Option<String> {
        let out = select_split_arms(src.as_bytes())?;
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
}
