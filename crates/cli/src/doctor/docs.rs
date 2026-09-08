//! Check markdown paths, anchors, symbols, and constants against the index and
//! working tree. Undecidable claims are neither counted nor reported.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Component, Path, PathBuf};
use std::sync::LazyLock;

use anyhow::{Context, Result};
use codesage_protocol::{FileCategory, Language, Symbol, SymbolKind};
use codesage_storage::Database;
use regex::Regex;

use crate::{find_project_root, load_project_config, open_db_read_only};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum ClaimClass {
    Path,
    Anchor,
    Symbol,
    Constant,
}

impl ClaimClass {
    fn label(self) -> &'static str {
        match self {
            ClaimClass::Path => "path",
            ClaimClass::Anchor => "anchor",
            ClaimClass::Symbol => "symbol",
            ClaimClass::Constant => "constant",
        }
    }
}

/// Token shape determines which symbol verdicts are possible.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SymbolShape {
    /// `a::b::c`
    Path,
    /// `Type.member` / `Type->member`
    Member,
    /// bare `foo()`
    Call,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ClaimDetail {
    Path {
        path: String,
    },
    /// A markdown link target resolved relative to the containing document.
    Link {
        target: String,
    },
    Anchor {
        path: String,
        line: u32,
        line_end: Option<u32>,
        symbol: Option<String>,
    },
    Symbol {
        owner: Option<String>,
        member: String,
        shape: SymbolShape,
        /// Trailing `()` distinguishes methods from unindexed fields.
        called: bool,
    },
    Constant {
        name: String,
        literal: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Claim {
    pub line: usize,
    pub class: ClaimClass,
    pub text: String,
    pub detail: ClaimDetail,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub(crate) struct Finding {
    pub doc_path: String,
    pub doc_line: usize,
    pub class: ClaimClass,
    pub claim: String,
    pub reason: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub suggestion: Option<String>,
    /// Repo-relative suggestion targets; empty without a concrete location.
    pub candidates: Vec<String>,
}

/// Keep paths separate so JSON consumers need not parse the suggestion prose.
struct Hint {
    text: String,
    candidates: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum SkipReason {
    /// The file carries a `<!-- codesage-docs: skip-file -->` line.
    Directive,
    /// The file matches `[docs] exclude_patterns`.
    Excluded,
    /// An explicit PATH that is not a regular markdown file.
    NotMarkdown,
    /// An explicit CHANGELOG.md; changelogs are never checked.
    Changelog,
}

impl SkipReason {
    fn label(self) -> &'static str {
        match self {
            SkipReason::Directive => "directive",
            SkipReason::Excluded => "excluded",
            SkipReason::NotMarkdown => "not-markdown",
            SkipReason::Changelog => "changelog",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub(crate) struct SkippedFile {
    pub path: String,
    pub reason: SkipReason,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub(crate) struct FailedFile {
    pub path: String,
    pub error: String,
}

#[derive(Debug, serde::Serialize)]
pub(crate) struct Report {
    pub files: Vec<String>,
    pub files_skipped: Vec<SkippedFile>,
    pub files_failed: Vec<FailedFile>,
    pub claims_checked: usize,
    pub drifted: Vec<Finding>,
}

fn plural(n: usize, noun: &str) -> String {
    if n == 1 {
        format!("{n} {noun}")
    } else {
        format!("{n} {noun}s")
    }
}

pub(crate) fn run(json: bool, strict: bool, paths: &[PathBuf]) -> Result<()> {
    let root = find_project_root()?;
    let db = open_db_read_only(&root)
        .context("doctor --docs needs an index; run `codesage index` first")?;
    let exclude_patterns = load_project_config(&root)?
        .docs
        .and_then(|d| d.exclude_patterns)
        .unwrap_or_default();

    // Explicit paths bypass config exclusions.
    let (selection, explicit) = if paths.is_empty() {
        (default_docs(&root), false)
    } else {
        (explicit_docs(paths), true)
    };
    let excludes: &[String] = if explicit { &[] } else { &exclude_patterns };

    let mut report = check_documents(&root, &db, &selection.docs, excludes)?;
    for p in selection.not_markdown {
        report.files_skipped.push(SkippedFile {
            path: display_doc_path(&root, &p),
            reason: SkipReason::NotMarkdown,
        });
    }
    for p in selection.changelog {
        report.files_skipped.push(SkippedFile {
            path: display_doc_path(&root, &p),
            reason: SkipReason::Changelog,
        });
    }
    for p in selection.missing {
        report.files_failed.push(FailedFile {
            path: display_doc_path(&root, &p),
            error: "No such file or directory".to_string(),
        });
    }
    for (path, error) in selection.failed {
        report.files_failed.push(FailedFile {
            path: display_doc_path(&root, &path),
            error,
        });
    }

    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        for s in &report.files_skipped {
            println!("skipped: {} ({})", s.path, s.reason.label());
        }
        for f in &report.files_failed {
            println!("failed: {}: {}", f.path, f.error);
        }
        for f in &report.drifted {
            let suggestion = f
                .suggestion
                .as_deref()
                .map(|s| format!(" ({s})"))
                .unwrap_or_default();
            println!(
                "{}:{}  {}  {}  → {}{suggestion}",
                f.doc_path,
                f.doc_line,
                f.class.label(),
                f.claim,
                f.reason
            );
        }
        let mut summary = format!(
            "{} claims checked, {} drifted",
            report.claims_checked,
            report.drifted.len()
        );
        if report.files.is_empty() {
            summary.push_str(", no documents checked");
        }
        if !report.files_skipped.is_empty() {
            summary.push_str(&format!(
                ", {} skipped",
                plural(report.files_skipped.len(), "file")
            ));
        }
        if !report.files_failed.is_empty() {
            summary.push_str(&format!(
                ", {} failed",
                plural(report.files_failed.len(), "file")
            ));
        }
        println!("{summary}");
    }

    if strict
        && (report.files.is_empty()
            || !report.drifted.is_empty()
            || !report.files_failed.is_empty())
    {
        std::process::exit(1);
    }
    Ok(())
}

/// AGENTS.md or CLAUDE.md (the real file when one symlinks to the other),
/// README.md, and every `docs/**/*.md`; CHANGELOG.md is never a candidate.
fn default_docs(root: &Path) -> ExplicitDocs {
    let mut set = DocSet::default();
    for name in ["AGENTS.md", "CLAUDE.md", "README.md"] {
        set.push(root.join(name));
    }
    let docs = root.join("docs");
    if docs.exists() {
        set.push_dir(&docs);
    }
    ExplicitDocs {
        docs: set.out,
        failed: set.failed,
        ..ExplicitDocs::default()
    }
}

fn explicit_docs(paths: &[PathBuf]) -> ExplicitDocs {
    let mut set = DocSet::default();
    let mut out = ExplicitDocs::default();
    for p in paths {
        if !p.exists() {
            // A typo in an explicit argument must not read as a clean sweep.
            out.missing.push(p.clone());
        } else if p.is_dir() {
            set.push_dir(p);
        } else if is_changelog(p) {
            out.changelog.push(p.clone());
        } else if is_markdown(p) && p.is_file() {
            set.push(p.clone());
        } else {
            out.not_markdown.push(p.clone());
        }
    }
    out.docs = set.out;
    out.failed = set.failed;
    out
}

#[derive(Default)]
struct ExplicitDocs {
    docs: Vec<PathBuf>,
    not_markdown: Vec<PathBuf>,
    changelog: Vec<PathBuf>,
    missing: Vec<PathBuf>,
    failed: Vec<(PathBuf, String)>,
}

#[derive(Default)]
struct DocSet {
    seen: HashSet<PathBuf>,
    out: Vec<PathBuf>,
    failed: Vec<(PathBuf, String)>,
}

impl DocSet {
    /// Dedupe canonically, but preserve spelling so symlinked-in docs remain internal.
    fn push(&mut self, p: PathBuf) {
        if !p.is_file() || is_changelog(&p) {
            return;
        }
        let key = std::fs::canonicalize(&p).unwrap_or_else(|_| p.clone());
        if self.seen.insert(key) {
            let spelled = normalize_lexically(&std::path::absolute(&p).unwrap_or(p));
            self.out.push(spelled);
        }
    }

    fn push_dir(&mut self, dir: &Path) {
        let mut found = Vec::new();
        walk_markdown(dir, &mut found, &mut HashSet::new(), &mut self.failed);
        found.sort();
        for f in found {
            self.push(f);
        }
    }
}

fn is_markdown(p: &Path) -> bool {
    p.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("md") || e.eq_ignore_ascii_case("markdown"))
}

fn is_changelog(p: &Path) -> bool {
    p.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.eq_ignore_ascii_case("CHANGELOG.md"))
}

/// Canonical directory keys stop symlink cycles.
fn walk_markdown(
    dir: &Path,
    out: &mut Vec<PathBuf>,
    visited: &mut HashSet<PathBuf>,
    failed: &mut Vec<(PathBuf, String)>,
) {
    let key = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
    if !visited.insert(key) {
        return;
    }
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) => {
            failed.push((dir.to_path_buf(), error.to_string()));
            return;
        }
    };
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                failed.push((dir.to_path_buf(), error.to_string()));
                continue;
            }
        };
        let path = entry.path();
        if path.is_dir() {
            walk_markdown(&path, out, visited, failed);
        } else if is_markdown(&path) {
            out.push(path);
        }
    }
}

fn display_doc_path(root: &Path, doc: &Path) -> String {
    let abs_root = normalize_lexically(&std::path::absolute(root).unwrap_or_else(|_| root.into()));
    let abs_doc = normalize_lexically(&std::path::absolute(doc).unwrap_or_else(|_| doc.into()));
    if let Ok(rel) = abs_doc.strip_prefix(&abs_root) {
        return rel.display().to_string();
    }
    let canon_root = std::fs::canonicalize(root).unwrap_or(abs_root);
    let canon_doc = std::fs::canonicalize(doc).unwrap_or_else(|_| abs_doc.clone());
    canon_doc
        .strip_prefix(&canon_root)
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| abs_doc.display().to_string())
}

/// A directive example inside prose or code must not skip its own document.
static SKIP_DIRECTIVE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^<!--\s*codesage-docs:\s*skip-file\s*-->$").expect("static regex")
});

/// Cache index facts across documents to avoid one query per token.
struct IndexView {
    files: HashSet<String>,
    /// Indexed directory prefixes avoid disk probes for known parents.
    dirs: HashSet<String>,
    by_basename: HashMap<String, Vec<String>>,
    /// The owner's language determines member naming conventions.
    languages: HashMap<String, Language>,
    /// Gitignored roots and `.codesage` are machine-local artifacts, not drift.
    ignored_roots: HashSet<String>,
    /// Crate names declared in the root and `crates/*/Cargo.toml` dependency
    /// tables, `-` normalised to `_`. A `dep::item` path is foreign code.
    dep_crates: HashSet<String>,
}

impl IndexView {
    fn load(root: &Path, db: &Database) -> Result<Self> {
        let rows = db.all_files_with_id_and_language()?;
        let mut files = HashSet::with_capacity(rows.len());
        let mut dirs = HashSet::new();
        let mut by_basename: HashMap<String, Vec<String>> = HashMap::new();
        let mut languages = HashMap::with_capacity(rows.len());
        for (_, p, language) in rows {
            let mut end = 0;
            while let Some(slash) = p[end..].find('/') {
                end += slash;
                dirs.insert(p[..end].to_string());
                end += 1;
            }
            if let Some(base) = p.rsplit('/').next() {
                by_basename
                    .entry(base.to_string())
                    .or_default()
                    .push(p.clone());
            }
            languages.insert(p.clone(), language);
            files.insert(p);
        }
        Ok(Self {
            files,
            dirs,
            by_basename,
            languages,
            ignored_roots: gitignored_roots(root),
            dep_crates: dependency_crates(root),
        })
    }
}

/// Plain top-level entries of the repo `.gitignore` (`/target`, `.plan/`),
/// without globs or nested paths, plus `.codesage` unconditionally.
fn gitignored_roots(root: &Path) -> HashSet<String> {
    let mut out: HashSet<String> = HashSet::from([".codesage".to_string()]);
    let Ok(text) = std::fs::read_to_string(root.join(".gitignore")) else {
        return out;
    };
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with('!') {
            continue;
        }
        let name = line.trim_start_matches('/').trim_end_matches('/');
        if name.is_empty() || name.contains(['*', '?', '[', '/', '\\']) {
            continue;
        }
        out.insert(name.to_string());
    }
    out
}

fn dependency_crates(root: &Path) -> HashSet<String> {
    let mut manifests = vec![root.join("Cargo.toml")];
    if let Ok(entries) = std::fs::read_dir(root.join("crates")) {
        for entry in entries.flatten() {
            manifests.push(entry.path().join("Cargo.toml"));
        }
    }
    let mut out = HashSet::new();
    let mut members = HashSet::new();
    for manifest in manifests {
        let Ok(text) = std::fs::read_to_string(&manifest) else {
            continue;
        };
        let Ok(table) = text.parse::<toml::Table>() else {
            continue;
        };
        if let Some(name) = table
            .get("package")
            .and_then(|p| p.get("name"))
            .and_then(|n| n.as_str())
        {
            members.insert(name.replace('-', "_"));
        }
        collect_dependency_keys(&table, &mut out);
    }
    // Workspace members remain checkable even when listed as dependencies.
    for member in members {
        out.remove(&member);
    }
    out
}

fn collect_dependency_keys(table: &toml::Table, out: &mut HashSet<String>) {
    for (key, value) in table {
        let toml::Value::Table(inner) = value else {
            continue;
        };
        if matches!(
            key.as_str(),
            "dependencies" | "dev-dependencies" | "build-dependencies"
        ) {
            for (dep, spec) in inner {
                out.insert(dep.replace('-', "_"));
                // Docs may use the package name instead of its code alias.
                if let Some(real) = spec.get("package").and_then(|p| p.as_str()) {
                    out.insert(real.replace('-', "_"));
                }
            }
        } else {
            collect_dependency_keys(inner, out);
        }
    }
}

