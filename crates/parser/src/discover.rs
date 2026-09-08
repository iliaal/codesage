use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;

use anyhow::Result;
use codesage_protocol::stat_cache::{CachedFileHash, FileStat};
use codesage_protocol::{CoverageSurvey, FileInfo, Language};
use globset::{Glob, GlobSet, GlobSetBuilder};
use sha2::{Digest, Sha256};

use crate::detect::{detect_language_with_dialect, is_unambiguous_cpp_extension};

/// Bound per-file allocation even when generated files evade exclusion patterns.
pub const MAX_INDEXABLE_FILE_BYTES: u64 = 10 * 1024 * 1024;

/// Canonical `files.content_hash`; drift checks must use the same hash.
pub fn content_hash(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

pub fn build_exclude_set(patterns: &[String]) -> Result<GlobSet> {
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        builder.add(Glob::new(pattern)?);
    }
    Ok(builder.build()?)
}

pub fn discover_files_with_excludes(
    root: &Path,
    exclude_patterns: &[String],
) -> Result<Vec<FileInfo>> {
    Ok(discover_files_report_with_excludes(root, exclude_patterns)?.files)
}

pub struct DiscoveryReport {
    pub files: Vec<FileInfo>,
    pub failed_paths: Vec<String>,
    pub hash_cache: HashMap<String, CachedFileHash>,
    pub hashes_reused: usize,
    pub bytes_hashed: u64,
}

pub fn discover_files_report_with_excludes(
    root: &Path,
    exclude_patterns: &[String],
) -> Result<DiscoveryReport> {
    discover_files_report_with_cache(root, exclude_patterns, &HashMap::new())
}

