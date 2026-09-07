//! Call graph analysis - tracks which functions call which
//!
//! This is critical for AI understanding of code flow and impact analysis.

pub mod types;

use crate::FastMap;
use anyhow::{anyhow, Result};
use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
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
    /// The type this function returns, as written in its declaration: Go's
    /// first non-`error` result, a Rust `-> T`, a TS annotation. What lets a
    /// call on the value a call produced (`accountRepo(ctx).FindByUuid(id)`,
    /// `svc := build()` then `svc.Run()`) be resolved on a type rather than
    /// guessed among namesakes.
    #[serde(default)]
    pub result_type: Option<String>,
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
    /// No receiver type was written at the call site: the call was chained onto
    /// a factory or constructor call, and the type came from *its* name
    /// (`SfxOperationUseFactory(c).Pay()` -> type `SfxOperationUse`). Exactly
    /// one namesake is defined on that type, so this is a strong candidate —
    /// but the type was inferred from a naming convention, not read.
    FactoryReceiver,
    /// The qualifier named an interface, and exactly one type in the index
    /// implements it — carries every method the interface declares — so the
    /// call was resolved on that type. Dispatch is dynamic: another
    /// implementation could be introduced, or injected from outside the index,
    /// which is why this is a strong candidate rather than a fact.
    InterfaceImpl,
    /// The qualifier matched the target's module or file path.
    Scope,
    /// Several namesakes; the one in the caller's own file was taken.
    SameFile,
    /// Several namesakes and nothing to choose between them — the target is a
    /// deterministic pick, not a fact.
    NameOnly,
    /// No function of that name is in the graph, and nothing at the call site
    /// says where it went: a stdlib or builtin name, a macro, a method on a
    /// value whose type was never written down. The residual case — the two
    /// variants below are the ones the call site actually explains.
    Unresolved,
    /// The call leaves the index, and the source says so: the qualifier named
    /// an import that resolves to no indexed package (`assert.Len`), or a
    /// local declared as a type from outside this repository (`wg.Add`).
    /// Nothing is missing from the graph — the target is not project code.
    ExternalPackage,
    /// The qualifier named a package that *is* indexed, which carries no
    /// function of that name: a method reached through a value, or a file the
    /// index skipped. Unlike [`ExternalPackage`](Self::ExternalPackage) this
    /// is a gap in what was read, so it is the count worth acting on.
    PackageMissing,
}

impl CallResolution {
    /// Does this edge point at a function the graph actually identified?
    pub fn is_certain(self) -> bool {
        matches!(self, Self::Unique | Self::Receiver | Self::Scope)
    }

    /// Does this edge point outside the graph — no target function at all,
    /// whatever the reason? Reports have to group these three together
    /// wherever they ask "did this call resolve", and equality against a
    /// single variant silently stopped covering that the moment the reasons
    /// were split apart.
    pub fn is_outside_graph(self) -> bool {
        matches!(
            self,
            Self::Unresolved | Self::ExternalPackage | Self::PackageMissing
        )
    }

    /// Short label for reports; `None` for edges that need no caveat.
    pub fn caveat(self) -> Option<&'static str> {
        match self {
            Self::FactoryReceiver => Some("receiver inferred from factory name"),
            Self::InterfaceImpl => Some("single implementation of the interface"),
            Self::SameFile => Some("same-file match"),
            Self::NameOnly => Some("name match only"),
            Self::Unresolved => Some("not in graph"),
            Self::ExternalPackage => Some("not in graph, outside the index"),
            Self::PackageMissing => Some("not in graph, indexed package has no such function"),
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
    /// Function name -> CallNode. Keys are derived from indexed source
    /// files, not from network input reaching this process directly — but
    /// narsil-mcp's own supply-chain/dependency-audit use case means "on
    /// disk" can still mean "an adversarial vendored dependency the user
    /// pointed the tool at". `ahash` trades SipHash's resistance to crafted
    /// collisions for real speed on a map this hot (one entry per symbol,
    /// looked up per call site across the corpus); the accepted risk is a
    /// hostile repo degrading its own indexing run toward worst case, not a
    /// security-boundary break — this is a single local process, not a
    /// multi-tenant service where one attacker could starve other users.
    nodes: FastMap<String, CallNode>,
    /// File -> Functions defined in that file
    file_functions: FastMap<String, Vec<String>>,
    /// Bare function name -> qualified keys. Lets `resolve_callee` look up
    /// candidates in O(1) instead of scanning every node per call site
    /// (O(N²) on a 469k-symbol corpus).
    name_index: FastMap<String, Vec<String>>,
    /// Lazily-memoized `file_path.to_lowercase()` per node key, used by
    /// `resolve_callee`'s scope-hint disambiguation (`scope_matches_file_path`).
    /// Not persisted: `CallGraph` is always reconstructed via `new()` then
    /// repopulated (fresh parse or from a persisted snapshot), so recomputing
    /// this once per key per process lifetime is cheap and needs no
    /// serde-compat handling for caches written before this field existed.
    file_path_lower_cache: FastMap<String, Arc<str>>,
    /// Import path (as written in the source) -> the directory of the indexed
    /// package it names, or `None` for a package that is not in the index at
    /// all. Memoized because answering it means scanning the known file paths;
    /// cleared whenever the set of indexed files changes, since both answers
    /// depend on it. See [`package_dir_for`](CallGraph::package_dir_for).
    import_dir_cache: FastMap<String, Option<Arc<str>>>,
    /// Declared types of the indexed sources: fields, embedded types,
    /// interface methods. What lets a qualifier naming a field or a value be
    /// resolved on a type that was written down rather than guessed at.
    types: types::TypeIndex,
    /// `interface + \u{1} + type -> does that type carry every method the interface
    /// declares`. Memoized because the check walks the interface's whole
    /// method set through `name_index`, and the same handful of interfaces is
    /// asked about at thousands of call sites. Cleared with the type index,
    /// since both answers depend on the set of indexed files.
    implements_cache: FastMap<String, bool>,
    /// Interface name -> the method names it declares. Read on every
    /// interface-qualified call site, and `TypeIndex` has to clone the vector
    /// to hand it out; once per interface is enough.
    interface_methods_cache: FastMap<String, Arc<Vec<String>>>,
}

impl Default for CallGraph {
    fn default() -> Self {
        Self::new()
    }
}

/// Per-file constants of a call-site walk: the file being walked, and the
/// packages its import list binds. A struct because these plus the
/// enclosing-function state ([`FnScope`]) pushed `walk_for_calls` past a
/// readable argument count.
struct FileScope<'a> {
    path: &'a str,
    imports: &'a HashMap<String, String>,
}

/// What a name in the enclosing function stands for.
#[derive(Debug, Clone, PartialEq, Eq)]
enum LocalKind {
    /// Declared as this type.
    Type(String),
    /// Declared as something no method of this repository can sit on: a
    /// builtin, a container, a type from a package outside the index. Recorded
    /// rather than dropped, because `err.Error()` and `wg.Add(1)` are calls
    /// into someone else's code, and the honest answer for them is "not in the
    /// graph" — not this corpus's first `Error` by sort order.
    Foreign,
}

/// What the walk knows about the function it is currently inside: its graph
/// key, the type it is defined on (for `self` call sites), and what the names
/// in it stand for.
#[derive(Default)]
struct FnScope {
    function: Option<String>,
    receiver: Option<String>,
    locals: HashMap<String, LocalKind>,
}

/// Everything resolution can use about the place a call was written: the file
/// (a tiebreaker among namesakes), the enclosing method's receiver type (for
/// `self`), the names declared around it, and the file's imports (to tell a
/// package qualifier from a variable). Passed as one value because these
/// travel together through callee description, chained-call typing and local
/// declaration reading.
#[derive(Clone, Copy)]
struct CallCtx<'a> {
    file: &'a str,
    self_type: Option<&'a str>,
    locals: &'a HashMap<String, LocalKind>,
    /// Declarations at the file's own top level. Kept apart from `locals`
    /// because those are cleared on entering each function, while a
    /// package-level `var limiter = ...` is in scope for all of them.
    file_locals: &'a HashMap<String, LocalKind>,
    imports: &'a HashMap<String, String>,
}

impl<'a> CallCtx<'a> {
    /// The declared type of a name in scope, if it is one that can carry a
    /// project method.
    fn local_type(&self, name: &str) -> Option<&'a str> {
        match self.declared(name) {
            Some(LocalKind::Type(type_name)) => Some(type_name.as_str()),
            _ => None,
        }
    }

    fn is_foreign_local(&self, name: &str) -> bool {
        matches!(self.declared(name), Some(LocalKind::Foreign))
    }

    /// A function's own declaration shadows the package-level one, as in the
    /// language itself.
    fn declared(&self, name: &str) -> Option<&'a LocalKind> {
        self.locals.get(name).or_else(|| self.file_locals.get(name))
    }
}

