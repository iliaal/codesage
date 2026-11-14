//! Definitions a patch removes that indexed files outside the patch still reference.
//!
//! The patch is the working tree against Git HEAD. A definition is removed
//! when a patch file defined its qualified name at HEAD and its working-tree
//! version does not. A reference from a file outside the patch counts when,
//! with the patch files shadowed at HEAD, it resolves to the removed
//! definition, and with them shadowed at the working tree, it resolves to
//! nothing. Moves, re-exports, and surviving homonyms are whatever the second
//! resolution still finds. Both views are request-local overlays on separate
//! read-only connections; the index itself is never written.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use codesage_features::trust_boundary::JavaTypeContext;
use codesage_parser::detect::detect_language;
use codesage_parser::discover::{MAX_INDEXABLE_FILE_BYTES, content_hash};
use codesage_parser::source::read_indexable_file;
use codesage_protocol::{
    FileInfo, Language, Reference, ReviewObjection, ReviewSeverity, Symbol, Visibility,
};
use codesage_storage::Database;

use crate::drift::{git_output, git_output_with_stdin};
use crate::impact::{WalkCache, symbol_identity_key};
use crate::index::{ParsedFile, parse_source};

pub(crate) const CATEGORY: &str = "removed-symbol-callers";
/// Patch files examined, in path order; later ones go unchecked.
const PATCH_FILE_CAP: usize = 50;
/// Removed definitions whose callers are resolved, in file and line order.
const REMOVED_DEFINITION_CAP: usize = 50;
/// Reference rows resolved across all removed definitions.
const ROW_BUDGET: usize = 20_000;
/// Caller files named per removed definition before the rest collapse to a count.
const CALLER_NOTE_CAP: usize = 5;
/// Caller files listed in the objection's `files`.
const FILES_CAP: usize = 20;
/// Paths named in a coverage note before the rest collapse to a count.
const NOTE_PATH_CAP: usize = 5;
/// Wall-clock backstops; the caps and the row budget are the intended bounds.
const GIT_DEADLINE: Duration = Duration::from_secs(10);
const CALLER_DEADLINE: Duration = Duration::from_secs(15);

pub(crate) struct RemovedDefinitionCheck {
    pub objection: Option<ReviewObjection>,
    /// Coverage gaps, reported in the summary when no objection carries them.
    pub notes: Vec<String>,
}

struct PatchFile {
    path: String,
    language: Language,
    indexed_hash: Option<String>,
    /// `None` when the working tree no longer holds the file.
    working: Option<Vec<u8>>,
    head_oid: Option<String>,
    head: Option<Vec<u8>>,
    unchanged: bool,
}

struct Removed {
    symbol: Symbol,
    callers: BTreeSet<String>,
    name_only: BTreeSet<String>,
}

#[derive(Default)]
struct Gaps {
    unresolved: Vec<String>,
    unchecked: Vec<String>,
    unreadable: Vec<String>,
    degraded: Vec<String>,
    unverified_globs: Vec<String>,
    unbounded_globs: Vec<String>,
    git_deadline: bool,
}

