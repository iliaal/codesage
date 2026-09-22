//! Git history indexing, coupling, risk assessment, test recommendations, and authorship.

mod bus_factor;
mod indexer;
mod risk;
mod test_commands;
mod tests_rec;

pub use indexer::{
    IndexMode, changed_files_since, feature_touched_since, git_history_index,
    git_history_index_with_options,
};
pub(crate) use risk::CompletePolicy;
pub(crate) use risk::ImportCycles;
#[cfg(test)]
pub(crate) use risk::TOP_SYMBOLS_PASSES;
pub(crate) use risk::{RiskRequestScope, assess_risk_diff_with_walk_cache, assess_risk_with_scope};
pub use risk::{
    assess_risk, assess_risk_batch, assess_risk_diff, find_coupling, find_coupling_ranked,
};
pub(crate) use tests_rec::reach_cap_clause;
pub(crate) use tests_rec::recommend_tests_with_walk_cache;
pub use tests_rec::{
    ReachabilityOptions, abbreviate_paths, recommend_tests, recommend_tests_with_reachability,
};
