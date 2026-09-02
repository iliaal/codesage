use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

pub const DEFAULT_EMBEDDING_DIM: usize = 384;

/// Generate `as_str` / `parse` / `Display` for a string-keyed enum. The literal
/// keys are the on-disk / DB representation, which is deliberately distinct from
/// the serde JSON wire format each enum owns via its own `#[serde(rename_all)]`
/// when a type needs a separate API spelling. Extra `| "alias"` literals are
/// accepted by `parse` but never emitted by `as_str`.
macro_rules! str_enum {
    ($name:ident { $($variant:ident => $s:literal $(| $alias:literal)* ),+ $(,)? }) => {
        impl $name {
            pub fn as_str(&self) -> &'static str {
                match self { $( $name::$variant => $s, )+ }
            }
            pub fn parse(s: &str) -> Option<Self> {
                match s { $( $s $(| $alias)* => Some($name::$variant), )+ _ => None }
            }
        }
        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(self.as_str())
            }
        }
    };
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Language {
    Php,
    Python,
    C,
    #[serde(alias = "c++", alias = "cxx")]
    Cpp,
    Java,
    Rust,
    #[serde(alias = "js")]
    JavaScript,
    #[serde(alias = "ts")]
    TypeScript,
    Go,
}

str_enum!(Language {
    Php => "php",
    Python => "python",
    C => "c",
    Cpp => "cpp" | "c++" | "cxx",
    Java => "java",
    Rust => "rust",
    JavaScript => "javascript" | "js",
    TypeScript => "typescript" | "ts",
    Go => "go",
});

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum SymbolKind {
    Function,
    Method,
    Class,
    Trait,
    Interface,
    Struct,
    Enum,
    Constant,
    Macro,
    Module,
    Namespace,
}

str_enum!(SymbolKind {
    Function => "function",
    Method => "method",
    Class => "class",
    Trait => "trait",
    Interface => "interface",
    Struct => "struct",
    Enum => "enum",
    Constant => "constant",
    Macro => "macro",
    Module => "module",
    Namespace => "namespace",
});

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct FileInfo {
    pub path: String,
    pub language: Language,
    pub content_hash: String,
}

/// The `#[serde]` attributes on this struct feed only the `JsonSchema` derive;
/// the wire format is the manual `Serialize` / `Deserialize` impls below,
/// which exist because `qualified_name` is dropped whenever it repeats `name`
/// (a per-field `skip_serializing_if` cannot see a sibling field).
#[derive(Debug, Clone, PartialEq, Eq, schemars::JsonSchema)]
pub struct Symbol {
    pub name: String,
    /// Fully qualified name (`Class::method`, `Ns\Class`). Omitted from JSON
    /// when identical to `name`; a missing field means "same as `name`".
    #[serde(default)]
    pub qualified_name: String,
    pub kind: SymbolKind,
    pub file_path: String,
    pub line_start: u32,
    pub line_end: u32,
    /// Byte column of the definition node. Kept in SQLite but never emitted
    /// over JSON: it is the indentation column, not something an agent acts on.
    #[serde(skip)]
    pub col_start: u32,
    /// Byte column where the definition node ends (the closing brace). Same
    /// treatment as `col_start`.
    #[serde(skip)]
    pub col_end: u32,
    /// Decision-shape comments attached to this symbol's definition: lines
    /// the author wrote to explain WHY this code exists or NOTE-worthy
    /// constraints. Extracted from comments immediately adjacent to the
    /// symbol's definition range, filtered to lines whose first token is
    /// a recognized rationale marker (`WHY:`, `NOTE:`, `IMPORTANT:`,
    /// `FIXME:`, `HACK:`, `XXX:`, `TODO:`).
    ///
    /// **Two limitations callers should know about:**
    /// 1. Rationale rots when code changes. We extract whatever the source
    ///    says at index time; we cannot verify it's still accurate. Treat
    ///    as "what the author said at write-time", not ground truth.
    /// 2. Populated only on definitions, not on callsites/references.
    ///    Rationale is a property of where the code lives.
    ///
    /// Empty when the symbol has no rationale-shape comments. Skipped on
    /// JSON serialization in that case so existing consumers see no diff.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub rationale: Vec<RationaleEntry>,
}

impl Serialize for Symbol {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let emit_qualified = self.qualified_name != self.name;
        let emit_rationale = !self.rationale.is_empty();
        let len = 5 + usize::from(emit_qualified) + usize::from(emit_rationale);
        let mut s = serializer.serialize_struct("Symbol", len)?;
        s.serialize_field("name", &self.name)?;
        if emit_qualified {
            s.serialize_field("qualified_name", &self.qualified_name)?;
        }
        s.serialize_field("kind", &self.kind)?;
        s.serialize_field("file_path", &self.file_path)?;
        s.serialize_field("line_start", &self.line_start)?;
        s.serialize_field("line_end", &self.line_end)?;
        if emit_rationale {
            s.serialize_field("rationale", &self.rationale)?;
        }
        s.end()
    }
}

impl<'de> Deserialize<'de> for Symbol {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct Wire {
            name: String,
            #[serde(default)]
            qualified_name: Option<String>,
            kind: SymbolKind,
            file_path: String,
            line_start: u32,
            line_end: u32,
            #[serde(default)]
            col_start: u32,
            #[serde(default)]
            col_end: u32,
            #[serde(default)]
            rationale: Vec<RationaleEntry>,
        }
        let w = Wire::deserialize(deserializer)?;
        Ok(Symbol {
            qualified_name: w.qualified_name.unwrap_or_else(|| w.name.clone()),
            name: w.name,
            kind: w.kind,
            file_path: w.file_path,
            line_start: w.line_start,
            line_end: w.line_end,
            col_start: w.col_start,
            col_end: w.col_end,
            rationale: w.rationale,
        })
    }
}

/// Marker class for a rationale comment. Lower-case JSON for stable
/// agent-facing strings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum RationaleKind {
    Why,
    Note,
    Important,
    Fixme,
    Hack,
    Xxx,
    Todo,
}

impl RationaleKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            RationaleKind::Why => "why",
            RationaleKind::Note => "note",
            RationaleKind::Important => "important",
            RationaleKind::Fixme => "fixme",
            RationaleKind::Hack => "hack",
            RationaleKind::Xxx => "xxx",
            RationaleKind::Todo => "todo",
        }
    }

    /// Recognize a marker keyword (case-insensitive). The trailing colon is
    /// not part of the input — callers split it off before calling.
    pub fn from_marker(marker: &str) -> Option<Self> {
        match marker.to_ascii_uppercase().as_str() {
            "WHY" => Some(RationaleKind::Why),
            "NOTE" => Some(RationaleKind::Note),
            "IMPORTANT" => Some(RationaleKind::Important),
            "FIXME" => Some(RationaleKind::Fixme),
            "HACK" => Some(RationaleKind::Hack),
            "XXX" => Some(RationaleKind::Xxx),
            "TODO" => Some(RationaleKind::Todo),
            _ => None,
        }
    }
}

/// A single rationale comment attached to a symbol. `text` has the marker
/// stripped and is trimmed; `line_start`/`line_end` are the comment's
/// span in the source file (1-based, matching `Symbol`'s convention).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RationaleEntry {
    pub kind: RationaleKind,
    pub text: String,
    pub line_start: u32,
    pub line_end: u32,
}

/// A trust-boundary tag attached to a file (or aggregated to a feature). One
/// tag per kind of capability the file actually exercises through its imports
/// or calls. Used by `assess_risk` to add a security-shaped term to the score
/// (a file talking to the network + reading secrets is meaningfully more
/// risky than one that only touches strings).
///
/// Derivation is heuristic: a per-language rule table maps known module/symbol
/// names to tags (e.g. Rust `reqwest::*` → [Network, ExternalApi]; PHP `exec`
/// → [ProcessExec]). False positives are possible (a file that imports
/// `reqwest` but only uses its types, not its client). For the risk signal,
/// boundary count is more important than perfect attribution.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    PartialOrd,
    Ord,
    Serialize,
    Deserialize,
    schemars::JsonSchema,
)]
#[serde(rename_all = "kebab-case")]
pub enum TrustBoundary {
    /// Crosses the network (HTTP clients, raw sockets, gRPC, etc.).
    Network,
    /// Reads or writes the filesystem beyond compile-time embedded data.
    Filesystem,
    /// Spawns or controls processes (exec, fork, child_process, popen, system).
    ProcessExec,
    /// Reads environment variables, credentials, secret stores, or interacts
    /// with cryptography primitives.
    Secrets,
    /// Talks to a database (SQL drivers, ORMs, key-value stores with auth).
    Database,
    /// Accepts user-controlled input directly (CLI argv, HTTP request bodies,
    /// stdin parsers).
    UserInput,
    /// Calls a third-party external API (a more specific Network signal —
    /// e.g. AWS SDK, Stripe, OpenAI client).
    ExternalApi,
    /// Performs serialization that crosses a trust boundary (XML, YAML
    /// loaders, pickle, deserialization of untrusted input).
    Serialization,
    /// Hand-rolled authentication / authorization paths (token validators,
    /// permission checks).
    Auth,
    /// Concurrency primitives that historically host data races (locks,
    /// atomics with non-trivial protocol, channels).
    Concurrency,
}

