//! Resolution quality over a real corpus, not a fixture.
//!
//! Ignored by default and driven by `NARSIL_GO_CORPUS=<path to a repo>`;
//! `NARSIL_CORPUS_EXT` picks the files (default `go`), and `NARSIL_EDGES_OUT`
//! writes every edge as TSV, so two builds can be compared edge by edge:
//!
//! ```text
//! NARSIL_GO_CORPUS=~/src/some-go-service \
//!     cargo test --release --test callgraph_corpus_measure -- --ignored --nocapture
//! NARSIL_GO_CORPUS=src NARSIL_CORPUS_EXT=rs NARSIL_EDGES_OUT=/tmp/edges.tsv \
//!     cargo test --release --test callgraph_corpus_measure -- --ignored --nocapture
//! ```
//!
//! Prints the mix of resolutions and, for the edges that are still a guess,
//! what shape their call site had. Fixtures prove one shape resolves; this is
//! what says whether the shapes that dominate real code do — and it is how
//! every figure in `docs/callgraph-resolution-plan.md` was obtained.

use narsil_mcp::callgraph::CallGraph;
use narsil_mcp::parser::LanguageParser;
use std::collections::HashMap;
use std::path::Path;

fn load(
    root: &str,
    ext: &str,
) -> (
    CallGraph,
    HashMap<String, Vec<String>>,
    usize,
    std::time::Duration,
) {
    let parser = LanguageParser::new().unwrap();
    let cg = CallGraph::new();
    let mut sources: HashMap<String, Vec<String>> = HashMap::new();
    let mut files = Vec::new();
    let mut stack = vec![std::path::PathBuf::from(root)];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).unwrap().flatten() {
            let path = entry.path();
            if path.is_dir() {
                let name = path.file_name().unwrap().to_string_lossy().to_string();
                if name != ".git" && name != "vendor" {
                    stack.push(path);
                }
            } else if path.extension().map(|e| e == ext).unwrap_or(false) {
                if let Ok(content) = std::fs::read_to_string(&path) {
                    let rel = path
                        .strip_prefix(root)
                        .unwrap()
                        .to_string_lossy()
                        .to_string();
                    if let Ok(tree) = parser.parse_to_tree(Path::new(&rel), &content) {
                        sources.insert(
                            rel.clone(),
                            content.lines().map(|l| l.to_string()).collect(),
                        );
                        files.push((rel, content, tree));
                    }
                }
            }
        }
    }
    let n = files.len();
    let started = std::time::Instant::now();
    cg.build_from_files(&files).unwrap();
    let elapsed = started.elapsed();
    (cg, sources, n, elapsed)
}

#[test]
#[ignore]
fn measure() {
    let root = std::env::var("NARSIL_GO_CORPUS").expect("NARSIL_GO_CORPUS");
    let ext = std::env::var("NARSIL_CORPUS_EXT").unwrap_or_else(|_| "go".to_string());
    let (cg, sources, n, elapsed) = load(&root, &ext);
    if let Ok(out) = std::env::var("NARSIL_EDGES_OUT") {
        let mut rows = Vec::new();
        for node in cg.iter_nodes() {
            for e in &node.calls {
                let line = sources
                    .get(&e.file_path)
                    .and_then(|lines| lines.get(e.line.saturating_sub(1)))
                    .map(|l| l.trim().replace('\t', " "))
                    .unwrap_or_default();
                rows.push(format!(
                    "{}:{}\t{}\t{:?}\t{:?}\t{}\t{}\t{}",
                    e.file_path,
                    e.line,
                    e.column,
                    e.call_type,
                    e.resolution,
                    e.target,
                    e.scope_hint.as_deref().unwrap_or(""),
                    line
                ));
            }
        }
        rows.sort();
        std::fs::write(&out, rows.join("\n") + "\n").unwrap();
    }
    println!("files {n}, nodes {}, built in {elapsed:?}", cg.node_count());

    let mut totals: HashMap<String, usize> = HashMap::new();
    let mut classes: HashMap<&str, usize> = HashMap::new();
    let mut samples: HashMap<&str, Vec<String>> = HashMap::new();
    for node in cg.iter_nodes() {
        for e in &node.calls {
            let res = format!("{:?}", e.resolution);
            *totals.entry(res.clone()).or_default() += 1;
            if res != "NameOnly" {
                continue;
            }
            let line = sources
                .get(&e.file_path)
                .and_then(|lines| lines.get(e.line.saturating_sub(1)))
                .map(|l| l.trim().to_string())
                .unwrap_or_default();
            let hint = e.scope_hint.as_deref().unwrap_or("");
            let class = if hint.is_empty() {
                if line.contains("self.") || line.contains("this.") {
                    "no hint: self-chain / embedded"
                } else {
                    "no hint: other"
                }
            } else if line.contains(&format!("self.{}.", hint)) {
                "hint is a struct field"
            } else if hint
                .chars()
                .next()
                .map(|c| c.is_uppercase())
                .unwrap_or(false)
            {
                "Capitalised hint (embedding)"
            } else {
                "lowercase hint: variable of unread type"
            };
            *classes.entry(class).or_default() += 1;
            let bucket = samples.entry(class).or_default();
            if bucket.len() < 5 {
                bucket.push(format!("{}:{} [{hint}] {line}", e.file_path, e.line));
            }
        }
    }
    let mut totals: Vec<_> = totals.into_iter().collect();
    totals.sort_by_key(|(_, n)| std::cmp::Reverse(*n));
    println!("RESOLUTIONS:");
    for (k, v) in &totals {
        println!("  {v:>6}  {k}");
    }
    let mut classes: Vec<_> = classes.into_iter().collect();
    classes.sort_by_key(|(_, n)| std::cmp::Reverse(*n));
    println!("REMAINING NAME-ONLY:");
    for (c, v) in &classes {
        println!("  {v:>6}  {c}");
        for s in samples.get(*c).into_iter().flatten() {
            println!("            {s}");
        }
    }
}
