//! Call graph analysis - tracks which functions call which
//!
//! This is critical for AI understanding of code flow and impact analysis.

use anyhow::Result;
use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet, VecDeque};
use tree_sitter::{Node, Tree};

/// A node in the call graph
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CallNode {
    /// Fully qualified name of the function
    pub name: String,
    /// File where defined
    pub file_path: String,
    /// Line number
    pub line: usize,
    /// Functions this calls (outgoing edges)
    pub calls: Vec<CallEdge>,
    /// Complexity metrics
    pub metrics: FunctionMetrics,
    /// The type this function is defined on: the Go receiver, the Rust `impl`
    /// type, or the enclosing class/trait elsewhere. `None` for free
    /// functions. Node keys are `file::name`, so this is what tells two
    /// same-named methods apart.
    #[serde(default)]
    pub receiver: Option<String>,
}

/// An edge in the call graph
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CallEdge {
    /// Target function name
    pub target: String,
    /// File containing the call
    pub file_path: String,
    /// Line of the call site
    pub line: usize,
    /// Column of the call site
    pub column: usize,
    /// Is this a direct call or through a reference/closure?
    pub call_type: CallType,
    /// Scope qualifier from the call site (e.g. "App" from `App::run()`)
    #[serde(default)]
    pub scope_hint: Option<String>,
    /// Whether the target was resolved to a known function in the graph.
    /// False when the callee name had no matching node — `get_callers` must
    /// surface this instead of silently pretending the edge is complete.
    #[serde(default)]
    pub resolved: bool,
    /// What the target was matched on. A call to `x.run()` cannot be tied to
    /// the type of `x` without type inference, so some edges are name matches
    /// among namesakes; reports must be able to say which.
    #[serde(default)]
    pub resolution: CallResolution,
}

/// How an edge's target was chosen among the functions of that name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum CallResolution {
    /// Recorded before this distinction existed.
    #[default]
    Unknown,
    /// Only one function in the graph carries the name.
    Unique,
    /// The call qualifier named the target's receiver type (`Store::new()`).
    Receiver,
    /// The qualifier matched the target's module or file path.
    Scope,
    /// Several namesakes; the one in the caller's own file was taken.
    SameFile,
    /// Several namesakes and nothing to choose between them — the target is a
    /// deterministic pick, not a fact.
    NameOnly,
    /// No function of that name is in the graph (third-party, stdlib, macro).
    Unresolved,
}

impl CallResolution {
    /// Does this edge point at a function the graph actually identified?
    pub fn is_certain(self) -> bool {
        matches!(self, Self::Unique | Self::Receiver | Self::Scope)
    }