pub fn discover_files_report_with_cache(
    root: &Path,
    exclude_patterns: &[String],
    cache: &HashMap<String, CachedFileHash>,
) -> Result<DiscoveryReport> {
    let excludes = if exclude_patterns.is_empty() {
        None
    } else {
        Some(build_exclude_set(exclude_patterns)?)
    };

    // Resolve header dialect after all workers have observed the project's C++ files.
    let mut builder = ignore::WalkBuilder::new(root);
    builder.hidden(true).git_ignore(true);
    if let Some(excludes_for_filter) = excludes.clone() {
        let root_for_filter = root.to_path_buf();
        builder.filter_entry(move |entry| {
            let path = entry.path();
            let Ok(rel) = path.strip_prefix(&root_for_filter) else {
                return true;
            };
            if rel.as_os_str().is_empty() {
                return true;
            }
            let rel_path = rel.to_string_lossy();
            let is_dir = entry.file_type().is_some_and(|ft| ft.is_dir());
            !exclude_matches_path(&excludes_for_filter, &rel_path, is_dir)
        });
    }
    let walker = builder.build_parallel();

    let saw_cpp = AtomicBool::new(false);
    let first_err: Mutex<Option<anyhow::Error>> = Mutex::new(None);
    let (tx, rx) = mpsc::channel::<FileInfo>();
    let (failed_tx, failed_rx) = mpsc::channel::<String>();
    let (cache_tx, cache_rx) = mpsc::channel();
    let root = root.to_path_buf();

    walker.run(|| {
        let tx = tx.clone();
        let failed_tx = failed_tx.clone();
        let cache_tx = cache_tx.clone();
        let excludes = excludes.clone();
        let saw_cpp = &saw_cpp;
        let first_err = &first_err;
        let root = root.clone();
        Box::new(move |entry| {
            use ignore::WalkState;
            let entry = match entry {
                Ok(e) => e,
                Err(e) => {
                    let mut slot = first_err.lock().unwrap();
                    if slot.is_none() {
                        *slot = Some(anyhow::Error::new(e));
                    }
                    return WalkState::Quit;
                }
            };
            if !entry.file_type().is_some_and(|ft| ft.is_file()) {
                return WalkState::Continue;
            }
            let path = entry.path();
            let Some(language) = detect_language_with_dialect(path, false) else {
                return WalkState::Continue;
            };
            let Some(rel_path) = project_relative_path(&root, path) else {
                tracing::warn!(
                    root = %root.display(),
                    path = %path.display(),
                    "skipping discovered path outside project root"
                );
                return WalkState::Continue;
            };
            if let Some(ref exc) = excludes
                && exclude_matches_path(exc, &rel_path, false)
            {
                return WalkState::Continue;
            }
            let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
            if is_unambiguous_cpp_extension(ext) {
                saw_cpp.store(true, Ordering::Relaxed);
            }
            if let Ok(meta) = entry.metadata()
                && meta.len() > MAX_INDEXABLE_FILE_BYTES
            {
                tracing::warn!(
                    path = %rel_path,
                    bytes = meta.len(),
                    cap = MAX_INDEXABLE_FILE_BYTES,
                    "skipping oversized file"
                );
                return WalkState::Continue;
            }
            let (hash, observation, reused, bytes_hashed) =
                match hash_indexable_file(path, cache.get(&rel_path)) {
                    Ok(Some(c)) => c,
                    Ok(None) => {
                        tracing::warn!(
                            path = %rel_path,
                            cap = MAX_INDEXABLE_FILE_BYTES,
                            "skipping oversized file (post-read or TOCTOU growth)"
                        );
                        return WalkState::Continue;
                    }
                    Err(e) => {
                        tracing::warn!(
                            path = %rel_path,
                            error = %e,
                            "skipping unreadable file"
                        );
                        let _ = failed_tx.send(rel_path);
                        return WalkState::Continue;
                    }
                };
            let _ = cache_tx.send((rel_path.clone(), observation, reused, bytes_hashed));
            if tx
                .send(FileInfo {
                    path: rel_path,
                    language,
                    content_hash: hash,
                })
                .is_err()
            {
                return WalkState::Quit;
            }
            WalkState::Continue
        })
    });
    drop(tx);
    drop(failed_tx);
    drop(cache_tx);

    if let Some(err) = first_err.lock().unwrap().take() {
        return Err(err);
    }

    let mut files: Vec<FileInfo> = rx.iter().collect();
    if saw_cpp.load(Ordering::Relaxed) {
        for f in &mut files {
            if f.path.ends_with(".h") {
                f.language = Language::Cpp;
            }
        }
    }
    files.sort_by(|a, b| a.path.cmp(&b.path));
    let mut failed_paths: Vec<String> = failed_rx.iter().collect();
    failed_paths.sort();
    let mut hash_cache = HashMap::new();
    let mut hashes_reused = 0;
    let mut bytes_hashed = 0;
    for (path, entry, reused, bytes) in cache_rx {
        if let Some(entry) = entry {
            hash_cache.insert(path, entry);
        }
        hashes_reused += usize::from(reused);
        bytes_hashed += bytes;
    }
    Ok(DiscoveryReport {
        files,
        failed_paths,
        hash_cache,
        hashes_reused,
        bytes_hashed,
    })
}

fn now_ns() -> Option<i64> {
    i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()?
            .as_nanos(),
    )
    .ok()
}

#[cfg(unix)]
fn file_stat(meta: &std::fs::Metadata) -> Option<FileStat> {
    use std::os::unix::fs::MetadataExt;
    Some(FileStat {
        size: i64::try_from(meta.len()).ok()?,
        mtime_ns: meta
            .mtime()
            .checked_mul(1_000_000_000)?
            .checked_add(meta.mtime_nsec())?,
        ctime_ns: meta
            .ctime()
            .checked_mul(1_000_000_000)?
            .checked_add(meta.ctime_nsec())?,
    })
}

#[cfg(not(unix))]
fn file_stat(_: &std::fs::Metadata) -> Option<FileStat> {
    None
}

type HashObservation = (String, Option<CachedFileHash>, bool, u64);

fn hash_indexable_file(
    path: &Path,
    cached: Option<&CachedFileHash>,
) -> Result<Option<HashObservation>> {
    let file = std::fs::File::open(path)?;
    let before = file.metadata()?;
    if before.len() > MAX_INDEXABLE_FILE_BYTES {
        return Ok(None);
    }
    let started = now_ns();
    let stat = file_stat(&before);
    if let (Some(cached), Some(stat), Some(now)) = (cached, stat.as_ref(), started)
        && cached.reusable(stat, now)
    {
        return Ok(Some((
            cached.content_hash.clone(),
            Some(cached.clone()),
            true,
            0,
        )));
    }
    let mut content = Vec::new();
    (&file)
        .take(MAX_INDEXABLE_FILE_BYTES + 1)
        .read_to_end(&mut content)?;
    if content.len() as u64 > MAX_INDEXABLE_FILE_BYTES {
        return Ok(None);
    }
    let hash = content_hash(&content);
    let after = file_stat(&file.metadata()?);
    let observation = match (stat, after, started) {
        (Some(stat), Some(after), Some(hashed_at_ns)) if stat == after => Some(CachedFileHash {
            stat,
            content_hash: hash.clone(),
            hashed_at_ns,
        }),
        _ => None,
    };
    Ok(Some((hash, observation, false, content.len() as u64)))
}

