use std::path::Path;

use anyhow::Result;
use codesage_protocol::FileInfo;
use globset::{Glob, GlobSet, GlobSetBuilder};
use sha2::{Digest, Sha256};

use crate::detect::detect_language;

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
    let mut files = Vec::new();
    let excludes = if exclude_patterns.is_empty() {
        None
    } else {
        Some(build_exclude_set(exclude_patterns)?)
    };

    let walker = ignore::WalkBuilder::new(root)
        .hidden(true)
        .git_ignore(true)
        .build();

    for entry in walker {
        let entry = entry?;
        if !entry.file_type().is_some_and(|ft| ft.is_file()) {
            continue;
        }

        let path = entry.path();
        let Some(language) = detect_language(path) else {
            continue;
        };

        let rel_path = path
            .strip_prefix(root)
            .unwrap_or(path)
            .to_string_lossy()
            .into_owned();

        if let Some(ref exc) = excludes
            && exc.is_match(&rel_path)
        {
            continue;
        }

        let content = std::fs::read(path)?;
        let hash = hex::encode(Sha256::digest(&content));

        files.push(FileInfo {
            path: rel_path,
            language,
            content_hash: hash,
        });
    }

    files.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(files)
}

/// Always-applied defaults. User config in `[index].exclude_patterns` extends this list.
/// Most build outputs live under top-level gitignore on real projects already; these defaults
/// catch the cases where they do not (vendored repos, sandboxed checkouts) and add language /
/// IDE / cache patterns that gitignore alone misses.
/// Subset of `DEFAULT_EXCLUDE_PATTERNS` that identifies test and bench files.
/// Used by `git_history` to keep these files in `git_files` (so `recommend_tests`
/// and `assess_risk`'s test-gap check can find them) while still dropping them
/// from co-change pair generation (where they'd pair with everything they cover
/// and skew coupling rankings).
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

pub const DEFAULT_EXCLUDE_PATTERNS: &[&str] = &[
    // ----- tests (excluded from semantic search; structural pass still sees test code) -----
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
