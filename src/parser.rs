use anyhow::{anyhow, Result};
use std::path::Path;
use std::sync::{Arc, OnceLock};
use streaming_iterator::StreamingIterator;
use tree_sitter::{Language, Parser, Query, QueryCursor, Tree};

use crate::symbols::{Symbol, SymbolKind};

/// Supported languages and their tree-sitter configurations
#[derive(Debug, Clone)]
pub struct LanguageConfig {
    pub name: String,
    pub language: Language,
    pub extensions: Vec<&'static str>,
    pub symbol_query: &'static str,
}

/// Language configuration with lazily-compiled query
/// Query is compiled on first use, warnings logged once, then cached
struct LazyLanguageConfig {
    config: LanguageConfig,
    /// Lazily compiled query (None if compilation failed)
    compiled_query: OnceLock<Option<Arc<Query>>>,
}

impl LazyLanguageConfig {
    fn new(config: LanguageConfig) -> Self {
        Self {
            config,
            compiled_query: OnceLock::new(),
        }
    }

    /// Get the compiled query, compiling on first access
    fn get_query(&self) -> Option<&Arc<Query>> {
        self.compiled_query
            .get_or_init(
                || match Query::new(&self.config.language, self.config.symbol_query) {
                    Ok(q) => Some(Arc::new(q)),
                    Err(e) => {
                        tracing::warn!(
                            "Query compilation failed for {} (this warning appears once): {:?}",
                            self.config.name,
                            e
                        );
                        None
                    }
                },
            )
            .as_ref()
    }
}

/// A parsed file with extracted information
#[derive(Debug, Clone)]
pub struct ParsedFile {
    #[allow(dead_code)]
    pub path: String,
    pub language: String,
    pub symbols: Vec<Symbol>,
    #[allow(dead_code)]
    pub tree: Option<Tree>,
}

/// Multi-language parser using tree-sitter
pub struct LanguageParser {
    configs: Vec<LazyLanguageConfig>,
}