pub(crate) fn check(
    root: &Path,
    db: &Database,
    files: &[String],
) -> Result<RemovedDefinitionCheck> {
    let mut gaps = Gaps::default();
    let cwd = std::env::current_dir().ok();
    let mut paths = BTreeSet::new();
    for file in files {
        match normalize(root, cwd.as_deref(), file) {
            Some(path) => {
                paths.insert(path);
            }
            None => gaps.unresolved.push(file.clone()),
        }
    }
    let patch: BTreeSet<String> = paths.clone();
    let mut considered = Vec::new();
    for path in paths {
        let indexed = db.indexed_file_info(&path)?;
        let Some(language) = indexed
            .as_ref()
            .map(|info| info.language)
            .or_else(|| detect_language(Path::new(&path)))
        else {
            continue;
        };
        if considered.len() == PATCH_FILE_CAP {
            gaps.unchecked.push(path);
            continue;
        }
        considered.push(PatchFile {
            path,
            language,
            indexed_hash: indexed.map(|info| info.content_hash),
            working: None,
            head_oid: None,
            head: None,
            unchanged: false,
        });
    }
    if considered.is_empty() {
        return Ok(gaps.finish(None));
    }

    let git_deadline = Instant::now() + GIT_DEADLINE;
    // No repository or an unborn HEAD: nothing can have been removed.
    if git_output(
        root,
        &["rev-parse", "--verify", "--quiet", "HEAD^{commit}"],
        git_deadline,
    )
    .is_none()
    {
        return Ok(gaps.finish(None));
    }
    let Some(head_oids) = head_blobs(root, &considered, git_deadline) else {
        gaps.git_deadline = Instant::now() >= git_deadline;
        return Ok(gaps.finish(Some(
            "Removed-definition check did not run: Git HEAD could not be listed".to_string(),
        )));
    };

    let mut readable = Vec::new();
    for mut file in considered {
        codesage_protocol::work::checkpoint()?;
        match read_indexable_file(root, Path::new(&file.path)) {
            Ok(Some(snapshot)) => file.working = Some(snapshot.bytes),
            Err(error)
                if error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) => {}
            Ok(None) | Err(_) => {
                gaps.unreadable.push(file.path);
                continue;
            }
        }
        file.head_oid = head_oids.get(&file.path).cloned();
        readable.push(file);
    }
    mark_unchanged(root, &mut readable, git_deadline);
    let mut patch_files = Vec::new();
    for mut file in readable {
        if !file.unchanged
            && let Some(oid) = &file.head_oid
        {
            match git_output(root, &["cat-file", "blob", oid], git_deadline) {
                Some(bytes) => {
                    file.unchanged = file.working.as_deref() == Some(bytes.as_slice());
                    file.head = Some(bytes);
                }
                None => {
                    gaps.git_deadline |= Instant::now() >= git_deadline;
                    gaps.unreadable.push(file.path);
                    continue;
                }
            }
        }
        patch_files.push(file);
    }
    if !patch_files.is_empty() && patch_files.iter().all(|file| file.unchanged) {
        return Ok(gaps.finish(Some(
            "Removed-definition check compares the working tree with Git HEAD, and every examined patch file matches HEAD (for example, the change is already committed)"
                .to_string(),
        )));
    }

    // Parse each version once; `pre` and `post` name the view a parse shadows.
    let mut pre: Vec<(String, Option<ParsedFile>, String)> = Vec::new();
    let mut post: Vec<(String, Option<ParsedFile>, String)> = Vec::new();
    let mut removed_names: BTreeMap<String, HashSet<String>> = BTreeMap::new();
    let mut reexported: HashMap<String, Reexports> = HashMap::new();
    for file in &patch_files {
        codesage_protocol::work::checkpoint()?;
        let working = match &file.working {
            Some(bytes) => match parse(&file.path, file.language, bytes) {
                Some(parsed) => Some((parsed, bytes)),
                None => {
                    gaps.unreadable.push(file.path.clone());
                    continue;
                }
            },
            None => None,
        };
        let head = if file.unchanged {
            None
        } else {
            match &file.head {
                Some(bytes) => match parse(&file.path, file.language, bytes) {
                    Some(parsed) => Some((parsed, bytes)),
                    None => {
                        gaps.unreadable.push(file.path.clone());
                        continue;
                    }
                },
                None => None,
            }
        };
        if !file.unchanged
            && let Some((head, _)) = &head
        {
            if working.as_ref().is_some_and(|(parsed, _)| parsed.degraded) {
                gaps.degraded.push(file.path.clone());
            } else {
                let after: HashSet<&str> = working
                    .iter()
                    .flat_map(|(parsed, _)| parsed.symbols.iter())
                    .map(|s| s.qualified_name.as_str())
                    .collect();
                let reexports = working
                    .as_ref()
                    .map(|(parsed, bytes)| reexports(parsed.info.language, bytes))
                    .unwrap_or_default();
                let mut before = head.symbols.clone();
                crate::resolver::retain_definitions(&mut before, false);
                let names: HashSet<String> = before
                    .into_iter()
                    .map(|s| s.qualified_name)
                    .filter(|name| {
                        !after.contains(name.as_str()) && !reexports.names.contains(name)
                    })
                    .collect();
                if !names.is_empty() {
                    removed_names.insert(file.path.clone(), names);
                    reexported.insert(file.path.clone(), reexports);
                }
            }
        }
        let post_hash = file.working.as_deref().map(content_hash);
        if file.indexed_hash != post_hash {
            let (parsed, source) = owned(working.as_ref());
            post.push((file.path.clone(), parsed, source));
        }
        let (pre_bytes, pre_parsed) = if file.unchanged {
            (file.working.as_deref(), working.as_ref())
        } else {
            (file.head.as_deref(), head.as_ref())
        };
        // A changed file is shadowed even when the index already holds its
        // HEAD rows: resolvers that reread source (Rust cross-crate surfaces)
        // must see HEAD bytes, not the working tree.
        if !file.unchanged || file.indexed_hash != pre_bytes.map(content_hash) {
            let (parsed, source) = owned(pre_parsed);
            pre.push((file.path.clone(), parsed, source));
        }
    }
    if removed_names.is_empty() {
        return Ok(gaps.finish(None));
    }

    let pre_view = match View::open(db, &pre) {
        Ok(view) => view,
        Err(error) => return Ok(gaps.unavailable(&error)),
    };
    let post_view = match View::open(db, &post) {
        Ok(view) => view,
        Err(error) => return Ok(gaps.unavailable(&error)),
    };
    let before = pre_view.db(db);
    let after = post_view.db(db);
    let _before_snapshot = pre_view.snapshot()?;
    let _after_snapshot = post_view.snapshot()?;
    if let Err(error) = pre_view
        .install(&pre)
        .and_then(|()| post_view.install(&post))
    {
        codesage_protocol::work::checkpoint()?;
        return Ok(gaps.unavailable(&error));
    }

    // Opened after installation: the memo is keyed on the connection's
    // change count, which the overlay's TEMP writes advance.
    let _before_memo = before.python_memo_scope();
    let _after_memo = after.python_memo_scope();
    let working_bytes: HashMap<&str, &[u8]> = patch_files
        .iter()
        .filter_map(|file| Some((file.path.as_str(), file.working.as_deref()?)))
        .collect();
    keep_reexported(
        root,
        after,
        &working_bytes,
        &mut removed_names,
        &reexported,
        &mut gaps,
    )?;
    if removed_names.is_empty() {
        return Ok(gaps.finish(None));
    }
    let mut candidates: Vec<Symbol> = Vec::new();
    for (path, names) in &removed_names {
        let mut symbols = before.symbols_for_file(path)?;
        crate::resolver::retain_definitions(&mut symbols, false);
        candidates.extend(
            symbols
                .into_iter()
                .filter(|s| names.contains(&s.qualified_name)),
        );
    }
    candidates.sort_by(|a, b| {
        (&a.file_path, a.line_start, a.col_start, &a.qualified_name).cmp(&(
            &b.file_path,
            b.line_start,
            b.col_start,
            &b.qualified_name,
        ))
    });

    let deadline = Instant::now() + CALLER_DEADLINE;
    let mut before_cache = WalkCache::default();
    let mut after_cache = WalkCache::default();
    let mut caches = CallerCaches::default();
    let mut removed: Vec<Removed> = Vec::new();
    let mut rows_left = ROW_BUDGET;
    let mut examined = 0usize;
    let mut stop: Option<&str> = None;
    'symbols: for symbol in candidates.iter().take(REMOVED_DEFINITION_CAP) {
        examined += 1;
        let mut entry = Removed {
            symbol: symbol.clone(),
            callers: BTreeSet::new(),
            name_only: BTreeSet::new(),
        };
        let mut evidenced: BTreeSet<String> = BTreeSet::new();
        let mut rows = crate::construction::references_for_symbol(before, symbol, &mut || {
            before_cache.rust_modules(before)
        })?;
        // The row budget cuts in a fixed order, not in SQLite's scan order.
        rows.sort_by(|a, b| (&a.from_file, a.line, a.col).cmp(&(&b.from_file, b.line, b.col)));
        for row in rows {
            codesage_protocol::work::checkpoint()?;
            if patch.contains(&row.from_file) || evidenced.contains(&row.from_file) {
                continue;
            }
            if rows_left == 0 {
                stop = Some("the row budget");
            } else if Instant::now() >= deadline {
                stop = Some("deadline reached");
            }
            if stop.is_some() {
                entry.name_only.retain(|file| !evidenced.contains(file));
                if !entry.callers.is_empty() {
                    removed.push(entry);
                }
                break 'symbols;
            }
            rows_left -= 1;
            let Some(has_evidence) = broken_caller(
                root,
                &row,
                symbol,
                (before, &mut before_cache),
                (after, &mut after_cache),
                &mut caches,
            )?
            else {
                continue;
            };
            entry.callers.insert(row.from_file.clone());
            if has_evidence {
                evidenced.insert(row.from_file.clone());
            } else {
                entry.name_only.insert(row.from_file.clone());
            }
        }
        entry.name_only.retain(|file| !evidenced.contains(file));
        if !entry.callers.is_empty() {
            removed.push(entry);
        }
    }

    let mut notes = gaps.notes();
    if let Some(cause) = stop {
        notes.push(format!(
            "Removed-definition callers resolved for {} of {} removed definition(s): {cause} stopped the rest; counts are lower bounds",
            examined.saturating_sub(1),
            candidates.len()
        ));
    } else if candidates.len() > examined {
        notes.push(format!(
            "Removed-definition callers resolved for {examined} of {} removed definition(s): the {REMOVED_DEFINITION_CAP}-definition cap stopped the rest",
            candidates.len()
        ));
    }
    if removed.is_empty() {
        return Ok(RemovedDefinitionCheck {
            objection: None,
            notes,
        });
    }
    Ok(RemovedDefinitionCheck {
        objection: Some(objection(&removed, notes)),
        notes: Vec::new(),
    })
}

/// Whether `row` named the removed definition before the patch and names
/// nothing after it, in a caller file that still spells the name; the flag
/// says whether it held the definition by import, qualified, or same-file
/// evidence. An evidenced row survives on a target it already resolved to,
/// a target in a file the caller imports, or any Python target (Python
/// resolution follows each binding, re-exports included). A row held by
/// name alone survives on any target.
fn broken_caller(
    root: &Path,
    row: &Reference,
    symbol: &Symbol,
    before: (&Database, &mut WalkCache),
    after: (&Database, &mut WalkCache),
    caches: &mut CallerCaches,
) -> Result<Option<bool>> {
    let Some(targets) = before.1.reference_targets(before.0, row)? else {
        return Ok(None);
    };
    if !targets.contains(&symbol_identity_key(symbol))
        || !still_spelled(root, row, &mut caches.sources)
    {
        return Ok(None);
    }
    let evidenced = before.1.binding_evidence(before.0, row, symbol)?;
    let Some(survivors) = after.1.reference_target_symbols(after.0, row)? else {
        return Ok(None);
    };
    if survivors.is_empty() {
        return Ok(Some(evidenced));
    }
    if !evidenced || codesage_protocol::python::is_python_path(&row.from_file) {
        return Ok(None);
    }
    let imports = imported_files(&mut caches.after_imports, after.0, &row.from_file)?;
    let kept = survivors.iter().any(|survivor| {
        targets.contains(&symbol_identity_key(survivor)) || imports.contains(&survivor.file_path)
    });
    Ok((!kept).then_some(evidenced))
}

#[derive(Default)]
struct CallerCaches {
    sources: HashMap<String, Option<String>>,
    after_imports: HashMap<String, HashSet<String>>,
}

/// Files `caller` imports in `db`'s view.
fn imported_files<'a>(
    cache: &'a mut HashMap<String, HashSet<String>>,
    db: &Database,
    caller: &str,
) -> Result<&'a HashSet<String>> {
    if !cache.contains_key(caller) {
        let files = db.import_targets_for_file(caller)?.into_iter().collect();
        cache.insert(caller.to_string(), files);
    }
    Ok(&cache[caller])
}

fn owned(parsed: Option<&(ParsedFile, &Vec<u8>)>) -> (Option<ParsedFile>, String) {
    match parsed {
        Some((parsed, bytes)) => (
            Some(parsed.clone()),
            String::from_utf8_lossy(bytes).into_owned(),
        ),
        None => (None, String::new()),
    }
}

/// One side of the comparison: the caller's connection when no patch file
/// needs shadowing and it carries no overlay, else a fresh read-only one.
enum View {
    Shared,
    Separate(Box<Database>),
}

