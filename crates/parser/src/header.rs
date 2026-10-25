use std::collections::{HashMap, HashSet};
use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::{Result, ensure};
use codesage_protocol::{FileInfo, Language};

use crate::detect::is_unambiguous_cpp_extension;
use crate::parse::parse_file_tolerant;
use crate::preproc::DeadRegions;
use crate::source::read_indexable_file;

/// One-hop evidence only: a C-compatible header included from C++ does not
/// turn its own include tree into C++. Missing build include paths are resolved
/// only when the indexed suffix is unique. For C-degraded ambiguous headers,
/// retain the legacy project dialect, also for otherwise clean C trees with
/// macro calls in function declarators. Error counts alone do not establish
/// that C preserves the symbols and ranges recovered by the C++ grammar.
pub fn cpp_headers(root: &Path, files: &[FileInfo]) -> Result<HashSet<String>> {
    cpp_headers_with_context(root, files, &[])
}

/// Reuse unchanged header decisions only when the admitted header set and all
/// C++ evidence are unchanged. Callers must check structural interpretation
/// before supplying persisted decisions.
pub fn cpp_headers_with_context(
    root: &Path,
    files: &[FileInfo],
    previous: &[FileInfo],
) -> Result<HashSet<String>> {
    let headers: HashSet<_> = files
        .iter()
        .filter(|file| file.path.ends_with(".h"))
        .map(|file| file.path.as_str())
        .collect();
    let mut cpp = HashSet::new();
    if headers.is_empty() {
        return Ok(cpp);
    }
    let prior_headers: HashMap<_, _> = previous
        .iter()
        .filter(|file| file.path.ends_with(".h"))
        .map(|file| (file.path.as_str(), file))
        .collect();
    let cpp_evidence = |files: &[FileInfo]| {
        files
            .iter()
            .filter(|file| is_cpp_evidence(Path::new(&file.path)))
            .map(|file| (file.path.clone(), file.content_hash.clone()))
            .collect::<HashMap<_, _>>()
    };
    let stable_evidence = headers.len() == prior_headers.len()
        && headers
            .iter()
            .all(|header| prior_headers.contains_key(header))
        && cpp_evidence(files) == cpp_evidence(previous);
    let reusable: HashMap<_, _> = files
        .iter()
        .filter(|file| stable_evidence && file.path.ends_with(".h"))
        .filter_map(|file| {
            prior_headers.get(file.path.as_str()).and_then(|prior| {
                (prior.content_hash == file.content_hash)
                    .then_some((file.path.as_str(), prior.language))
            })
        })
        .collect();
    cpp.extend(
        reusable
            .iter()
            .filter(|(_, language)| **language == Language::Cpp)
            .map(|(path, _)| (*path).to_owned()),
    );
    let cpp_sources: Vec<_> = files
        .iter()
        .filter(|file| is_cpp_evidence(Path::new(&file.path)))
        .collect();
    let cpp_directories: HashSet<_> = cpp_sources
        .iter()
        .filter_map(|file| Path::new(&file.path).parent())
        .collect();
    cpp.extend(
        headers
            .iter()
            .filter(|header| {
                Path::new(header)
                    .parent()
                    .is_some_and(|parent| cpp_directories.contains(parent))
            })
            .map(|header| (*header).to_owned()),
    );
    let mut pending: HashSet<_> = headers
        .iter()
        .copied()
        .filter(|header| !cpp.contains(*header) && !reusable.contains_key(header))
        .collect();
    for file in &cpp_sources {
        codesage_protocol::work::checkpoint()?;
        let parent = Path::new(&file.path).parent().unwrap_or(Path::new(""));
        let Some(source) = read_indexable_file(root, Path::new(&file.path))? else {
            continue;
        };
        ensure!(
            file.content_hash.is_empty() || source.content_hash == file.content_hash,
            "{} changed while establishing header routing; retry discovery",
            file.path
        );
        if pending.is_empty() {
            continue;
        }
        let directives = crate::export_macro::directives_only(&source.bytes);
        let parsed = parse_file_tolerant(&directives, Language::C)?;
        let dead = DeadRegions::scan(parsed.tree.root_node(), &source.bytes, Language::Cpp);
        let mut cursor = parsed.tree.walk();
        loop {
            codesage_protocol::work::checkpoint()?;
            let node = cursor.node();
            if node.kind() == "preproc_include"
                && !dead.covers(&node)
                && let Some(path) = node.child_by_field_name("path")
                && matches!(path.kind(), "string_literal" | "system_lib_string")
                && let Ok(text) = path.utf8_text(&source.bytes)
                && text.len() >= 2
            {
                let include = &text[1..text.len() - 1];
                let quoted = path.kind() == "string_literal";
                if let Some(header) = resolve_include(parent, include, quoted, &headers) {
                    cpp.insert(header.to_owned());
                    pending.remove(header);
                }
            }
            if cursor.goto_first_child() {
                continue;
            }
            loop {
                if cursor.goto_next_sibling() {
                    break;
                }
                if !cursor.goto_parent() {
                    break;
                }
            }
            if cursor.node() == parsed.tree.root_node() {
                break;
            }
        }
    }
    if !cpp_sources.is_empty() {
        for file in files.iter().filter(|file| file.path.ends_with(".h")) {
            let header = file.path.as_str();
            codesage_protocol::work::checkpoint()?;
            if cpp.contains(header) || reusable.contains_key(header) {
                continue;
            }
            let Some(source) = read_indexable_file(root, Path::new(header))? else {
                continue;
            };
            ensure!(
                file.content_hash.is_empty() || file.content_hash == source.content_hash,
                "{header} changed while establishing header routing; retry discovery"
            );
            if !crate::parse::c_header_prefers_c(&source.bytes)? {
                cpp.insert(header.to_owned());
            }
        }
    }
    Ok(cpp)
}

