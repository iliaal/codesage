use std::path::Path;
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
    let walker = ignore::WalkBuilder::new(root)
        .hidden(true)
        .git_ignore(true)
        .build_parallel();

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
            let rel_path = path
                .strip_prefix(&root)
                .unwrap_or(path)
                .to_string_lossy()
                .into_owned();
            if let Some(ref exc) = excludes
                && exc.is_match(&rel_path)
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
            let content = match std::fs::read(path) {
                Ok(c) => c,
                Err(e) => {
                    let mut slot = first_err.lock().unwrap();
                    if slot.is_none() {
                        *slot = Some(
                            anyhow::Error::new(e)
                                .context(format!("reading file at {}", path.display())),
                        );
                    }
                    return WalkState::Quit;
                }
            };
            let hash = hex::encode(Sha256::digest(&content));
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