// `as_str` is the stable lowercase-kebab string used in DB rows, JSON, and CLI output.
str_enum!(TrustBoundary {
    Network => "network",
    Filesystem => "filesystem",
    ProcessExec => "process-exec",
    Secrets => "secrets",
    Database => "database",
    UserInput => "user-input",
    ExternalApi => "external-api",
    Serialization => "serialization",
    Auth => "auth",
    Concurrency => "concurrency",
});

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ReferenceKind {
    Import,
    Include,
    Call,
    Instantiation,
    Inheritance,
    #[serde(alias = "traituse")]
    TraitUse,
    #[serde(alias = "typehint")]
    TypeHint,
    /// Framework routing edge: a route declaration bound to its handler
    /// method (e.g. Laravel `Route::get('/x', [Ctrl::class, 'show'])`).
    /// Synthesized by the feature mapper, not the tree-sitter parser, so
    /// `impact_analysis`/`find_references` traverse routing.
    #[serde(alias = "routehandler")]
    RouteHandler,
    /// The binding an import introduces, as opposed to the module it names:
    /// `import Foo from './foo.js'` records `./foo.js` as an `Import` and
    /// `Foo` as an `ImportBinding`. Kept distinct so file-level dependency
    /// listings keep showing modules while symbol lookups can still reach a
    /// file that imports a symbol and only uses it in a form no call or
    /// instantiation pattern captures.
    #[serde(alias = "importbinding")]
    ImportBinding,
}

