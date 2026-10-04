use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::io::Read;
use std::path::{Component, Path};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, ensure};
use codesage_protocol::work::{StopReason, WorkControl};
use codesage_protocol::{
    DescribeCard, DescribeCost, DescribeDetail, DescribeExpansion, DescribeIncomplete,
    DescribeResult, DescribeSection, FeatureFileRole, FeatureRecord, FileCategory, Handle,
    ReferenceKind, Symbol, TargetKind,
};
use codesage_storage::Database;
use serde_json::{Value, json};

use crate::CompleteRiskRanking;
use crate::git_history::{RiskRequestScope, assess_risk_with_scope};
use crate::impact::WalkCache;
use crate::resolver::{ResolveOptions, TargetError, require_one, resolve_target};

const MAX_READ: u64 = 10 * 1024 * 1024;
const SECTION_BUDGET: Duration = Duration::from_millis(500);
const RISK_BUDGET: Duration = Duration::from_millis(250);
const MAX_FINDINGS: usize = 4_096;

#[derive(Debug, Clone)]
pub struct DescribeOptions {
    pub detail: DescribeDetail,
    pub sections: Option<Vec<String>>,
    pub section_budget: Duration,
    pub risk_budget: Duration,
}

impl Default for DescribeOptions {
    fn default() -> Self {
        Self {
            detail: DescribeDetail::Compact,
            sections: None,
            section_budget: SECTION_BUDGET,
            risk_budget: RISK_BUDGET,
        }
    }
}

#[derive(Debug)]
pub struct DescribeParameterError(pub String);

impl std::fmt::Display for DescribeParameterError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for DescribeParameterError {}

fn row_limit(detail: DescribeDetail) -> usize {
    match detail {
        DescribeDetail::Compact => 1,
        DescribeDetail::Standard => 5,
        DescribeDetail::Full => 100,
    }
}

pub fn describe(
    root: &Path,
    db: &Database,
    target: &str,
    options: &DescribeOptions,
) -> Result<DescribeResult> {
    describe_with_ranking(root, db, target, options, None)
}

pub fn describe_with_ranking(
    root: &Path,
    db: &Database,
    target: &str,
    options: &DescribeOptions,
    ranking: Option<&CompleteRiskRanking>,
) -> Result<DescribeResult> {
    let started = Instant::now();
    codesage_protocol::work::checkpoint()?;
    let resolution = resolve_target(db, target, ResolveOptions::default())?;
    let mut result = DescribeResult {
        card: None,
        target: None,
        cost: DescribeCost {
            ms: 0,
            bytes: 0,
            detail: options.detail,
        },
    };
    if resolution.ambiguous && !resolution.guessed() {
        result.target = Some(resolution);
    } else {
        let resolved = require_one(&resolution)?;
        let handle = resolved.handle.clone();
        let kind = match resolution.kind {
            TargetKind::Route | TargetKind::Command => TargetKind::Feature,
            kind => kind,
        };
        let names: &[&str] = match kind {
            TargetKind::File => &[
                "identity",
                "symbols",
                "dependencies",
                "features",
                "risk",
                "coupling",
                "tests",
                "boundaries",
                "findings",
            ],
            TargetKind::Symbol => &[
                "identity",
                "references",
                "callees",
                "clones",
                "features",
                "hotness",
                "cycles",
            ],
            TargetKind::Feature => &["identity", "risk", "tests", "boundaries", "findings"],
            TargetKind::Dir => &["module_map", "fan_in", "features", "risk"],
            _ => {
                return Err(TargetError::Unsupported {
                    input: target.to_string(),
                    kind,
                    accepted: "a file, symbol, feature, or directory",
                }
                .into());
            }
        };
        if let Some(requested) = &options.sections {
            for name in requested {
                if !names.contains(&name.as_str()) {
                    return Err(DescribeParameterError(format!(
                        "describe: section {name:?} is not available for {kind:?}; choose {}",
                        names.join(", ")
                    ))
                    .into());
                }
            }
        }
        let mut builder = Builder {
            root,
            db,
            options,
            ranking,
            handle: handle.clone(),
            cache: WalkCache::default(),
            sections: BTreeMap::new(),
        };
        let parsed = Handle::parse(&handle).context("resolver emitted an invalid handle")?;
        match parsed {
            Handle::File { path } => builder.file(&path)?,
            Handle::Symbol {
                path,
                qualified: name,
                line,
            } => {
                let symbols = db.symbols_for_file(&path)?;
                let symbol = symbols
                    .into_iter()
                    .find(|s| s.qualified_name == name && line.is_none_or(|l| s.line_start == l))
                    .context("resolved definition is missing from the read snapshot")?;
                builder.symbol(&symbol)?;
            }
            Handle::Feature { .. } => {
                let feature = db
                    .load_feature(&handle)?
                    .context("resolved feature is missing from the read snapshot")?;
                builder.feature(&feature)?;
            }
            Handle::Dir { path } => builder.directory(&path)?,
            _ => unreachable!("unsupported kinds were refused before card construction"),
        }
        result.card = Some(DescribeCard {
            handle,
            kind,
            sections: builder.sections,
        });
    }
    result.cost.ms = started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;
    for _ in 0..4 {
        let bytes = serde_json::to_vec(&result)?.len();
        if bytes == result.cost.bytes {
            break;
        }
        result.cost.bytes = bytes;
    }
    Ok(result)
}

struct Builder<'a> {
    root: &'a Path,
    db: &'a Database,
    options: &'a DescribeOptions,
    ranking: Option<&'a CompleteRiskRanking>,
    handle: String,
    cache: WalkCache,
    sections: BTreeMap<String, DescribeSection>,
}

