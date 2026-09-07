use narsil_mcp::callgraph::{CallGraph, CallResolution};
use narsil_mcp::parser::LanguageParser;
use std::path::Path;

#[test]
fn test_rust_call_graph_simple() {
    let parser = LanguageParser::new().unwrap();
    let call_graph = CallGraph::new();

    // Simple Rust file with function calls
    let rust_code = r#"
fn main() {
    println!("Hello");
    helper();
}

fn helper() {
    worker();
}

fn worker() {
    println!("Working");
}
"#;

    // Parse the file
    let tree = parser
        .parse_to_tree(Path::new("test.rs"), rust_code)
        .unwrap();

    // Build call graph
    let files = vec![("test.rs".to_string(), rust_code.to_string(), tree)];

    call_graph.build_from_files(&files).unwrap();

    // Verify call graph structure (targets are now qualified keys: "file::name")
    let main_callees = call_graph.get_callees("main");
    assert_eq!(main_callees.len(), 1);
    assert!(
        main_callees[0].target.ends_with("::helper"),
        "expected target ending with ::helper, got: {}",
        main_callees[0].target
    );

    let helper_callees = call_graph.get_callees("helper");
    assert_eq!(helper_callees.len(), 1);
    assert!(
        helper_callees[0].target.ends_with("::worker"),
        "expected target ending with ::worker, got: {}",
        helper_callees[0].target
    );

    let worker_callers = call_graph.get_callers("worker");
    assert_eq!(worker_callers.len(), 1);
    assert!(
        worker_callers[0].target.ends_with("::helper"),
        "expected target ending with ::helper, got: {}",
        worker_callers[0].target
    );
}

#[test]
fn test_self_recursive_call_does_not_deadlock() {
    let parser = LanguageParser::new().unwrap();
    let call_graph = CallGraph::new();

    // A function that calls itself: caller and callee resolve to the same
    // qualified key, so both live in the same DashMap shard. The builder must
    // not take a write lock on the caller's shard and then try to take a read
    // lock on the same shard (a read-under-write self-deadlock).
    let rust_code = r#"
fn recurse(n: u32) -> u32 {
    if n == 0 { 0 } else { recurse(n - 1) }
}
"#;

    let tree = parser
        .parse_to_tree(Path::new("rec.rs"), rust_code)
        .unwrap();

    let files = vec![("rec.rs".to_string(), rust_code.to_string(), tree)];
    call_graph.build_from_files(&files).unwrap();

    let callees = call_graph.get_callees("recurse");
    assert_eq!(callees.len(), 1);
    assert!(callees[0].target.ends_with("::recurse"));
}

#[test]
fn test_python_call_graph() {
    let parser = LanguageParser::new().unwrap();
    let call_graph = CallGraph::new();

    let python_code = r#"
def main():
    print("Hello")
    helper()

def helper():
    worker()

def worker():
    print("Working")
"#;

    let tree = parser
        .parse_to_tree(Path::new("test.py"), python_code)
        .unwrap();

    let files = vec![("test.py".to_string(), python_code.to_string(), tree)];

    call_graph.build_from_files(&files).unwrap();

    // Verify call edges - main calls helper (and also print, which is detected)
    let main_callees = call_graph.get_callees("main");
    assert!(
        !main_callees.is_empty(),
        "main should have at least one callee"
    );
    let calls_helper = main_callees.iter().any(|e| e.target.ends_with("::helper"));
    assert!(
        calls_helper,
        "main should call helper, got: {:?}",
        main_callees.iter().map(|e| &e.target).collect::<Vec<_>>()
    );
}

#[test]
fn test_javascript_call_graph() {
    let parser = LanguageParser::new().unwrap();
    let call_graph = CallGraph::new();

    let js_code = r#"
function main() {
    console.log("Hello");
    helper();
}

function helper() {
    worker();
}

function worker() {
    console.log("Working");
}
"#;

    let tree = parser.parse_to_tree(Path::new("test.js"), js_code).unwrap();

    let files = vec![("test.js".to_string(), js_code.to_string(), tree)];

    call_graph.build_from_files(&files).unwrap();

    // Verify call graph - main calls helper (and also console.log, which is detected)
    let main_callees = call_graph.get_callees("main");
    assert!(
        !main_callees.is_empty(),
        "main should have at least one callee"
    );
    let calls_helper = main_callees.iter().any(|e| e.target.ends_with("::helper"));
    assert!(
        calls_helper,
        "main should call helper, got: {:?}",
        main_callees.iter().map(|e| &e.target).collect::<Vec<_>>()
    );

    // Test transitive callees
    let transitive = call_graph.get_transitive_callees("main", 10);
    assert!(transitive.len() >= 2); // Should find helper and worker
}

#[test]
fn test_cross_file_calls() {
    let parser = LanguageParser::new().unwrap();
    let call_graph = CallGraph::new();

    // File 1: main.rs
    let file1 = r#"
mod utils;

fn main() {
    utils::helper();
}
"#;

    // File 2: utils.rs
    let file2 = r#"
pub fn helper() {
    internal_worker();
}

fn internal_worker() {
    println!("Working");
}
"#;

    let tree1 = parser.parse_to_tree(Path::new("main.rs"), file1).unwrap();
    let tree2 = parser.parse_to_tree(Path::new("utils.rs"), file2).unwrap();

    let files = vec![
        ("main.rs".to_string(), file1.to_string(), tree1),
        ("utils.rs".to_string(), file2.to_string(), tree2),
    ];

    call_graph.build_from_files(&files).unwrap();

    // Verify helper is called
    let helper_callers = call_graph.get_callers("helper");
    assert!(!helper_callers.is_empty());
}

#[test]
fn test_chunked_two_pass_equivalent_to_monolithic() {
    let parser = LanguageParser::new().unwrap();

    // Cross-file call: main.rs -> utils.rs::helper
    let file1 = r#"
mod utils;

fn main() {
    utils::helper();
}
"#;
    let file2 = r#"
pub fn helper() {
    internal_worker();
}

fn internal_worker() {
    println!("Working");
}
"#;

    let tree1 = parser.parse_to_tree(Path::new("main.rs"), file1).unwrap();
    let tree2 = parser.parse_to_tree(Path::new("utils.rs"), file2).unwrap();

    // Monolithic: one two-pass build over both files.
    let monolithic = CallGraph::new();
    monolithic
        .build_from_files(&[
            ("main.rs".to_string(), file1.to_string(), tree1.clone()),
            ("utils.rs".to_string(), file2.to_string(), tree2.clone()),
        ])
        .unwrap();

    // Chunked streaming: functions for every chunk first, then calls for every
    // chunk — the exact shape index_repo uses. Same trees, split into windows.
    let chunked = CallGraph::new();
    chunked
        .collect_functions(&[("main.rs".to_string(), file1.to_string(), tree1.clone())])
        .unwrap();
    chunked
        .collect_functions(&[("utils.rs".to_string(), file2.to_string(), tree2.clone())])
        .unwrap();
    chunked
        .collect_calls(&[("main.rs".to_string(), file1.to_string(), tree1.clone())])
        .unwrap();
    chunked
        .collect_calls(&[("utils.rs".to_string(), file2.to_string(), tree2.clone())])
        .unwrap();

    // Cross-file resolution must be identical between the two paths.
    let mono_callers = monolithic.get_callers("helper");
    let chunked_callers = chunked.get_callers("helper");
    assert!(
        !mono_callers.is_empty(),
        "monolithic should resolve the cross-file call"
    );
    assert_eq!(chunked_callers.len(), mono_callers.len());
    for (a, b) in chunked_callers.iter().zip(mono_callers.iter()) {
        assert_eq!(a.target, b.target);
    }
}