fn project_relative_path(root: &Path, path: &Path) -> Option<String> {
    path.strip_prefix(root)
        .ok()
        .map(|rel| rel.to_string_lossy().into_owned())
}

fn exclude_matches_path(excludes: &GlobSet, rel_path: &str, is_dir: bool) -> bool {
    if excludes.is_match(rel_path) {
        return true;
    }
    if !is_dir {
        return false;
    }
    let slash = format!("{rel_path}/");
    if excludes.is_match(&slash) {
        return true;
    }
    excludes.is_match(format!("{rel_path}/_"))
}

/// Discovery's hidden, gitignore, and configured exclusions as a path predicate
/// for watcher registration and events.
pub struct WatchFilter {
    root: PathBuf,
    excludes: Option<GlobSet>,
    gitignore: ignore::gitignore::Gitignore,
}

impl WatchFilter {
    pub fn new(root: &Path, exclude_patterns: &[String]) -> Result<Self> {
        let excludes = if exclude_patterns.is_empty() {
            None
        } else {
            Some(build_exclude_set(exclude_patterns)?)
        };
        let mut gb = ignore::gitignore::GitignoreBuilder::new(root);
        add_nested_gitignores(&mut gb, root, excludes.as_ref());
        let _ = gb.add(root.join(".git").join("info").join("exclude"));
        let gitignore = gb.build()?;
        Ok(Self {
            root: root.to_path_buf(),
            excludes,
            gitignore,
        })
    }

    /// True if `abs_path` (a descendant of the project root) should be skipped.
    /// `is_dir` distinguishes directory globs from file globs.
    pub fn is_ignored(&self, abs_path: &Path, is_dir: bool) -> bool {
        let Ok(rel) = abs_path.strip_prefix(&self.root) else {
            return false;
        };
        if rel.as_os_str().is_empty() {
            return false;
        }
        // Hidden files/dirs — matches `WalkBuilder::hidden(true)`.
        if rel
            .components()
            .any(|c| c.as_os_str().to_string_lossy().starts_with('.'))
        {
            return true;
        }
        let rel_str = rel.to_string_lossy();
        if let Some(exc) = &self.excludes
            && exclude_matches_path(exc, &rel_str, is_dir)
        {
            return true;
        }
        self.gitignore
            .matched_path_or_any_parents(rel, is_dir)
            .is_ignore()
    }
}

fn add_nested_gitignores(
    gb: &mut ignore::gitignore::GitignoreBuilder,
    root: &Path,
    excludes: Option<&GlobSet>,
) {
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let _ = gb.add(dir.join(".gitignore"));

        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let file_type = match entry.file_type() {
                Ok(file_type) => file_type,
                Err(_) => continue,
            };
            if !file_type.is_dir() || file_type.is_symlink() {
                continue;
            }

            let name = entry.file_name();
            if name.to_string_lossy().starts_with('.') {
                continue;
            }
            let Ok(rel) = path.strip_prefix(root) else {
                continue;
            };
            let rel_str = rel.to_string_lossy();
            if let Some(excludes) = excludes
                && exclude_matches_path(excludes, &rel_str, true)
            {
                continue;
            }
            stack.push(path);
        }
    }
}