pub fn is_cpp_evidence(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(is_unambiguous_cpp_extension)
}

pub(crate) fn unchanged_context(
    files: &[FileInfo],
    previous: &[FileInfo],
) -> Option<HashSet<String>> {
    if previous.is_empty() {
        return None;
    }
    let relevant =
        |file: &&FileInfo| file.path.ends_with(".h") || is_cpp_evidence(Path::new(&file.path));
    let current: HashMap<_, _> = files
        .iter()
        .filter(relevant)
        .map(|file| (file.path.as_str(), file.content_hash.as_str()))
        .collect();
    let prior: HashMap<_, _> = previous
        .iter()
        .filter(relevant)
        .map(|file| (file.path.as_str(), file.content_hash.as_str()))
        .collect();
    (current == prior).then(|| {
        previous
            .iter()
            .filter(|file| file.path.ends_with(".h") && file.language == Language::Cpp)
            .map(|file| file.path.clone())
            .collect()
    })
}

/// Overlays cannot add files. New header-routing evidence requires persistent
/// discovery so suffix ambiguity and the admissible source set stay identical.
pub fn verify_membership(root: &Path, files: &[FileInfo], excludes: &[String]) -> Result<()> {
    let known: HashSet<_> = files.iter().map(|file| file.path.as_str()).collect();
    let observed = Mutex::new(HashSet::new());
    let excludes = crate::discover::build_exclude_set(excludes)?;
    let root_owned = root.to_path_buf();
    let mut walk = ignore::WalkBuilder::new(root);
    walk.threads(
        std::thread::available_parallelism()
            .map_or(1, usize::from)
            .min(4),
    )
    .hidden(true)
    .git_ignore(true)
    .filter_entry(move |entry| {
        let Ok(relative) = entry.path().strip_prefix(&root_owned) else {
            return false;
        };
        relative.as_os_str().is_empty()
            || !crate::discover::exclude_matches_path(
                &excludes,
                &relative.to_string_lossy(),
                entry.file_type().is_some_and(|kind| kind.is_dir()),
            )
    });
    let context_entries = AtomicUsize::new(0);
    let first_error = Mutex::new(None);
    let control = codesage_protocol::work::current();
    walk.build_parallel().run(|| {
        Box::new(|entry| {
            let result = (|| -> Result<()> {
                // The caller's thread-local deadline is not inherited by walk workers.
                if let Some(control) = &control {
                    control.check()?;
                }
                let entry = entry?;
                let is_directory = entry.file_type().is_some_and(|kind| kind.is_dir());
                if !is_directory {
                    if !entry.file_type().is_some_and(|kind| kind.is_file()) {
                        return Ok(());
                    }
                    let path = entry.path();
                    if !is_cpp_evidence(path) && path.extension().is_none_or(|ext| ext != "h") {
                        return Ok(());
                    }
                    if entry.metadata()?.len() > crate::discover::MAX_INDEXABLE_FILE_BYTES {
                        return Ok(());
                    }
                }
                ensure!(
                    context_entries.fetch_add(1, Ordering::Relaxed) < 16_384,
                    "header dialect context exceeds 16384 routing candidates and directories; run codesage index"
                );
                if !is_directory {
                    let relative = entry.path().strip_prefix(root)?.to_string_lossy();
                    ensure!(
                        known.contains(relative.as_ref()),
                        "header dialect requires codesage index: new candidate {relative}"
                    );
                    observed.lock().unwrap().insert(relative.into_owned());
                }
                Ok(())
            })();
            match result {
                Ok(()) => ignore::WalkState::Continue,
                Err(error) => {
                    first_error.lock().unwrap().get_or_insert(error);
                    ignore::WalkState::Quit
                }
            }
        })
    });
    if let Some(error) = first_error.into_inner().unwrap() {
        return Err(error);
    }
    codesage_protocol::work::checkpoint()?;
    let observed = observed.into_inner().unwrap();
    for file in files
        .iter()
        .filter(|file| file.path.ends_with(".h") || is_cpp_evidence(Path::new(&file.path)))
    {
        if !observed.contains(&file.path) && root.join(&file.path).try_exists()? {
            anyhow::bail!(
                "header dialect requires codesage index: candidate {} is no longer admitted",
                file.path
            );
        }
    }
    Ok(())
}

fn resolve_include<'a>(
    parent: &Path,
    include: &str,
    quoted: bool,
    headers: &HashSet<&'a str>,
) -> Option<&'a str> {
    if quoted
        && let Some(relative) = normalize(&parent.join(include))
        && let Some(header) = headers.get(relative.to_str()?)
    {
        return Some(header);
    }
    let normalized = normalize(Path::new(include))?;
    let normalized = normalized.to_str()?;
    if let Some(header) = headers.get(normalized) {
        return Some(header);
    }
    let suffix = format!("/{normalized}");
    let mut candidates = headers.iter().filter(|header| header.ends_with(&suffix));
    let first = *candidates.next()?;
    candidates.next().is_none().then_some(first)
}

fn normalize(path: &Path) -> Option<PathBuf> {
    let mut result = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => result.push(part),
            Component::CurDir => {}
            Component::ParentDir => {
                if !result.pop() {
                    return None;
                }
            }
            Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    Some(result)
}
