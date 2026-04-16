//! Git history subsystem (V2b).
//!
//! Split into three submodules:
//! - `indexer`: builds the `git_files` / `git_co_changes` / `git_index_state`
//!   tables from `git log`. Owns the subprocess call, log parser, churn/decay
//!   math, and transaction wrapping. Full + incremental paths.
//! - `risk`: query-time consumers. `find_coupling`, `assess_risk`,
//!   `assess_risk_diff`. Read-only against the tables the indexer populates.
//! - `tests_rec`: `recommend_tests` + language-specific sibling-test heuristics.
//!   `risk` depends on one exported helper here (`test_sibling_exists`).

mod indexer;
mod risk;
mod tests_rec;

pub use indexer::{IndexMode, git_history_index, git_history_index_with_options};
pub use risk::{assess_risk, assess_risk_diff, find_coupling};
pub use tests_rec::recommend_tests;