impl View {
    fn open(db: &Database, shadows: &[(String, Option<ParsedFile>, String)]) -> Result<Self> {
        if shadows.is_empty() && !db.has_structural_overlay()? {
            return Ok(Self::Shared);
        }
        let path = db.path().context("the index has no file to reopen")?;
        Ok(Self::Separate(Box::new(Database::open_read_only(path)?)))
    }

    fn db<'a>(&'a self, shared: &'a Database) -> &'a Database {
        match self {
            Self::Shared => shared,
            Self::Separate(db) => db,
        }
    }

    fn snapshot(&self) -> Result<Option<codesage_storage::db::ReadSnapshot<'_>>> {
        match self {
            Self::Shared => Ok(None),
            Self::Separate(db) => Ok(Some(db.read_snapshot()?)),
        }
    }

    /// Shadow each path with its parse; a path without one reads as absent.
    fn install(&self, shadows: &[(String, Option<ParsedFile>, String)]) -> Result<()> {
        let Self::Separate(db) = self else {
            return Ok(());
        };
        if shadows.is_empty() {
            return Ok(());
        }
        let paths: Vec<String> = shadows.iter().map(|(path, _, _)| path.clone()).collect();
        let parsed: Vec<_> = shadows
            .iter()
            .filter_map(|(_, parsed, source)| parsed.as_ref().map(|p| (p, source.as_str())))
            .collect();
        crate::overlay::install_parsed_overlay(db, &paths, &parsed)
    }
}

fn objection(removed: &[Removed], gaps: Vec<String>) -> ReviewObjection {
    let caller_files: BTreeSet<&str> = removed
        .iter()
        .flat_map(|r| r.callers.iter().map(String::as_str))
        .collect();
    let name_only = removed.iter().all(|r| r.name_only.len() == r.callers.len());
    let mut evidence = Vec::new();
    for entry in removed {
        let callers: Vec<&str> = entry.callers.iter().map(String::as_str).collect();
        evidence.push(format!(
            "{}: {} ({}, line {}) removed; {} referencing file(s) outside the patch: {}",
            entry.symbol.file_path,
            entry.symbol.qualified_name,
            entry.symbol.kind.as_str(),
            entry.symbol.line_start,
            callers.len(),
            sample(&callers, CALLER_NOTE_CAP)
        ));
        if !entry.name_only.is_empty() {
            let files: Vec<&str> = entry.name_only.iter().map(String::as_str).collect();
            evidence.push(format!(
                "  {} of them bind {} by name alone (no import, qualified, or same-file evidence): {}",
                files.len(),
                entry.symbol.name,
                sample(&files, CALLER_NOTE_CAP)
            ));
        }
    }
    evidence.push(
        "Counts are floors: references come from name-based extraction, so dynamic dispatch, reflection, and generated call sites leave no row."
            .to_string(),
    );
    if caller_files.len() > FILES_CAP {
        evidence.push(format!(
            "files lists the first {FILES_CAP} of {} caller file(s)",
            caller_files.len()
        ));
    }
    evidence.extend(gaps);
    ReviewObjection {
        severity: if name_only {
            ReviewSeverity::Medium
        } else {
            ReviewSeverity::High
        },
        category: CATEGORY.to_string(),
        title: format!(
            "{} removed definition(s) still referenced by {} indexed file(s) outside the patch{}",
            removed.len(),
            caller_files.len(),
            if name_only {
                "; every reference binds by name alone"
            } else {
                ""
            }
        ),
        evidence,
        files: caller_files
            .into_iter()
            .take(FILES_CAP)
            .map(str::to_string)
            .collect(),
        counts_floor: true,
    }
}

impl Gaps {
    fn notes(&self) -> Vec<String> {
        let mut notes = Vec::new();
        let mut push = |what: &str, paths: &[String]| {
            if !paths.is_empty() {
                let paths: Vec<&str> = paths.iter().map(String::as_str).collect();
                notes.push(format!(
                    "Removed-definition check {what}: {}",
                    sample(&paths, NOTE_PATH_CAP)
                ));
            }
        };
        push("could not place inside the project", &self.unresolved);
        push(
            &format!("skipped patch files past the {PATCH_FILE_CAP}-file cap"),
            &self.unchecked,
        );
        push("could not read or parse", &self.unreadable);
        push(
            "skipped files whose working-tree parse is degraded (syntax errors)",
            &self.degraded,
        );
        push(
            "could not resolve the source of a glob re-export, so its removals are kept unverified",
            &self.unverified_globs,
        );
        push(
            "kept removals under a glob re-export chain that loops or exceeds four hops",
            &self.unbounded_globs,
        );
        if self.git_deadline {
            notes.push("Removed-definition check: Git deadline reached".to_string());
        }
        notes
    }

    fn finish(self, extra: Option<String>) -> RemovedDefinitionCheck {
        let mut notes = self.notes();
        notes.extend(extra);
        RemovedDefinitionCheck {
            objection: None,
            notes,
        }
    }

    fn unavailable(self, error: &anyhow::Error) -> RemovedDefinitionCheck {
        self.finish(Some(format!(
            "Removed-definition check did not run: no pre- and post-patch view of the index: {error:#}"
        )))
    }
}

fn sample(items: &[&str], cap: usize) -> String {
    let shown = items[..items.len().min(cap)].join(", ");
    match items.len().saturating_sub(cap) {
        0 => shown,
        more => format!("{shown} (+{more} more)"),
    }
}

/// Project-relative spelling of `path`: absolute paths must lie under
/// `root`; a relative path holding `..` is taken from the current directory.
fn normalize(root: &Path, cwd: Option<&Path>, path: &str) -> Option<String> {
    let raw = Path::new(path);
    let joined: PathBuf = if raw.is_absolute() {
        raw.to_path_buf()
    } else if raw.components().any(|c| c == Component::ParentDir) {
        cwd?.join(raw)
    } else {
        root.join(raw)
    };
    let lexical = lexical(&joined)?;
    let relative = match lexical.strip_prefix(lexical_root(root)?) {
        Ok(relative) => relative.to_path_buf(),
        Err(_) => lexical
            .strip_prefix(root.canonicalize().ok()?)
            .ok()?
            .to_path_buf(),
    };
    let parts: Vec<&str> = relative
        .components()
        .map(|c| match c {
            Component::Normal(part) => part.to_str(),
            _ => None,
        })
        .collect::<Option<_>>()?;
    (!parts.is_empty()).then(|| parts.join("/"))
}

fn lexical_root(root: &Path) -> Option<PathBuf> {
    lexical(root)
}

fn lexical(path: &Path) -> Option<PathBuf> {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    return None;
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    Some(out)
}

#[derive(Default)]
struct Reexports {
    /// Exported spellings of explicit re-exports (`pub use a::Foo`, `export { x }`).
    names: HashSet<String>,
    /// Import text of each glob re-export (`crate::util::*`, `./util.js`).
    globs: Vec<String>,
}

/// Drop removals the working tree still exports from elsewhere: a member of
/// an explicitly re-exported owner (`Foo::new` under `pub use b::Foo`) when
/// the working-tree view still defines that qualified name, and anything a
/// glob re-export's source file exports in that view, following the
/// source's own re-exports up to [`GLOB_DEPTH`] hops. A glob whose chain
/// cannot be resolved within that depth keeps its file's removals and is
/// disclosed.
fn keep_reexported(
    root: &Path,
    after: &Database,
    working: &HashMap<&str, &[u8]>,
    removed_names: &mut BTreeMap<String, HashSet<String>>,
    reexported: &HashMap<String, Reexports>,
    gaps: &mut Gaps,
) -> Result<()> {
    let mut globs = GlobResolver {
        root,
        after,
        working,
        files: None,
        modules: None,
    };
    for (path, names) in removed_names.iter_mut() {
        let Some(exports) = reexported.get(path) else {
            continue;
        };
        let mut kept = Vec::new();
        for name in names.iter() {
            let owner = owner(name);
            if owner != name
                && exports.names.contains(owner)
                && after
                    .find_symbols(name, None)?
                    .iter()
                    .any(|s| s.qualified_name == *name)
            {
                kept.push(name.clone());
            }
        }
        for glob in &exports.globs {
            let mut visited = HashSet::from([path.clone()]);
            match globs.exported_through(path, glob, 1, &mut visited)? {
                Ok(exported) => kept.extend(
                    names
                        .iter()
                        .filter(|name| exported.contains(*name))
                        .cloned(),
                ),
                Err(GlobGap::Unresolved) => gaps.unverified_globs.push(format!("{path}: {glob}")),
                Err(GlobGap::Chain) => gaps.unbounded_globs.push(format!("{path}: {glob}")),
            }
        }
        for name in kept {
            names.remove(&name);
        }
    }
    removed_names.retain(|_, names| !names.is_empty());
    Ok(())
}

