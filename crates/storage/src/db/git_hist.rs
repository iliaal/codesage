//! V2b git history tables: `git_files`, `git_co_changes`, `git_index_state`.

use anyhow::Result;

use super::Database;

#[derive(Debug, Clone)]
pub struct GitFileRow {
    pub path: String,
    pub churn_score: f64,
    pub fix_count: u32,
    pub total_commits: u32,
    pub last_commit_at: Option<i64>,
}

#[derive(Debug, Clone)]
pub struct CoChangeRow {
    pub file: String,
    pub weight: f64,
    pub count: u32,
    pub last_observed_at: Option<i64>,
}

impl Database {
    /// UPSERT a git_files row. Re-running the indexer must replace prior values, not stack.
    pub fn upsert_git_file(
        &self,
        path: &str,
        churn_score: f64,
        fix_count: u32,
        total_commits: u32,
        last_commit_at: Option<i64>,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO git_files (path, churn_score, fix_count, total_commits, last_commit_at)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(path) DO UPDATE SET
                 churn_score = excluded.churn_score,
                 fix_count = excluded.fix_count,
                 total_commits = excluded.total_commits,
                 last_commit_at = excluded.last_commit_at,
                 indexed_at = unixepoch()",
            rusqlite::params![path, churn_score, fix_count, total_commits, last_commit_at],
        )?;
        Ok(())
    }

    /// UPSERT a co-change pair. Caller must ensure file_a < file_b lexicographically so
    /// each pair is stored exactly once.
    pub fn upsert_git_co_change(
        &self,
        file_a: &str,
        file_b: &str,
        weight: f64,
        count: u32,
        last_observed_at: Option<i64>,
    ) -> Result<()> {
        debug_assert!(
            file_a < file_b,
            "co-change pair must be sorted: {file_a} >= {file_b}"
        );
        self.conn.execute(
            "INSERT INTO git_co_changes (file_a, file_b, weight, count, last_observed_at)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(file_a, file_b) DO UPDATE SET
                 weight = excluded.weight,
                 count = excluded.count,
                 last_observed_at = excluded.last_observed_at",
            rusqlite::params![file_a, file_b, weight, count, last_observed_at],
        )?;
        Ok(())
    }

    /// Wipe all git data. Indexer should call before a fresh full pass to avoid stale rows
    /// for files that were renamed/deleted.
    pub fn clear_git_data(&self) -> Result<()> {
        self.conn.execute("DELETE FROM git_files", [])?;
        self.conn.execute("DELETE FROM git_co_changes", [])?;
        self.conn.execute("DELETE FROM git_index_state", [])?;
        Ok(())
    }

    /// Return (last_sha, last_indexed_at_unix) if an incremental state exists.
    pub fn get_git_index_state(&self) -> Result<Option<(String, i64)>> {
        let row = self.conn.query_row(
            "SELECT last_sha, last_indexed_at FROM git_index_state WHERE id = 1",
            [],
            |r| Ok((r.get::<_, Option<String>>(0)?, r.get::<_, i64>(1)?)),
        );
        match row {
            Ok((Some(sha), at)) if !sha.is_empty() => Ok(Some((sha, at))),
            Ok(_) => Ok(None),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// All file paths in `git_files` whose path begins with `prefix`. Used by
    /// the test recommender to enumerate Rust integration tests under a crate's
    /// `tests/` directory, where there's no per-file naming convention to lean on.
    pub fn git_files_with_prefix(&self, prefix: &str) -> Result<Vec<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT path FROM git_files WHERE path LIKE ?1 ORDER BY path")?;
        let pattern = format!("{prefix}%");
        let rows: Vec<String> = stmt
            .query_map(rusqlite::params![pattern], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Record the commit SHA we just indexed up to. indexed_at stamped with unixepoch().
    pub fn set_git_index_state(&self, sha: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO git_index_state (id, last_sha, last_indexed_at)
             VALUES (1, ?1, unixepoch())
             ON CONFLICT(id) DO UPDATE SET
                 last_sha = excluded.last_sha,
                 last_indexed_at = excluded.last_indexed_at",
            rusqlite::params![sha],
        )?;
        Ok(())
    }

    /// Apply a global multiplicative decay factor to existing churn and co-change weights.
    /// Used in incremental mode to age rows to "now" before adding new-commit deltas.
    pub fn scale_git_decay(&self, factor: f64) -> Result<()> {
        self.conn.execute(
            "UPDATE git_files SET churn_score = churn_score * ?1",
            rusqlite::params![factor],
        )?;
        self.conn.execute(
            "UPDATE git_co_changes SET weight = weight * ?1",
            rusqlite::params![factor],
        )?;
        Ok(())
    }

    /// Additive upsert: add the given counters to any existing row. Timestamp
    /// takes the newer of existing or proposed. Used by incremental mode.
    pub fn incr_git_file(
        &self,
        path: &str,
        churn_delta: f64,
        fix_delta: u32,
        commits_delta: u32,
        last_commit_at: Option<i64>,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO git_files (path, churn_score, fix_count, total_commits, last_commit_at)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(path) DO UPDATE SET
                 churn_score = churn_score + excluded.churn_score,
                 fix_count = fix_count + excluded.fix_count,
                 total_commits = total_commits + excluded.total_commits,
                 last_commit_at = CASE
                     WHEN excluded.last_commit_at IS NULL THEN last_commit_at
                     WHEN last_commit_at IS NULL THEN excluded.last_commit_at
                     ELSE MAX(last_commit_at, excluded.last_commit_at)
                 END,
                 indexed_at = unixepoch()",
            rusqlite::params![path, churn_delta, fix_delta, commits_delta, last_commit_at],
        )?;
        Ok(())
    }

    /// Additive upsert for a co-change pair. See `incr_git_file` for semantics.
    pub fn incr_git_co_change(
        &self,
        file_a: &str,
        file_b: &str,
        weight_delta: f64,
        count_delta: u32,
        last_observed_at: Option<i64>,
    ) -> Result<()> {
        debug_assert!(file_a < file_b);
        self.conn.execute(
            "INSERT INTO git_co_changes (file_a, file_b, weight, count, last_observed_at)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(file_a, file_b) DO UPDATE SET
                 weight = weight + excluded.weight,
                 count = count + excluded.count,
                 last_observed_at = CASE
                     WHEN excluded.last_observed_at IS NULL THEN last_observed_at
                     WHEN last_observed_at IS NULL THEN excluded.last_observed_at
                     ELSE MAX(last_observed_at, excluded.last_observed_at)
                 END",
            rusqlite::params![file_a, file_b, weight_delta, count_delta, last_observed_at],
        )?;
        Ok(())
    }

    /// True if a co-change pair already exists in the DB. `file_a` must be the lexicographic
    /// lower end. Used by incremental indexing to decide whether a sub-threshold pair should
    /// be upserted (existing pairs keep accumulating) or dropped (new noise below threshold).
    pub fn co_change_pair_exists(&self, file_a: &str, file_b: &str) -> Result<bool> {
        debug_assert!(file_a < file_b);
        let n: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM git_co_changes WHERE file_a = ?1 AND file_b = ?2",
            rusqlite::params![file_a, file_b],
            |r| r.get(0),
        )?;
        Ok(n > 0)
    }

    /// Preload every existing co-change pair as `file_a -> {file_b}`. Incremental
    /// indexing uses this instead of `co_change_pair_exists` per pair, replacing
    /// N round-trips inside the write transaction with one sequential scan before
    /// it. HashMap<HashSet> is chosen so membership probes don't need to allocate
    /// a tuple key: `existing.get(a).is_some_and(|rhs| rhs.contains(b))`.
    pub fn all_co_change_pairs(
        &self,
    ) -> Result<std::collections::HashMap<String, std::collections::HashSet<String>>> {
        use std::collections::{HashMap, HashSet};
        let mut stmt = self
            .conn
            .prepare("SELECT file_a, file_b FROM git_co_changes")?;
        let mut out: HashMap<String, HashSet<String>> = HashMap::new();
        for row in stmt
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
        {
            let (a, b) = row?;
            out.entry(a).or_default().insert(b);
        }
        Ok(out)
    }

    /// Fetch git_files row for one path, if present.
    pub fn git_file(&self, path: &str) -> Result<Option<GitFileRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT path, churn_score, fix_count, total_commits, last_commit_at
             FROM git_files WHERE path = ?1",
        )?;
        let mut rows = stmt.query(rusqlite::params![path])?;
        if let Some(row) = rows.next()? {
            Ok(Some(GitFileRow {
                path: row.get(0)?,
                churn_score: row.get(1)?,
                fix_count: row.get::<_, i64>(2)? as u32,
                total_commits: row.get::<_, i64>(3)? as u32,
                last_commit_at: row.get(4)?,
            }))
        } else {
            Ok(None)
        }
    }

    /// Top N files that historically co-change with `path`. Returns the OTHER file in each
    /// pair, weight-sorted descending.
    pub fn co_changes_for(&self, path: &str, limit: usize) -> Result<Vec<CoChangeRow>> {
        // Pair is stored with file_a < file_b. For a given path, results live on
        // either side, so query both columns and union-rank.
        let mut stmt = self.conn.prepare(
            "SELECT other, weight, count, last_observed_at FROM (
                 SELECT file_b AS other, weight, count, last_observed_at
                 FROM git_co_changes WHERE file_a = ?1
                 UNION ALL
                 SELECT file_a AS other, weight, count, last_observed_at
                 FROM git_co_changes WHERE file_b = ?1
             ) ORDER BY weight DESC LIMIT ?2",
        )?;
        let rows = stmt
            .query_map(rusqlite::params![path, limit as i64], |row| {
                Ok(CoChangeRow {
                    file: row.get(0)?,
                    weight: row.get(1)?,
                    count: row.get::<_, i64>(2)? as u32,
                    last_observed_at: row.get(3)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Compute churn percentile for a single path using all git_files churn scores.
    /// Returns 0.0..=1.0 where 1.0 means highest churn observed.
    pub fn churn_percentile(&self, path: &str) -> Result<f64> {
        let target: Option<f64> = match self.conn.query_row(
            "SELECT churn_score FROM git_files WHERE path = ?1",
            rusqlite::params![path],
            |r| r.get(0),
        ) {
            Ok(v) => Some(v),
            Err(rusqlite::Error::QueryReturnedNoRows) => None,
            Err(e) => return Err(e.into()),
        };
        let Some(target) = target else {
            return Ok(0.0);
        };
        let (lower, total): (i64, i64) = self.conn.query_row(
            "SELECT
                 SUM(CASE WHEN churn_score <= ?1 THEN 1 ELSE 0 END),
                 COUNT(*)
             FROM git_files",
            rusqlite::params![target],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        if total == 0 {
            return Ok(0.0);
        }
        Ok(lower as f64 / total as f64)
    }
}
