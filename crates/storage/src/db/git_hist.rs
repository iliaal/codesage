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
        super::get_index_state(&self.conn, "git_index_state")
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

    /// Paths from `git_files` ordered by churn_score desc, capped at `limit`.
    /// Used to bound the candidate set for top-risk scoring before the
    /// per-file blast-radius BFS. Empty when git history isn't indexed.
    pub fn top_churn_files(&self, limit: usize) -> Result<Vec<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT path FROM git_files ORDER BY churn_score DESC, path LIMIT ?1")?;
        let rows: Vec<String> = stmt
            .query_map(rusqlite::params![limit as i64], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Record the commit SHA we just indexed up to. indexed_at stamped with unixepoch().
    pub fn set_git_index_state(&self, sha: &str) -> Result<()> {
        super::set_index_state(&self.conn, "git_index_state", sha)
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

    /// Co-change weight for a file pair (symmetric — caller need not pre-sort).
    /// Returns 0.0 when the pair has no recorded co-change: either the two files
    /// have never been observed changing together, or git history isn't indexed.
    pub fn co_change_weight(&self, file_a: &str, file_b: &str) -> Result<f64> {
        let (lo, hi) = if file_a <= file_b {
            (file_a, file_b)
        } else {
            (file_b, file_a)
        };
        match self.conn.query_row(
            "SELECT weight FROM git_co_changes WHERE file_a = ?1 AND file_b = ?2",
            rusqlite::params![lo, hi],
            |r| r.get::<_, f64>(0),
        ) {
            Ok(w) => Ok(w),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(0.0),
            Err(e) => Err(e.into()),
        }
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
        for row in stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })? {
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
             ) ORDER BY weight DESC, other LIMIT ?2",
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

    /// Churn percentile for every path in `git_files`, in one query. Bulk
    /// counterpart of [`Database::churn_percentile`] for callers scoring many
    /// files at once. `CUME_DIST()` is defined as
    /// `count(rows with value <= current) / count(*)` — the exact formula the
    /// per-file query computes, ties included — so both paths return identical
    /// values. Paths absent from the map score 0.0, matching the per-file
    /// no-row fallback.
    pub fn churn_percentiles(&self) -> Result<std::collections::HashMap<String, f64>> {
        let mut stmt = self
            .conn
            .prepare("SELECT path, CUME_DIST() OVER (ORDER BY churn_score) FROM git_files")?;
        let mut out = std::collections::HashMap::new();
        for row in stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, f64>(1)?)))? {
            let (path, pct) = row?;
            out.insert(path, pct);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::Database;

    /// Tied weights at the LIMIT boundary must not let the cap pick an
    /// arbitrary subset: without a secondary sort key, which peers survive
    /// `LIMIT` is whatever order SQLite produced, so an agent re-running the
    /// same query can get a different answer with no underlying change.
    #[test]
    fn co_changes_break_weight_ties_deterministically_under_limit() {
        let db = Database::open_in_memory().unwrap();
        // Five peers all at the same weight; only three fit under the cap.
        // Pairs are stored sorted (file_a < file_b); every peer sorts before
        // "target.rs", so these all land on the UNION's second branch.
        for other in ["e.rs", "c.rs", "a.rs", "d.rs", "b.rs"] {
            db.upsert_git_co_change(other, "target.rs", 1.0, 3, Some(1_700_000_000))
                .unwrap();
        }

        let first = db.co_changes_for("target.rs", 3).unwrap();
        let files: Vec<&str> = first.iter().map(|r| r.file.as_str()).collect();
        assert_eq!(
            files,
            vec!["a.rs", "b.rs", "c.rs"],
            "tied peers must be capped in a total order, not an arbitrary one"
        );

        for _ in 0..5 {
            let again = db.co_changes_for("target.rs", 3).unwrap();
            let again_files: Vec<&str> = again.iter().map(|r| r.file.as_str()).collect();
            assert_eq!(files, again_files, "repeated identical query changed order");
        }
    }

    /// The tie-breaking `path` term must be served by the index, not by a
    /// full scan into a temp b-tree sorter. Widening idx_git_files_churn to
    /// (churn_score DESC, path) in migration 0014 is what keeps this a
    /// streaming scan that can stop at LIMIT.
    #[test]
    fn top_churn_query_plan_uses_the_index_without_a_sorter() {
        let db = Database::open_in_memory().unwrap();
        for i in 0..200 {
            db.upsert_git_file(&format!("f{i}.rs"), 2.0, 0, 1, None)
                .unwrap();
        }
        db.conn.execute_batch("ANALYZE;").unwrap();

        let mut stmt = db
            .conn
            .prepare(
                "EXPLAIN QUERY PLAN SELECT path FROM git_files \
                 ORDER BY churn_score DESC, path LIMIT 10",
            )
            .unwrap();
        let plan: String = stmt
            .query_map([], |r| r.get::<_, String>(3))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
            .join(" | ");

        assert!(
            plan.contains("idx_git_files_churn"),
            "top-churn query should ride the churn index, plan was: {plan}"
        );
        assert!(
            !plan.to_uppercase().contains("TEMP B-TREE"),
            "index should satisfy the ORDER BY without a sorter, plan was: {plan}"
        );
    }

    /// Same hazard on the top-churn candidate set that bounds risk scoring.
    #[test]
    fn top_churn_files_break_score_ties_deterministically() {
        let db = Database::open_in_memory().unwrap();
        for p in ["e.rs", "c.rs", "a.rs", "d.rs", "b.rs"] {
            db.upsert_git_file(p, 2.0, 0, 1, None).unwrap();
        }

        let top = db.top_churn_files(3).unwrap();
        assert_eq!(top, vec!["a.rs", "b.rs", "c.rs"]);
        for _ in 0..5 {
            assert_eq!(top, db.top_churn_files(3).unwrap());
        }
    }

    #[test]
    fn churn_percentiles_bit_identical_to_per_file_query_with_ties() {
        let db = Database::open_in_memory().unwrap();
        // Three-way tie at 2.0 plus distinct low/high values: CUME_DIST must
        // reproduce count(churn <= x)/count(*) exactly for every tie peer.
        for (p, c) in [
            ("a", 1.0_f64),
            ("b", 2.0),
            ("c", 2.0),
            ("d", 2.0),
            ("e", 5.0),
            ("f", 0.5),
        ] {
            db.upsert_git_file(p, c, 0, 1, None).unwrap();
        }

        let bulk = db.churn_percentiles().unwrap();
        assert_eq!(bulk.len(), 6);
        for p in ["a", "b", "c", "d", "e", "f"] {
            let single = db.churn_percentile(p).unwrap();
            let batch = bulk[p];
            assert_eq!(
                single.to_bits(),
                batch.to_bits(),
                "path {p}: per-file {single} != bulk {batch}"
            );
        }
        // Spot-check the tie group lands on the last-peer rank: 5 of 6 rows
        // have churn <= 2.0.
        assert_eq!(bulk["c"].to_bits(), (5.0_f64 / 6.0).to_bits());
    }

    #[test]
    fn churn_percentiles_empty_table_returns_empty_map() {
        let db = Database::open_in_memory().unwrap();
        assert!(db.churn_percentiles().unwrap().is_empty());
    }
}