str_enum!(ReferenceKind {
    Import => "import",
    Include => "include",
    Call => "call",
    Instantiation => "instantiation",
    Inheritance => "inheritance",
    TraitUse => "trait_use",
    TypeHint => "type_hint",
    RouteHandler => "route_handler",
    ImportBinding => "import_binding",
});

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Reference {
    pub from_file: String,
    pub from_symbol: Option<String>,
    pub to_name: String,
    pub kind: ReferenceKind,
    pub line: u32,
    /// Byte column of the reference. Kept in SQLite but never emitted over
    /// JSON; `line` is what agents navigate by.
    #[serde(skip)]
    pub col: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct FindSymbolRequest {
    pub name: String,
    pub kind: Option<SymbolKind>,
}

/// One near-clone of a queried function, scored by MinHash Jaccard similarity
/// over AST structure. Returned by `find_similar`.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SimilarSymbol {
    pub name: String,
    pub file_path: String,
    pub line_start: u32,
    pub line_end: u32,
    pub kind: String,
    /// Estimated Jaccard similarity in [0, 1]; 1.0 is a structurally identical
    /// body (identifiers and literals are ignored).
    pub jaccard: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct FindReferencesRequest {
    pub symbol_name: String,
    pub kind: Option<ReferenceKind>,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct DependencyEntry {
    pub file_path: String,
    #[serde(default)]
    pub found: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    pub imports: Vec<String>,
    pub imported_by: Vec<String>,
}

/// What a repository contains that indexing will not see.
///
/// Answers "what didn't I index", which neither `IndexStats` counter does:
/// `files_skipped` counts files unchanged since the last pass (freshness, not
/// coverage), and `files_failed` counts parse errors on files that were at
/// least recognized. Files whose extension maps to no supported language are
/// dropped at discovery and never reach either counter, so the largest
/// coverage gap is the one nothing reports.
#[derive(Debug, Default, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CoverageSurvey {
    /// Files that WOULD be indexed, per language.
    pub covered_by_language: std::collections::BTreeMap<String, usize>,
    /// Files skipped because no supported language matched, per extension.
    /// Extensionless files are keyed as `<none>`.
    pub uncovered_by_extension: std::collections::BTreeMap<String, usize>,
    /// Files matching a configured exclude pattern. Deliberate, not a gap.
    pub excluded: usize,
    /// Recognized language, but over the indexer's size cap.
    pub oversized: usize,
    /// Recognized language, but the indexer could not open the file.
    pub unreadable: usize,
    /// Files in a supported language that gitignore hides from indexing.
    /// Usually intentional; the most common answer to "why isn't this indexed".
    pub gitignored_source: usize,
    /// Directories the walk could not traverse. Non-zero means the numbers
    /// below describe an incomplete tree.
    pub walk_errors: usize,
    pub covered_total: usize,
    pub uncovered_total: usize,
}

impl CoverageSurvey {
    /// Share of walked, non-excluded files that indexing can see, 0.0..=1.0.
    /// Returns 1.0 for an empty repo rather than dividing by zero.
    pub fn covered_fraction(&self) -> f64 {
        let total = self.covered_total + self.uncovered_total;
        if total == 0 {
            return 1.0;
        }
        self.covered_total as f64 / total as f64
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct IndexStats {
    pub files_indexed: usize,
    pub files_skipped: usize,
    #[serde(default)]
    pub files_failed: usize,
    pub files_removed: usize,
    pub symbols_found: usize,
    pub references_found: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Chunk {
    pub text: String,
    pub start_line: u32,
    pub end_line: u32,
    pub start_byte: usize,
    pub end_byte: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SearchRequest {
    pub query: String,
    pub limit: Option<usize>,
    pub offset: Option<usize>,
    pub languages: Option<Vec<Language>>,
    pub paths: Option<Vec<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SearchResult {
    pub file_path: String,
    pub language: Language,
    pub content: String,
    pub start_line: u32,
    pub end_line: u32,
    pub score: f32,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub symbols: Vec<SymbolSummary>,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SymbolSummary {
    pub name: String,
    pub qualified_name: String,
    pub kind: SymbolKind,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SemanticIndexStats {
    pub files_processed: usize,
    pub files_skipped: usize,
    #[serde(default)]
    pub files_failed: usize,
    pub files_removed: usize,
    /// Chunks written for the processed files, embedded or reused.
    pub chunks_created: usize,
    /// Chunks whose text already had a vector in the index, so no model call
    /// was made for them. `chunks_created - chunks_reused` is the number of
    /// texts actually embedded this pass.
    #[serde(default)]
    pub chunks_reused: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum FileCategory {
    Source,
    Test,
    Config,
}

impl FileCategory {
    pub fn classify(path: &str) -> Self {
        let lower_owned = path.to_lowercase();
        // Strip a leading `./` so relative paths (`./tests/foo.rs`) still match
        // the directory-prefix checks below.
        let lower = lower_owned
            .strip_prefix("./")
            .unwrap_or(lower_owned.as_str());
        let has_dir = |seg: &str| {
            lower.contains(&format!("/{seg}/")) || lower.starts_with(&format!("{seg}/"))
        };
        if has_dir("test")
            || has_dir("tests")
            || has_dir("__tests__")
            || has_dir("spec")
            || lower.ends_with(".test.ts")
            || lower.ends_with(".test.tsx")
            || lower.ends_with(".test.js")
            || lower.ends_with(".test.jsx")
            || lower.ends_with(".spec.ts")
            || lower.ends_with(".spec.tsx")
            || lower.ends_with(".spec.js")
            || lower.ends_with(".spec.jsx")
            // Java / PHPUnit conventions require an uppercase `Test`
            // boundary (`FooTest.java`, `FooTests.java`, `FooTest.php`).
            // Matching against the lowercased path here would also catch
            // unrelated source files like `Latest.java`, `Manifests.java`,
            // or `latest.php`.
            || path.ends_with("Test.php")
            || path.ends_with("Test.java")
            || path.ends_with("Tests.java")
            || lower.ends_with("_test.py")
            || lower.ends_with("_test.go")
            || lower.ends_with(".phpt")
        {
            return FileCategory::Test;
        }
        let basename = lower.rsplit('/').next().unwrap_or(lower);
        if basename.starts_with("test_") {
            return FileCategory::Test;
        }
        if basename.ends_with(".toml")
            || basename.ends_with(".yaml")
            || basename.ends_with(".yml")
            || basename.ends_with(".json")
            || basename.ends_with(".ini")
            || basename.ends_with(".env")
            || basename == ".env"
            || basename.ends_with(".conf")
        {
            return FileCategory::Config;
        }
        FileCategory::Source
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ImpactTarget {
    Symbol { name: String },
    File { path: String },
}

impl ImpactTarget {
    /// Build from a user-supplied hint. `is_file=Some(true|false)` honors the explicit flag;
    /// `None` falls back to a conservative path heuristic. Dotted method symbols are common
    /// in Python/Go/JS, so a bare `.` is not enough to classify a target as a file.
    /// Callers with a CLI-style bool flag should pass `Some(true)` only when the user set it,
    /// else `None` (so an unset-false doesn't force a Symbol classification).
    pub fn from_hint(target: String, is_file: Option<bool>) -> Self {
        let looks_like_file = is_file.unwrap_or_else(|| looks_like_file_target(&target));
        if looks_like_file {
            ImpactTarget::File { path: target }
        } else {
            ImpactTarget::Symbol { name: target }
        }
    }
}

fn looks_like_file_target(target: &str) -> bool {
    if target.contains('/') {
        return true;
    }

    let Some((_, ext)) = target.rsplit_once('.') else {
        return false;
    };
    let ext = ext.to_ascii_lowercase();
    matches!(
        ext.as_str(),
        "c" | "cc"
            | "cpp"
            | "cxx"
            | "h"
            | "hh"
            | "hpp"
            | "hxx"
            | "java"
            | "go"
            | "js"
            | "jsx"
            | "mjs"
            | "cjs"
            | "ts"
            | "tsx"
            | "php"
            | "py"
            | "rs"
            | "toml"
            | "json"
            | "yaml"
            | "yml"
            | "ini"
            | "conf"
            | "env"
            | "md"
    )
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ImpactRequest {
    pub target: ImpactTarget,
    #[serde(default = "default_impact_depth")]
    pub depth: usize,
    #[serde(default)]
    pub source_only: bool,
}

fn default_impact_depth() -> usize {
    2
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ImpactReason {
    pub via_symbol: String,
    pub kind: ReferenceKind,
    pub line: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ImpactEntry {
    pub file_path: String,
    pub distance: u32,
    pub category: FileCategory,
    pub reasons: Vec<ImpactReason>,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ExportRequest {
    pub query: Option<String>,
    pub symbol: Option<String>,
    #[serde(default = "default_export_limit")]
    pub limit: usize,
    #[serde(default)]
    pub include_callers: bool,
    #[serde(default)]
    pub include_callees: bool,
}

impl ExportRequest {
    /// Build from a user-supplied target + is_symbol toggle. Centralizes the
    /// "exactly one of query/symbol" invariant so CLI and MCP can't drift.
    pub fn from_target(
        target: String,
        is_symbol: bool,
        limit: usize,
        include_callers: bool,
        include_callees: bool,
    ) -> Self {
        if is_symbol {
            Self {
                query: None,
                symbol: Some(target),
                limit,
                include_callers,
                include_callees,
            }
        } else {
            Self {
                query: Some(target),
                symbol: None,
                limit,
                include_callers,
                include_callees,
            }
        }
    }
}

fn default_export_limit() -> usize {
    5
}

fn default_found() -> bool {
    true
}

fn is_false(value: &bool) -> bool {
    !*value
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ContextBundle {
    /// False when the requested target (symbol or feature_id) does not
    /// exist in the index; the bundle is empty in that case.
    #[serde(default = "default_found")]
    pub found: bool,
    pub target_description: String,
    pub primary: Vec<SearchResult>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub related: Vec<SearchResult>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub symbol_definitions: Vec<Symbol>,
}

/// One co-changing file pair, ranked by exponentially-decayed weight.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CoChangeEntry {
    pub file: String,
    pub weight: f64,
    pub count: u32,
    pub last_observed_at: Option<i64>,
}

/// Result envelope for `find_coupling`. Wraps the ranked list with enough
/// context for an agent to tell apart the three empty-result causes:
///
/// - file never indexed (not tracked, or no commits yet) — `file_indexed=false`
/// - file has history but no co-change pair above the min-count threshold —
///   `file_indexed=true, file_commits>0, coupled=[]`, note explains
/// - file-path shape doesn't match the index (wrong case, leading slash,
///   etc.) — typically surfaces as `file_indexed=false` with a note suggesting
///   the caller verify the path
///
/// Non-empty `coupled` responses still include the indexed-state fields so an
/// agent can distinguish a thin result (`coupled.len() < limit`) from a full
/// one.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CouplingReport {
    /// False when the requested file does not exist in the git-history
    /// index (mirrors `file_indexed`; structured replacement for
    /// substring-matching the `note` prose).
    #[serde(default = "default_found")]
    pub found: bool,
    pub coupled: Vec<CoChangeEntry>,
    /// True when the file has at least one row in `git_files`.
    pub file_indexed: bool,
    /// Total commits tracked for the file. 0 when not indexed.
    pub file_commits: u32,
    /// Human-readable hint when `coupled` is empty; `None` otherwise.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// One symbol inside a file that contributes to its risk score, ranked by
/// the heuristic `ln(1 + line_count) + ref_count + (in_cycle ? 1.0 : 0.0)`.
/// The `why` string is a one-line human-readable explanation the agent can
/// quote in a PR description (e.g. `"hot: 142 lines, 38 refs, in 7-file cycle"`).
/// Capped at five entries per file in the producing pipeline so a heavy file
/// doesn't drown the response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TopSymbol {
    pub name: String,
    pub line: u32,
    /// Lowercase symbol kind ("function", "class", "struct", …). Kept as a
    /// raw string rather than the typed `SymbolKind` enum so consumers can
    /// pattern-match on it without depending on the protocol enum.
    pub kind: String,
    pub why: String,
}

/// Risk decomposition for a file. Score is the weighted sum; components let the agent
/// see WHY a file is risky, not just the magnitude.
///
/// The engine always fills every field. What reaches the wire depends on
/// [`RiskAssessment::verbose`]: the per-signal scalars (`churn_score` through
/// `cycle_size`) and `top_coupled` are emitted only when it is true. The CLI
/// keeps it true; the MCP tools clear it unless the caller passed
/// `verbose: true`, because agents branch on `score`, `notes`,
/// `trust_boundaries`, and `top_symbols` and the rest was measured at ~48% of
/// an `assess_risk_batch` payload. `cycle_files` is NOT gated: the `notes[]`
/// cycle lines name its members, and the staleness scan discovers those
/// members only through the serialized field. `Serialize` is hand-written
/// below to apply the switch; the `#[serde]` attributes here drive
/// `Deserialize` and the `JsonSchema` derive.
#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct RiskAssessment {
    /// False when the requested file does not exist in the structural or
    /// git-history index. A missing path is unknown, not low-risk.
    #[serde(default = "default_found")]
    pub found: bool,
    pub file: String,
    pub score: f64,
    /// Wire switch, not a measurement: when false the decomposition scalars
    /// and `top_coupled` are left out of the serialized form. Never
    /// serialized itself.
    #[serde(skip, default = "default_found")]
    pub verbose: bool,
    /// Present only when the caller asked for `verbose` output.
    #[serde(default)]
    pub churn_score: f64,
    /// Present only when the caller asked for `verbose` output.
    #[serde(default)]
    pub churn_percentile: f64,
    /// Present only when the caller asked for `verbose` output.
    #[serde(default)]
    pub fix_ratio: f64,
    /// Present only when the caller asked for `verbose` output.
    #[serde(default)]
    pub total_commits: u32,
    /// Present only when the caller asked for `verbose` output.
    #[serde(default)]
    pub fix_count: u32,
    /// Present only when the caller asked for `verbose` output.
    #[serde(default)]
    pub dependent_files: u32,
    /// Present only when the caller asked for `verbose` output.
    #[serde(default)]
    pub coupled_files: u32,
    /// Present only when the caller asked for `verbose` output.
    #[serde(default)]
    pub test_gap: bool,
    /// True when the file participates in a non-trivial import cycle (SCC of
    /// size ≥ 2 in the file-level import graph). Cycle membership is a strong
    /// structural signal that a change here can ripple unexpectedly through
    /// the cycle's other members; included as a 0.10-weighted input to `score`.
    /// Present only when the caller asked for `verbose` output.
    #[serde(default)]
    pub in_cycle: bool,
    /// Number of files in the SCC this file belongs to (including this file).
    /// Zero when `in_cycle` is false. Larger cycles get a larger contribution
    /// to `score`, capped at size 5. Present only when the caller asked for
    /// `verbose` output.
    #[serde(default)]
    pub cycle_size: u32,
    /// Other members of the import cycle (excluding this file), so the agent
    /// can name them in PR descriptions or follow up to assess whether the
    /// cycle should be broken. Empty when `in_cycle` is false. Not gated by
    /// `verbose`: the `notes[]` cycle lines name these files, and the
    /// response staleness scan reaches them only through this field.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub cycle_files: Vec<String>,
    /// Top co-changers, useful for the agent to know which tests/files to also
    /// touch. Present only when the caller asked for `verbose` output; use
    /// `find_coupling` or `recommend_tests` for the same data on demand.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub top_coupled: Vec<CoChangeEntry>,
    /// Trust-boundary tags derived from this file's imports/includes/calls.
    /// Each tag denotes a capability the file's structural dependencies imply
    /// it exercises (network, filesystem, secrets, process-exec, etc.).
    /// Contributes a `0.10 * min(count/5, 1.0)` term to `score`. Sorted by
    /// enum discriminant; empty when the file matches no boundary rule or
    /// has never been derived (run `codesage index` to populate).
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub trust_boundaries: Vec<TrustBoundary>,
    /// Human-readable rationale lines so the agent can quote them in PR descriptions.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub notes: Vec<String>,
    /// Top symbols inside this file ranked by a heuristic that blends symbol
    /// length, reference count, and cycle membership. Answers the agent
    /// follow-up "the file scored high — which symbols inside it drive that?"
    /// in one round-trip. Capped at 5. Empty when the file has no indexed
    /// symbols (text files, generated files, files not yet indexed) — omitted
    /// from JSON in that case so older agents see no schema churn.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub top_symbols: Vec<TopSymbol>,
}

impl RiskAssessment {
    /// Flip the wire verbosity switch; see the struct docs for what it hides.
    pub fn set_verbose(&mut self, verbose: bool) {
        self.verbose = verbose;
    }
}

impl Serialize for RiskAssessment {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let emit_cycle_files = !self.cycle_files.is_empty();
        let emit_top_coupled = self.verbose && !self.top_coupled.is_empty();
        let emit_trust = !self.trust_boundaries.is_empty();
        let emit_notes = !self.notes.is_empty();
        let emit_top_symbols = !self.top_symbols.is_empty();
        let len = 3
            + if self.verbose { 10 } else { 0 }
            + usize::from(emit_cycle_files)
            + usize::from(emit_top_coupled)
            + usize::from(emit_trust)
            + usize::from(emit_notes)
            + usize::from(emit_top_symbols);
        let mut s = serializer.serialize_struct("RiskAssessment", len)?;
        s.serialize_field("found", &self.found)?;
        s.serialize_field("file", &self.file)?;
        s.serialize_field("score", &self.score)?;
        if self.verbose {
            s.serialize_field("churn_score", &self.churn_score)?;
            s.serialize_field("churn_percentile", &self.churn_percentile)?;
            s.serialize_field("fix_ratio", &self.fix_ratio)?;
            s.serialize_field("total_commits", &self.total_commits)?;
            s.serialize_field("fix_count", &self.fix_count)?;
            s.serialize_field("dependent_files", &self.dependent_files)?;
            s.serialize_field("coupled_files", &self.coupled_files)?;
            s.serialize_field("test_gap", &self.test_gap)?;
            s.serialize_field("in_cycle", &self.in_cycle)?;
            s.serialize_field("cycle_size", &self.cycle_size)?;
        }
        if emit_cycle_files {
            s.serialize_field("cycle_files", &self.cycle_files)?;
        }
        if emit_top_coupled {
            s.serialize_field("top_coupled", &self.top_coupled)?;
        }
        if emit_trust {
            s.serialize_field("trust_boundaries", &self.trust_boundaries)?;
        }
        if emit_notes {
            s.serialize_field("notes", &self.notes)?;
        }
        if emit_top_symbols {
            s.serialize_field("top_symbols", &self.top_symbols)?;
        }
        s.end()
    }
}

/// Aggregate risk for a set of files (typically the file list of a patch or PR).
/// Lets an agent ask one question — "how risky is this change?" — instead of
/// per-file round-trips and manual aggregation.
#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RiskDiffAssessment {
    /// True when the caller supplied no files. Empty input is a usage signal,
    /// not a clean low-risk patch.
    #[serde(default, skip_serializing_if = "is_false")]
    pub empty_input: bool,
    /// Per-file decomposition. Same shape as a single `assess_risk` call.
    pub files: Vec<RiskAssessment>,
    /// Highest score across the patch. The signal that should drive the agent's
    /// caution: split the patch, add tests, request review.
    pub max_score: f64,
    pub mean_score: f64,
    /// File contributing `max_score`. None when the patch is empty.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_risk_file: Option<String>,
    /// Files with `test_gap == true`. Adding tests for these closes the most
    /// common reviewer concern.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub test_gap_files: Vec<String>,
    /// Files with `dependent_files >= 10` (depth-2). Wide blast radius.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub wide_blast_files: Vec<String>,
    /// Files with `fix_ratio >= 0.4 && total_commits >= 5`. Historically buggy.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub fix_heavy_files: Vec<String>,
    /// Files with `churn_percentile >= 0.75`. Pain points.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub hotspot_files: Vec<String>,
    /// Aggregate notes the agent can paste verbatim into a PR description.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub summary_notes: Vec<String>,
    /// When a patch touches 5+ files from a single directory, the per-file
    /// entries for that directory move out of `files` into one cluster here —
    /// keeping the top-3 by score fully detailed and listing the rest by name
    /// only. Rollup arrays (`test_gap_files`, `wide_blast_files`, etc.) still
    /// include every clustered file, so no information is lost.
    ///
    /// Empty when no directory hits the threshold (small patches keep the
    /// original shape, agent prompts written against the old schema keep
    /// working without changes).
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub clustered_directories: Vec<ClusteredDirectory>,
    /// Strongly-connected components in the file-level import graph that
    /// include at least one file from the patch. See [`CycleEntry`] docs
    /// for the "touches a cycle vs. introduces a cycle" distinction.
    /// Empty when no cycle overlaps the patch file set.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub cycles_touching_patch: Vec<CycleEntry>,
    /// Short-code legend for repeated categorical notes. When a note
    /// string repeats verbatim in ≥3 files of a single response, the
    /// per-file `notes[]` entries are replaced with a short code
    /// (e.g. `"T"`, `"NG"`); this map resolves each code back to its
    /// full string. Saves bytes on patches that touch many files in
    /// similar states (e.g. a refactor where every touched file lacks
    /// a co-located test). Templated notes (`"hotspot: churn 80%"`,
    /// `"in import cycle of 4 files: …"`) are not aliased — only
    /// non-templated categorical notes are eligible. Empty when no
    /// note repeated enough to trigger aliasing.
    #[serde(
        skip_serializing_if = "BTreeMap::is_empty",
        default,
        rename = "_legend"
    )]
    pub legend: BTreeMap<String, String>,
}

impl RiskDiffAssessment {
    /// Apply [`RiskAssessment::set_verbose`] to every per-file entry,
    /// including the detailed `top_files` kept inside directory clusters.
    pub fn set_verbose(&mut self, verbose: bool) {
        for f in &mut self.files {
            f.set_verbose(verbose);
        }
        for cluster in &mut self.clustered_directories {
            for f in &mut cluster.top_files {
                f.set_verbose(verbose);
            }
        }
    }
}

/// Result of `assess_risk_batch`: per-file decomposition for a list of
/// files, no patch-level aggregation. Use when the agent has a list of
/// files (e.g. from impact analysis or coupling) and wants individual
/// risk scores for each in one round-trip — avoids the per-file MCP
/// protocol overhead that retrospective session analysis showed
/// dominates `assess_risk` call volume.
///
/// Differs from [`RiskDiffAssessment`] in that it does *not* compute
/// max/mean across the set, rollup arrays, summary notes, cycles, or
/// directory clustering — those are patch-aggregate concerns. If the
/// agent wants "is this patch risky as a whole?", use `assess_risk_diff`
/// instead. If it wants "give me each of these files' scores", use
/// `assess_risk_batch`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RiskBatchAssessment {
    /// Per-file decomposition, in the order of the request's `file_paths`.
    /// One [`RiskAssessment`] per input path. Ordering preserved so the
    /// agent can zip with its own list.
    pub files: Vec<RiskAssessment>,
    /// Same shape and semantics as [`RiskDiffAssessment::legend`]: short
    /// codes for categorical notes that repeated in ≥3 files. Empty when
    /// no aliasing fired.
    #[serde(
        skip_serializing_if = "BTreeMap::is_empty",
        default,
        rename = "_legend"
    )]
    pub legend: BTreeMap<String, String>,
}

impl RiskBatchAssessment {
    /// Apply [`RiskAssessment::set_verbose`] to every per-file entry.
    pub fn set_verbose(&mut self, verbose: bool) {
        for f in &mut self.files {
            f.set_verbose(verbose);
        }
    }
}

/// A strongly-connected component in the file-level import graph that
/// contains at least one file from a patch. Reported by `assess_risk_diff`
/// as `cycles_touching_patch` when any member of the patch participates
/// in a cycle. Members are mutually reachable through `import`,
/// `include`, `inheritance`, or `trait_use` references — i.e. they can't
/// be compiled / type-checked / loaded independently. `max_churn_file`
/// is a heuristic pointer at the best refactor target: the historically
/// most-modified file usually accumulates the most cross-cutting
/// dependencies and extracting it tends to break the cycle.
///
/// Honest caveat: we do not compute "cycles newly introduced by the
/// patch". We report cycles that *include* a patch file, some of which
/// are pre-existing. Agents should frame PR guidance as "this patch
/// touches an existing cycle" unless they can confirm the cycle didn't
/// exist on the base branch.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CycleEntry {
    pub members: Vec<String>,
    pub size: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_churn_file: Option<String>,
}

/// A directory that contributed ≥5 files to a patch. The top-3 files by
/// risk score are detailed; the rest are listed by name.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ClusteredDirectory {
    pub directory: String,
    pub count: u32,
    pub top_files: Vec<RiskAssessment>,
    /// Files in this directory whose detail was omitted. Cross-reference
    /// against the top-level rollups to see which ones trigger concerns.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub omitted_files: Vec<String>,
}

/// A test file recommended for a change, with the reason it was suggested.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CoupledTestEntry {
    pub file: String,
    pub weight: f64,
    pub count: u32,
    /// Which file in the changed set this test couples with. Lets the agent
    /// explain "I ran X.test.ts because it co-changes with X.ts (8 times)".
    pub source: String,
}

/// Tests an agent should run after editing a set of files. Splits into
/// sibling-convention matches (high confidence) and historical co-change
/// (medium confidence; surfaces tests that other test heuristics miss).
#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TestRecommendations {
    /// Sibling tests resolved by language conventions (FooTest.php,
    /// foo.test.ts, test_foo.py, foo_test.go). Always run these.
    pub primary: Vec<String>,
    /// Tests that historically change with one of the input files. Worth
    /// running when sibling tests don't exist or when behavior crosses
    /// component boundaries.
    pub coupled: Vec<CoupledTestEntry>,
    /// Human-readable rationale.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub notes: Vec<String>,
}

/// One file in `SessionSnapshot.top_risk_files`. Captured at session start
/// so `session_end` can compute per-file risk-score deltas without needing
/// to re-derive the snapshot's risk inputs.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SessionRiskEntry {
    pub file: String,
    pub score: f64,
}

/// Snapshot of repo structural state captured at `session_start`. Persisted
/// as JSON under `.codesage/sessions/<session_id>.json` and consumed by the
/// matching `session_end` call to compute a `SessionDiff`. The snapshot is
/// intentionally compact (no per-symbol detail) so even large monorepos
/// produce ≤ a few MB per session.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SessionSnapshot {
    pub session_id: String,
    /// Unix epoch seconds.
    pub created_at: i64,
    pub file_count: u32,
    pub symbol_count: u32,
    /// Sorted full list of indexed file paths. Required to detect new /
    /// removed files in the diff. On a 10k-file repo this serializes to
    /// ~500 KB; sessions GC is deferred to a follow-up.
    pub files: Vec<String>,
    /// Cycles in the file-level import graph. Each inner Vec is sorted;
    /// the outer Vec is sorted by (descending size, members) for stable
    /// equality across snapshot/recompute cycles.
    pub cycles: Vec<Vec<String>>,
    /// Top-N highest-risk files at snapshot time. Used as the baseline set
    /// for `risk_regressions` in the diff. Files outside this set don't
    /// get a per-file risk delta even if their risk goes up — keeps the
    /// snapshot bounded on big repos.
    pub top_risk_files: Vec<SessionRiskEntry>,
    /// Best-effort `git rev-parse HEAD` at snapshot time. None when not in
    /// a git repo or git isn't on PATH.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub git_head: Option<String>,
}

