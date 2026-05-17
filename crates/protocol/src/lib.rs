use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

pub const DEFAULT_EMBEDDING_DIM: usize = 384;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Language {
    Php,
    Python,
    C,
    Cpp,
    Java,
    Rust,
    JavaScript,
    TypeScript,
    Go,
}

impl Language {
    pub fn as_str(&self) -> &'static str {
        match self {
            Language::Php => "php",
            Language::Python => "python",
            Language::C => "c",
            Language::Cpp => "cpp",
            Language::Java => "java",
            Language::Rust => "rust",
            Language::JavaScript => "javascript",
            Language::TypeScript => "typescript",
            Language::Go => "go",
        }
    }
}

impl Language {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "php" => Some(Language::Php),
            "python" => Some(Language::Python),
            "c" => Some(Language::C),
            "cpp" | "c++" | "cxx" => Some(Language::Cpp),
            "java" => Some(Language::Java),
            "rust" => Some(Language::Rust),
            "javascript" | "js" => Some(Language::JavaScript),
            "typescript" | "ts" => Some(Language::TypeScript),
            "go" => Some(Language::Go),
            _ => None,
        }
    }
}

impl std::fmt::Display for Language {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

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

impl SymbolKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            SymbolKind::Function => "function",
            SymbolKind::Method => "method",
            SymbolKind::Class => "class",
            SymbolKind::Trait => "trait",
            SymbolKind::Interface => "interface",
            SymbolKind::Struct => "struct",
            SymbolKind::Enum => "enum",
            SymbolKind::Constant => "constant",
            SymbolKind::Macro => "macro",
            SymbolKind::Module => "module",
            SymbolKind::Namespace => "namespace",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "function" => Some(SymbolKind::Function),
            "method" => Some(SymbolKind::Method),
            "class" => Some(SymbolKind::Class),
            "trait" => Some(SymbolKind::Trait),
            "interface" => Some(SymbolKind::Interface),
            "struct" => Some(SymbolKind::Struct),
            "enum" => Some(SymbolKind::Enum),
            "constant" => Some(SymbolKind::Constant),
            "macro" => Some(SymbolKind::Macro),
            "module" => Some(SymbolKind::Module),
            "namespace" => Some(SymbolKind::Namespace),
            _ => None,
        }
    }
}

impl std::fmt::Display for SymbolKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct FileInfo {
    pub path: String,
    pub language: Language,
    pub content_hash: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Symbol {
    pub name: String,
    pub qualified_name: String,
    pub kind: SymbolKind,
    pub file_path: String,
    pub line_start: u32,
    pub line_end: u32,
    pub col_start: u32,
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

impl TrustBoundary {
    /// Stable lowercase-kebab string used in DB rows, JSON, and CLI output.
    pub fn as_str(&self) -> &'static str {
        match self {
            TrustBoundary::Network => "network",
            TrustBoundary::Filesystem => "filesystem",
            TrustBoundary::ProcessExec => "process-exec",
            TrustBoundary::Secrets => "secrets",
            TrustBoundary::Database => "database",
            TrustBoundary::UserInput => "user-input",
            TrustBoundary::ExternalApi => "external-api",
            TrustBoundary::Serialization => "serialization",
            TrustBoundary::Auth => "auth",
            TrustBoundary::Concurrency => "concurrency",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "network" => Some(TrustBoundary::Network),
            "filesystem" => Some(TrustBoundary::Filesystem),
            "process-exec" => Some(TrustBoundary::ProcessExec),
            "secrets" => Some(TrustBoundary::Secrets),
            "database" => Some(TrustBoundary::Database),
            "user-input" => Some(TrustBoundary::UserInput),
            "external-api" => Some(TrustBoundary::ExternalApi),
            "serialization" => Some(TrustBoundary::Serialization),
            "auth" => Some(TrustBoundary::Auth),
            "concurrency" => Some(TrustBoundary::Concurrency),
            _ => None,
        }
    }
}

impl std::fmt::Display for TrustBoundary {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum ReferenceKind {
    Import,
    Include,
    Call,
    Instantiation,
    Inheritance,
    TraitUse,
    TypeHint,
}

impl ReferenceKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            ReferenceKind::Import => "import",
            ReferenceKind::Include => "include",
            ReferenceKind::Call => "call",
            ReferenceKind::Instantiation => "instantiation",
            ReferenceKind::Inheritance => "inheritance",
            ReferenceKind::TraitUse => "trait_use",
            ReferenceKind::TypeHint => "type_hint",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "import" => Some(ReferenceKind::Import),
            "include" => Some(ReferenceKind::Include),
            "call" => Some(ReferenceKind::Call),
            "instantiation" => Some(ReferenceKind::Instantiation),
            "inheritance" => Some(ReferenceKind::Inheritance),
            "trait_use" => Some(ReferenceKind::TraitUse),
            "type_hint" => Some(ReferenceKind::TypeHint),
            _ => None,
        }
    }
}

