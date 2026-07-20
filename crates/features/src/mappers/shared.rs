//! Shared helpers for per-language mappers: safe directory walks (realpath
//! + escape detection), normalized path strings, and glob-style listing.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Result;
use globset::GlobSet;

use crate::mappers::types::MapperContext;

/// Returns true if `candidate` is inside `root` after symlink resolution.
/// Used to defend mappers against repos that contain symlinks pointing
/// outside the tree (rare but real; clawpatch's mapper guard inspired this).
pub fn is_inside_root(root: &Path, candidate: &Path) -> bool {
    let real_root = fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let real_candidate = fs::canonicalize(candidate).unwrap_or_else(|_| candidate.to_path_buf());
    real_candidate.starts_with(&real_root)
}

/// File exists, is a regular file, is inside `root` after symlink check, and
/// is not itself a symlink. Returns false on any I/O error.
pub fn is_safe_file(root: &Path, candidate: &Path) -> bool {
    let Ok(meta) = fs::symlink_metadata(candidate) else {
        return false;
    };
    if !meta.is_file() || meta.file_type().is_symlink() {
        return false;
    }
    is_inside_root(root, candidate)
}

/// Directory exists, is a real directory (not a symlink), and is inside
/// `root` after canonicalization.
pub fn is_safe_dir(root: &Path, candidate: &Path) -> bool {
    let Ok(meta) = fs::symlink_metadata(candidate) else {
        return false;
    };
    if !meta.is_dir() || meta.file_type().is_symlink() {
        return false;
    }
    is_inside_root(root, candidate)
}

/// Convert an absolute path inside `root` to a repo-relative POSIX string.
/// Falls back to the absolute path on canonicalization failure.
pub fn rel_path(root: &Path, abs: &Path) -> String {
    let stripped = abs.strip_prefix(root).unwrap_or(abs);
    let s = stripped.to_string_lossy().into_owned();
    s.replace('\\', "/")
}

/// Walk a directory subtree under `start`, yielding repo-relative file
/// paths. **Honors `.gitignore`, the project's `[index].exclude_patterns`
/// when supplied, and a built-in hard-exclude list** — i.e. ignored
/// sibling worktrees, vendored deps, build output, and editor caches
/// don't leak into mapper output. Symlinks are skipped. Bounded by
/// `max_files`; returns the partial set when exceeded.
pub fn walk_files(
    root: &Path,
    start: &Path,
    max_files: usize,
    excludes: Option<&GlobSet>,
) -> Vec<String> {
    if !is_safe_dir(root, start) {
        return Vec::new();
    }
    if should_skip(&rel_path(root, start)) {
        return Vec::new();
    }
    let walker = ignore::WalkBuilder::new(start)
        .hidden(true)
        .git_ignore(true)
        .require_git(false)
        .build();

    let mut out: Vec<String> = Vec::new();
    for entry in walker.flatten() {
        if out.len() >= max_files {
            break;
        }
        let path = entry.path();
        let Some(ft) = entry.file_type() else {
            continue;
        };
        if !ft.is_file() || ft.is_symlink() {
            continue;
        }
        let rel = rel_path(root, path);
        if should_skip(&rel) {
            continue;
        }
        if excludes.is_some_and(|g| g.is_match(&rel)) {
            continue;
        }
        out.push(rel);
    }
    out.sort();
    out.dedup();
    out
}

/// Directory- or file-relative ignore predicate. Belt-and-suspenders to
/// `ignore::WalkBuilder`'s gitignore support: catches the directory names
/// that appear in mapper output even on repos whose gitignore is empty or
/// missing a relevant entry (vendored sandboxes, sandbox checkouts).
///
/// `.worktrees` is included specifically because the user-flow for git
/// worktrees in this repo plants them at `.worktrees/<branch>/...` and
/// they shouldn't surface as their own feature slices — they're the same
/// codebase at a different commit.
pub fn should_skip(rel: &str) -> bool {
    if rel.is_empty() || rel == "." {
        return false;
    }
    let parts = rel.split('/');
    for part in parts {
        match part {
            "node_modules" | "dist" | "build" | "coverage" | ".git" | ".codesage" | "vendor"
            | "target" | ".build" | ".next" | "__pycache__" | ".venv" | "venv" | ".worktrees"
            | "worktrees" => return true,
            _ => {}
        }
    }
    false
}