pub(crate) fn check_documents(
    root: &Path,
    db: &Database,
    docs: &[PathBuf],
    exclude_patterns: &[String],
) -> Result<Report> {
    let excludes = if exclude_patterns.is_empty() {
        None
    } else {
        Some(
            codesage_parser::discover::build_exclude_set(exclude_patterns)
                .context("invalid [docs] exclude_patterns")?,
        )
    };
    let index = IndexView::load(root, db)?;
    let mut checker = Checker {
        root,
        db,
        index: &index,
        symbols_by_file: HashMap::new(),
        line_counts: HashMap::new(),
        rust_extensions: None,
        doc_external: false,
    };
    let canon_root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let mut files = Vec::new();
    let mut files_skipped = Vec::new();
    let mut files_failed = Vec::new();
    let mut claims_checked = 0;
    let mut drifted = Vec::new();
    for doc in docs {
        let display = display_doc_path(root, doc);
        if excludes.as_ref().is_some_and(|set| set.is_match(&display)) {
            files_skipped.push(SkippedFile {
                path: display,
                reason: SkipReason::Excluded,
            });
            continue;
        }
        let text = match std::fs::read_to_string(doc) {
            Ok(text) => text,
            Err(e) => {
                files_failed.push(FailedFile {
                    path: display,
                    error: e.to_string(),
                });
                continue;
            }
        };
        let extraction = extract(&text);
        if extraction.skip_directive {
            files_skipped.push(SkippedFile {
                path: display,
                reason: SkipReason::Directive,
            });
            continue;
        }
        // A docs directory symlinked outside the root still belongs to this repo.
        let spelled =
            normalize_lexically(&std::path::absolute(doc).unwrap_or_else(|_| doc.clone()));
        let canonical = std::fs::canonicalize(doc).unwrap_or_else(|_| doc.clone());
        checker.doc_external = !(canonical.starts_with(&canon_root)
            || spelled.starts_with(&canon_root)
            || spelled.starts_with(root));
        let (checked, mut findings) = checker.check_document(&display, doc, extraction.claims);
        claims_checked += checked;
        drifted.append(&mut findings);
        files.push(display);
    }
    Ok(Report {
        files,
        files_skipped,
        files_failed,
        claims_checked,
        drifted,
    })
}

struct Checker<'a> {
    root: &'a Path,
    db: &'a Database,
    index: &'a IndexView,
    symbols_by_file: HashMap<String, Vec<Symbol>>,
    line_counts: HashMap<String, Option<usize>>,
    rust_extensions: Option<HashSet<String>>,
    doc_external: bool,
}

/// Common basenames cannot establish a relocation.
const GENERIC_BASENAMES: &[&str] = &[
    "README.md",
    "index.md",
    "mod.rs",
    "lib.rs",
    "main.rs",
    "main.go",
    "__init__.py",
    "index.js",
    "index.ts",
    "Makefile",
    "Cargo.toml",
    "package.json",
];

/// Directory segments too common to tie two paths together.
const GENERIC_SEGMENTS: &[&str] = &[
    "src", "crates", "lib", "tests", "test", "docs", "app", "pkg", "internal", "cmd", "",
];

/// Short paths under common roots cannot tie an external document to this repo.
const UBIQUITOUS_ROOTS: &[&str] = &[
    "docs", "scripts", "src", "lib", "tests", "test", ".github", "app", "config",
];

enum Verdict {
    Holds,
    Drift {
        reason: String,
        suggestion: Option<Hint>,
    },
    /// Neither counted nor reported.
    Undecidable,
}

impl Checker<'_> {
    fn check_document(
        &mut self,
        doc_path: &str,
        doc_abs: &Path,
        claims: Vec<Claim>,
    ) -> (usize, Vec<Finding>) {
        let mut checked = 0;
        let mut findings = Vec::new();
        // Relative links belong to the symlink target's directory.
        let real_doc = std::fs::canonicalize(doc_abs).unwrap_or_else(|_| doc_abs.to_path_buf());
        let doc_dir = real_doc.parent().unwrap_or(Path::new(""));
        for claim in claims {
            let verdict = match &claim.detail {
                ClaimDetail::Path { path } => self.check_path(path),
                ClaimDetail::Link { target } => self.check_link(doc_dir, target),
                ClaimDetail::Anchor {
                    path,
                    line,
                    line_end,
                    symbol,
                } => self.check_anchor(path, *line, *line_end, symbol.as_deref()),
                ClaimDetail::Symbol {
                    owner,
                    member,
                    shape,
                    called,
                } => self.check_symbol(owner.as_deref(), member, *shape, *called),
                ClaimDetail::Constant { name, literal } => self.check_constant(name, literal),
            };
            match verdict {
                Verdict::Undecidable => {}
                Verdict::Holds => checked += 1,
                Verdict::Drift { reason, suggestion } => {
                    checked += 1;
                    let (suggestion, candidates) = match suggestion {
                        Some(hint) => (Some(hint.text), hint.candidates),
                        None => (None, Vec::new()),
                    };
                    findings.push(Finding {
                        doc_path: doc_path.to_string(),
                        doc_line: claim.line,
                        class: claim.class,
                        claim: claim.text.clone(),
                        reason,
                        suggestion,
                        candidates,
                    });
                }
            }
        }
        (checked, findings)
    }

    fn file_known(&self, path: &str) -> bool {
        self.root.join(path).is_file() || self.index.files.contains(path)
    }

    /// Require an existing parent to avoid treating foreign examples as drift.
    fn plausibly_repo_relative(&self, path: &str) -> bool {
        self.plausible_path(path, true)
    }

    fn plausible_path(&self, path: &str, require_parent: bool) -> bool {
        let Some((first, _)) = path.split_once('/') else {
            return true;
        };
        if self.index.ignored_roots.contains(first) {
            return false;
        }
        if self.doc_external && path.matches('/').count() == 1 && UBIQUITOUS_ROOTS.contains(&first)
        {
            return false;
        }
        if !require_parent {
            return true;
        }
        let Some((parent, _)) = path.rsplit_once('/') else {
            return true;
        };
        self.root.join(parent).is_dir() || self.index.dirs.contains(parent)
    }

    /// A unique basename supplies a hint; shared distinctive directories strengthen it.
    fn suggest_by_basename(&self, path: &str) -> Option<Hint> {
        let (dir, base) = path.rsplit_once('/')?;
        // Fixture inputs are not relocation candidates; real test files are.
        let candidates: Vec<&String> = self
            .index
            .by_basename
            .get(base)?
            .iter()
            .filter(|c| !is_fixture_path(c))
            .collect();
        if GENERIC_BASENAMES.contains(&base) || candidates.len() != 1 {
            return None;
        }
        let candidate = candidates[0].clone();
        let claimed: HashSet<&str> = dir
            .split('/')
            .filter(|seg| !GENERIC_SEGMENTS.contains(seg))
            .collect();
        let related = candidate
            .rsplit_once('/')
            .is_some_and(|(cdir, _)| cdir.split('/').any(|seg| claimed.contains(seg)));
        let text = if related {
            format!("did you mean {candidate}")
        } else {
            format!("a file of the same name exists elsewhere: {candidate}")
        };
        Some(Hint {
            text,
            candidates: vec![candidate],
        })
    }

    fn check_path(&self, path: &str) -> Verdict {
        if self.file_known(path) {
            return Verdict::Holds;
        }
        if !self.plausibly_repo_relative(path) {
            return Verdict::Undecidable;
        }
        self.missing_path_verdict(path)
    }

    /// External documents require a concrete relocation to establish drift;
    /// an existing parent alone may belong to another codebase too.
    fn missing_path_verdict(&self, path: &str) -> Verdict {
        // A module-to-directory relocation must preserve the source extension.
        let (stem, ext) = path.rsplit_once('.').unwrap_or((path, ""));
        let dir_prefix = format!("{stem}/");
        let dir_has_same_kind = self.index.files.iter().any(|f| {
            f.strip_prefix(&dir_prefix).is_some_and(|rest| {
                !rest.contains('/') && rest.rsplit_once('.').map(|(_, e)| e) == Some(ext)
            })
        });
        if dir_has_same_kind {
            return Verdict::Drift {
                reason: "file not found; the module is now a directory".to_string(),
                suggestion: Some(Hint {
                    text: dir_prefix.clone(),
                    candidates: vec![dir_prefix],
                }),
            };
        }
        let suggestion = self.suggest_by_basename(path);
        if !self.doc_external {
            return Verdict::Drift {
                reason: "file not found on disk or in the index".to_string(),
                suggestion,
            };
        }
        match suggestion {
            Some(hint) => Verdict::Drift {
                reason: "file not found on disk or in the index".to_string(),
                suggestion: Some(hint),
            },
            None => Verdict::Undecidable,
        }
    }

    fn check_link(&self, doc_dir: &Path, target: &str) -> Verdict {
        let target = percent_decode(target.split('?').next().unwrap_or(target));
        let target = target.as_str();
        let resolved = normalize_lexically(&doc_dir.join(target));
        if resolved.is_file() || resolved.is_dir() {
            return Verdict::Holds;
        }
        // Unresolved extensionless links may use static-site routing, so remain undecidable.
        let basename = target
            .trim_end_matches('/')
            .rsplit('/')
            .next()
            .unwrap_or(target);
        let explicit_extension = !target.ends_with('/') && basename.contains('.');
        if !explicit_extension {
            return if ssg_target_exists(&resolved) {
                Verdict::Holds
            } else {
                Verdict::Undecidable
            };
        }
        let canon_root = std::fs::canonicalize(self.root).unwrap_or_else(|_| self.root.into());
        let canon_dir = std::fs::canonicalize(doc_dir).unwrap_or_else(|_| doc_dir.into());
        let rel = normalize_lexically(&canon_dir.join(target));
        match rel.strip_prefix(&canon_root) {
            Ok(rel) => {
                let rel = rel.to_string_lossy().replace('\\', "/");
                if self.index.files.contains(rel.as_str()) {
                    return Verdict::Holds;
                }
                // Requiring an existing parent would hide a wrong `../` count.
                if !self.plausible_path(&rel, false) {
                    return Verdict::Undecidable;
                }
                Verdict::Drift {
                    reason: "link target not found".to_string(),
                    suggestion: self.suggest_by_basename(&rel),
                }
            }
            Err(_) => Verdict::Drift {
                reason: "link target not found".to_string(),
                suggestion: None,
            },
        }
    }

    fn line_count(&mut self, path: &str) -> Option<usize> {
        if let Some(n) = self.line_counts.get(path) {
            return *n;
        }
        let n = std::fs::read(self.root.join(path))
            .ok()
            .map(|bytes| String::from_utf8_lossy(&bytes).lines().count());
        self.line_counts.insert(path.to_string(), n);
        n
    }

    fn symbols_in(&mut self, path: &str) -> &[Symbol] {
        if !self.symbols_by_file.contains_key(path) {
            let syms = self.db.symbols_for_file(path).unwrap_or_default();
            self.symbols_by_file.insert(path.to_string(), syms);
        }
        self.symbols_by_file
            .get(path)
            .map(Vec::as_slice)
            .unwrap_or_default()
    }

    fn check_anchor(
        &mut self,
        path: &str,
        line: u32,
        line_end: Option<u32>,
        symbol: Option<&str>,
    ) -> Verdict {
        if !self.file_known(path) {
            if !path.contains('/') || !self.plausibly_repo_relative(path) {
                return Verdict::Undecidable;
            }
            return self.missing_path_verdict(path);
        }
        let Some(total) = self.line_count(path) else {
            return Verdict::Drift {
                reason: "file is indexed but missing from the working tree".to_string(),
                suggestion: None,
            };
        };
        let last = line_end.unwrap_or(line).max(line);
        if last as usize > total {
            return Verdict::Drift {
                reason: format!("line out of range (file has {total} lines)"),
                suggestion: None,
            };
        }
        let Some(symbol) = symbol else {
            return Verdict::Holds;
        };
        let bare = symbol.trim_end_matches("()");
        let tail = bare.rsplit("::").next().unwrap_or(bare);
        let contains = |(start, end): &(u32, u32)| *start <= line && line <= *end;
        let symbols = self.symbols_in(path);
        let matching: Vec<(u32, u32)> = symbols
            .iter()
            .filter(|s| s.name == tail || s.qualified_name == bare)
            .map(|s| (s.line_start, s.line_end))
            .collect();
        if matching.is_empty() || matching.iter().any(contains) {
            return Verdict::Holds;
        }
        // Outside indexed definitions, a nearby name may describe a call site.
        if !symbols
            .iter()
            .any(|s| contains(&(s.line_start, s.line_end)))
        {
            return Verdict::Undecidable;
        }
        let now = matching.iter().map(|(start, _)| *start).min().unwrap_or(0);
        Verdict::Drift {
            reason: format!("symbol `{symbol}` not at that line (now at L{now})"),
            suggestion: Some(Hint {
                text: format!("{path}:{now}"),
                candidates: vec![path.to_string()],
            }),
        }
    }

    /// Exclude test and fixture definitions from evidence about documented APIs.
    fn find(&self, name: &str) -> Vec<Symbol> {
        let mut found = self.db.find_symbols(name, None).unwrap_or_default();
        found.retain(|s| {
            !is_fixture_path(&s.file_path)
                && FileCategory::classify(&s.file_path) != FileCategory::Test
        });
        found
    }

    /// Report missing members only for an exact indexed owner and a shape that
    /// cannot denote an unindexed field or variant. Unknown owners remain undecidable.
    fn check_symbol(
        &mut self,
        owner: Option<&str>,
        member: &str,
        shape: SymbolShape,
        called: bool,
    ) -> Verdict {
        let Some(owner) = owner else {
            return if self.find(member).is_empty() {
                Verdict::Undecidable
            } else {
                Verdict::Holds
            };
        };
        // Filenames such as Cargo.toml also have the Type.member shape.
        if shape == SymbolShape::Member {
            let as_file = format!("{owner}.{member}");
            if self.root.join(&as_file).is_file() || self.index.by_basename.contains_key(&as_file) {
                return Verdict::Undecidable;
            }
        }
        if shape == SymbolShape::Path {
            let crate_root = owner.split("::").next().unwrap_or(owner);
            if FOREIGN_ROOTS.contains(&crate_root) || self.index.dep_crates.contains(crate_root) {
                return Verdict::Undecidable;
            }
        }
        if !self.find(&format!("{owner}::{member}")).is_empty()
            || (shape == SymbolShape::Member && !self.find(&format!("{owner}.{member}")).is_empty())
        {
            return Verdict::Holds;
        }
        let owners = self.find(owner);
        if owners.is_empty() {
            // An owner-tail match may confirm a claim, never establish drift.
            if owner.contains("::") {
                let owner_tail = owner.rsplit("::").next().unwrap_or(owner);
                let files: BTreeSet<String> = self
                    .find(owner_tail)
                    .iter()
                    .map(|s| s.file_path.clone())
                    .collect();
                for file in &files {
                    if self.symbols_in(file).iter().any(|s| s.name == member) {
                        return Verdict::Holds;
                    }
                }
            }
            return Verdict::Undecidable;
        }
        let owner_files: BTreeSet<String> = owners.iter().map(|s| s.file_path.clone()).collect();
        for file in &owner_files {
            if self.symbols_in(file).iter().any(|s| s.name == member) {
                return Verdict::Holds;
            }
        }
        // Types require call syntax because fields and variants are unindexed.
        // Constant, function, and macro homonyms cannot establish member ownership.
        let module_like = owners
            .iter()
            .any(|s| matches!(s.kind, SymbolKind::Module | SymbolKind::Namespace));
        let type_like = owners.iter().any(|s| {
            matches!(
                s.kind,
                SymbolKind::Struct
                    | SymbolKind::Class
                    | SymbolKind::Enum
                    | SymbolKind::Trait
                    | SymbolKind::Interface
            )
        });
        if shape == SymbolShape::Member || !(module_like || (type_like && called)) {
            return Verdict::Undecidable;
        }
        if type_like
            && owners
                .iter()
                .any(|symbol| self.may_have_unindexed_methods(symbol))
        {
            return Verdict::Undecidable;
        }
        // camelCase suggests a foreign Rust/Python homonym; Go permits capitals.
        let snake_languages = owner_files.iter().all(|f| {
            matches!(
                self.index.languages.get(f),
                Some(Language::Rust | Language::Python)
            )
        });
        if snake_languages && is_camel_case(member) {
            return Verdict::Undecidable;
        }
        let files: Vec<&str> = owner_files.iter().map(String::as_str).take(2).collect();
        let where_ = if files.is_empty() {
            String::new()
        } else {
            format!(" ({})", files.join(", "))
        };
        // External documents may name unrelated types with the same owner name.
        if self.doc_external {
            return Verdict::Undecidable;
        }
        Verdict::Drift {
            reason: format!("unknown symbol: `{owner}` is indexed{where_} but has no `{member}`"),
            suggestion: None,
        }
    }

    fn check_constant(&self, name: &str, literal: &str) -> Verdict {
        let symbols: Vec<Symbol> = self
            .find(name)
            .into_iter()
            .filter(|s| s.kind == SymbolKind::Constant)
            .collect();
        let doc_value = normalize_literal(literal);
        let mut source_values: BTreeSet<String> = BTreeSet::new();
        for sym in &symbols {
            let Some(value) = self.constant_value(sym) else {
                continue;
            };
            if value == doc_value {
                return Verdict::Holds;
            }
            source_values.insert(value);
        }
        if source_values.is_empty() {
            return Verdict::Undecidable;
        }
        let source = source_values.into_iter().collect::<Vec<_>>().join(" / ");
        Verdict::Drift {
            reason: format!("constant value drift: doc says {doc_value}, source says {source}"),
            suggestion: None,
        }
    }

    fn may_have_unindexed_methods(&mut self, symbol: &Symbol) -> bool {
        // The structural index does not expand derives/macros or materialize
        // inherited methods onto every implementing type.
        let Ok(source) = std::fs::read_to_string(self.root.join(&symbol.file_path)) else {
            return true;
        };
        let start = symbol.line_start.max(1) as usize - 1;
        let definition = source
            .lines()
            .skip(start)
            .take((symbol.line_end.max(symbol.line_start) as usize).saturating_sub(start))
            .collect::<Vec<_>>()
            .join("\n");
        if self.index.languages.get(&symbol.file_path) != Some(&Language::Rust) {
            return INHERITED_TYPE.is_match(&definition);
        }
        if rust_type_has_attributes(&source, symbol) {
            return true;
        }
        let extensions = self.rust_extensions.get_or_insert_with(|| {
            let mut types = HashSet::new();
            for (path, language) in &self.index.languages {
                if *language != Language::Rust {
                    continue;
                }
                let Ok(source) = std::fs::read_to_string(self.root.join(path)) else {
                    types.insert("*".to_string());
                    continue;
                };
                for captures in TRAIT_IMPL.captures_iter(&source) {
                    let target = captures[2].rsplit("::").next().unwrap_or(&captures[2]);
                    types.insert(target.to_string());
                    if captures[1]
                        .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
                        .any(|token| token == target)
                    {
                        types.insert("*".to_string());
                    }
                }
                for captures in MACRO_ARGUMENTS.captures_iter(&source) {
                    types.extend(
                        captures[1]
                            .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
                            .filter(|token| TYPE_NAME.is_match(token))
                            .map(str::to_string),
                    );
                }
            }
            types
        });
        extensions.contains("*") || extensions.contains(&symbol.name)
    }

    fn constant_value(&self, sym: &Symbol) -> Option<String> {
        let bytes = std::fs::read(self.root.join(&sym.file_path)).ok()?;
        let text = String::from_utf8_lossy(&bytes);
        let start = sym.line_start.max(1) as usize - 1;
        let end = (sym.line_end.max(sym.line_start)) as usize;
        let definition: String = text
            .lines()
            .skip(start)
            .take(end - start)
            .collect::<Vec<_>>()
            .join(" ");
        let rhs = definition.split_once('=')?.1;
        let rhs = rhs.split(';').next().unwrap_or(rhs).trim();
        source_literal(rhs).map(|lit| normalize_literal(&lit))
    }
}

