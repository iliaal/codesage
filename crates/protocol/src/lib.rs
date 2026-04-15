use serde::{Deserialize, Serialize};

pub const DEFAULT_EMBEDDING_DIM: usize = 384;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Language {
    Php,
    Python,
    C,
    Rust,
    JavaScript,
    TypeScript,
}

impl Language {
    pub fn as_str(&self) -> &'static str {
        match self {
            Language::Php => "php",
            Language::Python => "python",
            Language::C => "c",
            Language::Rust => "rust",
            Language::JavaScript => "javascript",
            Language::TypeScript => "typescript",
        }
    }
}

impl Language {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "php" => Some(Language::Php),
            "python" => Some(Language::Python),
            "c" => Some(Language::C),
            "rust" => Some(Language::Rust),
            "javascript" | "js" => Some(Language::JavaScript),
            "typescript" | "ts" => Some(Language::TypeScript),
            _ => None,
        }
    }
}

impl std::fmt::Display for Language {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileInfo {
    pub path: String,
    pub language: Language,
    pub content_hash: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Symbol {
    pub name: String,
    pub qualified_name: String,
    pub kind: SymbolKind,
    pub file_path: String,
    pub line_start: u32,
    pub line_end: u32,
    pub col_start: u32,
    pub col_end: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Reference {
    pub from_file: String,
    pub from_symbol: Option<String>,
    pub to_name: String,
    pub kind: ReferenceKind,
    pub line: u32,
    pub col: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FindSymbolRequest {
    pub name: String,
    pub kind: Option<SymbolKind>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FindReferencesRequest {
    pub symbol_name: String,
    pub kind: Option<ReferenceKind>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DependencyEntry {
    pub file_path: String,
    pub imports: Vec<String>,
    pub imported_by: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct IndexStats {
    pub files_indexed: usize,
    pub files_skipped: usize,
    pub files_removed: usize,
    pub symbols_found: usize,
    pub references_found: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Chunk {
    pub text: String,
    pub start_line: u32,
    pub end_line: u32,
    pub start_byte: usize,
    pub end_byte: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchRequest {
    pub query: String,
    pub limit: Option<usize>,
    pub offset: Option<usize>,
    pub languages: Option<Vec<Language>>,
    pub paths: Option<Vec<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResult {
    pub file_path: String,
    pub language: String,
    pub content: String,
    pub start_line: u32,
    pub end_line: u32,
    pub score: f32,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub symbols: Vec<SymbolSummary>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SymbolSummary {
    pub name: String,
    pub qualified_name: String,
    pub kind: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SemanticIndexStats {
    pub files_processed: usize,
    pub files_skipped: usize,
    pub files_removed: usize,
    pub chunks_created: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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
            || lower.ends_with("test.php")
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

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ImpactTarget {
    Symbol { name: String },
    File { path: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImpactReason {
    pub via_symbol: String,
    pub kind: ReferenceKind,
    pub line: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImpactEntry {
    pub file_path: String,
    pub distance: u32,
    pub category: FileCategory,
    pub reasons: Vec<ImpactReason>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
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

fn default_export_limit() -> usize {
    5
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextBundle {
    pub target_description: String,
    pub primary: Vec<SearchResult>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub related: Vec<SearchResult>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub symbol_definitions: Vec<Symbol>,
}

/// One co-changing file pair, ranked by exponentially-decayed weight.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CoChangeEntry {
    pub file: String,
    pub weight: f64,
    pub count: u32,
    pub last_observed_at: Option<i64>,
}

/// Risk decomposition for a file. Score is the weighted sum; components let the agent
/// see WHY a file is risky, not just the magnitude.
#[derive(Debug, Clone, Serialize, Deserialize)]
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
    /// Top co-changers, useful for the agent to know which tests/files to also touch.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub top_coupled: Vec<CoChangeEntry>,
    /// Human-readable rationale lines so the agent can quote them in PR descriptions.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub notes: Vec<String>,
}

/// Stats from a git history indexing pass. Mirrors IndexStats shape.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GitIndexStats {
    pub commits_scanned: usize,
    pub files_tracked: usize,
    pub co_change_pairs: usize,
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
            FileCategory::classify("app/__tests__/helper.js"),
            FileCategory::Test
        );
        assert_eq!(
            FileCategory::classify("src/spec/helpers.rb"),
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
}