#[test]
fn test_cross_file_scoped_call_resolution() {
    let parser = LanguageParser::new().unwrap();
    let call_graph = CallGraph::new();

    // File 1: main.rs - calls App::run()
    let main_code = r#"
fn main() {
    App::run();
}
"#;

    // File 2: src/app/mod.rs - defines run()
    let app_code = r#"
pub fn run() {
    println!("app running");
}
"#;

    // File 3: src/agents/mod.rs - also defines run()
    let agents_code = r#"
pub fn run() {
    println!("agents running");
}
"#;

    let tree1 = parser
        .parse_to_tree(Path::new("main.rs"), main_code)
        .unwrap();
    let tree2 = parser
        .parse_to_tree(Path::new("src/app/mod.rs"), app_code)
        .unwrap();
    let tree3 = parser
        .parse_to_tree(Path::new("src/agents/mod.rs"), agents_code)
        .unwrap();

    let files = vec![
        ("main.rs".to_string(), main_code.to_string(), tree1),
        ("src/app/mod.rs".to_string(), app_code.to_string(), tree2),
        (
            "src/agents/mod.rs".to_string(),
            agents_code.to_string(),
            tree3,
        ),
    ];

    call_graph.build_from_files(&files).unwrap();

    // App::run() in main should resolve to src/app/mod.rs::run, not agents
    let main_callees = call_graph.get_callees("main");
    assert!(
        !main_callees.is_empty(),
        "main should have at least one callee"
    );

    let run_call = main_callees.iter().find(|e| e.target.ends_with("::run"));
    assert!(run_call.is_some(), "main should call some ::run function");
    assert_eq!(
        run_call.unwrap().target,
        "src/app/mod.rs::run",
        "App::run() should resolve to src/app/mod.rs::run via scope hint"
    );
}

#[test]
fn test_extract_scope_from_scoped_call() {
    let parser = LanguageParser::new().unwrap();
    let call_graph = CallGraph::new();

    // Code with a scoped call
    let code = r#"
fn caller() {
    utils::helper();
}

fn helper() {
    println!("local helper");
}
"#;

    let tree = parser.parse_to_tree(Path::new("test.rs"), code).unwrap();
    let files = vec![("test.rs".to_string(), code.to_string(), tree)];

    call_graph.build_from_files(&files).unwrap();

    // The call to utils::helper() should have scope_hint populated
    let caller_callees = call_graph.get_callees("caller");
    assert!(
        !caller_callees.is_empty(),
        "caller should have at least one callee"
    );

    // Find the edge targeting helper
    let helper_edge = caller_callees
        .iter()
        .find(|e| e.target.ends_with("::helper") || e.target == "helper");
    assert!(
        helper_edge.is_some(),
        "caller should call helper, got: {:?}",
        caller_callees.iter().map(|e| &e.target).collect::<Vec<_>>()
    );

    let edge = helper_edge.unwrap();
    assert_eq!(
        edge.scope_hint,
        Some("utils".to_string()),
        "scope_hint should capture 'utils' from utils::helper()"
    );
}

#[test]
fn test_find_call_path_deterministic_with_ambiguous_names() {
    let parser = LanguageParser::new().unwrap();
    let call_graph = CallGraph::new();

    // main calls App::start(), two modules define start()
    let main_code = r#"
fn main() {
    App::start();
}
"#;

    let app_code = r#"
pub fn start() {
    work();
}

fn work() {
    println!("working");
}
"#;

    let other_code = r#"
pub fn start() {
    println!("other start");
}
"#;

    let tree1 = parser
        .parse_to_tree(Path::new("main.rs"), main_code)
        .unwrap();
    let tree2 = parser
        .parse_to_tree(Path::new("src/app/mod.rs"), app_code)
        .unwrap();
    let tree3 = parser
        .parse_to_tree(Path::new("src/other/mod.rs"), other_code)
        .unwrap();

    let files = vec![
        ("main.rs".to_string(), main_code.to_string(), tree1),
        ("src/app/mod.rs".to_string(), app_code.to_string(), tree2),
        (
            "src/other/mod.rs".to_string(),
            other_code.to_string(),
            tree3,
        ),
    ];

    call_graph.build_from_files(&files).unwrap();

    // find_call_path should give consistent results
    let path1 = call_graph.find_call_path("main", "work");
    let path2 = call_graph.find_call_path("main", "work");
    assert_eq!(path1, path2, "find_call_path must be deterministic");

    // Should find the path: main -> src/app/mod.rs::start -> src/app/mod.rs::work
    assert!(path1.is_some(), "Should find a path from main to work");
    let path = path1.unwrap();
    assert!(
        path.iter()
            .any(|p| p.contains("app") && p.contains("start")),
        "Path should go through app::start, got: {:?}",
        path
    );
}

#[test]
fn test_bsl_common_module_call_resolution() {
    let parser = LanguageParser::new().unwrap();
    let call_graph = CallGraph::new();

    // Common module exports a function.
    let module_code = r#"
Функция Метод(Параметр) Экспорт
    Возврат Параметр;
КонецФункции
"#;
    // A document module calls it via the qualified `ОбщийМодуль.Метод()` form.
    let doc_code = r#"
Процедура ОбработкаПроведения(Отказ) Экспорт
    Результат = ОбщийМодуль.Метод(1);
КонецПроцедуры
"#;

    let module_tree = parser
        .parse_to_tree(
            Path::new("CommonModules/ОбщийМодуль/Ext/Module.bsl"),
            module_code,
        )
        .unwrap();
    let doc_tree = parser
        .parse_to_tree(Path::new("Documents/Заказ/Ext/ObjectModule.bsl"), doc_code)
        .unwrap();

    let files = vec![
        (
            "CommonModules/ОбщийМодуль/Ext/Module.bsl".to_string(),
            module_code.to_string(),
            module_tree,
        ),
        (
            "Documents/Заказ/Ext/ObjectModule.bsl".to_string(),
            doc_code.to_string(),
            doc_tree,
        ),
    ];

    call_graph.build_from_files(&files).unwrap();

    // The exported procedure resolves its cross-module caller.
    let callers = call_graph.get_callers("Метод");
    assert!(!callers.is_empty(), "Метод should have callers");
    assert!(
        callers
            .iter()
            .any(|e| e.target.ends_with("::ОбработкаПроведения")),
        "caller should be ОбработкаПроведения, got: {:?}",
        callers.iter().map(|e| &e.target).collect::<Vec<_>>()
    );
    assert!(
        callers.iter().all(|e| e.resolved),
        "cross-module call should resolve"
    );
}