/// Tests remain indexed for callsites and fixtures, but are demoted in search
/// and excluded from co-change pairs to avoid skewing coupling rankings.
pub const TEST_LIKE_EXCLUDE_PATTERNS: &[&str] = &[
    "**/tests/**",
    "**/test/**",
    "**/__tests__/**",
    "**/Tests/**",
    "**/*Test.php",
    "**/*_test.php",
    "**/*.test.ts",
    "**/*.test.tsx",
    "**/*.test.js",
    "**/*.test.jsx",
    "**/*.spec.ts",
    "**/*.spec.tsx",
    "**/*.spec.js",
    "**/*.spec.jsx",
    "**/test_*.py",
    "**/*_test.py",
    "**/*_test.rs",
    "**/*_test.go",
    // C/C++ tests often live beside production source.
    "**/*_test.cc",
    "**/*_test.cpp",
    "**/*_test.cxx",
    "**/*_test.h",
    "**/*_test.hpp",
    "**/*_benchmark.cc",
    "**/*_benchmark.cpp",
    "**/*_bench.cc",
    "**/*.phpt",
    "**/benches/**",
    "**/benchmarks/**",
];

/// Discovery exclusions that also apply when the checkout has no gitignore rules.
pub const HARD_EXCLUDE_PATTERNS: &[&str] = &[
    // Third-party code
    "**/vendor/**",
    "**/node_modules/**",
    "**/bower_components/**",
    "**/jspm_packages/**",
    "**/.bundle/**",
    // Build output
    "**/dist/**",
    "**/build/**",
    "**/out/**",
    "**/target/**", // Rust, Java/Maven
    "**/_build/**", // Erlang, Elixir, OCaml
    "**/.next/**",
    "**/.nuxt/**",
    "**/.svelte-kit/**",
    "**/.turbo/**",
    "**/.vercel/**",
    "**/.parcel-cache/**",
    "**/.angular/**",
    "**/.gradle/**",
    "**/cmake-build-*/**",
    "**/CMakeFiles/**",
    "**/public/js/**",
    "**/public/build/**",
    "**/storage/framework/views/**",
    // Compiled artifacts
    "**/__pycache__/**",
    "**/*.pyc",
    "**/*.pyo",
    "**/*.class",
    "**/*.o",
    "**/*.obj",
    "**/*.a",
    "**/*.lib",
    "**/*.so",
    "**/*.dylib",
    "**/*.dll",
    "**/*.exe",
    "**/*.egg-info/**",
    // Coverage output
    "**/coverage/**",
    "**/.coverage",
    "**/htmlcov/**",
    "**/.nyc_output/**",
    "**/*.lcov",
    // Tool caches
    "**/.cache/**",
    "**/.pytest_cache/**",
    "**/.ruff_cache/**",
    "**/.mypy_cache/**",
    "**/.tox/**",
    "**/.eslintcache",
    // Bundled and generated files
    "**/*.min.js",
    "**/*.min.css",
    "**/*.min.mjs",
    "**/*.bundle.js",
    "**/*.chunk.js",
    "**/*.generated.*",
    "**/*.gen.ts",
    "**/*.gen.tsx",
    "**/*.gen.js",
    "**/*.gen.go",
    "**/*embeddings*.json",
    // Lockfiles
    "**/package-lock.json",
    "**/yarn.lock",
    "**/pnpm-lock.yaml",
    "**/bun.lockb",
    "**/composer.lock",
    "**/Cargo.lock",
    "**/poetry.lock",
    "**/Pipfile.lock",
    "**/uv.lock",
    "**/Gemfile.lock",
    "**/go.sum",
    "**/mix.lock",
    // Documentation and release notes co-change too broadly for coupling evidence.
    "**/docs/**",
    "**/doc/**",
    "**/site/**",  // mkdocs default output
    "**/_site/**", // jekyll
    "**/NEWS",
    "**/NEWS.md",
    "**/UPGRADING",
    "**/UPGRADING.md",
    "**/UPGRADING.INTERNALS",
    "**/CHANGELOG",
    "**/CHANGELOG.md",
    "**/CHANGELOG.txt",
    "**/CHANGES",
    "**/CHANGES.md",
    "**/HISTORY",
    "**/HISTORY.md",
    "**/RELEASE_NOTES",
    "**/RELEASE_NOTES.md",
    // Editor state
    "**/.idea/**",
    "**/.vscode/**",
    "**/.vs/**",
    "**/*.swp",
    "**/*.swo",
    "**/.DS_Store",
];

/// `[index].exclude_patterns` extends these defaults. Tests remain indexed.
pub const DEFAULT_EXCLUDE_PATTERNS: &[&str] = HARD_EXCLUDE_PATTERNS;