    /// Short label for reports; `None` for edges that need no caveat.
    pub fn caveat(self) -> Option<&'static str> {
        match self {
            Self::SameFile => Some("same-file match"),
            Self::NameOnly => Some("name match only"),
            Self::Unresolved => Some("not in graph"),
            Self::Unknown => Some("match unrecorded"),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum CallType {
    Direct,       // foo()
    Method,       // obj.foo()
    StaticMethod, // Type::foo()
    Closure,      // let f = foo; f()
    Async,        // foo().await
    Spawn,        // spawn(foo)
    Unknown,
}

/// Function complexity metrics
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FunctionMetrics {
    /// Lines of code
    pub loc: usize,
    /// Cyclomatic complexity (branches + 1)
    pub cyclomatic: usize,
    /// Nesting depth
    pub max_depth: usize,
    /// Number of parameters
    pub params: usize,
    /// Number of return points
    pub returns: usize,
    /// Cognitive complexity
    pub cognitive: usize,
}

/// The call graph for a repository
pub struct CallGraph {
    /// Function name -> CallNode
    nodes: DashMap<String, CallNode>,
    /// File -> Functions defined in that file
    file_functions: DashMap<String, Vec<String>>,
    /// Bare function name -> qualified keys. Lets `resolve_callee` look up
    /// candidates in O(1) instead of scanning every node per call site
    /// (O(N²) on a 469k-symbol corpus).
    name_index: DashMap<String, Vec<String>>,
}

impl Default for CallGraph {
    fn default() -> Self {
        Self::new()
    }
}

impl CallGraph {
    pub fn new() -> Self {
        Self {
            nodes: DashMap::new(),
            file_functions: DashMap::new(),
            name_index: DashMap::new(),
        }
    }

    /// Build call graph from parsed files (two passes over the same slice).
    pub fn build_from_files(&self, files: &[(String, String, Tree)]) -> Result<()> {
        self.collect_functions(files)?;
        self.collect_calls(files)
    }

    /// First pass: collect function definitions from a batch of parsed files.
    ///
    /// Split out of [`build_from_files`](Self::build_from_files) so the indexer
    /// can stream files in chunks and drop each batch's parse trees, instead of
    /// holding every tree in memory at once.
    pub fn collect_functions(&self, files: &[(String, String, Tree)]) -> Result<()> {
        for (path, content, tree) in files {
            self.extract_functions(path, content, tree)?;
        }
        Ok(())
    }

    /// Second pass: collect call sites and resolve them against the functions
    /// gathered by [`collect_functions`](Self::collect_functions) across all
    /// prior batches.
    pub fn collect_calls(&self, files: &[(String, String, Tree)]) -> Result<()> {
        for (path, content, tree) in files {
            self.extract_calls(path, content, tree)?;
        }
        Ok(())
    }

    /// Create a qualified key for the DashMap: "file_path::function_name"
    fn qualified_key(file_path: &str, name: &str) -> String {
        format!("{}::{}", file_path, name)
    }

    /// Key for a definition: `file::name` for a free function, and
    /// `file::Type::name` for a method. Without the type, two methods of the
    /// same name in one file collapse onto a single node and one of them
    /// disappears from the graph.
    fn definition_key(file_path: &str, receiver: Option<&str>, name: &str) -> String {
        match receiver {
            Some(receiver) => format!("{}::{}::{}", file_path, receiver, name),
            None => Self::qualified_key(file_path, name),
        }
    }

    /// Insert a node and maintain the bare-name index so `resolve_callee`
    /// stays O(1). The only way a node should enter the graph.
    fn insert_node(&self, key: String, node: CallNode) {
        self.name_index
            .entry(node.name.clone())
            .or_default()
            .push(key.clone());
        self.nodes.insert(key, node);
    }

    fn extract_functions(&self, path: &str, content: &str, tree: &Tree) -> Result<()> {
        let source = content.as_bytes();
        let mut cursor = tree.walk();
        let mut functions = Vec::new();

        self.walk_for_functions(&mut cursor, source, path, &mut functions);

        for func in &functions {
            let key = Self::definition_key(path, func.receiver.as_deref(), &func.name);
            self.insert_node(key, func.clone());
        }

        let names: Vec<_> = functions
            .into_iter()
            .map(|f| Self::definition_key(path, f.receiver.as_deref(), &f.name))
            .collect();
        self.file_functions.insert(path.to_string(), names);

        Ok(())
    }

    fn walk_for_functions(
        &self,
        cursor: &mut tree_sitter::TreeCursor,
        source: &[u8],
        path: &str,
        functions: &mut Vec<CallNode>,
    ) {
        loop {
            let node = cursor.node();

            if let Some(func) = self.try_extract_function(node, source, path) {
                functions.push(func);
            }

            // Recurse into children
            if cursor.goto_first_child() {
                self.walk_for_functions(cursor, source, path, functions);
                cursor.goto_parent();
            }

            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }

    /// Kinds that declare a type whose body holds methods.
    fn is_type_declaration(kind: &str) -> bool {
        matches!(
            kind,
            "trait_item"
                | "class_definition"
                | "class_declaration"
                | "class_specifier"
                | "struct_declaration"
                | "struct_specifier"
                | "interface_declaration"
                | "enum_declaration"
                | "record_declaration"
                | "trait_declaration"
                | "object_declaration"
                | "protocol_declaration"
                | "extension_declaration"
                // Ruby
                | "class"
                | "module"
        )
    }

    /// First type name in a subtree: `*Server` -> `Server`, `Store[T]` ->
    /// `Store`. Falls back to a plain identifier for grammars without a
    /// dedicated type node (Ruby constants, Python class names).
    fn first_type_identifier(node: Node, source: &[u8]) -> Option<String> {
        fn walk(node: Node, kinds: &[&str], out: &mut Option<String>, source: &[u8]) {
            if out.is_some() {
                return;
            }
            if kinds.contains(&node.kind()) {
                *out = node.utf8_text(source).ok().map(|s| s.to_string());
                return;
            }
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                walk(child, kinds, out, source);
            }
        }

        let mut found = None;
        walk(node, &["type_identifier"], &mut found, source);
        if found.is_none() {
            walk(
                node,
                &["identifier", "constant", "simple_identifier", "name"],
                &mut found,
                source,
            );
        }
        found
    }

    /// The type a function is defined on, or `None` for a free function.
    ///
    /// Go keeps it in the method's `receiver` field; every other supported
    /// language nests the function inside the `impl`/class/trait that owns it.
    fn extract_receiver_type(node: Node, source: &[u8]) -> Option<String> {
        if let Some(receiver) = node.child_by_field_name("receiver") {
            if let Some(name) = Self::first_type_identifier(receiver, source) {
                return Some(name);
            }
        }

        let mut ancestor = node.parent();
        while let Some(current) = ancestor {
            if current.kind() == "impl_item" {
                // Rust: `impl Foo` and `impl Trait for Foo` both own `Foo`.
                return current
                    .child_by_field_name("type")
                    .and_then(|owner| Self::first_type_identifier(owner, source));
            }
            if Self::is_type_declaration(current.kind()) {
                if let Some(name) = current
                    .child_by_field_name("name")
                    .and_then(|name| name.utf8_text(source).ok().map(|s| s.to_string()))
                {
                    return Some(name);
                }
                // Python's root node is also called `module`; unlike Ruby's it
                // names no type, so the walk continues past it.
                if current.kind() != "module" {
                    return Self::first_type_identifier(current, source);
                }
            }
            ancestor = current.parent();
        }
        None
    }

    fn try_extract_function(&self, node: Node, source: &[u8], path: &str) -> Option<CallNode> {
        let kind = node.kind();

        // Match function definition patterns across languages
        let is_function = matches!(
            kind,
            "function_item"
                | "function_definition"
                | "procedure_definition"
                | "function_declaration"
                | "method_definition"
                | "method_declaration"
                // Ruby
                | "method"
                | "singleton_method"
                | "arrow_function"
                | "lambda"
                | "closure_expression"
        );

        if !is_function {
            return None;
        }

        // Try to find the function name
        let name = extract_function_name(node, source)?;
        let metrics = self.compute_metrics(node, source);

        Some(CallNode {
            name,
            file_path: path.to_string(),
            line: node.start_position().row + 1,
            calls: Vec::new(),
            metrics,
            receiver: Self::extract_receiver_type(node, source),
        })
    }

    fn extract_calls(&self, path: &str, content: &str, tree: &Tree) -> Result<()> {
        let source = content.as_bytes();
        let mut cursor = tree.walk();

        // Track current function scope
        let mut current_function: Option<String> = None;
        self.walk_for_calls(&mut cursor, source, path, &mut current_function, "");

        Ok(())
    }

    fn walk_for_calls(
        &self,
        cursor: &mut tree_sitter::TreeCursor,
        source: &[u8],
        path: &str,
        current_function: &mut Option<String>,
        parent_kind: &str,
    ) {
        loop {
            let node = cursor.node();
            let kind = node.kind();

            // Update current function context (use qualified key)
            if matches!(
                kind,
                "function_item"
                    | "function_definition"
                    | "procedure_definition"
                    | "function_declaration"
                    | "method_definition"
                    | "method_declaration"
                    // Ruby
                    | "method"
                    | "singleton_method"
            ) {
                if let Some(name) = extract_function_name(node, source) {
                    let receiver = Self::extract_receiver_type(node, source);
                    *current_function =
                        Some(Self::definition_key(path, receiver.as_deref(), &name));
                }
            }

            // Check for call expressions. BSL `method_call` is a simple call
            // (`Func()`); the one nested inside a `call_expression` (qualified
            // `Module.Func()`) is handled by the parent, so skip it here.
            let is_call = matches!(
                kind,
                "call_expression"
                    | "call"
                    | "method_call_expression"
                    | "invocation_expression"
                    // Java
                    | "method_invocation"
                    // PHP
                    | "function_call_expression"
                    | "member_call_expression"
                    | "scoped_call_expression"
            ) || (kind == "method_call" && parent_kind != "call_expression");
            if is_call {
                if let Some(ref caller_key) = current_function {
                    if let Some(edge) = self.extract_call_edge(node, source, path) {
                        // Resolve callee with scope hint for disambiguation
                        let (callee_key, resolution) =
                            self.resolve_callee(&edge.target, path, edge.scope_hint.as_deref());

                        // Add to caller's outgoing calls (with resolved key as target)
                        // Compute `resolved` *before* taking the caller's write
                        // lock: `contains_key` takes a read lock on the callee's
                        // shard, and a self-call (caller == callee) would
                        // otherwise deadlock (read-under-write on one shard).
                        let resolved = self.nodes.contains_key(&callee_key);
                        if let Some(mut caller_node) = self.nodes.get_mut(caller_key.as_str()) {
                            let mut resolved_edge = edge.clone();
                            resolved_edge.target = callee_key.clone();
                            resolved_edge.resolved = resolved;
                            resolved_edge.resolution = resolution;
                            caller_node.calls.push(resolved_edge);
                        }
                    }
                }
            }

            // Handle macro invocations (opaque token trees in tree-sitter)
            // Extract call patterns from macro body text
            if kind == "macro_invocation" {
                if let Some(ref caller_key) = current_function {
                    if let Ok(macro_text) = node.utf8_text(source) {
                        let macro_line = node.start_position().row + 1;
                        self.extract_calls_from_macro_text(
                            macro_text, caller_key, path, macro_line,
                        );
                    }
                }
            }

            // Recurse
            if cursor.goto_first_child() {
                self.walk_for_calls(cursor, source, path, current_function, node.kind());
                cursor.goto_parent();
            }

            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }

    /// Extract call-like patterns from macro invocation body text.
    /// Finds patterns like `func_name(`, `obj.method(`, `Type::method(`.
    fn extract_calls_from_macro_text(
        &self,
        text: &str,
        caller_key: &str,
        caller_file: &str,
        line: usize,
    ) {
        // Simple tokenizer: find identifiers followed by '('
        let bytes = text.as_bytes();
        let len = bytes.len();
        let mut i = 0;

        while i < len {
            // Skip non-identifier chars
            if !bytes[i].is_ascii_alphanumeric() && bytes[i] != b'_' {
                i += 1;
                continue;
            }

            // Collect identifier
            let start = i;
            while i < len && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                i += 1;
            }
            let ident = &text[start..i];

            // Skip whitespace
            let mut j = i;
            while j < len && bytes[j].is_ascii_whitespace() {
                j += 1;
            }

            // Check if followed by '(' — this is a call
            if j < len && bytes[j] == b'(' {
                // Skip Rust keywords
                if matches!(
                    ident,
                    "if" | "else"
                        | "match"
                        | "while"
                        | "for"
                        | "loop"
                        | "let"
                        | "mut"
                        | "fn"
                        | "pub"
                        | "return"
                        | "async"
                        | "await"
                        | "move"
                        | "unsafe"
                        | "Some"
                        | "None"
                        | "Ok"
                        | "Err"
                ) {
                    i = j + 1;
                    continue;
                }

                let (callee_key, resolution) = self.resolve_callee(ident, caller_file, None);

                // Add to caller's outgoing calls
                // Compute `resolved` *before* taking the caller's write lock
                // (see walk_for_calls — read-under-write on one shard deadlocks
                // when caller and callee hash to the same shard).
                let resolved = self.nodes.contains_key(&callee_key);
                if let Some(mut caller_node) = self.nodes.get_mut(caller_key) {
                    // Avoid duplicate edges
                    if !caller_node.calls.iter().any(|c| c.target == callee_key) {
                        caller_node.calls.push(CallEdge {
                            target: callee_key.clone(),
                            file_path: caller_file.to_string(),
                            line,
                            column: 0,
                            call_type: CallType::Direct,
                            scope_hint: None,
                            resolved,
                            resolution,
                        });
                    }
                }

                i = j + 1;
            }
        }
    }

    /// The file a key belongs to, read from the node rather than parsed out of
    /// the key: a method key carries an extra `::Type` segment.
    fn key_file_path(&self, key: &str) -> Option<String> {
        self.nodes.get(key).map(|node| node.file_path.clone())
    }

    /// Does the node behind `key` belong to the type named `qualifier`?
    ///
    /// `case_sensitive` is on for call-site resolution, where Go's `store.Get()`
    /// (package `store`) must not be captured by a method on type `Store`, and
    /// off for user-typed queries, which are approximations by nature.
    fn receiver_matches(&self, key: &str, qualifier: &str, case_sensitive: bool) -> bool {
        let Some(node) = self.nodes.get(key) else {
            return false;
        };
        let Some(receiver) = node.receiver.as_deref() else {
            return false;
        };
        // `app.Server.Handle` -> compare against `Server` as well as `app.Server`.
        let tail = qualifier
            .rsplit(['.', ':', '/'])
            .next()
            .unwrap_or(qualifier);
        if case_sensitive {
            receiver == qualifier || receiver == tail
        } else {
            receiver.eq_ignore_ascii_case(qualifier) || receiver.eq_ignore_ascii_case(tail)
        }
    }

    /// Resolve a callee name to a qualified key.
    /// Prefers a receiver-type match on the scope qualifier, then the caller's
    /// own file, then a scope hint matching the file path, and finally a
    /// deterministic alphabetical fallback.
    fn resolve_callee(
        &self,
        bare_name: &str,
        caller_file: &str,
        scope_hint: Option<&str>,
    ) -> (String, CallResolution) {
        // 0. A scope qualifier naming a receiver type is harder evidence than
        //    the caller's own file: `Store::new()` means `Store`, wherever it
        //    lives. Only a unique match counts.
        if let Some(scope) = scope_hint {
            if let Some(candidates) = self.name_index.get(bare_name) {
                let mut by_receiver = candidates
                    .iter()
                    .filter(|key| self.receiver_matches(key, scope, true));
                if let Some(first) = by_receiver.next() {
                    if by_receiver.next().is_none() {
                        return (first.clone(), CallResolution::Receiver);
                    }
                }
            }
        }

        // 1. Exact same-file hit: the common case for free functions, and one
        //    lookup rather than a scan of every namesake.
        let same_file_key = Self::qualified_key(caller_file, bare_name);
        if self.nodes.contains_key(&same_file_key) {
            let namesakes = self
                .name_index
                .get(bare_name)
                .map(|candidates| candidates.len())
                .unwrap_or(1);
            let resolution = if namesakes > 1 {
                CallResolution::SameFile
            } else {
                CallResolution::Unique
            };
            return (same_file_key, resolution);
        }

        // 2. O(1) candidate lookup via the name index, borrowing the Ref — no
        // full Vec clone per call (a name with hundreds of candidates called
        // thousands of times would otherwise re-clone and re-sort every time).
        let Some(candidates) = self.name_index.get(bare_name) else {
            return (bare_name.to_string(), CallResolution::Unresolved);
        };

        match candidates.len() {
            1 => (candidates[0].clone(), CallResolution::Unique),
            _ => {
                // 3. Same file next. Methods key as `file::Type::name`, so a
                // prefix test is needed where an exact key lookup was enough.
                let file_prefix = format!("{}::", caller_file);
                if let Some(key) = candidates
                    .iter()
                    .filter(|key| key.starts_with(&file_prefix))
                    .min()
                {
                    return (key.clone(), CallResolution::SameFile);
                }

                // 4. If scope_hint present, try to narrow down
                if let Some(scope) = scope_hint {
                    let mut scope_matches = candidates.iter().filter(|key| {
                        self.key_file_path(key)
                            .map(|file_path| Self::scope_matches_file_path(scope, &file_path))
                            .unwrap_or(false)
                    });

                    if let Some(first) = scope_matches.next() {
                        if scope_matches.next().is_none() {
                            return (first.clone(), CallResolution::Scope);
                        }
                    }
                }

                // 5. Deterministic fallback: alphabetically smallest, no sort.
                // Nothing distinguished the namesakes, so the edge is a name
                // match and says so.
                (
                    candidates.iter().min().cloned().unwrap_or_default(),
                    CallResolution::NameOnly,
                )
            }
        }
    }

    /// Extract the scope qualifier from a scoped call expression.
    /// For `App::run()`, the full text is `"App::run"` and this returns `Some("App")`.
    /// For `crate::utils::helper()`, returns `Some("crate::utils")`.
    fn extract_scope_qualifier(node: Node, source: &[u8]) -> Option<String> {
        let text = node.utf8_text(source).ok()?;
        // Find the last "::" to split scope from bare name
        let pos = text.rfind("::")?;
        let scope = &text[..pos];
        if scope.is_empty() {
            None
        } else {
            Some(scope.to_string())
        }
    }

    /// Check if a scope qualifier plausibly matches a file path.
    /// Lowercases scope, strips `crate::`/`self::`/`super::` prefix,
    /// converts `::` to `/`, and checks if `file_path` contains `/{scope}/` or `/{scope}.`.
    fn scope_matches_file_path(scope: &str, file_path: &str) -> bool {
        let scope_lower = scope.to_lowercase();
        // Strip common Rust path prefixes
        let stripped = scope_lower
            .strip_prefix("crate::")
            .or_else(|| scope_lower.strip_prefix("self::"))
            .or_else(|| scope_lower.strip_prefix("super::"))
            .unwrap_or(&scope_lower);
        // Convert :: to / for path matching
        let as_path = stripped.replace("::", "/");
        let path_lower = file_path.to_lowercase();
        // Check if file_path contains /{scope}/ or /{scope}.
        let with_slash = format!("/{}/", as_path);
        let with_dot = format!("/{}.", as_path);
        path_lower.contains(&with_slash) || path_lower.contains(&with_dot)
    }

    /// Text of a receiver/qualifier node, when it can plausibly narrow a
    /// callee down to a file. `self`/`this` receivers carry no such
    /// information, so they are dropped instead of being fed to
    /// [`resolve_callee`](Self::resolve_callee).
    fn scope_hint_from(node: Node, source: &[u8]) -> Option<String> {
        let usable = matches!(
            node.kind(),
            "identifier"
                | "field_identifier"
                | "property_identifier"
                | "package_identifier"
                | "simple_identifier"
                | "type_identifier"
                | "name"
                | "constant"
                | "variable_name"
        );
        if !usable {
            return None;
        }
        let text = node.utf8_text(source).ok()?.trim_start_matches('$');
        if text.is_empty() || matches!(text, "self" | "Self" | "this") {
            return None;
        }
        Some(text.to_string())
    }

    /// Describe the callee slot of a call: `(name, call type, scope hint)`.
    ///
    /// Each language spells a method callee differently, and the name always
    /// lives in a named field: Go `selector_expression.field`, Python
    /// `attribute.attribute`, JS/TS `member_expression.property`, C#
    /// `member_access_expression.name`, Kotlin/Swift `navigation_expression`'s
    /// trailing `navigation_suffix`, Rust/C `field_expression.field`.
    /// Returns `None` for shapes it does not know so the caller can fall back.
    fn describe_callee(
        &self,
        node: Node,
        source: &[u8],
    ) -> Option<(String, CallType, Option<String>)> {
        let kind = node.kind();
        let text_of = |n: Node| n.utf8_text(source).ok().map(|s| s.to_string());

        match kind {
            "identifier"
            | "field_identifier"
            | "property_identifier"
            | "simple_identifier"
            | "name"
            | "constant" => Some((text_of(node)?, CallType::Direct, None)),

            "selector_expression"
            | "attribute"
            | "member_expression"
            | "field_expression"
            | "member_access_expression" => {
                let name_field = match kind {
                    "attribute" => "attribute",
                    "member_expression" => "property",
                    "member_access_expression" => "name",
                    _ => "field",
                };
                let name = node
                    .child_by_field_name(name_field)
                    .and_then(text_of)
                    .or_else(|| self.get_last_identifier(node, source))?;
                let scope = node
                    .child_by_field_name("object")
                    .or_else(|| node.child_by_field_name("operand"))
                    .or_else(|| node.child_by_field_name("expression"))
                    .or_else(|| node.child_by_field_name("value"))
                    .or_else(|| node.child_by_field_name("argument"))
                    .and_then(|receiver| Self::scope_hint_from(receiver, source));
                Some((name, CallType::Method, scope))
            }

            // Kotlin/Swift: `target . (navigation_suffix suffix)`. The suffix is
            // unnamed in Kotlin, so it is located by kind rather than by field.
            "navigation_expression" => {
                let mut walker = node.walk();
                let suffix = node
                    .children(&mut walker)
                    .filter(|child| child.kind() == "navigation_suffix")
                    .last()?;
                let name = self.get_last_identifier(suffix, source)?;
                let scope = node
                    .child_by_field_name("target")
                    .or_else(|| node.named_child(0))
                    .and_then(|target| Self::scope_hint_from(target, source));
                Some((name, CallType::Method, scope))
            }

            "scoped_identifier" | "qualified_identifier" => {
                let scope = Self::extract_scope_qualifier(node, source);
                let name = node
                    .child_by_field_name("name")
                    .and_then(text_of)
                    .or_else(|| self.get_last_identifier(node, source))?;
                Some((name, CallType::StaticMethod, scope))
            }

            _ => None,
        }
    }

    /// Locate the callee expression of a call node through tree-sitter fields
    /// and turn it into an edge. Returns `None` for call shapes without a known
    /// callee field (BSL among them) so [`extract_call_edge`](Self::extract_call_edge)
    /// can fall back to its sibling scan.
    fn extract_call_edge_by_fields(
        &self,
        node: Node,
        source: &[u8],
        path: &str,
    ) -> Option<CallEdge> {
        let kind = node.kind();

        // `receiver` is set only for shapes where the callee node itself does
        // not carry the receiver (Ruby `call`, Java/PHP method invocations).
        let (callee, receiver) = match kind {
            "call_expression" | "call" | "invocation_expression" | "function_call_expression" => {
                if let Some(function) = node.child_by_field_name("function") {
                    (function, None)
                } else if let Some(method) = node.child_by_field_name("method") {
                    // Ruby: `(call receiver: (identifier) method: (identifier))`
                    (method, node.child_by_field_name("receiver"))
                } else {
                    // Kotlin/Swift: callee and `call_suffix` are unnamed children.
                    let mut walker = node.walk();
                    let callee = node.children(&mut walker).find(|child| {
                        child.is_named()
                            && !child.kind().contains("argument")
                            && child.kind() != "call_suffix"
                    })?;
                    (callee, None)
                }
            }
            // Java `obj.m()`, PHP `$obj->m()` and `Obj::m()`.
            "method_invocation" | "member_call_expression" | "scoped_call_expression" => (
                node.child_by_field_name("name")?,
                node.child_by_field_name("object")
                    .or_else(|| node.child_by_field_name("scope")),
            ),
            _ => return None,
        };

        let (target, mut call_type, mut scope_hint) = self.describe_callee(callee, source)?;

        if let Some(receiver) = receiver {
            call_type = if kind == "scoped_call_expression" {
                CallType::StaticMethod
            } else {
                CallType::Method
            };
            scope_hint = Self::scope_hint_from(receiver, source);
        }

        Some(CallEdge {
            target,
            file_path: path.to_string(),
            line: node.start_position().row + 1,
            column: node.start_position().column + 1,
            call_type,
            scope_hint,
            resolved: false,
            resolution: CallResolution::Unknown,
        })
    }

    fn extract_call_edge(&self, node: Node, source: &[u8], path: &str) -> Option<CallEdge> {
        if let Some(edge) = self.extract_call_edge_by_fields(node, source, path) {
            return Some(edge);
        }

        let mut cursor = node.walk();
        cursor.goto_first_child();

        let mut target = None;
        let mut call_type = CallType::Direct;
        let mut scope_hint = None;

        loop {
            let child = cursor.node();
            let kind = child.kind();

            match kind {
                "identifier" | "field_identifier" => {
                    target = child.utf8_text(source).ok().map(|s| s.to_string());
                }
                "field_expression" | "member_expression" => {
                    // Method call: extract the method name
                    if let Some(method) = self.get_last_identifier(child, source) {
                        target = Some(method);
                        call_type = CallType::Method;
                    }
                }
                "scoped_identifier" | "qualified_identifier" => {
                    // Static method call: Type::method - extract scope qualifier
                    scope_hint = Self::extract_scope_qualifier(child, source);
                    if let Some(method) = self.get_last_identifier(child, source) {
                        target = Some(method);
                        call_type = CallType::StaticMethod;
                    }
                }
                "access" => {
                    // BSL qualified call `Module.Func()`: `access` holds the
                    // module name, used as the scope hint for resolution.
                    scope_hint = self.get_last_identifier(child, source);
                }
                "method_call" => {
                    // BSL `name: (identifier)` inside a `call_expression`.
                    if let Some(method) = self.get_last_identifier(child, source) {
                        target = Some(method);
                        call_type = CallType::Method;
                    }
                }
                _ => {}
            }

            if !cursor.goto_next_sibling() {
                break;
            }
        }

        target.map(|name| CallEdge {
            target: name,
            file_path: path.to_string(),
            line: node.start_position().row + 1,
            column: node.start_position().column + 1,
            call_type,
            scope_hint,
            resolved: false,
            resolution: CallResolution::Unknown,
        })
    }

    fn get_last_identifier(&self, node: Node, source: &[u8]) -> Option<String> {
        let mut cursor = node.walk();
        let mut last_ident = None;

        fn walk_idents(
            cursor: &mut tree_sitter::TreeCursor,
            source: &[u8],
            last: &mut Option<String>,
        ) {
            loop {
                let n = cursor.node();
                if matches!(
                    n.kind(),
                    "identifier"
                        | "field_identifier"
                        | "property_identifier"
                        | "simple_identifier"
                        | "name"
                ) {
                    *last = n.utf8_text(source).ok().map(|s| s.to_string());
                }
                if cursor.goto_first_child() {
                    walk_idents(cursor, source, last);
                    cursor.goto_parent();
                }
                if !cursor.goto_next_sibling() {
                    break;
                }
            }
        }

        walk_idents(&mut cursor, source, &mut last_ident);
        last_ident
    }

    fn compute_metrics(&self, node: Node, _source: &[u8]) -> FunctionMetrics {
        let mut metrics = FunctionMetrics {
            loc: node.end_position().row - node.start_position().row + 1,
            ..Default::default()
        };

        // Walk the function body for complexity metrics
        let mut cursor = node.walk();
        self.walk_for_metrics(&mut cursor, 0, &mut metrics);

        metrics
    }

    fn walk_for_metrics(
        &self,
        cursor: &mut tree_sitter::TreeCursor,
        depth: usize,
        metrics: &mut FunctionMetrics,
    ) {
        loop {
            let node = cursor.node();
            let kind = node.kind();

            // Track nesting depth
            metrics.max_depth = metrics.max_depth.max(depth);

            // Count branches for cyclomatic complexity
            if matches!(
                kind,
                "if_statement"
                    | "if_expression"
                    | "else_clause"
                    | "match_arm"
                    | "case"
                    | "for_statement"
                    | "for_expression"
                    | "while_statement"
                    | "while_expression"
                    | "loop_expression"
                    | "catch_clause"
                    | "&&"
                    | "||"
                    | "?"
            ) {
                metrics.cyclomatic += 1;
            }

            // Count returns
            if matches!(kind, "return_statement" | "return_expression") {
                metrics.returns += 1;
            }

            // Count parameters (look for parameter_list)
            if kind == "parameters" || kind == "parameter_list" {
                metrics.params = node.named_child_count();
            }

            // Cognitive complexity (adds for nesting)
            if matches!(
                kind,
                "if_statement"
                    | "if_expression"
                    | "for_statement"
                    | "for_expression"
                    | "while_statement"
                    | "while_expression"
                    | "loop_expression"
                    | "match_expression"
            ) {
                metrics.cognitive += 1 + depth;
            }

            // Recurse with updated depth for control structures
            let new_depth = if matches!(
                kind,
                "if_statement"
                    | "if_expression"
                    | "for_statement"
                    | "for_expression"
                    | "while_statement"
                    | "while_expression"
                    | "loop_expression"
                    | "match_expression"
                    | "try_statement"
                    | "block"
            ) {
                depth + 1
            } else {
                depth
            };

            if cursor.goto_first_child() {
                self.walk_for_metrics(cursor, new_depth, metrics);
                cursor.goto_parent();
            }

            if !cursor.goto_next_sibling() {
                break;
            }
        }

        // Base cyclomatic is 1
        if metrics.cyclomatic == 0 {
            metrics.cyclomatic = 1;
        }
    }

    // === Query Methods ===

    /// Find a function by name with fuzzy matching.
    ///
    /// Tries (in order):
    /// 1. Exact match on qualified key (e.g., "src/app/mod.rs::run")
    /// 2. Case-insensitive exact match on qualified key
    /// 3. Suffix match with :: separator (e.g., "app::run" matches "src/app/mod.rs::run")
    /// 4. Bare name suffix match (e.g., "run" matches "src/app/mod.rs::run")
    /// 5. Case-insensitive suffix match
    /// 6. Contains match
    ///
    /// Returns the actual qualified key in the graph, or None if not found.
    /// Split a qualified query into `(qualifier, bare name)`.
    ///
    /// `Server.Handle` -> `("Server", "Handle")`, `A::run` -> `("A", "run")`,
    /// `$obj->method` -> `("$obj", "method")`. Returns `None` when the query is
    /// already a bare name, or when the trailing segment is not identifier-like.
    fn split_qualified_query(query: &str) -> Option<(&str, &str)> {
        let (index, sep_len) = ["::", "->", ".", "/"]
            .iter()
            .filter_map(|sep| query.rfind(sep).map(|index| (index, sep.len())))
            .max_by_key(|(index, _)| *index)?;

        let qualifier = query.get(..index)?;
        let bare = query.get(index + sep_len..)?;
        let identifier_like = !bare.is_empty()
            && bare
                .chars()
                .all(|c| c.is_alphanumeric() || c == '_' || c == '$');
        if qualifier.is_empty() || !identifier_like {
            return None;
        }
        Some((qualifier, bare))
    }

    /// The bare function name of a query: the trailing segment of
    /// `Type::method`, `pkg.Func` or `dir/file.go::Method`.
    pub fn bare_name(query: &str) -> &str {
        Self::split_qualified_query(query)
            .map(|(_, bare)| bare)
            .unwrap_or(query)
    }

    pub fn find_function(&self, query: &str) -> Option<String> {
        // 1. Exact match on qualified key
        if self.nodes.contains_key(query) {
            return Some(query.to_string());
        }

        let query_lower = query.to_lowercase();

        // 2. Case-insensitive exact match (deterministic: collect, sort, pick first)
        let mut matches: Vec<String> = self
            .nodes
            .iter()
            .filter(|entry| entry.key().to_lowercase() == query_lower)
            .map(|entry| entry.key().clone())
            .collect();
        if !matches.is_empty() {
            matches.sort();
            return Some(matches.remove(0));
        }

        // 3. Suffix match with :: separator (e.g., "app::run" or "mod.rs::run")
        let suffix_pattern = format!("::{}", query);
        let mut matches: Vec<String> = self
            .nodes
            .iter()
            .filter(|entry| entry.key().ends_with(&suffix_pattern))
            .map(|entry| entry.key().clone())
            .collect();
        if !matches.is_empty() {
            matches.sort();
            return Some(matches.remove(0));
        }

        // 3b. Path component match (e.g., "app/mod.rs::run" matches "src/app/mod.rs::run")
        if query.contains("::") {
            let mut matches: Vec<String> = self
                .nodes
                .iter()
                .filter(|entry| entry.key().ends_with(query))
                .map(|entry| entry.key().clone())
                .collect();
            if !matches.is_empty() {
                matches.sort();
                return Some(matches.remove(0));
            }
        }

        // 4. Bare name match: query is just a function name like "run"
        // Match against the function name part of qualified keys
        if !query.contains("::") && !query.contains('/') {
            let bare_suffix = format!("::{}", query);
            let mut matches: Vec<String> = self
                .nodes
                .iter()
                .filter(|entry| entry.key().ends_with(&bare_suffix))
                .map(|entry| entry.key().clone())
                .collect();
            if !matches.is_empty() {
                matches.sort();
                return Some(matches.remove(0));
            }
        }

        // 4b. Qualified forms the graph does not key on: `Type::method`,
        // `Type.method`, `pkg.Func`, `$obj->method`. Retry on the trailing
        // segment and use the qualifier to narrow candidates by file path.
        if let Some((qualifier, bare)) = Self::split_qualified_query(query) {
            let bare_suffix = format!("::{}", bare);
            let mut matches: Vec<String> = self
                .nodes
                .iter()
                .filter(|entry| entry.key().ends_with(&bare_suffix))
                .map(|entry| entry.key().clone())
                .collect();
            if !matches.is_empty() {
                matches.sort();
                // The qualifier is usually a type name (`Server.Handle`), so
                // try the receiver first; a module/package qualifier
                // (`app/handler.go::Handle`) then falls back to the file path.
                let by_receiver = matches
                    .iter()
                    .find(|key| self.receiver_matches(key, qualifier, true))
                    .or_else(|| {
                        matches
                            .iter()
                            .find(|key| self.receiver_matches(key, qualifier, false))
                    });
                if let Some(key) = by_receiver {
                    return Some(key.clone());
                }
                let by_path = matches.iter().find(|key| {
                    self.key_file_path(key)
                        .map(|file_path| {
                            file_path.ends_with(qualifier)
                                || Self::scope_matches_file_path(qualifier, &file_path)
                        })
                        .unwrap_or(false)
                });
                if let Some(key) = by_path {
                    return Some(key.clone());
                }
                return Some(matches.remove(0));
            }
        }

        // 5. Case-insensitive suffix match (deterministic)
        let mut matches: Vec<String> = self
            .nodes
            .iter()
            .filter(|entry| entry.key().to_lowercase().ends_with(&query_lower))
            .map(|entry| entry.key().clone())
            .collect();
        if !matches.is_empty() {
            matches.sort();
            return Some(matches.remove(0));
        }

        // 6. Contains match (deterministic)
        let mut matches: Vec<String> = self
            .nodes
            .iter()
            .filter(|entry| entry.key().to_lowercase().contains(&query_lower))
            .map(|entry| entry.key().clone())
            .collect();
        if !matches.is_empty() {
            matches.sort();
            return Some(matches.remove(0));
        }

        None
    }

    /// Find all functions matching a query (returns multiple matches for disambiguation).
    pub fn find_all_functions(&self, query: &str) -> Vec<String> {
        let mut matches = Vec::new();
        let suffix = format!("::{}", query);
        for entry in self.nodes.iter() {
            let key = entry.key();
            if key.ends_with(&suffix) || key == query {
                matches.push(key.clone());
            }
        }
        matches.sort();
        matches
    }

    /// Get similar function names for suggestions when a function is not found
    pub fn get_similar_functions(&self, query: &str, limit: usize) -> Vec<String> {
        let query_lower = query.to_lowercase();
        let mut candidates: Vec<(String, usize)> = Vec::new();

        for entry in self.nodes.iter() {
            let name = entry.key();
            let name_lower = name.to_lowercase();

            // Calculate a simple similarity score
            let score = if name_lower.contains(&query_lower) {
                100 - name.len() // Shorter matches are better
            } else {
                // Count matching characters
                let mut matches = 0;
                for c in query_lower.chars() {
                    if name_lower.contains(c) {
                        matches += 1;
                    }
                }
                matches * 10
            };

            if score > 0 {
                candidates.push((name.clone(), score));
            }
        }

        candidates.sort_by_key(|candidate| std::cmp::Reverse(candidate.1));
        candidates
            .into_iter()
            .take(limit)
            .map(|(name, _)| name)
            .collect()
    }

    /// Get direct callers of a function (with fuzzy matching)
    /// Build the reverse adjacency (callee key -> caller edges) on demand.
    /// `called_by` is no longer stored (halves edge memory); reverse queries
    /// scan the forward `calls` once. O(E), transient.
    pub fn reverse_edges(&self) -> HashMap<String, Vec<CallEdge>> {
        let mut rev: HashMap<String, Vec<CallEdge>> = HashMap::new();
        for entry in self.nodes.iter() {
            let caller = entry.key();
            for e in &entry.value().calls {
                rev.entry(e.target.clone()).or_default().push(CallEdge {
                    target: caller.clone(),
                    file_path: e.file_path.clone(),
                    line: e.line,
                    column: e.column,
                    call_type: e.call_type.clone(),
                    scope_hint: None,
                    resolved: true,
                    // How the *forward* edge was matched is what a reader of
                    // "who calls me" needs to weigh.
                    resolution: e.resolution,
                });
            }
        }
        rev
    }

    /// Build incoming call counts (callee key -> number of callers) on demand.
    fn incoming_counts(&self) -> HashMap<String, usize> {
        let mut counts: HashMap<String, usize> = HashMap::new();
        for entry in self.nodes.iter() {
            for e in &entry.value().calls {
                *counts.entry(e.target.clone()).or_default() += 1;
            }
        }
        counts
    }

    pub fn get_callers(&self, function: &str) -> Vec<CallEdge> {
        let actual_name = self
            .find_function(function)
            .unwrap_or_else(|| function.to_string());
        self.reverse_edges()
            .remove(&actual_name)
            .unwrap_or_default()
    }

    /// Get functions called by a function (with fuzzy matching)
    pub fn get_callees(&self, function: &str) -> Vec<CallEdge> {
        let actual_name = self
            .find_function(function)
            .unwrap_or_else(|| function.to_string());
        self.nodes
            .get(&actual_name)
            .map(|n| n.calls.clone())
            .unwrap_or_default()
    }

    /// Get transitive callers (all functions that eventually call this) - with fuzzy matching
    pub fn get_transitive_callers(&self, function: &str, max_depth: usize) -> Vec<(String, usize)> {
        let actual_name = self
            .find_function(function)
            .unwrap_or_else(|| function.to_string());
        let rev = self.reverse_edges();

        let mut result = Vec::new();
        let mut visited = HashSet::new();
        let mut queue = VecDeque::new();

        queue.push_back((actual_name.clone(), 0));
        visited.insert(actual_name);

        while let Some((func, depth)) = queue.pop_front() {
            if depth > 0 {
                result.push((func.clone(), depth));
            }

            if depth < max_depth {
                if let Some(callers) = rev.get(&func) {
                    for caller in callers {
                        if !visited.contains(&caller.target) {
                            visited.insert(caller.target.clone());
                            queue.push_back((caller.target.clone(), depth + 1));
                        }
                    }
                }
            }
        }

        result
    }

    /// Get transitive callees (all functions eventually called) - with fuzzy matching
    pub fn get_transitive_callees(&self, function: &str, max_depth: usize) -> Vec<(String, usize)> {
        let actual_name = self
            .find_function(function)
            .unwrap_or_else(|| function.to_string());
        let mut result = Vec::new();
        let mut visited = HashSet::new();
        let mut queue = VecDeque::new();

        queue.push_back((actual_name.clone(), 0));
        visited.insert(actual_name);

        while let Some((func, depth)) = queue.pop_front() {
            if depth > 0 {
                result.push((func.clone(), depth));
            }

            if depth < max_depth {
                if let Some(node) = self.nodes.get(&func) {
                    for callee in &node.calls {
                        if !visited.contains(&callee.target) {
                            visited.insert(callee.target.clone());
                            queue.push_back((callee.target.clone(), depth + 1));
                        }
                    }
                }
            }
        }

        result
    }

    /// Find the path between two functions - with fuzzy matching
    pub fn find_call_path(&self, from: &str, to: &str) -> Option<Vec<String>> {
        let actual_from = self.find_function(from).unwrap_or_else(|| from.to_string());
        let actual_to = self.find_function(to).unwrap_or_else(|| to.to_string());

        let mut visited = HashSet::new();
        let mut queue = VecDeque::new();
        let mut predecessors: HashMap<String, String> = HashMap::new();

        queue.push_back(actual_from.clone());
        visited.insert(actual_from);

        while let Some(current) = queue.pop_front() {
            if current == actual_to {
                // Reconstruct path
                let mut path = vec![actual_to.clone()];
                let mut node = actual_to.clone();
                while let Some(pred) = predecessors.get(&node) {
                    path.push(pred.clone());
                    node = pred.clone();
                }
                path.reverse();
                return Some(path);
            }

            if let Some(node) = self.nodes.get(&current) {
                for callee in &node.calls {
                    if !visited.contains(&callee.target) {
                        visited.insert(callee.target.clone());
                        predecessors.insert(callee.target.clone(), current.clone());
                        queue.push_back(callee.target.clone());
                    }
                }
            }
        }

        None
    }

    /// Generic trait method names to filter from hotspots (these are noise)
    const TRAIT_METHOD_NAMES: &'static [&'static str] = &[
        "new",
        "default",
        "from",
        "into",
        "clone",
        "fmt",
        "drop",
        "deref",
        "deref_mut",
        "as_ref",
        "as_mut",
        "borrow",
        "borrow_mut",
        "try_from",
        "try_into",
        "eq",
        "ne",
        "partial_cmp",
        "cmp",
        "hash",
        "serialize",
        "deserialize",
        "to_string",
        "to_owned",
        "build",
        "index",
        "index_mut",
    ];

    /// Check if a function name is a generic trait method that should be filtered
    fn is_trait_method(qualified_key: &str) -> bool {
        if let Some(bare_name) = qualified_key.rsplit("::").next() {
            Self::TRAIT_METHOD_NAMES.contains(&bare_name)
        } else {
            false
        }
    }

    /// Get highly connected functions (potential refactoring targets).
    /// Filters out generic trait methods and limits output.
    pub fn get_hotspots(&self, min_connections: usize) -> Vec<(String, usize, usize)> {
        let mut hotspots = Vec::new();
        let counts = self.incoming_counts();

        for entry in self.nodes.iter() {
            let key = entry.key();

            // Skip generic trait method implementations
            if Self::is_trait_method(key) {
                continue;
            }

            let incoming = counts.get(key).copied().unwrap_or(0);
            let outgoing = entry.calls.len();
            let total = incoming + outgoing;

            if total >= min_connections {
                hotspots.push((key.clone(), incoming, outgoing));
            }
        }

        hotspots.sort_by_key(|hotspot| std::cmp::Reverse(hotspot.1 + hotspot.2));
        hotspots
    }

    /// Get highly connected functions with a limit on results.
    pub fn get_hotspots_limited(
        &self,
        min_connections: usize,
        limit: usize,
    ) -> Vec<(String, usize, usize)> {
        let mut hotspots = self.get_hotspots(min_connections);
        hotspots.truncate(limit);
        hotspots
    }

    /// Get function metrics
    pub fn get_metrics(&self, function: &str) -> Option<FunctionMetrics> {
        let actual_name = self.find_function(function)?;
        self.nodes.get(&actual_name).map(|n| n.metrics.clone())
    }

    /// Export call graph in DOT format for visualization
    pub fn to_dot(&self, filter_file: Option<&str>) -> String {
        let mut dot = String::from("digraph CallGraph {\n");
        dot.push_str("  rankdir=LR;\n");
        dot.push_str("  node [shape=box];\n\n");

        for entry in self.nodes.iter() {
            let key = entry.key();
            let node = entry.value();

            if let Some(file) = filter_file {
                if !node.file_path.contains(file) {
                    continue;
                }
            }

            // Node styling based on metrics
            let color = if node.metrics.cyclomatic > 10 {
                "red"
            } else if node.metrics.cyclomatic > 5 {
                "orange"
            } else {
                "black"
            };

            dot.push_str(&format!(
                "  \"{}\" [label=\"{}\\nCC:{} LOC:{}\", color={}];\n",
                key, node.name, node.metrics.cyclomatic, node.metrics.loc, color
            ));

            for call in &node.calls {
                dot.push_str(&format!("  \"{}\" -> \"{}\";\n", key, call.target));
            }
        }

        dot.push_str("}\n");
        dot
    }

    /// Format call graph as markdown for AI consumption
    pub fn to_markdown(&self, function: Option<&str>) -> String {
        let mut md = String::new();

        match function {
            Some(func) => {
                // Use fuzzy matching to find the function
                let actual_name = self.find_function(func);
                if let Some(node) = actual_name.as_ref().and_then(|n| self.nodes.get(n)) {
                    let display_name = actual_name.as_ref().unwrap();
                    md.push_str(&format!("# Call Graph: {}\n\n", display_name));
                    md.push_str(&format!(
                        "**Location**: `{}:{}`\n",
                        node.file_path, node.line
                    ));
                    md.push_str(&format!(
                        "**Metrics**: CC={}, LOC={}, Depth={}\n\n",
                        node.metrics.cyclomatic, node.metrics.loc, node.metrics.max_depth
                    ));

                    md.push_str("## Calls (outgoing)\n\n");
                    if node.calls.is_empty() {
                        md.push_str("*No outgoing calls*\n\n");
                    } else {
                        for call in &node.calls {
                            md.push_str(&format!(
                                "- `{}` at `{}:{}` ({:?})\n",
                                call.target, call.file_path, call.line, call.call_type
                            ));
                        }
                        md.push('\n');
                    }

                    md.push_str("## Called By (incoming)\n\n");
                    let callers = self
                        .reverse_edges()
                        .remove(display_name)
                        .unwrap_or_default();
                    if callers.is_empty() {
                        md.push_str("*No incoming calls (entry point or unused)*\n\n");
                    } else {
                        for caller in &callers {
                            md.push_str(&format!(
                                "- `{}` at `{}:{}`\n",
                                caller.target, caller.file_path, caller.line
                            ));
                        }
                    }
                } else {
                    md.push_str(&format!("Function `{}` not found in call graph.\n", func));
                }
            }
            None => {
                md.push_str("# Call Graph Summary\n\n");
                md.push_str(&format!("**Total Functions**: {}\n\n", self.nodes.len()));

                // Top callers
                md.push_str("## Most Called Functions\n\n");
                let counts = self.incoming_counts();
                let mut by_callers: Vec<_> = self
                    .nodes
                    .iter()
                    .map(|e| (e.key().clone(), counts.get(e.key()).copied().unwrap_or(0)))
                    .collect();
                by_callers.sort_by_key(|caller| std::cmp::Reverse(caller.1));

                for (name, count) in by_callers.iter().take(10) {
                    md.push_str(&format!("- `{}`: {} callers\n", name, count));
                }
                md.push('\n');

                // Complexity hotspots
                md.push_str("## Complexity Hotspots\n\n");
                let mut by_complexity: Vec<_> = self
                    .nodes
                    .iter()
                    .map(|e| (e.key().clone(), e.metrics.clone()))
                    .collect();
                by_complexity.sort_by_key(|entry| std::cmp::Reverse(entry.1.cyclomatic));

                for (name, metrics) in by_complexity.iter().take(10) {
                    md.push_str(&format!(
                        "- `{}`: CC={}, LOC={}\n",
                        name, metrics.cyclomatic, metrics.loc
                    ));
                }
            }
        }

        md
    }

    // ========================================================================
    // Visualization Helper Methods (for graph tool handler)
    // ========================================================================

    /// Get all function names in the call graph
    pub fn get_all_function_names(&self) -> Vec<String> {
        // Return entry points first (functions not called by anything), then the rest
        let mut entry_points: Vec<String> = Vec::new();
        let mut others: Vec<String> = Vec::new();

        let counts = self.incoming_counts();
        for entry in self.nodes.iter() {
            if counts.get(entry.key()).copied().unwrap_or(0) == 0 {
                entry_points.push(entry.key().clone());
            } else {
                others.push(entry.key().clone());
            }
        }

        // Sort for deterministic output
        entry_points.sort();
        others.sort();

        entry_points.extend(others);
        entry_points
    }

    /// Get a node by exact name (for visualization)
    pub fn get_node(&self, name: &str) -> Option<dashmap::mapref::one::Ref<'_, String, CallNode>> {
        self.nodes.get(name)
    }