/// Build a one-file call graph, so the per-language method tests stay readable.
fn graph_of(path: &str, code: &str) -> CallGraph {
    let parser = LanguageParser::new().unwrap();
    let call_graph = CallGraph::new();
    let tree = parser.parse_to_tree(Path::new(path), code).unwrap();
    let files = vec![(path.to_string(), code.to_string(), tree)];
    call_graph.build_from_files(&files).unwrap();
    call_graph
}

fn caller_names(call_graph: &CallGraph, function: &str) -> Vec<String> {
    call_graph
        .get_callers(function)
        .iter()
        .map(|edge| edge.target.clone())
        .collect()
}

#[test]
fn test_go_method_calls_are_edges() {
    // `s.helper()` is a `selector_expression` callee; before it was handled,
    // every Go method call and every `pkg.Func()` call was silently dropped.
    let call_graph = graph_of(
        "main.go",
        r#"
package main

type Server struct{}

func (s *Server) Handle() {
	s.helper()
	plain()
}

func (s *Server) helper() {}

func plain() {}

func main() {
	s := &Server{}
	s.Handle()
}
"#,
    );

    assert_eq!(
        caller_names(&call_graph, "helper"),
        vec!["main.go::Server::Handle"],
        "s.helper() should be an edge from Handle"
    );
    assert_eq!(
        caller_names(&call_graph, "Handle"),
        vec!["main.go::main"],
        "s.Handle() should be an edge from main"
    );
    assert_eq!(
        caller_names(&call_graph, "plain"),
        vec!["main.go::Server::Handle"],
        "plain function calls should keep working"
    );
}

#[test]
fn test_python_method_calls_are_edges() {
    // Python spells the callee as `attribute`, which used to fall through.
    let call_graph = graph_of(
        "app.py",
        r#"
class Service:
    def run(self):
        self.step()

    def step(self):
        pass

def main():
    Service().run()
"#,
    );

    assert_eq!(
        caller_names(&call_graph, "step"),
        vec!["app.py::Service::run"]
    );
    assert_eq!(caller_names(&call_graph, "run"), vec!["app.py::main"]);
}

#[test]
fn test_javascript_method_calls_are_edges() {
    // JS names the method `property_identifier`; the old last-identifier walk
    // returned the receiver (`a`) instead of the method (`run`).
    let call_graph = graph_of(
        "app.js",
        r#"
class Service {
  run() {
    this.step();
  }
  step() {}
}

function main() {
  const a = new Service();
  a.run();
}
"#,
    );

    assert_eq!(
        caller_names(&call_graph, "step"),
        vec!["app.js::Service::run"]
    );
    assert_eq!(caller_names(&call_graph, "run"), vec!["app.js::main"]);
}

#[test]
fn test_java_method_invocations_are_edges() {
    // Java calls are `method_invocation`, a node kind the walker did not treat
    // as a call at all.
    let call_graph = graph_of(
        "App.java",
        r#"
class Service {
    void run() {
        this.step();
    }

    void step() {}

    void main() {
        Service a = new Service();
        a.run();
    }
}
"#,
    );

    assert_eq!(
        caller_names(&call_graph, "step"),
        vec!["App.java::Service::run"]
    );
    assert_eq!(
        caller_names(&call_graph, "run"),
        vec!["App.java::Service::main"]
    );
}

#[test]
fn test_method_call_carries_receiver_as_scope_hint() {
    // A receiver that names a package/module narrows cross-file resolution;
    // `self`/`this` receivers must not become scope hints.
    let parser = LanguageParser::new().unwrap();
    let call_graph = CallGraph::new();

    let store_code = "package store\n\nfunc Get() {}\n";
    let api_code = "package api\n\nfunc Handle() {\n\tstore.Get()\n}\n";
    let other_code = "package other\n\nfunc Get() {}\n";

    let files = vec![
        ("internal/store/store.go", store_code),
        ("internal/api/api.go", api_code),
        ("internal/other/other.go", other_code),
    ]
    .into_iter()
    .map(|(path, code)| {
        let tree = parser.parse_to_tree(Path::new(path), code).unwrap();
        (path.to_string(), code.to_string(), tree)
    })
    .collect::<Vec<_>>();

    call_graph.build_from_files(&files).unwrap();

    let callers = call_graph.get_callers("internal/store/store.go::Get");
    assert_eq!(
        callers.iter().map(|e| e.target.clone()).collect::<Vec<_>>(),
        vec!["internal/api/api.go::Handle"],
        "store.Get() should resolve to the store package, not to other"
    );
}

#[test]
fn test_find_function_accepts_qualified_method_names() {
    let call_graph = graph_of(
        "main.go",
        r#"
package main

type Server struct{}

func (s *Server) Handle() {}

func main() {
	s := &Server{}
	s.Handle()
}
"#,
    );

    // `Type.Method` / `Type::method` are how people spell methods; the graph
    // keys on `file::name`, so these have to be understood as queries.
    for query in [
        "Server.Handle",
        "Server::Handle",
        "main.go::Server::Handle",
        "main.go::Handle",
    ] {
        assert_eq!(
            call_graph.find_function(query).as_deref(),
            Some("main.go::Server::Handle"),
            "query {query} should resolve"
        );
        assert_eq!(
            caller_names(&call_graph, query),
            vec!["main.go::main"],
            "query {query} should report callers"
        );
    }

    assert_eq!(call_graph.find_function("Server.NoSuchThing"), None);
    assert_eq!(CallGraph::bare_name("Server.Handle"), "Handle");
    assert_eq!(CallGraph::bare_name("Handle"), "Handle");
}

#[test]
fn test_same_named_methods_on_different_types_stay_distinct() {
    // The node key used to be `file::name`, so the second `Handle` overwrote
    // the first and one method vanished from the graph entirely.
    let call_graph = graph_of(
        "main.go",
        r#"
package main

type Server struct{}
type Client struct{}

func (s *Server) Handle() {
	serverWork()
}

func (c Client) Handle() {
	clientWork()
}

func serverWork() {}
func clientWork() {}
"#,
    );

    let mut keys = call_graph.get_all_function_names();
    keys.sort();
    assert!(
        keys.contains(&"main.go::Server::Handle".to_string())
            && keys.contains(&"main.go::Client::Handle".to_string()),
        "both methods should have their own node, got: {keys:?}"
    );

    assert_eq!(
        caller_names(&call_graph, "serverWork"),
        vec!["main.go::Server::Handle"]
    );
    assert_eq!(
        caller_names(&call_graph, "clientWork"),
        vec!["main.go::Client::Handle"]
    );
    assert_eq!(
        call_graph.find_function("Client.Handle").as_deref(),
        Some("main.go::Client::Handle"),
        "the type in the query picks the method apart from its namesake"
    );
}

