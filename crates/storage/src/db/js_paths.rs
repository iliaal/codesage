//! Recorded `tsconfig*.json` / `jsconfig*.json` module-resolution settings.

use std::sync::Arc;

use anyhow::Result;
use codesage_protocol::js_paths::{JsImportMappings, JsPathConfig, JsPathConfigs};
use rusqlite::params;

use super::Database;

#[derive(Debug, Default)]
pub(super) struct JsPathMemo {
    cached: Option<((u64, u64, i64), Arc<JsPathConfigs>)>,
    mappings: Option<((u64, u64, i64), Arc<JsImportMappings>)>,
    /// Table reads performed, for tests that bound them.
    loads: usize,
    mapping_loads: usize,
}

impl Database {
    /// Replace every recorded config when the set differs from what is
    /// stored, and report whether it did. Each full or incremental index
    /// pass rereads the configs, so an edited `paths` never outlives the
    /// pass; an unchanged set costs no write.
    pub fn replace_js_path_configs(&self, configs: &[JsPathConfig]) -> Result<bool> {
        let rows = configs
            .iter()
            .map(|config| Ok((config.path.clone(), serde_json::to_string(config)?)))
            .collect::<Result<Vec<_>>>()?;
        if self.stored_js_path_rows()? == rows {
            return Ok(false);
        }
        self.js_path_memo.borrow_mut().cached = None;
        let result = self.execute_batch(|db| {
            db.conn.execute("DELETE FROM js_path_configs", [])?;
            let mut stmt = db.conn.prepare_cached(
                "INSERT OR REPLACE INTO js_path_configs (path, record) VALUES (?1, ?2)",
            )?;
            for (path, record) in &rows {
                stmt.execute(params![path, record])?;
            }
            db.set_js_aliases_stale(true)
        });
        self.js_path_memo.borrow_mut().cached = None;
        result.map(|()| true)
    }

    fn stored_js_path_rows(&self) -> Result<Vec<(String, String)>> {
        let exists: bool = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'js_path_configs')",
            [],
            |row| row.get(0),
        )?;
        if !exists {
            return Ok(Vec::new());
        }
        let mut stmt = self
            .conn
            .prepare_cached("SELECT path, record FROM js_path_configs ORDER BY path")?;
        let rows = stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// The recorded configs, read once per database generation, inside a
    /// read snapshot too: the table is written only by
    /// [`Self::replace_js_path_configs`], which discards the memo. An index
    /// written before the table existed reads as empty.
    pub fn js_path_configs(&self) -> Result<Arc<JsPathConfigs>> {
        let token = (
            self.cache_id,
            self.conn.total_changes(),
            self.data_version()?,
        );
        if let Some((cached, configs)) = &self.js_path_memo.borrow().cached
            && *cached == token
        {
            return Ok(Arc::clone(configs));
        }
        let mut records = Vec::new();
        for (path, record) in self.stored_js_path_rows()? {
            match serde_json::from_str::<JsPathConfig>(&record) {
                Ok(config) => records.push(config),
                Err(error) => {
                    tracing::warn!(%path, %error, "skipping unreadable js_path_configs row");
                }
            }
        }
        let configs = Arc::new(JsPathConfigs::new(records));
        let mut memo = self.js_path_memo.borrow_mut();
        memo.loads += 1;
        memo.cached = Some((token, Arc::clone(&configs)));
        Ok(configs)
    }

    /// Table reads [`Self::js_path_configs`] has performed on this handle.
    pub fn js_path_config_loads(&self) -> usize {
        self.js_path_memo.borrow().loads
    }
}

impl Database {
    /// Mark the stored import mappings stale, before writes that can change
    /// what they depend on; [`Self::replace_js_import_mappings`] clears it.
    /// A pass interrupted in between leaves the flag set, so the next pass
    /// remaps even when it finds nothing else to do.
    pub fn mark_js_aliases_stale(&self) -> Result<()> {
        self.set_js_aliases_stale(true)
    }

    fn set_js_aliases_stale(&self, stale: bool) -> Result<()> {
        self.conn.execute(
            "INSERT INTO js_alias_state (id, stale) VALUES (1, ?1)
             ON CONFLICT(id) DO UPDATE SET stale = excluded.stale",
            [stale],
        )?;
        Ok(())
    }

