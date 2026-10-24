use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anyhow::{Result, ensure};
use codesage_protocol::{
    FeatureConfidence, FeatureEntrypoint, FeatureKind, FileCategory, Language,
};

use super::{FeatureSeed, MapperContext, SeedFile, php_api_entries_in_file};
use crate::mappers::shared::{SOURCE_FILE_CAP, collect_source_files};

// Preserve the previous extension bundle's forty-source bound without dropping later files.
const MAX_GROUP_FILES: usize = 40;

pub(super) fn extension_groups(
    ctx: &MapperContext,
    directory: &str,
    context_files: Vec<SeedFile>,
    test_prefixes: Vec<String>,
) -> Result<Vec<FeatureSeed>> {
    let files = collect_source_files(
        ctx,
        &[ctx.root.join(directory)],
        usize::MAX,
        |path| {
            Path::new(path)
                .extension()
                .and_then(|ext| ext.to_str())
                .is_some_and(|ext| matches!(ext, "c" | "cc" | "cpp" | "cxx" | "h" | "hpp" | "hh"))
        },
        |path| {
            matches!(FileCategory::classify(path), FileCategory::Test)
                || Path::new(path)
                    .file_stem()
                    .and_then(|stem| stem.to_str())
                    .is_some_and(|stem| stem.ends_with("_arginfo"))
        },
        SOURCE_FILE_CAP + 1,
    );
    ensure!(
        files.len() <= SOURCE_FILE_CAP,
        "PHP extension {directory} exceeds {SOURCE_FILE_CAP} source files"
    );
    let mut seeds = Vec::new();
    for group in partition(directory, files) {
        let mut entrypoints = BTreeSet::new();
        for file in &group.files {
            if matches!(
                Path::new(file).extension().and_then(|ext| ext.to_str()),
                Some("c" | "cc" | "cpp" | "cxx")
            ) {
                entrypoints.extend(php_api_entries_in_file(ctx.root, file).into_iter().map(
                    |entry| FeatureEntrypoint {
                        path: entry.entry_path,
                        symbol: entry.entry_symbol,
                    },
                ));
            }
        }
        let label = match &group.partition {
            Some(partition) => format!("{} ({partition})", group.directory),
            None => group.directory.clone(),
        };
        seeds.push(FeatureSeed {
            summary: format!(
                "{} source files and {} PHP API entrypoints in {label}",
                group.files.len(),
                entrypoints.len()
            ),
            source: "php-ext-source-group",
            confidence: FeatureConfidence::Low,
            entrypoints: entrypoints.into_iter().collect(),
            target_name: group.partition,
            tags: vec!["php".into(), "php-src".into(), "source-group".into()],
            owned_files: group
                .files
                .into_iter()
                .map(|path| SeedFile {
                    path,
                    reason: "extension source group".into(),
                })
                .collect(),
            context_files: context_files.clone(),
            test_prefixes: test_prefixes.clone(),
            ..FeatureSeed::new(
                FeatureKind::Library,
                Language::Php,
                format!("PHP sources `{label}`"),
                group.directory,
            )
        });
    }
    Ok(seeds)
}

#[derive(Debug, PartialEq, Eq)]
struct SourceGroup {
    directory: String,
    partition: Option<String>,
    files: Vec<String>,
}

fn partition(directory: &str, mut files: Vec<String>) -> Vec<SourceGroup> {
    files.sort();
    files.dedup();
    let mut pending = vec![(directory.to_string(), files)];
    let mut groups = Vec::new();
    while let Some((directory, files)) = pending.pop() {
        if files.is_empty() {
            continue;
        }
        if files.len() <= MAX_GROUP_FILES {
            groups.push(SourceGroup {
                directory,
                partition: None,
                files,
            });
            continue;
        }
        let prefix = format!("{directory}/");
        let mut children: BTreeMap<String, Vec<String>> = BTreeMap::new();
        let mut direct = Vec::new();
        for file in files {
            let relative = file
                .strip_prefix(&prefix)
                .expect("group source is below its directory");
            if let Some((child, _)) = relative.split_once('/') {
                children
                    .entry(format!("{directory}/{child}"))
                    .or_default()
                    .push(file);
            } else {
                direct.push(file);
            }
        }
        pending.extend(children);
        // Sorting keeps adjacent filename families together; an oversized flat family
        // is split rather than emitted as an unbounded group or silently truncated.
        for chunk in direct.chunks(MAX_GROUP_FILES) {
            groups.push(SourceGroup {
                directory: directory.clone(),
                partition: Some(
                    chunk[0]
                        .strip_prefix(&prefix)
                        .expect("direct source")
                        .to_string(),
                ),
                files: chunk.to_vec(),
            });
        }
    }
    groups.sort_by(|a, b| (&a.directory, &a.partition).cmp(&(&b.directory, &b.partition)));
    groups
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_nested_and_flat_directories_without_loss_or_duplicates() {
        let mut files = (0..97)
            .map(|i| format!("ext/demo/flat/family_{i:03}.c"))
            .collect::<Vec<_>>();
        files.extend((0..43).map(|i| format!("ext/demo/nested/sub/source_{i:03}.h")));
        files.extend(["ext/demo/main.c".into(), "ext/demo/main.h".into()]);
        let expected = files.iter().cloned().collect::<BTreeSet<_>>();
        let first = partition("ext/demo", files.clone());
        assert!(
            first
                .iter()
                .all(|group| group.files.len() <= MAX_GROUP_FILES)
        );
        assert_eq!(
            first.iter().map(|group| group.files.len()).sum::<usize>(),
            expected.len()
        );
        assert_eq!(
            first
                .iter()
                .flat_map(|group| group.files.iter().cloned())
                .collect::<BTreeSet<_>>(),
            expected
        );
        assert!(
            first
                .iter()
                .any(|group| group.directory == "ext/demo/nested/sub")
        );
        files.reverse();
        assert_eq!(first, partition("ext/demo", files));
    }
}
