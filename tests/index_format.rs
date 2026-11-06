//! A persisted index is reused only while `INDEX_FORMAT_VERSION` matches, so
//! any change to what indexing produces must bump it. This test indexes a copy
//! of `test-fixtures` and compares a hash of each output with the newest line
//! for it in `tests/index_format_fingerprints.txt`.
#![cfg(unix)]

use narsil_mcp::callgraph::CallGraph;
use narsil_mcp::engine::{CodeIntelEngine, EngineOptions};
use narsil_mcp::persist::INDEX_FORMAT_VERSION;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::Path;
use tantivy::schema::Value;
use tantivy::{DocSet, Document, Index, TantivyDocument, TERMINATED};

const FINGERPRINTS: &str = "tests/index_format_fingerprints.txt";

fn sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), &target).unwrap();
        }
    }
}

fn sorted_json(v: serde_json::Value) -> serde_json::Value {
    use serde_json::Value;
    match v {
        Value::Array(items) => {
            let mut items: Vec<Value> = items.into_iter().map(sorted_json).collect();
            items.sort_by_cached_key(|i| i.to_string());
            Value::Array(items)
        }
        Value::Object(map) => {
            Value::Object(map.into_iter().map(|(k, v)| (k, sorted_json(v))).collect())
        }
        other => other,
    }
}

fn call_graph_text(snapshot: &[u8]) -> String {
    let graph = CallGraph::new();
    graph.from_snapshot(snapshot, None).unwrap();
    let mut lines: Vec<String> = graph
        .iter_nodes()
        .map(|n| {
            let node = sorted_json(serde_json::to_value(n.value()).unwrap());
            format!("{}\t{node}", n.key())
        })
        .collect();
    lines.sort_unstable();
    for (key, reason) in graph.platform_entries() {
        lines.push(format!("platform\t{key}\t{reason}"));
    }
    lines.join("\n")
}

fn stored_documents(dir: &Path) -> String {
    let index = Index::open_in_dir(dir).unwrap();
    let schema = index.schema();
    let searcher = index.reader().unwrap().searcher();
    let mut docs = Vec::new();
    for segment in searcher.segment_readers() {
        let store = segment.get_store_reader(64).unwrap();
        for doc in 0..segment.max_doc() {
            if segment
                .alive_bitset()
                .is_none_or(|alive| alive.is_alive(doc))
            {
                let stored: TantivyDocument = store.get(doc).unwrap();
                docs.push(stored.to_json(&schema));
            }
        }
    }
    docs.sort_unstable();
    docs.join("\n")
}

fn trigram_pairs(dir: &Path) -> String {
    let index = Index::open_in_dir(dir).unwrap();
    let schema = index.schema();
    let (grams, path) = (
        schema.get_field("grams").unwrap(),
        schema.get_field("path").unwrap(),
    );
    let searcher = index.reader().unwrap().searcher();
    let mut pairs = Vec::new();
    for segment in searcher.segment_readers() {
        let store = segment.get_store_reader(64).unwrap();
        let paths: Vec<String> = (0..segment.max_doc())
            .map(|d| {
                let stored: TantivyDocument = store.get(d).unwrap();
                stored
                    .get_first(path)
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string()
            })
            .collect();
        let inverted = segment.inverted_index(grams).unwrap();
        let mut terms = inverted.terms().stream().unwrap();
        while terms.advance() {
            let mut postings = inverted
                .read_postings_from_terminfo(
                    terms.value(),
                    tantivy::schema::IndexRecordOption::Basic,
                )
                .unwrap();
            let mut doc = postings.doc();
            while doc != TERMINATED {
                if segment
                    .alive_bitset()
                    .is_none_or(|alive| alive.is_alive(doc))
                {
                    pairs.push(format!(
                        "{}\t{}",
                        paths[doc as usize],
                        String::from_utf8_lossy(terms.key())
                    ));
                }
                doc = postings.advance();
            }
        }
    }
    pairs.sort_unstable();
    pairs.join("\n")
}

