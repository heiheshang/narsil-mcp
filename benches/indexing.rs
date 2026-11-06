//! Benchmarks for indexing performance.

#![allow(dead_code)]

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use tempfile::TempDir;

/// Benchmark indexing a repository with varying sizes
fn bench_index_repository(c: &mut Criterion) {
    let mut group = c.benchmark_group("index_repository");

    for num_files in [10, 50, 100, 500].iter() {
        let temp_dir = create_test_repo(*num_files);

        group.throughput(Throughput::Elements(*num_files as u64));
        group.bench_with_input(BenchmarkId::new("files", num_files), &temp_dir, |b, dir| {
            b.iter(|| index_repository(black_box(dir.path())));
        });
    }

    group.finish();
}

/// Benchmark symbol extraction
fn bench_symbol_extraction(c: &mut Criterion) {
    let temp_dir = create_test_repo(100);

    c.bench_function("extract_symbols_100_files", |b| {
        b.iter(|| extract_all_symbols(black_box(temp_dir.path())));
    });
}

/// Benchmark incremental indexing (single file change)
fn bench_incremental_index(c: &mut Criterion) {
    let temp_dir = create_test_repo(100);

    // Initial index
    let _index = index_repository(temp_dir.path());

    c.bench_function("incremental_single_file", |b| {
        b.iter(|| {
            // Simulate modifying one file
            let file_path = temp_dir.path().join("src/module_50.rs");
            let new_content = generate_rust_module(50, true); // Modified version
            std::fs::write(&file_path, new_content).unwrap();

            reindex_file(black_box(temp_dir.path()), black_box(&file_path))
        });
    });
}

/// Benchmark parallel vs sequential indexing
fn bench_parallel_indexing(c: &mut Criterion) {
    let temp_dir = create_test_repo(200);

    let mut group = c.benchmark_group("parallel_indexing");

    group.bench_function("sequential", |b| {
        b.iter(|| index_repository_sequential(black_box(temp_dir.path())));
    });

    group.bench_function("parallel", |b| {
        b.iter(|| index_repository_parallel(black_box(temp_dir.path())));
    });

    group.finish();
}

// Helper functions

fn create_test_repo(num_files: usize) -> TempDir {
    let temp_dir = TempDir::new().unwrap();
    let src_dir = temp_dir.path().join("src");
    std::fs::create_dir_all(&src_dir).unwrap();

    for i in 0..num_files {
        let content = generate_rust_module(i, false);
        std::fs::write(src_dir.join(format!("module_{}.rs", i)), content).unwrap();
    }

    // Create lib.rs with all module imports
    let mut lib_content = String::new();
    for i in 0..num_files {
        lib_content.push_str(&format!("mod module_{};\n", i));
    }
    std::fs::write(src_dir.join("lib.rs"), lib_content).unwrap();

    temp_dir
}

fn generate_rust_module(index: usize, modified: bool) -> String {
    let suffix = if modified { "_modified" } else { "" };
    format!(
        r#"//! Module {index}{suffix}

pub fn function_{index}_a(x: i32) -> i32 {{
    x + {index}
}}

pub fn function_{index}_b(x: i32) -> i32 {{
    function_{index}_a(x) * 2
}}

pub struct Struct{index} {{
    pub field_a: i32,
    pub field_b: String,
}}

impl Struct{index} {{
    pub fn new() -> Self {{
        Self {{
            field_a: {index},
            field_b: String::from("module_{index}"),
        }}
    }}

    pub fn process(&self) -> i32 {{
        function_{index}_a(self.field_a)
    }}
}}

pub enum Enum{index} {{
    VariantA,
    VariantB(i32),
    VariantC {{ x: i32, y: i32 }},
}}

pub trait Trait{index} {{
    fn method_a(&self) -> i32;
    fn method_b(&self) -> String;
}}

impl Trait{index} for Struct{index} {{
    fn method_a(&self) -> i32 {{
        self.field_a
    }}

    fn method_b(&self) -> String {{
        self.field_b.clone()
    }}
}}
"#
    )
}