/// Shared with the symbol parser, which labels methods with the same owner
/// the call graph resolves calls against.
pub(crate) fn is_type_declaration(kind: &str) -> bool {
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
pub(crate) fn first_type_identifier(node: Node, source: &[u8]) -> Option<String> {
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
pub(crate) fn extract_receiver_type(node: Node, source: &[u8]) -> Option<String> {
    if let Some(receiver) = node.child_by_field_name("receiver") {
        if let Some(name) = first_type_identifier(receiver, source) {
            return Some(name);
        }
    }

    let mut ancestor = node.parent();
    while let Some(current) = ancestor {
        if current.kind() == "impl_item" {
            // Rust: `impl Foo` and `impl Trait for Foo` both own `Foo`.
            return current
                .child_by_field_name("type")
                .and_then(|owner| first_type_identifier(owner, source));
        }
        if is_type_declaration(current.kind()) {
            if let Some(name) = current
                .child_by_field_name("name")
                .and_then(|name| name.utf8_text(source).ok().map(|s| s.to_string()))
            {
                return Some(name);
            }
            // Python's root node is also called `module`; unlike Ruby's it
            // names no type, so the walk continues past it.
            if current.kind() != "module" {
                return first_type_identifier(current, source);
            }
        }
        ancestor = current.parent();
    }
    None
}

impl CallGraph {
    pub fn new() -> Self {
        Self {
            nodes: DashMap::default(),
            file_functions: DashMap::default(),
            name_index: DashMap::default(),
            file_path_lower_cache: DashMap::default(),
            import_dir_cache: DashMap::default(),
            types: types::TypeIndex::new(),
            implements_cache: DashMap::default(),
            interface_methods_cache: DashMap::default(),
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
        // A "package not in the index" answer cached before this batch may be
        // wrong now, and a directory answer may have become ambiguous.
        self.import_dir_cache.clear();
        self.implements_cache.clear();
        self.interface_methods_cache.clear();
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

    /// Remove all functions defined in a file from the call graph.
    /// Call this before re-indexing a file to clean up stale function definitions.
    pub fn remove_functions_for_file(&self, path: &str) -> Result<()> {
        self.import_dir_cache.clear();
        self.implements_cache.clear();
        self.interface_methods_cache.clear();
        self.types.remove_file(path);
        if let Some((_, function_keys)) = self.file_functions.remove(path) {
            for key in function_keys {
                self.nodes.remove(&key);
                // Otherwise this grows for the life of the process under
                // `--watch`: every reindex of a changed file adds fresh
                // entries but never reclaims the ones for keys removed here.
                self.file_path_lower_cache.remove(&key);

                if let Some(bare_name) = key.split("::").last() {
                    let emptied = match self.name_index.get_mut(bare_name) {
                        Some(mut entry) => {
                            entry.retain(|k| k != &key);
                            entry.is_empty()
                        }
                        None => false,
                    };
                    // Leaving the empty Vec behind makes the name look known
                    // to `resolve_callee` while offering no candidate.
                    if emptied {
                        self.name_index.remove_if(bare_name, |_, v| v.is_empty());
                    }
                }
            }
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
    ///
    /// Re-inserting a key must not push into `name_index` a second time. A
    /// duplicate there is not cosmetic: `resolve_callee` counts candidates,
    /// so one function listed twice reads as two rivals, the unique-match
    /// steps bail out, and calls that used to resolve by receiver type fall
    /// through to the alphabetical fallback (`Store::new()` landing on
    /// `Cache::new`). The key embeds the bare name, so an overwrite always
    /// concerns the name already indexed.
    fn insert_node(&self, key: String, node: CallNode) {
        let name = node.name.clone();
        if self.nodes.insert(key.clone(), node).is_none() {
            self.name_index.entry(name).or_default().push(key);
        }
    }

    fn extract_functions(&self, path: &str, content: &str, tree: &Tree) -> Result<()> {
        let source = content.as_bytes();
        let mut cursor = tree.walk();
        let mut functions = Vec::new();

        // Declarations first: a re-indexed file must not leave the old ones
        // behind, and the walk below is where the file's own types stop being
        // whatever a previous version of it said.
        self.types.remove_file(path);
        self.types.ingest_file(path, tree.root_node(), source);

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
            receiver: extract_receiver_type(node, source),
            result_type: Self::extract_result_type(node, source),
        })
    }

    /// The declared return type of a function, reduced to the identifier a
    /// receiver is compared against.
    ///
    /// Go writes results in a `result` field — one type, or a `parameter_list`
    /// for `(T, error)`, where the first non-`error` entry is the value a
    /// caller chains onto. Rust and TypeScript name the field `return_type`.
    /// Anything else (a tuple of two meaningful values, a container) yields
    /// `None` rather than a type the call site never had.
    fn extract_result_type(node: Node, source: &[u8]) -> Option<String> {
        let result = node
            .child_by_field_name("result")
            .or_else(|| node.child_by_field_name("return_type"))?;
        if result.kind() != "parameter_list" {
            return types::type_name_of(result, source);
        }
        let mut cursor = result.walk();
        let declarations: Vec<Node> = result
            .children(&mut cursor)
            .filter(|child| child.kind() == "parameter_declaration")
            .collect();
        declarations.into_iter().find_map(|declaration| {
            declaration
                .child_by_field_name("type")
                .and_then(|declared| types::type_name_of(declared, source))
        })
    }

    /// The type a call produces: resolve the callee, then read its declared
    /// result type.
    ///
    /// Only a target the graph actually identified is trusted here. Reading
    /// the result type off a name match would compound one guess into another
    /// — the wrong namesake's return type, applied with the confidence of
    /// something that was read.
    fn call_result_type(
        &self,
        described: &(String, CallType, Option<String>, bool),
        ctx: CallCtx,
    ) -> Option<String> {
        let (name, _, scope_hint, scope_inferred) = described;
        // Resolution is the expensive half; a callee that is not in the graph
        // (stdlib, third party, a type conversion) can be dismissed on an
        // O(1) name lookup instead.
        if !self.name_index.contains_key(name) {
            return None;
        }
        let (key, resolution) =
            self.resolve_scoped_callee(name, ctx.file, scope_hint.as_deref(), *scope_inferred);
        if resolution.is_certain() || resolution == CallResolution::FactoryReceiver {
            return self
                .nodes
                .get(key.as_str())
                .and_then(|node| node.result_type.clone());
        }
        // The target is one namesake out of several, so its result type cannot
        // be read off — unless every namesake declares the same one. A corpus
        // full of per-package test helpers named `accountRepo` is exactly that
        // case: whichever one the call meant, the type it hands back is the
        // same.
        let candidates = self.name_index.get(name)?;
        // Consensus is for a handful of per-package helpers of one name. Past
        // that the scan costs more than the answer is worth, and a name shared
        // by dozens of functions is not one helper written repeatedly.
        if candidates.len() > 8 {
            return None;
        }
        let mut agreed: Option<String> = None;
        for candidate in candidates.iter() {
            let declared = self
                .nodes
                .get(candidate.as_str())
                .and_then(|node| node.result_type.clone())?;
            match &agreed {
                Some(held) if held != &declared => return None,
                Some(_) => {}
                None => agreed = Some(declared),
            }
        }
        agreed
    }

    /// Variable name -> declared type, for the parameters and local variables
    /// of one function.
    ///
    /// The largest remaining group of guessed edges is a call on a name that
    /// stands for a value: `batchService.Create(dto)`, `lineRepo.FindByUuid(id)`,
    /// `svc.Run()`. The qualifier is the variable, not the type, so resolution
    /// had nothing to match a receiver against — but the type is written a few
    /// lines up, in the parameter list or in the declaration that produced the
    /// value. This is a deliberately shallow reading of it (no flow analysis,
    /// no shadowing, last declaration of a name wins), which is why the type it
    /// yields is only ever *offered* to `resolve_scoped_callee`: it is used
    /// when a namesake is actually defined on that type, and dropped otherwise.
    fn note_local_types(
        &self,
        current: Node,
        source: &[u8],
        ctx: CallCtx,
        locals: &mut HashMap<String, LocalKind>,
    ) {
        {
            match current.kind() {
                // `func f(svc sfx_use.SfxOperationUse)`, and Go's `var x T`.
                "parameter_declaration" | "var_spec" | "const_spec" => {
                    if let Some(declared) = current.child_by_field_name("type") {
                        // A parameter typed `error`, `[]byte` or
                        // `sync.WaitGroup` is worth recording as foreign: it
                        // says a call on that name is not a call into this
                        // repository at all.
                        let kind = match types::classify_type(declared, source) {
                            types::TypeRef::Named { name, package } => {
                                if self.package_is_indexed(package.as_deref(), ctx) {
                                    LocalKind::Type(name)
                                } else {
                                    LocalKind::Foreign
                                }
                            }
                            types::TypeRef::Foreign => LocalKind::Foreign,
                            types::TypeRef::Unknown => return,
                        };
                        for name in Self::declared_names(current, source) {
                            locals.insert(name, kind.clone());
                        }
                    }
                }
                // `svc := NewSfxOperationUse(c)`, `dto := &PayoutDto{}`.
                "short_var_declaration" | "assignment_statement" => {
                    let names = current
                        .child_by_field_name("left")
                        .map(|left| Self::declared_names(left, source))
                        .unwrap_or_default();
                    let values: Vec<Node> = current
                        .child_by_field_name("right")
                        .map(|right| {
                            let mut walker = right.walk();
                            right
                                .children(&mut walker)
                                .filter(|child| child.is_named())
                                .collect()
                        })
                        .unwrap_or_default();
                    // One call on the right, and either one name or the Go
                    // idiom `value, err := f()` — where the extra names are
                    // the error and the discards, so the declared result type
                    // belongs to the first. Any other multi-value shape says
                    // nothing about which value is which.
                    // `self.field = f()` binds no plain identifier, so the
                    // name list can be empty — check that before slicing it.
                    let single_meaningful_name = !names.is_empty()
                        && names[1..]
                            .iter()
                            .all(|name| matches!(name.as_str(), "err" | "ok" | "_"));
                    if values.len() == 1 && single_meaningful_name {
                        if let Some(type_name) = self.expression_type_hint(values[0], source, ctx) {
                            locals.insert(names[0].clone(), LocalKind::Type(type_name));
                        }
                    }
                }
                _ => {}
            }
        }
    }

    /// The identifiers a declaration or assignment target binds.
    fn declared_names(node: Node, source: &[u8]) -> Vec<String> {
        let mut names = Vec::new();
        let mut cursor = node.walk();
        if matches!(node.kind(), "identifier" | "field_identifier") {
            if let Ok(text) = node.utf8_text(source) {
                names.push(text.to_string());
            }
            return names;
        }
        // A declaration names one or more identifiers before its type; an
        // `expression_list` on the left of `:=` holds them directly.
        for child in node.children(&mut cursor) {
            match child.kind() {
                "identifier" => {
                    if let Ok(text) = child.utf8_text(source) {
                        names.push(text.to_string());
                    }
                }
                "expression_list" | "identifier_list" => {
                    names.extend(Self::declared_names(child, source));
                }
                _ => {}
            }
        }
        names
    }

    /// The type an initialiser expression produces, as far as it can be read
    /// off the syntax: a composite literal names it outright, a factory or
    /// constructor call names it through
    /// [`factory_type_hint`](Self::factory_type_hint).
    fn expression_type_hint(&self, node: Node, source: &[u8], ctx: CallCtx) -> Option<String> {
        match node.kind() {
            "composite_literal" => node
                .child_by_field_name("type")
                .and_then(|node| types::type_name_of(node, source)),
            // `&T{}`, `*p`, `(expr)`.
            "unary_expression" | "parenthesized_expression" | "pointer_expression" => {
                let inner = node
                    .child_by_field_name("operand")
                    .or_else(|| node.child_by_field_name("argument"))
                    .or_else(|| node.named_child(0))?;
                self.expression_type_hint(inner, source, ctx)
            }
            kind if Self::is_call_kind(kind) => {
                let (callee, _) = Self::callee_slot(node)?;
                let described = self.describe_callee_in(callee, source, ctx)?;
                if let Some(declared) = self.call_result_type(&described, ctx) {
                    return Some(declared);
                }
                Self::factory_type_hint(&described.0)
            }
            _ => None,
        }
    }

    /// Package name (or alias) -> import path, for the file's imports.
    ///
    /// Go's `assert.Len(t, x, 3)` and `config.Get()` look identical to the
    /// resolver: a qualifier that names neither a receiver type nor a local
    /// variable. The import list is what tells them apart — one is a third
    /// party the index has never seen, the other is a package of this very
    /// repository — and [`resolve_package_qualified`](Self::resolve_package_qualified)
    /// turns that into either an honest `Unresolved` or a target narrowed to
    /// the right directory.
    ///
    /// Go-shaped `import_spec` only. Python's `from x import y` and JS's
    /// namespace imports bind names differently and would need their own
    /// handling rather than sharing this one.
    fn collect_imports(tree: &Tree, source: &[u8]) -> HashMap<String, String> {
        let mut imports = HashMap::new();
        let mut cursor = tree.walk();
        let mut stack = vec![tree.root_node()];
        while let Some(node) = stack.pop() {
            if node.kind() == "import_spec" {
                let Some(path) = node
                    .child_by_field_name("path")
                    .and_then(|path| path.utf8_text(source).ok())
                    .map(|path| path.trim_matches(['"', '`', '\''].as_slice()).to_string())
                else {
                    continue;
                };
                let alias = node
                    .child_by_field_name("name")
                    .and_then(|name| name.utf8_text(source).ok())
                    .map(|name| name.to_string());
                let name = match alias.as_deref() {
                    // `_ "pkg"` imports for side effects and `. "pkg"` dumps
                    // the package into the file scope: neither binds a
                    // qualifier a call site can carry.
                    Some("_") | Some(".") => continue,
                    Some(alias) => alias.to_string(),
                    None => path.rsplit('/').next().unwrap_or(&path).to_string(),
                };
                imports.insert(name, path);
                continue;
            }
            for child in node.children(&mut cursor) {
                // Imports sit at the top of a file; no need to walk into
                // function bodies looking for them.
                if child.is_named() && !child.kind().contains("function") {
                    stack.push(child);
                }
            }
        }
        imports
    }

    /// The directory of the indexed package an import path names, or `None`
    /// when no indexed file lives there — meaning the package is third-party,
    /// standard library, or simply not part of this index.
    ///
    /// Matched on the trailing path segments rather than the full import path,
    /// because the import path carries the module prefix
    /// (`github.com/org/repo/internal/config`) while indexed paths are
    /// repository-relative (`internal/config/config.go`).
    fn package_dir_for(&self, import_path: &str) -> Option<Arc<str>> {
        if let Some(cached) = self.import_dir_cache.get(import_path) {
            return cached.clone();
        }
        let needle = format!("{}/", import_path.trim_end_matches('/'));
        // Either string may carry a prefix the other lacks — the import path
        // its module prefix (`github.com/org/repo/`), the indexed path a
        // checkout subdirectory (`go-rest-api/`) — so the shorter one has to
        // be a trailing part of the longer. Two segments minimum, or a
        // third-party `.../model` would be answered with this repo's `model/`.
        let matches_dir = |dir: &str| {
            let (short, long) = if dir.len() <= needle.len() {
                (dir, needle.as_str())
            } else {
                (needle.as_str(), dir)
            };
            short.matches('/').count() >= 2 && long.ends_with(short)
        };
        let mut found: Option<Arc<str>> = None;
        for entry in self.file_functions.iter() {
            let file_path = entry.key();
            let Some(dir_end) = file_path.rfind('/') else {
                continue;
            };
            let dir = &file_path[..=dir_end];
            if matches_dir(dir) {
                found = Some(Arc::from(dir));
                break;
            }
        }
        self.import_dir_cache
            .insert(import_path.to_string(), found.clone());
        found
    }

    /// Is the package a written type came from part of this index?
    ///
    /// `sync.WaitGroup` and `gorm.DB` name types no method of this repository
    /// sits on, and the import list is what says so. A type written without a
    /// package is local to the file's own package, hence indexed by
    /// definition.
    fn package_is_indexed(&self, package: Option<&str>, ctx: CallCtx) -> bool {
        let Some(package) = package else {
            return true;
        };
        match ctx.imports.get(package) {
            Some(import_path) => self.package_dir_for(import_path).is_some(),
            // Not an import at all: a package name that came from somewhere
            // this reading does not model. Left alone rather than declared
            // foreign.
            None => true,
        }
    }

    /// Resolve `pkg.Func()` using the file's import of `pkg`.
    ///
    /// Two outcomes, both better than the name match this replaces: a package
    /// outside the index resolves to nothing at all (a call to
    /// `assert.Len(t, x, 3)` must not be reported as a call to some `Len`
    /// method of this repository), and a package inside it narrows the
    /// namesakes to that one directory.
    fn resolve_package_qualified(
        &self,
        bare_name: &str,
        import_path: &str,
    ) -> Option<(String, CallResolution)> {
        let Some(dir) = self.package_dir_for(import_path) else {
            // The import names a package this index does not contain: the
            // call is third-party, not a gap in what was read.
            return Some((bare_name.to_string(), CallResolution::ExternalPackage));
        };
        // The package is indexed and the graph has never heard the name: the
        // same gap as the `0` arm below, reached one step earlier. Falling
        // through to the general resolution would only relabel it as the
        // residual `Unresolved`, losing the one thing the call site said.
        let Some(candidates) = self.name_index.get(bare_name) else {
            return Some((bare_name.to_string(), CallResolution::PackageMissing));
        };
        let in_package: Vec<&String> = candidates
            .iter()
            .filter(|key| {
                // A Go package is exactly one directory: a subdirectory is a
                // different package, so `config.Get()` must not see the `Get`
                // of `internal/config/settings/repository`. And `pkg.Name()`
                // names a package-level function, never a method — a method
                // needs a value in front of it.
                self.nodes
                    .get(key.as_str())
                    .map(|node| {
                        node.receiver.is_none()
                            && node
                                .file_path
                                .rfind('/')
                                .map(|end| &node.file_path[..=end] == dir.as_ref())
                                .unwrap_or(false)
                    })
                    .unwrap_or(false)
            })
            .collect();
        match in_package.len() {
            // The package is indexed but carries no such function: a method on
            // one of its types reached through a value, or an unindexed file.
            0 => Some((bare_name.to_string(), CallResolution::PackageMissing)),
            1 => Some((in_package[0].clone(), CallResolution::Scope)),
            // Two package-level functions of one name cannot both exist in Go,
            // so this means the package directory was matched loosely (a
            // vendored copy, say). The pick is at least from one package.
            _ => Some((
                in_package.iter().min().map(|key| (*key).clone())?,
                CallResolution::NameOnly,
            )),
        }
    }

    fn extract_calls(&self, path: &str, content: &str, tree: &Tree) -> Result<()> {
        let source = content.as_bytes();
        let mut cursor = tree.walk();

        // Track current function scope
        let imports = Self::collect_imports(tree, source);
        let file = FileScope {
            path,
            imports: &imports,
        };
        let mut scope = FnScope::default();
        let mut package_locals = HashMap::new();
        self.walk_for_calls(
            &mut cursor,
            source,
            &file,
            &mut scope,
            &mut package_locals,
            "",
        );

        Ok(())
    }

    /// `scope` carries what the enclosing function contributes to resolution:
    /// its receiver type for `self` call sites and the declared types of its
    /// parameters and locals (see [`scope_hint_from`](Self::scope_hint_from)
    /// and [`collect_local_types`](Self::collect_local_types)).
    fn walk_for_calls(
        &self,
        cursor: &mut tree_sitter::TreeCursor,
        source: &[u8],
        file: &FileScope,
        scope: &mut FnScope,
        package_locals: &mut HashMap<String, LocalKind>,
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
                    let receiver = extract_receiver_type(node, source);
                    scope.function =
                        Some(Self::definition_key(file.path, receiver.as_deref(), &name));
                    scope.receiver = receiver;
                    scope.locals.clear();
                }
            }

            // Declarations are noted as the walk passes them, in the same
            // pass as the calls rather than in a sweep of each function's
            // subtree — which doubled the work over every function body. The
            // walk is in source order, so a declaration is seen before the
            // calls below it, which is also the only place its type can apply.
            // Noted into a separate map first: reading a declaration may
            // itself consult what is already in scope, and the borrow checker
            // will not have the table read and written at once.
            let noted = {
                let ctx = CallCtx {
                    file: file.path,
                    self_type: scope.receiver.as_deref(),
                    locals: &scope.locals,
                    file_locals: package_locals,
                    imports: file.imports,
                };
                let mut noted = HashMap::new();
                self.note_local_types(node, source, ctx, &mut noted);
                noted
            };
            // Outside any function the declaration belongs to the package and
            // stays in scope for every function in the file.
            if scope.function.is_none() {
                package_locals.extend(noted);
            } else {
                scope.locals.extend(noted);
            }

            let ctx = CallCtx {
                file: file.path,
                self_type: scope.receiver.as_deref(),
                locals: &scope.locals,
                file_locals: package_locals,
                imports: file.imports,
            };

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
                if let Some(ref caller_key) = scope.function {
                    if let Some((edge, scope_inferred)) = self.extract_call_edge(node, source, ctx)
                    {
                        // A qualifier naming a local variable or parameter
                        // carries its declared type a few lines up. Taken only
                        // when a namesake really is defined on that type — the
                        // reading of the declaration is shallow (see
                        // `collect_local_types`), so a miss must cost nothing.
                        let hint = edge.scope_hint.as_deref();
                        let by_local = hint
                            .and_then(|hint| ctx.local_type(hint))
                            .map(|declared| {
                                self.resolve_scoped_callee(
                                    &edge.target,
                                    ctx.file,
                                    Some(declared),
                                    false,
                                )
                            })
                            .filter(|(_, resolution)| *resolution == CallResolution::Receiver)
                            // A name declared as something outside this
                            // repository answers the question by itself: the
                            // call leaves the graph.
                            .or_else(|| {
                                hint.filter(|hint| ctx.is_foreign_local(hint))
                                    .map(|_| (edge.target.clone(), CallResolution::ExternalPackage))
                            });

                        // A qualifier that is not a shadowing local names an
                        // imported package: `config.Get()` is a call into this
                        // repository's `internal/config`, and `assert.Len(...)`
                        // is a call into testify — which the graph must report
                        // as absent rather than as some `Len` of this
                        // repository. This runs *before* the general
                        // resolution, not as a fallback for its weak answers:
                        // a package-qualified name with a single namesake in
                        // the index would otherwise be answered `Unique`,
                        // which is the same wrong target stated as a fact.
                        let by_package = by_local.is_none().then_some(()).and_then(|_| {
                            let import_path = edge
                                .scope_hint
                                .as_deref()
                                .and_then(|hint| file.imports.get(hint))?;
                            self.resolve_package_qualified(&edge.target, import_path)
                        });

                        // Resolve callee with scope hint for disambiguation
                        let (callee_key, resolution) = match by_local.or(by_package) {
                            Some(resolved) => resolved,
                            None => self.resolve_scoped_callee(
                                &edge.target,
                                file.path,
                                edge.scope_hint.as_deref(),
                                scope_inferred,
                            ),
                        };

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
                if let Some(ref caller_key) = scope.function {
                    if let Ok(macro_text) = node.utf8_text(source) {
                        let macro_line = node.start_position().row + 1;
                        self.extract_calls_from_macro_text(
                            macro_text, caller_key, file.path, macro_line,
                        );
                    }
                }
            }

            // Recurse
            if cursor.goto_first_child() {
                self.walk_for_calls(cursor, source, file, scope, package_locals, node.kind());
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

    /// Lowercased file path for `key`, memoized in `file_path_lower_cache`.
    ///
    /// `scope_matches_file_path` used to call `file_path.to_lowercase()` on
    /// every namesake candidate on every ambiguous call site — a Unicode
    /// case-fold recomputed from scratch for the same handful of file paths
    /// across the whole corpus. Profiling a real 49k-file index showed this
    /// (plus the matching `scope.to_lowercase()`, hoisted in `resolve_callee`
    /// instead) as ~5-6% of total indexing CPU. Caching it here means each
    /// distinct file path is lowercased once per process lifetime, not once
    /// per candidate per call site.
    fn key_file_path_lower(&self, key: &str) -> Option<Arc<str>> {
        if let Some(cached) = self.file_path_lower_cache.get(key) {
            return Some(cached.clone());
        }
        let file_path = self.nodes.get(key).map(|node| node.file_path.clone())?;
        let lower: Arc<str> = Arc::from(file_path.to_lowercase());
        self.file_path_lower_cache
            .insert(key.to_string(), lower.clone());
        Some(lower)
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

    /// Among `candidates` (the namesakes of one method), the one whose
    /// receiver type implements `interface_name` — provided exactly one does.
    ///
    /// "Implements" is Go's own rule read off the index: the type carries a
    /// method of every name the interface declares. Signatures are not
    /// compared, so a type could satisfy this by coincidence; requiring a
    /// single match across the whole namesake set is what keeps that from
    /// mattering in practice.
    fn only_implementor_among(
        &self,
        interface_name: &str,
        candidates: &[String],
    ) -> Option<String> {
        let required = match self.interface_methods_cache.get(interface_name) {
            Some(cached) => cached.clone(),
            None => {
                let methods = Arc::new(self.types.interface_methods(interface_name, None));
                self.interface_methods_cache
                    .insert(interface_name.to_string(), methods.clone());
                methods
            }
        };
        // An interface with one common method (`Error`, `String`) is satisfied
        // by half a corpus; there is nothing to narrow with.
        if required.len() < 2 {
            return None;
        }
        let implementors: Vec<&String> = candidates
            .iter()
            .filter(|key| {
                self.nodes
                    .get(key.as_str())
                    .and_then(|node| node.receiver.clone())
                    .map(|receiver| self.type_implements(interface_name, &receiver, &required))
                    .unwrap_or(false)
            })
            .collect();
        match implementors.len() {
            0 => None,
            1 => Some(implementors[0].clone()),
            // A mock in the test tree implements the same interface as the
            // real thing. Production code is what a caller outside the tests
            // reaches, so it wins — and if the remaining choice is still not
            // single, nothing here can make it.
            _ => {
                let mut production = implementors.iter().filter(|key| !Self::is_test_path(key));
                let first = production.next()?;
                production.next().is_none().then(|| (*first).clone())
            }
        }
    }

    /// Is this graph key from test or mock code? Read off the path, which is
    /// where every language convention for it shows up.
    fn is_test_path(key: &str) -> bool {
        let lower = key.to_lowercase();
        lower.contains("/test/")
            || lower.contains("/tests/")
            || lower.contains("/mock/")
            || lower.contains("/mocks/")
            || lower.contains("_test.")
            || lower.contains("test_helper")
            || lower.starts_with("test/")
    }

    /// Does a type carry a method of every one of these names? Memoized per
    /// `interface + type` pair: the walk below touches every namesake of every
    /// method the interface declares, and the same pairs recur across a corpus.
    fn type_implements(&self, interface_name: &str, type_name: &str, methods: &[String]) -> bool {
        let cache_key = format!("{interface_name}\u{1}{type_name}");
        if let Some(cached) = self.implements_cache.get(&cache_key) {
            return *cached;
        }
        let answer = self.type_has_all_methods(type_name, methods);
        self.implements_cache.insert(cache_key, answer);
        answer
    }

    /// Does a type carry a method of every one of these names?
    fn type_has_all_methods(&self, type_name: &str, methods: &[String]) -> bool {
        methods.iter().all(|method| {
            self.name_index
                .get(method)
                .map(|candidates| {
                    candidates
                        .iter()
                        .any(|key| self.receiver_matches(key, type_name, true))
                })
                .unwrap_or(false)
        })
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
        self.resolve_scoped_callee(bare_name, caller_file, scope_hint, false)
    }

    /// [`resolve_callee`](Self::resolve_callee) with the provenance of the
    /// hint. `scope_inferred` marks a hint guessed from a factory name rather
    /// than written at the call site: a receiver match on it is reported as
    /// [`CallResolution::FactoryReceiver`], not as a read qualifier.
    fn resolve_scoped_callee(
        &self,
        bare_name: &str,
        caller_file: &str,
        scope_hint: Option<&str>,
        scope_inferred: bool,
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
                        let resolution = if scope_inferred {
                            CallResolution::FactoryReceiver
                        } else {
                            CallResolution::Receiver
                        };
                        return (first.clone(), resolution);
                    }
                }

                // 0c. The qualifier names an interface: dispatch is dynamic,
                //     but if exactly one type in the index carries every
                //     method the interface declares, that is where the call
                //     goes. Checked against the namesakes rather than against
                //     every struct — there are a handful of those and
                //     thousands of these.
                if self
                    .types
                    .is_interface(scope, Some(types::directory_of(caller_file)))
                {
                    if let Some(key) = self.only_implementor_among(scope, &candidates) {
                        return (key, CallResolution::InterfaceImpl);
                    }
                }

                // 0b. Go promotes the methods of an embedded type onto the
                //     outer one, so `self.HandleError(err)` inside a method of
                //     `CustomerRepo { BaseRepo }` is `BaseRepo::HandleError`.
                //     The receiver type is right, the method just lives one
                //     level in — a rule of the language, not a guess, hence
                //     the same resolution as a direct receiver match.
                let prefer_dir = types::directory_of(caller_file);
                for embedded in self.types.embedded_chain(scope, Some(prefer_dir)) {
                    let mut promoted = candidates
                        .iter()
                        .filter(|key| self.receiver_matches(key, &embedded, true));
                    if let Some(first) = promoted.next() {
                        // Two embedded types carrying the same method is a
                        // compile error in Go, so an ambiguity here means the
                        // reading is off; leave it to the steps below.
                        if promoted.next().is_none() {
                            let resolution = if scope_inferred {
                                CallResolution::FactoryReceiver
                            } else {
                                CallResolution::Receiver
                            };
                            return (first.clone(), resolution);
                        }
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
            // Every namesake was removed (its file was re-indexed) but the
            // empty entry stayed behind. Falling through would hand the edge
            // an empty target labelled `NameOnly`.
            0 => (bare_name.to_string(), CallResolution::Unresolved),
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

                // 4. If scope_hint present, try to narrow down. `scope` is the
                // same string for every candidate here, so its path-fragment
                // form is computed once rather than per candidate (this loop
                // used to re-lowercase `scope` — a Unicode case-fold — on
                // every namesake; with "hundreds of candidates" per the O(1)
                // lookup comment above, that was pure waste).
                // An inferred hint is a type name, not a module path: feeding
                // `SfxOperationUse` to the path filter would match whatever
                // file happens to sit under a similarly named directory.
                if let Some(scope) = scope_hint.filter(|_| !scope_inferred) {
                    let scope_path = Self::scope_path_fragment(scope);
                    let mut scope_matches = candidates.iter().filter(|key| {
                        self.key_file_path_lower(key)
                            .map(|file_path_lower| {
                                Self::path_fragment_matches(&scope_path, &file_path_lower)
                            })
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

    /// Lowercases `scope`, strips a `crate::`/`self::`/`super::` prefix, and
    /// converts `::` to `/` — the path-fragment form `path_fragment_matches`
    /// checks a file path against. Split out of the old single-shot
    /// `scope_matches_file_path` so a caller checking many candidates against
    /// the same scope (`resolve_callee`'s namesake filter) computes this once
    /// instead of once per candidate.
    fn scope_path_fragment(scope: &str) -> String {
        let scope_lower = scope.to_lowercase();
        let stripped = scope_lower
            .strip_prefix("crate::")
            .or_else(|| scope_lower.strip_prefix("self::"))
            .or_else(|| scope_lower.strip_prefix("super::"))
            .unwrap_or(&scope_lower);
        stripped.replace("::", "/")
    }

    /// Does an already-lowercased file path contain `/{scope_path}/` or
    /// `/{scope_path}.`? `scope_path` comes from [`scope_path_fragment`] and
    /// `file_path_lower` from [`key_file_path_lower`] — both precomputed so
    /// this call itself does no case-folding.
    fn path_fragment_matches(scope_path: &str, file_path_lower: &str) -> bool {
        let with_slash = format!("/{}/", scope_path);
        let with_dot = format!("/{}.", scope_path);
        file_path_lower.contains(&with_slash) || file_path_lower.contains(&with_dot)
    }

    /// Single-shot convenience combining [`scope_path_fragment`] and
    /// [`path_fragment_matches`] for callers checking one scope against one
    /// file path (not a per-candidate loop, where computing the fragment
    /// once outside the loop is what actually matters).
    #[cfg(test)]
    fn scope_matches_file_path(scope: &str, file_path: &str) -> bool {
        Self::path_fragment_matches(&Self::scope_path_fragment(scope), &file_path.to_lowercase())
    }

    /// Text of a receiver/qualifier node, when it can plausibly narrow a
    /// callee down to a file.
    ///
    /// A `self`/`this` receiver names no type at the call site, but the
    /// enclosing method does: inside `func (self *BaseApplication) Start()`,
    /// `self.Logger()` means `BaseApplication::Logger`. `self_type` carries
    /// that type down from [`walk_for_calls`](Self::walk_for_calls), so such a
    /// call resolves on the receiver instead of falling through to a name
    /// match among namesakes — which, for a method name as common as `Logger`,
    /// picked an unrelated type. Without a known enclosing type the receiver
    /// is dropped as before.
    fn scope_hint_from(node: Node, source: &[u8], self_type: Option<&str>) -> Option<String> {
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
        if text.is_empty() {
            return None;
        }
        if matches!(text, "self" | "Self" | "this") {
            return self_type.map(|owner| owner.to_string());
        }
        Some(text.to_string())
    }

    /// Call shapes whose callee slot [`callee_slot`](Self::callee_slot) knows.
    fn is_call_kind(kind: &str) -> bool {
        matches!(
            kind,
            "call_expression"
                | "call"
                | "method_call_expression"
                | "invocation_expression"
                | "method_invocation"
                | "function_call_expression"
                | "member_call_expression"
                | "scoped_call_expression"
        )
    }

    /// A type name guessed from a factory or constructor function name.
    ///
    /// Needed because a call chained onto a factory names no receiver type at
    /// the call site: `self.Depend.SfxOperationUseFactory(self.C).PayOperation()`
    /// carries nothing to tell `PayOperation` on `SfxOperationUse` apart from
    /// the same method on five other types — yet the factory's own name says
    /// which one. Same for a constructor: `NewStore().get()` -> `Store`.
    ///
    /// Only names carrying one of the conventions produce a hint, so a plain
    /// `lookup(id).run()` stays a name match instead of inventing a type. The
    /// hint is a hypothesis either way: [`resolve_callee`](Self::resolve_callee)
    /// keeps it only if a namesake is actually defined on a type of that name.
    fn factory_type_hint(name: &str) -> Option<String> {
        const CAMEL_SUFFIXES: [&str; 3] = ["Factory", "Provider", "Builder"];
        const SNAKE_SUFFIXES: [&str; 3] = ["_factory", "_provider", "_builder"];
        const CAMEL_PREFIXES: [&str; 2] = ["New", "Make"];
        const SNAKE_PREFIXES: [&str; 2] = ["new_", "make_"];

        let stem = CAMEL_SUFFIXES
            .iter()
            .chain(SNAKE_SUFFIXES.iter())
            .find_map(|suffix| name.strip_suffix(suffix))
            .or_else(|| {
                // `NewsFeed` must not be read as `New` + `sFeed`: after a
                // CamelCase prefix the type name starts with a capital.
                CAMEL_PREFIXES.iter().find_map(|prefix| {
                    name.strip_prefix(prefix)
                        .filter(|rest| rest.starts_with(char::is_uppercase))
                })
            })
            .or_else(|| {
                SNAKE_PREFIXES
                    .iter()
                    .find_map(|prefix| name.strip_prefix(prefix))
            })?;

        let stem = stem.trim_matches('_');
        if stem.len() < 2 {
            return None;
        }
        Some(Self::to_upper_camel(stem))
    }

    /// `store` -> `Store`, `sfx_operation` -> `SfxOperation`, `SfxOp` -> `SfxOp`.
    /// Receiver types are CamelCase in every language where the factory
    /// convention above appears, while function names may be snake_case.
    fn to_upper_camel(name: &str) -> String {
        if !name.contains('_') {
            let mut chars = name.chars();
            return match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            };
        }
        name.split('_')
            .filter(|segment| !segment.is_empty())
            .map(|segment| {
                let mut chars = segment.chars();
                match chars.next() {
                    Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                    None => String::new(),
                }
            })
            .collect()
    }

    /// Scope hint for a call chained onto another call — the DI-factory and
    /// constructor shape `factory(args).method()`, where the receiver is an
    /// expression rather than a name and [`scope_hint_from`](Self::scope_hint_from)
    /// has nothing to offer. Falls back to the callee name of the inner call,
    /// turned into a type name by [`factory_type_hint`](Self::factory_type_hint).
    /// The type of a selector chain used as a receiver: `self.C`,
    /// `self.Depend.CustomerDocumentExtService`, `svc.Repo`.
    ///
    /// The base is either `self` — whose type the enclosing method declares —
    /// or a name in scope, and each step after it is a field whose type the
    /// struct declaration names. This is the shape behind
    /// `self.C.Logger().SendError(err)`: nothing at the call site says what
    /// `self.C` is, but the struct says `C ctx.Ctx` and the interface says
    /// `Logger() *Logger`.
    ///
    /// Bounded at four links: a longer chain is not worth a walk on every call
    /// site of a large corpus, and does not occur in practice.
    fn selector_chain_type(&self, node: Node, source: &[u8], ctx: CallCtx) -> Option<String> {
        let mut steps: Vec<String> = Vec::new();
        let mut current = node;
        loop {
            match current.kind() {
                "selector_expression" | "field_expression" | "member_expression" => {
                    if steps.len() >= 4 {
                        return None;
                    }
                    let field = current
                        .child_by_field_name("field")
                        .or_else(|| current.child_by_field_name("property"))
                        .or_else(|| current.child_by_field_name("name"))?;
                    steps.push(field.utf8_text(source).ok()?.to_string());
                    current = current
                        .child_by_field_name("operand")
                        .or_else(|| current.child_by_field_name("object"))
                        .or_else(|| current.child_by_field_name("argument"))?;
                }
                // The base of the chain.
                "identifier" | "field_identifier" => {
                    let base = current.utf8_text(source).ok()?;
                    let mut type_name = match base {
                        "self" | "this" | "Self" => ctx.self_type?.to_string(),
                        name => ctx.local_type(name)?.to_string(),
                    };
                    // Walked outermost-first, so the fields apply in reverse.
                    for field in steps.iter().rev() {
                        type_name = self.types.field_type(
                            &type_name,
                            field,
                            Some(types::directory_of(ctx.file)),
                        )?;
                    }
                    return Some(type_name);
                }
                _ => return None,
            }
        }
    }

    /// The type a method call on `receiver_type` produces, for the middle of a
    /// chain: the graph's own node when the type is a struct, and the
    /// interface's declaration when it is an interface — an interface method
    /// has no body and therefore no node, but it does state its result type.
    fn method_result_type(
        &self,
        receiver_type: &str,
        method: &str,
        prefer_dir: Option<&str>,
    ) -> Option<String> {
        if let Some(declared) =
            self.types
                .interface_method_result(receiver_type, method, prefer_dir)
        {
            return Some(declared);
        }
        let candidates = self.name_index.get(method)?;
        let mut on_receiver = candidates
            .iter()
            .filter(|key| self.receiver_matches(key, receiver_type, true));
        let first = on_receiver.next()?;
        if on_receiver.next().is_some() {
            return None;
        }
        self.nodes
            .get(first.as_str())
            .and_then(|node| node.result_type.clone())
    }

    fn chained_call_scope_hint(
        &self,
        operand: Node,
        source: &[u8],
        ctx: CallCtx,
    ) -> Option<(String, bool)> {
        if !Self::is_call_kind(operand.kind()) {
            return None;
        }
        // What the inner call declares it returns beats what its name
        // suggests: `accountRepo(ctx)` is an ordinary helper whose name says
        // nothing, while its result list says everything. The naming
        // convention stays as the answer for calls the graph cannot identify
        // — a factory held in a struct field, most of this corpus.
        let (callee, _) = Self::callee_slot(operand)?;
        let described = self.describe_callee_in(callee, source, ctx)?;
        // A declared result type was read; a factory name was only inferred.
        // The flag is what keeps the two apart in the reported resolution.
        if let Some(declared) = self.call_result_type(&described, ctx) {
            return Some((declared, false));
        }
        // The inner call sits on a typed receiver of its own
        // (`self.C.Logger()`), so its result can be read off that type's
        // declaration even when the method itself is not a graph node.
        if let Some(receiver_type) = described.2.as_deref() {
            if let Some(declared) = self.method_result_type(
                receiver_type,
                &described.0,
                Some(types::directory_of(ctx.file)),
            ) {
                return Some((declared, false));
            }
        }
        Self::factory_type_hint(&described.0).map(|inferred| (inferred, true))
    }

    /// Describe the callee slot of a call:
    /// `(name, call type, scope hint, hint was inferred)`.
    ///
    /// Each language spells a method callee differently, and the name always
    /// lives in a named field: Go `selector_expression.field`, Python
    /// `attribute.attribute`, JS/TS `member_expression.property`, C#
    /// `member_access_expression.name`, Kotlin/Swift `navigation_expression`'s
    /// trailing `navigation_suffix`, Rust/C `field_expression.field`.
    /// Returns `None` for shapes it does not know so the caller can fall back.
    /// The last element is set when the scope hint was inferred from a factory
    /// name rather than read off the call site (see
    /// [`chained_call_scope_hint`](Self::chained_call_scope_hint)).
    fn describe_callee_in(
        &self,
        node: Node,
        source: &[u8],
        ctx: CallCtx,
    ) -> Option<(String, CallType, Option<String>, bool)> {
        let kind = node.kind();
        let text_of = |n: Node| n.utf8_text(source).ok().map(|s| s.to_string());

        match kind {
            "identifier"
            | "field_identifier"
            | "property_identifier"
            | "simple_identifier"
            | "name"
            | "constant" => Some((text_of(node)?, CallType::Direct, None, false)),

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
                let receiver = node
                    .child_by_field_name("object")
                    .or_else(|| node.child_by_field_name("operand"))
                    .or_else(|| node.child_by_field_name("expression"))
                    .or_else(|| node.child_by_field_name("value"))
                    .or_else(|| node.child_by_field_name("argument"));
                let scope =
                    receiver.and_then(|node| Self::scope_hint_from(node, source, ctx.self_type));
                if scope.is_none() {
                    if let Some((chained, inferred)) =
                        receiver.and_then(|node| self.chained_call_scope_hint(node, source, ctx))
                    {
                        return Some((name, CallType::Method, Some(chained), inferred));
                    }
                    // `self.Depend.X.M()`: the receiver is a chain of fields,
                    // and every link's type is declared somewhere.
                    if let Some(declared) =
                        receiver.and_then(|node| self.selector_chain_type(node, source, ctx))
                    {
                        return Some((name, CallType::Method, Some(declared), false));
                    }
                }
                Some((name, CallType::Method, scope, false))
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
                let target = node
                    .child_by_field_name("target")
                    .or_else(|| node.named_child(0));
                let scope =
                    target.and_then(|node| Self::scope_hint_from(node, source, ctx.self_type));
                if scope.is_none() {
                    if let Some((chained, inferred)) =
                        target.and_then(|node| self.chained_call_scope_hint(node, source, ctx))
                    {
                        return Some((name, CallType::Method, Some(chained), inferred));
                    }
                }
                Some((name, CallType::Method, scope, false))
            }

            "scoped_identifier" | "qualified_identifier" => {
                let scope = Self::extract_scope_qualifier(node, source);
                let name = node
                    .child_by_field_name("name")
                    .and_then(text_of)
                    .or_else(|| self.get_last_identifier(node, source))?;
                Some((name, CallType::StaticMethod, scope, false))
            }

            _ => None,
        }
    }

    /// Locate the callee expression of a call node through tree-sitter fields
    /// and turn it into an edge. Returns `None` for call shapes without a known
    /// callee field (BSL among them) so [`extract_call_edge`](Self::extract_call_edge)
    /// can fall back to its sibling scan.
    /// The callee node of a call, plus a separate receiver node for the shapes
    /// where the callee itself does not carry one (Ruby `call`, Java/PHP method
    /// invocations). `None` for call shapes without a known callee field.
    fn callee_slot(node: Node) -> Option<(Node, Option<Node>)> {
        match node.kind() {
            "call_expression"
            | "call"
            | "method_call_expression"
            | "invocation_expression"
            | "function_call_expression" => {
                if let Some(function) = node.child_by_field_name("function") {
                    Some((function, None))
                } else if let Some(method) = node.child_by_field_name("method") {
                    // Ruby: `(call receiver: (identifier) method: (identifier))`
                    Some((method, node.child_by_field_name("receiver")))
                } else {
                    // Kotlin/Swift: callee and `call_suffix` are unnamed children.
                    let mut walker = node.walk();
                    let callee = node.children(&mut walker).find(|child| {
                        child.is_named()
                            && !child.kind().contains("argument")
                            && child.kind() != "call_suffix"
                    })?;
                    Some((callee, None))
                }
            }
            // Java `obj.m()`, PHP `$obj->m()` and `Obj::m()`.
            "method_invocation" | "member_call_expression" | "scoped_call_expression" => Some((
                node.child_by_field_name("name")?,
                node.child_by_field_name("object")
                    .or_else(|| node.child_by_field_name("scope")),
            )),
            _ => None,
        }
    }

    /// The edge plus whether its scope hint was inferred rather than read; see
    /// [`extract_call_edge`](Self::extract_call_edge).
    fn extract_call_edge_by_fields(
        &self,
        node: Node,
        source: &[u8],
        ctx: CallCtx,
    ) -> Option<(CallEdge, bool)> {
        let kind = node.kind();
        let (callee, receiver) = Self::callee_slot(node)?;

        let (target, mut call_type, mut scope_hint, mut scope_inferred) =
            self.describe_callee_in(callee, source, ctx)?;

        if let Some(receiver) = receiver {
            call_type = if kind == "scoped_call_expression" {
                CallType::StaticMethod
            } else {
                CallType::Method
            };
            scope_hint = Self::scope_hint_from(receiver, source, ctx.self_type);
            scope_inferred = false;
            if scope_hint.is_none() {
                if let Some((chained, inferred)) =
                    self.chained_call_scope_hint(receiver, source, ctx)
                {
                    scope_hint = Some(chained);
                    scope_inferred = inferred;
                }
            }
        }

        Some((
            CallEdge {
                target,
                file_path: ctx.file.to_string(),
                line: node.start_position().row + 1,
                column: node.start_position().column + 1,
                call_type,
                scope_hint,
                resolved: false,
                resolution: CallResolution::Unknown,
            },
            scope_inferred,
        ))
    }

    /// The edge for a call site, plus whether its `scope_hint` was inferred
    /// from a factory name instead of read off the call site — the flag
    /// `resolve_callee` needs to label the match honestly.
    fn extract_call_edge(
        &self,
        node: Node,
        source: &[u8],
        ctx: CallCtx,
    ) -> Option<(CallEdge, bool)> {
        if let Some(found) = self.extract_call_edge_by_fields(node, source, ctx) {
            return Some(found);
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

        target.map(|name| {
            (
                CallEdge {
                    target: name,
                    file_path: ctx.file.to_string(),
                    line: node.start_position().row + 1,
                    column: node.start_position().column + 1,
                    call_type,
                    scope_hint,
                    resolved: false,
                    resolution: CallResolution::Unknown,
                },
                false,
            )
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
                let qualifier_path = Self::scope_path_fragment(qualifier);
                let by_path = matches.iter().find(|key| {
                    let Some(file_path) = self.key_file_path(key) else {
                        return false;
                    };
                    if file_path.ends_with(qualifier) {
                        return true;
                    }
                    self.key_file_path_lower(key)
                        .map(|file_path_lower| {
                            Self::path_fragment_matches(&qualifier_path, &file_path_lower)
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
        self.callers_of(&actual_name)
    }

    /// Incoming edges for one exact key, without materialising the whole
    /// reverse adjacency.
    ///
    /// [`reverse_edges`](Self::reverse_edges) allocates a `CallEdge` with two
    /// cloned `String`s for every edge in the graph — 24k of them on narsil's
    /// own `src/`, 14.66 ms — and then discards all but one bucket. The scan
    /// is still O(E), but only the edges actually returned are cloned.
    fn callers_of(&self, callee: &str) -> Vec<CallEdge> {
        let mut callers = Vec::new();
        for entry in self.nodes.iter() {
            let caller = entry.key();
            for e in &entry.value().calls {
                if e.target != callee {
                    continue;
                }
                callers.push(CallEdge {
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
        callers
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
                    let callers = self.callers_of(display_name);
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
    /// How many edges were resolved each way, most certain first.
    ///
    /// Resolution quality is otherwise invisible: a corpus whose calls stop
    /// resolving (a grammar change, a naming convention this reading does not
    /// cover) produces exactly the same number of edges, pointing at worse
    /// targets. This is what makes that visible in `get_index_status`.
    ///
    /// The three "not in graph" rows are reported apart because one lumped
    /// figure could not be acted on. Half the edges of a Go corpus leaving
    /// the graph is normal — those are calls into the standard library and
    /// into dependencies nobody indexed — while an *indexed* package with no
    /// such function is a gap in what was read, and a name declared nowhere
    /// at all sits between the two. Read together they were indistinguishable,
    /// so a healthy 47% looked like half a lost graph.
    pub fn resolution_stats(&self) -> Vec<(&'static str, usize)> {
        let mut counts = [0usize; 10];
        for node in self.nodes.iter() {
            for edge in &node.calls {
                let slot = match edge.resolution {
                    CallResolution::Unique => 0,
                    CallResolution::Receiver => 1,
                    CallResolution::Scope => 2,
                    CallResolution::InterfaceImpl => 3,
                    CallResolution::FactoryReceiver => 4,
                    CallResolution::SameFile => 5,
                    CallResolution::NameOnly => 6,
                    CallResolution::ExternalPackage => 7,
                    CallResolution::PackageMissing => 8,
                    CallResolution::Unresolved | CallResolution::Unknown => 9,
                };
                counts[slot] += 1;
            }
        }
        vec![
            ("unique name", counts[0]),
            ("receiver type read", counts[1]),
            ("package or module scope", counts[2]),
            ("single interface implementation", counts[3]),
            ("receiver inferred from factory name", counts[4]),
            ("same file as caller", counts[5]),
            ("name match only", counts[6]),
            ("not in graph: outside the index", counts[7]),
            ("not in graph: indexed package, no such function", counts[8]),
            ("not in graph: name not declared anywhere", counts[9]),
        ]
    }

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

    /// Serialize call graph to JSON
    pub fn to_json(&self) -> Result<String> {
        let nodes: Vec<(String, CallNode)> = self
            .nodes
            .iter()
            .map(|ref_multi| (ref_multi.key().clone(), ref_multi.value().clone()))
            .collect();
        serde_json::to_string(&nodes).map_err(|e| anyhow!("Failed to serialize call-graph: {}", e))
    }

    /// Replace this graph's contents with the serialized one.
    ///
    /// Replacement, not a merge. Merging into a graph that was just built from
    /// source can only contribute staleness: a function deleted from the code
    /// comes back from the snapshot, outlives `find_symbols`, and `to_json`
    /// writes it out again on the next save, so the phantom is self-sustaining.
    ///
    /// `file_functions` is rebuilt from the loaded nodes as well. Without it
    /// the incremental re-index cannot evict anything that arrived this way —
    /// `remove_functions_for_file` looks the file up there and finds nothing.
    pub fn from_json(&self, json: &str) -> Result<()> {
        let nodes: Vec<(String, CallNode)> = serde_json::from_str(json)
            .map_err(|e| anyhow!("Failed to deserialize call-graph: {}", e))?;

        self.nodes.clear();
        self.name_index.clear();
        self.file_functions.clear();

        let mut per_file: HashMap<String, Vec<String>> = HashMap::new();
        for (key, node) in nodes {
            per_file
                .entry(node.file_path.clone())
                .or_default()
                .push(key.clone());
            self.insert_node(key, node);
        }
        for (path, keys) in per_file {
            self.file_functions.insert(path, keys);
        }
        Ok(())
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
            result_type: None,
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
            result_type: None,
        };

        let callee = CallNode {
            name: "callee".to_string(),
            file_path: "/path/to/file.rs".to_string(),
            line: 20,
            calls: Vec::new(),
            metrics: FunctionMetrics::default(),
            receiver: None,
            result_type: None,
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
                    result_type: None,
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
                result_type: None,
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
            result_type: None,
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
            result_type: None,
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
            result_type: None,
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
            result_type: None,
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
            result_type: None,
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
            result_type: None,
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
            result_type: None,
        };

        let node_d = CallNode {
            name: "d".to_string(),
            file_path: "/file.rs".to_string(),
            line: 30,
            calls: Vec::new(),
            metrics: FunctionMetrics::default(),
            receiver: None,
            result_type: None,
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
            result_type: None,
        };

        let node_c = CallNode {
            name: "c".to_string(),
            file_path: "/file.rs".to_string(),
            line: 20,
            calls: Vec::new(),
            metrics: FunctionMetrics::default(),
            receiver: None,
            result_type: None,
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
            result_type: None,
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
            result_type: None,
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
            result_type: None,
        };

        let node_d = CallNode {
            name: "d".to_string(),
            file_path: "/file.rs".to_string(),
            line: 30,
            calls: Vec::new(),
            metrics: FunctionMetrics::default(),
            receiver: None,
            result_type: None,
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
            result_type: None,
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
            result_type: None,
        };

        let node_c = CallNode {
            name: "c".to_string(),
            file_path: "/file.rs".to_string(),
            line: 20,
            calls: Vec::new(),
            metrics: FunctionMetrics::default(),
            receiver: None,
            result_type: None,
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
            result_type: None,
        };

        let node_b = CallNode {
            name: "b".to_string(),
            file_path: "/file.rs".to_string(),
            line: 10,
            calls: Vec::new(),
            metrics: FunctionMetrics::default(),
            receiver: None,
            result_type: None,
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
            result_type: None,
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
            result_type: None,
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
                    result_type: None,
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
            result_type: None,
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
                result_type: None,
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
            result_type: None,
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
            result_type: None,
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
                result_type: None,
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
            result_type: None,
        };
        let node_b = CallNode {
            name: "run".to_string(),
            file_path: "src/app/mod.rs".to_string(),
            line: 20,
            calls: Vec::new(),
            metrics: FunctionMetrics::default(),
            receiver: None,
            result_type: None,
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
            result_type: None,
        };
        let node_b = CallNode {
            name: "run".to_string(),
            file_path: "src/app/mod.rs".to_string(),
            line: 20,
            calls: Vec::new(),
            metrics: FunctionMetrics::default(),
            receiver: None,
            result_type: None,
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
            result_type: None,
        };
        let node_b = CallNode {
            name: "helper".to_string(),
            file_path: "src/utils.rs".to_string(),
            line: 10,
            calls: Vec::new(),
            metrics: FunctionMetrics::default(),
            receiver: None,
            result_type: None,
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
                result_type: None,
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

    fn node(name: &str, file: &str, receiver: Option<&str>) -> CallNode {
        CallNode {
            name: name.to_string(),
            file_path: file.to_string(),
            line: 1,
            calls: Vec::new(),
            metrics: FunctionMetrics::default(),
            receiver: receiver.map(str::to_string),
            result_type: None,
        }
    }

    /// A warm start used to merge the snapshot into the freshly built graph.
    /// Reloading the same snapshot twice then listed every function twice in
    /// `name_index`, and `resolve_callee` — which decides by candidate count —
    /// stopped resolving by receiver type: `Store::new()` landed on whatever
    /// sorted first, historically `Cache::new`.
    #[test]
    fn test_reload_does_not_duplicate_name_index_entries() {
        let graph = CallGraph::new();
        graph.insert_node(
            CallGraph::definition_key("src/store.rs", Some("Store"), "new"),
            node("new", "src/store.rs", Some("Store")),
        );
        graph.insert_node(
            CallGraph::definition_key("src/cache.rs", Some("Cache"), "new"),
            node("new", "src/cache.rs", Some("Cache")),
        );

        let snapshot = graph.to_json().unwrap();
        graph.from_json(&snapshot).unwrap();
        graph.from_json(&snapshot).unwrap();

        assert_eq!(
            graph.name_index.get("new").map(|c| c.len()),
            Some(2),
            "reloading must not multiply candidates"
        );
        let (target, resolution) = graph.resolve_callee("new", "src/main.rs", Some("Store"));
        assert_eq!(target, "src/store.rs::Store::new");
        assert_eq!(resolution, CallResolution::Receiver);
    }

    /// The snapshot replaces the graph instead of being merged into it. A
    /// function deleted from the source used to come back from disk on every
    /// warm start, outlive `find_symbols`, and be written out again by the
    /// next `to_json` — a phantom that sustained itself.
    #[test]
    fn test_from_json_replaces_rather_than_merges() {
        let stale = CallGraph::new();
        stale.insert_node(
            CallGraph::qualified_key("src/lib.rs", "deleted_fn"),
            node("deleted_fn", "src/lib.rs", None),
        );
        stale.insert_node(
            CallGraph::qualified_key("src/lib.rs", "kept_fn"),
            node("kept_fn", "src/lib.rs", None),
        );
        let snapshot = stale.to_json().unwrap();

        // The graph as rebuilt from source after `deleted_fn` was removed.
        let fresh = CallGraph::new();
        fresh.insert_node(
            CallGraph::qualified_key("src/lib.rs", "kept_fn"),
            node("kept_fn", "src/lib.rs", None),
        );
        fresh.from_json(&snapshot).unwrap();

        // Loading a snapshot means adopting it wholesale...
        assert_eq!(fresh.node_count(), 2);

        // ...and what it brings must be evictable, which needs `file_functions`
        // populated — `remove_functions_for_file` is the only thing standing
        // between an incremental re-index and a permanent phantom.
        fresh.remove_functions_for_file("src/lib.rs").unwrap();
        assert_eq!(
            fresh.node_count(),
            0,
            "loaded nodes were not registered against their file"
        );
    }

    /// `get_callers` used to build the entire reverse adjacency — a cloned
    /// `CallEdge` per edge in the graph — and then throw away every bucket but
    /// one. The targeted scan must return exactly what the map would have.
    #[test]
    fn test_callers_of_matches_reverse_edges() {
        let graph = CallGraph::new();
        for (file, name, callees) in [
            ("src/a.rs", "root", vec!["shared", "only_a"]),
            ("src/b.rs", "helper", vec!["shared"]),
            ("src/c.rs", "leaf", vec![]),
            ("src/d.rs", "shared", vec!["leaf"]),
        ] {
            let key = CallGraph::qualified_key(file, name);
            let mut n = node(name, file, None);
            n.calls = callees
                .iter()
                .enumerate()
                .map(|(i, target)| CallEdge {
                    target: (*target).to_string(),
                    file_path: file.to_string(),
                    line: i + 1,
                    column: 0,
                    call_type: CallType::Direct,
                    scope_hint: None,
                    resolved: true,
                    resolution: CallResolution::Unique,
                })
                .collect();
            graph.insert_node(key, n);
        }

        let mut rev = graph.reverse_edges();
        for callee in ["shared", "only_a", "leaf", "absent"] {
            // `CallEdge` has no `PartialEq`; compare the fields a caller of
            // `get_callers` actually reads.
            let project = |edges: Vec<CallEdge>| {
                let mut v: Vec<_> = edges
                    .into_iter()
                    .map(|e| (e.target, e.file_path, e.line, e.resolution))
                    .collect();
                // `CallResolution` is not `Ord`; a stable order over the
                // remaining fields is enough to compare the two sets.
                v.sort_by(|a, b| (&a.0, &a.1, a.2).cmp(&(&b.0, &b.1, b.2)));
                v
            };
            assert_eq!(
                project(graph.callers_of(callee)),
                project(rev.remove(callee).unwrap_or_default()),
                "targeted scan disagreed with the full map for {callee}"
            );
        }
    }

    /// Removing the last namesake used to leave an empty Vec in `name_index`,
    /// which made the name look known while offering no candidate: the edge
    /// got an empty target labelled `NameOnly` instead of `Unresolved`.
    #[test]
    fn test_removed_namesake_resolves_as_unresolved() {
        let source = CallGraph::new();
        source.insert_node(
            CallGraph::qualified_key("src/lib.rs", "gone"),
            node("gone", "src/lib.rs", None),
        );

        // Round-trip so `file_functions` knows the file, as it does after a
        // real walk; then re-index the file to nothing.
        let graph = CallGraph::new();
        graph.from_json(&source.to_json().unwrap()).unwrap();
        graph.remove_functions_for_file("src/lib.rs").unwrap();
        assert_eq!(graph.node_count(), 0);

        let (target, resolution) = graph.resolve_callee("gone", "src/main.rs", None);
        assert_eq!(target, "gone", "an edge must never carry an empty target");
        assert_eq!(resolution, CallResolution::Unresolved);
    }

    #[test]
    fn test_factory_type_hint_follows_naming_conventions() {
        for (name, expected) in [
            ("SfxOperationUseFactory", Some("SfxOperationUse")),
            ("sfx_operation_repo_factory", Some("SfxOperationRepo")),
            ("NewGoSchedulerApplication", Some("GoSchedulerApplication")),
            ("new_store", Some("Store")),
            ("StoreProvider", Some("Store")),
            ("RequestBuilder", Some("Request")),
        ] {
            assert_eq!(
                CallGraph::factory_type_hint(name).as_deref(),
                expected,
                "hint for {name}"
            );
        }
    }

    #[test]
    fn test_factory_type_hint_declines_names_without_a_convention() {
        for name in [
            // A plain lookup names no type.
            "lookup", "get", "handle",
            // `New` followed by a lowercase letter is one word, not a
            // constructor: `NewsFeed` must not be read as type `SFeed`.
            "NewsFeed", // Nothing left after stripping.
            "Factory", "New",
        ] {
            assert_eq!(
                CallGraph::factory_type_hint(name),
                None,
                "{name} must not produce a type hint"
            );
        }
    }
}