/// Glob hops followed from a patch file before a chain counts as unverified.
const GLOB_DEPTH: usize = 4;

/// Why a glob re-export's exports could not be established.
enum GlobGap {
    /// A hop's source file is not resolvable or not in a supported language.
    Unresolved,
    /// The chain revisits a file or runs past [`GLOB_DEPTH`] hops.
    Chain,
}

struct GlobResolver<'a> {
    root: &'a Path,
    after: &'a Database,
    /// Working-tree bytes of the patch files; other files are read from disk.
    working: &'a HashMap<&'a str, &'a [u8]>,
    files: Option<Vec<String>>,
    modules: Option<crate::rust_modules::RustModules>,
}

impl GlobResolver<'_> {
    /// Qualified names `glob` in `importer` exports, or why they cannot be
    /// established.
    fn exported_through(
        &mut self,
        importer: &str,
        glob: &str,
        depth: usize,
        visited: &mut HashSet<String>,
    ) -> Result<std::result::Result<HashSet<String>, GlobGap>> {
        codesage_protocol::work::checkpoint()?;
        if depth > GLOB_DEPTH {
            return Ok(Err(GlobGap::Chain));
        }
        let mut exported = HashSet::new();
        let mut found = false;
        for source in self.sources(importer, glob)? {
            // Module candidates name every layout (`b.rs`, `b/mod.rs`);
            // only the ones the working tree holds are the glob's source.
            let Some(bytes) = self.source_bytes(&source)? else {
                continue;
            };
            if !visited.insert(source.clone()) {
                return Ok(Err(GlobGap::Chain));
            }
            match self.exported(&source, &bytes, depth, visited)? {
                Ok(names) => exported.extend(names),
                Err(gap) => return Ok(Err(gap)),
            }
            found = true;
        }
        Ok(if found {
            Ok(exported)
        } else {
            Err(GlobGap::Unresolved)
        })
    }

    fn source_bytes(&self, file: &str) -> Result<Option<Vec<u8>>> {
        if let Some(bytes) = self.working.get(file) {
            return Ok(Some(bytes.to_vec()));
        }
        Ok(match self.after.overlay_source(file)? {
            Some(source) => source.map(String::into_bytes),
            None => read_indexable_file(self.root, Path::new(file))
                .ok()
                .flatten()
                .map(|snapshot| snapshot.bytes),
        })
    }

    /// Qualified names `file` exports in the working-tree view: its own
    /// exported definitions (Rust `pub` or `pub(crate)`; JavaScript
    /// `export` declarations) with their members, its explicit re-exports,
    /// and whatever its globs export.
    fn exported(
        &mut self,
        file: &str,
        bytes: &[u8],
        depth: usize,
        visited: &mut HashSet<String>,
    ) -> Result<std::result::Result<HashSet<String>, GlobGap>> {
        let Some(language) = detect_language(Path::new(file)) else {
            return Ok(Err(GlobGap::Unresolved));
        };
        let symbols = self.after.symbols_for_file(file)?;
        let own = reexports(language, bytes);
        let mut exported: HashSet<String> = own.names.clone();
        match language {
            Language::Rust => exported.extend(
                symbols
                    .iter()
                    .filter(|s| {
                        matches!(
                            s.visibility,
                            Some(Visibility::Public | Visibility::Crate) | None
                        )
                    })
                    .map(|s| s.qualified_name.clone()),
            ),
            _ => {
                let declared = js_exported_declarations(language, bytes);
                exported.extend(
                    symbols
                        .iter()
                        .filter(|s| declared.contains(owner(&s.qualified_name)))
                        .map(|s| s.qualified_name.clone()),
                );
            }
        }
        for glob in &own.globs {
            match self.exported_through(file, glob, depth + 1, visited)? {
                Ok(names) => exported.extend(names),
                Err(gap) => return Ok(Err(gap)),
            }
        }
        Ok(Ok(exported))
    }

    fn sources(&mut self, importer: &str, glob: &str) -> Result<Vec<String>> {
        if importer.ends_with(".rs") {
            let modules = match &mut self.modules {
                Some(modules) => modules,
                None => self
                    .modules
                    .insert(crate::rust_modules::RustModules::load(self.after)?),
            };
            return Ok(modules
                .candidates(importer, glob.strip_suffix("::*").unwrap_or(glob))
                .unwrap_or_default());
        }
        let files = match &mut self.files {
            Some(files) => files,
            None => {
                let mut all: Vec<String> = self.after.all_file_hashes()?.into_keys().collect();
                all.extend(self.working.keys().map(|p| p.to_string()));
                all.sort();
                all.dedup();
                self.files.insert(all)
            }
        };
        Ok(files
            .iter()
            .filter(|candidate| {
                candidate.as_str() != importer
                    && crate::bundle::import_ref_targets_file(glob, importer, candidate)
            })
            .cloned()
            .collect())
    }
}

/// Names a JavaScript/TypeScript file declares with `export` (`export
/// function`, `export class`, `export const`, `export default function`).
fn js_exported_declarations(language: Language, bytes: &[u8]) -> HashSet<String> {
    let mut out = HashSet::new();
    if !matches!(language, Language::JavaScript | Language::TypeScript) {
        return out;
    }
    let Ok(parsed) = codesage_parser::parse::parse_file_tolerant(bytes, language) else {
        return out;
    };
    let root = parsed.tree.root_node();
    let mut cursor = root.walk();
    for statement in root
        .named_children(&mut cursor)
        .filter(|node| node.kind() == "export_statement")
    {
        let Some(declaration) = statement.child_by_field_name("declaration") else {
            continue;
        };
        if let Some(name) = declaration.child_by_field_name("name") {
            out.insert(node_text(name, bytes).to_string());
        }
        let mut inner = declaration.walk();
        for declarator in declaration
            .named_children(&mut inner)
            .filter(|node| node.kind() == "variable_declarator")
        {
            if let Some(name) = declarator.child_by_field_name("name") {
                out.insert(node_text(name, bytes).to_string());
            }
        }
    }
    out
}

fn owner(qualified: &str) -> &str {
    qualified
        .split("::")
        .next()
        .and_then(|head| head.split('.').next())
        .unwrap_or(qualified)
}

/// Re-exports of a Rust or JavaScript/TypeScript file, read from the syntax
/// tree so multi-line lists count: Rust `pub use` (any `pub(..)` scope,
/// nested lists, `as` aliases, globs) and JavaScript `export { .. } from`,
/// `export { binding }`, and `export * from`. Caller resolution follows
/// neither to the moved definition, so re-exports are checked here instead.
/// Python re-bindings need no list: Python resolution follows them.
fn reexports(language: Language, bytes: &[u8]) -> Reexports {
    let mut out = Reexports::default();
    if !matches!(
        language,
        Language::Rust | Language::JavaScript | Language::TypeScript
    ) {
        return out;
    }
    let Ok(parsed) = codesage_parser::parse::parse_file_tolerant(bytes, language) else {
        return out;
    };
    let root = parsed.tree.root_node();
    let mut cursor = root.walk();
    for item in root.named_children(&mut cursor) {
        match (language, item.kind()) {
            (Language::Rust, "use_declaration") => {
                let mut inner = item.walk();
                let public = item
                    .children(&mut inner)
                    .any(|child| child.kind() == "visibility_modifier");
                if public && let Some(argument) = item.child_by_field_name("argument") {
                    rust_use_names(argument, bytes, "", &mut out);
                }
            }
            (Language::JavaScript | Language::TypeScript, "export_statement") => {
                js_export_names(item, bytes, &mut out);
            }
            _ => {}
        }
    }
    out
}

fn node_text<'a>(node: tree_sitter::Node<'_>, bytes: &'a [u8]) -> &'a str {
    std::str::from_utf8(&bytes[node.byte_range()]).unwrap_or_default()
}

