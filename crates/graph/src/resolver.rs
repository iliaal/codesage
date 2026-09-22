//! One target resolver behind every tool that names an entity.
//!
//! [`resolve_target`] takes the string a caller wrote and says what it names.
//! The grammar is tried in one fixed order, so the same string means the same
//! thing to `find_symbol`, `impact_analysis`, `trace_call_path`,
//! `export_context`, and `feature_bundle`:
//!
//! 1. a handle — `sym:path#qualified[@line]`, `file:path`, `dir:path`,
//!    `chunk:path:start-end`, `feat_<hex16>`
//! 2. `route:METHOD path` or `cmd:name`
//! 3. an indexed file path, exactly as stored
//! 4. `path:line` — the definition enclosing that line
//! 5. a qualified symbol name (`Db::open`, `App\Db::open`, `Class.method`)
//! 6. a bare symbol name
//! 7. a feature id
//! 8. nothing indexed: [`TargetKind::Text`], which the caller may hand to
//!    semantic search
//!
//! [`ResolveOptions::kind_hint`] skips the arms that cannot apply, so a
//! caller that knows it wants a file never resolves a path to a symbol with
//! the same spelling.
//!
//! One ambiguity policy comes out of it. A resolution carrying several
//! candidates is `ambiguous`; tools whose answer is a union of per-entity
//! answers return the union and disclose it, and tools whose answer would be
//! wrong as a union return [`TargetError::Ambiguous`] with the candidates'
//! handles. Nothing bails with prose.

use std::collections::HashSet;

use anyhow::Result;
use codesage_protocol::{
    FileCategory, Handle, ResolveVia, ResolvedTarget, Symbol, SymbolKind, TargetKind,
    TargetResolution,
};
use codesage_storage::Database;

use crate::bundle::resolve_callee_definitions;
use crate::impact::is_qualified_symbol_name;

/// Candidates returned for one ambiguous input before the list is cut.
pub const DEFAULT_CANDIDATE_LIMIT: usize = 10;

/// Nearest-candidate scans read every indexed path once; this bounds the
/// candidates kept from that scan, not the scan itself.
const MAX_NEAREST: usize = 10;

/// How a caller narrows [`resolve_target`].
#[derive(Debug, Clone)]
pub struct ResolveOptions {
    /// Skip the grammar arms that cannot produce this kind. `None` tries
    /// every arm in order.
    pub kind_hint: Option<TargetKind>,
    /// The file the name was read in. A bare name with several definitions
    /// that this file's imports narrow to one resolves through it, `via`
    /// [`ResolveVia::Import`].
    pub from_file: Option<String>,
    /// Keep module declarations (`mod x;`) among the candidates. They are
    /// dropped by default: a declaration is not the definition an agent that
    /// searched for the name is looking for.
    pub include_modules: bool,
    /// Keep only definitions of this kind. Applied before the resolution is
    /// built, so `ambiguous` and `candidates_total` describe the definitions
    /// the caller can see rather than every one that shares the name.
    pub kind: Option<SymbolKind>,
    /// Candidates kept; `candidates_total` still reports how many existed.
    pub limit: usize,
    /// Look for nearest candidates (case- and suffix-matched) when nothing
    /// matches as written. Off for hot internal paths, where a miss is
    /// ordinary and the scan is not worth its cost.
    pub nearest: bool,
}

impl Default for ResolveOptions {
    fn default() -> Self {
        Self {
            kind_hint: None,
            from_file: None,
            include_modules: false,
            kind: None,
            limit: DEFAULT_CANDIDATE_LIMIT,
            nearest: true,
        }
    }
}

impl ResolveOptions {
    /// Symbol-shaped arms only.
    pub fn symbol() -> Self {
        Self {
            kind_hint: Some(TargetKind::Symbol),
            ..Self::default()
        }
    }

    /// Path-shaped arms only; `path:line` may still name a symbol.
    pub fn file() -> Self {
        Self {
            kind_hint: Some(TargetKind::File),
            ..Self::default()
        }
    }

    pub fn with_from_file(mut self, file: impl Into<String>) -> Self {
        self.from_file = Some(file.into());
        self
    }

    pub fn with_modules(mut self, include: bool) -> Self {
        self.include_modules = include;
        self
    }

    pub fn with_kind(mut self, kind: Option<SymbolKind>) -> Self {
        self.kind = kind;
        self
    }

    /// Definitions only (unless modules were asked for), of the requested
    /// kind when one was given.
    fn retain(&self, symbols: &mut Vec<Symbol>) {
        retain_definitions(symbols, self.include_modules);
        self.retain_kind(symbols);
    }

    fn retain_kind(&self, symbols: &mut Vec<Symbol>) {
        if let Some(kind) = self.kind {
            symbols.retain(|s| s.kind == kind);
        }
    }

    pub fn with_limit(mut self, limit: usize) -> Self {
        self.limit = limit;
        self
    }

    pub fn with_nearest(mut self, nearest: bool) -> Self {
        self.nearest = nearest;
        self
    }

    /// Path-shaped arms: an indexed path, `path:line`, and path nearest
    /// candidates. `path:line` resolves to the enclosing definition, so a
    /// file hint admits a symbol result here.
    fn allows_paths(&self) -> bool {
        matches!(
            self.kind_hint,
            None | Some(TargetKind::File | TargetKind::Dir | TargetKind::Chunk)
        )
    }

    fn allows_symbols(&self) -> bool {
        matches!(self.kind_hint, None | Some(TargetKind::Symbol))
    }

    fn allows_features(&self) -> bool {
        matches!(
            self.kind_hint,
            None | Some(TargetKind::Feature | TargetKind::Route | TargetKind::Command)
        )
    }
}

