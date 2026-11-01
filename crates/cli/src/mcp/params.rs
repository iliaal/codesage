use anyhow::Result;
use codesage_protocol::{FeatureKind, Language, ReferenceKind, SymbolKind};
use rmcp::schemars;

use super::error::{ErrorCode, McpError};

const PROJECT_ARG_DESC: &str = "Absolute path to the project root. Must be an onboarded CodeSage project (contains .codesage/index.db).";

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HelpParams {
    /// Absolute existing project directory. An index and model are not required.
    pub project: String,
    /// Advertised tool name, hidden tool name, or daemon for operator tools.
    pub tool: Option<String>,
    /// Envelope field, or a tool field such as input.query or output.results[].score.
    pub field: Option<String>,
    /// Error code such as E_NOT_FOUND. Cannot combine with other selectors.
    pub code: Option<String>,
    /// Ordered investigation plan. Cannot combine with other selectors.
    pub intent: Option<super::help::Intent>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DescribeParams {
    #[schemars(description = PROJECT_ARG_DESC)]
    pub project: String,
    #[schemars(
        description = "File, symbol, feature, or directory handle; indexed path; path:line; qualified or bare name; route:METHOD path; cmd:name. Ambiguity returns candidates without a card."
    )]
    pub target: String,
    #[serde(default)]
    pub detail: codesage_protocol::DescribeDetail,
    #[schemars(
        description = "Optional section selection. File: identity,symbols,dependencies,features,risk,coupling,tests,boundaries,findings. Symbol: identity,references,callees,clones,features,hotness,cycles. Feature: identity,risk,tests,boundaries,findings. Directory: module_map,fan_in,features,risk. Unknown or inapplicable sections are E_PARAM."
    )]
    pub sections: Option<Vec<String>>,
}

/// Entity-naming argument on the tools whose answer is about a symbol, a
/// feature, or free text.
const SYMBOL_TARGET_DESC: &str = "Preferred spelling of what to look up: a `sym:`/`file:` handle, an indexed path, `path:line`, a qualified or bare name, a `feat_` id, `route:METHOD path`, or `cmd:name`. Alias of `name` — pass one, or the same value in both.";

/// Entity-naming argument on the tools whose answer is about one file.
const FILE_TARGET_DESC: &str = "Preferred spelling of the file: a `file:`, `sym:`, or `chunk:` handle, an indexed path (a leading `./` is ignored), or `path:line`; a symbol target resolves to its file. Alias of `file_path` — pass one, or the same value in both. A spelling that matches no indexed path and no file in the working tree is E_NOT_FOUND with the nearest candidates, so a bare basename or a path spelled from a subdirectory gets told where the file is; a path the working tree holds is taken as written, so a file you just created reads as unindexed rather than as a miss.";

/// Entity-naming argument on the tools whose answer is about a set of files.
const FILE_TARGETS_DESC: &str = "Preferred spelling of the file set: each entry is a `file:`, `sym:`, or `chunk:` handle, an indexed path (a leading `./` is ignored), or `path:line`; a symbol target resolves to its file. Alias of `file_paths` — pass one, or the same list in both. A path the index does not hold is taken as written, so a new file still appears in this tool's own not-indexed disclosure.";

/// One target from a legacy argument and its `target` alias.
///
/// Both spellings are accepted while the legacy one is deprecated. Equal
/// values proceed; different ones are a parameter error naming both, because
/// picking one would answer a question the caller did not ask.
pub(crate) fn one_target(
    tool: &str,
    legacy_field: &str,
    legacy: Option<&str>,
    target: Option<&str>,
) -> Result<String> {
    match (legacy, target) {
        (Some(legacy_value), Some(target_value)) if legacy_value.trim() != target_value.trim() => {
            Err(McpError::new(
                ErrorCode::Param,
                format!(
                    "{tool}: `{legacy_field}` ({legacy_value:?}) and `target` ({target_value:?}) name different things; pass one of them"
                ),
            )
            .into())
        }
        (Some(value), _) | (None, Some(value)) => Ok(value.to_string()),
        (None, None) => Err(McpError::new(
            ErrorCode::Param,
            format!("{tool}: pass `target` (or the deprecated `{legacy_field}`)"),
        )
        .into()),
    }
}

/// [`one_target`] for the tools that take a file set.
pub(crate) fn one_target_list(
    tool: &str,
    legacy_field: &str,
    legacy: Option<&[String]>,
    targets: Option<&[String]>,
) -> Result<Vec<String>> {
    match (legacy, targets) {
        (Some(legacy_values), Some(target_values)) if !same_list(legacy_values, target_values) => {
            Err(McpError::new(
                ErrorCode::Param,
                format!(
                    "{tool}: {}; pass one of them",
                    list_conflict(legacy_field, legacy_values, "targets", target_values)
                ),
            )
            .into())
        }
        (Some(values), _) | (None, Some(values)) => Ok(values.to_vec()),
        (None, None) => Err(McpError::new(
            ErrorCode::Param,
            format!("{tool}: pass `targets` (or the deprecated `{legacy_field}`)"),
        )
        .into()),
    }
}