    /// Whether a remap is pending; an index without the state row (a fresh
    /// table) counts as stale.
    pub fn js_aliases_stale(&self) -> Result<bool> {
        use rusqlite::OptionalExtension;
        let exists: bool = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'js_alias_state')",
            [],
            |row| row.get(0),
        )?;
        if !exists {
            return Ok(true);
        }
        let stale: Option<bool> = self
            .conn
            .query_row("SELECT stale FROM js_alias_state WHERE id = 1", [], |row| {
                row.get(0)
            })
            .optional()?;
        Ok(stale.unwrap_or(true))
    }

    /// Replace the stored import mappings when they differ, and clear the
    /// stale flag; see [`Self::replace_js_path_configs`].
    pub fn replace_js_import_mappings(&self, rows: &[(String, String, String)]) -> Result<()> {
        if self.stored_js_import_mappings()? == rows {
            if self.js_aliases_stale()? {
                self.set_js_aliases_stale(false)?;
            }
            return Ok(());
        }
        self.js_path_memo.borrow_mut().mappings = None;
        let result = self.execute_batch(|db| {
            db.conn.execute("DELETE FROM js_import_mappings", [])?;
            let mut stmt = db.conn.prepare_cached(
                "INSERT OR REPLACE INTO js_import_mappings (importer, spec, mapped) VALUES (?1, ?2, ?3)",
            )?;
            for (importer, spec, mapped) in rows {
                stmt.execute(params![importer, spec, mapped])?;
            }
            db.set_js_aliases_stale(false)
        });
        self.js_path_memo.borrow_mut().mappings = None;
        result
    }

    fn stored_js_import_mappings(&self) -> Result<Vec<(String, String, String)>> {
        let exists: bool = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'js_import_mappings')",
            [],
            |row| row.get(0),
        )?;
        if !exists {
            return Ok(Vec::new());
        }
        let mut stmt = self.conn.prepare_cached(
            "SELECT importer, spec, mapped FROM js_import_mappings ORDER BY importer, spec",
        )?;
        let rows = stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// The stored import mappings, read once per database generation like
    /// [`Self::js_path_configs`]. An index written before the table existed
    /// reads as empty, so aliases stay unresolved until `codesage index`.
    pub fn js_import_mappings(&self) -> Result<Arc<JsImportMappings>> {
        let token = (
            self.cache_id,
            self.conn.total_changes(),
            self.data_version()?,
        );
        if let Some((cached, mappings)) = &self.js_path_memo.borrow().mappings
            && *cached == token
        {
            return Ok(Arc::clone(mappings));
        }
        let mappings = Arc::new(JsImportMappings::new(self.stored_js_import_mappings()?));
        let mut memo = self.js_path_memo.borrow_mut();
        memo.mapping_loads += 1;
        memo.mappings = Some((token, Arc::clone(&mappings)));
        Ok(mappings)
    }

    /// Table reads [`Self::js_import_mappings`] has performed on this handle.
    pub fn js_import_mapping_loads(&self) -> usize {
        self.js_path_memo.borrow().mapping_loads
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replace_round_trips_and_a_write_invalidates_the_memo() {
        let db = Database::open_in_memory().unwrap();
        assert!(db.js_path_configs().unwrap().configs().is_empty());
        let config = JsPathConfig {
            path: "tsconfig.json".into(),
            paths_from: Some("tsconfig.json".into()),
            declared_paths: vec![("@/*".into(), vec!["src/*".into()])],
            ..JsPathConfig::default()
        };
        db.replace_js_path_configs(std::slice::from_ref(&config))
            .unwrap();
        assert_eq!(db.js_path_configs().unwrap().configs(), [config]);
        db.replace_js_path_configs(&[]).unwrap();
        assert!(db.js_path_configs().unwrap().configs().is_empty());
    }

    #[test]
    fn repeated_reads_inside_a_snapshot_load_the_table_once() {
        let db = Database::open_in_memory().unwrap();
        let config = JsPathConfig {
            path: "tsconfig.json".into(),
            ..JsPathConfig::default()
        };
        db.replace_js_path_configs(std::slice::from_ref(&config))
            .unwrap();
        let before = db.js_path_config_loads();
        let _snapshot = db.read_snapshot().unwrap();
        for _ in 0..100 {
            assert_eq!(db.js_path_configs().unwrap().configs().len(), 1);
        }
        assert_eq!(db.js_path_config_loads() - before, 1);
    }

    #[test]
    fn import_mappings_round_trip_and_load_once_per_generation() {
        let db = Database::open_in_memory().unwrap();
        let rows = vec![("src/a.ts".to_string(), "@/b".to_string(), "./b".to_string())];
        db.replace_js_import_mappings(&rows).unwrap();
        let before = db.js_import_mapping_loads();
        for _ in 0..10 {
            assert_eq!(
                db.js_import_mappings().unwrap().get("src/a.ts", "@/b"),
                Some("./b")
            );
        }
        assert_eq!(db.js_import_mapping_loads() - before, 1);
        db.replace_js_import_mappings(&[]).unwrap();
        assert!(db.js_import_mappings().unwrap().is_empty());
        assert!(!db.js_aliases_stale().unwrap());
        db.mark_js_aliases_stale().unwrap();
        assert!(db.js_aliases_stale().unwrap());
        db.replace_js_import_mappings(&[]).unwrap();
        assert!(!db.js_aliases_stale().unwrap());
    }
}
