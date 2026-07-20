//! CodeSage feature mapping + trust-boundary derivation.
//!
//! Two responsibilities, sharing the per-language rule tables:
//!
//! - **Trust boundaries** (this slice): map a file's already-extracted imports
//!   and calls to a set of [`codesage_protocol::TrustBoundary`] tags. The
//!   tags compose into [`codesage_protocol::RiskAssessment`] as a new term
//!   and surface via `codesage trust-boundaries <file>` and the
//!   `assess_risk` MCP tool.
//!
//! - **Feature slices** (next slice): map a repo into behavior-keyed bundles
//!   (entrypoint + owned files + context files + tests + trust boundaries).
//!   Per-language mappers (PHP, C, C++, Rust, Python, JS/TS, Go) seed
//!   deterministic `FeatureRecord`s. See `crates/features/src/mappers/`.

pub mod feature_id;
pub mod mapper;
pub mod mappers;
pub mod nearby_tests;
pub mod trust_boundary;
pub mod trust_boundary_rules;

pub use mapper::{FeatureMapOutcome, map_features, map_features_detailed};
pub use trust_boundary::{derive_for_file, derive_for_files, derive_for_index, store_for_file};