fn list_dir_children(
    root: &Path,
    dir: &Path,
    excludes: Option<&GlobSet>,
    want_dirs: bool,
    pred: &dyn Fn(&str) -> bool,
) -> Vec<String> {
    if !is_safe_dir(root, dir) {
        return Vec::new();
    }
    if should_skip(&rel_path(root, dir)) {
        return Vec::new();
    }
    let walker = ignore::WalkBuilder::new(dir)
        .max_depth(Some(1))
        .hidden(true)
        .git_ignore(true)
        .require_git(false)
        .build();
    let mut out: Vec<String> = Vec::new();
    for entry in walker.flatten() {
        // WalkBuilder yields the walk root itself at depth 0; skip it.
        if entry.path() == dir {
            continue;
        }
        let Some(ft) = entry.file_type() else {
            continue;
        };
        if ft.is_symlink() {
            continue;
        }
        if want_dirs != ft.is_dir() {
            continue;
        }
        let rel = rel_path(root, entry.path());
        if should_skip(&rel) {
            continue;
        }
        if excludes.is_some_and(|g| g.is_match(&rel)) {
            continue;
        }
        if !pred(&rel) {
            continue;
        }
        out.push(rel);
    }
    out.sort();
    out
}

/// Cap on per-feature source-file enumeration. Keeps the routing-only
/// `feature_files` rows bounded on monolithic monorepos.
pub const SOURCE_FILE_CAP: usize = 2_000;

/// Shared source-enumeration pipeline: walk each scan dir under the
/// project's exclusion contract (`walk_cap` bounds each raw walk), keep
/// files matching `ext_pred`, drop files matching `exclude_pred` (tests,
/// generated artifacts), dedup across dirs, stop at `cap`, sort.
pub fn collect_source_files(
    ctx: &MapperContext,
    scan_dirs: &[PathBuf],
    walk_cap: usize,
    ext_pred: impl Fn(&str) -> bool,
    exclude_pred: impl Fn(&str) -> bool,
    cap: usize,
) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut seen: BTreeSet<String> = BTreeSet::new();
    'dirs: for dir in scan_dirs {
        for rel in walk_files(ctx.root, dir, walk_cap, ctx.excludes) {
            if !ext_pred(&rel) || exclude_pred(&rel) {
                continue;
            }
            if !seen.insert(rel.clone()) {
                continue;
            }
            out.push(rel);
            if out.len() >= cap {
                break 'dirs;
            }
        }
    }
    out.sort();
    out
}

/// Depth-1 file listing of `dir` under the same skip contract as
/// [`walk_files`] (gitignore, hard-excludes, optional project excludes,
/// symlink rejection). Yields sorted repo-relative paths for which `pred`
/// holds. Replaces per-mapper hand-rolled `fs::read_dir` loops so single-
/// directory scans can't bypass the walker's filter rules.
pub fn list_dir_files(
    root: &Path,
    dir: &Path,
    excludes: Option<&GlobSet>,
    pred: impl Fn(&str) -> bool,
) -> Vec<String> {
    list_dir_children(root, dir, excludes, false, &pred)
}

/// Depth-1 listing of `dir`'s immediate child directories, same skip
/// contract as [`list_dir_files`]. Sorted repo-relative paths.
pub fn list_dir_subdirs(root: &Path, dir: &Path, excludes: Option<&GlobSet>) -> Vec<String> {
    list_dir_children(root, dir, excludes, true, &|_| true)
}