fn same_list(a: &[String], b: &[String]) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(x, y)| x.trim() == y.trim())
}

/// Where two file lists stop agreeing, with both spellings: lengths alone say
/// the request is contradictory without saying which entry to fix.
fn list_conflict(
    first_field: &str,
    first: &[String],
    second_field: &str,
    second: &[String],
) -> String {
    let index = first
        .iter()
        .zip(second)
        .position(|(a, b)| a.trim() != b.trim())
        .unwrap_or_else(|| first.len().min(second.len()));
    let entry = |values: &[String]| match values.get(index) {
        Some(value) => format!("{value:?}"),
        None => format!("nothing ({})", path_count(values.len())),
    };
    format!(
        "`{first_field}` and `{second_field}` name different sets, first at index {index}: {} vs {}",
        entry(first),
        entry(second)
    )
}

fn path_count(paths: usize) -> String {
    format!("{paths} path{}", if paths == 1 { "" } else { "s" })
}

/// The argument spellings a file-set request actually used, so an over-cap
/// retry remedy rewrites every list the caller wrote. Truncating one of two
/// equal lists would make the retry a conflict, which is not a remedy.
fn list_fields(
    file_paths: &Option<Vec<String>>,
    targets: &Option<Vec<String>>,
) -> &'static [&'static str] {
    match (file_paths.is_some(), targets.is_some()) {
        (true, true) => &["file_paths", "targets"],
        (false, true) => &["targets"],
        _ => &["file_paths"],
    }
}

/// Agents may encode numeric arguments as strings; accept both forms.
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

    // f32 parsing accepts NaN/inf, which make similarity thresholds silently reject matches.
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
#[serde(deny_unknown_fields)]
pub struct FindSymbolParams {
    #[schemars(description = PROJECT_ARG_DESC)]
    pub project: String,
    #[schemars(
        description = "DEPRECATED, still accepted: pass `target`. Symbol name or qualified name to search for."
    )]
    pub name: Option<String>,
    #[schemars(description = SYMBOL_TARGET_DESC)]
    pub target: Option<String>,
    #[schemars(
        description = "Filter by kind: function, method, class, trait, interface, struct, enum, constant, macro, module, namespace"
    )]
    pub kind: Option<SymbolKind>,
}

impl FindSymbolParams {
    pub(crate) fn target_arg(&self) -> Result<String> {
        one_target(
            "find_symbol",
            "name",
            self.name.as_deref(),
            self.target.as_deref(),
        )
    }
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FindReferencesParams {
    #[schemars(description = PROJECT_ARG_DESC)]
    pub project: String,
    #[schemars(
        description = "Symbol name to find references for. The primary spelling here, since the target is a name by nature; `target` is an accepted alias."
    )]
    pub name: Option<String>,
    #[schemars(description = SYMBOL_TARGET_DESC)]
    pub target: Option<String>,
    #[schemars(
        description = "Filter by reference kind: import, include, call, instantiation, inheritance, trait_use, type_hint, route_handler"
    )]
    pub kind: Option<ReferenceKind>,
}

impl FindReferencesParams {
    pub(crate) fn target_arg(&self) -> Result<String> {
        one_target(
            "find_references",
            "name",
            self.name.as_deref(),
            self.target.as_deref(),
        )
    }
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FindSimilarParams {
    #[schemars(description = PROJECT_ARG_DESC)]
    pub project: String,
    #[schemars(
        description = "DEPRECATED, still accepted: pass `target`. Function/method name to find near-clones of."
    )]
    pub name: Option<String>,
    #[schemars(description = SYMBOL_TARGET_DESC)]
    pub target: Option<String>,
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

impl FindSimilarParams {
    pub(crate) fn target_arg(&self) -> Result<String> {
        one_target(
            "find_similar",
            "name",
            self.name.as_deref(),
            self.target.as_deref(),
        )
    }
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ListDependenciesParams {
    #[schemars(description = PROJECT_ARG_DESC)]
    pub project: String,
    #[schemars(
        description = "DEPRECATED, still accepted: pass `target`. Relative file path from project root."
    )]
    pub file_path: Option<String>,
    #[schemars(description = FILE_TARGET_DESC)]
    pub target: Option<String>,
}

impl ListDependenciesParams {
    pub(crate) fn target_arg(&self) -> Result<String> {
        one_target(
            "list_dependencies",
            "file_path",
            self.file_path.as_deref(),
            self.target.as_deref(),
        )
    }
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CouplingParams {
    #[schemars(description = PROJECT_ARG_DESC)]
    pub project: String,
    #[schemars(
        description = "DEPRECATED, still accepted: pass `target`. Repo-relative file path to look up co-change history for."
    )]
    pub file_path: Option<String>,
    #[schemars(description = FILE_TARGET_DESC)]
    pub target: Option<String>,
    #[schemars(
        description = "Max results (default 10, max 100). Over-max requests are capped and reported under `_meta.clamps`."
    )]
    #[serde(default, deserialize_with = "deser_optional_usize")]
    pub limit: Option<usize>,
}

