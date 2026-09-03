use codesage_protocol::{FeatureKind, Language, ReferenceKind, SymbolKind};
use rmcp::schemars;

const PROJECT_ARG_DESC: &str = "Absolute path to the project root. Must be an onboarded CodeSage project (contains .codesage/index.db).";

/// Accept integer numeric params from agents that occasionally JSON-encode
/// numbers as strings (`{"limit": "5"}` instead of `{"limit": 5}`). The
/// default `Option<usize>` serde derive rejects the string form with
/// `invalid type: string "5", expected usize` — a hard error at the MCP
/// protocol layer that leaves the caller guessing. Retrospective session
/// analysis (`bench/analyze-codesage-quality.py`) found this was 100% of
/// the `find_coupling` error results, so the fix applies across every
/// integer param: `limit`, `offset`, `depth`.
fn deser_optional_usize<'de, D>(d: D) -> std::result::Result<Option<usize>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::Deserialize as _;

    #[derive(serde::Deserialize)]
    #[serde(untagged)]
    enum UsizeOrString {
        U(usize),
        S(String),
    }

    match Option::<UsizeOrString>::deserialize(d)? {
        None => Ok(None),
        Some(UsizeOrString::U(n)) => Ok(Some(n)),
        Some(UsizeOrString::S(s)) => {
            let trimmed = s.trim();
            if trimmed.is_empty() {
                return Ok(None);
            }
            trimmed.parse::<usize>().map(Some).map_err(|e| {
                serde::de::Error::custom(format!(
                    "expected integer or integer-as-string, got {s:?}: {e}"
                ))
            })
        }
    }
}

fn deser_optional_f32<'de, D>(d: D) -> std::result::Result<Option<f32>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::Deserialize as _;

    #[derive(serde::Deserialize)]
    #[serde(untagged)]
    enum F32OrString {
        F(f32),
        S(String),
    }

    // NaN/inf make every `value >= min_jaccard` comparison false, so a
    // non-finite threshold silently returns zero results instead of erroring.
    // `f32::parse` accepts "nan"/"inf", so reject non-finite on both paths.
    fn finite<E: serde::de::Error>(n: f32) -> std::result::Result<f32, E> {
        if n.is_finite() {
            Ok(n)
        } else {
            Err(E::custom(format!("expected a finite number, got {n}")))
        }
    }

    match Option::<F32OrString>::deserialize(d)? {
        None => Ok(None),
        Some(F32OrString::F(n)) => finite(n).map(Some),
        Some(F32OrString::S(s)) => {
            let trimmed = s.trim();
            if trimmed.is_empty() {
                return Ok(None);
            }
            let n = trimmed.parse::<f32>().map_err(|e| {
                serde::de::Error::custom(format!(
                    "expected number or number-as-string, got {s:?}: {e}"
                ))
            })?;
            finite(n).map(Some)
        }
    }
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct FindSymbolParams {
    #[schemars(description = PROJECT_ARG_DESC)]
    pub project: String,
    #[schemars(description = "Symbol name or qualified name to search for")]
    pub name: String,
    #[schemars(
        description = "Filter by kind: function, method, class, trait, interface, struct, enum, constant, macro, module, namespace"
    )]
    pub kind: Option<SymbolKind>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct FindReferencesParams {
    #[schemars(description = PROJECT_ARG_DESC)]
    pub project: String,
    #[schemars(description = "Symbol name to find references for")]
    pub name: String,
    #[schemars(
        description = "Filter by reference kind: import, include, call, instantiation, inheritance, trait_use, type_hint, route_handler"
    )]
    pub kind: Option<ReferenceKind>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct FindSimilarParams {
    #[schemars(description = PROJECT_ARG_DESC)]
    pub project: String,
    #[schemars(description = "Function/method name to find near-clones of")]
    pub name: String,
    #[schemars(
        description = "Minimum Jaccard similarity in [0, 1] (default 0.85). Out-of-range values are clamped, and the applied value is reported under `_meta.clamps`."
    )]
    #[serde(default, deserialize_with = "deser_optional_f32")]
    pub min_jaccard: Option<f32>,
    #[schemars(
        description = "Max results (default 20, max 100). Over-max requests are capped and reported under `_meta.clamps`."
    )]
    #[serde(default, deserialize_with = "deser_optional_usize")]
    pub limit: Option<usize>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct ListDependenciesParams {
    #[schemars(description = PROJECT_ARG_DESC)]
    pub project: String,
    #[schemars(description = "Relative file path from project root")]
    pub file_path: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct CouplingParams {
    #[schemars(description = PROJECT_ARG_DESC)]
    pub project: String,
    #[schemars(description = "Repo-relative file path to look up co-change history for")]
    pub file_path: String,
    #[schemars(
        description = "Max results (default 10, max 100). Over-max requests are capped and reported under `_meta.clamps`."
    )]
    #[serde(default, deserialize_with = "deser_optional_usize")]
    pub limit: Option<usize>,
}