impl LanguageParser {
    pub fn new() -> Result<Self> {
        let configs = vec![
            // Rust
            LanguageConfig {
                name: "rust".to_string(),
                language: tree_sitter_rust::LANGUAGE.into(),
                extensions: vec!["rs"],
                symbol_query: r#"
                    (function_item name: (identifier) @function.name) @function.def
                    (struct_item name: (type_identifier) @struct.name) @struct.def
                    (enum_item name: (type_identifier) @enum.name) @enum.def
                    (trait_item name: (type_identifier) @trait.name) @trait.def
                    (impl_item type: (type_identifier) @impl.name) @impl.def
                    (type_item name: (type_identifier) @type.name) @type.def
                    (const_item name: (identifier) @const.name) @const.def
                    (static_item name: (identifier) @static.name) @static.def
                    (mod_item name: (identifier) @mod.name) @mod.def
                "#,
            },
            // Python
            LanguageConfig {
                name: "python".to_string(),
                language: tree_sitter_python::LANGUAGE.into(),
                extensions: vec!["py", "pyi"],
                symbol_query: r#"
                    (function_definition name: (identifier) @function.name) @function.def
                    (class_definition name: (identifier) @class.name) @class.def
                "#,
            },
            // JavaScript
            LanguageConfig {
                name: "javascript".to_string(),
                language: tree_sitter_javascript::LANGUAGE.into(),
                extensions: vec!["js", "jsx", "mjs"],
                symbol_query: r#"
                    (function_declaration name: (identifier) @function.name) @function.def
                    (class_declaration name: (identifier) @class.name) @class.def
                    (method_definition name: (property_identifier) @method.name) @method.def
                    (arrow_function) @arrow.def
                    (variable_declarator name: (identifier) @var.name) @var.def
                "#,
            },
            // TypeScript
            LanguageConfig {
                name: "typescript".to_string(),
                language: tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
                extensions: vec!["ts"],
                symbol_query: r#"
                    (function_declaration name: (identifier) @function.name) @function.def
                    (class_declaration name: (type_identifier) @class.name) @class.def
                    (method_definition name: (property_identifier) @method.name) @method.def
                    (interface_declaration name: (type_identifier) @interface.name) @interface.def
                    (type_alias_declaration name: (type_identifier) @type.name) @type.def
                    (enum_declaration name: (identifier) @enum.name) @enum.def
                "#,
            },
            // TSX
            LanguageConfig {
                name: "tsx".to_string(),
                language: tree_sitter_typescript::LANGUAGE_TSX.into(),
                extensions: vec!["tsx"],
                symbol_query: r#"
                    (function_declaration name: (identifier) @function.name) @function.def
                    (class_declaration name: (type_identifier) @class.name) @class.def
                    (method_definition name: (property_identifier) @method.name) @method.def
                    (interface_declaration name: (type_identifier) @interface.name) @interface.def
                    (type_alias_declaration name: (type_identifier) @type.name) @type.def
                "#,
            },
            // Go
            LanguageConfig {
                name: "go".to_string(),
                language: tree_sitter_go::LANGUAGE.into(),
                extensions: vec!["go"],
                symbol_query: r#"
                    (function_declaration name: (identifier) @function.name) @function.def
                    (method_declaration name: (field_identifier) @method.name) @method.def
                    (type_declaration (type_spec name: (type_identifier) @type.name)) @type.def
                "#,
            },
            // C
            LanguageConfig {
                name: "c".to_string(),
                language: tree_sitter_c::LANGUAGE.into(),
                extensions: vec!["c", "h"],
                symbol_query: r#"
                    (function_definition declarator: (function_declarator declarator: (identifier) @function.name)) @function.def
                    (struct_specifier name: (type_identifier) @struct.name) @struct.def
                    (enum_specifier name: (type_identifier) @enum.name) @enum.def
                    (type_definition declarator: (type_identifier) @type.name) @type.def
                "#,
            },
            // C++
            LanguageConfig {
                name: "cpp".to_string(),
                language: tree_sitter_cpp::LANGUAGE.into(),
                extensions: vec!["cpp", "cc", "cxx", "hpp", "hxx", "hh"],
                symbol_query: r#"
                    (function_definition declarator: (function_declarator declarator: (identifier) @function.name)) @function.def
                    (class_specifier name: (type_identifier) @class.name) @class.def
                    (struct_specifier name: (type_identifier) @struct.name) @struct.def
                    (enum_specifier name: (type_identifier) @enum.name) @enum.def
                    (namespace_definition name: (namespace_identifier) @namespace.name) @namespace.def
                "#,
            },
            // Java
            LanguageConfig {
                name: "java".to_string(),
                language: tree_sitter_java::LANGUAGE.into(),
                extensions: vec!["java"],
                symbol_query: r#"
                    (method_declaration name: (identifier) @method.name) @method.def
                    (class_declaration name: (identifier) @class.name) @class.def
                    (interface_declaration name: (identifier) @interface.name) @interface.def
                    (enum_declaration name: (identifier) @enum.name) @enum.def
                "#,
            },
            // C#
            LanguageConfig {
                name: "csharp".to_string(),
                language: tree_sitter_c_sharp::LANGUAGE.into(),
                extensions: vec!["cs"],
                symbol_query: r#"
                    (method_declaration name: (identifier) @method.name) @method.def
                    (class_declaration name: (identifier) @class.name) @class.def
                    (interface_declaration name: (identifier) @interface.name) @interface.def
                    (struct_declaration name: (identifier) @struct.name) @struct.def
                    (enum_declaration name: (identifier) @enum.name) @enum.def
                    (record_declaration name: (identifier) @class.name) @class.def
                    (delegate_declaration name: (identifier) @type.name) @type.def
                    (namespace_declaration name: (identifier) @namespace.name) @namespace.def
                    (property_declaration name: (identifier) @var.name) @var.def
                "#,
            },
            // Bash
            LanguageConfig {
                name: "bash".to_string(),
                language: tree_sitter_bash::LANGUAGE.into(),
                extensions: vec!["sh", "bash", "zsh"],
                symbol_query: r#"
                    (function_definition name: (word) @function.name) @function.def
                    (variable_assignment name: (variable_name) @var.name) @var.def
                "#,
            },
            // Ruby
            LanguageConfig {
                name: "ruby".to_string(),
                language: tree_sitter_ruby::LANGUAGE.into(),
                extensions: vec!["rb", "rake", "gemspec"],
                symbol_query: r#"
                    (method name: (identifier) @method.name) @method.def
                    (singleton_method name: (identifier) @method.name) @method.def
                    (class name: (constant) @class.name) @class.def
                    (module name: (constant) @mod.name) @mod.def
                "#,
            },
            // Kotlin
            LanguageConfig {
                name: "kotlin".to_string(),
                language: tree_sitter_kotlin_sg::LANGUAGE.into(),
                extensions: vec!["kt", "kts"],
                symbol_query: r#"
                    (function_declaration (simple_identifier) @function.name) @function.def
                    (class_declaration (type_identifier) @class.name) @class.def
                    (object_declaration (type_identifier) @class.name) @class.def
                "#,
            },
            // PHP
            LanguageConfig {
                name: "php".to_string(),
                language: tree_sitter_php::LANGUAGE_PHP.into(),
                extensions: vec!["php", "phtml"],
                symbol_query: r#"
                    (function_definition name: (name) @function.name) @function.def
                    (method_declaration name: (name) @method.name) @method.def
                    (class_declaration name: (name) @class.name) @class.def
                    (interface_declaration name: (name) @interface.name) @interface.def
                    (trait_declaration name: (name) @trait.name) @trait.def
                "#,
            },
            // Swift
            // Note: Swift tree-sitter uses class_declaration for classes, structs, and enums
            LanguageConfig {
                name: "swift".to_string(),
                language: tree_sitter_swift::LANGUAGE.into(),
                extensions: vec!["swift"],
                symbol_query: r#"
                    (class_declaration name: (type_identifier) @class.name) @class.def
                    (protocol_declaration name: (type_identifier) @interface.name) @interface.def
                    (function_declaration name: (simple_identifier) @function.name) @function.def
                "#,
            },
            // Verilog/SystemVerilog
            LanguageConfig {
                name: "verilog".to_string(),
                language: tree_sitter_verilog::LANGUAGE.into(),
                extensions: vec!["v", "vh", "sv", "svh"],
                symbol_query: r#"
                    (module_declaration (module_header (simple_identifier) @module.name)) @module.def
                    (task_body_declaration (task_identifier (task_identifier (simple_identifier) @function.name))) @function.def
                    (function_body_declaration (function_identifier (function_identifier (simple_identifier) @function.name))) @function.def
                    (interface_declaration (interface_identifier (simple_identifier) @interface.name)) @interface.def
                    (class_declaration (class_identifier (simple_identifier) @class.name)) @class.def
                "#,
            },
        ];

        // Wrap configs in lazy wrappers (queries compiled on first use, not during init)
        let lazy_configs = configs.into_iter().map(LazyLanguageConfig::new).collect();

        Ok(Self {
            configs: lazy_configs,
        })
    }

