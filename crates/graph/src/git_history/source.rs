use std::collections::BTreeSet;
use std::path::Path;

use anyhow::{Context, Result, bail, ensure};
use codesage_storage::Database;

use crate::git::git_history_command;

#[derive(Debug, Default)]
pub(super) struct HistorySource {
    pub tree_only: bool,
    pub shallow: BTreeSet<String>,
}

const TREE_ONLY_NOTE: &str = "partial-clone history is tree-only: line churn and rename continuity are unavailable; churn is omitted from the risk score; missing blobs were not fetched";
const SHALLOW_NOTE: &str = "history was truncated by a shallow clone; boundary commits count in total_commits and last_commit_at but contribute no churn, fixes, authorship, or co-change pairs";
pub(super) const SHALLOW_CHURN_OMITTED_NOTE: &str =
    "churn risk omitted: no line churn was observed in the available shallow history for this file";

impl HistorySource {
    pub fn read(root: &Path) -> Result<Self> {
        let partial = config(root, &["--get", "extensions.partialclone"])?;
        let promisors = config(
            root,
            &["--type=bool", "--get-regexp", "^remote\\..*\\.promisor$"],
        )?;
        let tree_only = partial.is_some()
            || promisors.is_some_and(|value| value.lines().any(|line| line.ends_with(" true")));
        let output = git_history_command(root)
            .args(["rev-parse", "--git-path", "shallow"])
            .output()
            .context("locating shallow history boundary")?;
        ensure!(
            output.status.success(),
            "cannot locate shallow history boundary"
        );
        let path =
            String::from_utf8(output.stdout).context("shallow boundary path is not UTF-8")?;
        let path = root.join(path.trim());
        let raw = match std::fs::read_to_string(&path) {
            Ok(raw) => raw,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(error) => return Err(error).context("reading shallow history boundary"),
        };
        let mut shallow = BTreeSet::new();
        for sha in raw.split_whitespace() {
            ensure!(
                matches!(sha.len(), 40 | 64) && sha.bytes().all(|b| b.is_ascii_hexdigit()),
                "invalid shallow history boundary SHA"
            );
            shallow.insert(sha.to_owned());
        }
        Ok(Self { tree_only, shallow })
    }

    pub fn fingerprint(&self, policy: &str) -> String {
        if !self.tree_only && self.shallow.is_empty() {
            return policy.to_owned();
        }
        format!(
            "history:tree-only={};shallow={}\0{policy}",
            self.tree_only,
            self.shallow.iter().cloned().collect::<Vec<_>>().join(",")
        )
    }

    pub fn notes(&self) -> Vec<String> {
        let mut notes = Vec::new();
        if self.tree_only {
            notes.push(TREE_ONLY_NOTE.to_owned());
        }
        if !self.shallow.is_empty() {
            notes.push(SHALLOW_NOTE.to_owned());
        }
        notes
    }
}

pub(super) fn indexed_history_source(db: &Database) -> Result<HistorySource> {
    let Some(fingerprint) = db.git_index_exclusion_fingerprint()? else {
        return Ok(HistorySource::default());
    };
    let Some((source, _)) = fingerprint.split_once('\0') else {
        return Ok(HistorySource::default());
    };
    let Some(source) = source.strip_prefix("history:tree-only=") else {
        return Ok(HistorySource::default());
    };
    let Some((tree_only, shallow)) = source.split_once(";shallow=") else {
        return Ok(HistorySource::default());
    };
    Ok(HistorySource {
        tree_only: tree_only == "true",
        shallow: shallow
            .split(',')
            .filter(|sha| !sha.is_empty())
            .map(str::to_owned)
            .collect(),
    })
}

fn config(root: &Path, args: &[&str]) -> Result<Option<String>> {
    let output = git_history_command(root)
        .arg("config")
        .args(args)
        .output()
        .context("reading history clone configuration")?;
    if output.status.code() == Some(1) {
        return Ok(None);
    }
    if !output.status.success() {
        bail!(
            "cannot read history clone configuration: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(Some(String::from_utf8(output.stdout)?))
}
