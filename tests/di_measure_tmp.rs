use narsil_mcp::callgraph::CallGraph;
use narsil_mcp::parser::LanguageParser;
use std::collections::HashMap;
use std::path::Path;

fn load(
    root: &str,
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
            } else if path.extension().map(|e| e == "go").unwrap_or(false) {
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
    let (cg, sources, n, elapsed) = load(&root);
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
