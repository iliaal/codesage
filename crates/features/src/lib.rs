//! Deterministic feature mapping and trust-boundary derivation from parsed references.

pub mod feature_id;
pub mod mapper;
pub mod mappers;
pub mod nearby_tests;
pub mod trust_boundary;
pub mod trust_boundary_rules;

pub use mapper::{FeatureMapOutcome, map_features, map_features_detailed};
pub use trust_boundary::{derive_for_file, derive_for_files, derive_for_index, store_for_file};