async fn fingerprints() -> BTreeMap<&'static str, String> {
    let work = tempfile::tempdir().unwrap();
    let repo = work.path().join("fixtures");
    copy_dir(Path::new("test-fixtures"), &repo);
    let index_dir = work.path().join("index");
    #[allow(unused_mut)]
    let mut options = EngineOptions {
        git_enabled: false,
        call_graph_enabled: true,
        persist_enabled: true,
        watch_enabled: false,
        ..Default::default()
    };
    #[cfg(feature = "graph")]
    {
        options.graph_enabled = true;
        options.graph_path = Some(index_dir.join("graph"));
    }
    let engine = CodeIntelEngine::with_options(index_dir.clone(), vec![repo.clone()], options)
        .await
        .unwrap();
    engine.complete_initialization().await.unwrap();

    let root = work.path().to_string_lossy().to_string();
    let hash = |text: String| sha256(text.replace(&root, "<root>").as_bytes());
    let mut out = BTreeMap::new();
    out.insert(
        "symbols",
        hash(format!("{:?}", engine.repo_symbols("fixtures").unwrap())),
    );
    out.insert(
        "call_graph",
        hash(call_graph_text(
            &engine.call_graph_snapshot("fixtures").unwrap(),
        )),
    );
    out.insert(
        "normalized",
        hash(format!(
            "{:?}",
            engine.repo_normalized_docs("fixtures").unwrap_or_default()
        )),
    );
    out.insert("search", hash(stored_documents(&index_dir.join("search"))));
    out.insert(
        "substring",
        hash(trigram_pairs(&index_dir.join("substring"))),
    );
    #[cfg(feature = "graph")]
    {
        let graph = engine.knowledge_graph().unwrap();
        let mut quads: Vec<String> = graph
            .export_nquads()
            .unwrap()
            .lines()
            .map(str::to_owned)
            .collect();
        quads.sort_unstable();
        out.insert("knowledge_graph", hash(quads.join("\n")));
    }
    out
}

/// Each component's newest line, after checking that a component's versions
/// only grow and that the newest version in the file is the current one.
fn recorded() -> BTreeMap<String, (u32, String)> {
    let text = std::fs::read_to_string(FINGERPRINTS).unwrap();
    let mut newest: BTreeMap<String, (u32, String)> = BTreeMap::new();
    let mut max_version = 0;
    for line in text
        .lines()
        .filter(|l| !l.starts_with('#') && !l.trim().is_empty())
    {
        let [version, component, hash] = line.split_whitespace().collect::<Vec<_>>()[..] else {
            panic!("{FINGERPRINTS}: malformed line {line:?}");
        };
        let version: u32 = version.parse().unwrap();
        if let Some((previous, _)) = newest.get(component) {
            assert!(
                version > *previous,
                "{FINGERPRINTS}: a new {component} fingerprint needs a version above {previous}"
            );
        }
        max_version = max_version.max(version);
        newest.insert(component.to_string(), (version, hash.to_string()));
    }
    assert_eq!(
        max_version, INDEX_FORMAT_VERSION,
        "{FINGERPRINTS} and INDEX_FORMAT_VERSION disagree on the newest version"
    );
    newest
}

#[tokio::test]
async fn index_output_matches_its_format_version() {
    let recorded = recorded();
    let actual = fingerprints().await;
    let changed: Vec<String> = actual
        .iter()
        .filter(|(component, hash)| {
            recorded
                .get(**component)
                .is_none_or(|(_, recorded)| recorded != *hash)
        })
        .map(|(component, hash)| format!("{} {component} {hash}", INDEX_FORMAT_VERSION + 1))
        .collect();
    assert!(
        changed.is_empty(),
        "Indexing output changed. Bump INDEX_FORMAT_VERSION in src/persist.rs to {} \
         and append to {FINGERPRINTS}:\n{}",
        INDEX_FORMAT_VERSION + 1,
        changed.join("\n")
    );
}