impl Builder<'_> {
    fn wanted(&self, name: &str) -> bool {
        self.options
            .sections
            .as_ref()
            .is_none_or(|sections| sections.iter().any(|s| s == name))
    }

    fn expansion(&self, tool: &str, arguments: Value) -> DescribeExpansion {
        let mut arguments: BTreeMap<String, Value> = arguments
            .as_object()
            .into_iter()
            .flat_map(|map| map.iter().map(|(k, v)| (k.clone(), v.clone())))
            .collect();
        arguments.insert("project".into(), json!(self.root));
        DescribeExpansion {
            tool: tool.to_string(),
            arguments,
        }
    }

    fn own_expansion(&self, name: &str) -> DescribeExpansion {
        self.expansion(
            "describe",
            json!({"target": self.handle, "detail": "full", "sections": [name]}),
        )
    }

    fn section(
        &mut self,
        name: &str,
        expand: DescribeExpansion,
        budget: Duration,
        build: impl FnOnce(&mut Self) -> Result<Value>,
    ) -> Result<()> {
        if !self.wanted(name) {
            return Ok(());
        }
        codesage_protocol::work::checkpoint()?;
        let parent = codesage_protocol::work::current();
        let budget = match self.options.detail {
            DescribeDetail::Compact => budget,
            DescribeDetail::Standard => budget.saturating_mul(2),
            DescribeDetail::Full => budget.saturating_mul(10),
        };
        let mut deadline = Instant::now() + budget;
        if let Some(parent_deadline) = parent.as_ref().and_then(WorkControl::deadline) {
            deadline = deadline.min(
                parent_deadline
                    .checked_sub(Duration::from_millis(25))
                    .unwrap_or(parent_deadline),
            );
        }
        let control = WorkControl::new(Some(deadline));
        let _propagation = parent.as_ref().map(|parent| {
            let child = control.clone();
            let outer = parent.clone();
            parent.on_cancel(std::sync::Arc::new(move || {
                child.cancel(outer.reason().unwrap_or(StopReason::ClientCancelled));
            }))
        });
        let attempt = {
            let _scope = control.enter();
            control
                .check()
                .map_err(anyhow::Error::from)
                .and_then(|()| build(self))
        };
        codesage_protocol::work::checkpoint()?;
        let section = match attempt {
            Ok(data) if control.reason().is_none() => {
                if name == "findings" && data.is_null() {
                    return Ok(());
                }
                let mut completeness = data.get("unavailable").map(|_| DescribeIncomplete {
                    kind: "unscored".into(),
                    reason: "optional_schema_unavailable".into(),
                    recover: expand.clone(),
                });
                if name == "findings" && data.get("truncated") == Some(&Value::Bool(true)) {
                    completeness = Some(DescribeIncomplete {
                        kind: "truncated".into(),
                        reason: "findings_row_limit".into(),
                        recover: expand.clone(),
                    });
                }
                DescribeSection {
                    data,
                    expand,
                    completeness,
                }
            }
            attempt => {
                let reason = if control.reason() == Some(StopReason::DeadlineExceeded) {
                    "section_deadline".to_string()
                } else {
                    attempt
                        .err()
                        .map(|e| format!("section_failed: {e}"))
                        .unwrap_or_else(|| "section_incomplete".to_string())
                };
                DescribeSection {
                    data: json!({"unscored": true}),
                    completeness: Some(DescribeIncomplete {
                        kind: "unscored".into(),
                        reason,
                        recover: expand.clone(),
                    }),
                    expand,
                }
            }
        };
        self.sections.insert(name.to_string(), section);
        Ok(())
    }

    fn file(&mut self, path: &str) -> Result<()> {
        let file = Handle::file(path)
            .context("invalid indexed path")?
            .to_string();
        let budget = self.options.section_budget;
        self.section("identity", self.own_expansion("identity"), budget, |b| {
            let language = b.db.all_files_with_id_and_language()?.into_iter().find(|(_, p, _)| p == path).map(|(_, _, l)| l);
            let source = read_regular(b.root, path, MAX_READ)?;
            let dirty = b.db.get_file_hash(path)?.is_none_or(|hash| hash != codesage_parser::discover::content_hash(&source));
            let head = b.db.get_structural_index_state()?.map(|(sha, _)| sha);
            let is_test = b.file_is_test(path);
            let interpretation = b.db.file_interpretation_matches(path, crate::STRUCTURAL_INTERPRETATION);
            let mut data = json!({"language": language, "lines": line_count(&source), "is_test": is_test.as_ref().ok(), "indexed": {"head": head, "dirty": dirty, "interpretation_current": interpretation.as_ref().ok()}});
            let unavailable: Vec<&str> = [("is_test", is_test.is_err()), ("interpretation_current", interpretation.is_err())].into_iter().filter_map(|(name, failed)| failed.then_some(name)).collect();
            if !unavailable.is_empty() {
                data["unavailable"] = json!(unavailable);
            }
            Ok(data)
        })?;
        self.section("symbols", self.own_expansion("symbols"), budget, |b| {
            b.symbols(path)
        })?;
        self.section(
            "dependencies",
            self.expansion("list_dependencies", json!({"target": file})),
            budget,
            |b| b.dependencies(path),
        )?;
        self.section("features", self.expansion("find_feature", json!({"target": file})), budget, |b| {
            let features = b.db.features_for_file(path)?;
            Ok(json!({"total": features.len(), "top": features.iter().take(row_limit(b.options.detail)).map(|f| &f.feature_id).collect::<Vec<_>>()}))
        })?;
        self.section("coupling", self.expansion("find_coupling", json!({"target": file})), budget, |b| {
            let report = crate::find_coupling(b.db, path, row_limit(b.options.detail))?;
            Ok(json!({"top": report.coupled.iter().map(|r| json!({"handle": r.handle, "p_cochange": r.p_cochange, "recurring": r.recurring, "span_known": r.span_known})).collect::<Vec<_>>(), "history_indexed": b.db.git_file(path)?.is_some()}))
        })?;
        self.section(
            "tests",
            self.expansion("recommend_tests", json!({"targets": [file]})),
            budget,
            |b| b.tests(&[path.to_string()]),
        )?;
        self.section(
            "boundaries",
            self.expansion("assess_risk", json!({"target": file})),
            budget,
            |b| Ok(json!(b.db.trust_boundaries_for_file_path(path)?)),
        )?;
        self.section("findings", self.own_expansion("findings"), budget, |b| {
            b.findings(Some(path), &b.db.features_for_file(path)?)
        })?;
        self.section(
            "risk",
            self.expansion("assess_risk", json!({"target": file})),
            self.options.risk_budget,
            |b| b.risk(&[path.to_string()], false),
        )?;
        Ok(())
    }

    fn file_is_test(&self, path: &str) -> Result<bool> {
        Ok(self
            .db
            .file_test_flags(&[path.to_string()])?
            .get(path)
            .map(|(is_test, interpretation)| {
                *is_test
                    || (interpretation.as_deref() != Some(crate::STRUCTURAL_INTERPRETATION)
                        && FileCategory::classify(path) == FileCategory::Test)
            })
            .unwrap_or(false))
    }

    fn symbols(&mut self, path: &str) -> Result<Value> {
        let mut symbols = self.db.symbols_for_file(path)?;
        let names: Vec<String> = symbols.iter().map(|s| s.name.clone()).collect();
        let counts = self.db.reference_counts_for_names(&names)?;
        symbols.sort_by(|a, b| {
            counts
                .get(&b.name)
                .cmp(&counts.get(&a.name))
                .then_with(|| a.handle().to_string().cmp(&b.handle().to_string()))
        });
        let test = symbols.iter().filter(|s| s.is_test).count();
        Ok(
            json!({"total": symbols.len(), "product": symbols.len() - test, "test": test,
            "top": symbols.iter().take(row_limit(self.options.detail)).map(|s| json!({"handle": s.handle().to_string(), "kind": s.kind, "fan_in": counts.get(&s.name).copied().unwrap_or(0)})).collect::<Vec<_>>(), "counts_floor": true}),
        )
    }

    fn dependencies(&mut self, path: &str) -> Result<Value> {
        let dependency = crate::list_dependencies(self.db, path)?;
        let indexed: HashSet<String> = self.db.all_file_paths()?.into_iter().collect();
        let modules = crate::rust_modules::RustModules::load(self.db)?;
        let packages = if path.ends_with(".rs") {
            self.indexed_rust_packages()?
        } else {
            HashMap::new()
        };
        let mut internal = 0;
        let mut top = BTreeSet::new();
        let python_refs = if codesage_protocol::python::is_python_path(path) {
            Some(self.db.references_in_file_range(path, 1, u32::MAX)?)
        } else {
            None
        };
        for name in &dependency.imports {
            codesage_protocol::work::checkpoint()?;
            let mut targets = BTreeSet::new();
            if let Some(rows) = &python_refs {
                for row in rows.iter().filter(|row| row.to_name == *name) {
                    if row.kind == ReferenceKind::Import {
                        targets.extend(self.db.python_import_targets(row)?);
                    } else if row.kind == ReferenceKind::ImportBinding {
                        targets.extend(
                            self.cache
                                .resolve_reference(self.db, row)?
                                .into_iter()
                                .map(|s| s.file_path),
                        );
                    }
                }
            } else {
                targets.extend(
                    self.cache
                        .resolve_symbols(self.db, path, name)?
                        .into_iter()
                        .map(|s| s.file_path),
                );
                targets.extend(
                    crate::bundle::path_import_candidates(name, path)
                        .into_iter()
                        .filter(|p| indexed.contains(p)),
                );
            }
            targets.extend(modules.candidates(path, name).unwrap_or_default());
            if let Some((package, rest)) = name.split_once("::")
                && let Some(prefix) = packages.get(package)
            {
                let tail = rest.rsplit("::").next().unwrap_or(rest);
                targets.extend(
                    self.db
                        .find_symbols(tail, None)?
                        .into_iter()
                        .filter(|symbol| symbol.file_path.starts_with(prefix))
                        .map(|symbol| symbol.file_path),
                );
            }
            targets.retain(|p| indexed.contains(p));
            targets.remove(path);
            if !targets.is_empty() {
                internal += 1;
            }
            top.extend(
                targets
                    .into_iter()
                    .filter_map(|p| Handle::file(p).map(|h| h.to_string())),
            );
        }
        Ok(
            json!({"imports": {"internal": internal, "external": dependency.imports.len().saturating_sub(internal), "test_only": dependency.test_imports.len(), "top_internal": top.into_iter().take(row_limit(self.options.detail)).collect::<Vec<_>>()},
            "imported_by": {"total": dependency.imported_by.len(), "top": file_handles(&dependency.imported_by, row_limit(self.options.detail))}, "counts_floor": true}),
        )
    }

    fn indexed_rust_packages(&self) -> Result<HashMap<String, String>> {
        let manifests = self
            .db
            .list_features(None, None, None, 0)?
            .into_iter()
            .filter(|feature| feature.language == codesage_protocol::Language::Rust)
            .flat_map(|feature| feature.files)
            .filter(|file| file.path.ends_with("Cargo.toml"))
            .map(|file| file.path)
            .collect::<BTreeSet<_>>();
        let mut packages = HashMap::new();
        for manifest in manifests {
            codesage_protocol::work::checkpoint()?;
            let bytes = read_regular(self.root, &manifest, 1024 * 1024)?;
            let document: toml::Table = toml::from_str(std::str::from_utf8(&bytes)?)?;
            if let Some(name) = document
                .get("package")
                .and_then(|package| package.get("name"))
                .and_then(toml::Value::as_str)
            {
                let parent = Path::new(&manifest)
                    .parent()
                    .and_then(Path::to_str)
                    .unwrap_or("");
                packages.insert(
                    name.replace('-', "_"),
                    if parent.is_empty() {
                        String::new()
                    } else {
                        format!("{parent}/")
                    },
                );
            }
        }
        Ok(packages)
    }

    fn tests(&mut self, paths: &[String]) -> Result<Value> {
        let recs = crate::git_history::recommend_tests_with_commands(self.root, self.db, paths)?;
        Ok(
            json!({"primary_total": recs.primary.len(), "primary": file_handles(&recs.primary, row_limit(self.options.detail)), "command": recs.commands.first().map(|c| &c.command), "commands_total": recs.commands.len(), "inline_tests": recs.inline_test_modules.iter().map(|m| m.test_count).sum::<usize>()}),
        )
    }

    fn risk(&mut self, paths: &[String], rollup: bool) -> Result<Value> {
        self.risk_with_reuse(paths, rollup).map(|(data, _)| data)
    }

    fn risk_with_reuse(&mut self, paths: &[String], rollup: bool) -> Result<(Value, usize)> {
        let cached: HashMap<&str, &crate::session::CachedRiskAssessment> = self
            .ranking
            .into_iter()
            .flat_map(|r| r.assessments())
            .map(|assessment| (assessment.file.as_str(), assessment))
            .collect();
        let mut scope = RiskRequestScope::score_only(&mut self.cache);
        let mut scores = Vec::new();
        let mut unscored = 0;
        let mut notes = Vec::new();
        let mut reused = 0;
        for path in paths {
            codesage_protocol::work::checkpoint()?;
            let uncached;
            let risk = if let Some(assessment) = cached.get(path.as_str()) {
                reused += 1;
                *assessment
            } else {
                uncached = crate::session::CachedRiskAssessment::from(assess_risk_with_scope(
                    self.db, path, &mut scope,
                )?);
                &uncached
            };
            if risk.unscored {
                unscored += 1;
            } else {
                scores.push(risk.score);
            }
            if !rollup {
                notes.extend(
                    risk.notes
                        .iter()
                        .take(row_limit(self.options.detail))
                        .map(|note| brief(note, self.options.detail)),
                );
                if risk.unscored {
                    return Ok((
                        json!({"score": risk.score, "unscored": true, "history_terms": "unmeasured", "notes": notes, "recover": {"command": "codesage git-index"}}),
                        reused,
                    ));
                }
            }
        }
        if !rollup {
            return Ok((json!({"score": scores.first(), "notes": notes}), reused));
        }
        let mean = (!scores.is_empty()).then(|| scores.iter().sum::<f64>() / scores.len() as f64);
        let max = scores.iter().copied().max_by(f64::total_cmp);
        Ok((
            json!({"max": max, "mean": mean, "files": paths.len(), "scored": scores.len(), "unmeasured_files": unscored, "unscored": unscored > 0}),
            reused,
        ))
    }

    fn symbol(&mut self, symbol: &Symbol) -> Result<()> {
        let budget = self.options.section_budget;
        let handle = symbol.handle().to_string();
        self.section("identity", self.expansion("find_symbol", json!({"target": handle})), budget, |b| {
            Ok(json!({"kind": symbol.kind, "lines": [symbol.line_start, symbol.line_end], "is_test": symbol.is_test, "visibility": symbol.visibility, "rationale_total": symbol.rationale.len(), "rationale": symbol.rationale.iter().take(row_limit(b.options.detail)).map(|r| json!({"kind": r.kind, "text": brief(&r.text, b.options.detail), "line": r.line_start})).collect::<Vec<_>>()}))
        })?;
        self.section("references", self.expansion("find_references", json!({"target": handle})), budget, |b| {
            let refs = b.cache.references(b.db, symbol)?;
            let mut counts: BTreeMap<&str, [usize; 2]> = BTreeMap::new();
            let mut callers = BTreeMap::<String, usize>::new();
            for reference in refs.iter() {
                counts.entry(reference.kind.as_str()).or_default()[usize::from(reference.is_test)] += 1;
                if reference.kind == ReferenceKind::Call && let Some(caller) = reference.from_handle() { *callers.entry(caller.to_string()).or_default() += 1; }
            }
            let mut callers: Vec<_> = callers.into_iter().collect();
            callers.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
            Ok(json!({"by_kind": counts.into_iter().map(|(k, [product, test])| (k, json!({"product": product, "test": test}))).collect::<BTreeMap<_, _>>(), "callers_total": callers.len(), "callers": callers.iter().take(row_limit(b.options.detail)).map(|(h, _)| h).collect::<Vec<_>>(), "counts_floor": true}))
        })?;
        self.section("callees", self.expansion("export_context", json!({"target": handle, "include_callees": true})), budget, |b| {
            let refs = b.db.references_in_file_range(&symbol.file_path, symbol.line_start, symbol.line_end)?;
            let mut callees = BTreeSet::new();
            for reference in refs.iter().filter(|r| r.kind == ReferenceKind::Call && r.from_symbol.as_deref() == Some(symbol.qualified_name.as_str())) {
                for callee in b.cache.resolve_symbols(b.db, &symbol.file_path, &reference.to_name)? { callees.insert(callee.handle().to_string()); }
            }
            Ok(json!({"total": callees.len(), "top": callees.into_iter().take(row_limit(b.options.detail)).collect::<Vec<_>>(), "counts_floor": true}))
        })?;
        self.section("clones", self.expansion("find_similar", json!({"target": handle, "min_jaccard": 0.85})), budget, |b| {
            let (clones, seeds) = crate::similar::find_similar_with_seeds(b.db, &handle, 0.85, row_limit(b.options.detail))?;
            let is_target = |seed: &codesage_storage::db::StoredFingerprint| seed.file_path == symbol.file_path && seed.line_start == symbol.line_start && seed.name == symbol.name;
            let mut seed_definitions = Vec::new();
            for seed in seeds.iter().take(row_limit(b.options.detail)) {
                codesage_protocol::work::checkpoint()?;
                let definition = b.db.symbols_for_file(&seed.file_path)?.into_iter().find(|s| s.line_start == seed.line_start && s.name == seed.name);
                seed_definitions.push(json!({"handle": definition.map(|s| s.handle().to_string()), "file": Handle::file(&seed.file_path).map(|h| h.to_string()), "line": seed.line_start}));
            }
            let mut top = Vec::new();
            for clone in clones.results {
                let definition = b.db.symbols_for_file(&clone.file_path)?.into_iter().find(|s| s.line_start == clone.line_start && s.name == clone.name);
                if let Some(definition) = definition { top.push(json!({"handle": definition.handle().to_string(), "jaccard": clone.jaccard})); }
            }
            Ok(json!({"top": top, "counts_floor": true, "name_union": seeds.iter().any(|seed| !is_target(seed)), "target_seeded": seeds.iter().any(is_target), "seed_scope": "bare_name", "seed_total": seeds.len(), "seeds": seed_definitions}))
        })?;
        self.section("features", self.expansion("find_feature", json!({"target": handle})), budget, |b| {
            let features = b.db.features_for_file(&symbol.file_path)?;
            Ok(json!({"total": features.len(), "top": features.iter().take(row_limit(b.options.detail)).map(|f| &f.feature_id).collect::<Vec<_>>()}))
        })?;
        self.section("hotness", self.expansion("describe", json!({"target": Handle::file(&symbol.file_path).map(|h| h.to_string()), "detail": "full", "sections": ["symbols"]})), budget, |b| {
            let symbols = b.db.symbols_for_file(&symbol.file_path)?;
            let names = symbols.iter().map(|s| s.name.clone()).collect::<Vec<_>>();
            let counts = b.db.reference_counts_for_names(&names)?;
            let fan_in = counts.get(&symbol.name).copied().unwrap_or(0);
            let rank = 1 + symbols.iter().filter(|s| counts.get(&s.name).copied().unwrap_or(0) > fan_in).count();
            Ok(json!({"rank_in_file": rank, "symbols": symbols.len(), "fan_in": fan_in, "counts_floor": true}))
        })?;
        self.section("cycles", self.expansion("assess_risk", json!({"target": Handle::file(&symbol.file_path).map(|h| h.to_string())})), budget, |b| {
            let cycles = crate::git_history::ImportCycles::load(b.db)?;
            let component = cycles.components.iter().find(|c| c.contains(&symbol.file_path));
            Ok(json!({"scope": "file_imports", "member": component.is_some(), "size": component.map_or(0, Vec::len), "files": component.map(|c| file_handles(c, row_limit(b.options.detail))).unwrap_or_default()}))
        })?;
        Ok(())
    }

    fn feature(&mut self, feature: &FeatureRecord) -> Result<()> {
        let budget = self.options.section_budget;
        let owned_candidates = feature
            .files
            .iter()
            .filter(|f| matches!(f.role, FeatureFileRole::Entry | FeatureFileRole::Owned))
            .map(|f| f.path.clone())
            .collect::<Vec<_>>();
        let mut owned = Vec::new();
        for path in owned_candidates {
            if self.db.get_file_hash(&path)?.is_some() {
                owned.push(path);
            } else {
                owned.extend(self.directory_files(&path)?);
            }
        }
        owned.sort();
        owned.dedup();
        self.section(
            "identity",
            self.expansion("feature_bundle", json!({"target": feature.feature_id})),
            budget,
            |b| {
                let mut record = serde_json::to_value(feature)?;
                let total = feature.files.len();
                record["files_total"] = json!(total);
                record["files"] = json!(
                    feature
                        .files
                        .iter()
                        .take(row_limit(b.options.detail))
                        .collect::<Vec<_>>()
                );
                Ok(record)
            },
        )?;
        self.section("tests", self.expansion("feature_bundle", json!({"target": feature.feature_id})), budget, |b| {
            let mapped = feature.files.iter().filter(|f| f.role == FeatureFileRole::Test).map(|f| f.path.clone()).collect::<Vec<_>>();
            let command = if feature.test_command.is_some() { feature.test_command.clone() } else {
                let path = owned.iter().find(|path| b.root.join(path).is_file()).cloned();
                path.map(|path| b.tests(&[path])).transpose()?.and_then(|tests| tests["command"].as_str().map(str::to_owned))
            };
            Ok(json!({"primary_total": mapped.len(), "primary": file_handles(&mapped, row_limit(b.options.detail)), "command": command}))
        })?;
        self.section(
            "boundaries",
            self.expansion("feature_bundle", json!({"target": feature.feature_id})),
            budget,
            |_| Ok(json!(feature.trust_boundaries)),
        )?;
        self.section("findings", self.own_expansion("findings"), budget, |b| {
            b.findings(None, std::slice::from_ref(feature))
        })?;
        let risk_expand = self.own_expansion("risk");
        self.section("risk", risk_expand, self.options.risk_budget, |b| {
            b.risk(&owned, true)
        })?;
        Ok(())
    }

    fn directory(&mut self, path: &str) -> Result<()> {
        let path = path.trim_end_matches('/');
        let budget = self.options.section_budget;
        self.section(
            "module_map",
            self.own_expansion("module_map"),
            budget,
            |b| b.module_map(path),
        )?;
        self.section("fan_in", self.own_expansion("fan_in"), budget, |b| {
            let paths = b.directory_files(path)?;
            let symbols_by_file = b.db.symbols_for_files(&paths)?;
            let names = symbols_by_file.values().flatten().map(|symbol| symbol.name.clone()).collect::<BTreeSet<_>>().into_iter().collect::<Vec<_>>();
            let mut counts = HashMap::new();
            for batch in names.chunks(500) { counts.extend(b.db.reference_counts_for_names(batch)?); }
            let mut rows = Vec::new();
            for (path, symbols) in symbols_by_file {
                let names = symbols.iter().map(|s| &s.name).collect::<BTreeSet<_>>();
                rows.push((path, names.into_iter().map(|name| u64::from(counts.get(name).copied().unwrap_or(0))).sum::<u64>()));
            }
            rows.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
            Ok(json!({"top": rows.iter().take(row_limit(b.options.detail)).map(|(p, n)| json!({"handle": Handle::file(p).map(|h| h.to_string()), "fan_in": n})).collect::<Vec<_>>(), "counts_floor": true}))
        })?;
        self.section("features", self.own_expansion("features"), budget, |b| {
            let path = path.trim_end_matches('/');
            let prefix = format!("{path}/");
            let features = b.db.list_features(None, None, None, 0)?.into_iter().filter(|f| f.entry_path == path || f.entry_path.starts_with(&prefix)).collect::<Vec<_>>();
            Ok(json!({"total": features.len(), "top": features.iter().take(row_limit(b.options.detail)).map(|f| &f.feature_id).collect::<Vec<_>>()}))
        })?;
        self.section(
            "risk",
            self.own_expansion("risk"),
            self.options.risk_budget,
            |b| {
                let paths = b.directory_files(path)?;
                let flags = b.db.file_test_flags(&paths)?;
                let product = paths
                    .into_iter()
                    .filter(|p| {
                        !flags.get(p).is_some_and(|(test, _)| *test)
                            && FileCategory::classify(p) != FileCategory::Test
                    })
                    .collect::<Vec<_>>();
                b.risk(&product, true)
            },
        )?;
        Ok(())
    }

    fn directory_files(&self, path: &str) -> Result<Vec<String>> {
        let prefix = format!("{}/", path.trim_end_matches('/'));
        // SQLite LIKE folds ASCII case; indexed path ancestry is literal.
        Ok(self
            .db
            .indexed_files_with_prefix(&prefix)?
            .into_iter()
            .filter(|file| file.starts_with(&prefix))
            .collect())
    }

    fn module_map(&mut self, path: &str) -> Result<Value> {
        let paths = self.directory_files(path)?;
        let flags = self.db.file_test_flags(&paths)?;
        let symbols = self.db.symbols_for_files(&paths)?;
        let languages: HashMap<String, _> = self
            .db
            .all_files_with_id_and_language()?
            .into_iter()
            .map(|(_, p, l)| (p, l))
            .collect();
        let prefix = format!("{}/", path.trim_end_matches('/'));
        let mut modules: BTreeMap<String, Module> = BTreeMap::new();
        let mut total = Module::default();
        for file in &paths {
            codesage_protocol::work::checkpoint()?;
            let relative = file.strip_prefix(&prefix).unwrap_or(file);
            let key = relative
                .split_once('/')
                .map(|(first, _)| format!("{prefix}{first}"))
                .unwrap_or_else(|| path.to_string());
            let count = symbols.get(file).map_or(0, Vec::len);
            let test_symbols = symbols
                .get(file)
                .map_or(0, |rows| rows.iter().filter(|s| s.is_test).count());
            let is_test = flags.get(file).is_some_and(|(test, _)| *test)
                || FileCategory::classify(file) == FileCategory::Test;
            let source = read_regular(self.root, file, MAX_READ)?;
            let lines = line_count(&source);
            let language = languages.get(file).map(|l| l.as_str()).unwrap_or("unknown");
            for row in [&mut total, modules.entry(key).or_default()] {
                row.files += 1;
                row.lines += lines;
                row.symbols += count;
                row.test_symbols += test_symbols;
                row.test_files += usize::from(is_test);
                *row.languages.entry(language.to_string()).or_default() += 1;
            }
        }
        let mut modules = modules.into_iter().collect::<Vec<_>>();
        modules.sort_by(|a, b| {
            (b.1.symbols - b.1.test_symbols)
                .cmp(&(a.1.symbols - a.1.test_symbols))
                .then_with(|| a.0.cmp(&b.0))
        });
        let mut data = total.value();
        data["modules_total"] = json!(modules.len());
        data["modules"] = json!(
            modules
                .iter()
                .take(row_limit(self.options.detail))
                .map(|(p, m)| {
                    let mut v = m.value();
                    v["handle"] = json!(Handle::dir(p).map(|h| h.to_string()));
                    v
                })
                .collect::<Vec<_>>()
        );
        Ok(data)
    }

    fn findings(&self, path: Option<&str>, features: &[FeatureRecord]) -> Result<Value> {
        let directory = self.root.join(".codesage/findings");
        if !findings_path_exists(self.root, ".codesage/findings", true)? {
            return Ok(Value::Null);
        }
        let mut by_id = BTreeMap::new();
        let mut stores = if path.is_some() {
            let mut stores = Vec::new();
            for (index, entry) in std::fs::read_dir(&directory)?.enumerate() {
                codesage_protocol::work::checkpoint()?;
                ensure!(index < 512, "findings store exceeds describe scan limit");
                let path = entry?.path();
                if path.extension().is_some_and(|e| e == "json")
                    && let Some(name) = path.file_name().and_then(|n| n.to_str())
                {
                    stores.push(format!(".codesage/findings/{name}"));
                }
            }
            stores
        } else {
            features
                .iter()
                .map(|feature| format!(".codesage/findings/{}.json", feature.feature_id))
                .collect()
        };
        stores.sort();
        ensure!(
            stores.len() <= 512,
            "findings store exceeds describe scan limit"
        );
        let mut bytes_read = 0;
        let mut records_read = 0;
        let mut stores_read = 0;
        for relative in stores {
            codesage_protocol::work::checkpoint()?;
            if !findings_path_exists(self.root, &relative, false)? {
                continue;
            }
            let bytes = read_regular(self.root, &relative, 1024 * 1024)?;
            bytes_read += bytes.len() as u64;
            ensure!(
                bytes_read <= MAX_READ,
                "findings store exceeds describe read limit"
            );
            stores_read += 1;
            let document: Value = serde_json::from_slice(&bytes)?;
            let findings = document
                .get("findings")
                .and_then(Value::as_array)
                .context("findings store has no findings array")?;
            for finding in findings {
                codesage_protocol::work::checkpoint()?;
                records_read += 1;
                ensure!(
                    records_read <= MAX_FINDINGS,
                    "findings store exceeds describe record limit"
                );
                let status = finding.get("status").and_then(Value::as_str);
                ensure!(
                    matches!(
                        status,
                        Some("open" | "fixed" | "false-positive" | "wont-fix")
                    ),
                    "finding has invalid status"
                );
                if status != Some("open") {
                    continue;
                }
                if let Some(transfer) = finding.get("ack_transferred_to").filter(|v| !v.is_null()) {
                    let destinations = transfer
                        .as_array()
                        .context("open finding has invalid transfer metadata")?;
                    ensure!(
                        !destinations.is_empty()
                            && destinations.iter().all(|destination| {
                                [("feature_id", "feat_", 16), ("finding_id", "fnd_", 8)]
                                    .into_iter()
                                    .all(|(field, prefix, length)| {
                                        destination
                                            .get(field)
                                            .and_then(Value::as_str)
                                            .and_then(|id| id.strip_prefix(prefix))
                                            .is_some_and(|suffix| {
                                                suffix.len() == length
                                                    && suffix.bytes().all(|byte| {
                                                        byte.is_ascii_digit()
                                                            || (b'a'..=b'f').contains(&byte)
                                                    })
                                            })
                                    })
                            }),
                        "open finding has invalid transfer metadata"
                    );
                    continue;
                }
                let file = finding
                    .get("file")
                    .or_else(|| finding.pointer("/location/file"))
                    .or_else(|| finding.pointer("/location/file_path"))
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .context("open finding has no file")?;
                if path.is_some_and(|p| file != p) {
                    continue;
                }
                let id = finding
                    .get("finding_id")
                    .or_else(|| finding.get("id"))
                    .and_then(Value::as_str)
                    .filter(|s| s.starts_with("fnd_") && s.len() > 4 && s.len() <= 128)
                    .context("open finding has no stable id")?;
                let severity = finding
                    .get("severity")
                    .and_then(Value::as_str)
                    .context("open finding has no severity")?;
                let rank = match severity {
                    "high" => 0,
                    "medium" => 1,
                    "low" => 2,
                    _ => anyhow::bail!("open finding has invalid severity"),
                };
                let title = finding
                    .get("title")
                    .and_then(Value::as_str)
                    .filter(|s| !s.trim().is_empty())
                    .context("open finding has no title")?;
                let line = finding
                    .get("line")
                    .or_else(|| finding.pointer("/location/line"))
                    .and_then(Value::as_u64)
                    .filter(|line| *line > 0)
                    .context("open finding has no positive line")?;
                let summary = (rank, file.to_string(), line, title.to_string());
                if let Some(previous) = by_id.insert(id.to_string(), summary.clone()) {
                    ensure!(previous == summary, "conflicting records for finding {id}");
                }
            }
        }
        if stores_read == 0 {
            return Ok(Value::Null);
        }
        let mut findings: Vec<_> = by_id.into_iter().collect();
        findings.sort_by(|(left_id, left), (right_id, right)| {
            (&left.0, &left.1, &left.2, left_id).cmp(&(&right.0, &right.1, &right.2, right_id))
        });
        let limit = row_limit(self.options.detail);
        let ids: Vec<_> = findings.iter().take(limit).map(|(id, _)| id).collect();
        let top: Vec<_> = findings.iter().take(limit).map(|(id, (rank, _, line, title))| {
            let severity = ["high", "medium", "low"][*rank];
            json!({"id": id, "severity": severity, "title": brief(title, self.options.detail), "line": line})
        }).collect();
        let mut result = json!({"open": findings.len(), "ids": ids, "top": top});
        if findings.len() > limit {
            result["truncated"] = json!(true);
        }
        Ok(result)
    }
}

