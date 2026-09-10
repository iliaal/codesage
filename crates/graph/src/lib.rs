pub mod branch_overlap;
mod brief;
mod bundle;
mod call_path;
pub mod drift;
pub mod edit_check;
mod git_history;
mod impact;
mod index;
mod lookups;
mod overview;
mod rehearsal;
mod scc;
mod search;
mod semantic;
mod session;
mod similar;
#[doc(hidden)]
pub mod state_file;
mod trace_locate;

pub use brief::build_edit_brief;
pub use bundle::{export_context, export_context_for_symbol, feature_bundle};
pub use call_path::trace_call_path;
pub use git_history::{
    IndexMode, ReachabilityOptions, abbreviate_paths, assess_risk, assess_risk_batch,
    assess_risk_diff, changed_files_since, feature_touched_since, find_coupling,
    find_coupling_ranked, git_history_index, git_history_index_with_options, recommend_tests,
    recommend_tests_with_reachability,
};
pub use impact::{impact_analysis, impact_analysis_report};
pub use index::{full_index, incremental_index, index_files, remove_files};
pub use lookups::{find_references, find_symbol, list_dependencies, list_dependencies_batch};
pub use overview::{build_project_overview, build_project_overview_with_top_risk};
pub use rehearsal::{build_branch_only_rehearsal, build_review_rehearsal};
pub use search::{RerankFn, search, search_page};
pub use semantic::{
    ArtifactLookup, EmbedderInit, LazyEmbedder, SemanticFingerprint, SemanticTableState,
    StaleSemanticTable, TextEmbedder, require_current_semantic_table, resolve_semantic_fingerprint,
    resolve_semantic_fingerprint_for_artifacts, semantic_full_index, semantic_incremental_index,
    semantic_index_files, semantic_remove_files, semantic_table_state, summarize_paths,
};
pub use session::{
    CompleteRiskRanking, IncompleteRiskRanking, build_session_snapshot_with_top_risk,
    persist_session_snapshot, session_end, session_start, top_risk_files, top_risk_ranking,
    top_risk_ranking_with_policy,
};
pub use similar::find_similar;
pub use trace_locate::from_trace;