fn rust_type_has_attributes(source: &str, symbol: &Symbol) -> bool {
    let Ok(tree) = codesage_parser::parse::parse_file(source.as_bytes(), Language::Rust) else {
        return true;
    };
    let offset = source
        .split_inclusive('\n')
        .take(symbol.line_start.max(1) as usize - 1)
        .map(str::len)
        .sum::<usize>()
        .saturating_add(symbol.col_start as usize);
    let Some(mut node) = tree
        .root_node()
        .descendant_for_byte_range(offset, offset.saturating_add(1))
    else {
        return true;
    };
    while !matches!(node.kind(), "struct_item" | "enum_item" | "trait_item") {
        let Some(parent) = node.parent() else {
            return true;
        };
        node = parent;
    }
    if node
        .child_by_field_name("name")
        .and_then(|name| name.utf8_text(source.as_bytes()).ok())
        != Some(symbol.name.as_str())
    {
        return true;
    }
    let mut sibling = node.prev_named_sibling();
    while let Some(previous) = sibling {
        match previous.kind() {
            "attribute_item" => return true,
            "line_comment" | "block_comment" => sibling = previous.prev_named_sibling(),
            _ => return false,
        }
    }
    false
}

static INHERITED_TYPE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\b(?:extends|implements|use)\s|\b(?:class|struct)\s+\w+(?:\s+final)?\s*[:(]")
        .expect("static regex")
});
static TRAIT_IMPL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\bimpl\b([^;{}]*?)\bfor\s+([A-Za-z_][A-Za-z0-9_:]*)").expect("static regex")
});
static MACRO_ARGUMENTS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\b[A-Za-z_][A-Za-z0-9_]*!\s*[({\[]([^;]*?)[)}\]]").expect("static regex")
});

/// Match recommend_tests: fixture contents are inputs, not API definitions.
const FIXTURE_SEGMENTS: &[&str] = &[
    "fixtures",
    "fixture",
    "stubs",
    "testdata",
    "__fixtures__",
    "__snapshots__",
];

fn is_fixture_path(path: &str) -> bool {
    let lower = path.to_lowercase();
    FIXTURE_SEGMENTS
        .iter()
        .any(|seg| lower.contains(&format!("/{seg}/")) || lower.starts_with(&format!("{seg}/")))
}

fn ssg_target_exists(resolved: &Path) -> bool {
    if resolved.with_extension("md").is_file()
        || resolved.join("index.md").is_file()
        || resolved.join("README.md").is_file()
    {
        return true;
    }
    let (Some(parent), Some(name)) = (resolved.parent(), resolved.file_name()) else {
        return false;
    };
    let want = name.to_string_lossy().to_lowercase();
    let Ok(entries) = std::fs::read_dir(parent) else {
        return false;
    };
    entries.flatten().any(|e| {
        let have = e.file_name().to_string_lossy().to_lowercase();
        have == want || have == format!("{want}.md")
    })
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Some(hex) = s.get(i + 1..i + 3)
            && let Ok(v) = u8::from_str_radix(hex, 16)
        {
            out.push(v);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Preserve resolution of missing targets without filesystem canonicalization.
fn normalize_lexically(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    out.push("..");
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

static SOURCE_LITERAL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"^(?:"([^"]*)"|'([^']*)'|(true|false)\b|(-?(?:0x[0-9A-Fa-f_]+|\d[\d_]*(?:\.\d+)?)))"#,
    )
    .expect("static regex")
});

/// Accept literal RHS values only; expressions are not evaluated.
fn source_literal(rhs: &str) -> Option<String> {
    let caps = SOURCE_LITERAL.captures(rhs)?;
    let whole = caps.get(0)?.as_str();
    let rest = &rhs[whole.len()..];
    // A type suffix (`1_500usize`, `2.0f32`) is fine; an operator means an
    // expression, which this checker refuses to evaluate.
    if !(rest.is_empty()
        || (caps.get(4).is_some()
            && matches!(
                rest,
                "u8" | "u16"
                    | "u32"
                    | "u64"
                    | "u128"
                    | "usize"
                    | "i8"
                    | "i16"
                    | "i32"
                    | "i64"
                    | "i128"
                    | "isize"
                    | "f32"
                    | "f64"
            )))
    {
        return None;
    }
    Some(whole.to_string())
}

/// Compare quoted strings and numeric spellings such as `1.5M` and `1_500_000`.
pub(crate) fn normalize_literal(raw: &str) -> String {
    let s = raw.trim().trim_matches('`').trim();
    if (s.starts_with('"') && s.ends_with('"') && s.len() >= 2)
        || (s.starts_with('\'') && s.ends_with('\'') && s.len() >= 2)
    {
        return s[1..s.len() - 1].to_string();
    }
    if s == "true" || s == "false" {
        return s.to_string();
    }
    let (negative, body) = match s.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, s),
    };
    if let Some(hex) = body.strip_prefix("0x").or_else(|| body.strip_prefix("0X")) {
        let digits: String = hex.chars().filter(|c| *c != '_').collect();
        if let Ok(v) = u128::from_str_radix(&digits, 16) {
            return format!("{}{v}", if negative { "-" } else { "" });
        }
        return s.to_string();
    }
    let multiplier = match body.chars().last() {
        Some('K' | 'k') => 1_000.0,
        Some('M' | 'm') => 1_000_000.0,
        Some('G' | 'g') => 1_000_000_000.0,
        _ => 1.0,
    };
    let digits: String = body
        .trim_end_matches(|c: char| c.is_ascii_alphabetic())
        .chars()
        .filter(|c| *c != '_' && *c != ',')
        .collect();
    let Ok(value) = digits.parse::<f64>() else {
        return s.to_string();
    };
    let value = value * multiplier * if negative { -1.0 } else { 1.0 };
    if value.fract() == 0.0 && value.abs() < 1e18 {
        format!("{}", value as i128)
    } else {
        format!("{value}")
    }
}

static ANCHOR: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"((?:[A-Za-z0-9_.-]+/)*[A-Za-z0-9_.-]+\.[A-Za-z0-9]{1,10}):(\d{1,7})(?:-(\d{1,7}))?(?::\d{1,4})?",
    )
    .expect("static regex")
});
static LINK: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\]\(([^)\s]+)\)").expect("static regex"));
static IDENT_PATH: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^[A-Za-z_][A-Za-z0-9_]*(?:::[A-Za-z_][A-Za-z0-9_]*)*(?:\(\))?$")
        .expect("static regex")
});
static IDENT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[A-Za-z_][A-Za-z0-9_]*$").expect("static regex"));
static TYPE_NAME: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[A-Z][A-Za-z0-9_]*$").expect("static regex"));
static UPPER_SNAKE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[A-Z][A-Z0-9_]+$").expect("static regex"));
static CONSTANT_TAIL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"^([^`]*?)(?:=|\bis|\(default|\bdefaults to)\s*(`?)("[^"]*"|'[^']*'|true|false|-?\d[\d_,]*(?:\.\d+)?[KMGkmg]?)(`?)(?:\W|$)"#,
    )
    .expect("static regex")
});

const ANCHOR_SYMBOL_DISTANCE: usize = 80;

/// Unit-bearing quantities cannot be compared directly with source literals.
const UNIT_WORDS: &[&str] = &[
    "s",
    "sec",
    "secs",
    "second",
    "seconds",
    "ms",
    "millisecond",
    "milliseconds",
    "min",
    "mins",
    "minute",
    "minutes",
    "h",
    "hr",
    "hour",
    "hours",
    "day",
    "days",
    "kb",
    "mb",
    "gb",
    "kib",
    "mib",
    "gib",
    "%",
    "x",
];