#[test]
fn test_receiver_type_is_recorded_per_language() {
    let cases = [
        (
            "a.go",
            "package a\ntype Service struct{}\nfunc (s *Service) run() {}\nfunc free() {}\n",
        ),
        (
            "a.rs",
            "struct Service;\nimpl Service { fn run(&self) {} }\nfn free() {}\n",
        ),
        (
            "a.py",
            "class Service:\n    def run(self):\n        pass\n\ndef free():\n    pass\n",
        ),
        ("a.js", "class Service { run() {} }\nfunction free() {}\n"),
        ("A.java", "class Service { void run() {} }\n"),
        (
            "a.rb",
            "class Service\n  def run\n  end\nend\ndef free\nend\n",
        ),
    ];

    for (path, code) in cases {
        let call_graph = graph_of(path, code);
        let method = call_graph
            .get_node(&format!("{path}::Service::run"))
            .map(|node| node.receiver.clone());
        assert_eq!(
            method,
            Some(Some("Service".to_string())),
            "{path}: the method should be keyed and tagged with its type"
        );

        // A module-level function must not inherit the class it follows —
        // Python's root node is called `module`, like Ruby's `module` keyword.
        let free = call_graph
            .get_node(&format!("{path}::free"))
            .map(|node| node.receiver.clone());
        if let Some(receiver) = free {
            assert_eq!(receiver, None, "{path}: free function has no receiver");
        }
    }
}

#[test]
fn test_static_call_resolves_by_receiver_across_files() {
    let parser = LanguageParser::new().unwrap();
    let call_graph = CallGraph::new();

    // `a_other.rs` sorts first, so the alphabetical fallback would pick the
    // wrong `new`; only the receiver type points at `Store`.
    let files = vec![
        (
            "a_other.rs",
            "pub struct Other;\nimpl Other { pub fn new() -> Self { Other } }\n",
        ),
        (
            "z_store.rs",
            "pub struct Store;\nimpl Store { pub fn new() -> Self { Store } }\n",
        ),
        ("main.rs", "fn main() { let s = Store::new(); }\n"),
    ]
    .into_iter()
    .map(|(path, code)| {
        let tree = parser.parse_to_tree(Path::new(path), code).unwrap();
        (path.to_string(), code.to_string(), tree)
    })
    .collect::<Vec<_>>();

    call_graph.build_from_files(&files).unwrap();

    let callees: Vec<String> = call_graph
        .get_callees("main.rs::main")
        .iter()
        .map(|edge| edge.target.clone())
        .collect();
    assert_eq!(callees, vec!["z_store.rs::Store::new"]);
}

#[test]
fn test_edges_record_what_they_were_matched_on() {
    let parser = LanguageParser::new().unwrap();
    let call_graph = CallGraph::new();

    let files = vec![
        (
            "a_store.rs",
            "pub struct Store;\nimpl Store { pub fn new() -> Self { Store } pub fn only_here(&self) {} }\n",
        ),
        (
            "z_other.rs",
            "pub struct Other;\nimpl Other { pub fn new() -> Self { Other } }\n",
        ),
        (
            "main.rs",
            r#"
fn main() {
    let s = Store::new();
    s.only_here();
    helper();
    third_party::missing();
}

fn helper() {}
"#,
        ),
    ]
    .into_iter()
    .map(|(path, code)| {
        let tree = parser.parse_to_tree(Path::new(path), code).unwrap();
        (path.to_string(), code.to_string(), tree)
    })
    .collect::<Vec<_>>();

    call_graph.build_from_files(&files).unwrap();

    let by_target: Vec<(String, CallResolution)> = call_graph
        .get_callees("main.rs::main")
        .iter()
        .map(|edge| (edge.target.clone(), edge.resolution))
        .collect();

    let resolution_of = |name: &str| {
        by_target
            .iter()
            .find(|(target, _)| target.ends_with(name))
            .map(|(_, resolution)| *resolution)
    };

    assert_eq!(
        resolution_of("Store::new"),
        Some(CallResolution::Receiver),
        "the qualifier named the receiver type: {by_target:?}"
    );
    assert_eq!(
        resolution_of("Store::only_here"),
        Some(CallResolution::Unique),
        "only one function carries this name: {by_target:?}"
    );
    assert_eq!(
        resolution_of("::helper"),
        Some(CallResolution::Unique),
        "{by_target:?}"
    );
    assert_eq!(
        resolution_of("missing"),
        Some(CallResolution::Unresolved),
        "a call outside the graph is not a match: {by_target:?}"
    );

    assert!(
        CallResolution::Receiver.is_certain() && CallResolution::Unique.is_certain(),
        "receiver and unique matches need no caveat"
    );
    assert_eq!(
        CallResolution::NameOnly.caveat(),
        Some("name match only"),
        "a name match must be labelled in reports"
    );
}

#[test]
fn test_name_only_match_is_flagged() {
    let parser = LanguageParser::new().unwrap();
    let call_graph = CallGraph::new();

    // Two unrelated types with a `run` method, and a caller whose receiver
    // type is unknowable without type inference.
    let files = vec![
        (
            "a_first.rs",
            "pub struct First;\nimpl First { pub fn run(&self) {} }\n",
        ),
        (
            "b_second.rs",
            "pub struct Second;\nimpl Second { pub fn run(&self) {} }\n",
        ),
        (
            "main.rs",
            "fn main() { let thing = pick(); thing.run(); }\nfn pick() {}\n",
        ),
    ]
    .into_iter()
    .map(|(path, code)| {
        let tree = parser.parse_to_tree(Path::new(path), code).unwrap();
        (path.to_string(), code.to_string(), tree)
    })
    .collect::<Vec<_>>();

    call_graph.build_from_files(&files).unwrap();

    let run_edge = call_graph
        .get_callees("main.rs::main")
        .into_iter()
        .find(|edge| edge.target.ends_with("::run"))
        .expect("thing.run() should produce an edge");

    assert_eq!(
        run_edge.resolution,
        CallResolution::NameOnly,
        "nothing distinguished the two `run` methods, so the edge is a guess"
    );
    assert!(!run_edge.resolution.is_certain());
    assert_eq!(run_edge.resolution.caveat(), Some("name match only"));
}

