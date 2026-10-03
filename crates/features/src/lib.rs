//! Deterministic feature mapping and trust-boundary derivation from parsed references.

pub mod feature_id;
mod java_roles;
pub mod mapper;
pub mod mappers;
pub mod nearby_tests;
pub mod trust_boundary;
pub mod trust_boundary_rules;

pub use mapper::{FeatureMapOutcome, map_features, map_features_detailed};

/// Bump when mapper output (ids, routes, tags, file sets) changes for
/// unchanged input, so `codesage index` re-runs mapping instead of skipping
/// an unchanged tree.
pub const MAPPER_OUTPUT_VERSION: u32 = 8;
pub use trust_boundary::{derive_for_file, derive_for_files, derive_for_index, store_for_file};