fn rust_use_names(node: tree_sitter::Node<'_>, bytes: &[u8], prefix: &str, out: &mut Reexports) {
    match node.kind() {
        "use_wildcard" => out
            .globs
            .push(format!("{prefix}{}", node_text(node, bytes))),
        "identifier" | "type_identifier" => {
            out.names.insert(node_text(node, bytes).to_string());
        }
        "scoped_identifier" => {
            if let Some(name) = node.child_by_field_name("name") {
                out.names.insert(node_text(name, bytes).to_string());
            }
        }
        "use_as_clause" => {
            if let Some(alias) = node.child_by_field_name("alias") {
                let alias = node_text(alias, bytes);
                if alias != "_" {
                    out.names.insert(alias.to_string());
                }
            }
        }
        "scoped_use_list" => {
            let nested = match node.child_by_field_name("path") {
                Some(path) => format!("{prefix}{}::", node_text(path, bytes)),
                None => prefix.to_string(),
            };
            if let Some(list) = node.child_by_field_name("list") {
                rust_use_names(list, bytes, &nested, out);
            }
        }
        "use_list" => {
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                rust_use_names(child, bytes, prefix, out);
            }
        }
        _ => {}
    }
}

fn js_export_names(statement: tree_sitter::Node<'_>, bytes: &[u8], out: &mut Reexports) {
    let mut cursor = statement.walk();
    let children: Vec<_> = statement.children(&mut cursor).collect();
    if let Some(source) = statement.child_by_field_name("source")
        && children.iter().any(|child| child.kind() == "*")
        && !children
            .iter()
            .any(|child| child.kind() == "namespace_export")
    {
        let specifier = node_text(source, bytes).trim_matches(|c| matches!(c, '\'' | '"' | '`'));
        out.globs.push(specifier.to_string());
        return;
    }
    for clause in children
        .iter()
        .filter(|child| child.kind() == "export_clause")
    {
        let mut inner = clause.walk();
        for specifier in clause
            .named_children(&mut inner)
            .filter(|child| child.kind() == "export_specifier")
        {
            let exported = specifier
                .child_by_field_name("alias")
                .or_else(|| specifier.child_by_field_name("name"));
            if let Some(exported) = exported {
                out.names.insert(node_text(exported, bytes).to_string());
            }
        }
    }
}

/// Whether the caller's working-tree file still spells the referenced name.
/// The index can predate an edit to a file outside the patch; a caller
/// deleted or rewritten without the name cannot break.
fn still_spelled(
    root: &Path,
    row: &Reference,
    sources: &mut HashMap<String, Option<String>>,
) -> bool {
    let source = sources.entry(row.from_file.clone()).or_insert_with(|| {
        read_indexable_file(root, Path::new(&row.from_file))
            .ok()
            .flatten()
            .map(|snapshot| String::from_utf8_lossy(&snapshot.bytes).into_owned())
    });
    let tail = row
        .to_name
        .rsplit(|c: char| !(c.is_alphanumeric() || c == '_' || c == '$'))
        .find(|segment| !segment.is_empty())
        .unwrap_or(&row.to_name);
    source
        .as_deref()
        .is_some_and(|source| source.contains(tail))
}

fn parse(path: &str, language: Language, bytes: &[u8]) -> Option<ParsedFile> {
    let info = FileInfo {
        path: path.to_string(),
        language,
        content_hash: content_hash(bytes),
        is_test: codesage_parser::discover::is_test_like_path(path),
    };
    let context = JavaTypeContext::from_sources(&[(path, bytes)]).ok()?;
    parse_source(&info, bytes, &context).ok()
}

/// Blob ids of the patch paths Git HEAD carries as indexable regular files.
/// `None` when HEAD cannot be listed.
fn head_blobs(
    root: &Path,
    files: &[PatchFile],
    deadline: Instant,
) -> Option<HashMap<String, String>> {
    let mut args = vec![
        "--literal-pathspecs",
        "ls-tree",
        "-r",
        "-z",
        "-l",
        "HEAD",
        "--",
    ];
    args.extend(files.iter().map(|file| file.path.as_str()));
    let wanted: HashSet<&str> = files.iter().map(|file| file.path.as_str()).collect();
    let bytes = git_output(root, &args, deadline)?;
    // `<mode> SP <type> SP <oid> SP+ <size> TAB <path>` per record.
    Some(
        String::from_utf8_lossy(&bytes)
            .split('\0')
            .filter_map(|record| {
                let (meta, path) = record.split_once('\t')?;
                let fields: Vec<&str> = meta.split_whitespace().collect();
                let [mode, "blob", oid, size] = fields.as_slice() else {
                    return None;
                };
                let size: u64 = size.parse().ok()?;
                (matches!(*mode, "100644" | "100755")
                    && size <= MAX_INDEXABLE_FILE_BYTES
                    && wanted.contains(path))
                .then(|| (path.to_string(), (*oid).to_string()))
            })
            .collect(),
    )
}

/// Mark files whose working bytes hash to their HEAD blob id, from one
/// `git hash-object` over the raw bytes. A file it cannot answer for stays
/// changed and is compared by content after its blob is read.
fn mark_unchanged(root: &Path, files: &mut [PatchFile], deadline: Instant) {
    let asked: Vec<usize> = files
        .iter()
        .enumerate()
        .filter(|(_, file)| file.working.is_some() && file.head_oid.is_some())
        .filter(|(_, file)| !file.path.contains('\n'))
        .map(|(i, _)| i)
        .collect();
    if asked.is_empty() {
        return;
    }
    let input: String = asked
        .iter()
        .map(|&i| format!("{}\n", files[i].path))
        .collect();
    let Some((Some(0), stdout)) = git_output_with_stdin(
        root,
        &["hash-object", "--no-filters", "--stdin-paths"],
        input.as_bytes(),
        deadline,
    ) else {
        return;
    };
    let oids: Vec<&str> = std::str::from_utf8(&stdout)
        .unwrap_or_default()
        .lines()
        .collect();
    if oids.len() != asked.len() {
        return;
    }
    for (&i, oid) in asked.iter().zip(oids) {
        files[i].unchanged = files[i].head_oid.as_deref() == Some(oid);
    }
}
#[cfg(all(test, unix))]
mod tests {
    use std::path::PathBuf;
    use std::process::Command;

    use super::*;
    use crate::overlay::{OverlayState, WorkingTreeOverlayCache};

    const LIB: &str = "def used(x):\n    return x + 1\n\ndef other():\n    return 2\n";

    fn git(root: &Path, args: &[&str]) {
        let status = Command::new("git")
            .current_dir(root)
            .args([
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@example.com",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?}");
    }

    /// `lib.py` defines `used` and `other`; `u1.py`..`u3.py` import and call
    /// `used`. Committed, then indexed at HEAD.
    fn fixture() -> (tempfile::TempDir, PathBuf) {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("lib.py"), LIB).unwrap();
        for i in 1..=3 {
            std::fs::write(
                root.path().join(format!("u{i}.py")),
                format!("from lib import used\n\ndef f{i}():\n    return used({i})\n"),
            )
            .unwrap();
        }
        git(root.path(), &["init", "-q"]);
        git(root.path(), &["add", "."]);
        git(root.path(), &["commit", "-qm", "base"]);
        let path = root.path().join("index.sqlite");
        reindex(root.path(), &path);
        (root, path)
    }

    fn reindex(root: &Path, path: &Path) {
        let db = Database::open(path).unwrap();
        crate::full_index(root, &db, &[], false).unwrap();
    }

    fn removed(db: &Database, root: &Path, files: &[&str]) -> Option<ReviewObjection> {
        let files: Vec<String> = files.iter().map(|f| f.to_string()).collect();
        let report = crate::build_review_rehearsal(root, db, &files).unwrap();
        let mut found = report
            .objections
            .into_iter()
            .filter(|o| o.category == CATEGORY);
        let objection = found.next();
        assert!(found.next().is_none());
        objection
    }

    fn assert_used_callers(objection: Option<ReviewObjection>, callers: &[&str]) {
        let objection = objection.expect("removed-symbol-callers objection");
        assert_eq!(objection.severity, ReviewSeverity::High);
        assert!(objection.counts_floor);
        assert_eq!(objection.files, callers);
        assert!(
            objection.title.starts_with(&format!(
                "1 removed definition(s) still referenced by {} indexed file(s)",
                callers.len()
            )),
            "{objection:?}"
        );
        assert_eq!(
            objection.evidence[0],
            format!(
                "lib.py: used (function, line 1) removed; {} referencing file(s) outside the patch: {}",
                callers.len(),
                callers.join(", ")
            )
        );
        let json = serde_json::to_value(&objection).unwrap();
        assert_eq!(json["counts_floor"], serde_json::Value::Bool(true));
    }