/// The DI-factory shape that dominates Go service code:
/// `self.Depend.XFactory(self.C).Method()`. No receiver type is written at the
/// call site, so before the factory hint the edge landed on whichever namesake
/// sorted first — on a real corpus that was the wrong type most of the time.
#[test]
fn test_go_di_factory_call_resolves_to_the_type_the_factory_names() {
    let parser = LanguageParser::new().unwrap();
    let call_graph = CallGraph::new();

    let files = vec![
        (
            "internal/usecase/system_event/impl/handler/process_payment_operation.go",
            r#"
package handler

type HandlerProcessPaymentOperation struct {
	Depend Dependency
	C      ctx.Ctx
}

func (self *HandlerProcessPaymentOperation) handleSfx(uuid string) error {
	operation, err := self.Depend.SfxOperationRepoFactory(self.C).
		FindByUuid(sfx_model.SfxOperationUuid(uuid))
	if err != nil {
		return err
	}
	return self.Depend.SfxOperationUseFactory(self.C).ProcessNewOperation(operation)
}

func (self *HandlerProcessPaymentOperation) handleAirtime(uuid string) error {
	return self.Depend.AirtimeUseCaseFactory(self.C).ProcessNewOperation(nil)
}
"#,
        ),
        (
            "internal/usecase/sfx/impl/send_to_provider.go",
            r#"
package impl

type SfxOperationUse struct{}

func (self *SfxOperationUse) ProcessNewOperation(operation *sfx_model.SfxOperation) error {
	return nil
}
"#,
        ),
        (
            "internal/usecase/airtime/impl/airtime.go",
            r#"
package impl

type AirtimeUseCase struct{}

func (self *AirtimeUseCase) ProcessNewOperation(operation *airtime_model.AirtimeOperation) error {
	return nil
}
"#,
        ),
        (
            "internal/repository/sfx/impl/sfx_operation.go",
            r#"
package impl

type SfxOperationRepo struct{}

func (self *SfxOperationRepo) FindByUuid(uuid sfx_model.SfxOperationUuid) error {
	return nil
}
"#,
        ),
        (
            "internal/repository/airtime/impl/airtime.go",
            r#"
package impl

type AirtimeOperationRepo struct{}

func (self *AirtimeOperationRepo) FindByUuid(uuid string) error {
	return nil
}
"#,
        ),
    ]
    .into_iter()
    .map(|(path, code)| {
        let tree = parser.parse_to_tree(Path::new(path), code).unwrap();
        (path.to_string(), code.to_string(), tree)
    })
    .collect::<Vec<_>>();

    call_graph.build_from_files(&files).unwrap();

    let sfx_callers = call_graph.get_callers("SfxOperationUse.ProcessNewOperation");
    assert_eq!(sfx_callers.len(), 1, "got {sfx_callers:?}");
    assert!(
        sfx_callers[0]
            .target
            .ends_with("::HandlerProcessPaymentOperation::handleSfx"),
        "got {}",
        sfx_callers[0].target
    );
    assert_eq!(
        sfx_callers[0].resolution,
        CallResolution::FactoryReceiver,
        "the type came from the factory name, not from the call site"
    );
    assert!(
        !sfx_callers[0].resolution.is_certain(),
        "a naming convention is evidence, not proof"
    );

    // The other handler keeps its own namesake: the hint separates them.
    let airtime_callers = call_graph.get_callers("AirtimeUseCase.ProcessNewOperation");
    assert_eq!(airtime_callers.len(), 1, "got {airtime_callers:?}");
    assert!(airtime_callers[0].target.ends_with("::handleAirtime"));

    // Same for the repository call written across two lines.
    let repo_callers = call_graph.get_callers("SfxOperationRepo.FindByUuid");
    assert_eq!(repo_callers.len(), 1, "got {repo_callers:?}");
    assert_eq!(repo_callers[0].resolution, CallResolution::FactoryReceiver);
}

/// `NewType(...).method()` is the same shape without the DI container, and it
/// is what makes a call to a method named `Run` (107 namesakes on a real Go
/// corpus) resolvable at all.
#[test]
fn test_constructor_chained_call_resolves_by_constructed_type() {
    let parser = LanguageParser::new().unwrap();
    let call_graph = CallGraph::new();

    let files = vec![
        (
            "cmd/goscheduler/goscheduler.go",
            "package main\n\nfunc main() {\n\tapp.NewGoSchedulerApplication(cfg).Run()\n}\n",
        ),
        (
            "internal/app/scheduler_app.go",
            "package app\n\ntype GoSchedulerApplication struct{}\n\nfunc (self *GoSchedulerApplication) Run() {}\n",
        ),
        (
            "internal/app/api_app.go",
            "package app\n\ntype ApiApplication struct{}\n\nfunc (self *ApiApplication) Run() {}\n",
        ),
    ]
    .into_iter()
    .map(|(path, code)| {
        let tree = parser.parse_to_tree(Path::new(path), code).unwrap();
        (path.to_string(), code.to_string(), tree)
    })
    .collect::<Vec<_>>();

    call_graph.build_from_files(&files).unwrap();

    let edge = call_graph
        .get_callees("cmd/goscheduler/goscheduler.go::main")
        .into_iter()
        .find(|edge| edge.target.ends_with("::Run"))
        .expect("chained Run() should produce an edge");
    assert!(
        edge.target
            .ends_with("scheduler_app.go::GoSchedulerApplication::Run"),
        "got {}",
        edge.target
    );
    assert_eq!(edge.resolution, CallResolution::FactoryReceiver);
}

/// The hint is only taken when a namesake is actually defined on a type of
/// that name — a chained call whose inner name is not a factory, or names a
/// type nothing matches, must stay the guess it was.
#[test]
fn test_chained_call_without_factory_naming_stays_a_name_match() {
    let parser = LanguageParser::new().unwrap();
    let call_graph = CallGraph::new();

    let files = vec![
        (
            "main.go",
            "package main\n\nfunc main() {\n\tlookup(id).Run()\n}\n\nfunc lookup(id string) {}\n",
        ),
        (
            "first.go",
            "package main\n\ntype First struct{}\n\nfunc (self *First) Run() {}\n",
        ),
        (
            "second.go",
            "package main\n\ntype Second struct{}\n\nfunc (self *Second) Run() {}\n",
        ),
    ]
    .into_iter()
    .map(|(path, code)| {
        let tree = parser.parse_to_tree(Path::new(path), code).unwrap();
        (path.to_string(), code.to_string(), tree)
    })
    .collect::<Vec<_>>();

    call_graph.build_from_files(&files).unwrap();

    let edge = call_graph
        .get_callees("main.go::main")
        .into_iter()
        .find(|edge| edge.target.ends_with("::Run"))
        .expect("lookup(id).Run() should produce an edge");
    assert_eq!(
        edge.resolution,
        CallResolution::NameOnly,
        "`lookup` names no type, so the edge is still a deterministic guess"
    );
}

/// `self.method()` inside a method names no type at the call site, but the
/// enclosing method does. Go spreads the methods of one type across the files
/// of its package, so before this the target was picked among namesakes (or
/// from the caller's own file) rather than on the receiver.
#[test]
fn test_self_call_resolves_on_the_enclosing_type() {
    let parser = LanguageParser::new().unwrap();
    let call_graph = CallGraph::new();

    let files = vec![
        (
            "internal/app/sms_code.go",
            r#"
package app

func (self *BaseApplication) SendCode(code string) error {
	self.Logger().Info(code)
	return nil
}
"#,
        ),
        (
            "internal/app/base_app.go",
            "package app\n\ntype BaseApplication struct{}\n\nfunc (self *BaseApplication) Logger() *log.Logger {\n\treturn nil\n}\n",
        ),
        (
            "internal/deployer/runner.go",
            "package deployer\n\ntype Runner struct{}\n\nfunc (self *Runner) Logger() *log.Logger {\n\treturn nil\n}\n",
        ),
    ]
    .into_iter()
    .map(|(path, code)| {
        let tree = parser.parse_to_tree(Path::new(path), code).unwrap();
        (path.to_string(), code.to_string(), tree)
    })
    .collect::<Vec<_>>();

    call_graph.build_from_files(&files).unwrap();

    let edge = call_graph
        .get_callees("BaseApplication.SendCode")
        .into_iter()
        .find(|edge| edge.target.ends_with("::Logger"))
        .expect("self.Logger() should produce an edge");
    assert!(
        edge.target
            .ends_with("base_app.go::BaseApplication::Logger"),
        "got {}",
        edge.target
    );
    assert_eq!(
        edge.resolution,
        CallResolution::Receiver,
        "the enclosing method's receiver type is read from the source, not guessed"
    );
    assert!(edge.resolution.is_certain());
}