/// Content byte ranges exclude backticks. Unpaired delimiters make the line
/// undecidable because they may open a multiline code span.
fn code_spans(line: &str) -> Option<Vec<(usize, usize)>> {
    let mut spans = Vec::new();
    let mut open: Option<(usize, usize)> = None;
    let bytes = line.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'`' {
            i += 1;
            continue;
        }
        let run = bytes[i..].iter().take_while(|&&b| b == b'`').count();
        match open {
            None => open = Some((i + run, run)),
            Some((start, width)) if width == run => {
                if start < i {
                    spans.push((start, i));
                }
                open = None;
            }
            _ => {}
        }
        i += run;
    }
    open.is_none().then_some(spans)
}

/// A decimal point inside a number must not end the sentence.
fn sentence_end(line: &str, from: usize) -> usize {
    let bytes = line.as_bytes();
    let mut i = from;
    while i < bytes.len() {
        if bytes[i] == b'.' && (i + 1 == bytes.len() || bytes[i + 1].is_ascii_whitespace()) {
            return i;
        }
        i += 1;
    }
    bytes.len()
}

/// Blank out `<!-- … -->` comment bytes (keeping offsets) and carry the open
/// state across lines. Comment contents are not claims.
fn strip_html_comments(line: &str, in_comment: &mut bool) -> String {
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    loop {
        if *in_comment {
            match rest.find("-->") {
                Some(end) => {
                    out.extend(std::iter::repeat_n(' ', end + 3));
                    rest = &rest[end + 3..];
                    *in_comment = false;
                }
                None => {
                    out.extend(std::iter::repeat_n(' ', rest.len()));
                    return out;
                }
            }
        } else {
            match rest.find("<!--") {
                Some(start) => {
                    out.push_str(&rest[..start]);
                    out.extend(std::iter::repeat_n(' ', 4));
                    rest = &rest[start + 4..];
                    *in_comment = true;
                }
                None => {
                    out.push_str(rest);
                    return out;
                }
            }
        }
    }
}

/// Count tabs as four columns when locating list content.
fn columns(text: &str) -> usize {
    text.chars().map(|c| if c == '\t' { 4 } else { 1 }).sum()
}

/// CommonMark treats gaps of five or more spaces as one space plus indented code.
fn list_content_column(prefix: &str) -> usize {
    let marker = prefix.trim_end_matches([' ', '\t']);
    let gap = columns(&prefix[marker.len()..]);
    if gap >= 5 {
        columns(marker) + 1
    } else {
        columns(prefix)
    }
}

fn leading_indent(line: &str) -> usize {
    let ws_len = line.len() - line.trim_start_matches([' ', '\t']).len();
    columns(&line[..ws_len])
}

static LIST_ITEM: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s*(?:[-*+]|\d+[.)])\s+").expect("static regex"));

fn unquote(mut line: &str) -> &str {
    loop {
        let trimmed = line.trim_start_matches(' ');
        if line.len() - trimmed.len() > 3 {
            return line;
        }
        let Some(rest) = trimmed.strip_prefix('>') else {
            return line;
        };
        line = rest.strip_prefix([' ', '\t']).unwrap_or(rest);
    }
}

fn fence_content(mut line: &str) -> &str {
    loop {
        let content = unquote(line);
        if let Some(item) = LIST_ITEM.find(content) {
            line = &content[item.end()..];
        } else {
            return content.trim_start();
        }
    }
}

/// Claims and skip directives share the same code-block exclusions.
pub(crate) struct Extraction {
    pub claims: Vec<Claim>,
    pub skip_directive: bool,
}

#[cfg(test)]
pub(crate) fn extract_claims(markdown: &str) -> Vec<Claim> {
    extract(markdown).claims
}

pub(crate) fn extract(markdown: &str) -> Extraction {
    let mut claims = Vec::new();
    let mut skip_directive = false;
    let mut seen: HashSet<(usize, ClaimClass, String)> = HashSet::new();
    let mut fence: Option<String> = None;
    let mut in_comment = false;
    let mut prev_blank = true;
    let mut in_indented = false;
    let mut list_content_col: Option<usize> = None;

    for (idx, raw_line) in markdown.lines().enumerate() {
        let lineno = idx + 1;
        let raw_line = unquote(raw_line);
        if let Some(open) = &fence {
            if fence_content(raw_line).starts_with(open.as_str()) {
                fence = None;
                prev_blank = false;
            }
            continue;
        }
        // Check indentation before fences; a literal fence inside code opens nothing.
        // Lists measure four columns from their content start. Use the raw line
        // so blanked-out HTML comments cannot create indentation or separators.
        let blank = raw_line.trim().is_empty();
        let indent = leading_indent(raw_line);
        let code_col = list_content_col.map_or(4, |col| col + 4);
        if !blank && indent >= code_col && (prev_blank || in_indented) {
            in_indented = true;
            prev_blank = false;
            continue;
        }
        in_indented = false;
        if !in_comment && let Some(marker) = fence_marker(fence_content(raw_line)) {
            fence = Some(marker);
            prev_blank = false;
            continue;
        }
        prev_blank = blank;
        if blank {
            continue;
        }
        if let Some(m) = LIST_ITEM.find(raw_line) {
            list_content_col = Some(list_content_column(&raw_line[..m.end()]));
        } else if indent == 0 {
            list_content_col = None;
        }

        if !in_comment && SKIP_DIRECTIVE.is_match(raw_line.trim()) {
            skip_directive = true;
        }
        let line = strip_html_comments(raw_line, &mut in_comment);
        let line = line.as_str();

        let Some(spans) = code_spans(line) else {
            continue;
        };
        let mut push = |claim: Claim, claims: &mut Vec<Claim>| {
            if seen.insert((claim.line, claim.class, claim.text.clone())) {
                claims.push(claim);
            }
        };

        for &(start, end) in &spans {
            let tok = &line[start..end];
            if let Some(path) = classify_path(tok) {
                push(
                    Claim {
                        line: lineno,
                        class: ClaimClass::Path,
                        text: tok.to_string(),
                        detail: ClaimDetail::Path { path },
                    },
                    &mut claims,
                );
                continue;
            }
            if let Some((owner, member, shape)) = classify_symbol(tok) {
                push(
                    Claim {
                        line: lineno,
                        class: ClaimClass::Symbol,
                        text: tok.to_string(),
                        detail: ClaimDetail::Symbol {
                            owner,
                            member,
                            shape,
                            called: tok.ends_with("()"),
                        },
                    },
                    &mut claims,
                );
                continue;
            }
            if UPPER_SNAKE.is_match(tok) {
                let tail_start = line.len() - line[end..].trim_start_matches('`').len();
                let tail_end = sentence_end(line, tail_start);
                let tail = &line[tail_start..tail_end];
                if let Some(literal) = constant_literal(tail) {
                    push(
                        Claim {
                            line: lineno,
                            class: ClaimClass::Constant,
                            text: format!("{tok} = {literal}"),
                            detail: ClaimDetail::Constant {
                                name: tok.to_string(),
                                literal,
                            },
                        },
                        &mut claims,
                    );
                }
            }
        }

        for caps in LINK.captures_iter(line) {
            if caps.get(0).is_some_and(|link| {
                spans
                    .iter()
                    .any(|&(start, end)| link.start() >= start && link.start() < end)
            }) {
                continue;
            }
            let Some(target) = caps.get(1).map(|m| m.as_str()) else {
                continue;
            };
            let Some(detail) = classify_link(target) else {
                continue;
            };
            push(
                Claim {
                    line: lineno,
                    class: ClaimClass::Path,
                    text: target.to_string(),
                    detail,
                },
                &mut claims,
            );
        }

        for caps in ANCHOR.captures_iter(line) {
            let Some(path_m) = caps.get(1) else { continue };
            let Some(whole) = caps.get(0) else { continue };
            if !anchor_boundaries_ok(line, whole.start(), whole.end()) {
                continue;
            }
            let path = path_m.as_str();
            if path.starts_with('/')
                || path.starts_with('~')
                || path.split('/').any(|seg| seg == "..")
                || path.contains("//")
                || path.ends_with('/')
            {
                continue;
            }
            let Ok(line_no) = caps[2].parse::<u32>() else {
                continue;
            };
            let line_end = caps.get(3).and_then(|m| m.as_str().parse::<u32>().ok());
            let symbol = nearest_symbol_span(line, &spans, whole.start(), whole.end());
            push(
                Claim {
                    line: lineno,
                    class: ClaimClass::Anchor,
                    text: whole.as_str().to_string(),
                    detail: ClaimDetail::Anchor {
                        path: path.trim_start_matches("./").to_string(),
                        line: line_no,
                        line_end,
                        symbol,
                    },
                },
                &mut claims,
            );
        }
    }
    Extraction {
        claims,
        skip_directive,
    }
}

/// A markdown link target worth checking: `.md` pages and explicitly relative
/// targets resolve from the document; anything else must read as a
/// repo-relative path. URLs and same-page anchors are not claims.
fn classify_link(target: &str) -> Option<ClaimDetail> {
    let target = target.split('#').next().unwrap_or(target);
    if target.is_empty()
        || target.contains("://")
        || target.starts_with("mailto:")
        || target.starts_with(['<', '/', '~'])
    {
        return None;
    }
    if target.ends_with(".md") || target.starts_with("./") || target.starts_with("../") {
        return Some(ClaimDetail::Link {
            target: target.to_string(),
        });
    }
    classify_path(target).map(|path| ClaimDetail::Path { path })
}

fn fence_marker(trimmed: &str) -> Option<String> {
    for ch in ['`', '~'] {
        let run = trimmed.chars().take_while(|c| *c == ch).count();
        if run >= 3 {
            return Some(std::iter::repeat_n(ch, run).collect());
        }
    }
    None
}

/// Check boundaries without consuming the space shared by adjacent anchors.
fn anchor_boundaries_ok(line: &str, start: usize, end: usize) -> bool {
    let before_ok = line[..start]
        .chars()
        .next_back()
        .is_none_or(|c| c.is_whitespace() || matches!(c, '(' | '[' | '`'));
    let after_ok = line[end..]
        .chars()
        .next()
        .is_none_or(|c| !(c.is_alphanumeric() || matches!(c, '_' | '/' | ':' | '-')));
    before_ok && after_ok
}

fn nearest_symbol_span(
    line: &str,
    spans: &[(usize, usize)],
    anchor_start: usize,
    anchor_end: usize,
) -> Option<String> {
    let mut best: Option<(usize, &str)> = None;
    for &(start, end) in spans {
        if start <= anchor_start && anchor_end <= end {
            continue;
        }
        let tok = &line[start..end];
        if !IDENT_PATH.is_match(tok) || !tok.contains(|c: char| c.is_ascii_alphabetic()) {
            continue;
        }
        let distance = if end <= anchor_start {
            anchor_start - end
        } else {
            start.saturating_sub(anchor_end)
        };
        if distance > ANCHOR_SYMBOL_DISTANCE {
            continue;
        }
        if best.is_none_or(|(d, _)| distance < d) {
            best = Some((distance, tok));
        }
    }
    best.map(|(_, tok)| tok.to_string())
}

/// A backticked token that reads as a repo-relative file path. Globs, URLs,
/// placeholders, absolute and home-relative paths, flags, and domain-shaped
/// first segments are not path claims.
pub(crate) fn classify_path(tok: &str) -> Option<String> {
    let tok = tok.strip_suffix('.').unwrap_or(tok);
    if tok.is_empty()
        || tok.contains(|c: char| c.is_whitespace())
        || tok.contains([
            '*', '<', '>', '{', '}', '$', ':', '(', ')', '=', '|', ',', '\\', '"', '\'',
        ])
        || tok.contains("//")
        || tok.starts_with(['/', '~', '-'])
        || tok.split('/').any(|seg| seg == "..")
        || tok.ends_with('/')
    {
        return None;
    }
    let path = tok.strip_prefix("./").unwrap_or(tok);
    let (first, _) = path.split_once('/')?;
    if first.is_empty() || (first.contains('.') && !first.starts_with('.')) {
        return None;
    }
    let base = path.rsplit('/').next()?;
    let (stem, ext) = base.rsplit_once('.')?;
    if ext.is_empty()
        || ext.len() > 10
        || !ext.chars().all(|c| c.is_ascii_alphanumeric())
        || !ext.chars().any(|c| c.is_ascii_alphabetic())
    {
        return None;
    }
    // `crates/foo/Cargo.toml`, `docs/foo.md`: a stand-in, not a file.
    let segments = path.rsplit_once('/').map(|(dir, _)| dir).unwrap_or("");
    if segments
        .split('/')
        .chain(std::iter::once(stem))
        .any(|seg| PLACEHOLDER_SEGMENTS.contains(&seg))
    {
        return None;
    }
    Some(path.to_string())
}

fn is_camel_case(name: &str) -> bool {
    let mut chars = name.chars();
    chars.next().is_some_and(|c| c.is_ascii_lowercase())
        && name.chars().any(|c| c.is_ascii_uppercase())
        && !name.contains('_')
}

const FOREIGN_ROOTS: &[&str] = &["std", "core", "alloc"];
const PLACEHOLDER_SEGMENTS: &[&str] = &["foo", "bar", "baz", "qux"];
/// Owner names that documentation uses as stand-ins, never as real types.
const PLACEHOLDER_OWNERS: &[&str] = &["Type", "Class", "Foo", "Bar", "Baz", "X", "Y", "T", "U"];
const PLACEHOLDER_MEMBERS: &[&str] = &["method", "func", "fn", "name", "foo", "bar"];