    #[test]
    fn removed_definition_names_its_callers_outside_the_patch() {
        let (root, path) = fixture();
        let db = Database::open_read_only(&path).unwrap();
        std::fs::write(root.path().join("lib.py"), "def other():\n    return 2\n").unwrap();
        assert_used_callers(
            removed(&db, root.path(), &["lib.py"]),
            &["u1.py", "u2.py", "u3.py"],
        );

        // Callers edited in the same patch are excluded, and so is a caller
        // whose working-tree file no longer spells the name.
        std::fs::write(root.path().join("u1.py"), "def f1():\n    return 1\n").unwrap();
        assert_used_callers(
            removed(&db, root.path(), &["lib.py", "u1.py"]),
            &["u2.py", "u3.py"],
        );
        assert_used_callers(removed(&db, root.path(), &["lib.py"]), &["u2.py", "u3.py"]);
        for i in 2..=3 {
            std::fs::write(
                root.path().join(format!("u{i}.py")),
                format!("def f{i}():\n    return {i}\n"),
            )
            .unwrap();
        }
        assert!(removed(&db, root.path(), &["lib.py", "u1.py", "u2.py", "u3.py"]).is_none());
    }

    #[test]
    fn uncalled_removals_and_unchanged_files_add_nothing() {
        let (root, path) = fixture();
        let db = Database::open_read_only(&path).unwrap();
        assert!(removed(&db, root.path(), &["lib.py"]).is_none());
        std::fs::write(
            root.path().join("lib.py"),
            "def used(x):\n    return x + 1\n",
        )
        .unwrap();
        assert!(removed(&db, root.path(), &["lib.py"]).is_none());
    }

    #[test]
    fn deleted_and_renamed_definitions_fire() {
        let (root, path) = fixture();
        let db = Database::open_read_only(&path).unwrap();
        std::fs::remove_file(root.path().join("lib.py")).unwrap();
        assert_used_callers(
            removed(&db, root.path(), &["lib.py"]),
            &["u1.py", "u2.py", "u3.py"],
        );
        std::fs::write(
            root.path().join("lib.py"),
            LIB.replace("def used(", "def renamed("),
        )
        .unwrap();
        assert_used_callers(
            removed(&db, root.path(), &["lib.py"]),
            &["u1.py", "u2.py", "u3.py"],
        );
    }

    #[test]
    fn a_move_counts_only_where_callers_bind_by_qualified_name() {
        // Python callers import the old module, so a move breaks them.
        let (root, path) = fixture();
        let db = Database::open_read_only(&path).unwrap();
        std::fs::write(root.path().join("lib.py"), "def other():\n    return 2\n").unwrap();
        std::fs::write(root.path().join("lib2.py"), "def used(x):\n    return x\n").unwrap();
        assert_used_callers(
            removed(&db, root.path(), &["lib.py", "lib2.py"]),
            &["u1.py", "u2.py", "u3.py"],
        );

        // A PHP class keeps its namespace-qualified name in its new file.
        let root = tempfile::tempdir().unwrap();
        let class = "<?php\nnamespace App;\nclass Foo {\n    public static function bar() {}\n}\n";
        std::fs::create_dir_all(root.path().join("src")).unwrap();
        std::fs::write(root.path().join("src/Foo.php"), class).unwrap();
        std::fs::write(
            root.path().join("caller.php"),
            "<?php\nuse App\\Foo;\nfunction run() { Foo::bar(); }\n",
        )
        .unwrap();
        git(root.path(), &["init", "-q"]);
        git(root.path(), &["add", "."]);
        git(root.path(), &["commit", "-qm", "base"]);
        let path = root.path().join("index.sqlite");
        reindex(root.path(), &path);
        let db = Database::open_read_only(&path).unwrap();
        std::fs::remove_file(root.path().join("src/Foo.php")).unwrap();
        let deleted = removed(&db, root.path(), &["src/Foo.php"]).expect("deleting Foo fires");
        assert_eq!(deleted.files, ["caller.php"], "{deleted:?}");
        // `Foo::bar()` records the bare `bar`; the `use App\\Foo` import holds it.
        assert_eq!(deleted.severity, ReviewSeverity::High, "{deleted:?}");
        std::fs::write(root.path().join("src/Moved.php"), class).unwrap();
        assert!(removed(&db, root.path(), &["src/Foo.php", "src/Moved.php"]).is_none());
    }

    #[test]
    fn index_and_overlay_ahead_of_head_still_see_the_removed_definition() {
        let (root, path) = fixture();
        std::fs::write(root.path().join("lib.py"), "def other():\n    return 2\n").unwrap();
        reindex(root.path(), &path);
        let db = Database::open_read_only(&path).unwrap();
        assert!(db.find_symbols("used", None).unwrap().is_empty());
        assert_used_callers(
            removed(&db, root.path(), &["lib.py"]),
            &["u1.py", "u2.py", "u3.py"],
        );

        // The daemon's working-tree overlay shadows the removed definition.
        let (root, path) = fixture();
        std::fs::write(root.path().join("lib.py"), "def other():\n    return 2\n").unwrap();
        let db = Database::open_read_only(&path).unwrap();
        let _snapshot = db.read_snapshot().unwrap();
        let report = WorkingTreeOverlayCache::default().apply(root.path(), &db, Some(1));
        assert_eq!(report.state, OverlayState::Overlay, "{report:?}");
        assert!(db.find_symbols("used", None).unwrap().is_empty());
        assert_used_callers(
            removed(&db, root.path(), &["lib.py"]),
            &["u1.py", "u2.py", "u3.py"],
        );
    }