impl CouplingParams {
    pub(crate) fn target_arg(&self) -> Result<String> {
        one_target(
            "find_coupling",
            "file_path",
            self.file_path.as_deref(),
            self.target.as_deref(),
        )
    }
}

const RISK_TARGETS_DESC: &str = "Files to assess, at least one: each entry is a `file:`, `sym:`, or `chunk:` handle, an indexed path (a leading `./` is ignored), or `path:line`; a symbol target resolves to its file. One file is a list of one. A path the index does not hold is taken as written and reported with `found: false`, whatever the list length.";

const RISK_VERBOSE_DESC: &str = "Include per-signal decomposition (churn_score, churn_percentile, fix_ratio, total_commits, fix_count, dependent_files, coupled_files, test_gap, in_cycle, cycle_size) and coupled files (`top_coupled`) on each RiskAssessment; default false.";

/// Make an optional field required in the advertised schema. Retired
/// spellings are accepted for one release but not advertised, so the field a
/// client sees is the only way to name the input.
fn require(schema: &mut schemars::Schema, field: &str) {
    let required = schema
        .ensure_object()
        .entry("required")
        .or_insert_with(|| serde_json::json!([]));
    if let Some(list) = required.as_array_mut()
        && !list.iter().any(|name| name == field)
    {
        list.push(serde_json::json!(field));
    }
}

fn require_targets(schema: &mut schemars::Schema) {
    require(schema, "targets");
}

fn require_target(schema: &mut schemars::Schema) {
    require(schema, "target");
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(transform = require_targets)]
pub struct RiskParams {
    #[schemars(description = PROJECT_ARG_DESC)]
    pub project: String,
    #[schemars(description = RISK_TARGETS_DESC)]
    pub targets: Option<Vec<String>>,
    #[schemars(
        description = "false (default): score each target independently, in input order, under `files`. true: also roll the set up as one patch: max/mean over scored files, hotspot, fix-heavy, test-gap, and wide-blast lists, unscored files, cycles touching the patch, and paste-ready summary notes."
    )]
    pub aggregate: Option<bool>,
    #[schemars(description = RISK_VERBOSE_DESC)]
    pub verbose: Option<bool>,
    /// Retired list spelling of `targets` (formerly on `assess_risk_batch`
    /// and `assess_risk_diff`).
    #[schemars(skip)]
    pub file_paths: Option<Vec<String>>,
    /// Retired single-file spellings: one path is a batch of one.
    #[schemars(skip)]
    pub target: Option<String>,
    #[schemars(skip)]
    pub file_path: Option<String>,
}

impl RiskParams {
    /// Every spelling the call used must name the same set; picking one
    /// would answer a question the caller did not ask.
    pub(crate) fn target_args(&self) -> Result<Vec<String>> {
        let present: Vec<(&str, Vec<String>)> = [
            ("targets", self.targets.clone()),
            ("file_paths", self.file_paths.clone()),
            ("target", self.target.clone().map(|value| vec![value])),
            ("file_path", self.file_path.clone().map(|value| vec![value])),
        ]
        .into_iter()
        .filter_map(|(field, values)| values.map(|values| (field, values)))
        .collect();
        let Some((first_field, first)) = present.first() else {
            return Err(McpError::new(
                ErrorCode::Param,
                "assess_risk: pass `targets`, a list of file targets",
            )
            .into());
        };
        for (field, values) in &present[1..] {
            if !same_list(first, values) {
                return Err(McpError::new(
                    ErrorCode::Param,
                    format!(
                        "assess_risk: {}; pass one of them",
                        list_conflict(first_field, first, field, values)
                    ),
                )
                .into());
            }
        }
        Ok(first.clone())
    }

    pub(crate) fn list_fields(&self) -> &'static [&'static str] {
        list_fields(&self.file_paths, &self.targets)
    }

    /// True when the call named its file only through a retired single-file
    /// spelling (`target`, `file_path`), whose miss is refused with the
    /// nearest candidates rather than reported as an unindexed entry.
    pub(crate) fn single_spelling_only(&self) -> bool {
        self.targets.is_none()
            && self.file_paths.is_none()
            && (self.target.is_some() || self.file_path.is_some())
    }
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TestsForParams {
    #[schemars(description = PROJECT_ARG_DESC)]
    pub project: String,
    #[schemars(
        description = "DEPRECATED, still accepted: pass `targets`. Repo-relative file paths whose tests should be recommended. Must provide at least one path: an empty list is rejected."
    )]
    pub file_paths: Option<Vec<String>>,
    #[schemars(description = FILE_TARGETS_DESC)]
    pub targets: Option<Vec<String>>,
}

