//! Stable feature IDs. `feature_id` shape is `feat_<16-hex>` derived from
//! the seed's identity tuple (kind, source, entry_path, command|route|symbol).
//! Re-running the mapper against the same repo produces the same id so an
//! agent can quote it across sessions.

use codesage_protocol::FeatureKind;
use sha2::{Digest, Sha256};

pub fn build(kind: FeatureKind, source: &str, entry_path: &str, discriminator: &str) -> String {
    let mut h = Sha256::new();
    h.update(b"feat|");
    h.update(kind.as_str().as_bytes());
    h.update(b"|");
    h.update(source.as_bytes());
    h.update(b"|");
    h.update(entry_path.as_bytes());
    h.update(b"|");
    h.update(discriminator.as_bytes());
    let digest = h.finalize();
    let hex = hex::encode(&digest[..8]);
    format!("feat_{hex}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_is_stable_for_same_inputs() {
        let a = build(
            FeatureKind::CliCommand,
            "cargo-bin",
            "src/main.rs",
            "codesage",
        );
        let b = build(
            FeatureKind::CliCommand,
            "cargo-bin",
            "src/main.rs",
            "codesage",
        );
        assert_eq!(a, b);
        assert!(a.starts_with("feat_"));
        assert_eq!(a.len(), 5 + 16, "feat_ + 16 hex chars");
    }

    #[test]
    fn id_differs_when_kind_changes() {
        let a = build(FeatureKind::CliCommand, "cargo-bin", "src/main.rs", "x");
        let b = build(FeatureKind::Library, "cargo-bin", "src/main.rs", "x");
        assert_ne!(a, b);
    }

    #[test]
    fn id_differs_when_entry_changes() {
        let a = build(FeatureKind::CliCommand, "cargo-bin", "src/main.rs", "x");
        let b = build(FeatureKind::CliCommand, "cargo-bin", "src/main2.rs", "x");
        assert_ne!(a, b);
    }
}
