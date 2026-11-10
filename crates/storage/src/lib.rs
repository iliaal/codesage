pub mod db;
pub mod schema;

pub use db::{
    Database, PathSidecarVerdicts, RawSearchRow, SemanticAttestation, SemanticValidityToken,
    StaleSemanticFingerprint, embedding_to_bytes, is_unique_violation,
};
