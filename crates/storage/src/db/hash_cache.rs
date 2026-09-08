use std::collections::HashMap;

use anyhow::Result;
use codesage_protocol::stat_cache::{CachedFileHash, FileStat};
use rusqlite::params;

use super::Database;

impl Database {
    pub fn file_hash_cache(&self) -> Result<HashMap<String, CachedFileHash>> {
        let mut stmt = self.conn.prepare("SELECT path, size, mtime_ns, ctime_ns, content_hash, hashed_at_ns FROM file_hash_cache")?;
        Ok(stmt
            .query_map([], |row| {
                Ok((
                    row.get(0)?,
                    CachedFileHash {
                        stat: FileStat {
                            size: row.get(1)?,
                            mtime_ns: row.get(2)?,
                            ctime_ns: row.get(3)?,
                        },
                        content_hash: row.get(4)?,
                        hashed_at_ns: row.get(5)?,
                    },
                ))
            })?
            .collect::<rusqlite::Result<_>>()?)
    }

    pub fn replace_file_hash_cache(&self, entries: &HashMap<String, CachedFileHash>) -> Result<()> {
        self.execute_batch(|db| {
            let previous = db.file_hash_cache()?;
            for path in previous.keys() {
                if !entries.contains_key(path) {
                    db.conn.execute("DELETE FROM file_hash_cache WHERE path = ?1", [path])?;
                }
            }
            let mut stmt = db.conn.prepare_cached("INSERT INTO file_hash_cache
                (path, size, mtime_ns, ctime_ns, content_hash, hashed_at_ns)
                VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                ON CONFLICT(path) DO UPDATE SET size=excluded.size, mtime_ns=excluded.mtime_ns,
                    ctime_ns=excluded.ctime_ns, content_hash=excluded.content_hash, hashed_at_ns=excluded.hashed_at_ns
                WHERE size != excluded.size OR mtime_ns != excluded.mtime_ns
                    OR ctime_ns != excluded.ctime_ns OR content_hash != excluded.content_hash
                    OR hashed_at_ns != excluded.hashed_at_ns")?;
            for (path, entry) in entries {
                if previous.get(path) == Some(entry) {
                    continue;
                }
                stmt.execute(params![path, entry.stat.size, entry.stat.mtime_ns, entry.stat.ctime_ns,
                    entry.content_hash, entry.hashed_at_ns])?;
            }
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unchanged_cache_has_no_sql_row_writes() {
        let db = Database::open_in_memory().unwrap();
        let cache = HashMap::from([(
            "a.rs".into(),
            CachedFileHash {
                stat: FileStat {
                    size: 12,
                    mtime_ns: 1,
                    ctime_ns: 2,
                },
                content_hash: "hash".into(),
                hashed_at_ns: 3,
            },
        )]);
        db.replace_file_hash_cache(&cache).unwrap();
        let changes = db.conn.total_changes();
        db.replace_file_hash_cache(&cache).unwrap();
        assert_eq!(db.conn.total_changes(), changes);
        assert_eq!(db.file_hash_cache().unwrap(), cache);
        db.replace_file_hash_cache(&HashMap::new()).unwrap();
        assert!(db.file_hash_cache().unwrap().is_empty());
    }

    #[test]
    fn upgrade_from_pre_cache_schema_preserves_indexed_files() {
        let db = Database::open_in_memory().unwrap();
        db.upsert_file(&codesage_protocol::FileInfo {
            path: "a.rs".into(),
            language: codesage_protocol::Language::Rust,
            content_hash: "durable".into(),
        })
        .unwrap();
        db.conn.execute_batch("DROP TABLE file_hash_cache; DELETE FROM schema_migrations WHERE name='0019_file_hash_cache';").unwrap();
        crate::schema::init_db(&db.conn).unwrap();
        assert!(db.file_hash_cache().unwrap().is_empty());
        assert_eq!(
            db.get_file_hash("a.rs").unwrap().as_deref(),
            Some("durable")
        );
        crate::schema::init_db(&db.conn).unwrap();
        assert!(db.file_hash_cache().unwrap().is_empty());
    }
}