/// Compact MCP response for `session_start`. The full snapshot is still
/// persisted on disk as `SessionSnapshot`; the tool response deliberately
/// avoids returning the full file list so MCP budget truncation cannot make
/// the advertised shape ambiguous.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SessionStartReport {
    pub session_id: String,
    pub created_at: i64,
    pub file_count: u32,
    pub symbol_count: u32,
    pub cycle_count: usize,
    pub top_risk_file_count: usize,
    pub snapshot_path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub git_head: Option<String>,
}

/// Per-file risk regression observed between `session_start` and `session_end`.
/// Only emitted for files that appeared in `SessionSnapshot.top_risk_files`
/// (the baseline set) and whose risk score went up by at least 0.05.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SessionRiskRegression {
    pub file: String,
    pub before: f64,
    pub after: f64,
    pub delta: f64,
}

/// Diff between a `SessionSnapshot` and the current state of the repo,
/// returned by `session_end`. The headline `pass` flag closes the loop: an
/// agent that calls session_start before edits and session_end after sees
/// `pass=false` when its work introduced cycles or regressed top-risk
/// files materially.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SessionDiff {
    pub session_id: String,
    /// Wall-clock seconds between snapshot creation and diff computation.
    pub duration_seconds: i64,
    /// Pass when no new cycles were introduced AND no top-risk file
    /// regressed by ≥ 0.10. Conservative; tune after running on real
    /// session traces.
    pub pass: bool,
    pub file_count_before: u32,
    pub file_count_after: u32,
    pub symbol_count_before: u32,
    pub symbol_count_after: u32,
    /// Files indexed at session_end that were not in the snapshot.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub new_files: Vec<String>,
    /// Files in the snapshot that are not in the index at session_end.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub removed_files: Vec<String>,
    /// Cycles that exist now and didn't at snapshot time. Each inner Vec
    /// is the sorted member list. Single new cycle is enough to fail.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub new_cycles: Vec<Vec<String>>,
    /// Cycles that existed at snapshot time and don't now (broken by
    /// the agent's edits). Reported for completeness; doesn't affect pass.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub resolved_cycles: Vec<Vec<String>>,
    /// Per-file risk-score regressions (delta ≥ 0.05) for files in the
    /// snapshot's top-risk baseline.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub risk_regressions: Vec<SessionRiskRegression>,
    /// Largest delta in `risk_regressions`. Zero when none.
    pub max_risk_regression: f64,
    /// Aggregate notes the agent can paste into a PR description or a
    /// session-end report.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub summary_notes: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub git_head_before: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub git_head_after: Option<String>,
}