/// A target that names several entities, none, or a kind the tool does not
/// take.
///
/// Carried through `anyhow` so a caller can `downcast_ref` and answer with
/// handles: the MCP layer maps [`TargetError::Ambiguous`] onto `E_AMBIGUOUS`
/// with a retry remedy carrying the first candidate,
/// [`TargetError::NotFound`] onto `E_NOT_FOUND` with the nearest candidates,
/// and [`TargetError::Unsupported`] onto `E_PARAM`.
#[derive(Debug, Clone, PartialEq)]
pub enum TargetError {
    Ambiguous {
        input: String,
        /// At least two; each carries a handle that names one of them.
        candidates: Vec<ResolvedTarget>,
        candidates_total: usize,
        /// Candidates that share a file and qualified name with another
        /// candidate, so their handles carry `@line`. Summed over every file
        /// in the set.
        overloads: usize,
    },
    NotFound {
        input: String,
        /// Case- or suffix-matched leads; empty when the resolver found none.
        nearest: Vec<ResolvedTarget>,
    },
    /// The input parsed as a target kind the tool has no answer for (a
    /// `dir:` handle on `impact_analysis`). A parameter error, not a miss:
    /// nothing about the index would make it succeed.
    Unsupported {
        input: String,
        kind: TargetKind,
        /// The kinds the tool does take, as prose for the message.
        accepted: &'static str,
    },
}

impl TargetError {
    /// The refusal a resolution implies for a tool whose answer would be
    /// wrong as a union; `None` when the resolution names exactly one entity.
    pub fn of(resolution: &TargetResolution) -> Option<Self> {
        // Nearest candidates are leads, not entities: two of them are a miss
        // with two suggestions, not an ambiguous target.
        if resolution.guessed() {
            return Some(TargetError::NotFound {
                input: resolution.input.clone(),
                nearest: resolution.resolved.clone(),
            });
        }
        if resolution.ambiguous {
            return Some(TargetError::Ambiguous {
                input: resolution.input.clone(),
                candidates: resolution.resolved.clone(),
                candidates_total: resolution.candidates_total,
                overloads: resolution.overloads,
            });
        }
        if resolution.sole().is_none() {
            return Some(TargetError::NotFound {
                input: resolution.input.clone(),
                nearest: resolution.resolved.clone(),
            });
        }
        None
    }

    /// The error code this refusal maps onto, so the MCP contract and the
    /// CLI name the same failure.
    pub fn code(&self) -> &'static str {
        match self {
            Self::Ambiguous { .. } => "E_AMBIGUOUS",
            Self::NotFound { .. } => "E_NOT_FOUND",
            Self::Unsupported { .. } => "E_PARAM",
        }
    }

    /// Every candidate handle, in resolution order.
    pub fn handles(&self) -> Vec<String> {
        match self {
            Self::Ambiguous { candidates, .. }
            | Self::NotFound {
                nearest: candidates,
                ..
            } => candidates.iter().map(|c| c.handle.clone()).collect(),
            Self::Unsupported { .. } => Vec::new(),
        }
    }

    pub fn input(&self) -> &str {
        match self {
            Self::Ambiguous { input, .. }
            | Self::NotFound { input, .. }
            | Self::Unsupported { input, .. } => input,
        }
    }
}

fn kind_name(kind: TargetKind) -> &'static str {
    match kind {
        TargetKind::Symbol => "symbol",
        TargetKind::File => "file",
        TargetKind::Dir => "directory",
        TargetKind::Chunk => "chunk",
        TargetKind::Feature => "feature",
        TargetKind::Route => "route",
        TargetKind::Command => "command",
        TargetKind::Text => "text",
    }
}

impl std::fmt::Display for TargetError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Ambiguous {
                input,
                candidates,
                candidates_total,
                overloads,
            } => {
                write!(
                    f,
                    "ambiguous target '{input}': {candidates_total} definitions"
                )?;
                if *overloads > 0 {
                    write!(
                        f,
                        " ({overloads} of them are same-file overloads, addressed by @line)"
                    )?;
                }
                write!(f, " — retry with one handle: {}", sample(candidates))
            }
            Self::NotFound { input, nearest } => {
                write!(f, "no indexed target named '{input}'")?;
                if nearest.is_empty() {
                    Ok(())
                } else {
                    write!(f, " — nearest: {}", sample(nearest))
                }
            }
            Self::Unsupported {
                input,
                kind,
                accepted,
            } => write!(
                f,
                "'{input}' names a {}, which this tool does not take; accepted targets: {accepted}",
                kind_name(*kind)
            ),
        }
    }
}

impl std::error::Error for TargetError {}

fn sample(candidates: &[ResolvedTarget]) -> String {
    const SHOWN: usize = 5;
    let shown = candidates
        .iter()
        .take(SHOWN)
        .map(|c| c.handle.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    if candidates.len() > SHOWN {
        format!("{shown}, +{} more", candidates.len() - SHOWN)
    } else {
        shown
    }
}

/// The one entity `resolution` names, or the typed refusal.
pub fn require_one(resolution: &TargetResolution) -> Result<&ResolvedTarget> {
    match TargetError::of(resolution) {
        Some(error) => Err(error.into()),
        None => Ok(resolution
            .sole()
            .expect("`TargetError::of` returns None only for a sole candidate")),
    }
}

/// What `input` names. See the module docs for the grammar and its order.
pub fn resolve_target(
    db: &Database,
    input: &str,
    opts: ResolveOptions,
) -> Result<TargetResolution> {
    Ok(resolve_symbols(db, input, opts)?.0)
}

/// [`resolve_target`] plus the definitions behind a symbol resolution, so a
/// caller that seeds a walk from them does not query them again. The symbol
/// list is complete even when `resolved` was cut to the candidate limit, and
/// empty for every non-symbol kind.
pub(crate) fn resolve_symbols(
    db: &Database,
    input: &str,
    opts: ResolveOptions,
) -> Result<(TargetResolution, Vec<Symbol>)> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Ok((TargetResolution::text(trimmed), Vec::new()));
    }
    if let Some(handle) = Handle::parse(trimmed) {
        return resolve_handle(db, trimmed, handle, &opts);
    }
    if opts.allows_features() {
        if let Some(route) = trimmed.strip_prefix("route:") {
            return Ok((
                resolve_feature_entry(db, trimmed, route.trim(), TargetKind::Route, &opts)?,
                Vec::new(),
            ));
        }
        if let Some(command) = trimmed.strip_prefix("cmd:") {
            return Ok((
                resolve_feature_entry(db, trimmed, command.trim(), TargetKind::Command, &opts)?,
                Vec::new(),
            ));
        }
    }
    // Paths are stored without a leading `./`; the spelling a shell completes
    // to must still name the file.
    let path_input = strip_current_dir(trimmed);
    if opts.allows_paths() {
        if db.file_id_for_path(path_input)?.is_some() {
            return Ok((
                one(
                    trimmed,
                    TargetKind::File,
                    file_candidate(path_input, ResolveVia::Exact),
                ),
                Vec::new(),
            ));
        }
        if let Some((path, line)) = split_path_line(path_input)
            && db.file_id_for_path(path)?.is_some()
        {
            return resolve_path_line(db, trimmed, path, line, &opts);
        }
    }
    if opts.allows_symbols()
        && let Some(resolved) = resolve_symbol_name(db, trimmed, &opts)?
    {
        return Ok(resolved);
    }
    if opts.allows_features()
        && let Some(feature) = db.load_feature(trimmed)?
    {
        return Ok((
            one(
                trimmed,
                TargetKind::Feature,
                feature_candidate(&feature.feature_id, &feature.entry_path, ResolveVia::Exact),
            ),
            Vec::new(),
        ));
    }
    if opts.nearest {
        if opts.allows_symbols()
            && let Some(resolved) = nearest_symbols(db, trimmed, &opts)?
        {
            return Ok(resolved);
        }
        if opts.allows_paths()
            && let Some(mut resolution) = nearest_paths(db, path_input, &opts)?
        {
            resolution.input = trimmed.to_string();
            return Ok((resolution, Vec::new()));
        }
    }
    Ok((TargetResolution::text(trimmed), Vec::new()))
}