impl TestsForParams {
    pub(crate) fn target_args(&self) -> Result<Vec<String>> {
        one_target_list(
            "recommend_tests",
            "file_paths",
            self.file_paths.as_deref(),
            self.targets.as_deref(),
        )
    }

    pub(crate) fn list_fields(&self) -> &'static [&'static str] {
        list_fields(&self.file_paths, &self.targets)
    }
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SessionParams {
    #[schemars(description = PROJECT_ARG_DESC)]
    pub project: String,
    #[schemars(
        description = "start: persist the current structural state as the session's baseline (reusing an id replaces it). end: compare the current state with that baseline; the baseline stays in place for repeated comparisons."
    )]
    pub action: SessionAction,
    #[schemars(
        description = "Session identifier (alphanumerics, '-', '_', '.', max 128 chars). Use the same id for the matching start and end. Defaults to \"default\" when omitted."
    )]
    pub session_id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum SessionAction {
    Start,
    End,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TracePathParams {
    #[schemars(description = PROJECT_ARG_DESC)]
    pub project: String,
    #[schemars(
        description = "Origin symbol. Takes the shared target grammar: a `sym:` handle, an indexed path, `path:line`, or a qualified or bare name. There is no single `target` here — a chain needs both ends."
    )]
    pub from: String,
    #[schemars(description = "Symbol the chain should reach, in the same grammar as `from`.")]
    pub to: String,
    #[schemars(
        description = "Maximum hops to search before giving up (default 6, and 6 is also the ceiling over MCP — a larger value is capped and reported under `_meta.clamps`, so `bounded: true` at 6 cannot be retried deeper from here; use the `codesage trace` CLI for a longer search)"
    )]
    #[serde(default, deserialize_with = "deser_optional_usize")]
    pub max_depth: Option<usize>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FromTraceParams {
    #[schemars(description = PROJECT_ARG_DESC)]
    pub project: String,
    #[schemars(
        description = "The stack trace or sanitizer report, pasted verbatim (Python traceback, PHP fatal + `#N` frames or Xdebug `Call Stack`, Rust backtrace, Java/Kotlin `at` lines, Go goroutine dump, Node/JS `at` lines, gdb/ASan/UBSan `#N` frames). Chained reports (`Caused by:`, `During handling …`, `freed by thread`, several goroutines) are split into stacks. Unrecognized lines are skipped."
    )]
    pub trace: String,
    #[schemars(
        description = "Maximum frames returned, innermost-first (default 50, which is also the ceiling over MCP; a larger value is capped and reported under `_meta.clamps`; 0 means the default)"
    )]
    #[serde(default, deserialize_with = "deser_optional_usize")]
    pub limit: Option<usize>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ImpactParams {
    #[schemars(description = PROJECT_ARG_DESC)]
    pub project: String,
    #[schemars(
        description = "What to analyze, in the shared target grammar: a `sym:` or `file:` handle, an indexed path, `path:line`, or a qualified or bare name. `dir:`, `chunk:`, and `feat_` are refused with E_PARAM."
    )]
    pub target: String,
    /// Retired hint, accepted for one release and not advertised: the target
    /// grammar disambiguates. `false` forces symbol interpretation of a
    /// path-like target; a handle names its own kind and ignores it.
    #[schemars(skip)]
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
#[serde(deny_unknown_fields)]
#[schemars(transform = require_target)]
pub struct BundleParams {
    #[schemars(description = PROJECT_ARG_DESC)]
    pub project: String,
    #[schemars(
        description = "What to bundle, in the shared target grammar: a `feat_` id, `route:METHOD path`, or `cmd:name` reads the mapped feature's entrypoints, owned files, tests, and context; a `sym:`, `file:`, `dir:`, or `chunk:` handle, an indexed path, `path:line`, or a qualified or bare name reads that entity; anything else is a natural-language query answered by semantic search. Entity ambiguity and misses return resolver errors."
    )]
    pub target: Option<String>,
    #[schemars(
        description = "Max primary results to include (default 5, max 20); for a feature, max chunks per section. Over-max requests are capped and reported under `_meta.clamps`."
    )]
    #[serde(default, deserialize_with = "deser_optional_usize")]
    pub limit: Option<usize>,
    #[schemars(description = "Include caller code in the bundle")]
    pub include_callers: Option<bool>,
    #[schemars(description = "Include callee/dependency code in the bundle")]
    pub include_callees: Option<bool>,
    /// Retired spelling (formerly on `feature_bundle`): a feature lookup
    /// that reports `found: false` on a miss instead of falling through to
    /// semantic search.
    #[schemars(skip)]
    pub feature_id: Option<String>,
    /// Retired hint (formerly on `export_context`): for unresolved text,
    /// force symbol lookup instead of semantic search.
    #[schemars(skip)]
    pub is_symbol: Option<bool>,
}

