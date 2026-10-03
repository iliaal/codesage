use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Result, ensure};
use codesage_parser::parse::parse_file;
use codesage_protocol::{FeatureConfidence, Language};

use crate::java_roles::{JavaRole, JavaTypeContext, read_source, roles_with_context};
use crate::mappers::shared::walk_files;
use crate::mappers::types::{FeatureMapper, FeatureSeed, MapperContext, SeedFile};

pub struct JavaMapper;

impl FeatureMapper for JavaMapper {
    fn name(&self) -> &'static str {
        "java"
    }

    fn map(&self, ctx: &MapperContext) -> Result<Vec<FeatureSeed>> {
        const WALK_CAP: usize = 50_000;
        let paths = walk_files(ctx.root, ctx.root, WALK_CAP, ctx.excludes);
        ensure!(
            paths.len() < WALK_CAP,
            "Maven discovery reached its {WALK_CAP}-file limit"
        );
        let poms: BTreeSet<&str> = paths
            .iter()
            .filter(|path| {
                ctx.allowed(path) && (path.as_str() == "pom.xml" || path.ends_with("/pom.xml"))
            })
            .map(String::as_str)
            .collect();
        if poms.is_empty() {
            return Ok(Vec::new());
        }
        let java_paths: Vec<_> = paths
            .iter()
            .filter(|path| path.ends_with(".java") && ctx.allowed(path))
            .cloned()
            .collect();
        let context = JavaTypeContext::from_files(ctx.root, &java_paths)?;
        let mut groups: BTreeMap<(String, JavaRole), Vec<String>> = BTreeMap::new();
        for path in &paths {
            if !path.ends_with(".java") || !ctx.allowed(path) {
                continue;
            }
            let module = if path.starts_with("src/main/java/") {
                ""
            } else if let Some((module, _)) = path.rsplit_once("/src/main/java/") {
                module
            } else {
                continue;
            };
            let pom = if module.is_empty() {
                "pom.xml".to_string()
            } else {
                format!("{module}/pom.xml")
            };
            if !poms.contains(pom.as_str()) {
                continue;
            }
            let source = read_source(ctx.root, path)?;
            let tree = parse_file(&source, Language::Java)?;
            for role in roles_with_context(&tree, &source, path, &context) {
                groups
                    .entry((module.to_string(), role))
                    .or_default()
                    .push(path.clone());
            }
        }
        let mut seeds = Vec::new();
        for ((module, role), files) in groups {
            let Some(entry) = files.first() else { continue };
            let label = if module.is_empty() {
                "Maven"
            } else {
                module.as_str()
            };
            let mut seed = FeatureSeed::new(
                role.kind(),
                Language::Java,
                format!("{label} {}", role.as_str()),
                entry,
            );
            seed.source = "java-maven-role";
            seed.confidence = FeatureConfidence::High;
            seed.target_name = Some(role.as_str().to_string());
            seed.summary = format!(
                "Source-evidenced {} Java declarations in {label}",
                role.as_str()
            );
            seed.tags = vec![
                "java".to_string(),
                "maven".to_string(),
                role.as_str().to_string(),
            ];
            seed.owned_files = files
                .into_iter()
                .skip(1)
                .map(|path| SeedFile {
                    path,
                    reason: role.as_str().to_string(),
                })
                .collect();
            seed.context_files.push(SeedFile {
                path: if module.is_empty() {
                    "pom.xml".to_string()
                } else {
                    format!("{module}/pom.xml")
                },
                reason: "Maven module descriptor".to_string(),
            });
            seeds.push(seed);
        }
        Ok(seeds)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use codesage_parser::discover::build_exclude_set;

    fn write(root: &std::path::Path, path: &str, source: &str) {
        let path = root.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, source).unwrap();
    }

    #[test]
    fn maven_groups_roles_per_module_and_honors_source_layout_and_excludes() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, "pom.xml", "<project/>");
        write(root, "child/pom.xml", "<project/>");
        for (file, annotation) in [
            ("Orders", "RestController"),
            ("Users", "RestController"),
            ("Store", "Repository"),
            ("Logic", "Service"),
            ("Helper", "Component"),
        ] {
            let package = if annotation == "RestController" {
                "org.springframework.web.bind.annotation"
            } else {
                "org.springframework.stereotype"
            };
            write(
                root,
                &format!("src/main/java/example/{file}.java"),
                &format!("import {package}.{annotation}; @{annotation} class {file} {{}}"),
            );
        }
        write(
            root,
            "src/main/java/example/Client.java",
            "import org.springframework.cloud.openfeign.FeignClient; @FeignClient(name=\"remote\") interface Client {}",
        );
        write(
            root,
            "src/main/java/example/Config.java",
            "@org.springframework.context.annotation.Configuration class Config {}",
        );
        write(
            root,
            "child/src/main/java/example/Child.java",
            "@org.springframework.stereotype.Repository class Child {}",
        );
        for path in [
            "src/test/java/example/Fake.java",
            "docs/Fake.java",
            "src/main/java/controllers/PlainController.java",
        ] {
            write(
                root,
                path,
                if path.ends_with("PlainController.java") {
                    "class PlainController {}"
                } else {
                    "@org.springframework.stereotype.Repository class Fake {}"
                },
            );
        }
        let seeds = JavaMapper.map(&MapperContext::for_root(root)).unwrap();
        assert_eq!(seeds.len(), 7);
        let web = seeds
            .iter()
            .find(|seed| seed.tags.iter().any(|tag| tag == "web-entrypoint"))
            .unwrap();
        assert_eq!(web.owned_files.len(), 1);
        assert!(
            seeds
                .iter()
                .all(|seed| seed.entry_path.contains("src/main/java/"))
        );
        let excludes = build_exclude_set(&[
            "child".to_string(),
            "src/main/java/example/Users.java".to_string(),
        ])
        .unwrap();
        let seeds = JavaMapper
            .map(&MapperContext {
                root,
                excludes: Some(&excludes),
            })
            .unwrap();
        assert_eq!(seeds.len(), 6);
        assert!(
            seeds
                .iter()
                .all(|seed| !seed.entry_path.starts_with("child/"))
        );
        assert!(seeds.iter().all(|seed| {
            seed.owned_files
                .iter()
                .all(|file| !file.path.ends_with("Users.java"))
        }));
    }
}