/// `./src/x.rs` → `src/x.rs`, however many times the prefix repeats.
fn strip_current_dir(input: &str) -> &str {
    let mut rest = input;
    while let Some(stripped) = rest.strip_prefix("./") {
        rest = stripped;
    }
    rest
}

/// Candidates the resolver had to guess at (case- or suffix-matched) are
/// leads for the caller, never rows: keep them out of a result set.
pub(crate) fn matched_symbols(resolution: &TargetResolution, symbols: Vec<Symbol>) -> Vec<Symbol> {
    if resolution.guessed() {
        Vec::new()
    } else {
        symbols
    }
}

/// A `mod x;` declaration: a module or namespace symbol with no body on its
/// own line. Excluded from definition results by default — an agent that
/// searched for `render` wants the module's contents, not the line that
/// declares it.
pub fn is_module_declaration(sym: &Symbol) -> bool {
    matches!(sym.kind, SymbolKind::Module | SymbolKind::Namespace) && sym.line_start == sym.line_end
}

/// Drop module declarations unless the caller asked for them or for the
/// module kind itself.
pub fn retain_definitions(symbols: &mut Vec<Symbol>, include_modules: bool) {
    if include_modules {
        return;
    }
    symbols.retain(|s| !is_module_declaration(s));
}

fn resolve_handle(
    db: &Database,
    input: &str,
    handle: Handle,
    opts: &ResolveOptions,
) -> Result<(TargetResolution, Vec<Symbol>)> {
    match handle {
        Handle::Symbol {
            path,
            qualified,
            line,
        } => {
            let mut symbols: Vec<Symbol> = db
                .symbols_for_file(&path)?
                .into_iter()
                .filter(|s| names(s, &qualified))
                .filter(|s| line.is_none_or(|line| s.line_start == line))
                .collect();
            // A handle names a definition outright, module declaration or not.
            opts.retain_kind(&mut symbols);
            sort_definitions(&mut symbols);
            if symbols.is_empty()
                && opts.nearest
                && let Some(moved) = moved_definition(db, input, &qualified, opts)?
            {
                return Ok(moved);
            }
            let total = symbols.len();
            let overloads = symbols.iter().filter(|s| s.overloaded).count();
            let resolution = TargetResolution::new(
                input,
                TargetKind::Symbol,
                candidates(&symbols, ResolveVia::Exact, opts.limit),
                total,
                overloads,
            );
            Ok((resolution, symbols))
        }
        Handle::File { path } => Ok((
            if db.file_id_for_path(&path)?.is_some() {
                one(
                    input,
                    TargetKind::File,
                    file_candidate(&path, ResolveVia::Exact),
                )
            } else {
                TargetResolution::new(input, TargetKind::File, Vec::new(), 0, 0)
            },
            Vec::new(),
        )),
        Handle::Dir { path } => {
            let prefix = format!("{path}/");
            let holds_files = !db.indexed_files_with_prefix(&prefix)?.is_empty();
            let resolution = if holds_files {
                one(
                    input,
                    TargetKind::Dir,
                    ResolvedTarget {
                        handle: Handle::Dir { path: path.clone() }.to_string(),
                        kind: "dir".to_string(),
                        path: Some(path),
                        line_start: None,
                        line_end: None,
                        is_test: false,
                        confidence: ResolveVia::Exact.confidence(),
                        via: ResolveVia::Exact,
                    },
                )
            } else {
                TargetResolution::new(input, TargetKind::Dir, Vec::new(), 0, 0)
            };
            Ok((resolution, Vec::new()))
        }
        Handle::Chunk { path, start, end } => Ok((
            if db.file_id_for_path(&path)?.is_some() {
                one(
                    input,
                    TargetKind::Chunk,
                    ResolvedTarget {
                        handle: Handle::chunk(&path, start, end).to_string(),
                        kind: "chunk".to_string(),
                        is_test: is_test_path(&path),
                        path: Some(path),
                        line_start: Some(start),
                        line_end: Some(end),
                        confidence: ResolveVia::Exact.confidence(),
                        via: ResolveVia::Exact,
                    },
                )
            } else {
                TargetResolution::new(input, TargetKind::Chunk, Vec::new(), 0, 0)
            },
            Vec::new(),
        )),
        Handle::Feature { id } => Ok((
            match db.load_feature(&id)? {
                Some(feature) => one(
                    input,
                    TargetKind::Feature,
                    feature_candidate(&feature.feature_id, &feature.entry_path, ResolveVia::Exact),
                ),
                None => TargetResolution::new(input, TargetKind::Feature, Vec::new(), 0, 0),
            },
            Vec::new(),
        )),
    }
}

