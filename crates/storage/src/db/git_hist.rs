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
    /// Oldest shared commit; `None` on rows written before the column existed.
    pub first_observed_at: Option<i64>,
    /// Bit `i` set when a shared commit fell in fixed-epoch 90-day window
    /// `(ts / 90d) % 64`. 0 on rows written before the column existed.
    pub window_mask: u64,
    /// `window_mask.count_ones().max(1)`: distinct 90-day windows in which
    /// the pair co-changed. Derived on every write so SQL can order by it.
    pub windows: u32,
    /// `git_files.total_commits` for `file` (the other side of the pair);
    /// 0 when that row is missing. Denominator for the reverse confidence.
    pub other_commits: u32,
}

/// Full set of per-pair counters written by the git-history indexer.
#[derive(Debug, Clone, Copy, Default)]
pub struct CoChangeWrite {
    pub weight: f64,
    pub count: u32,
    pub window_mask: u64,
    pub first_observed_at: Option<i64>,
    pub last_observed_at: Option<i64>,
}

fn windows_of(mask: u64) -> u32 {
    mask.count_ones().max(1)
}

/// Rank multiplier applied to a pair's weight when its shared commits span
/// less than [`RECURRING_SPAN_SECS`] (or the span is unknown on a legacy
/// row): a pair that kept co-changing over a month or more outranks a one-off
/// mass commit of equal raw weight. `1.0` disables the demotion.
pub const ONE_OFF_RANK_MULTIPLIER: f64 = 0.5;