#[cfg(test)]
mod watch_filter_tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn touch(path: &Path) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, "").unwrap();
    }

    #[test]
    fn watch_filter_respects_gitignore_excludes_and_hidden() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        fs::write(root.join(".gitignore"), "generated/\n*.gen.rs\n").unwrap();
        touch(&root.join("src/main.rs"));
        touch(&root.join("generated/out.rs"));
        touch(&root.join("api.gen.rs"));
        touch(&root.join("target/debug/x.rs"));
        touch(&root.join(".hidden/x.rs"));

        let filter = WatchFilter::new(root, &["**/target/**".to_string()]).unwrap();

        assert!(!filter.is_ignored(&root.join("src/main.rs"), false));
        assert!(filter.is_ignored(&root.join("generated"), true));
        assert!(filter.is_ignored(&root.join("generated/out.rs"), false));
        assert!(filter.is_ignored(&root.join("api.gen.rs"), false));
        assert!(filter.is_ignored(&root.join("target/debug/x.rs"), false));
        assert!(filter.is_ignored(&root.join(".hidden/x.rs"), false));
    }

    #[test]
    fn watch_filter_empty_excludes_still_applies_gitignore() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        fs::write(root.join(".gitignore"), "build/\n").unwrap();
        let filter = WatchFilter::new(root, &[]).unwrap();
        assert!(filter.is_ignored(&root.join("build/artifact.rs"), false));
        assert!(!filter.is_ignored(&root.join("lib.rs"), false));
    }

    #[test]
    fn project_relative_path_fails_closed_outside_root() {
        let dir = TempDir::new().unwrap();
        let root = dir.path().join("root");
        let outside = dir.path().join("outside.rs");

        assert!(project_relative_path(&root, &outside).is_none());
        assert_eq!(
            project_relative_path(&root, &root.join("src/lib.rs")).as_deref(),
            Some("src/lib.rs")
        );
    }
}

/// True when a path, or any directory above it, matches an exclude glob.
///
/// Mirrors the outcome of the indexer's `filter_entry` pruning without
/// pruning: a glob naming a directory removes everything beneath it.
fn path_or_ancestor_excluded(excludes: &GlobSet, rel_path: &str) -> bool {
    if exclude_matches_path(excludes, rel_path, false) {
        return true;
    }
    let mut prefix = String::new();
    for segment in rel_path
        .split('/')
        .rev()
        .skip(1)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
    {
        if !prefix.is_empty() {
            prefix.push('/');
        }
        prefix.push_str(segment);
        if exclude_matches_path(excludes, &prefix, true) {
            return true;
        }
    }
    false
}