/// `route:GET /users/{id}` and `cmd:codesage` name a mapped feature by its
/// entry route or command rather than by id.
fn resolve_feature_entry(
    db: &Database,
    input: &str,
    entry: &str,
    kind: TargetKind,
    opts: &ResolveOptions,
) -> Result<TargetResolution> {
    use codesage_protocol::FeatureKind;
    let feature_kind = match kind {
        TargetKind::Route => FeatureKind::Route,
        _ => FeatureKind::CliCommand,
    };
    let matches: Vec<_> = db
        .list_features(Some(feature_kind), None, None, 0)?
        .into_iter()
        .filter(|f| {
            let stored = match kind {
                TargetKind::Route => f.entry_route.as_deref(),
                _ => f.entry_command.as_deref(),
            };
            stored.is_some_and(|stored| stored.eq_ignore_ascii_case(entry))
        })
        .collect();
    let total = matches.len();
    let resolved: Vec<ResolvedTarget> = matches
        .iter()
        .take(opts.limit)
        .map(|f| feature_candidate(&f.feature_id, &f.entry_path, ResolveVia::Exact))
        .collect();
    Ok(TargetResolution::new(input, kind, resolved, total, 0))
}

/// `path:line` names the innermost definition holding that line; the file
/// itself when no definition does.
fn resolve_path_line(
    db: &Database,
    input: &str,
    path: &str,
    line: u32,
    opts: &ResolveOptions,
) -> Result<(TargetResolution, Vec<Symbol>)> {
    let mut symbols = db.symbols_for_file(path)?;
    opts.retain(&mut symbols);
    let enclosing = symbols
        .into_iter()
        .filter(|s| s.line_start <= line && line <= s.line_end)
        .max_by_key(|s| s.line_start);
    Ok(match enclosing {
        Some(sym) => {
            let overloads = usize::from(sym.overloaded);
            let symbols = vec![sym];
            let resolution = TargetResolution::new(
                input,
                TargetKind::Symbol,
                candidates(&symbols, ResolveVia::Exact, opts.limit),
                1,
                overloads,
            );
            (resolution, symbols)
        }
        None => {
            let mut candidate = file_candidate(path, ResolveVia::Exact);
            candidate.line_start = Some(line);
            candidate.line_end = Some(line);
            (one(input, TargetKind::File, candidate), Vec::new())
        }
    })
}

/// A qualified or bare symbol name, narrowed by the calling file's imports
/// when one was supplied and the bare name is shared.
fn resolve_symbol_name(
    db: &Database,
    input: &str,
    opts: &ResolveOptions,
) -> Result<Option<(TargetResolution, Vec<Symbol>)>> {
    let mut symbols = db.find_symbols(input, None)?;
    opts.retain(&mut symbols);
    if symbols.is_empty() {
        return Ok(None);
    }
    sort_definitions(&mut symbols);
    // `unique` marks a bare-name match; several of them make the resolution
    // `ambiguous`, and no single candidate is then the answer.
    // `/` is not a qualifier here: a name carrying one is a path, and the
    // path arms ran first.
    let via = if is_qualified_symbol_name(input) {
        ResolveVia::Qualified
    } else {
        ResolveVia::Unique
    };
    if symbols.len() > 1
        && let Some(from_file) = opts.from_file.as_deref()
        && let Some(narrowed) = narrow_by_imports(db, from_file, input, &symbols)?
    {
        let resolution = one(
            input,
            TargetKind::Symbol,
            candidate_of(&narrowed, ResolveVia::Import),
        );
        return Ok(Some((resolution, vec![narrowed])));
    }
    let total = symbols.len();
    let overloads = symbols.iter().filter(|s| s.overloaded).count();
    let resolution = TargetResolution::new(
        input,
        TargetKind::Symbol,
        candidates(&symbols, via, opts.limit),
        total,
        overloads,
    );
    Ok(Some((resolution, symbols)))
}

/// The one definition a call to `name` in `from_file` resolves to, when
/// import-aware resolution narrows the shared name to exactly one of
/// `symbols`. `None` leaves the ambiguity standing.
fn narrow_by_imports(
    db: &Database,
    from_file: &str,
    name: &str,
    symbols: &[Symbol],
) -> Result<Option<Symbol>> {
    let narrowed = resolve_callee_definitions(db, from_file, name)?;
    let [only] = narrowed.as_slice() else {
        return Ok(None);
    };
    Ok(symbols
        .iter()
        .any(|s| identity(s) == identity(only))
        .then(|| only.clone()))
}

/// Where a handle's definition went: the same qualified name in another
/// file, then the case- and tail-matched leads. Every candidate is a lead,
/// so a handle whose definition was renamed or moved fails as not-found with
/// somewhere to look rather than as an empty answer.
///
/// A same-qualified-name hit in another file is labelled `suffix`, not
/// `qualified`: the input was the handle, whose path half did not match, so
/// only its trailing name segment did. `qualified` would put the lead at
/// confidence 1.0 and let a stale handle silently answer for a definition it
/// never named.
fn moved_definition(
    db: &Database,
    input: &str,
    qualified: &str,
    opts: &ResolveOptions,
) -> Result<Option<(TargetResolution, Vec<Symbol>)>> {
    let mut symbols = db.find_symbols(qualified, None)?;
    // A lead is a suggestion, so module declarations stay out of it even
    // though an exact handle may name one.
    opts.retain(&mut symbols);
    if symbols.is_empty() {
        return Ok(
            nearest_symbols(db, qualified, opts)?.map(|(mut resolution, symbols)| {
                resolution.input = input.to_string();
                (resolution, symbols)
            }),
        );
    }
    sort_definitions(&mut symbols);
    let total = symbols.len();
    let resolution = TargetResolution::new(
        input,
        TargetKind::Symbol,
        candidates(&symbols, ResolveVia::Suffix, opts.limit),
        total,
        0,
    );
    Ok(Some((resolution, symbols)))
}