/// A qualifier that names an imported package is neither a receiver nor a
/// variable. Before the import list was read, `assert.Len(t, x, 3)` from
/// testify resolved to whatever `Len` sorted first in this repository.
#[test]
fn test_call_qualified_by_a_third_party_package_is_not_in_the_graph() {
    let parser = LanguageParser::new().unwrap();
    let call_graph = CallGraph::new();

    let files = vec![
        (
            "internal/lib/argon/test/argon_test.go",
            r#"
package test

import (
	"testing"

	"github.com/stretchr/testify/assert"
)

func TestArgon(t *testing.T) {
	assert.Len(t, []int{1}, 1)
}
"#,
        ),
        (
            "internal/config/config.go",
            "package config\n\ntype CountryPhoneCodes struct{}\n\nfunc (self *CountryPhoneCodes) Len() int {\n\treturn 0\n}\n",
        ),
    ]
    .into_iter()
    .map(|(path, code)| {
        let tree = parser.parse_to_tree(Path::new(path), code).unwrap();
        (path.to_string(), code.to_string(), tree)
    })
    .collect::<Vec<_>>();

    call_graph.build_from_files(&files).unwrap();

    let edge = call_graph
        .get_callees("internal/lib/argon/test/argon_test.go::TestArgon")
        .into_iter()
        .find(|edge| edge.target.ends_with("Len"))
        .expect("the call site should still be recorded");
    assert_eq!(
        edge.resolution,
        CallResolution::ExternalPackage,
        "testify is not this repository's code, and the import list says so: got {}",
        edge.target
    );
    assert!(
        call_graph.get_callers("CountryPhoneCodes.Len").is_empty(),
        "an unrelated method must not collect a caller from another package"
    );
}

/// The mirror case: the qualifier names a package of this repository, so the
/// namesakes narrow to that one directory.
#[test]
fn test_call_qualified_by_an_internal_package_resolves_within_it() {
    let parser = LanguageParser::new().unwrap();
    let call_graph = CallGraph::new();

    let files = vec![
        (
            "internal/usecase/sfx/impl/pay.go",
            r#"
package impl

import (
	"go-rest-api/internal/config"
)

func Pay() {
	config.Get()
}
"#,
        ),
        (
            "internal/config/config.go",
            "package config\n\nfunc Get() *Config {\n\treturn nil\n}\n",
        ),
        (
            "internal/usecase/sfx/impl/other.go",
            "package impl\n\nfunc Get() string {\n\treturn \"\"\n}\n",
        ),
    ]
    .into_iter()
    .map(|(path, code)| {
        let tree = parser.parse_to_tree(Path::new(path), code).unwrap();
        (path.to_string(), code.to_string(), tree)
    })
    .collect::<Vec<_>>();

    call_graph.build_from_files(&files).unwrap();

    let edge = call_graph
        .get_callees("internal/usecase/sfx/impl/pay.go::Pay")
        .into_iter()
        .find(|edge| edge.target.ends_with("::Get"))
        .expect("config.Get() should produce an edge");
    assert_eq!(
        edge.target, "internal/config/config.go::Get",
        "the import names the package, so the namesake next door loses"
    );
    assert_eq!(edge.resolution, CallResolution::Scope);
}

/// A call on a local variable or parameter: the type is written in the
/// declaration, so `svc.Create()` need not be a guess among namesakes.
#[test]
fn test_call_on_a_local_variable_resolves_on_its_declared_type() {
    let parser = LanguageParser::new().unwrap();
    let call_graph = CallGraph::new();

    let files = vec![
        (
            "internal/module/payout/handler/create.go",
            r#"
package handler

func handleFromParameter(batchService payout_use.PayoutBatchUse) error {
	return batchService.Create(nil)
}

func handleFromConstructor() error {
	lineRepo := NewPayoutBatchLineRepo(nil)
	return lineRepo.Create(nil)
}
"#,
        ),
        (
            "internal/module/payout/usecase/impl/payout_batch.go",
            "package impl\n\ntype PayoutBatchUse struct{}\n\nfunc (self *PayoutBatchUse) Create(dto *Dto) error {\n\treturn nil\n}\n",
        ),
        (
            "internal/module/payout/model/repository/impl/payout_batch_line.go",
            "package impl\n\ntype PayoutBatchLineRepo struct{}\n\nfunc (self *PayoutBatchLineRepo) Create(dto *Dto) error {\n\treturn nil\n}\n",
        ),
        (
            "internal/model/account/repository/impl/account.go",
            "package impl\n\ntype AccountRepo struct{}\n\nfunc (self *AccountRepo) Create(dto *Dto) error {\n\treturn nil\n}\n",
        ),
    ]
    .into_iter()
    .map(|(path, code)| {
        let tree = parser.parse_to_tree(Path::new(path), code).unwrap();
        (path.to_string(), code.to_string(), tree)
    })
    .collect::<Vec<_>>();

    call_graph.build_from_files(&files).unwrap();

    let from_param = call_graph
        .get_callees("internal/module/payout/handler/create.go::handleFromParameter")
        .into_iter()
        .find(|edge| edge.target.ends_with("::Create"))
        .expect("batchService.Create() should produce an edge");
    assert!(
        from_param.target.ends_with("::PayoutBatchUse::Create"),
        "got {}",
        from_param.target
    );
    assert_eq!(from_param.resolution, CallResolution::Receiver);

    let from_ctor = call_graph
        .get_callees("internal/module/payout/handler/create.go::handleFromConstructor")
        .into_iter()
        .find(|edge| edge.target.ends_with("::Create"))
        .expect("lineRepo.Create() should produce an edge");
    assert!(
        from_ctor.target.ends_with("::PayoutBatchLineRepo::Create"),
        "got {}",
        from_ctor.target
    );
}

/// A helper whose name says nothing (`accountRepo(ctx)`) but whose result list
/// says everything. This is the shape that made up a quarter of the remaining
/// guesses: a call chained onto a call that is not named like a factory.
#[test]
fn test_chained_call_uses_the_declared_result_type() {
    let parser = LanguageParser::new().unwrap();
    let call_graph = CallGraph::new();

    let files = vec![
        (
            "internal/usecase/transaction/test/fee_test.go",
            r#"
package test

func accountRepo(c ctx.Ctx) *account_repo.AccountRepo {
	return nil
}

func TestFee(t *testing.T) {
	account, err := accountRepo(context.C).FindByUuid(uuid)
}
"#,
        ),
        (
            "internal/model/account/repository/impl/account.go",
            "package impl\n\ntype AccountRepo struct{}\n\nfunc (self *AccountRepo) FindByUuid(uuid string) error {\n\treturn nil\n}\n",
        ),
        (
            "internal/model/card/repository/impl/card.go",
            "package impl\n\ntype CardRepo struct{}\n\nfunc (self *CardRepo) FindByUuid(uuid string) error {\n\treturn nil\n}\n",
        ),
    ]
    .into_iter()
    .map(|(path, code)| {
        let tree = parser.parse_to_tree(Path::new(path), code).unwrap();
        (path.to_string(), code.to_string(), tree)
    })
    .collect::<Vec<_>>();

    call_graph.build_from_files(&files).unwrap();

    let edge = call_graph
        .get_callees("internal/usecase/transaction/test/fee_test.go::TestFee")
        .into_iter()
        .find(|edge| edge.target.ends_with("::FindByUuid"))
        .expect("the chained call should produce an edge");
    assert!(
        edge.target.ends_with("::AccountRepo::FindByUuid"),
        "got {}",
        edge.target
    );
    assert_eq!(edge.resolution, CallResolution::Receiver);
}