/// Stats from a git history indexing pass. Mirrors IndexStats shape.
#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct GitIndexStats {
    pub commits_scanned: usize,
    pub files_tracked: usize,
    pub co_change_pairs: usize,
}

/// The "shape" of a feature slice. Maps to the agent-facing reason the
/// feature exists — what someone would call the entrypoint when describing
/// what it does.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    PartialOrd,
    Ord,
    Serialize,
    Deserialize,
    schemars::JsonSchema,
)]
#[serde(rename_all = "kebab-case")]
pub enum FeatureKind {
    /// Executable command (CLI tool, `main()`, package bin). Run-it shape.
    CliCommand,
    /// HTTP/RPC route. Request-response shape.
    Route,
    /// Long-running service or daemon.
    Service,
    /// Library / module surface (no top-level entrypoint, importable API).
    Library,
    /// Test target (test suite, integration tests, .phpt fixture set).
    TestSuite,
    /// Build/release/config artifact (Cargo.toml, composer.json, CMakeLists.txt).
    Config,
    /// Background job / queue worker / scheduled task.
    Job,
    /// Catch-all for shapes the mapper can't classify confidently.
    Unknown,
}

str_enum!(FeatureKind {
    CliCommand => "cli-command",
    Route => "route",
    Service => "service",
    Library => "library",
    TestSuite => "test-suite",
    Config => "config",
    Job => "job",
    Unknown => "unknown",
});

/// Confidence that the mapper got this feature right. `High` means the
/// signal is unambiguous (a `[[bin]]` in Cargo.toml, a `composer.json`
/// `bin` entry). `Medium` is heuristic (a top-level `main.go`). `Low` is
/// best-effort (a directory pattern match).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum FeatureConfidence {
    High,
    Medium,
    Low,
}

str_enum!(FeatureConfidence {
    High => "high",
    Medium => "medium",
    Low => "low",
});

/// Role of a file within a feature.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    PartialOrd,
    Ord,
    Serialize,
    Deserialize,
    schemars::JsonSchema,
)]
#[serde(rename_all = "lowercase")]
pub enum FeatureFileRole {
    /// The entrypoint file itself (always exactly one).
    Entry,
    /// Files directly implementing the feature.
    Owned,
    /// Supporting files: imports, shared helpers, configs the feature reads.
    Context,
    /// Test files associated with this feature.
    Test,
}

str_enum!(FeatureFileRole {
    Entry => "entry",
    Owned => "owned",
    Context => "context",
    Test => "test",
});

/// A file attached to a feature with its role and optional reason. `reason`
/// is a short human-readable note ("entrypoint", "nearby test", "imported
/// by entry") the mapper recorded when including this file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct FeatureFileRef {
    pub path: String,
    pub role: FeatureFileRole,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// One behavior-keyed slice of the repository. Produced deterministically by
/// per-language mappers (no LLM in the seed path). The `feature_id` is
/// stable across re-runs as long as the entrypoint and kind don't change,
/// so an agent can quote it in conversation and have the same record
/// surface in the next session.
///
/// Designed as a *retrieval surface*, not a workflow object: `find_feature`
/// answers "what feature owns this file?", `feature_bundle` returns the
/// curated context an agent would want before reviewing or modifying the
/// slice. Mapping is deliberately conservative — a file may appear in
/// multiple features (a shared helper imported by two routes) and that's
/// fine; the goal is recall, not partition.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct FeatureRecord {
    /// Stable blake3-derived id. Format: `feat_<16-hex>`. Computed from
    /// `(kind, source, entry_path, command|route|symbol)` so renaming an
    /// entry file regenerates the id, but the same file producing the
    /// same feature across re-runs keeps it.
    pub feature_id: String,
    /// Human-readable title ("Rust binary `codesage`", "PHP route `GET /login`").
    pub title: String,
    /// One-line summary suitable for listing.
    pub summary: String,
    pub kind: FeatureKind,
    /// Mapper-defined source token: `cargo-bin`, `composer-bin`,
    /// `laravel-route`, `php-ext`, `c-main`, `cmake-target`, etc. Lets the
    /// caller filter by detection source without parsing the title.
    pub source: String,
    pub confidence: FeatureConfidence,
    pub entry_path: String,
    /// Symbol the entrypoint anchors on (Rust `main`, C `main`, Python
    /// `if __name__`, etc.). `None` for features without a code-level
    /// entry symbol (config files, route registrations).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entry_symbol: Option<String>,
    /// HTTP route, queue topic, or URL template (Laravel `GET /users/{id}`,
    /// Express `/api/login`). `None` for non-route features.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entry_route: Option<String>,
    /// CLI command/subcommand name (`codesage`, `composer install`, the
    /// argv[0]-shape token). `None` for non-CLI features.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entry_command: Option<String>,
    /// Full shell command an agent should run to exercise this feature's
    /// tests (e.g. `pnpm --dir packages/api test`, `go test ./pkg/util/...`,
    /// `uv run pytest`). `None` when no test runner is detectable for the
    /// feature's language/manifest. Distinct from `entry_command`: that's
    /// an argv[0]-shape token used in feature-ID hashing; this is a free-
    /// form shell command that may change as the project's test config
    /// evolves without affecting feature identity.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub test_command: Option<String>,
    pub language: Language,
    /// Free-form taxonomy tags the mapper attached (`["rust", "cli"]`,
    /// `["php", "framework:laravel"]`). Sorted, deduped.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub tags: Vec<String>,
    /// Trust boundaries aggregated across the feature's owned files. Same
    /// shape as `RiskAssessment.trust_boundaries`; sorted dedupe.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub trust_boundaries: Vec<TrustBoundary>,
    /// Files attached to the feature with role classification. Always
    /// non-empty (contains at least the entry).
    pub files: Vec<FeatureFileRef>,
}

/// Envelope around `Vec<FeatureRecord>` for the `list_features` and
/// `find_feature` MCP tools — same `{"results": [...]}` shape
/// `render_with_kind` produces.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct FeatureListResults {
    pub results: Vec<FeatureRecord>,
}

/// Returned by `codesage map`. Counts mirror the prior indexing-stats shape.
#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct FeatureMapStats {
    pub created: usize,
    pub updated: usize,
    pub removed: usize,
    pub total_features: usize,
}

/// `{"results": [...]}` envelope around `Vec<Symbol>`. Exists only to back the
/// MCP `outputSchema` for `find_symbol`. The MCP server wraps bare-array
/// responses into this shape in `render_with_kind` (see commit `dc66de6`);
/// the schema simply describes what the agent actually receives.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct FindSymbolResults {
    pub results: Vec<Symbol>,
}

/// `{"results": [...]}` envelope around `Vec<Reference>`. See [`FindSymbolResults`].
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct FindReferencesResults {
    pub results: Vec<Reference>,
}

/// `{"results": [...]}` envelope around `Vec<SearchResult>`. See [`FindSymbolResults`].
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SearchResults {
    pub results: Vec<SearchResult>,
}

/// `{"results": [...]}` envelope around `Vec<SimilarSymbol>`. See [`FindSymbolResults`].
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct FindSimilarResults {
    pub results: Vec<SimilarSymbol>,
}

