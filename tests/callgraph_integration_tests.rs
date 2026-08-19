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