/// Minimum `last_observed_at - first_observed_at` for a pair to count as
/// recurring: 30 days. Window count alone is not used, because two commits
/// seconds apart can straddle a fixed 90-day grid boundary.
pub const RECURRING_SPAN_SECS: i64 = 30 * 86_400;

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

    /// Order a co-change pair for storage. Pairs are stored once with
    /// `file_a < file_b` lexicographically; the write path normalizes here
    /// instead of trusting callers (a `debug_assert` was a no-op in release,
    /// letting a reversed pair silently insert a mirrored duplicate row).
    /// A self-pair (`file_a == file_b`) is meaningless — error, don't store.
    fn order_co_change_pair<'a>(file_a: &'a str, file_b: &'a str) -> Result<(&'a str, &'a str)> {
        if file_a == file_b {
            anyhow::bail!("co-change pair must be two distinct files, got {file_a:?} twice");
        }
        if file_a < file_b {
            Ok((file_a, file_b))
        } else {
            Ok((file_b, file_a))
        }
    }

    /// UPSERT a co-change pair with no recurrence data (`window_mask = 0`,
    /// `first_observed_at = last_observed_at`). Pair order is normalized (see
    /// [`Database::order_co_change_pair`]); a self-pair errors. Seeded
    /// fixtures use this; the indexer writes through
    /// [`Database::upsert_git_co_change_full`].
    pub fn upsert_git_co_change(
        &self,
        file_a: &str,
        file_b: &str,
        weight: f64,
        count: u32,
        last_observed_at: Option<i64>,
    ) -> Result<()> {
        self.upsert_git_co_change_full(
            file_a,
            file_b,
            &CoChangeWrite {
                weight,
                count,
                window_mask: 0,
                first_observed_at: last_observed_at,
                last_observed_at,
            },
        )
    }

    /// UPSERT a co-change pair with every counter, replacing prior values.
    /// `windows` is derived from the mask.
    pub fn upsert_git_co_change_full(
        &self,
        file_a: &str,
        file_b: &str,
        w: &CoChangeWrite,
    ) -> Result<()> {
        let (lo, hi) = Self::order_co_change_pair(file_a, file_b)?;
        self.conn.execute(
            "INSERT INTO git_co_changes (file_a, file_b, weight, count, last_observed_at,
                                         first_observed_at, window_mask, windows)
              VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
              ON CONFLICT(file_a, file_b) DO UPDATE SET
                  weight = excluded.weight,
                  count = excluded.count,
                  last_observed_at = excluded.last_observed_at,
                  first_observed_at = excluded.first_observed_at,
                  window_mask = excluded.window_mask,
                  windows = excluded.windows",
            rusqlite::params![
                lo,
                hi,
                w.weight,
                w.count,
                w.last_observed_at,
                w.first_observed_at,
                w.window_mask as i64,
                windows_of(w.window_mask)
            ],
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
    ///
    /// Atomic unit: both UPDATEs run inside one savepoint, so a crash or
    /// error between them cannot leave `git_files` decayed while
    /// `git_co_changes` still holds pre-decay weights (or vice versa) —
    /// the two tables would then disagree about the age of the same pass.
    /// A savepoint (not a bare BEGIN) composes with a caller's outer
    /// transaction elsewhere in the indexer.
    pub fn scale_git_decay(&self, factor: f64) -> Result<()> {
        self.conn.execute_batch("SAVEPOINT scale_git_decay")?;
        let result = (|| -> Result<()> {
            self.conn.execute(
                "UPDATE git_files SET churn_score = churn_score * ?1",
                rusqlite::params![factor],
            )?;
            self.conn.execute(
                "UPDATE git_co_changes SET weight = weight * ?1",
                rusqlite::params![factor],
            )?;
            Ok(())
        })();
        match result {
            Ok(()) => {
                self.conn.execute_batch("RELEASE scale_git_decay")?;
                Ok(())
            }
            Err(e) => {
                let _ = self.conn.execute_batch("ROLLBACK TO scale_git_decay");
                let _ = self.conn.execute_batch("RELEASE scale_git_decay");
                Err(e)
            }
        }
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

    /// Additive upsert for a co-change pair with no recurrence data. See
    /// `incr_git_file` for semantics. Pair order is normalized like
    /// [`Database::upsert_git_co_change`]; a self-pair errors.
    pub fn incr_git_co_change(
        &self,
        file_a: &str,
        file_b: &str,
        weight_delta: f64,
        count_delta: u32,
        last_observed_at: Option<i64>,
    ) -> Result<()> {
        self.incr_git_co_change_full(
            file_a,
            file_b,
            &CoChangeWrite {
                weight: weight_delta,
                count: count_delta,
                window_mask: 0,
                first_observed_at: last_observed_at,
                last_observed_at,
            },
        )
    }

    /// Additive upsert for a co-change pair: weight and count add, the window
    /// mask ORs, `first_observed_at` takes the older and `last_observed_at`
    /// the newer timestamp, and `windows` is recomputed from the merged mask.
    /// Exact under incremental indexing because the mask is keyed to a fixed
    /// epoch, so a delta's bits are the same bits a full rescan would set.
    ///
    /// A row with `first_observed_at IS NULL` was written before migration
    /// 0017 and has never been baselined by a `--full` pass. An incremental
    /// delta must leave it that way: writing the delta's oldest commit would
    /// turn "unknown" into a wrong measured span, and a mask built from a
    /// partial range would be misleading. NULL stays the "not baselined"
    /// marker until `--full` rewrites the row.
    pub fn incr_git_co_change_full(
        &self,
        file_a: &str,
        file_b: &str,
        w: &CoChangeWrite,
    ) -> Result<()> {
        let (lo, hi) = Self::order_co_change_pair(file_a, file_b)?;
        let merged_mask: i64 = self.conn.query_row(
            "INSERT INTO git_co_changes (file_a, file_b, weight, count, last_observed_at,
                                         first_observed_at, window_mask, windows)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(file_a, file_b) DO UPDATE SET
                 weight = weight + excluded.weight,
                 count = count + excluded.count,
                 last_observed_at = CASE
                     WHEN excluded.last_observed_at IS NULL THEN last_observed_at
                     WHEN last_observed_at IS NULL THEN excluded.last_observed_at
                     ELSE MAX(last_observed_at, excluded.last_observed_at)
                 END,
                 first_observed_at = CASE
                     WHEN first_observed_at IS NULL THEN NULL
                     WHEN excluded.first_observed_at IS NULL THEN first_observed_at
                     ELSE MIN(first_observed_at, excluded.first_observed_at)
                 END,
                 window_mask = CASE
                     WHEN first_observed_at IS NULL THEN window_mask
                     ELSE window_mask | excluded.window_mask
                 END
             RETURNING window_mask",
            rusqlite::params![
                lo,
                hi,
                w.weight,
                w.count,
                w.last_observed_at,
                w.first_observed_at,
                w.window_mask as i64,
                windows_of(w.window_mask)
            ],
            |r| r.get(0),
        )?;
        // SQLite has no popcount; derive `windows` from the merged mask here.
        self.conn.execute(
            "UPDATE git_co_changes SET windows = ?3 WHERE file_a = ?1 AND file_b = ?2",
            rusqlite::params![lo, hi, windows_of(merged_mask as u64)],
        )?;
        Ok(())
    }

    /// True if a co-change pair already exists in the DB. Order-insensitive
    /// (normalized like the upserts); a self-pair errors. Used by incremental
    /// indexing to decide whether a sub-threshold pair should
    /// be upserted (existing pairs keep accumulating) or dropped (new noise below threshold).
    pub fn co_change_pair_exists(&self, file_a: &str, file_b: &str) -> Result<bool> {
        let (lo, hi) = Self::order_co_change_pair(file_a, file_b)?;
        let n: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM git_co_changes WHERE file_a = ?1 AND file_b = ?2",
            rusqlite::params![lo, hi],
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

    /// True when at least one stored pair is recurring (observation span of
    /// [`RECURRING_SPAN_SECS`] or more). False across the whole table means
    /// either the index predates the recurrence columns (populated by the
    /// next `git-index --full`) or no pair really recurs; `find_coupling`
    /// combines this with the history span to tell the two apart.
    pub fn any_co_change_recurring(&self) -> Result<bool> {
        let exists: bool = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM git_co_changes
                           WHERE last_observed_at - first_observed_at >= ?1)",
            rusqlite::params![RECURRING_SPAN_SECS],
            |r| r.get(0),
        )?;
        Ok(exists)
    }

    /// True when some pair still has no `first_observed_at`: rows written
    /// before migration 0017 that no `git-index --full` has rewritten yet.
    pub fn any_co_change_missing_first_observed(&self) -> Result<bool> {
        let exists: bool = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM git_co_changes WHERE first_observed_at IS NULL)",
            [],
            |r| r.get(0),
        )?;
        Ok(exists)
    }

    /// Oldest and newest co-change observation across the whole table, in
    /// unix seconds; `None` when no pair carries timestamps. Bounds how much
    /// history the recurrence signal has had to work with.
    pub fn co_change_history_span(&self) -> Result<Option<(i64, i64)>> {
        let (first, last): (Option<i64>, Option<i64>) = self.conn.query_row(
            "SELECT MIN(COALESCE(first_observed_at, last_observed_at)), MAX(last_observed_at)
             FROM git_co_changes",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        Ok(match (first, last) {
            (Some(f), Some(l)) => Some((f, l)),
            _ => None,
        })
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
    /// pair, weight-sorted descending. Raw-weight order; see
    /// [`Database::co_changes_for_ranked`] for the recurrence-aware order.
    pub fn co_changes_for(&self, path: &str, limit: usize) -> Result<Vec<CoChangeRow>> {
        self.co_changes_for_ranked(path, limit, 1.0)
    }

    /// Like [`Database::co_changes_for`], but ranks by `weight *
    /// one_off_multiplier` for pairs whose observation span is under
    /// [`RECURRING_SPAN_SECS`] or unknown (legacy rows with a NULL
    /// `first_observed_at`); recurring pairs keep their raw weight. The
    /// returned `weight` is the raw stored value either way; only the order
    /// changes. Pass `1.0` for raw-weight order.
    pub fn co_changes_for_ranked(
        &self,
        path: &str,
        limit: usize,
        one_off_multiplier: f64,
    ) -> Result<Vec<CoChangeRow>> {
        // Pair is stored with file_a < file_b. For a given path, results live on
        // either side, so query both columns and union-rank. The LEFT JOIN
        // picks up the other file's commit total for the reverse confidence;
        // a missing git_files row reads as 0.
        let mut stmt = self.conn.prepare(
            "SELECT p.other, p.weight, p.count, p.last_observed_at, p.windows,
                    COALESCE(g.total_commits, 0), p.first_observed_at, p.window_mask
             FROM (
                 SELECT file_b AS other, weight, count, last_observed_at, windows,
                        first_observed_at, window_mask
                 FROM git_co_changes WHERE file_a = ?1
                 UNION ALL
                 SELECT file_a AS other, weight, count, last_observed_at, windows,
                        first_observed_at, window_mask
                 FROM git_co_changes WHERE file_b = ?1
             ) AS p
             LEFT JOIN git_files AS g ON g.path = p.other
             ORDER BY p.weight * (CASE
                          WHEN COALESCE(p.last_observed_at - p.first_observed_at, -1) >= ?4
                          THEN 1.0 ELSE ?3 END) DESC,
                      p.other
             LIMIT ?2",
        )?;
        let rows = stmt
            .query_map(
                rusqlite::params![path, limit as i64, one_off_multiplier, RECURRING_SPAN_SECS],
                |row| Self::co_change_row_from(row, 0),
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Decode a `CoChangeRow` from a result row whose columns start at `off`
    /// in the order `other, weight, count, last_observed_at, windows,
    /// other_commits, first_observed_at, window_mask`.
    fn co_change_row_from(row: &rusqlite::Row<'_>, off: usize) -> rusqlite::Result<CoChangeRow> {
        Ok(CoChangeRow {
            file: row.get(off)?,
            weight: row.get(off + 1)?,
            count: row.get::<_, i64>(off + 2)? as u32,
            last_observed_at: row.get(off + 3)?,
            windows: row.get::<_, i64>(off + 4)?.max(1) as u32,
            other_commits: row.get::<_, i64>(off + 5)?.max(0) as u32,
            first_observed_at: row.get(off + 6)?,
            window_mask: row.get::<_, i64>(off + 7)? as u64,
        })
    }

    /// Top-`limit` co-changing files for every path in `paths`, in bounded queries.
    /// Bulk counterpart of [`Database::co_changes_for_ranked`] for callers
    /// scoring many files at once (`recommend_tests`' coupled bucket):
    /// `one_off_multiplier` demotes pairs whose span is under
    /// [`RECURRING_SPAN_SECS`] or unknown, `1.0` for raw order. Batched
    /// row-numbered passes over the union of both pair sides reproduce the
    /// per-file `ORDER BY ... LIMIT` exactly, ties included, so it agrees
    /// with `find_coupling`. Every requested path is present in the map, with
    /// an empty vec when it has no recorded pairs.
    pub fn co_changes_for_many(
        &self,
        paths: &[&str],
        limit: usize,
        one_off_multiplier: f64,
    ) -> Result<std::collections::HashMap<String, Vec<CoChangeRow>>> {
        use std::collections::{HashMap, HashSet};
        let mut out: HashMap<String, Vec<CoChangeRow>> = HashMap::new();
        for p in paths {
            out.entry(p.to_string()).or_default();
        }
        // Duplicate inputs share one map entry: query each distinct path once
        // so its rows are not pushed twice.
        let mut seen = HashSet::new();
        let mut unique = Vec::new();
        for p in paths {
            if seen.insert(*p) {
                unique.push(*p);
            }
        }
        if unique.is_empty() {
            return Ok(out);
        }
        // Each path needs two SELECT arms; SQLite permits 500 per compound query.
        for batch in unique.chunks(250) {
            let mut arms = Vec::with_capacity(batch.len() * 2);
            for (i, _) in batch.iter().enumerate() {
                let param = format!("?{}", i + 1);
                arms.push(format!(
                    "SELECT {param} AS qpath, file_b AS other, weight, count AS cnt, \
                 last_observed_at, windows, first_observed_at, window_mask \
                 FROM git_co_changes WHERE file_a = {param}"
                ));
                arms.push(format!(
                    "SELECT {param} AS qpath, file_a AS other, weight, count AS cnt, \
                 last_observed_at, windows, first_observed_at, window_mask \
                 FROM git_co_changes WHERE file_b = {param}"
                ));
            }
            let limit_param = format!("?{}", batch.len() + 1);
            let multiplier_param = format!("?{}", batch.len() + 2);
            let sql = format!(
                "SELECT r.qpath, r.other, r.weight, r.cnt, r.last_observed_at, r.windows,
                    COALESCE(g.total_commits, 0), r.first_observed_at, r.window_mask
             FROM (
               SELECT qpath, other, weight, cnt, last_observed_at, windows,
                      first_observed_at, window_mask,
                      ROW_NUMBER() OVER (
                          PARTITION BY qpath
                          ORDER BY weight * (CASE
                              WHEN COALESCE(last_observed_at - first_observed_at, -1)
                                   >= {RECURRING_SPAN_SECS}
                              THEN 1.0 ELSE {multiplier_param} END) DESC,
                              other
                      ) AS rn
               FROM ({})
             ) AS r
             LEFT JOIN git_files AS g ON g.path = r.other
             WHERE r.rn <= {limit_param}",
                arms.join(" UNION ALL ")
            );
            let mut stmt = self.conn.prepare(&sql)?;
            let mut bound: Vec<&dyn rusqlite::types::ToSql> = Vec::with_capacity(batch.len() + 2);
            for p in batch {
                bound.push(p);
            }
            let limit_i64 = limit as i64;
            bound.push(&limit_i64);
            bound.push(&one_off_multiplier);
            let rows = stmt.query_map(bound.as_slice(), |row| {
                Ok((row.get::<_, String>(0)?, Self::co_change_row_from(row, 1)?))
            })?;
            for row in rows {
                let (qpath, co) = row?;
                if let Some(peers) = out.get_mut(&qpath) {
                    peers.push(co);
                }
            }
        }
        Ok(out)
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
    use super::{CoChangeWrite, Database, RECURRING_SPAN_SECS};

    /// A row indexed before migration 0017 carries `first_observed_at IS NULL`
    /// and `window_mask = 0`. Incremental deltas must leave both alone (a
    /// delta's oldest commit is not the pair's, and a mask from a partial
    /// range misleads), so NULL stays the "not baselined" marker and the row
    /// ranks as one-off until a `--full` pass rewrites it.
    #[test]
    fn incremental_delta_onto_legacy_row_with_null_first_observed_at() {
        let db = Database::open_in_memory().unwrap();
        let t = 1_750_000_000_i64;
        db.conn
            .execute(
                "INSERT INTO git_co_changes (file_a, file_b, weight, count, last_observed_at)
                 VALUES ('a.rs', 'b.rs', 2.0, 3, ?1)",
                rusqlite::params![t - 100 * 86_400],
            )
            .unwrap();
        let legacy = &db.co_changes_for("a.rs", 10).unwrap()[0];
        assert_eq!(legacy.first_observed_at, None);
        assert_eq!((legacy.window_mask, legacy.windows), (0, 1));
        // Unknown span ranks as one-off: a 40-day recurring peer at lower
        // weight comes first under the multiplier.
        db.upsert_git_co_change_full(
            "a.rs",
            "peer.rs",
            &CoChangeWrite {
                weight: 1.6,
                count: 3,
                window_mask: 0b1,
                first_observed_at: Some(t - 40 * 86_400),
                last_observed_at: Some(t),
            },
        )
        .unwrap();
        let ranked = db
            .co_changes_for_ranked("a.rs", 10, super::ONE_OFF_RANK_MULTIPLIER)
            .unwrap();
        assert_eq!(
            ranked[0].file, "peer.rs",
            "NULL span must not rank as recurring"
        );

        // The incremental delta: one commit now, 100 days after the legacy last.
        db.incr_git_co_change_full(
            "a.rs",
            "b.rs",
            &CoChangeWrite {
                weight: 0.5,
                count: 1,
                window_mask: 0b100,
                first_observed_at: Some(t),
                last_observed_at: Some(t),
            },
        )
        .unwrap();
        let row = db
            .co_changes_for("a.rs", 10)
            .unwrap()
            .into_iter()
            .find(|r| r.file == "b.rs")
            .unwrap();
        assert_eq!(row.count, 4);
        assert_eq!(row.weight, 2.5);
        assert_eq!(
            row.first_observed_at, None,
            "an incremental delta must not baseline a legacy row"
        );
        assert_eq!(row.last_observed_at, Some(t));
        assert_eq!(
            (row.window_mask, row.windows),
            (0, 1),
            "no partial-range mask on an unbaselined row"
        );
        // A second delta 30 days later changes nothing about that: still
        // unknown span, still one-off, still no recurring pair in the table.
        db.incr_git_co_change_full(
            "a.rs",
            "b.rs",
            &CoChangeWrite {
                weight: 0.5,
                count: 1,
                window_mask: 0b100,
                first_observed_at: Some(t + 30 * 86_400),
                last_observed_at: Some(t + 30 * 86_400),
            },
        )
        .unwrap();
        let row = db
            .co_changes_for("a.rs", 10)
            .unwrap()
            .into_iter()
            .find(|r| r.file == "b.rs")
            .unwrap();
        assert_eq!(row.first_observed_at, None);
        assert_eq!((row.window_mask, row.windows), (0, 1));
        assert!(db.any_co_change_missing_first_observed().unwrap());
        // peer.rs spans 40 days, so the table does have a recurring pair; the
        // legacy row is not it.
        let ranked = db
            .co_changes_for_ranked("a.rs", 10, super::ONE_OFF_RANK_MULTIPLIER)
            .unwrap();
        assert_eq!(
            ranked[0].file, "peer.rs",
            "legacy 3.0 * 0.5 = 1.5 stays below the 1.6 recurring peer"
        );

        // Only a full rewrite baselines the row.
        db.upsert_git_co_change_full(
            "a.rs",
            "b.rs",
            &CoChangeWrite {
                weight: 3.0,
                count: 5,
                window_mask: 0b101,
                first_observed_at: Some(t - 100 * 86_400),
                last_observed_at: Some(t + 30 * 86_400),
            },
        )
        .unwrap();
        let row = db
            .co_changes_for("a.rs", 10)
            .unwrap()
            .into_iter()
            .find(|r| r.file == "b.rs")
            .unwrap();
        assert_eq!(row.first_observed_at, Some(t - 100 * 86_400));
        assert_eq!((row.window_mask, row.windows), (0b101, 2));
        assert!(!db.any_co_change_missing_first_observed().unwrap());
        assert!(
            row.last_observed_at.unwrap() - row.first_observed_at.unwrap() >= RECURRING_SPAN_SECS
        );
    }

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

    #[test]
    fn co_changes_for_many_handles_large_input_sets() {
        let db = Database::open_in_memory().unwrap();
        let paths: Vec<String> = (0..1001).map(|i| format!("src/{i:04}.rs")).collect();
        for path in &paths {
            db.upsert_git_co_change("a.rs", path, 3.0, 3, None).unwrap();
            db.upsert_git_co_change(path, "z.rs", 5.0, 5, None).unwrap();
        }
        for count in [250, 251, 500, 501, 1001] {
            let mut inputs: Vec<&str> = paths[..count].iter().map(String::as_str).collect();
            inputs.extend([paths[0].as_str(), "unknown.rs"]);
            let result = db.co_changes_for_many(&inputs, 1, 0.5).unwrap();
            assert_eq!(result.len(), count + 1);
            assert!(result["unknown.rs"].is_empty());
            for path in &paths[..count] {
                let rows = &result[path];
                assert_eq!(rows.len(), 1, "{path}");
                assert_eq!(rows[0].file, "z.rs", "{path}");
                assert_eq!(rows[0].weight, 5.0);
                assert_eq!(rows[0].count, 5);
            }
        }
    }

    #[test]
    fn co_changes_for_many_matches_per_file_query_including_ties_and_gaps() {
        let db = Database::open_in_memory().unwrap();
        // target.rs: five peers tied at one weight (limit cuts the tie), plus
        // a heavier peer that must sort first on both paths.
        db.upsert_git_co_change("heavy.rs", "target.rs", 9.0, 10, Some(1_700_000_000))
            .unwrap();
        for other in ["e.rs", "c.rs", "a.rs", "d.rs", "b.rs"] {
            db.upsert_git_co_change(other, "target.rs", 1.0, 3, Some(1_700_000_000))
                .unwrap();
        }
        // A second queried path sharing one pair side with the first.
        db.upsert_git_co_change("a.rs", "solo.rs", 4.0, 2, None)
            .unwrap();

        let paths = ["target.rs", "solo.rs", "unknown.rs", "target.rs"];
        let bulk = db
            .co_changes_for_many(&paths, 3, super::ONE_OFF_RANK_MULTIPLIER)
            .expect("batch co-change lookup");
        // Every requested path is present, even the unknown and duplicated one.
        assert_eq!(bulk.len(), 3);
        assert!(bulk["unknown.rs"].is_empty());
        for p in ["target.rs", "solo.rs"] {
            let single = db
                .co_changes_for_ranked(p, 3, super::ONE_OFF_RANK_MULTIPLIER)
                .unwrap();
            let batch = &bulk[p];
            assert_eq!(
                batch.len(),
                single.len(),
                "path {p}: batch cut a different number of rows than the per-file query"
            );
            for (got, want) in batch.iter().zip(single.iter()) {
                assert_eq!(got.file, want.file);
                assert_eq!(got.weight.to_bits(), want.weight.to_bits());
                assert_eq!(got.count, want.count);
                assert_eq!(got.last_observed_at, want.last_observed_at);
            }
        }
        // The tie under the cap resolves the same total order both ways.
        let files: Vec<&str> = bulk["target.rs"].iter().map(|r| r.file.as_str()).collect();
        assert_eq!(files, vec!["heavy.rs", "a.rs", "b.rs"]);
        assert!(db.co_changes_for_many(&[], 3, 1.0).unwrap().is_empty());
    }

    /// The bulk path feeds `recommend_tests`' coupled bucket; it must apply
    /// the same span-based demotion as `find_coupling`, so a heavier one-off
    /// pair ranks below a lighter recurring one and the per-path cap cuts the
    /// one-off, not the recurring pair.
    #[test]
    fn co_changes_for_many_demotes_one_off_pairs_like_find_coupling() {
        let db = Database::open_in_memory().unwrap();
        let t = 1_750_000_000_i64;
        // one-off: 5.0 raw, 3 days of span (halves to 2.5).
        db.upsert_git_co_change_full(
            "target.rs",
            "one-off.rs",
            &CoChangeWrite {
                weight: 5.0,
                count: 4,
                window_mask: 0b1,
                first_observed_at: Some(t - 3 * 86_400),
                last_observed_at: Some(t),
            },
        )
        .unwrap();
        // recurring: 3.0 raw, 45 days of span (keeps 3.0).
        db.upsert_git_co_change_full(
            "target.rs",
            "recurring.rs",
            &CoChangeWrite {
                weight: 3.0,
                count: 3,
                window_mask: 0b1,
                first_observed_at: Some(t - 45 * 86_400),
                last_observed_at: Some(t),
            },
        )
        .unwrap();
        // legacy: 4.0 raw, unknown span (halves to 2.0).
        db.conn
            .execute(
                "INSERT INTO git_co_changes (file_a, file_b, weight, count, last_observed_at)
                 VALUES ('legacy.rs', 'target.rs', 4.0, 3, ?1)",
                rusqlite::params![t],
            )
            .unwrap();

        let bulk = db
            .co_changes_for_many(&["target.rs"], 10, super::ONE_OFF_RANK_MULTIPLIER)
            .unwrap();
        let files: Vec<&str> = bulk["target.rs"].iter().map(|r| r.file.as_str()).collect();
        assert_eq!(files, vec!["recurring.rs", "one-off.rs", "legacy.rs"]);
        // Raw weights are reported.
        assert_eq!(bulk["target.rs"][1].weight, 5.0);

        let capped = db
            .co_changes_for_many(&["target.rs"], 1, super::ONE_OFF_RANK_MULTIPLIER)
            .unwrap();
        assert_eq!(capped["target.rs"].len(), 1);
        assert_eq!(capped["target.rs"][0].file, "recurring.rs");

        // Agrees with the per-file recurrence-ranked query.
        let single = db
            .co_changes_for_ranked("target.rs", 10, super::ONE_OFF_RANK_MULTIPLIER)
            .unwrap();
        let single_files: Vec<&str> = single.iter().map(|r| r.file.as_str()).collect();
        assert_eq!(files, single_files);

        // Multiplier 1.0 restores raw-weight order (the env toggle's path).
        let raw = db.co_changes_for_many(&["target.rs"], 10, 1.0).unwrap();
        let raw_files: Vec<&str> = raw["target.rs"].iter().map(|r| r.file.as_str()).collect();
        assert_eq!(raw_files, vec!["one-off.rs", "legacy.rs", "recurring.rs"]);
    }

    /// Reversed pairs must land on the same stored row, not a mirrored
    /// duplicate: the old `debug_assert!(file_a < file_b)` was compiled out
    /// in release, so a reversed call silently inserted a second row and
    /// double-counted the pair in every downstream weight query.
    #[test]
    fn reversed_co_change_pair_normalizes_to_one_row() {
        let db = Database::open_in_memory().unwrap();
        db.upsert_git_co_change("b.rs", "a.rs", 2.0, 1, None)
            .unwrap();
        db.upsert_git_co_change("a.rs", "b.rs", 3.0, 2, None)
            .unwrap();
        // Second upsert overwrote the first (UPSERT), it did not add a row.
        let n: i64 = db
            .conn
            .query_row("SELECT COUNT(*) FROM git_co_changes", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1, "reversed pair must not create a mirrored row");
        assert_eq!(db.co_change_weight("a.rs", "b.rs").unwrap(), 3.0);
        assert_eq!(db.co_change_weight("b.rs", "a.rs").unwrap(), 3.0);
        // Existence probes are order-insensitive too.
        assert!(db.co_change_pair_exists("b.rs", "a.rs").unwrap());
        // Additive path normalizes as well.
        db.incr_git_co_change("b.rs", "a.rs", 1.0, 1, None).unwrap();
        assert_eq!(db.co_change_weight("a.rs", "b.rs").unwrap(), 4.0);
    }

    /// A self-pair has no meaning (a file never co-changes with itself) and
    /// previously passed the `debug_assert` only when `file_a < file_b`
    /// happened to hold vacuously false — it must be a runtime error.
    #[test]
    fn self_pair_co_change_is_rejected() {
        let db = Database::open_in_memory().unwrap();
        assert!(
            db.upsert_git_co_change("a.rs", "a.rs", 1.0, 1, None)
                .is_err()
        );
        assert!(db.incr_git_co_change("a.rs", "a.rs", 1.0, 1, None).is_err());
        assert!(db.co_change_pair_exists("a.rs", "a.rs").is_err());
        let n: i64 = db
            .conn
            .query_row("SELECT COUNT(*) FROM git_co_changes", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 0, "rejected self-pair must not leave a row");
    }

    /// Decay must move both tables together: a partial application (files
    /// aged, co-changes not) would make the next incremental pass add fresh
    /// deltas onto inconsistently-aged baselines.
    #[test]
    fn scale_git_decay_applies_to_both_tables() {
        let db = Database::open_in_memory().unwrap();
        db.upsert_git_file("a.rs", 10.0, 4, 4, None).unwrap();
        db.upsert_git_co_change("a.rs", "b.rs", 8.0, 2, None)
            .unwrap();
        db.scale_git_decay(0.5).unwrap();
        let churn = db.git_file("a.rs").unwrap().expect("git file row");
        assert_eq!(churn.churn_score, 5.0);
        assert_eq!(db.co_change_weight("a.rs", "b.rs").unwrap(), 4.0);
    }
}
