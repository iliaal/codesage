"""Prepare isolated copies; never modify production semantic indexing."""
import argparse
import io
import pathlib
import subprocess
import tarfile

parser = argparse.ArgumentParser()
parser.add_argument("scratch", type=pathlib.Path)
args = parser.parse_args()
args.scratch = args.scratch.resolve()
if args.scratch.exists():
    parser.error("scratch directory must not exist; keep measured inputs immutable")
repo = pathlib.Path(__file__).resolve().parents[2]
dependencies_root = args.scratch / "dependencies"
dependencies_root.mkdir(parents=True)
archive = subprocess.check_output([
    "git", "archive", "8be4cbb8d7c8e9ea4c5ea29fa95ea3a9aefb7584",
    "Cargo.toml", "Cargo.lock", "crates"
], cwd=repo)
with tarfile.open(fileobj=io.BytesIO(archive)) as files:
    files.extractall(dependencies_root, filter="data")
source = subprocess.check_output([
    "git", "show", "8be4cbb8d7c8e9ea4c5ea29fa95ea3a9aefb7584:crates/graph/src/semantic.rs"
], cwd=repo, text=True)
start = source.index("    let chunk_results: Vec<(&FileInfo, Result<Option<ChunkedFile>>)>")
end = source.index("    let mut selected =", start)
candidate = source[:start] + '''    let mut pending = Vec::new();
    let mut pending_bytes = 0usize;
    for file in batch {
        let result = chunk_one(root, file, config);
        let bytes = match &result {
            Ok(Some(cf)) => cf.chunks.iter().map(|(text, _, _)| {
                text.capacity() + 768 * 4 + std::mem::size_of::<Vec<f32>>()
            }).sum(),
            _ => 0usize,
        };
        if !pending.is_empty() && pending_bytes.saturating_add(bytes) > 1024 * 1024 {
            process_chunk_results(db, embedder, std::mem::take(&mut pending), policy, stats)?;
            pending_bytes = 0;
        }
        pending_bytes = pending_bytes.saturating_add(bytes);
        pending.push((*file, result));
    }
    process_chunk_results(db, embedder, pending, policy, stats)
}

fn process_chunk_results(
    db: &Database,
    embedder: &mut dyn TextEmbedder,
    pending: Vec<(&FileInfo, Result<Option<ChunkedFile>>)>,
    policy: BatchPolicy,
    stats: &mut SemanticIndexStats,
) -> Result<()> {
    let BatchPolicy { reuse_stored, purge_failed } = policy;
    let batch_len = pending.len();
    let chunk_results = pending;
''' + source[end:]
candidate = candidate.replace("Vec::with_capacity(batch.len())", "Vec::with_capacity(batch_len)")
candidate = candidate.replace("use rayon::prelude::*;", "")
candidate = candidate.replace("    let BatchPolicy {\n        reuse_stored,\n        purge_failed,\n    } = policy;\n", "", 1)
for name, text in [("baseline", source), ("budget", candidate)]:
    text += '''
pub fn growth_probe(root: &Path) -> anyhow::Result<usize> {
    use std::io::Write;
    let files = codesage_parser::discover::discover_files_with_excludes(root, &[])?;
    anyhow::ensure!(files.len() == 1, "growth probe requires exactly one file");
    let mut file = std::fs::OpenOptions::new().append(true).open(root.join(&files[0].path))?;
    file.write_all(&vec![b'x'; 11 * 1024 * 1024])?;
    let result = chunk_one(root, &files[0], &ChunkConfig::default())?.unwrap();
    Ok(result.chunks.iter().map(|(text,_,_)| text.len()).sum())
}

pub fn failure_probe(root: &Path, path: &Path) -> anyhow::Result<serde_json::Value> {
    struct FailAfter300 { seen: usize }
    impl TextEmbedder for FailAfter300 {
        fn embed_batch(&mut self, texts: &[&str]) -> anyhow::Result<Vec<Vec<f32>>> {
            self.seen += texts.len();
            anyhow::ensure!(self.seen <= 300, "controlled inference failure after 300 texts");
            Ok(vec![vec![0.0; 768]; texts.len()])
        }
    }
    let files = codesage_parser::discover::discover_files_with_excludes(root, &[])?;
    let config = EmbeddingConfig::default();
    let db = Database::open_for_model(path, &config.model, 768)?;
    let fingerprint = SemanticFingerprint::with_artifact_digest(&config, 768, "test-failure-only");
    let result = semantic_index_files(root, &db, &mut FailAfter300 { seen: 0 }, &files, &fingerprint, false);
    anyhow::ensure!(result.is_err(), "controlled failure was not reached");
    Ok(serde_json::json!({"controlled_failure": result.unwrap_err().to_string(),
        "stored_hashes": db.all_semantic_file_hashes()?.len(), "stored_chunks": db.chunk_count()?}))
}
'''
    marker = "    write_semantic_updates(db, &selected, &chunked, &all_embeddings, stats)?;"
    assert text.count(marker) == 1
    text = text.replace(marker, '''    eprintln!("ACCOUNT {}", serde_json::json!({
        "files": selected.len(),
        "chunks": chunked.iter().map(|cf| cf.chunks.len()).sum::<usize>(),
        "chunk_capacity": chunked.iter().flat_map(|cf| &cf.chunks).map(|(t,_,_)| t.capacity()).sum::<usize>(),
        "vector_capacity": all_embeddings.iter().map(|v| v.capacity() * 4).sum::<usize>(),
    }));
''' + marker)
    folder = args.scratch / name
    folder.mkdir(parents=True, exist_ok=True)
    (folder / "main.rs").write_text((repo / "bench/semantic-batch-memory/probe.rs").read_text())
    (folder / "semantic.rs").write_text(text)
    dependencies = "\n".join(
        f'codesage-{crate} = {{ path = "{dependencies_root}/crates/{crate}"' +
        (', features = ["cuda"]' if crate == "embed" else '') + ' }'
        for crate in ["embed", "parser", "protocol", "storage"]
    )
    (folder / "Cargo.toml").write_text(f'''[package]
name = "semantic-memory-{name}"
version = "0.0.0"
edition = "2024"
[workspace]
[[bin]]
name = "semantic-memory-{name}"
path = "main.rs"
[dependencies]
{dependencies}
anyhow = "1"
rayon = "1"
tracing = "0.1"
serde_json = "1"
sha2 = "0.10"
''')
    (folder / "Cargo.lock").write_bytes((dependencies_root / "Cargo.lock").read_bytes())

ordinary = args.scratch / "ordinary"
ordinary.mkdir(exist_ok=True)
paths = sorted(repo / path for path in subprocess.check_output([
    "git", "ls-tree", "-r", "--name-only", "8be4cbb8d7c8e9ea4c5ea29fa95ea3a9aefb7584", "crates"
], cwd=repo, text=True).splitlines() if "/src/" in path and path.endswith(".rs"))
for index, path in enumerate(paths[:50]):
    content = subprocess.check_output([
        "git", "show", f"8be4cbb8d7c8e9ea4c5ea29fa95ea3a9aefb7584:{path.relative_to(repo)}"
    ], cwd=repo)
    (ordinary / f"file_{index:03}.rs").write_bytes(content)
skewed = args.scratch / "skewed"
skewed.mkdir(exist_ok=True)
for index in range(50):
    target = 512 * 1024 if index < 20 else 4096
    rows = []
    size = 0
    row = 0
    while size < target:
        line = f'pub const VALUE_{index}_{row}: &str = "{index * 32452843 + row * 49979687:032x}";\n'
        rows.append(line)
        size += len(line)
        row += 1
    (skewed / f"file_{index:03}.rs").write_text("".join(rows))
