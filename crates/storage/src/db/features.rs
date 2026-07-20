//! Feature record CRUD: `features` + `feature_files` + `feature_trust_boundaries`.
//!
//! Methods are kept thin and direct — the mapper crate composes them into a
//! single `replace_all_features` transaction. No domain logic here; this
//! module is just SQL plus enum<->string conversions.

use std::collections::HashMap;

use anyhow::Result;
use codesage_protocol::{
    FeatureConfidence, FeatureFileRef, FeatureFileRole, FeatureKind, FeatureRecord, Language,
    TrustBoundary,
};
use rusqlite::params;

use super::{Database, row_enum};

fn parse_feature_kind(s: &str) -> rusqlite::Result<FeatureKind> {
    row_enum(s, FeatureKind::parse, "FeatureKind")
}

fn parse_feature_confidence(s: &str) -> rusqlite::Result<FeatureConfidence> {
    row_enum(s, FeatureConfidence::parse, "FeatureConfidence")
}

fn parse_feature_role(s: &str) -> rusqlite::Result<FeatureFileRole> {
    row_enum(s, FeatureFileRole::parse, "FeatureFileRole")
}

fn escape_like_pattern(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '%' | '_' | '\\' => {
                out.push('\\');
                out.push(c);
            }
            _ => out.push(c),
        }
    }
    out
}

fn parse_language_loose(s: &str) -> rusqlite::Result<Language> {
    row_enum(s, Language::parse, "Language")
}

fn parse_trust_boundary(s: &str) -> rusqlite::Result<TrustBoundary> {
    row_enum(s, TrustBoundary::parse, "TrustBoundary")
}