    /// Get language config for a file extension
    fn get_config(&self, path: &Path) -> Option<&LazyLanguageConfig> {
        let ext = path.extension()?.to_str()?;
        self.configs
            .iter()
            .find(|c| c.config.extensions.contains(&ext))
    }

    /// Parse a file and extract symbols
    pub fn parse_file(&self, path: &Path, content: &str) -> Result<ParsedFile> {
        let lazy_config = self
            .get_config(path)
            .ok_or_else(|| anyhow!("Unsupported file type: {:?}", path))?;

        let mut parser = Parser::new();
        parser.set_language(&lazy_config.config.language)?;

        let tree = parser
            .parse(content, None)
            .ok_or_else(|| anyhow!("Failed to parse file"))?;

        let symbols = self.extract_symbols(&tree, content, lazy_config)?;

        Ok(ParsedFile {
            path: path.to_string_lossy().to_string(),
            language: lazy_config.config.name.clone(),
            symbols,
            tree: Some(tree),
        })
    }

    /// Parse a file and return just the tree (for call graph analysis)
    #[allow(dead_code)]
    pub fn parse_to_tree(&self, path: &Path, content: &str) -> Result<Tree> {
        let lazy_config = self
            .get_config(path)
            .ok_or_else(|| anyhow!("Unsupported file type: {:?}", path))?;

        let mut parser = Parser::new();
        parser.set_language(&lazy_config.config.language)?;

        parser
            .parse(content, None)
            .ok_or_else(|| anyhow!("Failed to parse file"))
    }