/// Case-insensitive name matches, then the last segment of a qualified name.
/// Leads for a rename, never an answer: both land below [`CONFIDENT`].
fn nearest_symbols(
    db: &Database,
    input: &str,
    opts: &ResolveOptions,
) -> Result<Option<(TargetResolution, Vec<Symbol>)>> {
    let mut seen: HashSet<(String, String, u32)> = HashSet::new();
    let mut found: Vec<(Symbol, ResolveVia)> = Vec::new();
    for variant in case_variants(input) {
        let mut symbols = db.find_symbols(&variant, None)?;
        opts.retain(&mut symbols);
        for sym in symbols {
            if seen.insert(identity(&sym)) {
                found.push((sym, ResolveVia::Casefold));
            }
        }
    }
    if found.is_empty()
        && let Some(tail) = last_segment(input)
    {
        let mut symbols = db.find_symbols(tail, None)?;
        opts.retain(&mut symbols);
        for sym in symbols {
            if seen.insert(identity(&sym)) {
                found.push((sym, ResolveVia::Suffix));
            }
        }
    }
    if found.is_empty() {
        return Ok(None);
    }
    found.sort_by(|(a, _), (b, _)| {
        a.file_path
            .cmp(&b.file_path)
            .then_with(|| a.line_start.cmp(&b.line_start))
    });
    let total = found.len();
    let resolved: Vec<ResolvedTarget> = found
        .iter()
        .take(opts.limit.min(MAX_NEAREST))
        .map(|(sym, via)| candidate_of(sym, *via))
        .collect();
    let resolution = TargetResolution::new(input, TargetKind::Symbol, resolved, total, 0);
    Ok(Some((
        resolution,
        found.into_iter().map(|(sym, _)| sym).collect(),
    )))
}

/// Indexed paths that end in the input at a `/` boundary, then ones that do
/// so ignoring case.
fn nearest_paths(
    db: &Database,
    input: &str,
    opts: &ResolveOptions,
) -> Result<Option<TargetResolution>> {
    if !input.contains('/') && !input.contains('.') {
        return Ok(None);
    }
    let paths = db.all_file_paths()?;
    let suffix: Vec<&String> = paths
        .iter()
        .filter(|p| path_suffix_matches(p, input, false))
        .collect();
    let (matched, via) = if suffix.is_empty() {
        (
            paths
                .iter()
                .filter(|p| path_suffix_matches(p, input, true))
                .collect::<Vec<_>>(),
            ResolveVia::Casefold,
        )
    } else {
        (suffix, ResolveVia::Suffix)
    };
    if matched.is_empty() {
        return Ok(None);
    }
    let total = matched.len();
    let resolved: Vec<ResolvedTarget> = matched
        .iter()
        .take(opts.limit.min(MAX_NEAREST))
        .map(|p| file_candidate(p, via))
        .collect();
    Ok(Some(TargetResolution::new(
        input,
        TargetKind::File,
        resolved,
        total,
        0,
    )))
}

fn path_suffix_matches(indexed: &str, input: &str, fold: bool) -> bool {
    let (indexed, input) = if fold {
        (indexed.to_ascii_lowercase(), input.to_ascii_lowercase())
    } else {
        (indexed.to_string(), input.to_string())
    };
    indexed == input
        || indexed
            .strip_suffix(input.as_str())
            .is_some_and(|head| head.ends_with('/'))
}

/// `Db::open` → `open`, `App\Db` → `Db`, `pkg/mod.py` → `mod.py`. `None` for
/// a name with no separator.
fn last_segment(name: &str) -> Option<&str> {
    ["::", "\\", ".", "/"]
        .iter()
        .filter_map(|sep| name.rfind(sep).map(|pos| pos + sep.len()))
        .max()
        .map(|cut| &name[cut..])
        .filter(|tail| !tail.is_empty() && *tail != name)
}

/// Spellings that differ from `input` only in ASCII case.
fn case_variants(input: &str) -> Vec<String> {
    let mut out = Vec::new();
    let lower = input.to_ascii_lowercase();
    let upper = input.to_ascii_uppercase();
    let mut capitalized = lower.clone();
    if let Some(first) = capitalized.get_mut(..1) {
        first.make_ascii_uppercase();
    }
    for variant in [lower, upper, capitalized] {
        if variant != input && !out.contains(&variant) {
            out.push(variant);
        }
    }
    out
}

fn names(sym: &Symbol, qualified: &str) -> bool {
    sym.qualified_name == qualified || (sym.qualified_name.is_empty() && sym.name == qualified)
}

/// Order candidates by where they live, so one input yields one candidate
/// order across runs: an agent that retries with `candidates[0]` must get the
/// same definition every time.
fn sort_definitions(symbols: &mut [Symbol]) {
    symbols.sort_by(|a, b| {
        a.file_path
            .cmp(&b.file_path)
            .then_with(|| a.line_start.cmp(&b.line_start))
            .then_with(|| a.qualified_name.cmp(&b.qualified_name))
    });
}

fn identity(sym: &Symbol) -> (String, String, u32) {
    (
        sym.file_path.clone(),
        sym.qualified_name.clone(),
        sym.line_start,
    )
}

/// `path:line`, where `line` is all digits. A Windows drive letter or a
/// `chunk:` range never reaches here: handles parse first and paths are
/// repository-relative.
fn split_path_line(input: &str) -> Option<(&str, u32)> {
    let (path, digits) = input.rsplit_once(':')?;
    if path.is_empty() || digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok().map(|line| (path, line))
}

fn is_test_path(path: &str) -> bool {
    matches!(FileCategory::classify(path), FileCategory::Test)
}

fn candidates(symbols: &[Symbol], via: ResolveVia, limit: usize) -> Vec<ResolvedTarget> {
    symbols
        .iter()
        .take(limit)
        .map(|sym| candidate_of(sym, via))
        .collect()
}

/// `is_test` is the stored symbol flag, which reads 0 for rows indexed
/// before `0024_is_test` until the next `codesage index` reparses them;
/// [`file_candidate`] derives the same flag from the path instead.
fn candidate_of(sym: &Symbol, via: ResolveVia) -> ResolvedTarget {
    ResolvedTarget {
        handle: sym.handle().to_string(),
        kind: sym.kind.as_str().to_string(),
        is_test: sym.is_test,
        path: Some(sym.file_path.clone()),
        line_start: Some(sym.line_start),
        line_end: Some(sym.line_end),
        confidence: via.confidence(),
        via,
    }
}

/// A file has no symbol row to read `is_test` from, so the flag is derived
/// from the path here, unlike [`candidate_of`], which reads the stored one.
fn file_candidate(path: &str, via: ResolveVia) -> ResolvedTarget {
    ResolvedTarget {
        handle: Handle::file(path)
            .map(|h| h.to_string())
            .unwrap_or_default(),
        kind: "file".to_string(),
        path: Some(path.to_string()),
        line_start: None,
        line_end: None,
        is_test: is_test_path(path),
        confidence: via.confidence(),
        via,
    }
}