fn findings_path_exists(root: &Path, relative: &str, directory: bool) -> Result<bool> {
    let components: Vec<_> = Path::new(relative).components().collect();
    ensure!(
        components.iter().all(|c| matches!(c, Component::Normal(_))),
        "invalid findings path"
    );
    let mut path = root.to_path_buf();
    for (index, component) in components.iter().enumerate() {
        path.push(component.as_os_str());
        let metadata = match std::fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error.into()),
        };
        ensure!(!metadata.is_symlink(), "findings path contains a symlink");
        if directory || index + 1 < components.len() {
            ensure!(metadata.is_dir(), "findings path is not a directory");
        }
    }
    Ok(true)
}

#[derive(Default)]
struct Module {
    files: usize,
    lines: usize,
    symbols: usize,
    test_files: usize,
    test_symbols: usize,
    languages: BTreeMap<String, usize>,
}

impl Module {
    fn value(&self) -> Value {
        json!({"files": self.files, "lines": self.lines, "symbols": {"total": self.symbols, "product": self.symbols - self.test_symbols, "test": self.test_symbols}, "languages": self.languages, "test_share": if self.files == 0 { 0.0 } else { self.test_files as f64 / self.files as f64 }})
    }
}

fn file_handles(paths: &[String], limit: usize) -> Vec<String> {
    paths
        .iter()
        .take(limit)
        .filter_map(|p| Handle::file(p).map(|h| h.to_string()))
        .collect()
}