/// Walk `root` and report coverage without reading or hashing file contents.
///
/// Keep diagnostic-only counting off the incremental indexing path.
pub fn survey_coverage(root: &Path, exclude_patterns: &[String]) -> Result<CoverageSurvey> {
    use std::collections::HashSet;

    let excludes = if exclude_patterns.is_empty() {
        None
    } else {
        Some(build_exclude_set(exclude_patterns)?)
    };

    // Descend excluded directories to count files, then apply ancestor exclusions
    // to match indexing's pruned walk (e.g. bare `vendor` excludes `vendor/dep.rs`).
    let make_walker = |honor_gitignore: bool| {
        let mut builder = ignore::WalkBuilder::new(root);
        builder.hidden(true).git_ignore(honor_gitignore);
        builder.build()
    };

    let mut survey = CoverageSurvey::default();
    // Every path the indexing-equivalent pass accounted for, in any bucket.
    // The gitignore pass reports only what is missing from this set.
    let mut accounted: HashSet<String> = HashSet::new();
    let mut c_headers = 0usize;

    for entry in make_walker(true) {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => {
                // Preserve partial diagnostics while disclosing traversal failures.
                survey.walk_errors += 1;
                continue;
            }
        };
        if !entry.file_type().is_some_and(|ft| ft.is_file()) {
            continue;
        }
        let path = entry.path();
        let Some(rel_path) = project_relative_path(root, path) else {
            continue;
        };
        accounted.insert(rel_path.clone());

        if let Some(ref exc) = excludes
            && path_or_ancestor_excluded(exc, &rel_path)
        {
            survey.excluded += 1;
            continue;
        }

        let Some(language) = detect_language_with_dialect(path, false) else {
            let ext = path
                .extension()
                .and_then(|e| e.to_str())
                .map(|e| format!(".{e}"))
                .unwrap_or_else(|| "<none>".to_string());
            *survey.uncovered_by_extension.entry(ext).or_default() += 1;
            survey.uncovered_total += 1;
            continue;
        };

        // Recognized extensions still fail indexing's size and access checks.
        if let Ok(meta) = entry.metadata()
            && meta.len() > MAX_INDEXABLE_FILE_BYTES
        {
            survey.oversized += 1;
            continue;
        }
        if std::fs::File::open(path).is_err() {
            survey.unreadable += 1;
            continue;
        }

        if rel_path.ends_with(".h") {
            c_headers += 1;
        }
        *survey
            .covered_by_language
            .entry(language.to_string())
            .or_default() += 1;
        survey.covered_total += 1;
    }

    // A second walk reveals gitignored source absent from every first-pass bucket.
    for entry in make_walker(false).flatten() {
        if !entry.file_type().is_some_and(|ft| ft.is_file()) {
            continue;
        }
        let Some(rel_path) = project_relative_path(root, entry.path()) else {
            continue;
        };
        if accounted.contains(&rel_path) {
            continue;
        }
        // Exclude build output from the unexpectedly gitignored source count.
        if let Some(ref exc) = excludes
            && path_or_ancestor_excluded(exc, &rel_path)
        {
            continue;
        }
        // Only report ignored files indexing could otherwise have handled;
        // an ignored .png is noise.
        if detect_language_with_dialect(entry.path(), false).is_some() {
            survey.gitignored_source += 1;
        }
    }

    // Apply discovery's project-wide header dialect to the coverage counts.
    let cpp_key = Language::Cpp.to_string();
    let c_key = Language::C.to_string();
    if c_headers > 0 && survey.covered_by_language.contains_key(&cpp_key) {
        if let Some(c_total) = survey.covered_by_language.get_mut(&c_key) {
            *c_total = c_total.saturating_sub(c_headers);
            if *c_total == 0 {
                survey.covered_by_language.remove(&c_key);
            }
        }
        *survey.covered_by_language.entry(cpp_key).or_default() += c_headers;
    }

    Ok(survey)
}

#[cfg(test)]
mod coverage_survey_tests {
    use super::survey_coverage;
    use std::fs;

    fn write(dir: &std::path::Path, rel: &str, body: &str) {
        let p = dir.join(rel);
        if let Some(parent) = p.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(p, body).unwrap();
    }

    #[test]
    fn counts_what_indexing_would_and_would_not_see() {
        let dir = tempfile::tempdir().unwrap();
        let r = dir.path();
        write(r, "src/main.rs", "fn main() {}");
        write(r, "src/lib.rs", "pub fn a() {}");
        write(r, "app/User.php", "<?php class User {}");
        write(r, "lib/thing.rb", "class Thing; end");
        write(r, "lib/other.rb", "class Other; end");
        write(r, "App.swift", "struct App {}");
        // A shebang does not override extension-based detection.
        write(r, "bin/deploy", "#!/usr/bin/env bash\necho hi");

        let s = survey_coverage(r, &[]).unwrap();

        assert_eq!(s.covered_total, 3, "rust x2 + php x1");
        assert_eq!(s.covered_by_language.get("rust"), Some(&2));
        assert_eq!(s.covered_by_language.get("php"), Some(&1));

        assert_eq!(s.uncovered_total, 4, "2 ruby + 1 swift + 1 extensionless");
        assert_eq!(s.uncovered_by_extension.get(".rb"), Some(&2));
        assert_eq!(s.uncovered_by_extension.get(".swift"), Some(&1));
        assert_eq!(
            s.uncovered_by_extension.get("<none>"),
            Some(&1),
            "extensionless files must be reported, not silently dropped"
        );
    }