    /// Extract symbols using tree-sitter queries
    fn extract_symbols(
        &self,
        tree: &Tree,
        source: &str,
        lazy_config: &LazyLanguageConfig,
    ) -> Result<Vec<Symbol>> {
        let mut symbols = Vec::new();
        let source_bytes = source.as_bytes();

        // Get lazily-compiled query (errors logged once on first access)
        let query = match lazy_config.get_query() {
            Some(q) => q,
            None => return Ok(symbols), // Query compilation failed, return empty
        };

        let mut cursor = QueryCursor::new();
        let mut matches = cursor.matches(query, tree.root_node(), source_bytes);

        while let Some(match_) = matches.next() {
            let mut name: Option<String> = None;
            let mut kind: Option<SymbolKind> = None;
            let mut start_line = 0;
            let mut end_line = 0;
            let mut signature: Option<String> = None;

            for capture in match_.captures {
                let capture_name = query.capture_names()[capture.index as usize];
                let node = capture.node;
                let text = node.utf8_text(source_bytes).unwrap_or("");

                if capture_name.ends_with(".name") {
                    name = Some(text.to_string());
                    kind = Some(parse_symbol_kind(capture_name));
                } else if capture_name.ends_with(".def") {
                    start_line = node.start_position().row + 1;
                    end_line = node.end_position().row + 1;

                    // Extract first line as signature
                    let first_line_end = text.find('\n').unwrap_or(text.len());
                    signature = Some(text[..first_line_end.min(200)].to_string());
                }
            }

            if let (Some(name), Some(kind)) = (name, kind) {
                symbols.push(Symbol {
                    name,
                    kind,
                    file_path: String::new(), // Will be set by caller
                    start_line,
                    end_line,
                    signature,
                    qualified_name: None,
                    doc_comment: None,
                });
            }
        }

        Ok(symbols)
    }

    /// Get all supported extensions
    #[allow(dead_code)]
    pub fn supported_extensions(&self) -> Vec<&'static str> {
        self.configs
            .iter()
            .flat_map(|c| c.config.extensions.iter().copied())
            .collect()
    }
}