/// Return owner, member, and syntax shape; placeholder spellings are not claims.
pub(crate) fn classify_symbol(tok: &str) -> Option<(Option<String>, String, SymbolShape)> {
    let without_arrow = tok.replace("->", "");
    if tok.contains(|c: char| c.is_whitespace())
        || without_arrow.contains(['<', '>'])
        || tok.contains([
            '/', '*', '"', '\'', '=', '[', ']', '{', '}', '$', '@', '#', ',', ';', '!', '\\',
        ])
        || tok.starts_with('-')
    {
        return None;
    }
    let called = tok.ends_with("()");
    let bare = tok.strip_suffix("()").unwrap_or(tok);
    if bare.is_empty() || bare.contains(['(', ')']) {
        return None;
    }
    let is_placeholder = |owner: &str, member: &str| {
        owner
            .split("::")
            .any(|seg| PLACEHOLDER_OWNERS.contains(&seg))
            || PLACEHOLDER_MEMBERS.contains(&member)
    };
    if bare.contains("::") {
        let segments: Vec<&str> = bare.split("::").collect();
        if segments.len() < 2 || !segments.iter().all(|s| IDENT.is_match(s)) {
            return None;
        }
        if FOREIGN_ROOTS.contains(&segments[0]) {
            return None;
        }
        let member = segments[segments.len() - 1];
        let owner = segments[..segments.len() - 1].join("::");
        if is_placeholder(&owner, member) {
            return None;
        }
        return Some((Some(owner), member.to_string(), SymbolShape::Path));
    }
    let member = bare.split_once("->").or_else(|| bare.split_once('.'));
    if let Some((ty, method)) = member {
        if TYPE_NAME.is_match(ty) && IDENT.is_match(method) && !is_placeholder(ty, method) {
            return Some((
                Some(ty.to_string()),
                method.to_string(),
                SymbolShape::Member,
            ));
        }
        return None;
    }
    if called && IDENT.is_match(bare) && !is_placeholder("", bare) {
        return Some((None, bare.to_string(), SymbolShape::Call));
    }
    None
}