    #[test]
    fn a_recognized_file_over_the_size_cap_is_not_counted_as_covered() {
        let dir = tempfile::tempdir().unwrap();
        let r = dir.path();
        write(r, "small.rs", "fn a() {}");
        let big = "x".repeat((super::MAX_INDEXABLE_FILE_BYTES + 1) as usize);
        write(r, "huge.py", &big);

        let s = survey_coverage(r, &[]).unwrap();
        assert_eq!(s.covered_total, 1, "only small.rs is indexable");
        assert_eq!(
            s.oversized, 1,
            "huge.py must be reported, not counted covered"
        );
        assert_eq!(s.covered_by_language.get("python"), None);
    }

    #[test]
    fn a_directory_shaped_exclude_prunes_its_files() {
        let dir = tempfile::tempdir().unwrap();
        let r = dir.path();
        write(r, "src/main.rs", "fn main() {}");
        write(r, "vendor/dep.rs", "fn dep() {}");
        write(r, "vendor/nested/deep.rs", "fn deep() {}");

        let s = survey_coverage(r, &["vendor".to_string()]).unwrap();
        assert_eq!(
            s.covered_total, 1,
            "a directory-shaped exclude must prune its whole subtree"
        );
    }

    #[test]
    fn gitignored_source_is_reported_rather_than_vanishing() {
        let dir = tempfile::tempdir().unwrap();
        let r = dir.path();
        // The ignore crate only applies .gitignore inside a git repository.
        fs::create_dir_all(r.join(".git")).unwrap();
        write(r, ".gitignore", "generated/\n*.png\n");
        write(r, "src/main.rs", "fn main() {}");
        write(r, "generated/api.rs", "pub fn generated() {}");
        write(r, "logo.png", "not really a png");

        let s = survey_coverage(r, &[]).unwrap();
        assert_eq!(s.covered_total, 1, "only src/main.rs reaches indexing");
        assert_eq!(
            s.gitignored_source, 1,
            "the gitignored .rs must be reported; the .png must not"
        );
    }

    #[test]
    fn gitignored_and_excluded_is_not_reported_as_a_surprise() {
        let dir = tempfile::tempdir().unwrap();
        let r = dir.path();
        fs::create_dir_all(r.join(".git")).unwrap();
        write(r, ".gitignore", "target/\nscratch/\n");
        write(r, "src/main.rs", "fn main() {}");
        write(r, "target/debug/build/generated.rs", "pub fn gen() {}");
        write(r, "scratch/idea.rs", "fn idea() {}");

        let s = survey_coverage(r, &["**/target/**".to_string()]).unwrap();
        assert_eq!(
            s.gitignored_source, 1,
            "scratch/idea.rs is a real surprise; target/ build output is not"
        );
    }

    #[test]
    fn headers_follow_the_project_wide_cpp_flip() {
        let dir = tempfile::tempdir().unwrap();
        let r = dir.path();
        write(r, "src/app.cc", "int main() {}");
        write(r, "src/app.h", "#pragma once");

        let s = survey_coverage(r, &[]).unwrap();
        assert_eq!(
            s.covered_by_language.get("cpp"),
            Some(&2),
            "{:?}",
            s.covered_by_language
        );
        assert_eq!(s.covered_by_language.get("c"), None);
    }

    #[test]
    fn excluded_files_are_counted_separately_from_uncovered() {
        let dir = tempfile::tempdir().unwrap();
        let r = dir.path();
        write(r, "src/main.rs", "fn main() {}");
        write(r, "vendor/dep.rs", "fn dep() {}");
        write(r, "notes.rb", "puts 1");

        let s = survey_coverage(r, &["**/vendor/**".to_string()]).unwrap();

        assert_eq!(s.excluded, 1);
        assert_eq!(s.covered_total, 1, "vendor/dep.rs excluded, not covered");
        assert_eq!(s.uncovered_total, 1, "notes.rb");
    }

    #[test]
    fn covered_fraction_reports_the_gap_and_survives_an_empty_repo() {
        let dir = tempfile::tempdir().unwrap();
        let r = dir.path();
        write(r, "a.rs", "fn a() {}");
        write(r, "b.rb", "puts 1");
        let s = survey_coverage(r, &[]).unwrap();
        assert!((s.covered_fraction() - 0.5).abs() < 1e-9);

        let empty = tempfile::tempdir().unwrap();
        let s = survey_coverage(empty.path(), &[]).unwrap();
        assert_eq!(
            s.covered_fraction(),
            1.0,
            "empty repo must not divide by zero"
        );
    }
}