const RISK_VERBOSE_DESC: &str = "Include per-signal decomposition (churn_score, churn_percentile, fix_ratio, total_commits, fix_count, dependent_files, coupled_files, test_gap, in_cycle, cycle_size) and coupled files (`top_coupled`) on each RiskAssessment; default false.";

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct RiskParams {
    #[schemars(description = PROJECT_ARG_DESC)]
    pub project: String,
    #[schemars(description = "Repo-relative file path to assess")]
    pub file_path: String,
    #[schemars(description = RISK_VERBOSE_DESC)]
    pub verbose: Option<bool>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct RiskDiffParams {
    #[schemars(description = PROJECT_ARG_DESC)]
    pub project: String,
    #[schemars(
        description = "Repo-relative file paths in the patch (typically the output of `git diff --name-only`). Must provide at least one path: an empty list is rejected."
    )]
    pub file_paths: Vec<String>,
    #[schemars(description = RISK_VERBOSE_DESC)]
    pub verbose: Option<bool>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct RiskBatchParams {
    #[schemars(description = PROJECT_ARG_DESC)]
    pub project: String,
    #[schemars(
        description = "Repo-relative file paths to score individually. Must provide at least one path: an empty list is rejected. Returns one RiskAssessment per path, in input order. Use when you have a list of files (e.g. from impact analysis or coupling) and want each one's individual score — saves the per-file MCP round-trip overhead vs N separate `assess_risk` calls. For patch-level aggregation (max/mean, summary_notes, cycles), use `assess_risk_diff` instead."
    )]
    pub file_paths: Vec<String>,
    #[schemars(description = RISK_VERBOSE_DESC)]
    pub verbose: Option<bool>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct TestsForParams {
    #[schemars(description = PROJECT_ARG_DESC)]
    pub project: String,
    #[schemars(
        description = "Repo-relative file paths whose tests should be recommended. Must provide at least one path: an empty list is rejected."
    )]
    pub file_paths: Vec<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct SessionParams {
    #[schemars(description = PROJECT_ARG_DESC)]
    pub project: String,
    #[schemars(
        description = "Session identifier (alphanumerics, '-', '_', '.', max 128 chars). Use the same id for the matching session_start and session_end. Defaults to \"default\" when omitted."
    )]
    pub session_id: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct TracePathParams {
    #[schemars(description = PROJECT_ARG_DESC)]
    pub project: String,
    #[schemars(description = "Origin symbol name (bare or qualified)")]
    pub from: String,
    #[schemars(description = "Target symbol the chain should reach")]
    pub to: String,
    #[schemars(
        description = "Maximum hops to search before giving up (default 6, and 6 is also the ceiling over MCP — a larger value is capped and reported under `_meta.clamps`, so `bounded: true` at 6 cannot be retried deeper from here; use the `codesage trace` CLI for a longer search)"
    )]
    #[serde(default, deserialize_with = "deser_optional_usize")]
    pub max_depth: Option<usize>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct ImpactParams {
    #[schemars(description = PROJECT_ARG_DESC)]
    pub project: String,
    #[schemars(description = "Symbol name or file path to analyze")]
    pub target: String,
    #[schemars(
        description = "Treat target as file path (auto-detected if path-like); pass false to force symbol interpretation"
    )]
    pub is_file: Option<bool>,
    #[schemars(
        description = "Recursion depth for transitive impact (default 2, max 6). Over-max requests are capped and reported under `_meta.clamps`."
    )]
    #[serde(default, deserialize_with = "deser_optional_usize")]
    pub depth: Option<usize>,
    #[schemars(description = "Exclude test and config files from results")]
    pub source_only: Option<bool>,
    #[schemars(
        description = "Also return the target's forward dependencies — the import targets (modules/symbols) its file imports — in `forward_dependencies`"
    )]
    pub include_forward: Option<bool>,
    #[schemars(
        description = "Also return the symbols defined alongside the target in its own file, in `sibling_symbols`"
    )]
    pub include_siblings: Option<bool>,
    #[schemars(
        description = "Cap the reverse-impact `results` list to this many entries (max 500). Over-max requests are capped and reported under `_meta.clamps`."
    )]
    #[serde(default, deserialize_with = "deser_optional_usize")]
    pub limit: Option<usize>,
    #[schemars(
        description = "Collapse per-reason detail and attach a `summary` rollup (total affected, counts by distance and category) — use on wide blast radii"
    )]
    pub summary_only: Option<bool>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct ExportContextParams {
    #[schemars(description = PROJECT_ARG_DESC)]
    pub project: String,
    #[schemars(description = "Natural language query or symbol name")]
    pub target: String,
    #[schemars(description = "Treat target as a symbol name instead of a semantic query")]
    pub is_symbol: Option<bool>,
    #[schemars(
        description = "Max primary results to include (default 5, max 20). Over-max requests are capped and reported under `_meta.clamps`."
    )]
    #[serde(default, deserialize_with = "deser_optional_usize")]
    pub limit: Option<usize>,
    #[schemars(description = "Include caller code in the bundle")]
    pub include_callers: Option<bool>,
    #[schemars(description = "Include callee/dependency code in the bundle")]
    pub include_callees: Option<bool>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct SearchParams {
    #[schemars(description = PROJECT_ARG_DESC)]
    pub project: String,
    #[schemars(
        description = "Natural language query or code snippet to search for semantically similar code"
    )]
    pub query: String,
    #[schemars(
        description = "Maximum results to return (default 10, max 100). Over-max requests are capped and reported under `_meta.clamps`."
    )]
    #[serde(default, deserialize_with = "deser_optional_usize")]
    pub limit: Option<usize>,
    #[schemars(
        description = "Results offset for pagination (max 1000). Over-max requests are capped and reported under `_meta.clamps`."
    )]
    #[serde(default, deserialize_with = "deser_optional_usize")]
    pub offset: Option<usize>,
    #[schemars(
        description = "Filter by language: php, python, c, cpp, java, rust, javascript, typescript, go"
    )]
    pub language: Option<Language>,
    #[schemars(description = "Filter by file path glob patterns")]
    pub paths: Option<Vec<String>>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct ListFeaturesParams {
    #[schemars(description = PROJECT_ARG_DESC)]
    pub project: String,
    #[schemars(
        description = "Filter by feature kind: cli-command, route, service, library, test-suite, config, job, unknown"
    )]
    pub kind: Option<FeatureKind>,
    #[schemars(
        description = "Filter by language: php, python, c, cpp, java, rust, javascript, typescript, go"
    )]
    pub language: Option<Language>,
    #[schemars(description = "Filter by tag substring (e.g. \"framework:laravel\", \"library\")")]
    pub tag: Option<String>,
    #[schemars(
        description = "Keep only features whose entry/owned/context files changed since this git ref (e.g. \"main\", \"HEAD~5\"). Uses `git diff <ref>...HEAD`; errors if the ref is unknown."
    )]
    pub since: Option<String>,
    #[schemars(
        description = "Max results (default 100, 0 = no limit, max 500). Over-max requests are capped and reported under `_meta.clamps`."
    )]
    #[serde(default, deserialize_with = "deser_optional_usize")]
    pub limit: Option<usize>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct FindFeatureParams {
    #[schemars(description = PROJECT_ARG_DESC)]
    pub project: String,
    #[schemars(description = "Repo-relative file path to look up")]
    pub file_path: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct FeatureBundleParams {
    #[schemars(description = PROJECT_ARG_DESC)]
    pub project: String,
    #[schemars(
        description = "Feature id (e.g. feat_abc123) from `list_features` / `find_feature`"
    )]
    pub feature_id: String,
    #[schemars(
        description = "Include caller chunks for the feature's entry symbol (default false)"
    )]
    pub include_callers: Option<bool>,
    #[schemars(
        description = "Include callee chunks reached from the feature's entry symbol (default false)"
    )]
    pub include_callees: Option<bool>,
    #[schemars(
        description = "Max chunks per section (primary, related). Default 5, max 20. Over-max requests are capped and reported under `_meta.clamps`."
    )]
    #[serde(default, deserialize_with = "deser_optional_usize")]
    pub limit: Option<usize>,
}