/// `value, err := f()` is how Go returns anything at all, so a variable's type
/// is usually only readable through the result list of a multi-value call.
#[test]
fn test_multi_value_declaration_types_its_first_name() {
    let parser = LanguageParser::new().unwrap();
    let call_graph = CallGraph::new();

    let files = vec![
        (
            "internal/module/payout/usecase/test/execution_test.go",
            r#"
package test

func buildBatchService() (*payout_use.PayoutBatchUse, error) {
	return nil, nil
}

func TestExecution(t *testing.T) {
	batchService, err := buildBatchService()
	_, err = batchService.Submit("")
}
"#,
        ),
        (
            "internal/module/payout/usecase/impl/payout_batch.go",
            "package impl\n\ntype PayoutBatchUse struct{}\n\nfunc (self *PayoutBatchUse) Submit(id string) error {\n\treturn nil\n}\n",
        ),
        (
            "internal/module/payout/usecase/impl/payout_line.go",
            "package impl\n\ntype PayoutLineUse struct{}\n\nfunc (self *PayoutLineUse) Submit(id string) error {\n\treturn nil\n}\n",
        ),
    ]
    .into_iter()
    .map(|(path, code)| {
        let tree = parser.parse_to_tree(Path::new(path), code).unwrap();
        (path.to_string(), code.to_string(), tree)
    })
    .collect::<Vec<_>>();

    call_graph.build_from_files(&files).unwrap();

    let edge = call_graph
        .get_callees("internal/module/payout/usecase/test/execution_test.go::TestExecution")
        .into_iter()
        .find(|edge| edge.target.ends_with("::Submit"))
        .expect("batchService.Submit() should produce an edge");
    assert!(
        edge.target.ends_with("::PayoutBatchUse::Submit"),
        "got {}",
        edge.target
    );
    assert_eq!(edge.resolution, CallResolution::Receiver);
}

/// The reading of declarations must not invent a name where there is none:
/// `self.field = f()` binds no identifier, and used to panic on the empty
/// name list.
#[test]
fn test_assignment_to_a_field_declares_no_local() {
    let parser = LanguageParser::new().unwrap();
    let call_graph = CallGraph::new();
    let code = r#"
package impl

type Handler struct {
	svc *Service
}

func (self *Handler) setup() {
	self.svc = build()
	self.svc.Run()
}

func build() *Service {
	return nil
}
"#;
    let tree = parser.parse_to_tree(Path::new("handler.go"), code).unwrap();
    call_graph
        .build_from_files(&[("handler.go".to_string(), code.to_string(), tree)])
        .unwrap();

    assert!(
        !call_graph.get_callees("Handler.setup").is_empty(),
        "the calls in the body must still be recorded"
    );
}

/// Go promotes an embedded type's methods onto the outer type, so
/// `self.HandleError(err)` inside a method of `CustomerRepo { base.SimpleQuery }`
/// is `SimpleQuery::HandleError` — the most common single shape in a repository
/// built on a shared base struct.
#[test]
fn test_promoted_method_of_an_embedded_type_resolves() {
    let parser = LanguageParser::new().unwrap();
    let call_graph = CallGraph::new();

    let files = vec![
        (
            "internal/model/customer/repository/impl/customer.go",
            r#"
package impl

type CustomerRepo struct {
	base.SimpleQuery
}

func (self *CustomerRepo) FindByUuid(uuid string) error {
	return self.HandleError(err)
}
"#,
        ),
        (
            "internal/model/base/base.go",
            "package base\n\ntype SimpleQuery struct{}\n\nfunc (self *SimpleQuery) HandleError(err error) error {\n\treturn err\n}\n",
        ),
        (
            "internal/extservice/flash/impl/flash.go",
            "package impl\n\ntype FlashExtService struct{}\n\nfunc (self *FlashExtService) HandleError(err error) error {\n\treturn err\n}\n",
        ),
        (
            "internal/deployer/deploy/runner.go",
            "package deploy\n\ntype Runner struct{}\n\nfunc (self *Runner) HandleError(err error) error {\n\treturn err\n}\n",
        ),
    ]
    .into_iter()
    .map(|(path, code)| {
        let tree = parser.parse_to_tree(Path::new(path), code).unwrap();
        (path.to_string(), code.to_string(), tree)
    })
    .collect::<Vec<_>>();

    call_graph.build_from_files(&files).unwrap();

    let edge = call_graph
        .get_callees("CustomerRepo.FindByUuid")
        .into_iter()
        .find(|edge| edge.target.ends_with("::HandleError"))
        .expect("self.HandleError() should produce an edge");
    assert_eq!(
        edge.target, "internal/model/base/base.go::SimpleQuery::HandleError",
        "the method is promoted from the embedded type"
    );
    assert_eq!(
        edge.resolution,
        CallResolution::Receiver,
        "promotion is a rule of the language, not a guess"
    );
}

/// `self.C.Logger().SendError(err)`: nothing at the call site says what
/// `self.C` is, but the struct declares the field and the interface declares
/// what its method returns — two hops, both read.
#[test]
fn test_chain_through_a_field_and_an_interface_method() {
    let parser = LanguageParser::new().unwrap();
    let call_graph = CallGraph::new();

    let files = vec![
        (
            "internal/usecase/account/impl/account.go",
            r#"
package impl

type AccountUseCase struct {
	C solcontext.IContext
}

func (self *AccountUseCase) Pay(id string) error {
	self.C.Logger().SendError(err)
	return nil
}
"#,
        ),
        (
            "internal/lib/solcontext/context.go",
            "package solcontext\n\ntype IContext interface {\n\tLogger() log.ILogger\n\tDb() *gorm.DB\n}\n",
        ),
        (
            "internal/lib/logger/logger.go",
            "package log\n\ntype ILogger interface {\n\tSendError(err error)\n\tSendInfo(msg string)\n}\n",
        ),
        (
            "internal/lib/logger/impl/logger.go",
            "package impl\n\ntype Logger struct{}\n\nfunc (self *Logger) SendError(err error) {}\n\nfunc (self *Logger) SendInfo(msg string) {}\n",
        ),
        (
            "internal/extservice/sentry/impl/sentry.go",
            "package impl\n\ntype SentryClient struct{}\n\nfunc (self *SentryClient) SendError(err error) {}\n",
        ),
    ]
    .into_iter()
    .map(|(path, code)| {
        let tree = parser.parse_to_tree(Path::new(path), code).unwrap();
        (path.to_string(), code.to_string(), tree)
    })
    .collect::<Vec<_>>();

    call_graph.build_from_files(&files).unwrap();

    let edge = call_graph
        .get_callees("AccountUseCase.Pay")
        .into_iter()
        .find(|edge| edge.target.ends_with("::SendError"))
        .expect("the chained call should produce an edge");
    assert!(
        edge.target
            .ends_with("logger/impl/logger.go::Logger::SendError"),
        "got {} — the only type implementing ILogger",
        edge.target
    );
    assert_eq!(
        edge.resolution,
        CallResolution::InterfaceImpl,
        "dispatch is dynamic; one implementation is a candidate, not a fact"
    );
    assert!(!edge.resolution.is_certain());
    assert_eq!(
        edge.resolution.caveat(),
        Some("single implementation of the interface")
    );
}