// Stub functions - would be replaced with actual implementation

fn index_repository(path: &std::path::Path) -> usize {
    // Placeholder - would use CodeIntelEngine
    walkdir::WalkDir::new(path)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|ext| ext == "rs"))
        .count()
}

fn index_repository_sequential(path: &std::path::Path) -> usize {
    index_repository(path)
}

fn index_repository_parallel(path: &std::path::Path) -> usize {
    index_repository(path)
}

fn extract_all_symbols(path: &std::path::Path) -> usize {
    // Placeholder
    walkdir::WalkDir::new(path)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|ext| ext == "rs"))
        .map(|e| std::fs::read_to_string(e.path()).unwrap_or_default())
        .map(|content| content.matches("fn ").count() + content.matches("struct ").count())
        .sum()
}

fn reindex_file(_repo_path: &std::path::Path, file_path: &std::path::Path) -> usize {
    // Placeholder - would trigger incremental index
    let content = std::fs::read_to_string(file_path).unwrap_or_default();
    content.matches("fn ").count()
}

/// Call-graph construction over a Go corpus shaped like the ones this
/// resolution work targets: dependency-injected services, methods spread
/// across a package's files, a shared embedded base struct, package-qualified
/// calls into both this "repository" and outside it.
///
/// Guards the cost of resolution, which reads declarations, follows field
/// chains and checks interface implementations — none of which shows up in
/// edge counts, only in wall clock.
fn bench_call_graph_resolution(c: &mut Criterion) {
    use narsil_mcp::callgraph::CallGraph;
    use narsil_mcp::parser::LanguageParser;

    let parser = LanguageParser::new().unwrap();
    let mut sources: Vec<(String, String)> = Vec::new();
    for module in 0..24 {
        sources.push((
            format!("internal/module/m{module}/usecase/impl/service.go"),
            format!(
                r#"
package impl

type Service{module} struct {{
	base.SimpleQuery
	Depend Depend{module}
	C      solcontext.IContext
}}

type Depend{module} struct {{
	RepoFactory repo.Repo{module}Factory
}}

func (self *Service{module}) Process(id string) error {{
	item, err := self.Depend.RepoFactory(self.C).FindByUuid(id)
	if err != nil {{
		return self.HandleError(err)
	}}
	self.C.Logger().SendError(err)
	svc := NewService{module}()
	return svc.Finish(item)
}}

func (self *Service{module}) Finish(item string) error {{
	return nil
}}

func NewService{module}() *Service{module} {{
	return &Service{module}{{}}
}}
"#
            ),
        ));
        sources.push((
            format!("internal/module/m{module}/model/repository/impl/repo.go"),
            format!(
                r#"
package impl

import (
	"fmt"

	"go-rest-api/internal/config"
)

type Repo{module} struct {{
	base.SimpleQuery
}}

func (self *Repo{module}) FindByUuid(uuid string) (string, error) {{
	fmt.Println(config.Get())
	return "", nil
}}
"#
            ),
        ));
    }
    sources.push((
        "internal/model/base/base.go".to_string(),
        "package base

type SimpleQuery struct{}

func (self *SimpleQuery) HandleError(err error) error {
	return err
}
"
        .to_string(),
    ));
    sources.push((
        "internal/config/config.go".to_string(),
        "package config

func Get() *Config {
	return nil
}

type Config struct{}
"
        .to_string(),
    ));

    let files: Vec<_> = sources
        .into_iter()
        .map(|(path, code)| {
            let tree = parser
                .parse_to_tree(std::path::Path::new(&path), &code)
                .unwrap();
            (path, code, tree)
        })
        .collect();

    c.bench_function("call_graph_build_50_go_files", |b| {
        b.iter(|| {
            let graph = CallGraph::new();
            graph.build_from_files(&files).unwrap();
            black_box(graph.node_count())
        })
    });
}

criterion_group!(
    benches,
    bench_index_repository,
    bench_symbol_extraction,
    bench_incremental_index,
    bench_parallel_indexing,
    bench_call_graph_resolution,
);
criterion_main!(benches);