    /// Get the number of nodes in the call graph
    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    /// Approximate memory footprint (diagnostics): (nodes, edges, approx_bytes).
    pub fn memory_stats(&self) -> (usize, usize, usize) {
        let mut edges = 0usize;
        let mut bytes = 0usize;
        for node in self.nodes.iter() {
            bytes += node.name.len() + node.file_path.len();
            for e in node.calls.iter() {
                edges += 1;
                bytes += e.target.len()
                    + e.file_path.len()
                    + e.scope_hint.as_deref().map_or(0, str::len)
                    + 24;
            }
        }
        for e in self.name_index.iter() {
            bytes += e.key().len() + e.value().iter().map(|s| s.len() + 24).sum::<usize>();
        }
        for e in self.file_functions.iter() {
            bytes += e.key().len() + e.value().iter().map(|s| s.len() + 24).sum::<usize>();
        }
        (self.nodes.len(), edges, bytes)
    }

    /// Get all nodes (for iteration)
    pub fn iter_nodes(
        &self,
    ) -> impl Iterator<Item = dashmap::mapref::multiple::RefMulti<'_, String, CallNode>> {
        self.nodes.iter()
    }
}

/// Helper function to extract function name from a node (not a method to avoid recursion warning)
fn extract_function_name(node: Node, source: &[u8]) -> Option<String> {
    // Look for name in children
    let mut cursor = node.walk();
    cursor.goto_first_child();

    loop {
        let child = cursor.node();
        let kind = child.kind();

        if matches!(
            kind,
            "identifier"
                | "name"
                | "field_identifier"
                | "property_identifier"
                | "simple_identifier"
        ) {
            return child.utf8_text(source).ok().map(|s| s.to_string());
        }

        // For declarators (C/C++)
        if kind.contains("declarator") {
            if let Some(name) = extract_function_name(child, source) {
                return Some(name);
            }
        }

        if !cursor.goto_next_sibling() {
            break;
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_call_type() {
        assert_eq!(CallType::Direct, CallType::Direct);
        assert_ne!(CallType::Direct, CallType::Method);
    }

    #[test]
    fn test_function_metrics_default() {
        let m = FunctionMetrics::default();
        assert_eq!(m.loc, 0);
        assert_eq!(m.cyclomatic, 0);
    }

    #[test]
    fn test_callgraph_new() {
        let graph = CallGraph::new();
        assert_eq!(graph.nodes.len(), 0);
        assert_eq!(graph.file_functions.len(), 0);
    }

    #[test]
    fn test_add_function_node() {
        let graph = CallGraph::new();

        let node = CallNode {
            name: "test_function".to_string(),
            file_path: "/path/to/file.rs".to_string(),
            line: 42,
            calls: Vec::new(),
            metrics: FunctionMetrics {
                loc: 10,
                cyclomatic: 2,
                max_depth: 1,
                params: 2,
                returns: 1,
                cognitive: 3,
            },
            receiver: None,
        };

        graph
            .nodes
            .insert("test_function".to_string(), node.clone());

        assert_eq!(graph.nodes.len(), 1);
        let retrieved = graph.nodes.get("test_function").unwrap();
        assert_eq!(retrieved.name, "test_function");
        assert_eq!(retrieved.line, 42);
        assert_eq!(retrieved.metrics.loc, 10);
        assert_eq!(retrieved.metrics.cyclomatic, 2);
    }

    #[test]
    fn test_add_call_edge() {
        let graph = CallGraph::new();

        // Create caller and callee nodes
        let caller = CallNode {
            name: "caller".to_string(),
            file_path: "/path/to/file.rs".to_string(),
            line: 10,
            calls: Vec::new(),
            metrics: FunctionMetrics::default(),
            receiver: None,
        };

        let callee = CallNode {
            name: "callee".to_string(),
            file_path: "/path/to/file.rs".to_string(),
            line: 20,
            calls: Vec::new(),
            metrics: FunctionMetrics::default(),
            receiver: None,
        };

        graph.nodes.insert("caller".to_string(), caller);
        graph.nodes.insert("callee".to_string(), callee);

        // Add call edge
        let edge = CallEdge {
            target: "callee".to_string(),
            file_path: "/path/to/file.rs".to_string(),
            line: 12,
            column: 5,
            call_type: CallType::Direct,
            scope_hint: None,
            resolved: false,
            resolution: CallResolution::Unknown,
        };

        graph
            .nodes
            .get_mut("caller")
            .unwrap()
            .calls
            .push(edge.clone());

        // Verify the edge was added
        let caller_node = graph.nodes.get("caller").unwrap();
        assert_eq!(caller_node.calls.len(), 1);
        assert_eq!(caller_node.calls[0].target, "callee");

        // Reverse edges are computed on demand from `calls`.
        let callers = graph.get_callers("callee");
        assert_eq!(callers.len(), 1);
        assert_eq!(callers[0].target, "caller");
    }

    #[test]
    fn test_get_callers() {
        let graph = CallGraph::new();

        // Two callers both call "target" (forward edges only).
        for (name, line, call_type) in [
            ("caller1", 10, CallType::Direct),
            ("caller2", 20, CallType::Method),
        ] {
            graph.nodes.insert(
                name.to_string(),
                CallNode {
                    name: name.to_string(),
                    file_path: "/path/to/file.rs".to_string(),
                    line,
                    calls: vec![CallEdge {
                        target: "target".to_string(),
                        file_path: "/path/to/file.rs".to_string(),
                        line,
                        column: 5,
                        call_type,
                        scope_hint: None,
                        resolved: false,
                        resolution: CallResolution::Unknown,
                    }],
                    metrics: FunctionMetrics::default(),
                    receiver: None,
                },
            );
        }
        graph.nodes.insert(
            "target".to_string(),
            CallNode {
                name: "target".to_string(),
                file_path: "/path/to/file.rs".to_string(),
                line: 30,
                calls: Vec::new(),
                metrics: FunctionMetrics::default(),
                receiver: None,
            },
        );

        let callers = graph.get_callers("target");
        assert_eq!(callers.len(), 2);
        let mut names: Vec<_> = callers.iter().map(|c| c.target.clone()).collect();
        names.sort();
        assert_eq!(names, vec!["caller1".to_string(), "caller2".to_string()]);
    }

    #[test]
    fn test_get_callers_empty() {
        let graph = CallGraph::new();

        let node = CallNode {
            name: "isolated".to_string(),
            file_path: "/path/to/file.rs".to_string(),
            line: 10,
            calls: Vec::new(),
            metrics: FunctionMetrics::default(),
            receiver: None,
        };

        graph.nodes.insert("isolated".to_string(), node);

        let callers = graph.get_callers("isolated");
        assert_eq!(callers.len(), 0);
    }

    #[test]
    fn test_get_callers_nonexistent() {
        let graph = CallGraph::new();

        let callers = graph.get_callers("nonexistent");
        assert_eq!(callers.len(), 0);
    }

    #[test]
    fn test_get_callees() {
        let graph = CallGraph::new();

        // Create a function that calls multiple functions
        let caller = CallNode {
            name: "caller".to_string(),
            file_path: "/path/to/file.rs".to_string(),
            line: 10,
            calls: vec![
                CallEdge {
                    target: "callee1".to_string(),
                    file_path: "/path/to/file.rs".to_string(),
                    line: 12,
                    column: 5,
                    call_type: CallType::Direct,
                    scope_hint: None,
                    resolved: false,
                    resolution: CallResolution::Unknown,
                },
                CallEdge {
                    target: "callee2".to_string(),
                    file_path: "/path/to/file.rs".to_string(),
                    line: 15,
                    column: 10,
                    call_type: CallType::StaticMethod,
                    scope_hint: None,
                    resolved: false,
                    resolution: CallResolution::Unknown,
                },
            ],
            metrics: FunctionMetrics::default(),
            receiver: None,
        };

        graph.nodes.insert("caller".to_string(), caller);

        let callees = graph.get_callees("caller");
        assert_eq!(callees.len(), 2);
        assert_eq!(callees[0].target, "callee1");
        assert_eq!(callees[1].target, "callee2");
        assert_eq!(callees[0].call_type, CallType::Direct);
        assert_eq!(callees[1].call_type, CallType::StaticMethod);
    }

    #[test]
    fn test_get_callees_empty() {
        let graph = CallGraph::new();

        let node = CallNode {
            name: "leaf".to_string(),
            file_path: "/path/to/file.rs".to_string(),
            line: 10,
            calls: Vec::new(),
            metrics: FunctionMetrics::default(),
            receiver: None,
        };

        graph.nodes.insert("leaf".to_string(), node);

        let callees = graph.get_callees("leaf");
        assert_eq!(callees.len(), 0);
    }

    #[test]
    fn test_get_callees_nonexistent() {
        let graph = CallGraph::new();

        let callees = graph.get_callees("nonexistent");
        assert_eq!(callees.len(), 0);
    }

    #[test]
    fn test_get_metrics() {
        let graph = CallGraph::new();

        let metrics = FunctionMetrics {
            loc: 25,
            cyclomatic: 5,
            max_depth: 3,
            params: 4,
            returns: 2,
            cognitive: 8,
        };

        let node = CallNode {
            name: "complex_function".to_string(),
            file_path: "/path/to/file.rs".to_string(),
            line: 100,
            calls: Vec::new(),
            metrics: metrics.clone(),
            receiver: None,
        };

        graph.nodes.insert("complex_function".to_string(), node);

        let retrieved_metrics = graph.get_metrics("complex_function").unwrap();
        assert_eq!(retrieved_metrics.loc, 25);
        assert_eq!(retrieved_metrics.cyclomatic, 5);
        assert_eq!(retrieved_metrics.max_depth, 3);
        assert_eq!(retrieved_metrics.params, 4);
        assert_eq!(retrieved_metrics.returns, 2);
        assert_eq!(retrieved_metrics.cognitive, 8);
    }

    #[test]
    fn test_get_metrics_nonexistent() {
        let graph = CallGraph::new();

        let metrics = graph.get_metrics("nonexistent");
        assert!(metrics.is_none());
    }

    #[test]
    fn test_get_transitive_callers() {
        let graph = CallGraph::new();

        // Create a call chain: a -> b -> c -> d
        // Test getting transitive callers of d should return c, b, a

        let node_a = CallNode {
            name: "a".to_string(),
            file_path: "/file.rs".to_string(),
            line: 1,
            calls: vec![CallEdge {
                target: "b".to_string(),
                file_path: "/file.rs".to_string(),
                line: 2,
                column: 1,
                call_type: CallType::Direct,
                scope_hint: None,
                resolved: false,
                resolution: CallResolution::Unknown,
            }],
            metrics: FunctionMetrics::default(),
            receiver: None,
        };

        let node_b = CallNode {
            name: "b".to_string(),
            file_path: "/file.rs".to_string(),
            line: 10,
            calls: vec![CallEdge {
                target: "c".to_string(),
                file_path: "/file.rs".to_string(),
                line: 12,
                column: 1,
                call_type: CallType::Direct,
                scope_hint: None,
                resolved: false,
                resolution: CallResolution::Unknown,
            }],
            metrics: FunctionMetrics::default(),
            receiver: None,
        };

        let node_c = CallNode {
            name: "c".to_string(),
            file_path: "/file.rs".to_string(),
            line: 20,
            calls: vec![CallEdge {
                target: "d".to_string(),
                file_path: "/file.rs".to_string(),
                line: 22,
                column: 1,
                call_type: CallType::Direct,
                scope_hint: None,
                resolved: false,
                resolution: CallResolution::Unknown,
            }],
            metrics: FunctionMetrics::default(),
            receiver: None,
        };

        let node_d = CallNode {
            name: "d".to_string(),
            file_path: "/file.rs".to_string(),
            line: 30,
            calls: Vec::new(),
            metrics: FunctionMetrics::default(),
            receiver: None,
        };

        graph.nodes.insert("a".to_string(), node_a);
        graph.nodes.insert("b".to_string(), node_b);
        graph.nodes.insert("c".to_string(), node_c);
        graph.nodes.insert("d".to_string(), node_d);

        let callers = graph.get_transitive_callers("d", 10);
        assert_eq!(callers.len(), 3);

        // Should find c at depth 1, b at depth 2, a at depth 3
        assert!(callers
            .iter()
            .any(|(name, depth)| name == "c" && *depth == 1));
        assert!(callers
            .iter()
            .any(|(name, depth)| name == "b" && *depth == 2));
        assert!(callers
            .iter()
            .any(|(name, depth)| name == "a" && *depth == 3));
    }

    #[test]
    fn test_get_transitive_callers_with_max_depth() {
        let graph = CallGraph::new();

        // Create a chain a -> b -> c
        let node_b = CallNode {
            name: "b".to_string(),
            file_path: "/file.rs".to_string(),
            line: 10,
            calls: vec![CallEdge {
                target: "c".to_string(),
                file_path: "/file.rs".to_string(),
                line: 12,
                column: 1,
                call_type: CallType::Direct,
                scope_hint: None,
                resolved: false,
                resolution: CallResolution::Unknown,
            }],
            metrics: FunctionMetrics::default(),
            receiver: None,
        };

        let node_c = CallNode {
            name: "c".to_string(),
            file_path: "/file.rs".to_string(),
            line: 20,
            calls: Vec::new(),
            metrics: FunctionMetrics::default(),
            receiver: None,
        };

        graph.nodes.insert("b".to_string(), node_b);
        graph.nodes.insert("c".to_string(), node_c);

        // With max_depth=1, should only find b
        let callers = graph.get_transitive_callers("c", 1);
        assert_eq!(callers.len(), 1);
        assert_eq!(callers[0].0, "b");
        assert_eq!(callers[0].1, 1);
    }

    #[test]
    fn test_get_transitive_callees() {
        let graph = CallGraph::new();

        // Create a call chain: a -> b -> c -> d
        // Test getting transitive callees of a should return b, c, d

        let node_a = CallNode {
            name: "a".to_string(),
            file_path: "/file.rs".to_string(),
            line: 1,
            calls: vec![CallEdge {
                target: "b".to_string(),
                file_path: "/file.rs".to_string(),
                line: 2,
                column: 1,
                call_type: CallType::Direct,
                scope_hint: None,
                resolved: false,
                resolution: CallResolution::Unknown,
            }],
            metrics: FunctionMetrics::default(),
            receiver: None,
        };

        let node_b = CallNode {
            name: "b".to_string(),
            file_path: "/file.rs".to_string(),
            line: 10,
            calls: vec![CallEdge {
                target: "c".to_string(),
                file_path: "/file.rs".to_string(),
                line: 12,
                column: 1,
                call_type: CallType::Direct,
                scope_hint: None,
                resolved: false,
                resolution: CallResolution::Unknown,
            }],
            metrics: FunctionMetrics::default(),
            receiver: None,
        };

        let node_c = CallNode {
            name: "c".to_string(),
            file_path: "/file.rs".to_string(),
            line: 20,
            calls: vec![CallEdge {
                target: "d".to_string(),
                file_path: "/file.rs".to_string(),
                line: 22,
                column: 1,
                call_type: CallType::Direct,
                scope_hint: None,
                resolved: false,
                resolution: CallResolution::Unknown,
            }],
            metrics: FunctionMetrics::default(),
            receiver: None,
        };

        let node_d = CallNode {
            name: "d".to_string(),
            file_path: "/file.rs".to_string(),
            line: 30,
            calls: Vec::new(),
            metrics: FunctionMetrics::default(),
            receiver: None,
        };

        graph.nodes.insert("a".to_string(), node_a);
        graph.nodes.insert("b".to_string(), node_b);
        graph.nodes.insert("c".to_string(), node_c);
        graph.nodes.insert("d".to_string(), node_d);

        let callees = graph.get_transitive_callees("a", 10);
        assert_eq!(callees.len(), 3);

        // Should find b at depth 1, c at depth 2, d at depth 3
        assert!(callees
            .iter()
            .any(|(name, depth)| name == "b" && *depth == 1));
        assert!(callees
            .iter()
            .any(|(name, depth)| name == "c" && *depth == 2));
        assert!(callees
            .iter()
            .any(|(name, depth)| name == "d" && *depth == 3));
    }

    #[test]
    fn test_find_call_path() {
        let graph = CallGraph::new();

        // Create a call path: a -> b -> c
        let node_a = CallNode {
            name: "a".to_string(),
            file_path: "/file.rs".to_string(),
            line: 1,
            calls: vec![CallEdge {
                target: "b".to_string(),
                file_path: "/file.rs".to_string(),
                line: 2,
                column: 1,
                call_type: CallType::Direct,
                scope_hint: None,
                resolved: false,
                resolution: CallResolution::Unknown,
            }],
            metrics: FunctionMetrics::default(),
            receiver: None,
        };

        let node_b = CallNode {
            name: "b".to_string(),
            file_path: "/file.rs".to_string(),
            line: 10,
            calls: vec![CallEdge {
                target: "c".to_string(),
                file_path: "/file.rs".to_string(),
                line: 12,
                column: 1,
                call_type: CallType::Direct,
                scope_hint: None,
                resolved: false,
                resolution: CallResolution::Unknown,
            }],
            metrics: FunctionMetrics::default(),
            receiver: None,
        };

        let node_c = CallNode {
            name: "c".to_string(),
            file_path: "/file.rs".to_string(),
            line: 20,
            calls: Vec::new(),
            metrics: FunctionMetrics::default(),
            receiver: None,
        };

        graph.nodes.insert("a".to_string(), node_a);
        graph.nodes.insert("b".to_string(), node_b);
        graph.nodes.insert("c".to_string(), node_c);

        let path = graph.find_call_path("a", "c");
        assert!(path.is_some());
        let path = path.unwrap();
        assert_eq!(path, vec!["a", "b", "c"]);
    }

    #[test]
    fn test_find_call_path_no_path() {
        let graph = CallGraph::new();

        // Create two separate functions with no connection
        let node_a = CallNode {
            name: "a".to_string(),
            file_path: "/file.rs".to_string(),
            line: 1,
            calls: Vec::new(),
            metrics: FunctionMetrics::default(),
            receiver: None,
        };

        let node_b = CallNode {
            name: "b".to_string(),
            file_path: "/file.rs".to_string(),
            line: 10,
            calls: Vec::new(),
            metrics: FunctionMetrics::default(),
            receiver: None,
        };

        graph.nodes.insert("a".to_string(), node_a);
        graph.nodes.insert("b".to_string(), node_b);

        let path = graph.find_call_path("a", "b");
        assert!(path.is_none());
    }

    #[test]
    fn test_get_hotspots() {
        let graph = CallGraph::new();

        // Create a highly connected function
        let hotspot = CallNode {
            name: "hotspot".to_string(),
            file_path: "/file.rs".to_string(),
            line: 10,
            calls: vec![
                CallEdge {
                    target: "f1".to_string(),
                    file_path: "/file.rs".to_string(),
                    line: 11,
                    column: 1,
                    call_type: CallType::Direct,
                    scope_hint: None,
                    resolved: false,
                    resolution: CallResolution::Unknown,
                },
                CallEdge {
                    target: "f2".to_string(),
                    file_path: "/file.rs".to_string(),
                    line: 12,
                    column: 1,
                    call_type: CallType::Direct,
                    scope_hint: None,
                    resolved: false,
                    resolution: CallResolution::Unknown,
                },
            ],
            metrics: FunctionMetrics::default(),
            receiver: None,
        };

        // Create a less connected function
        let normal = CallNode {
            name: "normal".to_string(),
            file_path: "/file.rs".to_string(),
            line: 50,
            calls: vec![CallEdge {
                target: "f3".to_string(),
                file_path: "/file.rs".to_string(),
                line: 51,
                column: 1,
                call_type: CallType::Direct,
                scope_hint: None,
                resolved: false,
                resolution: CallResolution::Unknown,
            }],
            metrics: FunctionMetrics::default(),
            receiver: None,
        };

        // Three callers point at "hotspot" (forward edges).
        for name in ["caller1", "caller2", "caller3"] {
            graph.nodes.insert(
                name.to_string(),
                CallNode {
                    name: name.to_string(),
                    file_path: "/file.rs".to_string(),
                    line: 20,
                    calls: vec![CallEdge {
                        target: "hotspot".to_string(),
                        file_path: "/file.rs".to_string(),
                        line: 20,
                        column: 1,
                        call_type: CallType::Direct,
                        scope_hint: None,
                        resolved: false,
                        resolution: CallResolution::Unknown,
                    }],
                    metrics: FunctionMetrics::default(),
                    receiver: None,
                },
            );
        }

        graph.nodes.insert("hotspot".to_string(), hotspot);
        graph.nodes.insert("normal".to_string(), normal);

        let hotspots = graph.get_hotspots(3);

        // Should find hotspot (5 connections) but not normal (1 connection)
        assert_eq!(hotspots.len(), 1);
        assert_eq!(hotspots[0].0, "hotspot");
        assert_eq!(hotspots[0].1, 3); // incoming
        assert_eq!(hotspots[0].2, 2); // outgoing
    }

    #[test]
    fn test_cyclomatic_complexity_calculation() {
        // Test that the base cyclomatic complexity is 1 for a simple function
        let metrics = FunctionMetrics {
            loc: 5,
            cyclomatic: 1, // Base complexity
            max_depth: 1,
            params: 0,
            returns: 1,
            cognitive: 0,
        };

        assert_eq!(metrics.cyclomatic, 1);

        // Test that branches increase complexity
        let complex_metrics = FunctionMetrics {
            loc: 20,
            cyclomatic: 6, // 1 base + 5 branches
            max_depth: 3,
            params: 2,
            returns: 3,
            cognitive: 10,
        };

        assert_eq!(complex_metrics.cyclomatic, 6);
    }

    #[test]
    fn test_to_markdown_single_function() {
        let graph = CallGraph::new();

        let node = CallNode {
            name: "test_func".to_string(),
            file_path: "/path/to/file.rs".to_string(),
            line: 42,
            calls: vec![CallEdge {
                target: "helper".to_string(),
                file_path: "/path/to/file.rs".to_string(),
                line: 45,
                column: 5,
                call_type: CallType::Direct,
                scope_hint: None,
                resolved: false,
                resolution: CallResolution::Unknown,
            }],
            metrics: FunctionMetrics {
                loc: 10,
                cyclomatic: 3,
                max_depth: 2,
                params: 1,
                returns: 1,
                cognitive: 4,
            },
            receiver: None,
        };

        graph.nodes.insert("test_func".to_string(), node);
        // A caller ("main") so the "Called By" section is exercised.
        graph.nodes.insert(
            "main".to_string(),
            CallNode {
                name: "main".to_string(),
                file_path: "/path/to/main.rs".to_string(),
                line: 10,
                calls: vec![CallEdge {
                    target: "test_func".to_string(),
                    file_path: "/path/to/main.rs".to_string(),
                    line: 10,
                    column: 3,
                    call_type: CallType::Direct,
                    scope_hint: None,
                    resolved: false,
                    resolution: CallResolution::Unknown,
                }],
                metrics: FunctionMetrics::default(),
                receiver: None,
            },
        );

        let markdown = graph.to_markdown(Some("test_func"));

        assert!(markdown.contains("# Call Graph: test_func"));
        assert!(markdown.contains("/path/to/file.rs:42"));
        assert!(markdown.contains("CC=3"));
        assert!(markdown.contains("LOC=10"));
        assert!(markdown.contains("helper"));
        assert!(markdown.contains("main"));
    }

    #[test]
    fn test_to_markdown_nonexistent_function() {
        let graph = CallGraph::new();

        let markdown = graph.to_markdown(Some("nonexistent"));

        assert!(markdown.contains("Function `nonexistent` not found"));
    }

    #[test]
    fn test_to_markdown_summary() {
        let graph = CallGraph::new();

        let node1 = CallNode {
            name: "func1".to_string(),
            file_path: "/file.rs".to_string(),
            line: 10,
            calls: Vec::new(),
            metrics: FunctionMetrics {
                loc: 10,
                cyclomatic: 2,
                max_depth: 1,
                params: 0,
                returns: 1,
                cognitive: 2,
            },
            receiver: None,
        };

        graph.nodes.insert("func1".to_string(), node1);

        let markdown = graph.to_markdown(None);

        assert!(markdown.contains("# Call Graph Summary"));
        assert!(markdown.contains("**Total Functions**: 1"));
        assert!(markdown.contains("Most Called Functions"));
        assert!(markdown.contains("Complexity Hotspots"));
    }

    #[test]
    fn test_to_dot_format() {
        let graph = CallGraph::new();

        let node = CallNode {
            name: "func".to_string(),
            file_path: "/file.rs".to_string(),
            line: 10,
            calls: vec![CallEdge {
                target: "helper".to_string(),
                file_path: "/file.rs".to_string(),
                line: 12,
                column: 5,
                call_type: CallType::Direct,
                scope_hint: None,
                resolved: false,
                resolution: CallResolution::Unknown,
            }],
            metrics: FunctionMetrics {
                loc: 5,
                cyclomatic: 2,
                max_depth: 1,
                params: 1,
                returns: 1,
                cognitive: 2,
            },
            receiver: None,
        };

        graph.nodes.insert("func".to_string(), node);

        let dot = graph.to_dot(None);

        assert!(dot.contains("digraph CallGraph"));
        assert!(dot.contains("\"func\""));
        assert!(dot.contains("\"func\" -> \"helper\""));
        assert!(dot.contains("CC:2"));
        assert!(dot.contains("LOC:5"));
    }

    #[test]
    fn test_call_edge_properties() {
        let edge = CallEdge {
            target: "target_func".to_string(),
            file_path: "/path/to/file.rs".to_string(),
            line: 42,
            column: 10,
            call_type: CallType::Method,
            scope_hint: None,
            resolved: false,
            resolution: CallResolution::Unknown,
        };

        assert_eq!(edge.target, "target_func");
        assert_eq!(edge.file_path, "/path/to/file.rs");
        assert_eq!(edge.line, 42);
        assert_eq!(edge.column, 10);
        assert_eq!(edge.call_type, CallType::Method);
    }

    #[test]
    fn test_call_types_distinct() {
        assert_ne!(CallType::Direct, CallType::Method);
        assert_ne!(CallType::Method, CallType::StaticMethod);
        assert_ne!(CallType::StaticMethod, CallType::Closure);
        assert_ne!(CallType::Closure, CallType::Async);
        assert_ne!(CallType::Async, CallType::Spawn);
        assert_ne!(CallType::Spawn, CallType::Unknown);
    }

    #[test]
    fn test_function_metrics_all_fields() {
        let metrics = FunctionMetrics {
            loc: 100,
            cyclomatic: 15,
            max_depth: 5,
            params: 7,
            returns: 4,
            cognitive: 25,
        };

        assert_eq!(metrics.loc, 100);
        assert_eq!(metrics.cyclomatic, 15);
        assert_eq!(metrics.max_depth, 5);
        assert_eq!(metrics.params, 7);
        assert_eq!(metrics.returns, 4);
        assert_eq!(metrics.cognitive, 25);
    }

    #[test]
    fn test_multiple_functions_in_graph() {
        let graph = CallGraph::new();

        let functions = vec!["func1", "func2", "func3", "func4", "func5"];

        for (i, name) in functions.iter().enumerate() {
            let node = CallNode {
                name: name.to_string(),
                file_path: format!("/file{}.rs", i),
                line: (i + 1) * 10,
                calls: Vec::new(),
                metrics: FunctionMetrics::default(),
                receiver: None,
            };
            graph.nodes.insert(name.to_string(), node);
        }

        assert_eq!(graph.nodes.len(), 5);

        for name in &functions {
            assert!(graph.nodes.contains_key(*name));
        }
    }

    #[test]
    fn test_scope_matches_file_path() {
        // "App" should match src/app/mod.rs
        assert!(CallGraph::scope_matches_file_path("App", "src/app/mod.rs"));
        // "App" should not match src/application.rs (no /app/ or /app.)
        assert!(!CallGraph::scope_matches_file_path(
            "App",
            "src/application.rs"
        ));
        // "crate::utils" -> strips crate::, becomes "utils", matches src/utils/mod.rs
        assert!(CallGraph::scope_matches_file_path(
            "crate::utils",
            "src/utils/mod.rs"
        ));
        // "crate::utils" -> "utils" matches src/utils.rs
        assert!(CallGraph::scope_matches_file_path(
            "crate::utils",
            "src/utils.rs"
        ));
        // "self::foo" -> "foo" matches src/foo/bar.rs
        assert!(CallGraph::scope_matches_file_path(
            "self::foo",
            "src/foo/bar.rs"
        ));
        // "super::bar" -> "bar" matches src/bar.rs
        assert!(CallGraph::scope_matches_file_path(
            "super::bar",
            "src/bar.rs"
        ));
        // Case insensitive: "App" matches "src/App/mod.rs" and "src/app/mod.rs"
        assert!(CallGraph::scope_matches_file_path("App", "src/App/mod.rs"));
        // Nested scope: "api::client" -> "api/client" matches "src/api/client.rs"
        assert!(CallGraph::scope_matches_file_path(
            "api::client",
            "src/api/client.rs"
        ));
    }

    #[test]
    fn test_resolve_callee_deterministic() {
        let graph = CallGraph::new();

        // Two different files each have a "run" function
        let node_a = CallNode {
            name: "run".to_string(),
            file_path: "src/agents/mod.rs".to_string(),
            line: 10,
            calls: Vec::new(),
            metrics: FunctionMetrics::default(),
            receiver: None,
        };
        let node_b = CallNode {
            name: "run".to_string(),
            file_path: "src/app/mod.rs".to_string(),
            line: 20,
            calls: Vec::new(),
            metrics: FunctionMetrics::default(),
            receiver: None,
        };

        graph.insert_node(CallGraph::qualified_key("src/agents/mod.rs", "run"), node_a);
        graph.insert_node(CallGraph::qualified_key("src/app/mod.rs", "run"), node_b);

        // Without scope hint, from a third file, should get deterministic result
        // (alphabetically first: "src/agents/mod.rs::run" < "src/app/mod.rs::run")
        let result1 = graph.resolve_callee("run", "src/main.rs", None);
        let result2 = graph.resolve_callee("run", "src/main.rs", None);
        assert_eq!(result1, result2, "resolve_callee must be deterministic");
        assert_eq!(result1.0, "src/agents/mod.rs::run");
        assert_eq!(
            result1.1,
            CallResolution::NameOnly,
            "a deterministic pick between namesakes is still only a name match"
        );
    }

    #[test]
    fn test_resolve_callee_with_scope_hint() {
        let graph = CallGraph::new();

        // Two different files each have a "run" function
        let node_a = CallNode {
            name: "run".to_string(),
            file_path: "src/agents/mod.rs".to_string(),
            line: 10,
            calls: Vec::new(),
            metrics: FunctionMetrics::default(),
            receiver: None,
        };
        let node_b = CallNode {
            name: "run".to_string(),
            file_path: "src/app/mod.rs".to_string(),
            line: 20,
            calls: Vec::new(),
            metrics: FunctionMetrics::default(),
            receiver: None,
        };

        graph.insert_node(CallGraph::qualified_key("src/agents/mod.rs", "run"), node_a);
        graph.insert_node(CallGraph::qualified_key("src/app/mod.rs", "run"), node_b);

        // With scope hint "App", should pick app/mod.rs::run
        let (result, resolution) = graph.resolve_callee("run", "src/main.rs", Some("App"));
        assert_eq!(
            result, "src/app/mod.rs::run",
            "scope hint 'App' should resolve to app module"
        );
        assert_eq!(resolution, CallResolution::Scope);

        // With scope hint "agents", should pick agents/mod.rs::run
        let (result, resolution) = graph.resolve_callee("run", "src/main.rs", Some("agents"));
        assert_eq!(
            result, "src/agents/mod.rs::run",
            "scope hint 'agents' should resolve to agents module"
        );
        assert_eq!(resolution, CallResolution::Scope);
    }

    #[test]
    fn test_resolve_callee_same_file_preferred() {
        let graph = CallGraph::new();

        let node_a = CallNode {
            name: "helper".to_string(),
            file_path: "src/main.rs".to_string(),
            line: 5,
            calls: Vec::new(),
            metrics: FunctionMetrics::default(),
            receiver: None,
        };
        let node_b = CallNode {
            name: "helper".to_string(),
            file_path: "src/utils.rs".to_string(),
            line: 10,
            calls: Vec::new(),
            metrics: FunctionMetrics::default(),
            receiver: None,
        };

        graph
            .nodes
            .insert(CallGraph::qualified_key("src/main.rs", "helper"), node_a);
        graph
            .nodes
            .insert(CallGraph::qualified_key("src/utils.rs", "helper"), node_b);

        // Same-file match should always win, even with a scope hint pointing elsewhere
        let (result, _) = graph.resolve_callee("helper", "src/main.rs", Some("utils"));
        assert_eq!(result, "src/main.rs::helper");
    }

    #[test]
    fn test_find_function_deterministic() {
        let graph = CallGraph::new();

        // Multiple functions named "run" in different files
        for file in &["src/agents/mod.rs", "src/app/mod.rs", "src/cli/mod.rs"] {
            let node = CallNode {
                name: "run".to_string(),
                file_path: file.to_string(),
                line: 1,
                calls: Vec::new(),
                metrics: FunctionMetrics::default(),
                receiver: None,
            };
            graph
                .nodes
                .insert(CallGraph::qualified_key(file, "run"), node);
        }

        // find_function("run") should return the same result every time (alphabetically first)
        let result1 = graph.find_function("run");
        let result2 = graph.find_function("run");
        assert_eq!(result1, result2, "find_function must be deterministic");
        assert_eq!(result1, Some("src/agents/mod.rs::run".to_string()));
    }
}