fn parse_symbol_kind(capture_name: &str) -> SymbolKind {
    let prefix = capture_name.split('.').next().unwrap_or("");
    match prefix {
        "function" => SymbolKind::Function,
        "method" => SymbolKind::Method,
        "class" => SymbolKind::Class,
        "struct" => SymbolKind::Struct,
        "enum" => SymbolKind::Enum,
        "interface" => SymbolKind::Interface,
        "trait" => SymbolKind::Trait,
        "type" => SymbolKind::TypeAlias,
        "const" | "static" => SymbolKind::Constant,
        "mod" | "module" | "namespace" => SymbolKind::Module,
        "impl" => SymbolKind::Implementation,
        "var" | "arrow" => SymbolKind::Variable,
        _ => SymbolKind::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_rust() {
        let parser = LanguageParser::new().unwrap();
        let content = r#"
            pub struct MyStruct {
                field: u32,
            }

            pub fn my_function() -> i32 {
                42
            }

            impl MyStruct {
                pub fn method(&self) {}
            }
        "#;

        let parsed = parser.parse_file(Path::new("test.rs"), content).unwrap();
        assert_eq!(parsed.language, "rust");
        assert!(!parsed.symbols.is_empty());

        let names: Vec<_> = parsed.symbols.iter().map(|s| &s.name).collect();
        assert!(names.contains(&&"MyStruct".to_string()));
        assert!(names.contains(&&"my_function".to_string()));
    }

    #[test]
    fn test_parse_python() {
        let parser = LanguageParser::new().unwrap();
        let content = r#"
class MyClass:
    def __init__(self):
        pass

    def method(self):
        return 42

def standalone_function():
    pass
        "#;

        let parsed = parser.parse_file(Path::new("test.py"), content).unwrap();
        assert_eq!(parsed.language, "python");

        let names: Vec<_> = parsed.symbols.iter().map(|s| &s.name).collect();
        assert!(names.contains(&&"MyClass".to_string()));
        assert!(names.contains(&&"standalone_function".to_string()));
    }

    #[test]
    fn test_parse_csharp() {
        let parser = LanguageParser::new().unwrap();
        let content = r#"
namespace MyApp
{
    public interface IService
    {
        void DoWork();
    }

    public class MyService : IService
    {
        public string Name { get; set; }

        public void DoWork()
        {
            Console.WriteLine("Working");
        }

        private int Calculate(int x, int y)
        {
            return x + y;
        }
    }

    public struct Point
    {
        public int X;
        public int Y;
    }

    public enum Status
    {
        Active,
        Inactive
    }
}
        "#;

        let parsed = parser.parse_file(Path::new("test.cs"), content).unwrap();
        assert_eq!(parsed.language, "csharp");

        let names: Vec<_> = parsed.symbols.iter().map(|s| &s.name).collect();
        assert!(
            names.contains(&&"MyApp".to_string()),
            "Should find namespace"
        );
        assert!(
            names.contains(&&"IService".to_string()),
            "Should find interface"
        );
        assert!(
            names.contains(&&"MyService".to_string()),
            "Should find class"
        );
        assert!(names.contains(&&"DoWork".to_string()), "Should find method");
        assert!(names.contains(&&"Point".to_string()), "Should find struct");
        assert!(names.contains(&&"Status".to_string()), "Should find enum");
    }

    #[test]
    fn test_parse_swift() {
        let parser = LanguageParser::new().unwrap();
        let content = r#"
class MyClass {
    var name: String

    init(name: String) {
        self.name = name
    }

    func greet() -> String {
        return "Hello, \(name)"
    }
}

struct Point {
    var x: Int
    var y: Int
}

protocol Drawable {
    func draw()
}

enum Direction {
    case north
    case south
    case east
    case west
}

func standaloneFunction() {
    print("Hello")
}
        "#;

        let parsed = parser.parse_file(Path::new("test.swift"), content).unwrap();
        assert_eq!(parsed.language, "swift");
        assert!(!parsed.symbols.is_empty());

        let names: Vec<_> = parsed.symbols.iter().map(|s| &s.name).collect();
        assert!(names.contains(&&"MyClass".to_string()), "Should find class");
        assert!(names.contains(&&"Point".to_string()), "Should find struct");
        assert!(
            names.contains(&&"Drawable".to_string()),
            "Should find protocol"
        );
        assert!(
            names.contains(&&"Direction".to_string()),
            "Should find enum"
        );
        assert!(
            names.contains(&&"standaloneFunction".to_string()),
            "Should find function"
        );
    }

    #[test]
    fn test_parse_cpp() {
        let parser = LanguageParser::new().unwrap();
        let content = r#"
namespace MyNamespace {
    class MyClass {
    public:
        void myMethod() {}
    };

    struct MyStruct {
        int x;
        int y;
    };

    enum MyEnum {
        VALUE_A,
        VALUE_B
    };
}

void standaloneFunction() {
    // do something
}
        "#;

        let parsed = parser.parse_file(Path::new("test.cpp"), content).unwrap();
        assert_eq!(parsed.language, "cpp");
        assert!(!parsed.symbols.is_empty());

        let names: Vec<_> = parsed.symbols.iter().map(|s| &s.name).collect();
        assert!(
            names.contains(&&"MyNamespace".to_string()),
            "Should find namespace, found: {:?}",
            names
        );
        assert!(names.contains(&&"MyClass".to_string()), "Should find class");
        assert!(
            names.contains(&&"MyStruct".to_string()),
            "Should find struct"
        );
        assert!(names.contains(&&"MyEnum".to_string()), "Should find enum");
        assert!(
            names.contains(&&"standaloneFunction".to_string()),
            "Should find function"
        );
    }

    #[test]
    fn test_parse_verilog() {
        let parser = LanguageParser::new().unwrap();
        let content = r#"
module counter(
    input clk,
    input reset,
    output reg [7:0] count
);
    always @(posedge clk or posedge reset) begin
        if (reset)
            count <= 8'b0;
        else
            count <= count + 1;
    end
endmodule

module test_bench;
    reg clk;
    reg reset;
    wire [7:0] count;

    counter uut (
        .clk(clk),
        .reset(reset),
        .count(count)
    );

    task run_test;
        begin
            #10 reset = 0;
            #50 $finish;
        end
    endtask

    function [7:0] double_value;
        input [7:0] val;
        begin
            double_value = val * 2;
        end
    endfunction
endmodule
        "#;

        let parsed = parser.parse_file(Path::new("test.v"), content).unwrap();
        assert_eq!(parsed.language, "verilog");
        assert!(!parsed.symbols.is_empty());

        let names: Vec<_> = parsed.symbols.iter().map(|s| &s.name).collect();
        assert!(
            names.contains(&&"counter".to_string()),
            "Should find module counter"
        );
        assert!(
            names.contains(&&"test_bench".to_string()),
            "Should find module test_bench"
        );
        assert!(names.contains(&&"run_test".to_string()), "Should find task");
        assert!(
            names.contains(&&"double_value".to_string()),
            "Should find function"
        );
    }
}