/// Read a file's contents to a UTF-8 string, returning None on any error
/// (missing file, binary, permission). Bounded by an internal 1 MB cap so
/// a stray large generated file doesn't blow up a mapper that only wants
/// to grep a header.
pub fn read_to_string_bounded(path: &Path) -> Result<Option<String>> {
    let meta = match fs::metadata(path) {
        Ok(m) => m,
        Err(_) => return Ok(None),
    };
    if meta.len() > 1_000_000 {
        return Ok(None);
    }
    Ok(fs::read_to_string(path).ok())
}

/// Strip TOML `#`-style line comments while leaving the strings inside
/// quoted values intact. Cheap helper used by mappers that parse
/// `Cargo.toml` or similar without pulling in a full toml crate. Matches
/// clawpatch's `stripLineComment` behavior.
pub fn strip_line_comments(source: &str, marker: char) -> String {
    let mut out = String::with_capacity(source.len());
    for line in source.lines() {
        out.push_str(&strip_line_comment(line, marker));
        out.push('\n');
    }
    out
}

fn strip_line_comment(line: &str, marker: char) -> String {
    let mut in_string = false;
    let mut escaped = false;
    let bytes = line.as_bytes();
    let mut cut: Option<usize> = None;
    for (i, &b) in bytes.iter().enumerate() {
        let c = b as char;
        if in_string {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_string = false;
            }
            continue;
        }
        if c == '"' {
            in_string = true;
        } else if c == marker {
            cut = Some(i);
            break;
        }
    }
    match cut {
        Some(i) => line[..i].to_string(),
        None => line.to_string(),
    }
}

/// How [`strip_comments`] emits `"…"` / `'…'` string literals.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StringMode {
    /// Copy the literal through verbatim, delimiters included.
    Preserve,
    /// Blank the whole literal, delimiters included.
    Blank,
    /// Keep the quote delimiters, blank the contents.
    BlankContents,
}

/// Per-language knobs for [`strip_comments`]. All variants blank `//` and
/// `/* */` comments; string handling and extra comment/literal syntax vary.
#[derive(Debug, Clone, Copy)]
pub struct CommentSyntax {
    /// Treat `#` as a line comment. `#[` is exempt — that's PHP attribute
    /// syntax, not a comment, and attributes on route definitions must
    /// survive the strip.
    pub hash_line_comments: bool,
    pub strings: StringMode,
    /// Recognize `` ` `` template literals (JS). Delimiters are preserved
    /// and contents always blanked regardless of [`Self::strings`] —
    /// embedded `${…}` interpolation would need balanced-brace tracking,
    /// and losing identifiers inside `${}` is an acceptable false negative.
    pub template_literals: bool,
}

/// Blank comments (and optionally strings) out of source text while
/// preserving byte-for-char offsets: every stripped char becomes a space,
/// newlines survive so line numbers still line up for downstream regex
/// scans. One scanner parameterized per language; the per-language configs
/// live next to their mappers.
pub fn strip_comments(input: &str, syntax: CommentSyntax) -> String {
    enum State {
        Code,
        LineComment,
        BlockComment,
        Str { quote: char, template: bool },
    }

    let mut out = String::with_capacity(input.len());
    let mut state = State::Code;
    let mut chars = input.chars().peekable();
    while let Some(ch) = chars.next() {
        match state {
            State::Code => match ch {
                '/' if chars.peek() == Some(&'/') => {
                    chars.next();
                    out.push_str("  ");
                    state = State::LineComment;
                }
                '/' if chars.peek() == Some(&'*') => {
                    chars.next();
                    out.push_str("  ");
                    state = State::BlockComment;
                }
                '#' if syntax.hash_line_comments && chars.peek() != Some(&'[') => {
                    out.push(' ');
                    state = State::LineComment;
                }
                '`' if syntax.template_literals => {
                    out.push('`');
                    state = State::Str {
                        quote: '`',
                        template: true,
                    };
                }
                '"' | '\'' => {
                    out.push(if syntax.strings == StringMode::Blank {
                        ' '
                    } else {
                        ch
                    });
                    state = State::Str {
                        quote: ch,
                        template: false,
                    };
                }
                _ => out.push(ch),
            },
            State::LineComment => {
                if ch == '\n' {
                    out.push('\n');
                    state = State::Code;
                } else {
                    out.push(' ');
                }
            }
            State::BlockComment => {
                if ch == '*' && chars.peek() == Some(&'/') {
                    chars.next();
                    out.push_str("  ");
                    state = State::Code;
                } else {
                    out.push(if ch == '\n' { '\n' } else { ' ' });
                }
            }
            State::Str { quote, template } => {
                let blank = template || syntax.strings != StringMode::Preserve;
                if ch == '\\' {
                    out.push(if blank { ' ' } else { '\\' });
                    if let Some(next) = chars.next() {
                        out.push(if !blank {
                            next
                        } else if next == '\n' {
                            '\n'
                        } else {
                            ' '
                        });
                    }
                } else if ch == quote {
                    out.push(if !template && syntax.strings == StringMode::Blank {
                        ' '
                    } else {
                        ch
                    });
                    state = State::Code;
                } else {
                    out.push(if !blank {
                        ch
                    } else if ch == '\n' {
                        '\n'
                    } else {
                        ' '
                    });
                }
            }
        }
    }
    out
}