/// Input of the hidden `embed_texts` tool: the CLI's request to embed with
/// the daemon's resident session instead of a private one.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct EmbedTextsParams {
    #[schemars(description = PROJECT_ARG_DESC)]
    pub project: String,
    #[schemars(
        description = "Embedding model the caller indexes with; must equal the project's configured model"
    )]
    pub model: String,
    #[schemars(
        description = "The full semantic fingerprint the caller will attest these vectors under; required with non-empty texts and must equal the daemon's own"
    )]
    #[serde(default)]
    pub fingerprint: Option<String>,
    #[schemars(description = "Chunk texts to embed, in order; empty to probe model and dimension")]
    pub texts: Vec<String>,
}

/// Output of the hidden `embed_texts` tool.
#[derive(Debug, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub struct EmbedTextsResult {
    pub model: String,
    pub dim: usize,
    /// The semantic fingerprint of the vectors this daemon's session
    /// produces: configured setup, model-file digest, and the execution
    /// provider the session actually initialised on.
    pub fingerprint: String,
    pub embeddings: Vec<Vec<f32>>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct ProjectOverviewParams {
    #[schemars(description = PROJECT_ARG_DESC)]
    pub project: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct ReviewRehearsalParams {
    #[schemars(description = PROJECT_ARG_DESC)]
    pub project: String,
    #[schemars(
        description = "Repo-relative file paths in the patch / working-tree change set (typically `git diff --name-only`). Must provide at least one path: an empty list is rejected (the CLI's stdin/working-tree fallback does not exist over MCP)."
    )]
    pub file_paths: Vec<String>,
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn coupling_params_accept_int_limit() {
        let p: CouplingParams = serde_json::from_value(json!({
            "project": "/p",
            "file_path": "a.rs",
            "limit": 5,
        }))
        .unwrap();
        assert_eq!(p.limit, Some(5));
    }

    #[test]
    fn coupling_params_accept_stringy_limit() {
        // Session logs showed 100% of find_coupling MCP -32602 errors were
        // agents sending `"limit": "5"` as a JSON string. Must parse.
        let p: CouplingParams = serde_json::from_value(json!({
            "project": "/p",
            "file_path": "a.rs",
            "limit": "5",
        }))
        .unwrap();
        assert_eq!(p.limit, Some(5));
    }

    #[test]
    fn coupling_params_accept_missing_limit() {
        let p: CouplingParams = serde_json::from_value(json!({
            "project": "/p",
            "file_path": "a.rs",
        }))
        .unwrap();
        assert_eq!(p.limit, None);
    }

    #[test]
    fn coupling_params_reject_non_numeric_string() {
        let r: Result<CouplingParams, _> = serde_json::from_value(json!({
            "project": "/p",
            "file_path": "a.rs",
            "limit": "not-a-number",
        }));
        assert!(r.is_err(), "non-numeric string must still error");
        // Error should name the offending value rather than be a generic
        // "expected usize" so the agent can fix its request.
        let msg = r.unwrap_err().to_string();
        assert!(
            msg.contains("not-a-number"),
            "error must quote offending value, got: {msg}"
        );
    }

    #[test]
    fn impact_params_coerce_depth_string() {
        let p: ImpactParams = serde_json::from_value(json!({
            "project": "/p",
            "target": "Foo",
            "depth": "3",
        }))
        .unwrap();
        assert_eq!(p.depth, Some(3));
    }

    #[test]
    fn search_params_coerce_limit_and_offset_strings() {
        let p: SearchParams = serde_json::from_value(json!({
            "project": "/p",
            "query": "auth",
            "limit": "10",
            "offset": "20",
        }))
        .unwrap();
        assert_eq!(p.limit, Some(10));
        assert_eq!(p.offset, Some(20));
    }

    #[test]
    fn find_similar_params_coerce_min_jaccard_string() {
        let p: FindSimilarParams = serde_json::from_value(json!({
            "project": "/p",
            "name": "clone_me",
            "min_jaccard": "0.72",
        }))
        .unwrap();
        assert_eq!(p.min_jaccard, Some(0.72));
    }

    #[test]
    fn find_similar_params_reject_bad_min_jaccard_string() {
        let r: Result<FindSimilarParams, _> = serde_json::from_value(json!({
            "project": "/p",
            "name": "clone_me",
            "min_jaccard": "close",
        }));
        assert!(r.is_err(), "non-float string must still error");
        let msg = r.unwrap_err().to_string();
        assert!(
            msg.contains("close"),
            "error must quote offending value, got: {msg}"
        );
    }

    #[test]
    fn find_similar_params_reject_non_finite_min_jaccard() {
        for bad in ["nan", "inf", "-inf", "infinity"] {
            let r: Result<FindSimilarParams, _> = serde_json::from_value(json!({
                "project": "/p",
                "name": "clone_me",
                "min_jaccard": bad,
            }));
            assert!(
                r.is_err(),
                "non-finite threshold {bad:?} must error (NaN/inf silently zeroes results)"
            );
        }
    }

    #[test]
    fn find_similar_params_accept_out_of_range_min_jaccard_for_param_layer_clamp() {
        // Out-of-range is a clamp, not a deser error: the tool layer clamps
        // to [0, 1] and reports the applied value under `_meta.clamps`.
        // (Non-finite values ARE still rejected here — NaN/inf silently
        // zero the results instead of erroring.)
        for v in [1.5, -0.2, 0.0, 1.0] {
            let p: FindSimilarParams = serde_json::from_value(json!({
                "project": "/p",
                "name": "clone_me",
                "min_jaccard": v,
            }))
            .unwrap();
            assert_eq!(p.min_jaccard, Some(v as f32));
        }
    }

    #[test]
    fn find_symbol_kind_accepts_every_documented_string() {
        for kind in [
            "function",
            "method",
            "class",
            "trait",
            "interface",
            "struct",
            "enum",
            "constant",
            "macro",
            "module",
            "namespace",
        ] {
            let p: FindSymbolParams = serde_json::from_value(json!({
                "project": "/p",
                "name": "x",
                "kind": kind,
            }))
            .unwrap_or_else(|e| panic!("documented kind `{kind}` must parse: {e}"));
            assert!(p.kind.is_some(), "kind `{kind}` deserialized to None");
        }
    }

    #[test]
    fn find_symbol_rejects_unknown_kind() {
        // Pre-fix, a bad kind silently dropped the filter and returned
        // unfiltered results. Typed enum params make it a serde error the
        // MCP layer reports as -32602.
        let r: Result<FindSymbolParams, _> = serde_json::from_value(json!({
            "project": "/p",
            "name": "x",
            "kind": "clazz",
        }));
        assert!(r.is_err(), "unknown kind must error, not unfilter");
    }

    #[test]
    fn find_references_kind_accepts_every_documented_string() {
        for kind in [
            "import",
            "include",
            "call",
            "instantiation",
            "inheritance",
            "trait_use",
            "type_hint",
            "route_handler",
        ] {
            let p: FindReferencesParams = serde_json::from_value(json!({
                "project": "/p",
                "name": "x",
                "kind": kind,
            }))
            .unwrap_or_else(|e| panic!("documented kind `{kind}` must parse: {e}"));
            assert!(p.kind.is_some(), "kind `{kind}` deserialized to None");
        }
    }

    #[test]
    fn find_references_rejects_unknown_kind() {
        let r: Result<FindReferencesParams, _> = serde_json::from_value(json!({
            "project": "/p",
            "name": "x",
            "kind": "callsite",
        }));
        assert!(r.is_err(), "unknown reference kind must error");
    }

    #[test]
    fn list_features_accepts_documented_kind_and_language_strings() {
        for kind in [
            "cli-command",
            "route",
            "service",
            "library",
            "test-suite",
            "config",
            "job",
            "unknown",
        ] {
            let p: ListFeaturesParams = serde_json::from_value(json!({
                "project": "/p",
                "kind": kind,
            }))
            .unwrap_or_else(|e| panic!("documented kind `{kind}` must parse: {e}"));
            assert!(p.kind.is_some(), "kind `{kind}` deserialized to None");
        }
        for lang in [
            "php",
            "python",
            "c",
            "cpp",
            "java",
            "rust",
            "javascript",
            "typescript",
            "go",
        ] {
            let p: ListFeaturesParams = serde_json::from_value(json!({
                "project": "/p",
                "language": lang,
            }))
            .unwrap_or_else(|e| panic!("documented language `{lang}` must parse: {e}"));
            assert!(
                p.language.is_some(),
                "language `{lang}` deserialized to None"
            );
        }
    }

    #[test]
    fn list_features_rejects_unknown_kind() {
        let r: Result<ListFeaturesParams, _> = serde_json::from_value(json!({
            "project": "/p",
            "kind": "microservice",
        }));
        assert!(r.is_err(), "unknown feature kind must error");
    }

    #[test]
    fn search_rejects_unknown_language() {
        let r: Result<SearchParams, _> = serde_json::from_value(json!({
            "project": "/p",
            "query": "auth",
            "language": "cobol",
        }));
        assert!(r.is_err(), "unknown language must error, not unfilter");
    }
}