/// `{"results": [...]}` envelope around `Vec<ImpactEntry>`. See [`FindSymbolResults`].
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ImpactAnalysisResults {
    pub results: Vec<ImpactEntry>,
}

/// Optional controls for the richer `impact_analysis` report. All default off,
/// so the report reduces to the classic reverse-impact list.
#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ImpactOptions {
    /// Also include the target's forward dependencies — the import targets
    /// (modules/symbols) its file imports.
    pub include_forward: bool,
    /// Also include the symbols defined alongside the target in the same file.
    pub include_siblings: bool,
    /// Cap the reverse-impact `results` list to this many entries.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub limit: Option<usize>,
    /// Drop per-reason detail (keep one exemplar reason per file) and attach a
    /// `summary` rollup — keeps the response small on wide blast radii.
    pub summary_only: bool,
}

/// A symbol defined in the same file as the impact target. Signature-level
/// (name + kind + line, no body) so a dense file collapses to a compact list.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SiblingSymbol {
    pub name: String,
    pub kind: SymbolKind,
    pub line: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct DistanceCount {
    pub distance: u32,
    pub count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CategoryCount {
    pub category: FileCategory,
    pub count: usize,
}

/// Rollup over the reverse-impact results, attached when `summary_only` is set.
/// `total_affected` is the count before any `limit` truncation.
#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ImpactSummary {
    pub total_affected: usize,
    pub by_distance: Vec<DistanceCount>,
    pub by_category: Vec<CategoryCount>,
}

/// What is worth telling an agent about a file it is ABOUT to edit, assembled
/// in one invocation from facts that are already counted.
///
/// Deliberately excludes anything derived from `assess_risk`: that walks import
/// cycles and per-symbol reference counts and was measured at 464-675ms on real
/// indexes, which is not a per-edit budget. It also excludes `test_gap`, whose
/// false-positive rate measured 15-25%.
///
/// Every field here is history- or convention-derived, so an unsaved edit to
/// the file does not invalidate it. `stale` is therefore informational, not a
/// reason to suppress.
#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct EditBrief {
    pub file_path: String,
    /// True when there is nothing worth an agent's context. A caller that
    /// serves this unasked must emit NOTHING in that case.
    pub empty: bool,
    /// The file ranks high on churn AND has enough commits for that rank to
    /// mean something. Churn percentile is a within-repo rank, so on its own it
    /// always promotes the top quartile of any repo, however young — a file
    /// with two commits can sit at the 90th percentile. Renderers should key on
    /// this rather than re-deriving a threshold from `churn_percentile`.
    pub hotspot: bool,
    /// 0.0-1.0. Present only when the file has git history indexed.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub churn_percentile: Option<f64>,
    /// Commits touching this file whose message looks like a fix.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub fix_count: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub commits: Option<u32>,
    /// Files that historically change with this one, most-coupled first.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub coupled: Vec<String>,
    /// Tests to run after the edit: sibling-convention matches first, then
    /// co-change matches.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub tests: Vec<String>,
    /// The file on disk differs from what was indexed. Does not invalidate
    /// anything above, which is why these signals were the ones chosen.
    #[serde(skip_serializing_if = "std::ops::Not::not", default)]
    pub stale: bool,
}

/// One symbol on a call chain, with the line in the *previous* step's body
/// where it is invoked. `call_line` is None on the first step, which is the
/// origin and is not called by anything in the path.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CallPathStep {
    pub name: String,
    pub qualified_name: String,
    pub file_path: String,
    pub line_start: u32,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub call_line: Option<u32>,
}

/// Shortest call chain between two symbols, or the reason there is none.
#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CallPathReport {
    pub found: bool,
    /// Symbols from origin to target inclusive. Empty when `found` is false.
    pub steps: Vec<CallPathStep>,
    /// Edge count — one less than `steps.len()`.
    pub length: usize,
    /// Why an unfound path is unfound: the origin or target did not resolve,
    /// or the search hit its depth or breadth bound before reaching the
    /// target. Distinguishes "no such path" from "stopped looking".
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub note: Option<String>,
    /// True when a bound stopped the search, so `found: false` is not proof
    /// that no path exists.
    #[serde(skip_serializing_if = "std::ops::Not::not", default)]
    pub bounded: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CallPathRequest {
    pub from: String,
    pub to: String,
    #[serde(default = "default_call_path_depth")]
    pub max_depth: usize,
}

fn default_call_path_depth() -> usize {
    6
}

/// Adaptive `impact_analysis` output. `results` is the existing reverse-impact
/// list (so callers reading `.results` keep working); the other fields populate
/// only when the matching [`ImpactOptions`] flag is set.
#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ImpactReport {
    /// Reverse impact: files affected by changing the target, by distance.
    pub results: Vec<ImpactEntry>,
    /// Forward dependencies — the import targets (modules/symbols) the target's
    /// file imports. Present with `include_forward`.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub forward_dependencies: Vec<String>,
    /// Symbols in the target's own file(s). Present with `include_siblings`.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub sibling_symbols: Vec<SiblingSymbol>,
    /// True when `limit` dropped entries from `results`.
    #[serde(skip_serializing_if = "std::ops::Not::not", default)]
    pub truncated: bool,
    /// Rollup counts; present with `summary_only`.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub summary: Option<ImpactSummary>,
}

/// One-call orientation snapshot for an agent starting work on a project.
/// Aggregates already-indexed facts (languages, freshness, features, risk,
/// trust boundaries, conventions) plus suggested next tool calls. Bounded by
/// construction — every list is capped — so it stays a digest, not a dump.
#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ProjectOverview {
    /// Absolute canonical project root.
    pub project_root: String,
    /// Per-language file counts, descending.
    pub languages: Vec<LanguageStat>,
    /// Total indexed files.
    pub file_count: usize,
    /// Total indexed symbols.
    pub symbol_count: usize,
    /// Structural + semantic index freshness vs git HEAD.
    pub freshness: FreshnessInfo,
    /// Mapped feature count by kind, descending.
    pub feature_summary: Vec<FeatureKindCount>,
    /// Total mapped features.
    pub feature_count: usize,
    /// Highest-risk files (capped), descending by score. Empty when git
    /// history is not yet indexed.
    pub top_risk_files: Vec<SessionRiskEntry>,
    /// Trust-boundary file counts across the project, descending. Surfaces the
    /// security-sensitive clusters (network, secrets, process-exec, …).
    pub trust_boundary_clusters: Vec<TrustBoundaryCount>,
    /// Test-file naming conventions an agent should expect, one hint per
    /// indexed language.
    pub test_conventions: Vec<String>,
    /// A sample of mapped entrypoints (routes, CLI commands, services,
    /// libraries) — the agent-facing surface area. Capped.
    pub entrypoints: Vec<EntrypointSummary>,
    /// Recommended next CodeSage calls for common intents, given this project's
    /// current state.
    pub suggested_next_calls: Vec<SuggestedCall>,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct LanguageStat {
    pub language: Language,
    pub file_count: usize,
}

/// Index freshness. Structural drift is measured against git HEAD; semantic
/// coverage is reported as the number of files with embedding chunks.
#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct FreshnessInfo {
    /// Drift classification: `fresh`, `behind_head`, `unrelated_ancestor`,
    /// `never_indexed`, `not_git`, or `unknown`.
    pub structural_kind: String,
    /// Human-readable one-line summary of the structural drift state.
    pub structural_summary: String,
    /// Commits between the indexed SHA and HEAD when behind on the same line.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub commits_behind: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub indexed_sha: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub head_sha: Option<String>,
    /// Files with at least one semantic chunk indexed.
    pub semantic_indexed_files: usize,
    /// True when any semantic chunks exist (a model is configured and the
    /// semantic pass has run).
    pub semantic_indexed: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct FeatureKindCount {
    pub kind: FeatureKind,
    pub count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TrustBoundaryCount {
    pub boundary: TrustBoundary,
    pub file_count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct EntrypointSummary {
    pub feature_id: String,
    pub kind: FeatureKind,
    pub title: String,
    pub entry_path: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub command: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub route: Option<String>,
}

/// A suggested next tool call keyed by the agent's likely intent.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SuggestedCall {
    /// The intent this call serves (e.g. "find code by behavior",
    /// "before editing a file", "before committing a patch").
    pub intent: String,
    /// The CodeSage tool to call.
    pub tool: String,
    /// Why this call, in one line.
    pub why: String,
}

/// Severity of a predicted review objection. Declaration order is the sort
/// order: `High` first.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "lowercase")]
pub enum ReviewSeverity {
    High,
    Medium,
    Low,
}

impl ReviewSeverity {
    pub fn as_str(&self) -> &'static str {
        match self {
            ReviewSeverity::High => "high",
            ReviewSeverity::Medium => "medium",
            ReviewSeverity::Low => "low",
        }
    }
}

/// One predicted review objection for a patch. Composed from the same signals
/// that back `assess_risk_diff`, `recommend_tests`, session checks, and feature
/// mapping — no new analysis, just the objections a reviewer would likely raise.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ReviewObjection {
    pub severity: ReviewSeverity,
    /// Short machine code: `missing-tests`, `high-risk-file`, `blast-radius`,
    /// `fix-prone`, `hotspot`, `import-cycle`, `trust-boundary`,
    /// `feature-test-gap`, `stale-index`.
    pub category: String,
    /// One-line human-readable objection.
    pub title: String,
    /// Concrete evidence lines (counts, scores, file lists) a reviewer can check.
    pub evidence: Vec<String>,
    /// Files this objection concerns.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub files: Vec<String>,
}