/// A mock in the test tree implements the same interface as the real thing.
/// Production code is what a caller outside the tests reaches.
#[test]
fn test_interface_resolution_prefers_production_over_a_mock() {
    let parser = LanguageParser::new().unwrap();
    let call_graph = CallGraph::new();

    let files = vec![
        (
            "internal/usecase/sms/impl/sms.go",
            r#"
package impl

type SmsUseCase struct {
	Provider sms.IProvider
}

func (self *SmsUseCase) Send(to string) error {
	return self.Provider.Deliver(to)
}
"#,
        ),
        (
            "internal/extservice/sms/provider.go",
            "package sms\n\ntype IProvider interface {\n\tDeliver(to string) error\n\tBalance() int\n}\n",
        ),
        (
            "internal/extservice/sms/impl/provider.go",
            "package impl\n\ntype Provider struct{}\n\nfunc (self *Provider) Deliver(to string) error {\n\treturn nil\n}\n\nfunc (self *Provider) Balance() int {\n\treturn 0\n}\n",
        ),
        (
            "internal/extservice/sms/mock/provider.go",
            "package mock\n\ntype ProviderMock struct{}\n\nfunc (self *ProviderMock) Deliver(to string) error {\n\treturn nil\n}\n\nfunc (self *ProviderMock) Balance() int {\n\treturn 0\n}\n",
        ),
    ]
    .into_iter()
    .map(|(path, code)| {
        let tree = parser.parse_to_tree(Path::new(path), code).unwrap();
        (path.to_string(), code.to_string(), tree)
    })
    .collect::<Vec<_>>();

    call_graph.build_from_files(&files).unwrap();

    let edge = call_graph
        .get_callees("SmsUseCase.Send")
        .into_iter()
        .find(|edge| edge.target.ends_with("::Deliver"))
        .expect("self.Provider.Deliver() should produce an edge");
    assert!(
        edge.target
            .ends_with("sms/impl/provider.go::Provider::Deliver"),
        "got {}",
        edge.target
    );
}

/// A name declared as something outside this repository answers the question
/// by itself. `err.Error()` is not a call to whatever `Error` method this
/// corpus happens to define.
#[test]
fn test_call_on_a_foreign_typed_variable_leaves_the_graph() {
    let parser = LanguageParser::new().unwrap();
    let call_graph = CallGraph::new();

    let files = vec![
        (
            "internal/usecase/system_event/impl/handler.go",
            r#"
package impl

import (
	"sync"
)

func describe(err error, wg sync.WaitGroup) string {
	wg.Add(1)
	return err.Error()
}
"#,
        ),
        (
            "internal/model/report/impl/report.go",
            "package impl\n\ntype Report struct{}\n\nfunc (self *Report) Error() string {\n\treturn \"\"\n}\n\nfunc (self *Report) Add(n int) {}\n",
        ),
        (
            "internal/model/audit/impl/audit.go",
            "package impl\n\ntype Audit struct{}\n\nfunc (self *Audit) Error() string {\n\treturn \"\"\n}\n",
        ),
    ]
    .into_iter()
    .map(|(path, code)| {
        let tree = parser.parse_to_tree(Path::new(path), code).unwrap();
        (path.to_string(), code.to_string(), tree)
    })
    .collect::<Vec<_>>();

    call_graph.build_from_files(&files).unwrap();

    let callees = call_graph.get_callees("internal/usecase/system_event/impl/handler.go::describe");
    for edge in &callees {
        assert_eq!(
            edge.resolution,
            CallResolution::ExternalPackage,
            "{} is a call into someone else's code",
            edge.target
        );
    }
    assert!(
        call_graph.get_callers("Report.Error").is_empty(),
        "a builtin `error` must not collect callers for this repository's types"
    );
}

/// The three ways an edge can leave the graph mean three different things,
/// and one lumped `not in graph` count could not be acted on: half a Go
/// corpus leaving the graph is normal (stdlib and dependencies), while a call
/// into an *indexed* package that has no such function is a gap in what was
/// read.
#[test]
fn test_edges_outside_the_graph_say_which_kind_of_outside() {
    let parser = LanguageParser::new().unwrap();
    let call_graph = CallGraph::new();

    let files = vec![
        (
            "internal/usecase/pay/pay.go",
            r#"
package pay

import (
	"github.com/stretchr/testify/assert"

	"go-rest-api/internal/config"
)

func Pay(t assert.TestingT) {
	// An import that resolves to no indexed package.
	assert.Len(t, []int{1}, 1)
	// A package this index does carry, which has no such function.
	config.NeverDefined()
	// A name declared nowhere at all.
	println("done")
}
"#,
        ),
        (
            "internal/config/config.go",
            "package config\n\nfunc Get() string {\n\treturn \"\"\n}\n",
        ),
    ]
    .into_iter()
    .map(|(path, code)| {
        let tree = parser.parse_to_tree(Path::new(path), code).unwrap();
        (path.to_string(), code.to_string(), tree)
    })
    .collect::<Vec<_>>();

    call_graph.build_from_files(&files).unwrap();

    let callees = call_graph.get_callees("internal/usecase/pay/pay.go::Pay");
    let resolution_of = |name: &str| {
        callees
            .iter()
            .find(|edge| edge.target.ends_with(name))
            .map(|edge| edge.resolution)
    };

    assert_eq!(
        resolution_of("Len"),
        Some(CallResolution::ExternalPackage),
        "testify is outside the index: {callees:?}"
    );
    assert_eq!(
        resolution_of("NeverDefined"),
        Some(CallResolution::PackageMissing),
        "internal/config is indexed and has no such function: {callees:?}"
    );

    // All three group together wherever a report asks "did this resolve".
    for edge in &callees {
        assert!(
            edge.resolution.is_outside_graph(),
            "{} should count as outside the graph",
            edge.target
        );
        assert!(!edge.resolution.is_certain(), "{}", edge.target);
    }

    // And the statistics report them apart rather than as one figure.
    let stats = call_graph.resolution_stats();
    let count_of = |label: &str| {
        stats
            .iter()
            .find(|(name, _)| *name == label)
            .map(|(_, count)| *count)
            .unwrap_or_else(|| panic!("no such row: {label} in {stats:?}"))
    };
    assert_eq!(count_of("not in graph: outside the index"), 1, "{stats:?}");
    assert_eq!(
        count_of("not in graph: indexed package, no such function"),
        1,
        "{stats:?}"
    );
}