/// The literal a sentence attaches to a constant name: `= 5`, `is 5`,
/// `(default 5)`, or `defaults to 5`, with no other code span in between and
/// no unit word after the number.
pub(crate) fn constant_literal(tail: &str) -> Option<String> {
    let caps = CONSTANT_TAIL.captures(tail)?;
    let before = &caps[1];
    if before.trim().ends_with(|c: char| c.is_ascii_alphanumeric())
        && caps.get(0)?.as_str()[before.len()..].starts_with('=')
    {
        // `FOO_BAR=1` style assignments belong to a different token.
        return None;
    }
    let literal = caps.get(3)?;
    let after = &tail[literal.end() + caps.get(4).map_or(0, |m| m.len())..];
    let unit: String = after
        .trim_start()
        .chars()
        .take_while(|c| c.is_ascii_alphabetic() || *c == '%')
        .collect::<String>()
        .to_ascii_lowercase();
    if !unit.is_empty() && UNIT_WORDS.contains(&unit.as_str()) {
        return None;
    }
    Some(literal.as_str().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use codesage_protocol::{FileInfo, Language};

    const FIXTURE: &str = r#"# Fixture

The parser lives in `crates/parser/src/lib.rs` and the glob `**/tests/**` is excluded.
Visit `https://example.com/a/b.md` or read `docs/<name>.md`; set `CODESAGE_WATCH=0` or `CODESAGE_WATCH`.
Pass `--json` to any command; the flag `--adaptive-limit` cuts the page.
The cut happens in `search.rs` near `crates/graph/src/search.rs:2185` inside `truncate_page`.
Call `Database::open_in_memory()` or `Reranker.score`; plain `snake_case_word` and `Vec` are not claims.
`BATCH_SIZE` defaults to 32 and `MAX_CHUNK_CHARS` is 1,500,000; `CHUNKER_VERSION` 1 → 2 says nothing.
The std path `std::process::exit` is foreign, as are `Type::method` and `Foo.bar` placeholders.

```rust
let x = `crates/nowhere/src/gone.rs`; // fenced: ignored
```

A range anchor `crates/cli/src/main.rs:10-20` and a plain-text one crates/cli/src/util.rs:3.
<!-- `crates/commented/out.rs` is not a claim,
nor is crates/commented/out.rs:9 on the next line --> but `crates/after/comment.rs` is.

    `crates/indented/code.rs` sits in an indented code block

- a list item
    whose `crates/list/continuation.rs` continuation is prose

See [the guide](./guide.md#intro), [a file](crates/linked/file.rs), and [home](https://example.com/x.md).
A column anchor crates/cli/src/main.rs:12:7 is fine.
"#;

    fn claims_of(class: ClaimClass) -> Vec<Claim> {
        extract_claims(FIXTURE)
            .into_iter()
            .filter(|c| c.class == class)
            .collect()
    }

    #[test]
    fn extracts_path_claims_and_skips_glob_url_placeholder_env_flag() {
        let paths: Vec<String> = claims_of(ClaimClass::Path)
            .into_iter()
            .map(|c| c.text)
            .collect();
        assert_eq!(
            paths,
            vec![
                "crates/parser/src/lib.rs".to_string(),
                "crates/after/comment.rs".to_string(),
                "crates/list/continuation.rs".to_string(),
                "./guide.md#intro".to_string(),
                "crates/linked/file.rs".to_string(),
            ]
        );
    }

    #[test]
    fn html_comments_and_indented_code_are_not_claims() {
        let all = extract_claims(FIXTURE);
        assert!(all.iter().all(|c| !c.text.contains("commented")), "{all:?}");
        assert!(all.iter().all(|c| !c.text.contains("indented")), "{all:?}");
    }

    #[test]
    fn link_targets_resolve_md_relative_and_skip_urls() {
        let links: Vec<ClaimDetail> = claims_of(ClaimClass::Path)
            .into_iter()
            .filter(|c| matches!(c.detail, ClaimDetail::Link { .. }))
            .map(|c| c.detail)
            .collect();
        assert_eq!(
            links,
            vec![ClaimDetail::Link {
                target: "./guide.md".into()
            }]
        );
    }

    #[test]
    fn extracts_anchor_claims_with_nearby_symbol() {
        let anchors = claims_of(ClaimClass::Anchor);
        assert_eq!(anchors.len(), 4, "{anchors:?}");
        assert_eq!(
            anchors[0].detail,
            ClaimDetail::Anchor {
                path: "crates/graph/src/search.rs".into(),
                line: 2185,
                line_end: None,
                symbol: Some("truncate_page".into()),
            }
        );
        assert_eq!(anchors[0].line, 6);
        assert_eq!(
            anchors[1].detail,
            ClaimDetail::Anchor {
                path: "crates/cli/src/main.rs".into(),
                line: 10,
                line_end: Some(20),
                symbol: None,
            }
        );
        assert_eq!(anchors[2].text, "crates/cli/src/util.rs:3");
        assert_eq!(anchors[3].text, "crates/cli/src/main.rs:12:7");
        assert_eq!(
            anchors[3].detail,
            ClaimDetail::Anchor {
                path: "crates/cli/src/main.rs".into(),
                line: 12,
                line_end: None,
                symbol: None,
            }
        );
    }

    #[test]
    fn extracts_symbol_claims_and_skips_plain_words_std_and_placeholders() {
        let symbols: Vec<String> = claims_of(ClaimClass::Symbol)
            .into_iter()
            .map(|c| c.text)
            .collect();
        assert_eq!(
            symbols,
            vec![
                "Database::open_in_memory()".to_string(),
                "Reranker.score".to_string()
            ]
        );
    }

    #[test]
    fn extracts_constant_claims_only_with_a_literal() {
        let constants: Vec<ClaimDetail> = claims_of(ClaimClass::Constant)
            .into_iter()
            .map(|c| c.detail)
            .collect();
        assert_eq!(
            constants,
            vec![
                ClaimDetail::Constant {
                    name: "BATCH_SIZE".into(),
                    literal: "32".into()
                },
                ClaimDetail::Constant {
                    name: "MAX_CHUNK_CHARS".into(),
                    literal: "1,500,000".into()
                },
            ]
        );
    }

    #[test]
    fn adjacent_anchors_and_urls() {
        let claims =
            extract_claims("See a/b.rs:1 a/c.rs:2 and https://x.y/z.md:3 or (a/d.rs:4).\n");
        let texts: Vec<&str> = claims.iter().map(|c| c.text.as_str()).collect();
        assert_eq!(texts, vec!["a/b.rs:1", "a/c.rs:2", "a/d.rs:4"]);
    }

    #[test]
    fn unpaired_backtick_line_is_skipped() {
        let claims = extract_claims("open span ` `crates/x/y.rs` and a/b.rs:1\nplain `a/c.rs`\n");
        let texts: Vec<&str> = claims.iter().map(|c| c.text.as_str()).collect();
        assert_eq!(texts, vec!["a/c.rs"]);
    }

    #[test]
    fn fenced_blocks_are_ignored() {
        assert!(
            extract_claims(FIXTURE)
                .iter()
                .all(|c| !c.text.contains("nowhere"))
        );
    }

    #[test]
    fn classify_path_rules() {
        assert_eq!(classify_path("a/b.rs"), Some("a/b.rs".into()));
        assert_eq!(classify_path("a/b.rs."), Some("a/b.rs".into()));
        assert_eq!(classify_path("./a/b.rs"), Some("a/b.rs".into()));
        assert_eq!(
            classify_path(".github/workflows/ci.yml"),
            Some(".github/workflows/ci.yml".into())
        );
        assert_eq!(classify_path("a/*.rs"), None);
        assert_eq!(classify_path("a/**/b.rs"), None);
        assert_eq!(classify_path("https://x.y/a.md"), None);
        assert_eq!(classify_path("github.com/foo/bar.git"), None);
        assert_eq!(classify_path("mcp-<version>-<key>.sock"), None);
        assert_eq!(classify_path("a/<name>.md"), None);
        assert_eq!(classify_path("/usr/lib/x.so"), None);
        assert_eq!(classify_path("~/ai/wiki/x.md"), None);
        assert_eq!(classify_path("--manifest-path"), None);
        assert_eq!(classify_path("a/b"), None);
        assert_eq!(classify_path("a/b/"), None);
        assert_eq!(classify_path("lib.rs"), None);
        assert_eq!(classify_path("tests/{Unit,Feature}/x.php"), None);
        assert_eq!(classify_path("crates/foo/Cargo.toml"), None);
        assert_eq!(classify_path("docs/foo.md"), None);
        assert_eq!(classify_path("a/bar/baz.rs"), None);
        assert_eq!(classify_path("a/foobar.rs"), Some("a/foobar.rs".into()));
    }

    #[test]
    fn classify_symbol_rules() {
        assert_eq!(
            classify_symbol("Foo2::bar_fn"),
            Some((Some("Foo2".into()), "bar_fn".into(), SymbolShape::Path))
        );
        assert_eq!(
            classify_symbol("a::b::c()"),
            Some((Some("a::b".into()), "c".into(), SymbolShape::Path))
        );
        assert_eq!(
            classify_symbol("main()"),
            Some((None, "main".into(), SymbolShape::Call))
        );
        assert_eq!(
            classify_symbol("Node->children"),
            Some((Some("Node".into()), "children".into(), SymbolShape::Member))
        );
        assert_eq!(classify_symbol("Type::method"), None);
        assert_eq!(classify_symbol("Type->method"), None);
        assert_eq!(classify_symbol("Foo::bar"), None);
        assert_eq!(classify_symbol("Class.name"), None);
        assert_eq!(classify_symbol("foo()"), None);
        assert_eq!(classify_symbol("std::process::exit"), None);
        assert_eq!(classify_symbol("site.getsitepackages()"), None);
        assert_eq!(classify_symbol("snake_case"), None);
        assert_eq!(classify_symbol("ALL_CAPS"), None);
        assert_eq!(classify_symbol("--flag"), None);
        assert_eq!(classify_symbol("Vec<T>::new"), None);
        assert_eq!(classify_symbol("a.b.c"), None);
    }

    #[test]
    fn constant_literal_forms() {
        assert_eq!(constant_literal(" = 5"), Some("5".into()));
        assert_eq!(constant_literal(" is 1_500_000."), Some("1_500_000".into()));
        assert_eq!(constant_literal(" (default 50)"), Some("50".into()));
        assert_eq!(constant_literal(" defaults to `1.5M`"), Some("1.5M".into()));
        assert_eq!(constant_literal(r#" is "abc""#), Some("\"abc\"".into()));
        assert_eq!(constant_literal(" is 32 and more"), Some("32".into()));
        assert_eq!(constant_literal(" in `x.rs` is 5"), None);
        assert_eq!(constant_literal(" is set by the operator"), None);
        assert_eq!(constant_literal(" 1 → 2"), None);
    }

    #[test]
    fn constant_literal_refuses_unit_suffixed_quantities() {
        assert_eq!(constant_literal(" is 30 seconds"), None);
        assert_eq!(constant_literal(" defaults to 30 minutes"), None);
        assert_eq!(constant_literal(" is 30s"), None);
        assert_eq!(constant_literal(" is 5%"), None);
        assert_eq!(constant_literal(" = 4 MiB"), None);
    }

    #[test]
    fn literal_normalisation() {
        assert_eq!(normalize_literal("1,500,000"), "1500000");
        assert_eq!(normalize_literal("1_500_000"), "1500000");
        assert_eq!(normalize_literal("1.5M"), "1500000");
        assert_eq!(normalize_literal("`32`"), "32");
        assert_eq!(normalize_literal("0x10"), "16");
        assert_eq!(normalize_literal("\"gpu\""), "gpu");
        assert_eq!(normalize_literal("2.5"), "2.5");
    }

    #[test]
    fn source_literal_refuses_expressions() {
        assert_eq!(source_literal("1_500_000usize"), Some("1_500_000".into()));
        assert_eq!(source_literal("\"x\""), Some("\"x\"".into()));
        assert_eq!(source_literal("60 * 5"), None);
        assert_eq!(source_literal("Duration::from_secs(5)"), None);
        assert_eq!(source_literal("&[\"a\"]"), None);
    }

    #[test]
    fn source_literal_never_truncates_unsupported_numeric_forms() {
        for value in [
            "0b1010",
            "0o12",
            "1e3",
            "1E3",
            "10garbage",
            "1u128garbage",
            "trueu8",
            "\"x\"u8",
        ] {
            assert_eq!(source_literal(value), None, "{value}");
        }
        assert_eq!(source_literal("32u8"), Some("32".into()));
        assert_eq!(source_literal("2.0f32"), Some("2.0".into()));
    }

    #[test]
    fn indented_skip_directive_examples_do_not_skip_the_document() {
        for example in [
            "    <!-- codesage-docs: skip-file -->\n",
            "- Example:\n\n      <!-- codesage-docs: skip-file -->\n",
        ] {
            let markdown = format!("{example}\n[broken](./missing.md)\n");
            let extracted = extract(&markdown);
            assert!(!extracted.skip_directive, "{markdown}");
            let (_, findings) = check(&seeded(), &markdown);
            assert_eq!(findings.len(), 1, "{findings:?}");
        }
        assert!(extract("<!-- codesage-docs: skip-file -->\n").skip_directive);
    }

    #[test]
    fn inline_code_link_examples_do_not_become_links() {
        let markdown = "Use `[example](./absent.md)` or ``[example](./absent.md)``.\n[broken](./missing.md) and `src/gone.rs`.\n";
        let (_, findings) = check(&seeded(), markdown);
        let claims: Vec<_> = findings.iter().map(|f| f.claim.as_str()).collect();
        assert_eq!(claims, vec!["src/gone.rs", "./missing.md"]);
    }

    #[test]
    fn container_fences_hide_examples_but_quoted_prose_is_checked() {
        for example in [
            "> ```markdown\n> [example](./absent.md)\n> ```\n",
            "- ```markdown\n  [example](./absent.md)\n  ```\n",
            "> - ```markdown\n>   [example](./absent.md)\n>   ```\n",
            "- > ```markdown\n  > [example](./absent.md)\n  > ```\n",
        ] {
            let markdown = format!("{example}\n> [broken](./missing.md)\n");
            let (_, findings) = check(&seeded(), &markdown);
            assert_eq!(findings.len(), 1, "{markdown}: {findings:?}");
            assert_eq!(findings[0].claim, "./missing.md");
        }
    }

    #[test]
    fn gitignore_roots_keep_plain_entries_only() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".gitignore"),
            "# c\n/target\n.plan/\n*.db\n!/keep\n/docs/spikes/x.md\nnotes\n",
        )
        .unwrap();
        let roots = gitignored_roots(dir.path());
        let mut got: Vec<&str> = roots.iter().map(String::as_str).collect();
        got.sort_unstable();
        assert_eq!(got, vec![".codesage", ".plan", "notes", "target"]);
    }

    #[test]
    fn dependency_crates_are_read_from_manifests() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("crates/a")).unwrap();
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[workspace]\nmembers = [\"crates/a\"]\n[workspace.dependencies]\ntokio = \"1\"\ncodesage-a = { path = \"crates/a\" }\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("crates/a/Cargo.toml"),
            "[package]\nname = \"codesage-a\"\n[dependencies]\nsj = { package = \"serde-json\", version = \"1\" }\ncodesage-a.workspace = true\n[dev-dependencies]\ntempfile = \"3\"\n[target.'cfg(unix)'.dependencies]\nlibc = \"0.2\"\n",
        )
        .unwrap();
        let deps = dependency_crates(dir.path());
        for name in ["tokio", "sj", "serde_json", "tempfile", "libc"] {
            assert!(deps.contains(name), "{name} missing from {deps:?}");
        }
        assert!(
            !deps.contains("codesage_a"),
            "workspace member must stay checkable: {deps:?}"
        );
    }

    struct Fixture {
        dir: tempfile::TempDir,
        db: Database,
    }

    fn sym(
        name: &str,
        qualified: &str,
        kind: SymbolKind,
        file: &str,
        start: u32,
        end: u32,
    ) -> Symbol {
        Symbol {
            name: name.into(),
            qualified_name: qualified.into(),
            kind,
            file_path: file.into(),
            line_start: start,
            line_end: end,
            col_start: 0,
            col_end: 0,
            rationale: Vec::new(),
        }
    }

    fn seeded() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("src/db")).unwrap();
        std::fs::create_dir_all(root.join("docs/sub")).unwrap();
        std::fs::create_dir_all(root.join(".codesage")).unwrap();
        std::fs::write(root.join(".gitignore"), "/target\n/.codesage/\n").unwrap();
        std::fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = \"fx\"\n[dependencies]\ntokio = \"1\"\n",
        )
        .unwrap();
        std::fs::write(root.join("docs/guide.md"), "# guide\n").unwrap();
        let lib = "pub const MAX_CHUNK_CHARS: usize = 1_500_000;\n\
                   pub const BATCH_SIZE: usize = 32;\n\
                   pub const TIMEOUT: u64 = 60 * 5;\n\
                   pub const DEADLINE_MS: u64 = 30_000;\n\
                   pub fn truncate_page() {\n\
                   }\n\
                   pub enum Verdict { Holds }\n\
                   pub struct Database;\n\
                   impl Database {\n\
                   \x20   pub fn open_in_memory() {}\n\
                   }\n\
                   pub struct MapperContext { pub excludes: u8 }\n";
        std::fs::write(root.join("src/lib.rs"), lib).unwrap();
        std::fs::write(root.join("src/db/mod.rs"), "fn a() {}\nfn new() {}\n").unwrap();

        let db = Database::open_in_memory().unwrap();
        let lib_id = db
            .upsert_file(&FileInfo {
                path: "src/lib.rs".into(),
                language: Language::Rust,
                content_hash: "h1".into(),
            })
            .unwrap();
        db.insert_symbols(
            lib_id,
            &[
                sym(
                    "MAX_CHUNK_CHARS",
                    "MAX_CHUNK_CHARS",
                    SymbolKind::Constant,
                    "src/lib.rs",
                    1,
                    1,
                ),
                sym(
                    "BATCH_SIZE",
                    "BATCH_SIZE",
                    SymbolKind::Constant,
                    "src/lib.rs",
                    2,
                    2,
                ),
                sym(
                    "TIMEOUT",
                    "TIMEOUT",
                    SymbolKind::Constant,
                    "src/lib.rs",
                    3,
                    3,
                ),
                sym(
                    "DEADLINE_MS",
                    "DEADLINE_MS",
                    SymbolKind::Constant,
                    "src/lib.rs",
                    4,
                    4,
                ),
                sym(
                    "truncate_page",
                    "truncate_page",
                    SymbolKind::Function,
                    "src/lib.rs",
                    5,
                    6,
                ),
                sym("Verdict", "Verdict", SymbolKind::Enum, "src/lib.rs", 7, 7),
                sym(
                    "Database",
                    "Database",
                    SymbolKind::Struct,
                    "src/lib.rs",
                    8,
                    8,
                ),
                sym(
                    "open_in_memory",
                    "Database::open_in_memory",
                    SymbolKind::Method,
                    "src/lib.rs",
                    10,
                    10,
                ),
                sym(
                    "MapperContext",
                    "MapperContext",
                    SymbolKind::Struct,
                    "src/lib.rs",
                    12,
                    12,
                ),
            ],
        )
        .unwrap();
        let mod_id = db
            .upsert_file(&FileInfo {
                path: "src/db/mod.rs".into(),
                language: Language::Rust,
                content_hash: "h2".into(),
            })
            .unwrap();
        db.insert_symbols(
            mod_id,
            &[
                sym(
                    "search",
                    "search",
                    SymbolKind::Module,
                    "src/db/mod.rs",
                    1,
                    2,
                ),
                sym("a", "a", SymbolKind::Function, "src/db/mod.rs", 1, 1),
                sym("new", "new", SymbolKind::Function, "src/db/mod.rs", 2, 2),
            ],
        )
        .unwrap();
        std::fs::create_dir_all(root.join("src/db/net")).unwrap();
        std::fs::write(root.join("src/db/net/mux.go"), "type Mux struct{}\n").unwrap();
        let go_id = db
            .upsert_file(&FileInfo {
                path: "src/db/net/mux.go".into(),
                language: Language::Go,
                content_hash: "h3".into(),
            })
            .unwrap();
        db.insert_symbols(
            go_id,
            &[sym(
                "Mux",
                "Mux",
                SymbolKind::Struct,
                "src/db/net/mux.go",
                1,
                1,
            )],
        )
        .unwrap();
        std::fs::create_dir_all(root.join("tests/fixtures")).unwrap();
        std::fs::write(
            root.join("tests/fixtures/sample.php"),
            "<?php\nclass UserController {}\nconst FIXTURE_LIMIT = 5;\n",
        )
        .unwrap();
        let fixture_id = db
            .upsert_file(&FileInfo {
                path: "tests/fixtures/sample.php".into(),
                language: Language::Php,
                content_hash: "h4".into(),
            })
            .unwrap();
        db.insert_symbols(
            fixture_id,
            &[
                sym(
                    "UserController",
                    "UserController",
                    SymbolKind::Class,
                    "tests/fixtures/sample.php",
                    2,
                    2,
                ),
                sym(
                    "FIXTURE_LIMIT",
                    "FIXTURE_LIMIT",
                    SymbolKind::Constant,
                    "tests/fixtures/sample.php",
                    3,
                    3,
                ),
            ],
        )
        .unwrap();
        Fixture { dir, db }
    }

    fn check_at(fx: &Fixture, rel: &str, markdown: &str) -> Report {
        let path = fx.dir.path().join(rel);
        std::fs::write(&path, markdown).unwrap();
        check_documents(fx.dir.path(), &fx.db, &[path], &[]).unwrap()
    }

    fn check(fx: &Fixture, markdown: &str) -> (usize, Vec<Finding>) {
        let report = check_at(fx, "DOC.md", markdown);
        assert_eq!(report.files, vec!["DOC.md".to_string()]);
        (report.claims_checked, report.drifted)
    }

    #[test]
    fn clean_document_reports_no_drift() {
        let fx = seeded();
        let (checked, drifted) = check(
            &fx,
            "See `src/lib.rs` and `src/db/mod.rs`. `truncate_page` sits at `src/lib.rs:5-6`.\n\
             Use `Database::open_in_memory()`; `MAX_CHUNK_CHARS` is 1.5M and `BATCH_SIZE` = 32.\n\
             `TIMEOUT` is 300 (an expression in source, so undecidable and uncounted).\n\
             `app/Http/Kernel.php` is a foreign example and uncounted.\n\
             `DEADLINE_MS` is 30 seconds: a quantity, not the literal, so uncounted.\n\
             Read [the guide](docs/guide.md) and `.codesage/watch.disabled` (gitignored, uncounted).\n",
        );
        assert!(drifted.is_empty(), "{drifted:?}");
        assert_eq!(checked, 7);
    }

    #[test]
    fn missing_path_is_reported_with_basename_suggestion() {
        let fx = seeded();
        // Distinguish distinctive shared directories from generic names and duplicate basenames.
        let root = fx.dir.path();
        std::fs::create_dir_all(root.join("src/web")).unwrap();
        for file in ["src/mux2.go", "src/index.js", "src/web/index.js"] {
            std::fs::write(root.join(file), "\n").unwrap();
            fx.db
                .upsert_file(&FileInfo {
                    path: file.into(),
                    language: Language::Go,
                    content_hash: file.into(),
                })
                .unwrap();
        }
        let (checked, drifted) = check(
            &fx,
            "Edit `src/db/mux.go`, `src/db/mux2.go`, `src/db/lib.rs`, and `src/db/index.js`.\n",
        );
        assert_eq!(checked, 4);
        let hints: Vec<(&str, Option<&str>, &[String])> = drifted
            .iter()
            .map(|f| {
                (
                    f.claim.as_str(),
                    f.suggestion.as_deref(),
                    f.candidates.as_slice(),
                )
            })
            .collect();
        assert_eq!(drifted[0].class, ClaimClass::Path);
        assert_eq!(drifted[0].doc_line, 1);
        assert_eq!(drifted[0].reason, "file not found on disk or in the index");
        assert_eq!(
            hints,
            vec![
                (
                    "src/db/mux.go",
                    Some("did you mean src/db/net/mux.go"),
                    &["src/db/net/mux.go".to_string()][..]
                ),
                (
                    "src/db/mux2.go",
                    Some("a file of the same name exists elsewhere: src/mux2.go"),
                    &["src/mux2.go".to_string()][..]
                ),
                ("src/db/lib.rs", None, &[][..]),
                ("src/db/index.js", None, &[][..]),
            ]
        );
    }

    #[test]
    fn internal_document_gets_the_directory_relocation_too() {
        let fx = seeded();
        let root = fx.dir.path();
        std::fs::create_dir_all(root.join("src/db/mods")).unwrap();
        std::fs::write(root.join("src/db/mods/x.rs"), "\n").unwrap();
        fx.db
            .upsert_file(&FileInfo {
                path: "src/db/mods/x.rs".into(),
                language: Language::Rust,
                content_hash: "m".into(),
            })
            .unwrap();
        let (checked, drifted) = check(&fx, "See `src/db/mods.rs`.\n");
        assert_eq!(checked, 1);
        assert_eq!(drifted.len(), 1);
        assert_eq!(
            drifted[0].reason,
            "file not found; the module is now a directory"
        );
        assert_eq!(drifted[0].suggestion.as_deref(), Some("src/db/mods/"));
        assert_eq!(drifted[0].candidates, vec!["src/db/mods/"]);
    }

    #[test]
    fn missing_explicit_path_is_a_failure_not_a_clean_sweep() {
        let fx = seeded();
        let root = fx.dir.path();
        std::fs::write(root.join("Cargo.lock"), "\n").unwrap();
        let selection = explicit_docs(&[
            root.join("typo-AGENTS.md"),
            root.join("nowhere.txt"),
            root.join("Cargo.lock"),
            root.join("docs/guide.md"),
        ]);
        assert_eq!(
            selection.missing,
            vec![root.join("typo-AGENTS.md"), root.join("nowhere.txt")]
        );
        assert_eq!(selection.not_markdown, vec![root.join("Cargo.lock")]);
        assert_eq!(selection.docs.len(), 1);
    }

    #[test]
    fn fixture_candidates_never_unlock_an_external_report() {
        let fx = seeded();
        let root = fx.dir.path();
        std::fs::create_dir_all(root.join("tests/fixtures")).unwrap();
        for (file, lang) in [
            ("tests/fixtures/sample.rs", Language::Rust),
            ("tests/impact_test.rs", Language::Rust),
        ] {
            std::fs::write(root.join(file), "\n").unwrap();
            fx.db
                .upsert_file(&FileInfo {
                    path: file.into(),
                    language: lang,
                    content_hash: file.into(),
                })
                .unwrap();
        }
        let outside = tempfile::tempdir().unwrap();
        let doc = outside.path().join("note.md");
        std::fs::write(
            &doc,
            "`src/db/sample.rs` has only a fixture namesake; `src/db/impact_test.rs` has a real test file.\n",
        )
        .unwrap();
        let report = check_documents(root, &fx.db, &[doc], &[]).unwrap();
        let claims: Vec<(&str, Option<&str>)> = report
            .drifted
            .iter()
            .map(|f| (f.claim.as_str(), f.suggestion.as_deref()))
            .collect();
        assert_eq!(
            claims,
            vec![(
                "src/db/impact_test.rs",
                Some("a file of the same name exists elsewhere: tests/impact_test.rs")
            )]
        );
        assert_eq!(report.claims_checked, 1);
    }

    #[test]
    #[cfg(unix)]
    fn explicit_changelog_and_non_regular_files_are_named_as_skipped() {
        let fx = seeded();
        let root = fx.dir.path();
        std::fs::write(root.join("CHANGELOG.md"), "`src/gone.rs`\n").unwrap();
        let socket = root.join("pipe.md");
        let _listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        let selection = explicit_docs(&[root.join("CHANGELOG.md"), socket.clone()]);
        assert!(selection.docs.is_empty(), "{:?}", selection.docs);
        assert_eq!(selection.changelog, vec![root.join("CHANGELOG.md")]);
        assert_eq!(selection.not_markdown, vec![socket]);
        assert!(selection.missing.is_empty());
        let defaults = default_docs(root).docs;
        assert!(defaults.iter().all(|d| !is_changelog(d)), "{defaults:?}");
    }

    #[test]
    #[cfg(unix)]
    fn links_resolve_from_the_real_document_directory() {
        let fx = seeded();
        let root = fx.dir.path();
        std::fs::write(root.join("docs/sibling.md"), "# s\n").unwrap();
        std::fs::write(
            root.join("docs/guide.md"),
            "See [sibling](./sibling.md) and [guide](guide.md).\n",
        )
        .unwrap();
        std::fs::create_dir_all(root.join("elsewhere")).unwrap();
        std::os::unix::fs::symlink(root.join("docs/guide.md"), root.join("elsewhere/guide.md"))
            .unwrap();
        let selection = explicit_docs(&[root.join("elsewhere/guide.md")]);
        let report = check_documents(root, &fx.db, &selection.docs, &[]).unwrap();
        assert_eq!(report.claims_checked, 2, "{:?}", report.drifted);
        assert!(report.drifted.is_empty(), "{:?}", report.drifted);
    }

    #[test]
    fn external_document_never_reports_symbols() {
        let fx = seeded();
        let outside = tempfile::tempdir().unwrap();
        let doc = outside.path().join("note.md");
        std::fs::write(&doc, "Call `Database::gone_method()`.\n").unwrap();
        let report = check_documents(fx.dir.path(), &fx.db, &[doc], &[]).unwrap();
        assert_eq!(report.claims_checked, 0, "{:?}", report.drifted);
        assert!(report.drifted.is_empty());
    }

    #[test]
    fn fixture_constants_supply_no_evidence() {
        let fx = seeded();
        let (checked, drifted) = check(&fx, "`FIXTURE_LIMIT` is 7.\n");
        assert_eq!(checked, 0, "{drifted:?}");
        assert!(drifted.is_empty());
    }

    #[test]
    fn missing_path_needs_an_existing_parent_directory() {
        let fx = seeded();
        // `src/db` exists; `docs/code/Services` and `.github/actions/x` do not.
        let (checked, drifted) = check(
            &fx,
            "`src/db/gone.rs`, `docs/code/Services/Provider.md`, `.github/actions/test-linux/action.yml`, and `src/gone.rs`.\n",
        );
        assert_eq!(checked, 2, "{drifted:?}");
        let claims: Vec<&str> = drifted.iter().map(|f| f.claim.as_str()).collect();
        assert_eq!(claims, vec!["src/db/gone.rs", "src/gone.rs"]);
    }

    #[test]
    fn external_document_skips_two_segment_ubiquitous_paths() {
        let fx = seeded();
        let outside = tempfile::tempdir().unwrap();
        let doc = outside.path().join("note.md");
        std::fs::write(
            &doc,
            "`docs/x.md`, `scripts/run.sh`, `src/gone.rs` are another repo's; `src/db/mux.go` moved.\n",
        )
        .unwrap();
        let report = check_documents(fx.dir.path(), &fx.db, &[doc], &[]).unwrap();
        assert_eq!(report.claims_checked, 1, "{:?}", report.drifted);
        assert_eq!(report.drifted.len(), 1);
        assert_eq!(report.drifted[0].claim, "src/db/mux.go");

        let (checked, drifted) = check(
            &fx,
            "`docs/x.md`, `scripts/run.sh`, `src/gone.rs` are another repo's; `src/db/mux.go` moved.\n",
        );
        assert_eq!(checked, 3, "{drifted:?}");
    }

    #[test]
    fn external_document_reports_only_concrete_relocations() {
        let fx = seeded();
        let outside = tempfile::tempdir().unwrap();
        let doc = outside.path().join("note.md");
        // External findings require unambiguous relocations with the same source kind.
        let root = fx.dir.path();
        for (dir, file, lang) in [
            ("src/db/mods", "src/db/mods/x.rs", Language::Rust),
            ("src/db/queries", "src/db/queries/x.go", Language::Go),
            ("src/db/old", "src/db/util.rs", Language::Rust),
            ("src", "src/util.rs", Language::Rust),
            ("src/other", "src/other/thing.rs", Language::Rust),
        ] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
            std::fs::write(root.join(file), "\n").unwrap();
            fx.db
                .upsert_file(&FileInfo {
                    path: file.into(),
                    language: lang,
                    content_hash: file.into(),
                })
                .unwrap();
        }
        std::fs::write(
            &doc,
            "`src/db/README.md`, `src/db/lib.rs`, `src/db/mux.go`, `src/db/mods.rs`, `src/db/queries.rs`, `src/db/old/util.rs`, `src/db/thing.rs`, and `src/db/gone.rs`.\n",
        )
        .unwrap();
        let report = check_documents(root, &fx.db, &[doc], &[]).unwrap();
        let claims: Vec<(&str, Option<&str>)> = report
            .drifted
            .iter()
            .map(|f| (f.claim.as_str(), f.suggestion.as_deref()))
            .collect();
        assert_eq!(
            claims,
            vec![
                ("src/db/mux.go", Some("did you mean src/db/net/mux.go")),
                ("src/db/mods.rs", Some("src/db/mods/")),
                (
                    "src/db/thing.rs",
                    Some("a file of the same name exists elsewhere: src/other/thing.rs"),
                ),
            ]
        );
        assert_eq!(report.claims_checked, 3);
    }

    #[test]
    fn camel_case_member_on_snake_case_owner_is_undecidable() {
        let fx = seeded();
        let (checked, drifted) = check(
            &fx,
            "`Database::renderToFile()` is another codebase's Database; `Database::gone_method()` is ours.\n",
        );
        assert_eq!(checked, 1, "{drifted:?}");
        assert_eq!(drifted.len(), 1);
        assert_eq!(drifted[0].claim, "Database::gone_method()");
    }

    #[test]
    fn go_owner_keeps_camel_case_members_checkable() {
        let fx = seeded();
        let (checked, drifted) = check(&fx, "Route with `Mux::routeHTTP()`.\n");
        assert_eq!(checked, 1, "{drifted:?}");
        assert_eq!(drifted.len(), 1);
        assert_eq!(drifted[0].claim, "Mux::routeHTTP()");
    }

    #[test]
    fn fixture_only_owner_is_undecidable() {
        let fx = seeded();
        let (checked, drifted) = check(&fx, "See `UserController::store()`.\n");
        assert_eq!(checked, 0, "{drifted:?}");
        assert!(drifted.is_empty(), "{drifted:?}");
    }

    #[test]
    fn parent_directory_escapes_are_not_claims() {
        let fx = seeded();
        let (checked, drifted) = check(
            &fx,
            "Neither `src/../../etc/passwd` nor src/../../etc/passwd:1 nor `src/db/../../x.rs` is a claim.\n",
        );
        assert_eq!(checked, 0, "{drifted:?}");
        assert!(drifted.is_empty(), "{drifted:?}");
        assert_eq!(classify_path("crates/../../wiki/index.md"), None);
        assert!(extract_claims("see crates/../../wiki/index.md:999999 here\n").is_empty());
    }

    #[test]
    fn extensionless_links_resolve_like_a_static_site_generator() {
        let fx = seeded();
        let root = fx.dir.path();
        std::fs::create_dir_all(root.join("docs/basics")).unwrap();
        std::fs::create_dir_all(root.join("docs/api")).unwrap();
        std::fs::write(root.join("docs/part-3.md"), "# p\n").unwrap();
        std::fs::write(root.join("docs/basics/index.md"), "# b\n").unwrap();
        std::fs::write(root.join("docs/api/README.md"), "# a\n").unwrap();
        std::fs::write(root.join("docs/Style-Guide.md"), "# s\n").unwrap();
        let report = check_at(
            &fx,
            "docs/sub/page.md",
            "[a](../part-3) [b](../basics/) [c](../api) [d](../style-guide) [e](../nothing) [f](../nothing/) [g](../nothing.md)\n",
        );
        assert_eq!(report.claims_checked, 5, "{:?}", report.drifted);
        assert_eq!(report.drifted.len(), 1, "{:?}", report.drifted);
        assert_eq!(report.drifted[0].claim, "../nothing.md");
    }

    #[test]
    fn relative_link_one_level_short_is_reported() {
        let fx = seeded();
        // `../src/db/mod.rs` from docs/sub resolves to docs/src/db/mod.rs, a
        // directory that never existed; the parent rule must not hide it.
        let report = check_at(
            &fx,
            "docs/sub/page.md",
            "[wrong](../src/db/mod.rs) and [right](../../src/db/mod.rs)\n",
        );
        assert_eq!(report.claims_checked, 2, "{:?}", report.drifted);
        assert_eq!(report.drifted.len(), 1, "{:?}", report.drifted);
        assert_eq!(report.drifted[0].claim, "../src/db/mod.rs");
    }

    #[test]
    fn fence_marker_inside_indented_code_opens_nothing() {
        let fx = seeded();
        let (checked, drifted) = check(
            &fx,
            "Example:\n\n    ```\n    code with `src/code.rs`\n\nProse again with `src/gone.rs`.\n",
        );
        assert_eq!(checked, 1, "{drifted:?}");
        assert_eq!(drifted.len(), 1);
        assert_eq!(drifted[0].claim, "src/gone.rs");
    }

    #[test]
    fn wide_gap_after_list_marker_caps_the_content_column() {
        let fx = seeded();
        // `-` plus six spaces: content column is 2, so six columns is code
        // and three columns is the item's continuation paragraph.
        let (checked, drifted) = check(
            &fx,
            "-      first line\n\n   `src/gone1.rs` continues the item\n\n      `src/code.rs` is code\n",
        );
        let claims: Vec<&str> = drifted.iter().map(|f| f.claim.as_str()).collect();
        assert_eq!(claims, vec!["src/gone1.rs"]);
        assert_eq!(checked, 1);
        assert_eq!(list_content_column("-      "), 2);
        assert_eq!(list_content_column("  - "), 4);
        assert_eq!(list_content_column("1.   "), 5);
    }

    #[test]
    #[cfg(unix)]
    fn symlinked_docs_dir_counts_as_internal() {
        let fx = seeded();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(
            outside.path().join("note.md"),
            "`docs/x.md` and `src/gone.rs`\n",
        )
        .unwrap();
        std::os::unix::fs::symlink(outside.path(), fx.dir.path().join("linked")).unwrap();
        // Canonicalizing away the operator's spelling would make this doc external.
        let selection = explicit_docs(&[fx.dir.path().join("linked/note.md")]);
        assert!(selection.not_markdown.is_empty() && selection.missing.is_empty());
        let report = check_documents(fx.dir.path(), &fx.db, &selection.docs, &[]).unwrap();
        assert_eq!(report.files, vec!["linked/note.md"]);
        assert_eq!(report.claims_checked, 2, "{:?}", report.drifted);
        assert_eq!(report.drifted.len(), 2);
    }

    #[test]
    fn explicit_docs_dedupe_and_survive_symlink_cycles() {
        let fx = seeded();
        let root = fx.dir.path();
        std::fs::create_dir_all(root.join("docs/a")).unwrap();
        std::fs::write(root.join("docs/a/one.md"), "`src/gone.rs`\n").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(root.join("docs/a"), root.join("docs/a/self")).unwrap();
        let selection = explicit_docs(&[
            root.join("docs/a"),
            root.join("docs/a/one.md"),
            root.join("docs/a/self/one.md"),
        ]);
        assert!(selection.not_markdown.is_empty() && selection.missing.is_empty());
        let docs = selection.docs;
        assert_eq!(docs.len(), 1, "{docs:?}");
        let report = check_documents(root, &fx.db, &docs, &[]).unwrap();
        assert_eq!(report.files.len(), 1);
        assert_eq!(report.drifted.len(), 1, "{:?}", report.drifted);
    }

    #[test]
    fn code_indented_past_a_list_items_content_column_is_not_a_claim() {
        let fx = seeded();
        let (checked, drifted) = check(
            &fx,
            "- item with `src/gone1.rs`\n\n      `src/code6.rs` at six columns is code\n\n        `src/code8.rs` at eight columns is code\n\n    `src/gone2.rs` at four columns is the item's paragraph\n  1. nested `src/gone3.rs`\n\n         `src/code9.rs` nine columns under a nested item is code\n",
        );
        let claims: Vec<&str> = drifted.iter().map(|f| f.claim.as_str()).collect();
        assert_eq!(claims, vec!["src/gone1.rs", "src/gone2.rs", "src/gone3.rs"]);
        assert_eq!(checked, 3);
    }

    #[test]
    fn gitignored_runtime_paths_are_not_reported() {
        let fx = seeded();
        let (checked, drifted) = check(
            &fx,
            "`.codesage/index.db`, `target/release/x.bin`, `src/gone.rs`.\n",
        );
        assert_eq!(checked, 1);
        assert_eq!(drifted.len(), 1, "{drifted:?}");
        assert_eq!(drifted[0].claim, "src/gone.rs");
    }

    #[test]
    fn broken_relative_link_is_reported_from_the_document_dir() {
        let fx = seeded();
        let report = check_at(
            &fx,
            "docs/sub/page.md",
            "Back to [the guide](../guide.md) or [old](./old-guide.md#top) or [src](../../src/lib.rs).\n",
        );
        assert_eq!(report.claims_checked, 3);
        assert_eq!(report.drifted.len(), 1, "{:?}", report.drifted);
        assert_eq!(report.drifted[0].claim, "./old-guide.md#top");
        assert_eq!(report.drifted[0].reason, "link target not found");
    }

    #[test]
    fn line_out_of_range_is_reported() {
        let fx = seeded();
        let (_, drifted) = check(&fx, "See src/db/mod.rs:40 for details.\n");
        assert_eq!(drifted.len(), 1);
        assert_eq!(drifted[0].class, ClaimClass::Anchor);
        assert_eq!(drifted[0].claim, "src/db/mod.rs:40");
        assert_eq!(drifted[0].reason, "line out of range (file has 2 lines)");
    }

    #[test]
    fn column_anchor_ignores_the_column() {
        let fx = seeded();
        let (checked, drifted) = check(&fx, "See src/db/mod.rs:2:900 for details.\n");
        assert_eq!(checked, 1);
        assert!(drifted.is_empty(), "{drifted:?}");
    }

    #[test]
    fn moved_symbol_is_reported_with_current_line() {
        let fx = seeded();
        let (_, drifted) = check(&fx, "`truncate_page` (`src/lib.rs:2`) cuts the page.\n");
        assert_eq!(drifted.len(), 1, "{drifted:?}");
        assert_eq!(drifted[0].class, ClaimClass::Anchor);
        assert_eq!(
            drifted[0].reason,
            "symbol `truncate_page` not at that line (now at L5)"
        );
        assert_eq!(drifted[0].suggestion.as_deref(), Some("src/lib.rs:5"));
    }

    #[test]
    fn anchor_with_unrelated_identifier_is_not_reported() {
        let fx = seeded();
        let (checked, drifted) = check(&fx, "`not_a_symbol` at `src/lib.rs:2`.\n");
        assert_eq!(checked, 1);
        assert!(drifted.is_empty(), "{drifted:?}");
    }

    #[test]
    fn dotted_file_name_is_not_a_symbol_claim() {
        let fx = seeded();
        let (checked, drifted) = check(&fx, "Bump `Cargo.toml` and `Database.open_in_memory`.\n");
        assert_eq!(checked, 1);
        assert!(drifted.is_empty(), "{drifted:?}");
    }

    #[test]
    fn unknown_member_of_indexed_owner_is_reported() {
        let fx = seeded();
        let (checked, drifted) = check(
            &fx,
            "Call `Database::open_read_only()` after `Database::open_in_memory()`.\n",
        );
        assert_eq!(checked, 2);
        assert_eq!(drifted.len(), 1);
        assert_eq!(drifted[0].class, ClaimClass::Symbol);
        assert_eq!(drifted[0].claim, "Database::open_read_only()");
        assert_eq!(
            drifted[0].reason,
            "unknown symbol: `Database` is indexed (src/lib.rs) but has no `open_read_only`"
        );
    }

    #[test]
    fn foreign_symbols_are_undecidable() {
        let fx = seeded();
        let (checked, drifted) = check(
            &fx,
            "`tokio::spawn()`, `Duration::from_millis`, `up()` / `down()`, `Type::method`, and `Regex::new` are foreign.\n",
        );
        assert_eq!(checked, 0, "{drifted:?}");
        assert!(drifted.is_empty(), "{drifted:?}");
    }

    #[test]
    fn struct_field_and_enum_variant_are_undecidable() {
        let fx = seeded();
        let (checked, drifted) = check(
            &fx,
            "`MapperContext.excludes` and `Verdict::Holds` cannot be checked; `Database.open_in_memory` can.\n",
        );
        assert_eq!(checked, 1, "{drifted:?}");
        assert!(drifted.is_empty(), "{drifted:?}");
    }

    #[test]
    fn constant_drift_normalises_separators() {
        let fx = seeded();
        let (checked, drifted) = check(
            &fx,
            "`MAX_CHUNK_CHARS` is 1,500,000. `BATCH_SIZE` defaults to 64.\n",
        );
        assert_eq!(checked, 2);
        assert_eq!(drifted.len(), 1, "{drifted:?}");
        assert_eq!(drifted[0].class, ClaimClass::Constant);
        assert_eq!(drifted[0].claim, "BATCH_SIZE = 64");
        assert_eq!(
            drifted[0].reason,
            "constant value drift: doc says 64, source says 32"
        );
        assert_eq!(drifted[0].suggestion, None);
    }

    #[test]
    fn unknown_constant_is_skipped() {
        let fx = seeded();
        let (checked, drifted) = check(&fx, "`CODESAGE_WATCH_IDLE_SECS` defaults to 300.\n");
        assert_eq!(checked, 0);
        assert!(drifted.is_empty());
    }

    #[test]
    fn skip_directive_and_exclude_patterns_skip_files() {
        let fx = seeded();
        let root = fx.dir.path();
        std::fs::write(
            root.join("docs/archive.md"),
            "<!-- codesage-docs: skip-file -->\n`src/gone.rs`\n",
        )
        .unwrap();
        std::fs::write(root.join("docs/sub/old.md"), "`src/gone.rs`\n").unwrap();
        std::fs::write(root.join("docs/live.md"), "`src/gone.rs`\n").unwrap();
        let docs = vec![
            root.join("docs/archive.md"),
            root.join("docs/sub/old.md"),
            root.join("docs/live.md"),
        ];
        let report = check_documents(root, &fx.db, &docs, &["docs/sub/**".to_string()]).unwrap();
        assert_eq!(report.files, vec!["docs/live.md"]);
        assert_eq!(
            report.files_skipped,
            vec![
                SkippedFile {
                    path: "docs/archive.md".into(),
                    reason: SkipReason::Directive
                },
                SkippedFile {
                    path: "docs/sub/old.md".into(),
                    reason: SkipReason::Excluded
                },
            ]
        );
        assert_eq!(report.drifted.len(), 1);
    }

    #[test]
    fn directive_mentioned_in_code_does_not_skip_the_file() {
        let fx = seeded();
        let report = check_at(
            &fx,
            "docs/about.md",
            "Add `<!-- codesage-docs: skip-file -->` to opt out, or in a fence:\n\n```markdown\n<!-- codesage-docs: skip-file -->\n```\n\n  <!-- codesage-docs: skip-file --> trailing prose keeps it a mention too.\n`src/gone.rs`\n",
        );
        assert_eq!(report.files, vec!["docs/about.md"]);
        assert!(
            report.files_skipped.is_empty(),
            "{:?}",
            report.files_skipped
        );
        assert_eq!(report.drifted.len(), 1);

        let report = check_at(
            &fx,
            "docs/bare.md",
            "# Old\n\n   <!--codesage-docs:skip-file-->\n`src/gone.rs`\n",
        );
        assert!(report.files.is_empty());
        assert_eq!(report.files_skipped[0].reason, SkipReason::Directive);
        assert!(report.drifted.is_empty());
    }

    #[test]
    fn unreadable_file_is_recorded_not_fatal() {
        let fx = seeded();
        let root = fx.dir.path();
        std::fs::write(root.join("docs/bad.md"), b"# bad \xff\xfe utf8\n").unwrap();
        std::fs::write(root.join("docs/good.md"), "`src/gone.rs`\n").unwrap();
        let docs = vec![root.join("docs/bad.md"), root.join("docs/good.md")];
        let report = check_documents(root, &fx.db, &docs, &[]).unwrap();
        assert_eq!(report.files, vec!["docs/good.md"]);
        assert_eq!(report.files_failed.len(), 1);
        assert_eq!(report.files_failed[0].path, "docs/bad.md");
        assert!(
            report.files_failed[0].error.contains("UTF-8")
                || report.files_failed[0].error.contains("utf-8"),
            "{}",
            report.files_failed[0].error
        );
        assert_eq!(report.drifted.len(), 1);
    }

    #[test]
    fn list_continuation_paragraphs_are_prose() {
        let fx = seeded();
        let (checked, drifted) = check(
            &fx,
            "- First item mentions `src/gone1.rs`.\n\n    Continuation with `src/gone2.rs` and `src/gone3.rs`.\n\n    Still the item: `src/gone4.rs`.\n- Second item `src/gone5.rs`\n\n  Two-space continuation `src/gone6.rs`.\n\nBack to prose.\n\n    `src/code.rs` is real indented code now\n",
        );
        assert_eq!(checked, 6, "{drifted:?}");
        let claims: Vec<&str> = drifted.iter().map(|f| f.claim.as_str()).collect();
        assert_eq!(
            claims,
            vec![
                "src/gone1.rs",
                "src/gone2.rs",
                "src/gone3.rs",
                "src/gone4.rs",
                "src/gone5.rs",
                "src/gone6.rs"
            ]
        );
    }

    #[test]
    fn link_targets_accept_dirs_percent_encoding_and_query_strings() {
        let fx = seeded();
        std::fs::write(fx.dir.path().join("docs/guide two.md"), "# g\n").unwrap();
        let report = check_at(
            &fx,
            "docs/sub/page.md",
            "[dir](../sub) [enc](../guide%20two.md) [q](../guide.md?v=2#top) [missing](../guide%20three.md)\n",
        );
        assert_eq!(report.claims_checked, 4);
        assert_eq!(report.drifted.len(), 1, "{:?}", report.drifted);
        assert_eq!(report.drifted[0].claim, "../guide%20three.md");
    }

    #[test]
    fn anchor_outside_any_symbol_is_undecidable() {
        let fx = seeded();
        // Line 11 closes impl Database; no indexed symbol spans it.
        let (checked, drifted) = check(&fx, "`truncate_page` is called near `src/lib.rs:11`.\n");
        assert_eq!(checked, 0, "{drifted:?}");
        assert!(drifted.is_empty(), "{drifted:?}");
    }

    #[test]
    fn struct_fields_via_path_are_undecidable_but_missing_methods_report() {
        let fx = seeded();
        let (checked, drifted) = check(
            &fx,
            "`MapperContext::excludes` and `Database::connection` are fields; `Database::gone_method()` is not.\n",
        );
        assert_eq!(checked, 1, "{drifted:?}");
        assert_eq!(drifted.len(), 1, "{drifted:?}");
        assert_eq!(drifted[0].claim, "Database::gone_method()");
    }

    #[test]
    fn constant_homonym_owner_is_not_evidence() {
        let fx = seeded();
        let (checked, drifted) = check(&fx, "Wrap it in `BATCH_SIZE::transaction()`.\n");
        assert_eq!(checked, 0, "{drifted:?}");
        assert!(drifted.is_empty(), "{drifted:?}");
    }

    #[test]
    fn owner_tail_fallback_confirms_but_never_reports() {
        let fx = seeded();
        let (checked, drifted) = check(
            &fx,
            "`graph::search::a` holds via the tail; `graph::search::nonexistent_fn` is undecidable; `search::nonexistent_fn` drifts.\n",
        );
        assert_eq!(checked, 2, "{drifted:?}");
        assert_eq!(drifted.len(), 1, "{drifted:?}");
        assert_eq!(drifted[0].claim, "search::nonexistent_fn");
        assert_eq!(
            drifted[0].reason,
            "unknown symbol: `search` is indexed (src/db/mod.rs) but has no `nonexistent_fn`"
        );
    }

    #[test]
    #[cfg(unix)]
    fn default_docs_follow_the_claude_symlink_and_skip_changelog() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("AGENTS.md"), "# a\n").unwrap();
        std::os::unix::fs::symlink("AGENTS.md", root.join("CLAUDE.md")).unwrap();
        std::fs::write(root.join("README.md"), "# r\n").unwrap();
        std::fs::write(root.join("CHANGELOG.md"), "# c\n").unwrap();
        std::fs::create_dir_all(root.join("docs/sub")).unwrap();
        std::fs::write(root.join("docs/sub/guide.md"), "# g\n").unwrap();
        std::fs::write(root.join("docs/CHANGELOG.md"), "# c\n").unwrap();
        let docs: Vec<String> = default_docs(root)
            .docs
            .iter()
            .map(|p| display_doc_path(root, p))
            .collect();
        assert_eq!(docs, vec!["AGENTS.md", "README.md", "docs/sub/guide.md"]);
    }
}