impl std::fmt::Display for ReferenceKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Reference {
    pub from_file: String,
    pub from_symbol: Option<String>,
    pub to_name: String,
    pub kind: ReferenceKind,
    pub line: u32,
    pub col: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct FindSymbolRequest {
    pub name: String,
    pub kind: Option<SymbolKind>,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct FindReferencesRequest {
    pub symbol_name: String,
    pub kind: Option<ReferenceKind>,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct DependencyEntry {
    pub file_path: String,
    pub imports: Vec<String>,
    pub imported_by: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct IndexStats {
    pub files_indexed: usize,
    pub files_skipped: usize,
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
    pub files_removed: usize,
    pub chunks_created: usize,
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
        let lower = path.to_lowercase();
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
            // or `latest.php`. fnd_9931a623.
            || path.ends_with("Test.php")
            || path.ends_with("Test.java")
            || path.ends_with("Tests.java")
            || lower.ends_with("_test.py")
            || lower.ends_with("_test.go")
            || lower.ends_with(".phpt")
        {
            return FileCategory::Test;
        }
        let basename = lower.rsplit('/').next().unwrap_or(&lower);
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

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ContextBundle {
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
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RiskAssessment {
    pub file: String,
    pub score: f64,
    pub churn_score: f64,
    pub churn_percentile: f64,
    pub fix_ratio: f64,
    pub total_commits: u32,
    pub fix_count: u32,
    pub dependent_files: u32,
    pub coupled_files: u32,
    pub test_gap: bool,
    /// True when the file participates in a non-trivial import cycle (SCC of
    /// size ≥ 2 in the file-level import graph). Cycle membership is a strong
    /// structural signal that a change here can ripple unexpectedly through
    /// the cycle's other members; included as a 0.10-weighted input to `score`.
    #[serde(default)]
    pub in_cycle: bool,
    /// Number of files in the SCC this file belongs to (including this file).
    /// Zero when `in_cycle` is false. Larger cycles get a larger contribution
    /// to `score`, capped at size 5.
    #[serde(default)]
    pub cycle_size: u32,
    /// Other members of the import cycle (excluding this file), so the agent
    /// can name them in PR descriptions or follow up to assess whether the
    /// cycle should be broken. Empty when `in_cycle` is false.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub cycle_files: Vec<String>,
    /// Top co-changers, useful for the agent to know which tests/files to also touch.
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

/// Aggregate risk for a set of files (typically the file list of a patch or PR).
/// Lets an agent ask one question — "how risky is this change?" — instead of
/// per-file round-trips and manual aggregation.
#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RiskDiffAssessment {
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

impl FeatureKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            FeatureKind::CliCommand => "cli-command",
            FeatureKind::Route => "route",
            FeatureKind::Service => "service",
            FeatureKind::Library => "library",
            FeatureKind::TestSuite => "test-suite",
            FeatureKind::Config => "config",
            FeatureKind::Job => "job",
            FeatureKind::Unknown => "unknown",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "cli-command" => Some(FeatureKind::CliCommand),
            "route" => Some(FeatureKind::Route),
            "service" => Some(FeatureKind::Service),
            "library" => Some(FeatureKind::Library),
            "test-suite" => Some(FeatureKind::TestSuite),
            "config" => Some(FeatureKind::Config),
            "job" => Some(FeatureKind::Job),
            "unknown" => Some(FeatureKind::Unknown),
            _ => None,
        }
    }
}

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

impl FeatureConfidence {
    pub fn as_str(&self) -> &'static str {
        match self {
            FeatureConfidence::High => "high",
            FeatureConfidence::Medium => "medium",
            FeatureConfidence::Low => "low",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "high" => Some(FeatureConfidence::High),
            "medium" => Some(FeatureConfidence::Medium),
            "low" => Some(FeatureConfidence::Low),
            _ => None,
        }
    }
}

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

impl FeatureFileRole {
    pub fn as_str(&self) -> &'static str {
        match self {
            FeatureFileRole::Entry => "entry",
            FeatureFileRole::Owned => "owned",
            FeatureFileRole::Context => "context",
            FeatureFileRole::Test => "test",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "entry" => Some(FeatureFileRole::Entry),
            "owned" => Some(FeatureFileRole::Owned),
            "context" => Some(FeatureFileRole::Context),
            "test" => Some(FeatureFileRole::Test),
            _ => None,
        }
    }
}

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

/// `{"results": [...]}` envelope around `Vec<ImpactEntry>`. See [`FindSymbolResults`].
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ImpactAnalysisResults {
    pub results: Vec<ImpactEntry>,
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
    fn file_category_does_not_misclassify_source_files_named_like_tests() {
        // Regression for fnd_9931a623: the previous Java/PHP arms used
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

        // Regression for fnd_f736f669: `.java` was missing from the
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
    fn risk_diff_assessment_legend_serializes_with_underscore_prefix() {
        let mut a = RiskDiffAssessment::default();
        a.legend.insert("T".to_string(), "test gap: …".to_string());
        let json = serde_json::to_string(&a).unwrap();
        assert!(
            json.contains("\"_legend\""),
            "expected `_legend` key in JSON, got {json}"
        );
        assert!(
            !json.contains("\"legend\""),
            "the unprefixed `legend` key must NOT leak into JSON, got {json}"
        );
    }

    #[test]
    fn risk_batch_assessment_legend_serializes_with_underscore_prefix() {
        let mut a = RiskBatchAssessment::default();
        a.legend
            .insert("NG".to_string(), "no git history…".to_string());
        let json = serde_json::to_string(&a).unwrap();
        assert!(
            json.contains("\"_legend\""),
            "expected `_legend` key in JSON, got {json}"
        );
        assert!(
            !json.contains("\"legend\""),
            "the unprefixed `legend` key must NOT leak into JSON, got {json}"
        );
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
}
