//! Seed types produced by per-language mappers, plus the `FeatureMapper`
//! trait. Mappers run deterministically (no LLM); each seed maps 1:1 to a
//! [`codesage_protocol::FeatureRecord`] after the orchestrator resolves
//! nearby tests and trust boundaries.

use std::path::Path;

use anyhow::Result;
use codesage_protocol::{FeatureConfidence, FeatureKind, Language};
use globset::GlobSet;

/// Context handed to every mapper. Bundles the project root with a
/// pre-compiled `GlobSet` of the project's `[index].exclude_patterns`, so
/// every walker honors the same exclusion contract as the structural
/// indexer. Pass `&MapperContext::for_root(root)` when no excludes apply
/// (tests, narrow programmatic callers).
pub struct MapperContext<'a> {
    pub root: &'a Path,
    pub excludes: Option<&'a GlobSet>,
}

impl<'a> MapperContext<'a> {
    /// Context with no exclusion globs. Equivalent to mapper behavior
    /// before `[index].exclude_patterns` was plumbed.
    pub fn for_root(root: &'a Path) -> Self {
        Self {
            root,
            excludes: None,
        }
    }

    /// Test the supplied repo-relative path against the configured
    /// excludes. Returns `false` when no excludes are set.
    pub fn excluded(&self, rel: &str) -> bool {
        self.excludes.is_some_and(|g| g.is_match(rel))
    }
}

/// A file the mapper attaches to the seed with an explicit role hint and
/// a one-line reason. The orchestrator later folds these into the
/// `FeatureRecord.files[]` array.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeedFile {
    pub path: String,
    pub reason: String,
}

/// A test file associated with a feature. `command` is the language-native
/// "how would you run it?" hint (e.g. `cargo test --package foo`) or `None`
/// when the framework doesn't expose one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeedTest {
    pub path: String,
    pub command: Option<String>,
}

/// Mapper output before the orchestrator resolves nearby tests, trust
/// boundaries, and assigns a stable `feature_id`. The orchestrator merges
/// seeds by `(kind, source, entry_path, command|route|symbol)` so two
/// mappers seeding the same shape deduplicate naturally.
#[derive(Debug, Clone)]
pub struct FeatureSeed {
    pub title: String,
    pub summary: String,
    pub kind: FeatureKind,
    pub source: &'static str,
    pub confidence: FeatureConfidence,
    pub entry_path: String,
    pub entry_symbol: Option<String>,
    pub entry_route: Option<String>,
    pub entry_command: Option<String>,
    pub language: Language,
    pub tags: Vec<String>,
    /// Files that *implement* this feature beyond the entry path itself.
    /// The orchestrator always adds the entry file as `Entry`; these go
    /// in as `Owned`.
    pub owned_files: Vec<SeedFile>,
    /// Files the feature reads but doesn't own (imports, configs, shared
    /// helpers). Folded in as `Context`.
    pub context_files: Vec<SeedFile>,
    /// Hand-attached tests. Orchestrator also runs `nearby_tests` and
    /// merges; explicit seed tests take precedence on path collision.
    pub tests: Vec<SeedTest>,
    /// Glob-style directories the nearby-test walker should consult for
    /// this seed in addition to its language defaults. Empty when the
    /// language conventions are sufficient.
    pub test_prefixes: Vec<String>,
}

/// One mapper module. `name` is the language/source tag used in logs;
/// `map` returns deterministic seeds. The `ctx` carries the repo root
/// plus the project's exclude globs so every walker honors the same
/// filter contract.
pub trait FeatureMapper: Send + Sync {
    fn name(&self) -> &'static str;
    fn map(&self, ctx: &MapperContext) -> Result<Vec<FeatureSeed>>;
}
