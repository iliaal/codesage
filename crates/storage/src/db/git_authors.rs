use anyhow::Result;
use rusqlite::params;

use super::Database;

impl Database {
    pub fn reset_git_authors(&self) -> Result<()> {
        self.conn.execute_batch(
            "DELETE FROM git_author_events;
             INSERT INTO git_author_state (id, complete) VALUES (1, 1)
             ON CONFLICT(id) DO UPDATE SET complete = 1;",
        )?;
        Ok(())
    }

    pub fn git_authors_complete(&self) -> Result<bool> {
        let exists: bool = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'git_author_state')",
            [],
            |row| row.get(0),
        )?;
        if !exists {
            return Ok(false);
        }
        Ok(self.conn.query_row(
            "SELECT COALESCE((SELECT complete FROM git_author_state WHERE id = 1), 0)",
            [],
            |row| row.get(0),
        )?)
    }

    pub fn upsert_git_author_event(
        &self,
        file_path: &str,
        commit_sha: &str,
        author: &str,
        committed_at: i64,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO git_author_events (file_path, commit_sha, author, committed_at)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(file_path, commit_sha) DO UPDATE
             SET author = excluded.author, committed_at = excluded.committed_at",
            params![file_path, commit_sha, author, committed_at],
        )?;
        Ok(())
    }

    pub fn prune_git_author_events(&self, cutoff: i64) -> Result<()> {
        self.conn.execute(
            "DELETE FROM git_author_events WHERE committed_at < ?1",
            [cutoff],
        )?;
        Ok(())
    }

    pub fn git_author_events(&self, file_path: &str) -> Result<Vec<(String, i64)>> {
        let mut stmt = self.conn.prepare(
            "SELECT author, committed_at FROM git_author_events WHERE file_path = ?1
             ORDER BY committed_at, commit_sha",
        )?;
        Ok(stmt
            .query_map([file_path], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unmigrated_author_schema_is_unavailable_without_mutation() {
        let db = Database::open_in_memory().unwrap();
        db.conn
            .execute_batch("DROP TABLE git_author_state; DROP TABLE git_author_events;")
            .unwrap();
        assert!(!db.git_authors_complete().unwrap());
        let count: i64 = db
            .conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE name LIKE 'git_author_%'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 0);
    }
}