fn line_count(bytes: &[u8]) -> usize {
    bytes.iter().filter(|b| **b == b'\n').count()
        + usize::from(!bytes.is_empty() && !bytes.ends_with(b"\n"))
}

fn brief(text: &str, detail: DescribeDetail) -> String {
    let limit = match detail {
        DescribeDetail::Compact => 160,
        DescribeDetail::Standard => 400,
        DescribeDetail::Full => 4_000,
    };
    if text.len() <= limit {
        return text.to_string();
    }
    let mut end = limit;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

fn read_regular(root: &Path, relative: &str, limit: u64) -> Result<Vec<u8>> {
    ensure!(
        !Path::new(relative).is_absolute()
            && Path::new(relative)
                .components()
                .all(|c| matches!(c, Component::Normal(_))),
        "invalid repository-relative path"
    );
    let root = root.canonicalize()?;
    let path = root.join(relative);
    ensure!(
        path.canonicalize()?.starts_with(&root),
        "path escapes project root"
    );
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW);
    }
    let file = options.open(path)?;
    ensure!(file.metadata()?.is_file(), "path is not a regular file");
    let mut bytes = Vec::new();
    file.take(limit + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= limit,
        "file exceeds describe read limit"
    );
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use codesage_protocol::{FeatureConfidence, FeatureFileRef, FeatureKind, Language};

    fn fixture() -> (tempfile::TempDir, Database, FeatureRecord) {
        let root = tempfile::tempdir().unwrap();
        for (path, source) in [
            (
                "Cargo.toml",
                "[package]\nname = \"describe_fixture\"\nversion = \"0.1.0\"\nedition = \"2024\"\n[workspace]\nmembers=[\"dependency\"]\n[dependencies]\ndep-package={path=\"dependency\"}\n",
            ),
            (
                "src/lib.rs",
                "pub mod helper;\nuse crate::helper::leaf;\nuse dep_package::Shared;\nuse std::collections::HashMap;\npub fn caller() { leaf(); }\n",
            ),
            (
                "src/helper.rs",
                "// WHY: preserve fixture rationale\npub fn leaf() { target(); }\npub fn target() {}\n#[cfg(test)]\nmod tests {\n    use super::leaf;\n    #[test]\n    fn exercises_leaf() { leaf(); }\n}\n",
            ),
            ("src/other.rs", "pub fn duplicate() {}\n"),
            ("other.rs", "pub fn duplicate() {}\n"),
            ("single/one.rs", "pub fn only() {}\n"),
            (
                "single-tests/tests/only_test.rs",
                "#[test] fn only_test() {}\n",
            ),
            (
                "dependency/Cargo.toml",
                "[package]\nname='dep-package'\nversion='0.1.0'\n",
            ),
            ("dependency/src/lib.rs", "pub struct Shared;\n"),
        ] {
            let file = root.path().join(path);
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            std::fs::write(file, source).unwrap();
        }
        let db = Database::open_in_memory().unwrap();
        crate::full_index(root.path(), &db, &[], false).unwrap();
        codesage_features::map_features(root.path(), &db, &[]).unwrap();
        let feature = FeatureRecord {
            feature_id: "feat_1111111111111111".into(),
            title: "Fixture".into(),
            summary: "Fixture functions".into(),
            kind: FeatureKind::Library,
            source: "cargo-lib".into(),
            confidence: FeatureConfidence::High,
            entry_path: "src/lib.rs".into(),
            entry_symbol: None,
            entry_route: None,
            entry_command: None,
            test_command: Some("cargo test -p describe_fixture".into()),
            language: Language::Rust,
            tags: vec![],
            trust_boundaries: vec![],
            files: vec![
                FeatureFileRef {
                    path: "src/lib.rs".into(),
                    role: FeatureFileRole::Entry,
                    reason: None,
                },
                FeatureFileRef {
                    path: "src/helper.rs".into(),
                    role: FeatureFileRole::Owned,
                    reason: None,
                },
            ],
        };
        db.upsert_feature(&feature).unwrap();
        (root, db, feature)
    }

    fn card(root: &Path, db: &Database, target: &str) -> DescribeCard {
        describe(root, db, target, &DescribeOptions::default())
            .unwrap()
            .card
            .unwrap()
    }

    #[test]
    fn describe_file_and_symbol_expose_test_split_relationships_and_commands() {
        let (root, db, _) = fixture();
        let file = card(root.path(), &db, "file:src/helper.rs");
        assert_eq!(file.sections.len(), 8);
        assert!(!file.sections.contains_key("findings"));
        let symbols = &file.sections["symbols"].data;
        assert!(symbols["test"].as_u64().unwrap() >= 2);
        assert_eq!(
            symbols["total"].as_u64().unwrap(),
            symbols["product"].as_u64().unwrap() + symbols["test"].as_u64().unwrap()
        );
        assert_eq!(file.sections["identity"].data["indexed"]["dirty"], false);
        assert!(
            file.sections["tests"].data["command"]
                .as_str()
                .unwrap()
                .starts_with("cargo test -p describe_fixture")
        );
        assert!(file.sections["risk"].data["unscored"].as_bool().unwrap());
        let symbol = card(root.path(), &db, "sym:src/helper.rs#leaf");
        assert_eq!(symbol.sections.len(), 7);
        let counts = &symbol.sections["references"].data["by_kind"]["call"];
        assert_eq!(counts["product"], 1);
        assert_eq!(counts["test"], 1);
        assert_eq!(
            symbol.sections["callees"].data["top"][0],
            "sym:src/helper.rs#target"
        );
        assert!(
            symbol.sections["identity"].data["rationale"][0]["text"]
                .as_str()
                .unwrap()
                .contains("fixture rationale")
        );
        assert_eq!(symbol.sections["cycles"].data["scope"], "file_imports");
        let dependencies = card(root.path(), &db, "file:src/lib.rs");
        let imports = &dependencies.sections["dependencies"].data["imports"];
        assert!(imports["external"].as_u64().unwrap() > 0);
        assert!(imports["internal"].as_u64().unwrap() >= 2, "{imports}");
        assert_eq!(imports["top_internal"][0], "file:dependency/src/lib.rs");
        for handle in imports["top_internal"].as_array().unwrap() {
            let Handle::File { path } = Handle::parse(handle.as_str().unwrap()).unwrap() else {
                panic!("internal dependency must be a file");
            };
            assert!(db.get_file_hash(&path).unwrap().is_some());
        }
    }

    #[test]
    fn describe_ambiguity_is_candidates_only_and_section_selection_is_strict() {
        let (root, db, _) = fixture();
        let result = describe(root.path(), &db, "duplicate", &DescribeOptions::default()).unwrap();
        assert!(result.card.is_none());
        assert_eq!(result.target.unwrap().candidates_total, 2);
        let options = DescribeOptions {
            sections: Some(vec!["identity".into()]),
            ..Default::default()
        };
        assert_eq!(
            describe(root.path(), &db, "src/helper.rs", &options)
                .unwrap()
                .card
                .unwrap()
                .sections
                .len(),
            1
        );
        let wrong = DescribeOptions {
            sections: Some(vec!["module_map".into()]),
            ..Default::default()
        };
        assert!(
            describe(root.path(), &db, "src/helper.rs", &wrong)
                .unwrap_err()
                .is::<DescribeParameterError>()
        );
    }

    #[test]
    fn describe_directory_rolls_up_only_its_subtree_and_feature_retains_metadata() {
        let (root, db, feature) = fixture();
        let directory = card(root.path(), &db, "dir:src");
        assert_eq!(directory.sections["module_map"].data["files"], 3);
        assert_eq!(
            directory.sections["features"].data["top"][0],
            feature.feature_id
        );
        let feature_card = card(root.path(), &db, &feature.feature_id);
        assert_eq!(feature_card.sections["identity"].data["files_total"], 2);
        assert_eq!(
            feature_card.sections["tests"].data["command"],
            feature.test_command.unwrap()
        );
        assert_eq!(feature_card.sections["risk"].data["scored"], 0);
        assert!(feature_card.sections["risk"].data["max"].is_null());
    }

    #[test]
    fn describe_directory_consumers_use_literal_component_ancestry() {
        let (root, db, template) = fixture();
        for (path, source) in [
            (
                "src-other/outside.rs",
                "pub fn outsider() { outsider(); outsider(); outsider(); }\n",
            ),
            ("SRC/case.rs", "pub fn uppercase() {}\n"),
            ("literal_/only.rs", "pub fn under() {}\n"),
            ("literal_-other/outside.rs", "pub fn under_sibling() {}\n"),
            ("literalX/outside.rs", "pub fn under_wildcard() {}\n"),
            ("literal%/only.rs", "pub fn percent() {}\n"),
            ("literal%-other/outside.rs", "pub fn percent_sibling() {}\n"),
            ("literalXYZ/outside.rs", "pub fn percent_wildcard() {}\n"),
        ] {
            let file = root.path().join(path);
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            std::fs::write(file, source).unwrap();
        }
        crate::full_index(root.path(), &db, &[], false).unwrap();
        for path in db.all_file_paths().unwrap() {
            let outside = path.contains("outside") || path.starts_with("SRC/");
            db.upsert_git_file(
                &path,
                if outside { 100.0 } else { 1.0 },
                u32::from(outside) * 5,
                10,
                Some(1_700_000_000),
            )
            .unwrap();
        }
        let options = DescribeOptions {
            detail: DescribeDetail::Full,
            sections: Some(vec!["module_map".into(), "fan_in".into(), "risk".into()]),
            ..Default::default()
        };
        for (index, (scope, paths)) in [
            ("src", vec!["src/helper.rs", "src/lib.rs", "src/other.rs"]),
            ("src/", vec!["src/helper.rs", "src/lib.rs", "src/other.rs"]),
            ("literal_", vec!["literal_/only.rs"]),
            ("literal%", vec!["literal%/only.rs"]),
        ]
        .into_iter()
        .enumerate()
        {
            let expected_scores = paths
                .iter()
                .map(|path| crate::assess_risk(&db, path).unwrap().score)
                .collect::<Vec<_>>();
            let expected_max = expected_scores
                .iter()
                .copied()
                .max_by(f64::total_cmp)
                .unwrap();
            let expected_mean = expected_scores.iter().sum::<f64>() / paths.len() as f64;
            let expected_symbols = paths
                .iter()
                .map(|path| db.symbols_for_file(path).unwrap().len())
                .sum::<usize>();
            let expected_lines = paths
                .iter()
                .map(|path| {
                    std::fs::read_to_string(root.path().join(path))
                        .unwrap()
                        .lines()
                        .count()
                })
                .sum::<usize>();
            let target = Handle::dir(scope).unwrap().to_string();
            let result = describe(root.path(), &db, &target, &options)
                .unwrap()
                .card
                .unwrap();
            assert!(
                result
                    .sections
                    .values()
                    .all(|section| section.completeness.is_none())
            );
            let modules = &result.sections["module_map"].data;
            assert_eq!(modules["files"], paths.len());
            assert_eq!(modules["lines"], expected_lines);
            assert_eq!(modules["symbols"]["total"], expected_symbols);
            assert_eq!(modules["modules_total"], 1);
            assert_eq!(
                modules["modules"][0]["handle"],
                Handle::dir(scope.trim_end_matches('/'))
                    .unwrap()
                    .to_string()
            );
            let mut actual_handles = result.sections["fan_in"].data["top"]
                .as_array()
                .unwrap()
                .iter()
                .map(|row| row["handle"].as_str().unwrap().to_string())
                .collect::<Vec<_>>();
            actual_handles.sort();
            let mut expected_handles = paths
                .iter()
                .map(|path| Handle::file(*path).unwrap().to_string())
                .collect::<Vec<_>>();
            expected_handles.sort();
            assert_eq!(actual_handles, expected_handles);
            let risk = &result.sections["risk"].data;
            assert_eq!(risk["files"], paths.len());
            assert_eq!(risk["scored"], paths.len());
            assert_eq!(risk["max"], expected_max);
            assert_eq!(risk["mean"], expected_mean);
            let mut feature = template.clone();
            feature.feature_id = format!("feat_{:016x}", index + 2);
            feature.source = "fixture".into();
            feature.entry_path = scope.into();
            feature.files = vec![FeatureFileRef {
                path: scope.into(),
                role: FeatureFileRole::Entry,
                reason: None,
            }];
            db.upsert_feature(&feature).unwrap();
            let feature_card = describe(
                root.path(),
                &db,
                &feature.feature_id,
                &DescribeOptions {
                    sections: Some(vec!["risk".into()]),
                    ..options.clone()
                },
            )
            .unwrap()
            .card
            .unwrap();
            assert_eq!(feature_card.sections["risk"].data["files"], paths.len());
            assert_eq!(feature_card.sections["risk"].data["max"], expected_max);
            assert_eq!(feature_card.sections["risk"].data["mean"], expected_mean);
        }
        let mut exact_file = template;
        exact_file.feature_id = "feat_9999999999999999".into();
        exact_file.files.truncate(1);
        db.upsert_feature(&exact_file).unwrap();
        let file_score = crate::assess_risk(&db, &exact_file.entry_path)
            .unwrap()
            .score;
        let exact = describe(
            root.path(),
            &db,
            &exact_file.feature_id,
            &DescribeOptions {
                detail: DescribeDetail::Full,
                sections: Some(vec!["risk".into()]),
                ..Default::default()
            },
        )
        .unwrap()
        .card
        .unwrap();
        assert_eq!(exact.sections["risk"].data["files"], 1);
        assert_eq!(exact.sections["risk"].data["max"], file_score);
    }

    #[test]
    fn describe_single_file_and_empty_risk_rollups_retain_aggregate_fields() {
        let (root, db, mut feature) = fixture();
        feature.feature_id = "feat_2222222222222222".into();
        feature.entry_path = "single/one.rs".into();
        feature.files[0].path = "single/one.rs".into();
        feature.files[1].role = FeatureFileRole::Test;
        db.upsert_feature(&feature).unwrap();
        let options = DescribeOptions {
            detail: DescribeDetail::Full,
            sections: Some(vec!["risk".into()]),
            ..Default::default()
        };
        for target in [&feature.feature_id, "dir:single"] {
            let result = describe(root.path(), &db, target, &options)
                .unwrap()
                .card
                .unwrap();
            let risk = &result.sections["risk"];
            assert!(risk.completeness.is_none());
            assert_eq!(risk.data["files"], 1);
            assert_eq!(risk.data["scored"], 0);
            assert_eq!(risk.data["unmeasured_files"], 1);
            assert!(risk.data["max"].is_null());
            assert!(risk.data["mean"].is_null());
            assert_eq!(risk.data["unscored"], true);
            assert!(risk.data.get("score").is_none());
        }
        db.upsert_git_file("single/one.rs", 8.0, 3, 10, Some(1_700_000_000))
            .unwrap();
        let expected = crate::assess_risk(&db, "single/one.rs").unwrap();
        assert!(!expected.unscored);
        assert!(expected.score > 0.0);
        for target in [&feature.feature_id, "dir:single"] {
            let result = describe(root.path(), &db, target, &options)
                .unwrap()
                .card
                .unwrap();
            let risk = &result.sections["risk"];
            assert!(risk.completeness.is_none());
            assert_eq!(risk.data["files"], 1);
            assert_eq!(risk.data["scored"], 1);
            assert_eq!(risk.data["unmeasured_files"], 0);
            assert_eq!(risk.data["max"], expected.score);
            assert_eq!(risk.data["mean"], expected.score);
            assert!(risk.data.get("score").is_none());
        }
        feature.files = vec![feature.files[1].clone()];
        db.upsert_feature(&feature).unwrap();
        for target in [&feature.feature_id, "dir:single-tests"] {
            let result = describe(root.path(), &db, target, &options)
                .unwrap()
                .card
                .unwrap();
            let risk = &result.sections["risk"];
            assert!(risk.completeness.is_none());
            assert_eq!(risk.data["files"], 0);
            assert_eq!(risk.data["scored"], 0);
            assert_eq!(risk.data["unmeasured_files"], 0);
            assert!(risk.data["max"].is_null());
            assert!(risk.data["mean"].is_null());
        }
        let file = describe(root.path(), &db, "file:single/one.rs", &options)
            .unwrap()
            .card
            .unwrap();
        assert_eq!(file.sections["risk"].data["score"], expected.score);
        assert!(file.sections["risk"].data.get("max").is_none());
    }

    #[test]
    fn describe_directory_features_include_self_and_expand_only_the_subtree() {
        let (root, db, feature) = fixture();
        for (id, path) in [
            ("feat_2222222222222222", "single"),
            ("feat_3333333333333333", "single/one.rs"),
            ("feat_4444444444444444", "single-other"),
            ("feat_5555555555555555", "src"),
        ] {
            let mut record = feature.clone();
            record.feature_id = id.into();
            record.entry_path = path.into();
            db.upsert_feature(&record).unwrap();
        }
        let compact = card(root.path(), &db, "dir:single");
        let section = &compact.sections["features"];
        assert_eq!(section.data["total"], 2);
        assert_eq!(section.data["top"], json!(["feat_2222222222222222"]));
        assert_eq!(section.expand.tool, "describe");
        assert_eq!(section.expand.arguments["target"], "dir:single");
        assert_eq!(section.expand.arguments["detail"], "full");
        assert_eq!(section.expand.arguments["sections"], json!(["features"]));
        let expanded = describe(
            root.path(),
            &db,
            section.expand.arguments["target"].as_str().unwrap(),
            &DescribeOptions {
                detail: DescribeDetail::Full,
                sections: Some(vec!["features".into()]),
                ..Default::default()
            },
        )
        .unwrap()
        .card
        .unwrap();
        assert_eq!(expanded.sections.len(), 1);
        assert_eq!(expanded.sections["features"].data["total"], 2);
        assert_eq!(
            expanded.sections["features"].data["top"],
            json!(["feat_2222222222222222", "feat_3333333333333333"])
        );
    }

    #[test]
    fn describe_risk_rollups_measure_owned_files_and_keep_missing_history_explicit() {
        let (root, db, feature) = fixture();
        db.upsert_git_file("src/lib.rs", 8.0, 3, 10, Some(1_700_000_000))
            .unwrap();
        db.upsert_git_file("src/helper.rs", 1.0, 0, 2, Some(1_700_000_000))
            .unwrap();
        let expected: Vec<f64> = ["src/lib.rs", "src/helper.rs"]
            .into_iter()
            .map(|path| {
                let assessment = crate::assess_risk(&db, path).unwrap();
                assert!(!assessment.unscored);
                assessment.score
            })
            .collect();
        let max = expected.iter().copied().max_by(f64::total_cmp).unwrap();
        let mean = expected.iter().sum::<f64>() / 2.0;
        for target in [&feature.feature_id, "dir:src"] {
            let result = describe(
                root.path(),
                &db,
                target,
                &DescribeOptions {
                    detail: DescribeDetail::Full,
                    ..Default::default()
                },
            )
            .unwrap()
            .card
            .unwrap();
            let risk = &result.sections["risk"];
            assert!(risk.completeness.is_none(), "{target}: {risk:?}");
            assert_eq!(risk.data["scored"], 2);
            assert_eq!(risk.data["max"], max);
            assert_eq!(risk.data["mean"], mean);
            assert_eq!(
                risk.data["unmeasured_files"],
                usize::from(target == "dir:src")
            );
        }
    }

    #[test]
    fn describe_cached_assessments_preserve_missing_partial_and_measured_history() {
        let (root, db, feature) = fixture();
        let paths: Vec<String> = ["src/lib.rs", "src/helper.rs", "src/other.rs"]
            .into_iter()
            .map(String::from)
            .collect();
        let options = DescribeOptions {
            detail: DescribeDetail::Full,
            sections: Some(vec!["risk".into()]),
            ..Default::default()
        };
        for measured in [0, 1, 3] {
            for path in paths.iter().take(measured) {
                db.upsert_git_file(path, 8.0, 3, 10, Some(1_700_000_000))
                    .unwrap();
            }
            let ranking = crate::top_risk_ranking(&db).unwrap();
            for assessment in ranking.assessments() {
                let fresh = crate::assess_risk(&db, &assessment.file).unwrap();
                assert_eq!(assessment.score, fresh.score);
                assert_eq!(assessment.unscored, fresh.unscored);
                assert_eq!(assessment.notes, fresh.notes);
            }
            let mut builder = Builder {
                root: root.path(),
                db: &db,
                options: &options,
                ranking: Some(&ranking),
                handle: "dir:src".into(),
                cache: WalkCache::default(),
                sections: BTreeMap::new(),
            };
            let (aggregate, reused) = builder.risk_with_reuse(&paths, true).unwrap();
            assert_eq!(reused, paths.len());
            assert_eq!(aggregate["scored"], measured);
            assert_eq!(aggregate["unmeasured_files"], paths.len() - measured);
            assert_eq!(aggregate["unscored"], measured < paths.len());
            assert_eq!(aggregate["max"].is_null(), measured == 0);
            assert_eq!(aggregate["mean"].is_null(), measured == 0);
            for target in paths
                .iter()
                .map(|path| format!("file:{path}"))
                .chain([feature.feature_id.clone(), "dir:src".into()])
            {
                let fresh = describe(root.path(), &db, &target, &options)
                    .unwrap()
                    .card
                    .unwrap();
                let cached =
                    describe_with_ranking(root.path(), &db, &target, &options, Some(&ranking))
                        .unwrap()
                        .card
                        .unwrap();
                assert_eq!(
                    serde_json::to_value(&cached.sections).unwrap(),
                    serde_json::to_value(&fresh.sections).unwrap(),
                    "{measured}: {target}"
                );
                assert!(cached.sections["risk"].completeness.is_none());
                if let Some(path) = target.strip_prefix("file:") {
                    let expected = crate::assess_risk(&db, path).unwrap();
                    let (data, reused) = builder.risk_with_reuse(&[path.into()], false).unwrap();
                    assert_eq!(reused, 1);
                    assert_eq!(data["notes"], json!(expected.notes));
                    if expected.unscored {
                        assert_eq!(data["unscored"], true);
                        assert_eq!(data["history_terms"], "unmeasured");
                        assert_eq!(data["recover"]["command"], "codesage git-index");
                    }
                }
            }
        }
    }

    #[test]
    fn describe_deadline_cut_has_executable_recover_and_outer_cancellation_fails() {
        let (root, db, _) = fixture();
        let options = DescribeOptions {
            risk_budget: Duration::ZERO,
            ..Default::default()
        };
        let result = describe(root.path(), &db, "src/helper.rs", &options).unwrap();
        let risk = &result.card.unwrap().sections["risk"];
        assert_eq!(risk.data["unscored"], true);
        let incomplete = risk.completeness.as_ref().unwrap();
        assert_eq!(incomplete.reason, "section_deadline");
        assert_eq!(incomplete.recover.tool, "assess_risk");
        assert_eq!(incomplete.recover.arguments["target"], "file:src/helper.rs");
        let cancelled = WorkControl::new(None);
        let _scope = cancelled.enter();
        cancelled.cancel(StopReason::ClientCancelled);
        assert!(
            describe(root.path(), &db, "src/helper.rs", &options)
                .unwrap_err()
                .is::<codesage_protocol::work::WorkStopped>()
        );
    }

    #[test]
    fn describe_findings_lists_existing_open_records_and_discloses_bad_store() {
        let (root, db, feature) = fixture();
        let directory = root.path().join(".codesage/findings");
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join(format!("{}.json", feature.feature_id));
        std::fs::write(
            &path,
            serde_json::to_vec(&json!({"findings": [
                {"finding_id": "fnd_a", "file": "src/helper.rs", "status": "open", "severity": "medium", "title": "Helper defect", "line": 2},
                {"finding_id": "fnd_b", "file": "src/lib.rs", "status": "open", "severity": "high", "title": "Library defect", "line": 1},
                {"finding_id": "fnd_c", "file": "src/helper.rs", "status": "fixed"}
            ]}))
            .unwrap(),
        )
        .unwrap();
        let file = card(root.path(), &db, "src/helper.rs");
        assert_eq!(
            file.sections["findings"].data,
            json!({"open": 1, "ids": ["fnd_a"], "top": [{"id": "fnd_a", "severity": "medium", "title": "Helper defect", "line": 2}]})
        );
        let feature_card = card(root.path(), &db, &feature.feature_id);
        assert_eq!(feature_card.sections["findings"].data["open"], 2);
        assert_eq!(
            feature_card.sections["findings"].data["ids"],
            json!(["fnd_b"])
        );
        assert_eq!(
            feature_card.sections["findings"]
                .completeness
                .as_ref()
                .unwrap()
                .kind,
            "truncated"
        );
        std::fs::write(path, "malformed").unwrap();
        let result = card(root.path(), &db, "src/helper.rs");
        assert_eq!(result.sections["findings"].data["unscored"], true);
        assert!(result.sections["findings"].completeness.is_some());
    }

    #[test]
    fn describe_findings_missing_and_empty_stores_are_distinct() {
        let (root, db, feature) = fixture();
        let directory = root.path().join(".codesage/findings");
        for target in ["src/helper.rs", feature.feature_id.as_str()] {
            assert!(
                !card(root.path(), &db, target)
                    .sections
                    .contains_key("findings")
            );
        }
        std::fs::create_dir_all(&directory).unwrap();
        for target in ["src/helper.rs", feature.feature_id.as_str()] {
            assert!(
                !card(root.path(), &db, target)
                    .sections
                    .contains_key("findings")
            );
        }
        std::fs::write(
            directory.join(format!("{}.json", feature.feature_id)),
            r#"{"findings":[]}"#,
        )
        .unwrap();
        for target in ["src/helper.rs", feature.feature_id.as_str()] {
            assert_eq!(
                card(root.path(), &db, target).sections["findings"].data,
                json!({"open": 0, "ids": [], "top": []})
            );
        }
    }

    #[test]
    fn describe_findings_deduplicates_and_orders_severity_location_then_id() {
        let (root, db, feature) = fixture();
        let directory = root.path().join(".codesage/findings");
        std::fs::create_dir_all(&directory).unwrap();
        let records = json!([
            {"finding_id":"fnd_low", "file":"src/helper.rs", "line":1, "severity":"low", "title":"Low", "status":"open"},
            {"finding_id":"fnd_b", "file":"src/helper.rs", "line":2, "severity":"high", "title":"High B", "status":"open"},
            {"finding_id":"fnd_a", "file":"src/helper.rs", "line":2, "severity":"high", "title":"High A", "status":"open"},
            {"finding_id":"fnd_first", "file":"src/helper.rs", "line":1, "severity":"high", "title":"High first", "status":"open"},
            {"finding_id":"fnd_other", "file":"src/lib.rs", "line":1, "severity":"high", "title":"Other file", "status":"open"},
            {"finding_id":"fnd_fixed", "file":"src/helper.rs", "status":"fixed"},
            {"finding_id":"fnd_false", "file":"src/helper.rs", "status":"false-positive"},
            {"finding_id":"fnd_wont", "file":"src/helper.rs", "status":"wont-fix"},
            {"finding_id":"fnd_transferred", "file":"src/helper.rs", "status":"open", "ack_transferred_to": [{"feature_id":"feat_1111111111111111", "finding_id":"fnd_11111111"}]}
        ]);
        let document = serde_json::to_vec(&json!({"findings": records})).unwrap();
        std::fs::write(
            directory.join(format!("{}.json", feature.feature_id)),
            &document,
        )
        .unwrap();
        std::fs::write(directory.join("duplicate.json"), &document).unwrap();
        let options = DescribeOptions {
            detail: DescribeDetail::Full,
            sections: Some(vec!["findings".into()]),
            ..Default::default()
        };
        let file = describe(root.path(), &db, "src/helper.rs", &options)
            .unwrap()
            .card
            .unwrap();
        assert_eq!(file.sections["findings"].data["open"], 4);
        assert_eq!(
            file.sections["findings"].data["ids"],
            json!(["fnd_first", "fnd_a", "fnd_b", "fnd_low"])
        );
        assert!(file.sections["findings"].completeness.is_none());
        let feature_card = describe(root.path(), &db, &feature.feature_id, &options)
            .unwrap()
            .card
            .unwrap();
        assert_eq!(feature_card.sections["findings"].data["open"], 5);
        assert_eq!(
            feature_card.sections["findings"].data["ids"],
            json!(["fnd_first", "fnd_a", "fnd_b", "fnd_other", "fnd_low"])
        );
    }

    #[test]
    fn describe_findings_bounds_rows_and_refuses_malformed_records() {
        let (root, db, feature) = fixture();
        let directory = root.path().join(".codesage/findings");
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join(format!("{}.json", feature.feature_id));
        let record = json!({"finding_id":"fnd_a", "file":"src/helper.rs", "line":1, "severity":"high", "title":"A defect", "status":"open"});
        let records: Vec<_> = (0..101)
            .map(|i| {
                let mut r = record.clone();
                r["finding_id"] = json!(format!("fnd_{i:03}"));
                r
            })
            .collect();
        std::fs::write(
            &path,
            serde_json::to_vec(&json!({"findings": records})).unwrap(),
        )
        .unwrap();
        for (detail, limit) in [
            (DescribeDetail::Compact, 1),
            (DescribeDetail::Standard, 5),
            (DescribeDetail::Full, 100),
        ] {
            let options = DescribeOptions {
                detail,
                sections: Some(vec!["findings".into()]),
                ..Default::default()
            };
            let card = describe(root.path(), &db, "src/helper.rs", &options)
                .unwrap()
                .card
                .unwrap();
            let section = &card.sections["findings"];
            assert_eq!(section.data["open"], 101);
            assert_eq!(section.data["ids"].as_array().unwrap().len(), limit);
            assert_eq!(section.data["top"].as_array().unwrap().len(), limit);
            assert_eq!(section.completeness.as_ref().unwrap().kind, "truncated");
        }
        for (field, value) in [
            ("finding_id", json!("")),
            ("severity", json!("critical")),
            ("title", json!(null)),
            ("line", json!(true)),
            ("line", json!(0)),
            ("status", json!("unknown")),
        ] {
            let mut invalid = record.clone();
            invalid[field] = value;
            std::fs::write(
                &path,
                serde_json::to_vec(&json!({"findings": [invalid]})).unwrap(),
            )
            .unwrap();
            assert_eq!(
                card(root.path(), &db, "src/helper.rs").sections["findings"].data,
                json!({"unscored":true}),
                "{field}"
            );
        }
        std::fs::write(
            &path,
            serde_json::to_vec(&json!({"findings": vec![record.clone(); MAX_FINDINGS + 1]}))
                .unwrap(),
        )
        .unwrap();
        assert!(
            card(root.path(), &db, "src/helper.rs").sections["findings"]
                .completeness
                .as_ref()
                .unwrap()
                .reason
                .contains("record limit")
        );
        std::fs::write(&path, " ".repeat(1024 * 1024 + 1)).unwrap();
        assert!(
            card(root.path(), &db, "src/helper.rs").sections["findings"]
                .completeness
                .as_ref()
                .unwrap()
                .reason
                .contains("read limit")
        );
        let mut conflict = record.clone();
        conflict["title"] = json!("Different defect");
        std::fs::write(
            &path,
            serde_json::to_vec(&json!({"findings": [record, conflict]})).unwrap(),
        )
        .unwrap();
        assert!(
            card(root.path(), &db, "src/helper.rs").sections["findings"]
                .completeness
                .as_ref()
                .unwrap()
                .reason
                .contains("conflicting records")
        );
    }

    #[test]
    fn describe_findings_refuses_malformed_transfer_metadata() {
        let (root, db, feature) = fixture();
        let directory = root.path().join(".codesage/findings");
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join(format!("{}.json", feature.feature_id));
        let mut record = json!({"finding_id":"fnd_11111111", "file":"src/helper.rs", "line":1, "severity":"high", "title":"A defect", "status":"open"});
        for transfer in [
            json!(false),
            json!(0),
            json!(""),
            json!({}),
            json!([]),
            json!([{}]),
            json!([{"feature_id":"feat_1111111111111111"}]),
            json!([{"feature_id":"", "finding_id":"fnd_22222222"}]),
            json!([{"feature_id":"feat_1111111111111111", "finding_id":false}]),
            json!([{"feature_id":"feat_1111111111111111", "finding_id":"fnd_abcdefgh"}]),
        ] {
            record["ack_transferred_to"] = transfer.clone();
            std::fs::write(
                &path,
                serde_json::to_vec(&json!({"findings":[record]})).unwrap(),
            )
            .unwrap();
            for target in ["src/helper.rs", feature.feature_id.as_str()] {
                let card = card(root.path(), &db, target);
                let section = &card.sections["findings"];
                assert_eq!(section.data, json!({"unscored":true}), "{transfer}");
                assert!(
                    section
                        .completeness
                        .as_ref()
                        .unwrap()
                        .reason
                        .contains("transfer metadata")
                );
            }
        }
        record["ack_transferred_to"] = Value::Null;
        std::fs::write(
            &path,
            serde_json::to_vec(&json!({"findings":[record]})).unwrap(),
        )
        .unwrap();
        for target in ["src/helper.rs", feature.feature_id.as_str()] {
            let card = card(root.path(), &db, target);
            assert_eq!(card.sections["findings"].data["open"], 1);
            assert_eq!(
                card.sections["findings"].data["ids"],
                json!(["fnd_11111111"])
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn describe_findings_refuses_symlinked_components_and_nonregular_stores() {
        use std::os::unix::fs::symlink;
        for component in [
            ".codesage",
            ".codesage/findings",
            ".codesage/findings/feat_1111111111111111.json",
        ] {
            for dangling in [false, true] {
                let (root, db, feature) = fixture();
                let destination = root.path().join("inside");
                if !dangling {
                    std::fs::create_dir_all(&destination).unwrap();
                }
                let link = root.path().join(component);
                std::fs::create_dir_all(link.parent().unwrap()).unwrap();
                symlink(&destination, &link).unwrap();
                for target in ["src/helper.rs", feature.feature_id.as_str()] {
                    let result = card(root.path(), &db, target);
                    let section = &result.sections["findings"];
                    assert_eq!(section.data, json!({"unscored":true}));
                    assert!(
                        section
                            .completeness
                            .as_ref()
                            .unwrap()
                            .reason
                            .contains("symlink"),
                        "{component}"
                    );
                }
            }
        }
        let (root, db, _) = fixture();
        std::fs::create_dir_all(root.path().join(".codesage/findings/directory.json")).unwrap();
        assert!(
            card(root.path(), &db, "src/helper.rs").sections["findings"]
                .completeness
                .as_ref()
                .unwrap()
                .reason
                .contains("not a regular file")
        );
    }
}