fn feature_candidate(feature_id: &str, entry_path: &str, via: ResolveVia) -> ResolvedTarget {
    ResolvedTarget {
        handle: feature_id.to_string(),
        kind: "feature".to_string(),
        path: Some(entry_path.to_string()),
        line_start: None,
        line_end: None,
        is_test: false,
        confidence: via.confidence(),
        via,
    }
}

fn one(input: &str, kind: TargetKind, candidate: ResolvedTarget) -> TargetResolution {
    TargetResolution::new(input, kind, vec![candidate], 1, 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use codesage_protocol::{
        FeatureConfidence, FeatureFileRef, FeatureFileRole, FeatureKind, FeatureRecord, Language,
    };

    /// Two definitions of `search` in different modules, one `mod render;`
    /// declaration beside the module it declares, and a C++ file with two
    /// same-named overloads on different lines.
    fn project() -> Database {
        let db = Database::open_in_memory().unwrap();
        db.execute_raw_for_tests(
            "INSERT INTO files (id, path, language, content_hash) VALUES
                (1, 'src/lib.rs', 'rust', 'lib'),
                (2, 'src/search.rs', 'rust', 'search'),
                (3, 'src/index/search.rs', 'rust', 'nested'),
                (4, 'src/render.rs', 'rust', 'render'),
                (5, 'src/over.cpp', 'cpp', 'over'),
                (6, 'tests/search_test.rs', 'rust', 'test');
             INSERT INTO symbols
                (file_id, name, qualified_name, kind, line_start, line_end, col_start, col_end, is_test)
                VALUES
                (1, 'render', 'render', 'module', 3, 3, 0, 11, 0),
                (1, 'run', 'run', 'function', 10, 20, 0, 1, 0),
                (2, 'search', 'search', 'function', 5, 9, 0, 1, 0),
                (3, 'search', 'search', 'function', 2, 4, 0, 1, 0),
                (4, 'render', 'render', 'function', 1, 6, 0, 1, 0),
                (5, 'run', 'Foo::run', 'function', 10, 12, 0, 1, 0),
                (5, 'run', 'Foo::run', 'function', 30, 33, 0, 1, 0),
                (6, 'covers_search', 'covers_search', 'function', 1, 4, 0, 1, 1);",
        )
        .unwrap();
        db
    }

    fn resolve(db: &Database, input: &str) -> TargetResolution {
        resolve_target(db, input, ResolveOptions::default()).unwrap()
    }

    #[test]
    fn handles_name_one_entity_of_each_kind() {
        let db = project();

        let symbol = resolve(&db, "sym:src/search.rs#search");
        assert_eq!(symbol.kind, TargetKind::Symbol);
        assert_eq!(
            symbol.sole().map(|c| c.line_start),
            Some(Some(5)),
            "{symbol:?}"
        );
        assert_eq!(symbol.resolved[0].via, ResolveVia::Exact);
        assert_eq!(symbol.resolved[0].confidence, 1.0);

        let file = resolve(&db, "file:src/search.rs");
        assert_eq!(file.kind, TargetKind::File);
        assert_eq!(
            file.sole().map(|c| c.handle.as_str()),
            Some("file:src/search.rs")
        );

        let dir = resolve(&db, "dir:src/index");
        assert_eq!(dir.kind, TargetKind::Dir);
        assert_eq!(dir.sole().map(|c| c.handle.as_str()), Some("dir:src/index"));

        let chunk = resolve(&db, "chunk:src/search.rs:1-9");
        assert_eq!(chunk.kind, TargetKind::Chunk);
        assert_eq!(chunk.sole().map(|c| c.line_end), Some(Some(9)));

        // A handle whose entity is gone names nothing, and says where the
        // name lives now when it moved.
        let renamed = resolve(&db, "sym:src/search.rs#seek");
        assert_eq!(renamed.kind, TargetKind::Symbol);
        assert!(renamed.resolved.is_empty(), "{renamed:?}");

        let moved = resolve(&db, "sym:src/gone.rs#search");
        assert!(moved.guessed(), "{moved:?}");
        assert_eq!(moved.input, "sym:src/gone.rs#search");
        assert_eq!(
            moved.handles(),
            ["sym:src/index/search.rs#search", "sym:src/search.rs#search"],
            "a moved definition is a lead, not an answer"
        );
        assert_eq!(moved.resolved[0].via, ResolveVia::Suffix);
        assert!(resolve(&db, "file:src/gone.rs").resolved.is_empty());
        assert!(resolve(&db, "dir:src/empty").resolved.is_empty());
    }

    #[test]
    fn the_grammar_is_tried_in_one_order() {
        let db = project();

        // An indexed path outranks a symbol lookup that would never match it.
        let path = resolve(&db, "src/search.rs");
        assert_eq!(path.kind, TargetKind::File);
        assert_eq!(path.resolved[0].via, ResolveVia::Exact);

        // A shell-completed `./` spelling names the same file; the input is
        // kept as written so a remedy can find the argument that carried it.
        for spelled in ["./src/search.rs", "././src/search.rs"] {
            let dotted = resolve(&db, spelled);
            assert_eq!(dotted.kind, TargetKind::File, "{spelled}: {dotted:?}");
            assert_eq!(dotted.input, spelled);
            assert_eq!(
                dotted.sole().map(|c| c.handle.as_str()),
                Some("file:src/search.rs"),
                "{spelled}"
            );
        }
        let dotted_line = resolve(&db, "./src/search.rs:7");
        assert_eq!(
            dotted_line.sole().map(|c| c.handle.as_str()),
            Some("sym:src/search.rs#search")
        );
        let dotted_suffix = resolve(&db, "./index/search.rs");
        assert_eq!(dotted_suffix.input, "./index/search.rs");
        assert_eq!(dotted_suffix.resolved[0].via, ResolveVia::Suffix);
        assert_eq!(dotted_suffix.resolved[0].handle, "file:src/index/search.rs");

        // `path:line` names the definition holding the line.
        let at_line = resolve(&db, "src/search.rs:7");
        assert_eq!(at_line.kind, TargetKind::Symbol);
        assert_eq!(
            at_line.sole().map(|c| c.handle.as_str()),
            Some("sym:src/search.rs#search")
        );
        // A line inside no definition still names the file.
        let outside = resolve(&db, "src/search.rs:99");
        assert_eq!(outside.kind, TargetKind::File);
        assert_eq!(outside.resolved[0].line_start, Some(99));

        // A qualified name matches the stored qualified name.
        let qualified = resolve(&db, "Foo::run");
        assert_eq!(qualified.resolved[0].via, ResolveVia::Qualified);
        assert_eq!(qualified.candidates_total, 2, "both overloads");

        // A bare name with one definition.
        let bare = resolve(&db, "covers_search");
        assert_eq!(bare.resolved[0].via, ResolveVia::Unique);
        assert_eq!(bare.resolved[0].confidence, 0.9);
        assert!(bare.resolved[0].is_test, "the stored flag marks its rows");

        // Nothing indexed carries it: the caller may search for the text.
        let text = resolve(&db, "where does ranking happen");
        assert_eq!(text.kind, TargetKind::Text);
        assert!(text.resolved.is_empty());
        assert_eq!(text.candidates_total, 0);
    }

    #[test]
    fn a_shared_name_is_ambiguous_with_one_handle_per_definition() {
        let db = project();
        let ambiguous = resolve(&db, "search");
        assert!(ambiguous.ambiguous);
        assert_eq!(ambiguous.candidates_total, 2);
        assert_eq!(ambiguous.sole(), None);
        assert_eq!(
            ambiguous.handles(),
            ["sym:src/index/search.rs#search", "sym:src/search.rs#search"]
        );
        assert_eq!(ambiguous.overloads, 0, "the two live in different files");
    }

    #[test]
    fn overloads_are_counted_and_carry_line_handles() {
        let db = project();
        let overloaded = resolve(&db, "Foo::run");
        assert!(overloaded.ambiguous);
        assert_eq!(overloaded.overloads, 2);
        assert_eq!(
            overloaded.handles(),
            [
                "sym:src/over.cpp#Foo::run@10",
                "sym:src/over.cpp#Foo::run@30"
            ],
            "an intra-file ambiguity is addressable only by line"
        );
        // And the line handle names one of them.
        let one = resolve(&db, "sym:src/over.cpp#Foo::run@30");
        assert_eq!(one.sole().map(|c| c.line_start), Some(Some(30)));
    }

    #[test]
    fn module_declarations_stay_out_of_definition_results() {
        let db = project();
        let render = resolve(&db, "render");
        assert_eq!(
            render.handles(),
            ["sym:src/render.rs#render"],
            "`mod render;` is a declaration, not the definition"
        );
        assert!(!render.ambiguous);

        let with_modules =
            resolve_target(&db, "render", ResolveOptions::default().with_modules(true)).unwrap();
        assert_eq!(with_modules.candidates_total, 2);
        assert!(with_modules.ambiguous);

        // A handle names what it names, declaration included.
        let declaration = resolve(&db, "sym:src/lib.rs#render");
        assert_eq!(declaration.sole().map(|c| c.kind.as_str()), Some("module"));
    }

    #[test]
    fn a_kind_hint_skips_the_arms_that_cannot_apply() {
        let db = project();
        // `src/search.rs` is a path, never a symbol name.
        let as_symbol = resolve_target(&db, "src/search.rs", ResolveOptions::symbol()).unwrap();
        assert_eq!(as_symbol.kind, TargetKind::Text, "{as_symbol:?}");

        // A file hint still lets `path:line` name the definition there.
        let at_line = resolve_target(&db, "src/search.rs:7", ResolveOptions::file()).unwrap();
        assert_eq!(at_line.kind, TargetKind::Symbol);

        // A symbol name is not looked up as a path.
        let as_file = resolve_target(&db, "search", ResolveOptions::file()).unwrap();
        assert_eq!(as_file.kind, TargetKind::Text, "{as_file:?}");
    }

    #[test]
    fn nearest_candidates_are_leads_rather_than_answers() {
        let db = project();

        let casefold = resolve(&db, "Search");
        assert_eq!(casefold.resolved[0].via, ResolveVia::Casefold);
        assert!(casefold.guessed(), "{casefold:?}");
        assert_eq!(casefold.sole(), None, "a guess is never acted on");

        let suffix = resolve(&db, "Missing::search");
        assert_eq!(suffix.resolved[0].via, ResolveVia::Suffix);
        assert!(suffix.guessed());

        let path = resolve(&db, "index/search.rs");
        assert_eq!(path.kind, TargetKind::File);
        assert_eq!(path.resolved[0].via, ResolveVia::Suffix);
        assert_eq!(path.resolved[0].handle, "file:src/index/search.rs");

        // Off for the hot internal paths, where a miss is ordinary.
        let quiet =
            resolve_target(&db, "Search", ResolveOptions::default().with_nearest(false)).unwrap();
        assert_eq!(quiet.kind, TargetKind::Text);
        assert!(quiet.resolved.is_empty());
    }

    #[test]
    fn refusals_separate_several_entities_from_none() {
        let db = project();

        let error = TargetError::of(&resolve(&db, "search")).expect("two definitions");
        let TargetError::Ambiguous {
            candidates_total, ..
        } = &error
        else {
            panic!("expected an ambiguous refusal, got {error}");
        };
        assert_eq!(*candidates_total, 2);
        assert_eq!(error.code(), "E_AMBIGUOUS");
        assert!(error.to_string().contains("sym:src/search.rs#search"));

        let missing = TargetError::of(&resolve(&db, "Search")).expect("a guess is not an answer");
        let TargetError::NotFound { nearest, .. } = &missing else {
            panic!("expected a miss, got {missing}");
        };
        assert_eq!(nearest.len(), 2, "both `search` definitions are leads");
        assert_eq!(missing.code(), "E_NOT_FOUND");

        assert!(TargetError::of(&resolve(&db, "covers_search")).is_none());
        assert!(require_one(&resolve(&db, "covers_search")).is_ok());
        assert!(require_one(&resolve(&db, "search")).is_err());

        let unsupported = TargetError::Unsupported {
            input: "dir:src".into(),
            kind: TargetKind::Dir,
            accepted: "a `sym:` handle",
        };
        assert_eq!(unsupported.code(), "E_PARAM");
        assert!(unsupported.handles().is_empty());
        let text = unsupported.to_string();
        assert!(text.contains("names a directory"), "{text}");
        assert!(text.contains("a `sym:` handle"), "{text}");
    }

    #[test]
    fn overload_counts_are_not_described_as_one_file() {
        let overloaded = TargetError::Ambiguous {
            input: "run".into(),
            candidates: Vec::new(),
            candidates_total: 4,
            overloads: 4,
        };
        let text = overloaded.to_string();
        assert!(text.contains("4 of them are same-file overloads"), "{text}");
        assert!(!text.contains("sharing one file"), "{text}");
    }

    #[test]
    fn a_kind_filter_narrows_the_resolution_before_ambiguity_is_judged() {
        let db = project();
        db.execute_raw_for_tests(
            "INSERT INTO symbols
                (file_id, name, qualified_name, kind, line_start, line_end, col_start, col_end)
                VALUES (4, 'search', 'search', 'constant', 8, 8, 0, 1);",
        )
        .unwrap();
        let all = resolve(&db, "search");
        assert_eq!(all.candidates_total, 3);
        assert!(all.ambiguous);

        let constants = resolve_target(
            &db,
            "search",
            ResolveOptions::default().with_kind(Some(SymbolKind::Constant)),
        )
        .unwrap();
        assert!(!constants.ambiguous, "{constants:?}");
        assert_eq!(constants.handles(), ["sym:src/render.rs#search"]);

        let by_handle = resolve_target(
            &db,
            "sym:src/render.rs#search",
            ResolveOptions::default().with_kind(Some(SymbolKind::Function)),
        )
        .unwrap();
        // Nothing matches as a function, so the handle falls through to
        // leads, like a handle whose definition moved: no answer, only
        // same-named functions elsewhere at lead confidence.
        assert!(
            by_handle.guessed(),
            "a handle to a constant is not a function: {by_handle:?}"
        );
        assert_eq!(by_handle.sole(), None);
        assert!(
            by_handle.resolved.iter().all(|c| c.kind == "function"),
            "{by_handle:?}"
        );
    }

    /// `caller.ts` imports `./base`, so a call to `anchor` there names
    /// `base.ts`'s definition even though `other.ts` defines one too.
    fn import_project() -> Database {
        let db = Database::open_in_memory().unwrap();
        db.execute_raw_for_tests(
            "INSERT INTO files (id, path, language, content_hash) VALUES
                (1, 'base.ts', 'typescript', 'base'),
                (2, 'other.ts', 'typescript', 'other'),
                (3, 'caller.ts', 'typescript', 'caller'),
                (4, 'orphan.ts', 'typescript', 'orphan');
             INSERT INTO symbols
                (file_id, name, qualified_name, kind, line_start, line_end, col_start, col_end)
                VALUES (1, 'anchor', 'anchor', 'function', 1, 1, 0, 20),
                       (2, 'anchor', 'anchor', 'function', 1, 1, 0, 20);
             INSERT INTO refs (from_file_id, to_name, to_name_tail, kind, line, col) VALUES
                (3, './base', 'base', 'import', 1, 0),
                (3, 'anchor', 'anchor', 'call', 2, 0);",
        )
        .unwrap();
        db
    }

    #[test]
    fn the_calling_files_imports_narrow_a_shared_name() {
        let db = import_project();
        assert!(resolve(&db, "anchor").ambiguous);

        let narrowed = resolve_target(
            &db,
            "anchor",
            ResolveOptions::symbol().with_from_file("caller.ts"),
        )
        .unwrap();
        assert!(!narrowed.ambiguous);
        assert_eq!(
            narrowed.sole().map(|c| c.handle.as_str()),
            Some("sym:base.ts#anchor")
        );
        assert_eq!(narrowed.resolved[0].via, ResolveVia::Import);
        assert_eq!(narrowed.resolved[0].confidence, 0.8);

        // A file that imports neither leaves the ambiguity standing.
        let orphan = resolve_target(
            &db,
            "anchor",
            ResolveOptions::symbol().with_from_file("orphan.ts"),
        )
        .unwrap();
        assert!(orphan.ambiguous, "{orphan:?}");
    }

    fn feature(id: &str, route: Option<&str>, command: Option<&str>) -> FeatureRecord {
        FeatureRecord {
            feature_id: id.to_string(),
            title: "t".into(),
            summary: "s".into(),
            kind: if route.is_some() {
                FeatureKind::Route
            } else {
                FeatureKind::CliCommand
            },
            source: "test".into(),
            confidence: FeatureConfidence::High,
            entry_path: "src/lib.rs".into(),
            entry_symbol: None,
            entry_route: route.map(str::to_string),
            entry_command: command.map(str::to_string),
            test_command: None,
            language: Language::Rust,
            tags: Vec::new(),
            trust_boundaries: Vec::new(),
            files: vec![FeatureFileRef {
                path: "src/lib.rs".into(),
                role: FeatureFileRole::Entry,
                reason: None,
            }],
        }
    }

    #[test]
    fn features_resolve_by_id_route_and_command() {
        let db = project();
        db.upsert_feature(&feature(
            "feat_0123456789abcdef",
            Some("POST /api/login"),
            None,
        ))
        .unwrap();
        db.upsert_feature(&feature("feat_fedcba9876543210", None, Some("codesage")))
            .unwrap();

        let by_id = resolve(&db, "feat_0123456789abcdef");
        assert_eq!(by_id.kind, TargetKind::Feature);
        assert_eq!(
            by_id.sole().map(|c| c.handle.as_str()),
            Some("feat_0123456789abcdef")
        );

        let by_route = resolve(&db, "route:POST /api/login");
        assert_eq!(by_route.kind, TargetKind::Route);
        assert_eq!(
            by_route.sole().map(|c| c.handle.as_str()),
            Some("feat_0123456789abcdef")
        );

        let by_command = resolve(&db, "cmd:codesage");
        assert_eq!(by_command.kind, TargetKind::Command);
        assert_eq!(
            by_command.sole().map(|c| c.handle.as_str()),
            Some("feat_fedcba9876543210")
        );

        let unknown = resolve(&db, "feat_ffffffffffffffff");
        assert_eq!(unknown.kind, TargetKind::Feature);
        assert!(unknown.resolved.is_empty());
        assert!(resolve(&db, "route:GET /nothing").resolved.is_empty());
    }
}