impl BundleParams {
    pub(crate) fn target_arg(&self) -> Result<String> {
        let target = one_target(
            "bundle",
            "feature_id",
            self.feature_id.as_deref(),
            self.target.as_deref(),
        )?;
        if target.trim().is_empty() {
            return Err(McpError::new(
                ErrorCode::Param,
                "bundle: `target` is empty; pass an entity in the target grammar or a question",
            )
            .into());
        }
        Ok(target)
    }
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
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
    #[schemars(
        description = "When true, truncate results at the relevance cliff (largest relative score drop that rounds to 20% or more) instead of returning exactly `limit` rows; only truncates when `confidence` is `high`, a flat page comes back in full. The cut is page-local (computed over this page's rows only), so combining it with `offset` paging can skip rows between pages. `cliff_at`/`margin_pct` still disclose the cliff either way."
    )]
    #[serde(default)]
    pub adaptive_limit: Option<bool>,
    #[schemars(
        description = "Include per-row score traces and raw retrieval/reranker signals; default false, ranking unchanged."
    )]
    #[serde(default)]
    pub explain: Option<bool>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FeaturesParams {
    #[schemars(description = PROJECT_ARG_DESC)]
    pub project: String,
    #[schemars(
        description = "Only the features containing this file as entrypoint, owned code, context, or test: a `file:`, `sym:`, or `chunk:` handle, an indexed path (a leading `./` is ignored), or `path:line`; a symbol target resolves to its file. Omit to list every mapped feature. A spelling that matches no indexed path and no file in the working tree is E_NOT_FOUND with the nearest candidates."
    )]
    pub target: Option<String>,
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
        description = "Max results (0 = no limit, max 500). Defaults to 100 when listing and to no limit with `target`. Over-max requests are capped and reported under `_meta.clamps`."
    )]
    #[serde(default, deserialize_with = "deser_optional_usize")]
    pub limit: Option<usize>,
    /// Retired spelling of `target` (formerly on `find_feature`).
    #[schemars(skip)]
    pub file_path: Option<String>,
}

impl FeaturesParams {
    /// The file a lookup is scoped to, when the call names one.
    pub(crate) fn target_arg(&self) -> Result<Option<String>> {
        if self.file_path.is_none() && self.target.is_none() {
            return Ok(None);
        }
        one_target(
            "features",
            "file_path",
            self.file_path.as_deref(),
            self.target.as_deref(),
        )
        .map(Some)
    }
}

/// Input of the hidden `embed_texts` tool: the CLI's request to embed with
/// the daemon's resident session instead of a private one.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
pub struct RerankPairsParams {
    #[schemars(description = PROJECT_ARG_DESC)]
    pub project: String,
    pub model: String,
    pub device: String,
    pub query: String,
    pub documents: Vec<String>,
}

#[derive(Debug, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub struct RerankPairsResult {
    pub model: String,
    pub device: String,
    pub scores: Vec<f32>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProjectOverviewParams {
    #[schemars(description = PROJECT_ARG_DESC)]
    pub project: String,
    #[serde(default)]
    #[schemars(
        description = "Rank test files in `top_risk_files` too. Default false: files the index marks `is_test` are left out so hot test modules do not crowd out product code. `true` bypasses the shared ranking cache and computes a fresh ranking for this call."
    )]
    pub include_tests: bool,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReviewRehearsalParams {
    #[schemars(description = PROJECT_ARG_DESC)]
    pub project: String,
    #[schemars(
        description = "DEPRECATED, still accepted: pass `targets`. Repo-relative file paths in the patch / working-tree change set (typically `git diff --name-only`). Must provide at least one path: an empty list is rejected (the CLI's stdin/working-tree fallback does not exist over MCP)."
    )]
    pub file_paths: Option<Vec<String>>,
    #[schemars(description = FILE_TARGETS_DESC)]
    pub targets: Option<Vec<String>>,
}

impl ReviewRehearsalParams {
    pub(crate) fn target_args(&self) -> Result<Vec<String>> {
        one_target_list(
            "review_rehearsal",
            "file_paths",
            self.file_paths.as_deref(),
            self.targets.as_deref(),
        )
    }

