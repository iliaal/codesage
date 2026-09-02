mod brief;
mod bundle;
mod call_path;
pub mod drift;
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

pub use brief::build_edit_brief;
pub use bundle::{export_context, export_context_for_symbol, feature_bundle};
pub use call_path::trace_call_path;
pub use git_history::{
    IndexMode, assess_risk, assess_risk_batch, assess_risk_diff, changed_files_since,
    feature_touched_since, find_coupling, git_history_index, git_history_index_with_options,
    recommend_tests,
};
pub use impact::{impact_analysis, impact_analysis_report};
pub use index::{full_index, incremental_index, index_files, remove_files};
pub use lookups::{find_references, find_symbol, list_dependencies};
pub use overview::build_project_overview;
pub use rehearsal::build_review_rehearsal;
pub use search::{RerankFn, search};
pub use semantic::{
    EmbedderInit, LazyEmbedder, SemanticFingerprint, TextEmbedder, semantic_full_index,
    semantic_incremental_index, semantic_index_files, semantic_remove_files,
};
pub use session::{session_end, session_start, top_risk_files};
pub use similar::find_similar;