/// Tag attached to route seeds whose shape suggests a privileged or
/// state-changing surface (see [`route_is_auth_sensitive`]). Kept as a
/// free-form seed tag rather than a derived trust boundary: the boundary
/// model stays single-sourced from parsed imports/references, while this
/// surfaces a route-shape heuristic for humans and agents reviewing the
/// slice. Ported from clawpatch's per-route boundary heuristic, demoted to
/// a tag here.
pub const AUTH_SENSITIVE_TAG: &str = "auth-sensitive";

/// Heuristic: does a route's shape suggest it needs a closer security look?
/// True when the method is anything other than `GET`/`HEAD` (i.e. it can
/// mutate state) or when a path segment is one of `admin` / `auth` /
/// `login` / `token`. `method` may be empty for frameworks that don't bind
/// a verb at the URL layer (e.g. Django), in which case only the path is
/// considered. Matching is segment-exact and case-insensitive, so
/// `/authors` does NOT match `auth` but `/Auth/login` does.
pub fn route_is_auth_sensitive(method: &str, path: &str) -> bool {
    let m = method.trim().to_ascii_uppercase();
    if !m.is_empty() && m != "GET" && m != "HEAD" {
        return true;
    }
    path.split('/').any(|seg| {
        let seg = seg.trim().to_ascii_lowercase();
        matches!(seg.as_str(), "admin" | "auth" | "login" | "token")
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::symlink;
    use tempfile::tempdir;

    #[test]
    fn should_skip_catches_known_dirs() {
        assert!(should_skip("node_modules/foo.js"));
        assert!(should_skip("crates/foo/target/debug/x"));
        assert!(should_skip(".git/HEAD"));
        assert!(should_skip("vendor/lib/file.php"));
        assert!(!should_skip("src/main.rs"));
        assert!(!should_skip("crates/foo/src/main.rs"));
    }

    #[test]
    fn should_skip_hard_excludes_worktrees() {
        // Belt-and-suspenders: even when `.gitignore` doesn't list
        // `.worktrees/`, the mapper must not surface sibling worktrees
        // as their own feature slices.
        assert!(should_skip(".worktrees/feature-x/src/main.rs"));
        assert!(should_skip("worktrees/feature-x/src/main.rs"));
        assert!(should_skip("some/nested/.worktrees/feature/file.rs"));
    }

    #[test]
    fn walk_honors_gitignore_entries() {
        // gitignore'd directories must not show up in mapper output —
        // WalkBuilder reads .gitignore at the walk root.
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join(".gitignore"), b".worktrees/\nignored_lib/\n").unwrap();
        fs::write(root.join("src.rs"), b"fn main() {}").unwrap();
        fs::create_dir_all(root.join(".worktrees/feature/src")).unwrap();
        fs::write(root.join(".worktrees/feature/src/leak.rs"), b"// leak").unwrap();
        fs::create_dir_all(root.join("ignored_lib")).unwrap();
        fs::write(root.join("ignored_lib/leak.rs"), b"// leak").unwrap();
        let walked = walk_files(root, root, 100, None);
        assert!(walked.iter().any(|p| p == "src.rs"));
        assert!(
            !walked.iter().any(|p| p.starts_with(".worktrees/")),
            ".worktrees/ leaked into walk despite gitignore: {:?}",
            walked
        );
        assert!(
            !walked.iter().any(|p| p.starts_with("ignored_lib/")),
            "ignored_lib/ leaked into walk despite gitignore: {:?}",
            walked
        );
    }

    #[test]
    fn walk_skips_symlinks_and_escape_paths() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        let outside = tempdir().unwrap();
        fs::write(root.join("real.rs"), b"fn main() {}").unwrap();
        // symlink pointing outside the root should not be walked.
        let _ = symlink(outside.path(), root.join("escape"));
        let walked = walk_files(root, root, 100, None);
        assert!(walked.iter().any(|p| p == "real.rs"));
        assert!(
            walked.iter().all(|p| !p.starts_with("escape")),
            "got {:?}",
            walked
        );
    }

    #[test]
    fn walk_filters_against_exclude_globset() {
        // Project-level `[index].exclude_patterns` are honored in addition
        // to gitignore and the hardcoded should_skip list.
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("keep.rs"), b"// keep").unwrap();
        fs::create_dir_all(root.join("migrations")).unwrap();
        fs::write(root.join("migrations/0001.rs"), b"// drop").unwrap();
        let mut builder = globset::GlobSetBuilder::new();
        builder.add(globset::Glob::new("**/migrations/**").unwrap());
        let excludes = builder.build().unwrap();
        let walked = walk_files(root, root, 100, Some(&excludes));
        assert!(walked.iter().any(|p| p == "keep.rs"));
        assert!(
            !walked.iter().any(|p| p.starts_with("migrations/")),
            "exclude pattern not applied: {:?}",
            walked
        );
    }

    #[test]
    fn rel_path_is_posix() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        let sub = root.join("a").join("b.txt");
        assert_eq!(rel_path(root, &sub), "a/b.txt");
    }

    #[test]
    fn strip_line_comment_preserves_strings() {
        let src = "name = \"foo # not a comment\" # real comment\n";
        let out = strip_line_comments(src, '#');
        assert_eq!(out, "name = \"foo # not a comment\" \n");
    }

    #[test]
    fn auth_sensitive_by_non_get_method() {
        assert!(route_is_auth_sensitive("POST", "/users"));
        assert!(route_is_auth_sensitive("delete", "/users/1"));
        assert!(!route_is_auth_sensitive("GET", "/users"));
        assert!(!route_is_auth_sensitive("HEAD", "/users"));
    }

    #[test]
    fn auth_sensitive_by_path_segment() {
        assert!(route_is_auth_sensitive("GET", "/admin"));
        assert!(route_is_auth_sensitive("GET", "/api/auth/refresh"));
        assert!(route_is_auth_sensitive("GET", "/Auth/login"));
        assert!(route_is_auth_sensitive("", "/token"));
        // Substring-only matches must NOT trigger (segment-exact).
        assert!(!route_is_auth_sensitive("GET", "/authors"));
        assert!(!route_is_auth_sensitive("GET", "/tokenizer"));
    }

    const C_STRIP: CommentSyntax = CommentSyntax {
        hash_line_comments: false,
        strings: StringMode::Blank,
        template_literals: false,
    };
    const PHP_STRIP: CommentSyntax = CommentSyntax {
        hash_line_comments: true,
        strings: StringMode::Preserve,
        template_literals: false,
    };
    const JS_STRIP: CommentSyntax = CommentSyntax {
        hash_line_comments: false,
        strings: StringMode::Preserve,
        template_literals: true,
    };
    const JS_STRIP_STRINGS: CommentSyntax = CommentSyntax {
        hash_line_comments: false,
        strings: StringMode::BlankContents,
        template_literals: true,
    };

    #[test]
    fn strip_comments_blank_mode_blanks_strings_and_comments() {
        // C-source shape: comments AND string literals (delimiters
        // included) blank out; code and offsets survive.
        let src = "int x; // trailing\nchar *s = \"PHP_FUNCTION(fake)\";\n/* block\n */ int y;\n";
        let out = strip_comments(src, C_STRIP);
        assert_eq!(out.len(), src.len(), "offsets must be preserved");
        assert!(out.contains("int x;"));
        assert!(out.contains("int y;"));
        assert!(!out.contains("trailing"));
        assert!(!out.contains("block"));
        assert!(!out.contains("PHP_FUNCTION"));
        assert!(!out.contains('"'), "Blank mode blanks delimiters too");
        // `#` is NOT a comment in this mode.
        assert!(strip_comments("#define X 1\n", C_STRIP).contains("#define X 1"));
    }

    #[test]
    fn strip_comments_preserve_mode_keeps_strings() {
        let src = "$r = '/path'; // note\n# hash comment\n$s = \"a // not comment\";\n";
        let out = strip_comments(src, PHP_STRIP);
        assert_eq!(out.len(), src.len());
        assert!(out.contains("'/path'"), "string kept verbatim");
        assert!(out.contains("\"a // not comment\""));
        assert!(!out.contains("note"));
        assert!(!out.contains("hash comment"));
        // `#[` is PHP attribute syntax, not a comment.
        let attr = strip_comments("#[Attribute]\n", PHP_STRIP);
        assert!(attr.contains("#[Attribute]"));
        // Escaped quote doesn't terminate the string early.
        let esc = strip_comments("$q = 'it\\'s'; // c\n", PHP_STRIP);
        assert!(esc.contains("'it\\'s'"));
        assert!(!esc.contains("// c"));
    }

    #[test]
    fn strip_comments_template_literals_always_blank() {
        let src = "const t = `app.get(\"/x\")`; app.get('/y', h); // c\n";
        let out = strip_comments(src, JS_STRIP);
        assert_eq!(out.len(), src.len());
        assert!(out.contains("'/y'"), "regular string preserved");
        assert!(!out.contains("/x"), "template contents blanked: {out:?}");
        assert!(out.contains('`'), "template delimiters preserved");
        assert!(!out.contains("// c"));
    }

    #[test]
    fn strip_comments_blank_contents_keeps_delimiters() {
        let src = "const app = express(); const s = \"const app = express()\";\n";
        let out = strip_comments(src, JS_STRIP_STRINGS);
        assert_eq!(out.len(), src.len());
        assert_eq!(
            out.matches("const app = express()").count(),
            1,
            "string body must be blanked so the ctor scan can't false-match"
        );
        assert_eq!(out.matches('"').count(), 2, "delimiters preserved");
    }

    #[test]
    fn strip_comments_preserves_newlines_in_all_regions() {
        let src = "a\n/* x\ny */\n\"s1\ns2\"\n`t1\nt2`\n// c\nb\n";
        for syntax in [C_STRIP, PHP_STRIP, JS_STRIP, JS_STRIP_STRINGS] {
            let out = strip_comments(src, syntax);
            assert_eq!(
                out.matches('\n').count(),
                src.matches('\n').count(),
                "newline count must survive for line-offset consumers"
            );
        }
    }

    #[test]
    fn auth_sensitive_empty_method_path_only() {
        // Django-style: no verb at the URL layer, GET-equivalent default.
        assert!(!route_is_auth_sensitive("", "/articles"));
        assert!(route_is_auth_sensitive("", "/admin/users"));
    }
}