    pub(crate) fn list_fields(&self) -> &'static [&'static str] {
        list_fields(&self.file_paths, &self.targets)
    }
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
    fn search_params_adaptive_limit_defaults_off_and_accepts_bool() {
        let p: SearchParams = serde_json::from_value(json!({
            "project": "/p",
            "query": "auth",
        }))
        .unwrap();
        assert_eq!(p.adaptive_limit, None);
        let p: SearchParams = serde_json::from_value(json!({
            "project": "/p",
            "query": "auth",
            "adaptive_limit": true,
        }))
        .unwrap();
        assert_eq!(p.adaptive_limit, Some(true));
    }

    #[test]
    fn search_explain_defaults_off_and_requires_a_boolean() {
        let input = json!({"project": "/p", "query": "auth"});
        let p: SearchParams = serde_json::from_value(input.clone()).unwrap();
        assert_eq!(p.explain, None);
        let mut input = input;
        input["explain"] = json!(true);
        let p: SearchParams = serde_json::from_value(input.clone()).unwrap();
        assert_eq!(p.explain, Some(true));
        input["explain"] = json!("true");
        assert!(serde_json::from_value::<SearchParams>(input).is_err());
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
    fn features_accepts_documented_kind_and_language_strings() {
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
            let p: FeaturesParams = serde_json::from_value(json!({
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
            let p: FeaturesParams = serde_json::from_value(json!({
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
    fn features_rejects_unknown_kind() {
        let r: Result<FeaturesParams, _> = serde_json::from_value(json!({
            "project": "/p",
            "kind": "microservice",
        }));
        assert!(r.is_err(), "unknown feature kind must error");
    }

    #[test]
    fn retired_spellings_on_consolidated_tools_name_one_entity() {
        let features =
            |value: serde_json::Value| -> FeaturesParams { serde_json::from_value(value).unwrap() };
        assert_eq!(
            features(json!({"project": "/p"})).target_arg().unwrap(),
            None
        );
        for spelling in [
            json!({"project": "/p", "target": "a.rs"}),
            json!({"project": "/p", "file_path": "a.rs"}),
            json!({"project": "/p", "file_path": "a.rs", "target": "a.rs"}),
        ] {
            assert_eq!(
                features(spelling.clone()).target_arg().unwrap().as_deref(),
                Some("a.rs"),
                "{spelling}"
            );
        }
        let err = features(json!({"project": "/p", "file_path": "a.rs", "target": "b.rs"}))
            .target_arg()
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("features") && err.contains("`file_path`"),
            "{err}"
        );

        let bundle =
            |value: serde_json::Value| -> BundleParams { serde_json::from_value(value).unwrap() };
        assert_eq!(
            bundle(json!({"project": "/p", "feature_id": "feat_1"}))
                .target_arg()
                .unwrap(),
            "feat_1"
        );
        let err = bundle(json!({"project": "/p", "feature_id": "feat_1", "target": "feat_2"}))
            .target_arg()
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("bundle") && err.contains("`feature_id`"),
            "{err}"
        );
        assert!(
            bundle(json!({"project": "/p", "target": "x", "is_symbol": true}))
                .is_symbol
                .unwrap()
        );

        for (action, expected) in [("start", SessionAction::Start), ("end", SessionAction::End)] {
            let p: SessionParams =
                serde_json::from_value(json!({"project": "/p", "action": action})).unwrap();
            assert_eq!(p.action, expected);
        }
        assert!(serde_json::from_value::<SessionParams>(json!({"project": "/p"})).is_err());
        assert!(
            serde_json::from_value::<SessionParams>(json!({"project": "/p", "action": "stop"}))
                .is_err()
        );

        let impact: ImpactParams =
            serde_json::from_value(json!({"project": "/p", "target": "a.rs", "is_file": false}))
                .unwrap();
        assert_eq!(impact.is_file, Some(false));
    }

    #[test]
    fn from_trace_requires_trace_and_accepts_string_limit() {
        let r: Result<FromTraceParams, _> = serde_json::from_value(json!({
            "project": "/p",
        }));
        assert!(r.is_err(), "a missing `trace` must be a param error");

        let p: FromTraceParams = serde_json::from_value(json!({
            "project": "/p",
            "trace": "   0: main\n",
            "limit": "5",
        }))
        .expect("numeric-string limit parses like every other integer param");
        assert_eq!(p.limit, Some(5));

        let r: Result<FromTraceParams, _> = serde_json::from_value(json!({
            "project": "/p",
            "trace": "x",
            "limit": "many",
        }));
        assert!(r.is_err(), "a non-numeric limit must error, not be dropped");
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

    #[test]
    fn every_params_struct_rejects_an_unknown_field_and_names_the_valid_set() {
        fn check<T: serde::de::DeserializeOwned>(struct_name: &str, mut valid: serde_json::Value) {
            let obj = valid.as_object_mut().unwrap();
            assert!(
                serde_json::from_value::<T>(serde_json::Value::Object(obj.clone())).is_ok(),
                "{struct_name}: the fixture must be valid before the unknown field is added"
            );
            obj.insert("max_results".to_string(), json!(5));
            let err = match serde_json::from_value::<T>(serde_json::Value::Object(obj.clone())) {
                Ok(_) => panic!("{struct_name} silently accepted unknown field `max_results`"),
                Err(e) => e.to_string(),
            };
            assert!(
                err.contains("max_results"),
                "{struct_name}: error must name the unknown field, got: {err}"
            );
            assert!(
                err.contains("expected"),
                "{struct_name}: error must say what was expected, got: {err}"
            );
            assert!(
                err.contains("`project`"),
                "{struct_name}: error must list the valid field set, got: {err}"
            );
        }

        check::<FindSymbolParams>("FindSymbolParams", json!({"project": "/p", "name": "x"}));
        check::<FindReferencesParams>(
            "FindReferencesParams",
            json!({"project": "/p", "name": "x"}),
        );
        check::<FindSimilarParams>("FindSimilarParams", json!({"project": "/p", "name": "x"}));
        check::<ListDependenciesParams>(
            "ListDependenciesParams",
            json!({"project": "/p", "file_path": "a.rs"}),
        );
        check::<CouplingParams>(
            "CouplingParams",
            json!({"project": "/p", "file_path": "a.rs"}),
        );
        check::<RiskParams>(
            "RiskParams",
            json!({"project": "/p", "targets": ["a.rs"], "aggregate": true}),
        );
        check::<TestsForParams>(
            "TestsForParams",
            json!({"project": "/p", "file_paths": ["a.rs"]}),
        );
        check::<SessionParams>("SessionParams", json!({"project": "/p", "action": "end"}));
        check::<TracePathParams>(
            "TracePathParams",
            json!({"project": "/p", "from": "a", "to": "b"}),
        );
        check::<FromTraceParams>(
            "FromTraceParams",
            json!({"project": "/p", "trace": "   0: main\n"}),
        );
        check::<ImpactParams>("ImpactParams", json!({"project": "/p", "target": "Foo"}));
        check::<BundleParams>("BundleParams", json!({"project": "/p", "target": "Foo"}));
        check::<SearchParams>("SearchParams", json!({"project": "/p", "query": "auth"}));
        check::<FeaturesParams>("FeaturesParams", json!({"project": "/p"}));
        check::<FeaturesParams>(
            "FeaturesParams",
            json!({"project": "/p", "file_path": "a.rs"}),
        );
        check::<BundleParams>(
            "BundleParams",
            json!({"project": "/p", "feature_id": "feat_1"}),
        );
        check::<EmbedTextsParams>(
            "EmbedTextsParams",
            json!({"project": "/p", "model": "m", "texts": []}),
        );
        check::<ProjectOverviewParams>("ProjectOverviewParams", json!({"project": "/p"}));
        check::<ReviewRehearsalParams>(
            "ReviewRehearsalParams",
            json!({"project": "/p", "file_paths": ["a.rs"]}),
        );
    }

    #[test]
    fn project_overview_include_tests_defaults_false_and_accepts_true() {
        let p: ProjectOverviewParams = serde_json::from_value(json!({"project": "/p"})).unwrap();
        assert!(!p.include_tests);
        let p: ProjectOverviewParams =
            serde_json::from_value(json!({"project": "/p", "include_tests": true})).unwrap();
        assert!(p.include_tests);
        assert!(
            serde_json::from_value::<ProjectOverviewParams>(
                json!({"project": "/p", "include_tests": "yes"})
            )
            .is_err(),
            "include_tests is a boolean"
        );
    }

    #[test]
    fn search_unknown_field_error_offers_limit_as_the_alternative() {
        let err = serde_json::from_value::<SearchParams>(json!({
            "project": "/p",
            "query": "auth",
            "max_results": 5,
        }))
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("`limit`"),
            "refusal must list `limit` as a valid field, got: {err}"
        );
    }

    #[test]
    fn a_legacy_argument_and_its_target_alias_name_one_entity() {
        // Either spelling alone.
        let by_name: FindSymbolParams =
            serde_json::from_value(json!({"project": "/p", "name": "search"})).unwrap();
        assert_eq!(by_name.target_arg().unwrap(), "search");
        let by_target: FindSymbolParams =
            serde_json::from_value(json!({"project": "/p", "target": "sym:src/a.rs#search"}))
                .unwrap();
        assert_eq!(by_target.target_arg().unwrap(), "sym:src/a.rs#search");

        // Both, equal: the caller repeated itself, which is not a conflict.
        let both: FindSymbolParams = serde_json::from_value(
            json!({"project": "/p", "name": " search ", "target": "search"}),
        )
        .unwrap();
        assert_eq!(both.target_arg().unwrap(), " search ");

        // Both, different: naming two entities in one call is a param error
        // quoting both, never a silent pick.
        let conflict: FindSymbolParams = serde_json::from_value(
            json!({"project": "/p", "name": "search", "target": "sym:src/a.rs#render"}),
        )
        .unwrap();
        let err = conflict.target_arg().unwrap_err().to_string();
        assert!(
            err.contains("find_symbol")
                && err.contains("`name`")
                && err.contains("search")
                && err.contains("sym:src/a.rs#render"),
            "{err}"
        );

        // Neither: a param error naming the preferred spelling.
        let missing: FindSymbolParams = serde_json::from_value(json!({"project": "/p"})).unwrap();
        let err = missing.target_arg().unwrap_err().to_string();
        assert!(err.contains("`target`") && err.contains("`name`"), "{err}");
    }

    #[test]
    fn file_and_file_set_aliases_follow_the_same_precedence() {
        // assess_risk takes one file set; each retired spelling (the batch and
        // diff `file_paths`, the single-file `target` and `file_path`) names it.
        let risk =
            |value: serde_json::Value| -> RiskParams { serde_json::from_value(value).unwrap() };
        for spelling in [
            json!({"project": "/p", "targets": ["src/a.rs"]}),
            json!({"project": "/p", "file_paths": ["src/a.rs"]}),
            json!({"project": "/p", "target": "src/a.rs"}),
            json!({"project": "/p", "file_path": "src/a.rs"}),
            json!({"project": "/p", "targets": ["src/a.rs"], "target": "src/a.rs"}),
            json!({"project": "/p", "file_paths": ["src/a.rs"], "file_path": " src/a.rs "}),
        ] {
            assert_eq!(
                risk(spelling.clone()).target_args().unwrap(),
                ["src/a.rs"],
                "{spelling}"
            );
        }
        for (conflict, fields) in [
            (
                json!({"project": "/p", "targets": ["src/a.rs"], "target": "src/b.rs"}),
                ["`targets`", "`target`"],
            ),
            (
                json!({"project": "/p", "targets": ["src/a.rs", "src/b.rs"], "file_path": "src/a.rs"}),
                ["`targets`", "`file_path`"],
            ),
            (
                json!({"project": "/p", "file_paths": ["src/a.rs"], "targets": ["src/b.rs"]}),
                ["`targets`", "`file_paths`"],
            ),
        ] {
            let err = risk(conflict).target_args().unwrap_err().to_string();
            assert!(
                err.contains("assess_risk")
                    && fields.iter().all(|field| err.contains(field))
                    && err.contains("pass one of them"),
                "{err}"
            );
        }
        let missing = risk(json!({"project": "/p"}))
            .target_args()
            .unwrap_err()
            .to_string();
        assert!(missing.contains("`targets`"), "{missing}");

        let by_paths: TestsForParams =
            serde_json::from_value(json!({"project": "/p", "file_paths": ["a.rs", "b.rs"]}))
                .unwrap();
        assert_eq!(by_paths.target_args().unwrap(), vec!["a.rs", "b.rs"]);
        assert_eq!(by_paths.list_fields(), ["file_paths"]);
        let by_targets: TestsForParams =
            serde_json::from_value(json!({"project": "/p", "targets": ["file:a.rs"]})).unwrap();
        assert_eq!(by_targets.target_args().unwrap(), vec!["file:a.rs"]);
        assert_eq!(by_targets.list_fields(), ["targets"]);
        let both: TestsForParams = serde_json::from_value(
            json!({"project": "/p", "file_paths": ["a.rs"], "targets": ["a.rs"]}),
        )
        .unwrap();
        assert_eq!(both.target_args().unwrap(), vec!["a.rs"]);
        // Both spellings carried the list, so an over-cap remedy has to cut
        // both: a retry that shortened one would be the conflict above.
        assert_eq!(both.list_fields(), ["file_paths", "targets"]);
        // A different set, including one that only adds an entry. The refusal
        // names where they part and quotes both spellings, since the lengths
        // alone do not say which entry to fix.
        for (targets, expected) in [
            (json!(["b.rs"]), "index 0: \"a.rs\" vs \"b.rs\""),
            (
                json!(["a.rs", "b.rs"]),
                "index 1: nothing (1 path) vs \"b.rs\"",
            ),
        ] {
            let conflict: TestsForParams = serde_json::from_value(
                json!({"project": "/p", "file_paths": ["a.rs"], "targets": targets}),
            )
            .unwrap();
            let err = conflict.target_args().unwrap_err().to_string();
            assert!(
                err.contains("recommend_tests") && err.contains("`file_paths`"),
                "{err}"
            );
            assert!(err.contains(expected), "{err}");
        }
        let missing: TestsForParams = serde_json::from_value(json!({"project": "/p"})).unwrap();
        let err = missing.target_args().unwrap_err().to_string();
        assert!(
            err.contains("`targets`") && err.contains("`file_paths`"),
            "{err}"
        );
        // An explicitly empty list is still the empty-input refusal, not a
        // missing argument: the two say different things to the caller.
        let empty: TestsForParams =
            serde_json::from_value(json!({"project": "/p", "file_paths": []})).unwrap();
        assert!(empty.target_args().unwrap().is_empty());
    }

    #[test]
    fn impact_params_accept_every_declared_field_with_stringy_numerics() {
        let p: ImpactParams = serde_json::from_value(json!({
            "project": "/p",
            "target": "Foo",
            "is_file": false,
            "depth": "3",
            "source_only": true,
            "include_forward": true,
            "include_siblings": false,
            "limit": "50",
            "summary_only": true,
        }))
        .unwrap();
        assert_eq!(p.depth, Some(3));
        assert_eq!(p.limit, Some(50));
        assert_eq!(p.summary_only, Some(true));
    }
}
