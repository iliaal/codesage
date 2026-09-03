pub mod db;
pub mod schema;

pub use db::{
    Database, RawSearchRow, SemanticAttestation, SemanticValidityToken, StaleSemanticFingerprint,
    embedding_to_bytes,
};
