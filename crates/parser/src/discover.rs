use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;

use anyhow::Result;
use codesage_protocol::{FileInfo, Language};
use globset::{Glob, GlobSet, GlobSetBuilder};
use sha2::{Digest, Sha256};

use crate::detect::{detect_language_with_dialect, is_unambiguous_cpp_extension};

/// Skip files larger than this at discovery time. `HARD_EXCLUDE_PATTERNS`
/// catches the common offenders (lockfiles, minified JS, build outputs), but
/// a stray large generated SQL dump or vendored data file in the project
/// root would otherwise be `fs::read` into memory and OOM the indexer. 10MB
/// is well above any real source file (php-src's biggest .c hovers ~1.5MB)
/// while bounding worst-case allocation per file.
pub const MAX_INDEXABLE_FILE_BYTES: u64 = 10 * 1024 * 1024;

/// The canonical per-file content hash the indexer stores in `files.content_hash`.
/// Single source of truth: any consumer that wants to detect drift against the
/// index (e.g. the MCP staleness banner) must hash with this exact function, or
/// comparisons against the stored hash are meaningless.
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
    let excludes = if exclude_patterns.is_empty() {
        None
    } else {
        Some(build_exclude_set(exclude_patterns)?)
    };

    // Parallel walk + read + hash. The previous serial version was the
    // dominant indexing bottleneck on monorepos: SHA-256 hashing every source
    // file in series is CPU-bound and reads block on `fs::read`. Fan-out is
    // bounded by `WalkParallel`'s default thread count (logical CPUs).
    //
    // First pass collects every relevant file with a tentative language;
    // `.h` defaults to C and a second pass flips it to C++ when the project
    // contains an unambiguous C++ extension. We track the C++ signal across
    // workers via `AtomicBool` and drain finished file rows through an mpsc
    // channel — single-producer-single-consumer per thread, no shared Vec
    // contention.
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
    let root = root.to_path_buf();

    walker.run(|| {
        let tx = tx.clone();
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
            let content = match read_indexable_content(path) {
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
                    // Skip an individual unreadable file rather than aborting the
                    // whole index — matching the oversized-file branch above. A
                    // permission-restricted file, or one deleted mid-walk (the
                    // post-checkout git hook races `git checkout`/`stash`), must
                    // not fail a whole-project reindex. Walk-entry errors (a
                    // directory we can't traverse) still Quit above, since those
                    // mean the discovered file set is genuinely incomplete.
                    tracing::warn!(
                        path = %rel_path,
                        error = %e,
                        "skipping unreadable file"
                    );
                    return WalkState::Continue;
                }
            };
            let hash = content_hash(&content);
            // Receiver drop is fine — just bail.
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
    Ok(files)
}

fn project_relative_path(root: &Path, path: &Path) -> Option<String> {
    path.strip_prefix(root)
        .ok()
        .map(|rel| rel.to_string_lossy().into_owned())
}

/// Read up to `MAX_INDEXABLE_FILE_BYTES` bytes. Returns `Ok(None)` when the
/// file exceeds the cap (including a TOCTOU growth between metadata and read).
fn read_indexable_content(path: &Path) -> Result<Option<Vec<u8>>> {
    let file = std::fs::File::open(path)?;
    let cap = MAX_INDEXABLE_FILE_BYTES;
    let mut limited = file.take(cap.saturating_add(1));
    let mut content = Vec::new();
    limited.read_to_end(&mut content)?;
    if content.len() as u64 > cap {
        return Ok(None);
    }
    Ok(Some(content))
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

/// Replicates the indexer's discovery filters — hidden files,
/// `.gitignore` / `.git/info/exclude`, and `[index].exclude_patterns` — as a
/// single-path predicate. The live watcher uses it both to prune the inotify
/// watch set (never watching `target/`, `.git/`, `node_modules/`, or anything
/// gitignored) and to filter individual events, so it reindexes exactly the
/// file set the indexer would.
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

/// Test and benchmark files. These ARE indexed structurally and semantically
/// (so `find_references` can see test callsites and `find_symbol` can find test
/// fixtures), but the search ranker demotes them via path-based penalties and
/// the git-history layer drops them from co-change pair generation (where they'd
/// pair with everything they cover and skew coupling rankings).
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
    "**/*.phpt",
    "**/benches/**",
    "**/benchmarks/**",
];

/// Files that never enter any index — third-party code, build outputs, generated
/// artifacts, lockfiles, docs/changelogs, IDE state. Dropped at file discovery
/// time. Most live under project gitignore already; these defaults catch the
/// cases where they don't (vendored repos, sandboxed checkouts) and add
/// language/IDE/cache patterns that gitignore alone misses.
pub const HARD_EXCLUDE_PATTERNS: &[&str] = &[
    // ----- third-party / vendored code -----
    "**/vendor/**",
    "**/node_modules/**",
    "**/bower_components/**",
    "**/jspm_packages/**",
    "**/.bundle/**",
    // ----- compiled / bundled outputs -----
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
    // ----- language caches / compiled artifacts -----
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
    // ----- coverage / test output -----
    "**/coverage/**",
    "**/.coverage",
    "**/htmlcov/**",
    "**/.nyc_output/**",
    "**/*.lcov",
    // ----- tool caches -----
    "**/.cache/**",
    "**/.pytest_cache/**",
    "**/.ruff_cache/**",
    "**/.mypy_cache/**",
    "**/.tox/**",
    "**/.eslintcache",
    // ----- minified / bundled JS/CSS -----
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
    // ----- lock files (huge, low signal) -----
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
    // ----- docs and changelogs (low signal; in git history, NEWS/UPGRADING co-change
    //       with everything because the team touches them on every commit) -----
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
    // ----- IDE / editor (most are dotfiles already filtered, but be explicit) -----
    "**/.idea/**",
    "**/.vscode/**",
    "**/.vs/**",
    "**/*.swp",
    "**/*.swo",
    "**/.DS_Store",
];

/// Excludes applied at file discovery time. User config in
/// `[index].exclude_patterns` extends this list. Equals `HARD_EXCLUDE_PATTERNS`;
/// `TEST_LIKE_EXCLUDE_PATTERNS` are intentionally NOT excluded here so that the
/// structural graph stays correct on test code (callsites, fixtures) and the
/// search ranker can demote them at rank time instead.
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

        // Real source is watched.
        assert!(!filter.is_ignored(&root.join("src/main.rs"), false));
        // Gitignored dir + pattern are skipped.
        assert!(filter.is_ignored(&root.join("generated"), true));
        assert!(filter.is_ignored(&root.join("generated/out.rs"), false));
        assert!(filter.is_ignored(&root.join("api.gen.rs"), false));
        // exclude_patterns globset is honored.
        assert!(filter.is_ignored(&root.join("target/debug/x.rs"), false));
        // Hidden dirs are skipped (matches WalkBuilder::hidden(true)).
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