impl Database {
    /// Insert or update one feature record plus its file refs and
    /// boundary tags. Replace-on-conflict semantics: existing rows for the
    /// same `feature_id` are wiped and re-inserted so re-mapping is
    /// idempotent. Wrap multiple calls in `execute_batch` for a large
    /// mapping pass.
    pub fn upsert_feature(&self, feature: &FeatureRecord) -> Result<()> {
        self.conn.execute_batch("SAVEPOINT upsert_feature")?;
        // prepare_cached throughout: five constant statements, run once per
        // feature in a mapping pass (thousands of features on seed-dense
        // repos). Same rationale as the structural index-loop inserts.
        let result = (|| -> Result<()> {
            let tags_json =
                serde_json::to_string(&feature.tags).unwrap_or_else(|_| "[]".to_string());
            self.conn.prepare_cached(
                "INSERT INTO features (
                    feature_id, title, summary, kind, source, confidence,
                    entry_path, entry_symbol, entry_route, entry_command,
                    language, tags, test_command, created_at, updated_at
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, unixepoch(), unixepoch())
                 ON CONFLICT(feature_id) DO UPDATE SET
                    title         = excluded.title,
                    summary       = excluded.summary,
                    kind          = excluded.kind,
                    source        = excluded.source,
                    confidence    = excluded.confidence,
                    entry_path    = excluded.entry_path,
                    entry_symbol  = excluded.entry_symbol,
                    entry_route   = excluded.entry_route,
                    entry_command = excluded.entry_command,
                    language      = excluded.language,
                    tags          = excluded.tags,
                    test_command  = excluded.test_command,
                    updated_at    = unixepoch()",
            )?
            .execute(params![
                    feature.feature_id,
                    feature.title,
                    feature.summary,
                    feature.kind.as_str(),
                    feature.source,
                    feature.confidence.as_str(),
                    feature.entry_path,
                    feature.entry_symbol,
                    feature.entry_route,
                    feature.entry_command,
                    feature.language.as_str(),
                    tags_json,
                    feature.test_command,
            ])?;
            self.conn
                .prepare_cached("DELETE FROM feature_files WHERE feature_id = ?1")?
                .execute(params![feature.feature_id])?;
            {
                let mut files_stmt = self.conn.prepare_cached(
                    "INSERT OR IGNORE INTO feature_files (feature_id, path, role, reason)
                     VALUES (?1, ?2, ?3, ?4)",
                )?;
                for f in &feature.files {
                    files_stmt.execute(params![
                        feature.feature_id,
                        f.path,
                        f.role.as_str(),
                        f.reason,
                    ])?;
                }
            }
            self.conn
                .prepare_cached("DELETE FROM feature_trust_boundaries WHERE feature_id = ?1")?
                .execute(params![feature.feature_id])?;
            {
                let mut tb_stmt = self.conn.prepare_cached(
                    "INSERT OR IGNORE INTO feature_trust_boundaries (feature_id, boundary)
                     VALUES (?1, ?2)",
                )?;
                for b in &feature.trust_boundaries {
                    tb_stmt.execute(params![feature.feature_id, b.as_str()])?;
                }
            }
            Ok(())
        })();
        match result {
            Ok(()) => {
                self.conn.execute_batch("RELEASE upsert_feature")?;
                Ok(())
            }
            Err(e) => {
                let _ = self.conn.execute_batch("ROLLBACK TO upsert_feature");
                let _ = self.conn.execute_batch("RELEASE upsert_feature");
                Err(e)
            }
        }
    }

    /// Drop every feature whose `feature_id` is not in `keep`. Used at the
    /// end of a mapping pass to garbage-collect features whose seed has
    /// disappeared. Cascade hits `feature_files` and `feature_trust_boundaries`
    /// via FK ON DELETE CASCADE.
    pub fn remove_features_not_in(&self, keep: &[String]) -> Result<usize> {
        if keep.is_empty() {
            let n = self.conn.execute("DELETE FROM features", [])?;
            return Ok(n);
        }
        // SQLite doesn't support direct `IN (rust slice)`; build a temp table.
        self.conn.execute_batch(
            "CREATE TEMP TABLE IF NOT EXISTS _keep_features (feature_id TEXT PRIMARY KEY);",
        )?;
        self.conn.execute_batch("DELETE FROM _keep_features;")?;
        {
            let mut ins = self
                .conn
                .prepare("INSERT OR IGNORE INTO _keep_features (feature_id) VALUES (?1)")?;
            for id in keep {
                ins.execute(params![id])?;
            }
        }
        let n = self.conn.execute(
            "DELETE FROM features WHERE feature_id NOT IN (SELECT feature_id FROM _keep_features)",
            [],
        )?;
        self.conn.execute_batch("DROP TABLE _keep_features;")?;
        Ok(n)
    }

    /// Count features in the DB. Cheap, single query.
    pub fn feature_count(&self) -> Result<usize> {
        let n: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM features", [], |r| r.get(0))?;
        Ok(n as usize)
    }

    /// Cheap existence probe for a feature id. `load_feature` hydrates the
    /// head row plus files and boundaries; the mapping pass only needs the
    /// bool, once per feature, so the statement is cached.
    pub fn feature_exists(&self, feature_id: &str) -> Result<bool> {
        let mut stmt = self
            .conn
            .prepare_cached("SELECT 1 FROM features WHERE feature_id = ?1 LIMIT 1")?;
        match stmt.query_row(params![feature_id], |_| Ok(())) {
            Ok(()) => Ok(true),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(false),
            Err(e) => Err(e.into()),
        }
    }

    /// Load one feature with its files and boundaries. None when the id
    /// doesn't exist.
    pub fn load_feature(&self, feature_id: &str) -> Result<Option<FeatureRecord>> {
        let head = self.conn.query_row(
            "SELECT feature_id, title, summary, kind, source, confidence,
                    entry_path, entry_symbol, entry_route, entry_command,
                    language, tags, test_command
             FROM features WHERE feature_id = ?1",
            params![feature_id],
            row_to_feature_head,
        );
        let mut feature = match head {
            Ok(f) => f,
            Err(rusqlite::Error::QueryReturnedNoRows) => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        feature.files = self.feature_files_for(feature_id)?;
        feature.trust_boundaries = self.feature_trust_boundaries_for(feature_id)?;
        Ok(Some(feature))
    }

    /// List features, optionally filtered by kind, language, or a tag
    /// substring. Results sorted by (kind, entry_path, feature_id) for
    /// stable output. `limit` clamps the row count (0 means "all").
    pub fn list_features(
        &self,
        kind: Option<FeatureKind>,
        language: Option<Language>,
        tag: Option<&str>,
        limit: usize,
    ) -> Result<Vec<FeatureRecord>> {
        let mut sql = String::from(
            "SELECT feature_id, title, summary, kind, source, confidence,
                    entry_path, entry_symbol, entry_route, entry_command,
                    language, tags, test_command
             FROM features WHERE 1=1",
        );
        let mut binds: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
        if let Some(k) = kind {
            sql.push_str(" AND kind = ?");
            binds.push(Box::new(k.as_str().to_string()));
        }
        if let Some(l) = language {
            sql.push_str(" AND language = ?");
            binds.push(Box::new(l.as_str().to_string()));
        }
        if let Some(t) = tag {
            // True substring match against the JSON-encoded tags column.
            // Previously bound `%"{tag}"%`, which only matched the whole
            // tag string (with quote anchors) — `tag="framework"` would
            // miss `framework:react-router` even though the doc above
            // promises "tag substring" semantics.
            sql.push_str(" AND tags LIKE ? ESCAPE '\\'");
            binds.push(Box::new(format!("%{}%", escape_like_pattern(t))));
        }
        sql.push_str(" ORDER BY kind, entry_path, feature_id");
        if limit > 0 {
            sql.push_str(&format!(" LIMIT {limit}"));
        }
        let bind_refs: Vec<&dyn rusqlite::ToSql> = binds
            .iter()
            .map(|b| b.as_ref() as &dyn rusqlite::ToSql)
            .collect();
        let mut stmt = self.conn.prepare(&sql)?;
        let heads: Vec<FeatureRecord> = stmt
            .query_map(bind_refs.as_slice(), row_to_feature_head)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut features = heads;
        self.hydrate_feature_children(&mut features)?;
        Ok(features)
    }

    /// Features that include the given file path in any role. Inverse of
    /// `feature.files[].path`. Empty result means no mapped feature owns
    /// or contexts this file (which is common — not every file belongs to
    /// a feature slice).
    pub fn features_for_file(&self, file_path: &str) -> Result<Vec<FeatureRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT DISTINCT f.feature_id, f.title, f.summary, f.kind, f.source, f.confidence,
                    f.entry_path, f.entry_symbol, f.entry_route, f.entry_command,
                    f.language, f.tags, f.test_command
             FROM features f
             JOIN feature_files ff ON ff.feature_id = f.feature_id
             WHERE ff.path = ?1
             ORDER BY f.kind, f.entry_path, f.feature_id",
        )?;
        let heads: Vec<FeatureRecord> = stmt
            .query_map(params![file_path], row_to_feature_head)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut features = heads;
        self.hydrate_feature_children(&mut features)?;
        Ok(features)
    }

    fn hydrate_feature_children(&self, features: &mut [FeatureRecord]) -> Result<()> {
        if features.is_empty() {
            return Ok(());
        }
        let ids: Vec<String> = features.iter().map(|f| f.feature_id.clone()).collect();
        let mut files_by_id = self.feature_files_for_many(&ids)?;
        let mut boundaries_by_id = self.feature_trust_boundaries_for_many(&ids)?;
        for feature in features {
            feature.files = files_by_id.remove(&feature.feature_id).unwrap_or_default();
            feature.trust_boundaries = boundaries_by_id
                .remove(&feature.feature_id)
                .unwrap_or_default();
        }
        Ok(())
    }

    fn feature_files_for_many(
        &self,
        feature_ids: &[String],
    ) -> Result<HashMap<String, Vec<FeatureFileRef>>> {
        let mut out: HashMap<String, Vec<FeatureFileRef>> = feature_ids
            .iter()
            .map(|id| (id.clone(), Vec::new()))
            .collect();
        if feature_ids.is_empty() {
            return Ok(out);
        }
        let placeholders: Vec<String> = (1..=feature_ids.len()).map(|i| format!("?{i}")).collect();
        let sql = format!(
            "SELECT feature_id, path, role, reason
             FROM feature_files
             WHERE feature_id IN ({})
             ORDER BY feature_id, role, path",
            placeholders.join(",")
        );
        let bind_refs: Vec<&dyn rusqlite::ToSql> = feature_ids
            .iter()
            .map(|id| id as &dyn rusqlite::ToSql)
            .collect();
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(bind_refs.as_slice(), |row| {
            let feature_id: String = row.get(0)?;
            let path: String = row.get(1)?;
            let role_str: String = row.get(2)?;
            let reason: Option<String> = row.get(3)?;
            Ok((
                feature_id,
                FeatureFileRef {
                    path,
                    role: parse_feature_role(&role_str)?,
                    reason,
                },
            ))
        })?;
        for row in rows {
            let (feature_id, file) = row?;
            out.entry(feature_id).or_default().push(file);
        }
        Ok(out)
    }

    fn feature_trust_boundaries_for_many(
        &self,
        feature_ids: &[String],
    ) -> Result<HashMap<String, Vec<TrustBoundary>>> {
        let mut out: HashMap<String, Vec<TrustBoundary>> = feature_ids
            .iter()
            .map(|id| (id.clone(), Vec::new()))
            .collect();
        if feature_ids.is_empty() {
            return Ok(out);
        }
        let placeholders: Vec<String> = (1..=feature_ids.len()).map(|i| format!("?{i}")).collect();
        let sql = format!(
            "SELECT feature_id, boundary
             FROM feature_trust_boundaries
             WHERE feature_id IN ({})
             ORDER BY feature_id, boundary",
            placeholders.join(",")
        );
        let bind_refs: Vec<&dyn rusqlite::ToSql> = feature_ids
            .iter()
            .map(|id| id as &dyn rusqlite::ToSql)
            .collect();
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(bind_refs.as_slice(), |row| {
            let feature_id: String = row.get(0)?;
            let boundary_str: String = row.get(1)?;
            Ok((feature_id, parse_trust_boundary(&boundary_str)?))
        })?;
        for row in rows {
            let (feature_id, boundary) = row?;
            out.entry(feature_id).or_default().push(boundary);
        }
        for boundaries in out.values_mut() {
            boundaries.sort();
            boundaries.dedup();
        }
        Ok(out)
    }

    fn feature_files_for(&self, feature_id: &str) -> Result<Vec<FeatureFileRef>> {
        let mut stmt = self.conn.prepare(
            "SELECT path, role, reason FROM feature_files WHERE feature_id = ?1
             ORDER BY role, path",
        )?;
        let rows: Vec<FeatureFileRef> = stmt
            .query_map(params![feature_id], |row| {
                let path: String = row.get(0)?;
                let role_str: String = row.get(1)?;
                let reason: Option<String> = row.get(2)?;
                Ok(FeatureFileRef {
                    path,
                    role: parse_feature_role(&role_str)?,
                    reason,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    fn feature_trust_boundaries_for(&self, feature_id: &str) -> Result<Vec<TrustBoundary>> {
        let mut stmt = self
            .conn
            .prepare("SELECT boundary FROM feature_trust_boundaries WHERE feature_id = ?1")?;
        let rows: rusqlite::Result<Vec<TrustBoundary>> = stmt
            .query_map(params![feature_id], |row| {
                let s: String = row.get(0)?;
                parse_trust_boundary(&s)
            })?
            .collect();
        let mut tags = rows?;
        tags.sort();
        tags.dedup();
        Ok(tags)
    }
}

fn row_to_feature_head(row: &rusqlite::Row<'_>) -> rusqlite::Result<FeatureRecord> {
    let feature_id: String = row.get(0)?;
    let title: String = row.get(1)?;
    let summary: String = row.get(2)?;
    let kind_str: String = row.get(3)?;
    let source: String = row.get(4)?;
    let confidence_str: String = row.get(5)?;
    let entry_path: String = row.get(6)?;
    let entry_symbol: Option<String> = row.get(7)?;
    let entry_route: Option<String> = row.get(8)?;
    let entry_command: Option<String> = row.get(9)?;
    let language_str: String = row.get(10)?;
    let tags_json: String = row.get(11)?;
    let test_command: Option<String> = row.get(12)?;
    let tags: Vec<String> = serde_json::from_str(&tags_json).unwrap_or_default();
    Ok(FeatureRecord {
        feature_id,
        title,
        summary,
        kind: parse_feature_kind(&kind_str)?,
        source,
        confidence: parse_feature_confidence(&confidence_str)?,
        entry_path,
        entry_symbol,
        entry_route,
        entry_command,
        test_command,
        language: parse_language_loose(&language_str)?,
        tags,
        trust_boundaries: Vec::new(),
        files: Vec::new(),
    })
}