    /// A committed repository holding `files`, indexed at HEAD into
    /// `.codesage/index.db` (where the Rust cross-crate resolver looks).
    fn repo(files: &[(&str, &str)]) -> (tempfile::TempDir, PathBuf) {
        let root = tempfile::tempdir().unwrap();
        for (path, source) in files {
            let path = root.path().join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, source).unwrap();
        }
        git(root.path(), &["init", "-q"]);
        git(root.path(), &["add", "."]);
        git(root.path(), &["commit", "-qm", "base"]);
        std::fs::create_dir_all(root.path().join(".codesage")).unwrap();
        let path = root.path().join(".codesage/index.db");
        reindex(root.path(), &path);
        (root, path)
    }

    fn write(root: &Path, path: &str, source: &str) {
        std::fs::write(root.join(path), source).unwrap();
    }

    fn rehearsal(db: &Database, root: &Path, files: &[&str]) -> codesage_protocol::ReviewRehearsal {
        let files: Vec<String> = files.iter().map(|f| f.to_string()).collect();
        crate::build_review_rehearsal(root, db, &files).unwrap()
    }

    #[test]
    fn callers_in_another_rust_crate_count() {
        let (root, path) = repo(&[
            (
                "Cargo.toml",
                "[workspace]\nmembers = [\"crates/core\", \"crates/app\"]\nresolver = \"2\"\n",
            ),
            (
                "crates/core/Cargo.toml",
                "[package]\nname = \"fx-core\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
            ),
            ("crates/core/src/lib.rs", "pub mod work;\n"),
            (
                "crates/core/src/work.rs",
                "pub fn used(x: i32) -> i32 { x + 1 }\npub fn other() {}\n",
            ),
            (
                "crates/app/Cargo.toml",
                "[package]\nname = \"fx-app\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[dependencies]\nfx-core = { path = \"../core\" }\n",
            ),
            (
                "crates/app/src/lib.rs",
                "pub fn f() -> i32 { fx_core::work::used(1) }\n",
            ),
        ]);
        let db = Database::open_read_only(&path).unwrap();
        write(
            root.path(),
            "crates/core/src/work.rs",
            "pub fn other() {}\n",
        );
        let objection = removed(&db, root.path(), &["crates/core/src/work.rs"]).expect("fires");
        assert_eq!(objection.severity, ReviewSeverity::High);
        assert_eq!(objection.files, ["crates/app/src/lib.rs"], "{objection:?}");
    }

    #[test]
    fn rebinding_or_reexporting_the_name_keeps_it() {
        let lib2 = "def used(x):\n    return x\n";
        for (lib, fires) in [
            (
                "from lib2 import used\n\ndef other():\n    return 2\n",
                false,
            ),
            ("from lib2 import *\n\ndef other():\n    return 2\n", false),
            ("def other():\n    return 2\n", true),
        ] {
            let (root, path) = fixture();
            let db = Database::open_read_only(&path).unwrap();
            write(root.path(), "lib2.py", lib2);
            write(root.path(), "lib.py", lib);
            assert_eq!(
                removed(&db, root.path(), &["lib.py", "lib2.py"]).is_some(),
                fires,
                "{lib}"
            );
        }

        for (lib, fires) in [
            (
                "export { used } from './lib2.js';\nexport function other() { return 2; }\n",
                false,
            ),
            (
                "export * from './lib2.js';\nexport function other() { return 2; }\n",
                false,
            ),
            (
                "export {\n  used,\n} from './lib2.js';\nexport function other() { return 2; }\n",
                false,
            ),
            ("export function other() { return 2; }\n", true),
        ] {
            let (root, path) = repo(&[
                (
                    "lib.js",
                    "export function used(x) { return x + 1; }\nexport function other() { return 2; }\n",
                ),
                (
                    "u1.js",
                    "import { used } from './lib.js';\nexport function f1() { return used(1); }\n",
                ),
            ]);
            let db = Database::open_read_only(&path).unwrap();
            write(
                root.path(),
                "lib2.js",
                "export function used(x) { return x + 1; }\n",
            );
            write(root.path(), "lib.js", lib);
            assert_eq!(
                removed(&db, root.path(), &["lib.js", "lib2.js"]).is_some(),
                fires,
                "{lib}"
            );
        }

        for (a, fires) in [
            ("pub use crate::b::used;\n", false),
            ("pub use crate::b::*;\n", false),
            ("pub(crate) use crate::b::{other, used};\n", false),
            ("pub use crate::b::other as used;\n", false),
            ("pub use crate::b::{\n    used,\n    other,\n};\n", false),
            ("use crate::b::used;\n", true),
            ("", true),
        ] {
            let (root, path) = repo(&[
                (
                    "Cargo.toml",
                    "[package]\nname = \"fx\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
                ),
                ("src/lib.rs", "pub mod a;\npub mod b;\npub mod user;\n"),
                ("src/a.rs", "pub fn used(x: i32) -> i32 { x + 1 }\n"),
                ("src/b.rs", "pub fn other() {}\n"),
                ("src/user.rs", "pub fn f() -> i32 { crate::a::used(1) }\n"),
            ]);
            let db = Database::open_read_only(&path).unwrap();
            write(
                root.path(),
                "src/b.rs",
                "pub fn used(x: i32) -> i32 { x + 1 }\npub fn other() {}\n",
            );
            write(root.path(), "src/a.rs", a);
            assert_eq!(
                removed(&db, root.path(), &["src/a.rs", "src/b.rs"]).is_some(),
                fires,
                "{a:?}"
            );
        }
    }

    #[test]
    fn name_only_callers_are_medium() {
        let (root, path) = repo(&[
            (
                "store.js",
                "class Store {\n  save() { return 1; }\n  load() { return 2; }\n}\n",
            ),
            (
                "other.js",
                "function persist(model) { return model.save(); }\n",
            ),
        ]);
        let db = Database::open_read_only(&path).unwrap();
        write(
            root.path(),
            "store.js",
            "class Store {\n  load() { return 2; }\n}\n",
        );
        let objection = removed(&db, root.path(), &["store.js"]).expect("fires");
        assert_eq!(objection.severity, ReviewSeverity::Medium, "{objection:?}");
        assert!(objection.title.contains("by name alone"), "{objection:?}");
        assert!(
            objection
                .evidence
                .iter()
                .any(|e| e.contains("by name alone") && e.contains("other.js")),
            "{objection:?}"
        );
    }

    #[test]
    fn degraded_parses_and_unchanged_patches_are_disclosed_not_flagged() {
        let (root, path) = fixture();
        let db = Database::open_read_only(&path).unwrap();
        let notes = |report: &codesage_protocol::ReviewRehearsal| report.summary_notes.join("\n");

        let report = rehearsal(&db, root.path(), &["lib.py"]);
        assert!(report.objections.iter().all(|o| o.category != CATEGORY));
        assert!(
            notes(&report).contains("every examined patch file matches HEAD"),
            "{report:?}"
        );

        write(root.path(), "lib.py", "def other(:\n    return 2\n");
        let report = rehearsal(&db, root.path(), &["lib.py"]);
        assert!(
            report.objections.iter().all(|o| o.category != CATEGORY),
            "{report:?}"
        );
        assert!(
            notes(&report).contains("degraded (syntax errors): lib.py"),
            "{report:?}"
        );
    }

    #[test]
    fn paths_normalize_and_repositories_without_head_stay_silent() {
        let (root, path) = fixture();
        let db = Database::open_read_only(&path).unwrap();
        write(root.path(), "lib.py", "def other():\n    return 2\n");
        let absolute = root.path().join("lib.py").display().to_string();
        for spelling in ["./lib.py", absolute.as_str()] {
            assert_used_callers(
                removed(&db, root.path(), &[spelling, "../outside.py"]),
                &["u1.py", "u2.py", "u3.py"],
            );
        }

        let unborn = tempfile::tempdir().unwrap();
        write(unborn.path(), "lib.py", "def other():\n    return 2\n");
        git(unborn.path(), &["init", "-q"]);
        let path = unborn.path().join("index.sqlite");
        reindex(unborn.path(), &path);
        let db = Database::open_read_only(&path).unwrap();
        let report = rehearsal(&db, unborn.path(), &["lib.py"]);
        assert!(
            report
                .summary_notes
                .iter()
                .all(|n| !n.contains("Removed-definition")),
            "{report:?}"
        );
    }

    #[test]
    fn caller_files_are_bounded() {
        let mut files = vec![("lib.py".to_string(), LIB.to_string())];
        for i in 0..FILES_CAP + 3 {
            files.push((
                format!("u{i:02}.py"),
                format!("from lib import used\n\ndef f():\n    return used({i})\n"),
            ));
        }
        let files: Vec<(&str, &str)> = files
            .iter()
            .map(|(p, s)| (p.as_str(), s.as_str()))
            .collect();
        let (root, path) = repo(&files);
        let db = Database::open_read_only(&path).unwrap();
        write(root.path(), "lib.py", "def other():\n    return 2\n");
        let objection = removed(&db, root.path(), &["lib.py"]).expect("fires");
        assert_eq!(objection.files.len(), FILES_CAP);
        assert!(
            objection
                .title
                .contains(&format!("{} indexed file(s)", FILES_CAP + 3))
        );
        assert!(
            objection.evidence.iter().any(|e| e
                == &format!(
                    "files lists the first {FILES_CAP} of {} caller file(s)",
                    FILES_CAP + 3
                )),
            "{objection:?}"
        );
    }

    #[test]
    fn a_reexported_rust_type_keeps_its_methods() {
        let foo = "pub struct Foo;\nimpl Foo {\n    pub fn new() -> Self { Foo }\n}\n";
        for (a, fires) in [
            ("pub use crate::b::Foo;\n", false),
            ("pub use crate::b::*;\n", false),
            ("pub use crate::b::{\n    Foo,\n};\n", false),
            ("", true),
        ] {
            let (root, path) = repo(&[
                (
                    "Cargo.toml",
                    "[package]\nname = \"fx\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
                ),
                ("src/lib.rs", "pub mod a;\npub mod b;\npub mod user;\n"),
                ("src/a.rs", foo),
                ("src/b.rs", "pub fn other() {}\n"),
                (
                    "src/user.rs",
                    "use crate::a::Foo;\npub fn f() -> Foo { Foo::new() }\n",
                ),
            ]);
            let db = Database::open_read_only(&path).unwrap();
            write(
                root.path(),
                "src/b.rs",
                &format!("{foo}pub fn other() {{}}\n"),
            );
            write(root.path(), "src/a.rs", a);
            let objection = removed(&db, root.path(), &["src/a.rs", "src/b.rs"]);
            assert_eq!(objection.is_some(), fires, "{a:?}: {objection:?}");
        }
    }

    fn rust_repo(a: &str) -> (tempfile::TempDir, PathBuf) {
        repo(&[
            (
                "Cargo.toml",
                "[package]\nname = \"fx\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
            ),
            (
                "src/lib.rs",
                "pub mod a;\npub mod b;\npub mod util;\npub mod user;\n",
            ),
            ("src/a.rs", a),
            ("src/b.rs", "pub fn other() {}\n"),
            ("src/util.rs", "pub fn helper() {}\n"),
            ("src/user.rs", "pub fn f() -> i32 { crate::a::used(1) }\n"),
        ])
    }

    #[test]
    fn a_glob_keeps_only_what_its_source_defines() {
        // An unrelated glob in the file does not hide a removal.
        let (root, path) =
            rust_repo("pub use crate::util::*;\npub fn used(x: i32) -> i32 { x + 1 }\n");
        let db = Database::open_read_only(&path).unwrap();
        write(root.path(), "src/a.rs", "pub use crate::util::*;\n");
        assert_eq!(
            removed(&db, root.path(), &["src/a.rs"]).map(|o| o.files),
            Some(vec!["src/user.rs".to_string()])
        );
        // A glob whose source does define the name keeps it.
        write(
            root.path(),
            "src/util.rs",
            "pub fn helper() {}\npub fn used(x: i32) -> i32 { x }\n",
        );
        assert!(removed(&db, root.path(), &["src/a.rs", "src/util.rs"]).is_none());
        // An unresolvable glob keeps the removal and says so.
        write(root.path(), "src/util.rs", "pub fn helper() {}\n");
        write(root.path(), "src/a.rs", "pub use crate::missing::*;\n");
        let report = rehearsal(&db, root.path(), &["src/a.rs", "src/util.rs"]);
        let objection = report
            .objections
            .iter()
            .find(|o| o.category == CATEGORY)
            .expect("an unverified glob keeps the removal");
        assert!(
            objection
                .evidence
                .iter()
                .any(|e| e.contains("glob re-export") && e.contains("src/a.rs: crate::missing::*")),
            "{objection:?}"
        );

        for (util, fires) in [
            ("export function helper() { return 1; }\n", true),
            (
                "export function helper() { return 1; }\nexport function used(x) { return x; }\n",
                false,
            ),
        ] {
            let (root, path) = repo(&[
                (
                    "lib.js",
                    "export * from './util.js';\nexport function used(x) { return x + 1; }\n",
                ),
                ("util.js", "export function helper() { return 1; }\n"),
                (
                    "u1.js",
                    "import { used } from './lib.js';\nexport function f1() { return used(1); }\n",
                ),
            ]);
            let db = Database::open_read_only(&path).unwrap();
            write(root.path(), "util.js", util);
            write(root.path(), "lib.js", "export * from './util.js';\n");
            assert_eq!(
                removed(&db, root.path(), &["lib.js", "util.js"]).is_some(),
                fires,
                "{util}"
            );
        }
    }

    #[test]
    fn a_reexported_type_keeps_only_members_it_still_defines() {
        let (root, path) = repo(&[
            (
                "Cargo.toml",
                "[package]\nname = \"fx\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
            ),
            ("src/lib.rs", "pub mod a;\npub mod b;\npub mod user;\n"),
            (
                "src/a.rs",
                "pub struct Foo;\nimpl Foo {\n    pub fn new() -> Self { Foo }\n}\n",
            ),
            ("src/b.rs", "pub fn other() {}\n"),
            (
                "src/user.rs",
                "use crate::a::Foo;\npub fn f() -> Foo { Foo::new() }\n",
            ),
        ]);
        let db = Database::open_read_only(&path).unwrap();
        write(
            root.path(),
            "src/b.rs",
            "pub struct Foo;\npub fn other() {}\n",
        );
        write(root.path(), "src/a.rs", "pub use crate::b::Foo;\n");
        let objection =
            removed(&db, root.path(), &["src/a.rs", "src/b.rs"]).expect("Foo::new is gone");
        assert!(
            objection.evidence[0].starts_with("src/a.rs: Foo::new (method"),
            "{objection:?}"
        );
    }

    /// Rust crate where `src/user.rs` calls `crate::a::used`; `a.rs` loses
    /// `used` for `pub use crate::b::*`, and `extra` adds or replaces files.
    fn rust_chain(extra: &[(&str, &str)], mods: &[&str]) -> Option<ReviewObjection> {
        let lib: String = ["a", "user"]
            .iter()
            .chain(mods)
            .map(|m| format!("pub mod {m};\n"))
            .collect();
        let mut files = vec![
            (
                "Cargo.toml",
                "[package]\nname = \"fx\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
            ),
            ("src/lib.rs", lib.as_str()),
            ("src/a.rs", "pub fn used(x: i32) -> i32 { x + 1 }\n"),
            ("src/user.rs", "pub fn f() -> i32 { crate::a::used(1) }\n"),
        ];
        files.extend(extra.iter().copied());
        let (root, path) = repo(&files);
        let db = Database::open_read_only(&path).unwrap();
        write(root.path(), "src/a.rs", "pub use crate::b::*;\n");
        removed(&db, root.path(), &["src/a.rs"])
    }

    /// `lib.js` loses `used` for `export * from './b.js'`; the patch also
    /// writes `b.js` and `c.js`, which start out empty.
    fn js_chain(b: &str, c: &str) -> Option<ReviewObjection> {
        let (root, path) = repo(&[
            ("lib.js", "export function used(x) { return x + 1; }\n"),
            ("b.js", "export function placeholder() {}\n"),
            ("c.js", "export function placeholder() {}\n"),
            (
                "u1.js",
                "import { used } from './lib.js';\nexport function f1() { return used(1); }\n",
            ),
        ]);
        let db = Database::open_read_only(&path).unwrap();
        write(root.path(), "lib.js", "export * from './b.js';\n");
        write(root.path(), "b.js", b);
        write(root.path(), "c.js", c);
        removed(&db, root.path(), &["lib.js", "b.js", "c.js"])
    }

    #[test]
    fn glob_sources_are_followed_through_their_own_reexports() {
        let c = ("src/c.rs", "pub fn used(x: i32) -> i32 { x }\n");
        assert!(rust_chain(&[("src/b.rs", "pub use crate::c::*;\n"), c], &["b", "c"]).is_none());
        assert!(rust_chain(&[("src/b.rs", "pub use crate::c::used;\n"), c], &["b", "c"]).is_none());
        assert!(
            rust_chain(
                &[
                    ("src/b.rs", "pub use crate::c::*;\n"),
                    ("src/c.rs", "pub fn other() {}\n")
                ],
                &["b", "c"]
            )
            .is_some()
        );
        let c = "export function used(x) { return x; }\n";
        assert!(js_chain("export * from './c.js';\n", c).is_none());
        assert!(js_chain("export { used } from './c.js';\n", c).is_none());
        assert!(
            js_chain(
                "export * from './c.js';\n",
                "export function other() { return 1; }\n"
            )
            .is_some()
        );

        // Past the depth cap the removal stays, with the unverified note.
        let hops: Vec<(String, String)> = (0..GLOB_DEPTH + 1)
            .map(|i| {
                let next = format!("m{}", i + 1);
                (
                    format!(
                        "src/{}.rs",
                        if i == 0 {
                            "b".to_string()
                        } else {
                            format!("m{i}")
                        }
                    ),
                    format!("pub use crate::{next}::*;\n"),
                )
            })
            .chain(std::iter::once((
                format!("src/m{}.rs", GLOB_DEPTH + 1),
                "pub fn used(x: i32) -> i32 { x }\n".to_string(),
            )))
            .collect();
        let mods: Vec<String> = std::iter::once("b".to_string())
            .chain((1..=GLOB_DEPTH + 1).map(|i| format!("m{i}")))
            .collect();
        let extra: Vec<(&str, &str)> = hops.iter().map(|(p, s)| (p.as_str(), s.as_str())).collect();
        let mods: Vec<&str> = mods.iter().map(String::as_str).collect();
        let objection = rust_chain(&extra, &mods).expect("a chain past the cap stays unverified");
        assert!(
            objection
                .evidence
                .iter()
                .any(|e| e.contains("loops or exceeds four hops")
                    && e.contains("src/a.rs: crate::b::*")),
            "{objection:?}"
        );
    }

    #[test]
    fn a_glob_keeps_only_exported_items() {
        assert!(
            rust_chain(
                &[("src/b.rs", "pub fn used(x: i32) -> i32 { x }\n")],
                &["b"]
            )
            .is_none()
        );
        assert!(
            rust_chain(
                &[("src/b.rs", "pub(crate) fn used(x: i32) -> i32 { x }\n")],
                &["b"]
            )
            .is_none()
        );
        assert!(rust_chain(&[("src/b.rs", "fn used(x: i32) -> i32 { x }\n")], &["b"]).is_some());
        assert!(js_chain("export function used(x) { return x; }\n", "").is_none());
        assert!(js_chain("function used(x) { return x; }\n", "").is_some());
    }
}