/// Predicted review objections for a patch, severity-ranked. Read-only,
/// pre-commit. Reuses `assess_risk_diff`, `recommend_tests`, drift, and feature
/// mapping rather than duplicating their logic.
#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ReviewRehearsal {
    /// The files considered (the patch / working-tree set).
    pub files: Vec<String>,
    /// Objections, `High` first.
    pub objections: Vec<ReviewObjection>,
    /// Paste-ready summary lines (headline + risk summary + tests to run).
    pub summary_notes: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_category_classifies_tests() {
        assert_eq!(FileCategory::classify("tests/foo.rs"), FileCategory::Test);
        assert_eq!(
            FileCategory::classify("app/tests/UserTest.php"),
            FileCategory::Test
        );
        assert_eq!(
            FileCategory::classify("src/components/Button.test.tsx"),
            FileCategory::Test
        );
        assert_eq!(
            FileCategory::classify("src/utils.spec.ts"),
            FileCategory::Test
        );
        assert_eq!(
            FileCategory::classify("app/tests/UserTest.php"),
            FileCategory::Test
        );
        assert_eq!(
            FileCategory::classify("src/ext/iconv/tests/bug_001.phpt"),
            FileCategory::Test
        );
        assert_eq!(
            FileCategory::classify("pkg/auth_test.py"),
            FileCategory::Test
        );
        assert_eq!(
            FileCategory::classify("src/test/java/UserServiceTest.java"),
            FileCategory::Test
        );
        assert_eq!(
            FileCategory::classify("app/__tests__/helper.js"),
            FileCategory::Test
        );
        assert_eq!(
            FileCategory::classify("src/spec/helpers.rb"),
            FileCategory::Test
        );
    }

    #[test]
    fn reference_kind_json_uses_stable_snake_case_names() {
        assert_eq!(
            serde_json::to_string(&ReferenceKind::TraitUse).unwrap(),
            "\"trait_use\""
        );
        assert_eq!(
            serde_json::to_string(&ReferenceKind::TypeHint).unwrap(),
            "\"type_hint\""
        );
        assert_eq!(
            serde_json::to_string(&ReferenceKind::RouteHandler).unwrap(),
            "\"route_handler\""
        );

        assert_eq!(
            serde_json::from_str::<ReferenceKind>("\"trait_use\"").unwrap(),
            ReferenceKind::TraitUse
        );
        assert_eq!(
            serde_json::from_str::<ReferenceKind>("\"type_hint\"").unwrap(),
            ReferenceKind::TypeHint
        );
        assert_eq!(
            serde_json::from_str::<ReferenceKind>("\"route_handler\"").unwrap(),
            ReferenceKind::RouteHandler
        );

        assert_eq!(
            serde_json::from_str::<ReferenceKind>("\"traituse\"").unwrap(),
            ReferenceKind::TraitUse
        );
        assert_eq!(
            serde_json::from_str::<ReferenceKind>("\"typehint\"").unwrap(),
            ReferenceKind::TypeHint
        );
        assert_eq!(
            serde_json::from_str::<ReferenceKind>("\"routehandler\"").unwrap(),
            ReferenceKind::RouteHandler
        );
    }

    #[test]
    fn file_category_does_not_misclassify_source_files_named_like_tests() {
        // Regression: the previous Java/PHP arms used
        // a lowercase-suffix match without a separator, so source files
        // whose names happen to end in `test.java`/`tests.java`/
        // `test.php` got classified as tests and dropped from
        // impact_analysis with source_only=true.
        assert_eq!(
            FileCategory::classify("src/main/java/com/acme/Latest.java"),
            FileCategory::Source
        );
        assert_eq!(
            FileCategory::classify("src/main/java/com/acme/Manifests.java"),
            FileCategory::Source
        );
        assert_eq!(
            FileCategory::classify("app/Models/Latest.php"),
            FileCategory::Source
        );
        // Sanity: legitimate test conventions still classify as tests.
        assert_eq!(
            FileCategory::classify("src/main/java/com/acme/UserServiceTest.java"),
            FileCategory::Test
        );
        assert_eq!(
            FileCategory::classify("src/main/java/com/acme/UserServiceTests.java"),
            FileCategory::Test
        );
    }

    #[test]
    fn file_category_classifies_configs() {
        assert_eq!(FileCategory::classify("Cargo.toml"), FileCategory::Config);
        assert_eq!(FileCategory::classify(".env"), FileCategory::Config);
        assert_eq!(
            FileCategory::classify("config/database.yml"),
            FileCategory::Config
        );
        assert_eq!(FileCategory::classify("package.json"), FileCategory::Config);
        assert_eq!(FileCategory::classify("nginx.conf"), FileCategory::Config);
    }

    #[test]
    fn file_category_classifies_source() {
        assert_eq!(FileCategory::classify("src/main.rs"), FileCategory::Source);
        assert_eq!(
            FileCategory::classify("app/Services/AuthService.php"),
            FileCategory::Source
        );
        assert_eq!(
            FileCategory::classify("pkg/handlers.py"),
            FileCategory::Source
        );
        assert_eq!(
            FileCategory::classify("src/components/Button.tsx"),
            FileCategory::Source
        );
    }

    #[test]
    fn impact_target_serializes_with_discriminator() {
        let sym = ImpactTarget::Symbol { name: "Foo".into() };
        let json = serde_json::to_string(&sym).unwrap();
        assert!(json.contains("\"type\":\"symbol\""));
        assert!(json.contains("\"name\":\"Foo\""));

        let file = ImpactTarget::File {
            path: "src/a.rs".into(),
        };
        let json = serde_json::to_string(&file).unwrap();
        assert!(json.contains("\"type\":\"file\""));
        assert!(json.contains("\"path\":\"src/a.rs\""));
    }

    #[test]
    fn impact_target_heuristic_keeps_dotted_symbols_as_symbols() {
        match ImpactTarget::from_hint("Repository.find".into(), None) {
            ImpactTarget::Symbol { name } => assert_eq!(name, "Repository.find"),
            other => panic!("dotted method target must stay a symbol, got {other:?}"),
        }

        match ImpactTarget::from_hint("fmt.Println".into(), None) {
            ImpactTarget::Symbol { name } => assert_eq!(name, "fmt.Println"),
            other => panic!("Go selector target must stay a symbol, got {other:?}"),
        }

        match ImpactTarget::from_hint("App\\Repository\\find".into(), None) {
            ImpactTarget::Symbol { name } => assert_eq!(name, "App\\Repository\\find"),
            other => panic!("PHP qualified target must stay a symbol, got {other:?}"),
        }
    }

    #[test]
    fn impact_target_heuristic_still_detects_file_like_targets() {
        match ImpactTarget::from_hint("src/main.rs".into(), None) {
            ImpactTarget::File { path } => assert_eq!(path, "src/main.rs"),
            other => panic!("slash path must be a file target, got {other:?}"),
        }

        match ImpactTarget::from_hint("Cargo.toml".into(), None) {
            ImpactTarget::File { path } => assert_eq!(path, "Cargo.toml"),
            other => panic!("known file extension must be a file target, got {other:?}"),
        }

        // Regression: `.java` was missing from the
        // allow-list after Java was added to `Language`, so bare
        // `UserService.java` resolved as a symbol name and produced an
        // empty impact result.
        match ImpactTarget::from_hint("UserService.java".into(), None) {
            ImpactTarget::File { path } => assert_eq!(path, "UserService.java"),
            other => panic!(".java target must be a file target, got {other:?}"),
        }
    }

    /// Regression trap: the `legend` field on RiskDiffAssessment / RiskBatchAssessment
    /// is serialized as `_legend` (not `legend`). Agent prompts and downstream
    /// docs reference the underscore form. If the `serde(rename = "_legend")`
    /// annotation is dropped, the JSON key changes silently and every prompt
    /// that mentions `_legend` becomes wrong.
    #[test]
    fn legend_serializes_with_underscore_prefix() {
        let assert_underscored = |json: &str| {
            assert!(
                json.contains("\"_legend\""),
                "expected `_legend` key in JSON, got {json}"
            );
            assert!(
                !json.contains("\"legend\""),
                "the unprefixed `legend` key must NOT leak into JSON, got {json}"
            );
        };

        let mut diff = RiskDiffAssessment::default();
        diff.legend
            .insert("T".to_string(), "test gap: …".to_string());
        assert_underscored(&serde_json::to_string(&diff).unwrap());

        let mut batch = RiskBatchAssessment::default();
        batch
            .legend
            .insert("NG".to_string(), "no git history…".to_string());
        assert_underscored(&serde_json::to_string(&batch).unwrap());
    }

    #[test]
    fn empty_legend_is_omitted_from_json() {
        // Empty BTreeMap is gated by `skip_serializing_if = "BTreeMap::is_empty"`.
        // Confirms no spurious `_legend: {}` lands in responses with no aliasing.
        let a = RiskDiffAssessment::default();
        let json = serde_json::to_string(&a).unwrap();
        assert!(
            !json.contains("_legend"),
            "empty legend must be omitted, got {json}"
        );

        let b = RiskBatchAssessment::default();
        let json = serde_json::to_string(&b).unwrap();
        assert!(
            !json.contains("_legend"),
            "empty legend must be omitted, got {json}"
        );
    }

    /// Backward compatibility: payloads serialized before the structured
    /// `found` field existed must deserialize as `found=true` — absence of
    /// the field meant a success result under the old prose-sentinel scheme.
    #[test]
    fn found_defaults_to_true_when_absent() {
        let bundle: ContextBundle =
            serde_json::from_str(r#"{"target_description":"symbol: foo","primary":[]}"#).unwrap();
        assert!(bundle.found);

        let report: CouplingReport =
            serde_json::from_str(r#"{"coupled":[],"file_indexed":true,"file_commits":4}"#).unwrap();
        assert!(report.found);
    }

    /// Shaped like a real hotspot: ten coupled files (the `co_changes_for`
    /// cap), a small cycle, two boundaries, notes, one top symbol.
    fn risk_fixture() -> RiskAssessment {
        RiskAssessment {
            found: true,
            file: "src/lib.rs".to_string(),
            score: 0.61,
            verbose: true,
            churn_score: 12.5,
            churn_percentile: 0.94,
            fix_ratio: 0.31,
            total_commits: 16,
            fix_count: 5,
            dependent_files: 5,
            coupled_files: 10,
            test_gap: false,
            in_cycle: true,
            cycle_size: 3,
            cycle_files: vec!["src/a.rs".to_string(), "src/b.rs".to_string()],
            top_coupled: (0..10)
                .map(|i| CoChangeEntry {
                    file: format!("src/coupled_{i}.rs"),
                    weight: 7.5 - f64::from(i) * 0.4,
                    count: 12 - i,
                    last_observed_at: Some(1_788_000_000),
                })
                .collect(),
            trust_boundaries: vec![TrustBoundary::Filesystem, TrustBoundary::Database],
            notes: vec![
                "hotspot: churn percentile 94%".to_string(),
                "in import cycle of 3 files: src/a.rs, src/b.rs".to_string(),
            ],
            top_symbols: vec![TopSymbol {
                name: "run".to_string(),
                line: 10,
                kind: "function".to_string(),
                why: "hot: 142 lines, 38 refs, in 3-file cycle".to_string(),
            }],
        }
    }

    const RISK_VERBOSE_ONLY_KEYS: [&str; 11] = [
        "churn_score",
        "churn_percentile",
        "fix_ratio",
        "total_commits",
        "fix_count",
        "dependent_files",
        "coupled_files",
        "test_gap",
        "in_cycle",
        "cycle_size",
        "top_coupled",
    ];

    fn json_keys(json: &str) -> Vec<String> {
        // serde_json's Map is sorted, so read key order off the wire text.
        let value: serde_json::Value = serde_json::from_str(json).unwrap();
        let mut keyed: Vec<(usize, String)> = value
            .as_object()
            .unwrap()
            .keys()
            .map(|k| (json.find(&format!("\"{k}\":")).unwrap(), k.clone()))
            .collect();
        keyed.sort();
        keyed.into_iter().map(|(_, k)| k).collect()
    }

    /// The MCP default hides the decomposition and `top_coupled` but keeps
    /// `cycle_files` (the staleness scan and the cycle notes depend on it);
    /// `verbose` keeps the pre-trim field set and order.
    #[test]
    fn risk_assessment_verbose_switch_gates_wire_fields() {
        let full = risk_fixture();
        let full_json = serde_json::to_string(&full).unwrap();
        assert_eq!(
            json_keys(&full_json),
            [
                "found",
                "file",
                "score",
                "churn_score",
                "churn_percentile",
                "fix_ratio",
                "total_commits",
                "fix_count",
                "dependent_files",
                "coupled_files",
                "test_gap",
                "in_cycle",
                "cycle_size",
                "cycle_files",
                "top_coupled",
                "trust_boundaries",
                "notes",
                "top_symbols",
            ]
        );
        assert!(!full_json.contains("\"verbose\""));

        let mut trimmed = risk_fixture();
        trimmed.set_verbose(false);
        let trimmed_json = serde_json::to_string(&trimmed).unwrap();
        assert_eq!(
            json_keys(&trimmed_json),
            [
                "found",
                "file",
                "score",
                "cycle_files",
                "trust_boundaries",
                "notes",
                "top_symbols"
            ]
        );
        for key in RISK_VERBOSE_ONLY_KEYS {
            assert!(
                !trimmed_json.contains(&format!("\"{key}\"")),
                "`{key}` leaked into the trimmed payload: {trimmed_json}"
            );
        }

        // Size claim the trim exists for: the hidden fields were measured at
        // ~48% of a real `assess_risk_batch` payload, so this fixture must
        // shed at least that much.
        let saved = full_json.len() - trimmed_json.len();
        assert!(
            saved * 100 >= full_json.len() * 48,
            "expected >=48% reduction, got {saved}/{} bytes",
            full_json.len()
        );
    }

    /// Guards the hand-written `Serialize`: a field added to the struct but
    /// forgotten in the impl would come back at its default here and fail the
    /// `Debug` comparison.
    #[test]
    fn risk_assessment_verbose_round_trips_every_field() {
        let full = risk_fixture();
        let json = serde_json::to_string(&full).unwrap();
        let back: RiskAssessment = serde_json::from_str(&json).unwrap();
        assert_eq!(format!("{back:?}"), format!("{full:?}"));
    }

    /// A trimmed payload (what an agent gets by default) still deserializes:
    /// the hidden scalars fall back to their defaults.
    #[test]
    fn risk_assessment_trimmed_payload_deserializes() {
        let mut trimmed = risk_fixture();
        trimmed.set_verbose(false);
        let json = serde_json::to_string(&trimmed).unwrap();
        let back: RiskAssessment = serde_json::from_str(&json).unwrap();
        assert!(back.found);
        assert_eq!(back.score, 0.61);
        assert_eq!(back.churn_score, 0.0);
        assert_eq!(back.cycle_size, 0);
        assert_eq!(
            back.cycle_files.len(),
            2,
            "cycle_files is not verbose-gated"
        );
        assert!(back.top_coupled.is_empty());
        assert_eq!(back.notes.len(), 2);
    }

    /// Batch and diff containers propagate the switch into every entry,
    /// including the detail kept inside directory clusters.
    #[test]
    fn risk_containers_propagate_verbose_switch() {
        let mut batch = RiskBatchAssessment {
            files: vec![risk_fixture(), risk_fixture()],
            legend: BTreeMap::new(),
        };
        batch.set_verbose(false);
        let json = serde_json::to_string(&batch).unwrap();
        assert!(!json.contains("\"top_coupled\""), "{json}");

        let mut diff = RiskDiffAssessment {
            files: vec![risk_fixture()],
            clustered_directories: vec![ClusteredDirectory {
                directory: "src".to_string(),
                count: 5,
                top_files: vec![risk_fixture()],
                omitted_files: vec!["src/x.rs".to_string()],
            }],
            ..RiskDiffAssessment::default()
        };
        diff.set_verbose(false);
        let json = serde_json::to_string(&diff).unwrap();
        assert!(!json.contains("\"top_coupled\""), "{json}");
        assert!(!json.contains("\"churn_score\""), "{json}");
    }

    fn symbol_fixture(qualified_name: &str) -> Symbol {
        Symbol {
            name: "open".to_string(),
            qualified_name: qualified_name.to_string(),
            kind: SymbolKind::Method,
            file_path: "src/db.rs".to_string(),
            line_start: 10,
            line_end: 20,
            col_start: 4,
            col_end: 5,
            rationale: Vec::new(),
        }
    }

    /// `qualified_name` travels only when it adds information; columns never
    /// do. Both directions restore the struct.
    #[test]
    fn symbol_wire_drops_columns_and_redundant_qualified_name() {
        let bare = symbol_fixture("open");
        let json = serde_json::to_string(&bare).unwrap();
        assert_eq!(
            json,
            r#"{"name":"open","kind":"method","file_path":"src/db.rs","line_start":10,"line_end":20}"#
        );
        let back: Symbol = serde_json::from_str(&json).unwrap();
        assert_eq!(back.qualified_name, "open");
        assert_eq!(back.col_start, 0);

        let qualified = symbol_fixture("Database::open");
        let json = serde_json::to_string(&qualified).unwrap();
        assert_eq!(
            json_keys(&json),
            [
                "name",
                "qualified_name",
                "kind",
                "file_path",
                "line_start",
                "line_end"
            ]
        );
        let back: Symbol = serde_json::from_str(&json).unwrap();
        assert_eq!(back.qualified_name, "Database::open");
        assert_eq!(back.name, "open");

        // Pre-trim payloads that still carry columns are accepted.
        let legacy: Symbol = serde_json::from_str(
            r#"{"name":"open","qualified_name":"open","kind":"method","file_path":"src/db.rs","line_start":10,"line_end":20,"col_start":4,"col_end":5}"#,
        )
        .unwrap();
        assert_eq!(legacy.col_start, 4);
        assert_eq!(legacy.col_end, 5);
    }

    #[test]
    fn symbol_wire_keeps_rationale_when_present() {
        let mut sym = symbol_fixture("open");
        sym.rationale.push(RationaleEntry {
            kind: RationaleKind::Why,
            text: "keeps the pool warm".to_string(),
            line_start: 8,
            line_end: 9,
        });
        let json = serde_json::to_string(&sym).unwrap();
        assert!(json.contains("\"rationale\""), "{json}");
        assert!(!json.contains("\"col_start\""), "{json}");
        assert!(!json.contains("\"col_end\""), "{json}");
        let back: Symbol = serde_json::from_str(&json).unwrap();
        // The wire drops the columns by design, so the round-trip restores
        // everything except them; they come back at their default.
        let expected = Symbol {
            col_start: 0,
            col_end: 0,
            ..sym
        };
        assert_eq!(back, expected);
    }

    #[test]
    fn reference_wire_drops_column() {
        let r = Reference {
            from_file: "src/a.rs".to_string(),
            from_symbol: Some("a::run".to_string()),
            to_name: "open".to_string(),
            kind: ReferenceKind::Call,
            line: 12,
            col: 8,
        };
        let json = serde_json::to_string(&r).unwrap();
        assert!(!json.contains("\"col\""), "{json}");
        let back: Reference = serde_json::from_str(&json).unwrap();
        assert_eq!(back.col, 0);
        assert_eq!(back.line, 12);
    }
}
