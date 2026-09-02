//! Type declarations of the indexed sources, as far as the call graph needs
//! them: which fields a type has, what they are typed as, which types it
//! embeds, and which methods an interface requires.
//!
//! The call graph itself stores functions, so a qualifier that names anything
//! else — a field, a variable, a value returned by a call — used to resolve to
//! whichever namesake sorted first. The type is written in the source; this is
//! where that writing is kept so [`CallGraph::resolve_scoped_callee`] can be
//! offered a real type instead of a guess.
//!
//! [`CallGraph::resolve_scoped_callee`]: super::CallGraph::resolve_scoped_callee
//!
//! **Not persisted.** A `CallGraph` is saved through `to_json`/`from_json` with
//! its edges already resolved, so this index is only needed while indexing —
//! there is nothing to restore and no serde compatibility to keep.
//!
//! Go-shaped declarations only for now. The shape of the index is
//! language-neutral, but the extraction is not: a Python class keeps its fields
//! in `__init__`, a TypeScript one in the class body and in interfaces, and
//! each needs its own reading.

use crate::FastMap;
use dashmap::DashMap;
use std::sync::Arc;
use tree_sitter::Node;

/// What a declaration declares. `Alias` is kept because `type Uuid = string`
/// makes two names for one type, and a method may sit on either.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TypeKind {
    Struct,
    Interface,
    Alias,
}

/// One field of a struct. `embedded` marks Go's anonymous field — the type is
/// written without a name, and its methods are promoted onto the outer type.
#[derive(Debug, Clone)]
pub struct Field {
    pub name: String,
    pub type_name: String,
    pub embedded: bool,
}

/// A declared type, stripped to what resolution can act on.
#[derive(Debug, Clone)]
pub struct TypeDecl {
    pub name: String,
    pub file_path: String,
    pub kind: TypeKind,
    pub fields: Vec<Field>,
    /// Method names an interface requires; empty for a struct or alias.
    pub methods: Vec<String>,
    /// The aliased type, for `TypeKind::Alias`.
    pub aliased: Option<String>,
}

impl TypeDecl {
    /// The type of a named field, if the type has one.
    fn field_type(&self, field: &str) -> Option<&str> {
        self.fields
            .iter()
            .find(|candidate| candidate.name == field)
            .map(|found| found.type_name.as_str())
    }
}

/// Declared types by name, plus the per-file listing needed to drop a file's
/// declarations when it is re-indexed.
#[derive(Default)]
pub struct TypeIndex {
    by_name: FastMap<String, Vec<Arc<TypeDecl>>>,
    by_file: FastMap<String, Vec<String>>,
}

impl TypeIndex {
    pub fn new() -> Self {
        Self {
            by_name: DashMap::default(),
            by_file: DashMap::default(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.by_name.is_empty()
    }

    /// Number of distinct type names held, for memory reporting.
    pub fn len(&self) -> usize {
        self.by_name.len()
    }

    /// Read every type declaration of one parsed file into the index.
    pub fn ingest_file(&self, path: &str, root: Node, source: &[u8]) {
        for decl in collect_go_types(path, root, source) {
            self.insert(decl);
        }
    }

    fn insert(&self, decl: TypeDecl) {
        let name = decl.name.clone();
        self.by_file
            .entry(decl.file_path.clone())
            .or_default()
            .push(name.clone());
        let decl = Arc::new(decl);
        let mut entry = self.by_name.entry(name).or_default();
        // Re-indexing the same file must not stack duplicate declarations: a
        // name listed twice makes `unique_decl` bail out and every lookup
        // through it degrade to nothing.
        if let Some(existing) = entry
            .iter_mut()
            .find(|held| held.file_path == decl.file_path)
        {
            *existing = decl;
        } else {
            entry.push(decl);
        }
    }

    /// Drop the declarations a file contributed, before it is re-indexed.
    pub fn remove_file(&self, path: &str) {
        if let Some((_, names)) = self.by_file.remove(path) {
            for name in names {
                let emptied = match self.by_name.get_mut(&name) {
                    Some(mut entry) => {
                        entry.retain(|decl| decl.file_path != path);
                        entry.is_empty()
                    }
                    None => false,
                };
                if emptied {
                    self.by_name.remove_if(&name, |_, held| held.is_empty());
                }
            }
        }
    }

    /// The single declaration of `name`, or `None` when the name is unknown or
    /// declared more than once. Two types of one name in a corpus (a `Config`
    /// per module, say) cannot be told apart here, and picking one would put a
    /// wrong type into resolution — where the whole point is to replace a
    /// guess with something read.
    fn unique_decl(&self, name: &str) -> Option<Arc<TypeDecl>> {
        let entry = self.by_name.get(name)?;
        match entry.len() {
            1 => Some(entry[0].clone()),
            _ => None,
        }
    }

    /// The type of `type_name`'s field `field`, following aliases and embedded
    /// types (Go promotes an embedded type's fields as well as its methods).
    pub fn field_type(&self, type_name: &str, field: &str) -> Option<String> {
        self.walk_from(type_name, 4, &mut |decl| {
            decl.field_type(field).map(|found| found.to_string())
        })
    }

    /// Names of the types `type_name` embeds, outermost first, following
    /// embedded types transitively. Empty when the type is unknown.
    pub fn embedded_chain(&self, type_name: &str) -> Vec<String> {
        let mut found = Vec::new();
        let mut queue = vec![(type_name.to_string(), 0usize)];
        let mut seen = vec![type_name.to_string()];
        while let Some((current, depth)) = queue.pop() {
            if depth >= 3 {
                continue;
            }
            let Some(decl) = self.unique_decl(&current) else {
                continue;
            };
            for field in decl.fields.iter().filter(|field| field.embedded) {
                if seen.contains(&field.type_name) {
                    continue;
                }
                seen.push(field.type_name.clone());
                found.push(field.type_name.clone());
                queue.push((field.type_name.clone(), depth + 1));
            }
        }
        found
    }

    /// Is `type_name` an interface?
    pub fn is_interface(&self, type_name: &str) -> bool {
        self.unique_decl(type_name)
            .map(|decl| decl.kind == TypeKind::Interface)
            .unwrap_or(false)
    }

    /// The methods an interface requires, for matching an implementing type.
    pub fn interface_methods(&self, type_name: &str) -> Vec<String> {
        self.unique_decl(type_name)
            .map(|decl| decl.methods.clone())
            .unwrap_or_default()
    }

    /// Every struct whose declaration is known, as `(name, method names it
    /// would need)` — used by interface matching, which has to ask "which type
    /// carries all of these methods".
    pub fn struct_names(&self) -> Vec<String> {
        self.by_name
            .iter()
            .filter(|entry| entry.value().len() == 1 && entry.value()[0].kind == TypeKind::Struct)
            .map(|entry| entry.key().clone())
            .collect()
    }

    /// Apply `probe` to `type_name`'s declaration, then to the declarations it
    /// aliases or embeds, until one answers. Bounded by `depth` so a cyclic
    /// declaration cannot spin.
    fn walk_from<T>(
        &self,
        type_name: &str,
        depth: usize,
        probe: &mut dyn FnMut(&TypeDecl) -> Option<T>,
    ) -> Option<T> {
        if depth == 0 {
            return None;
        }
        let decl = self.unique_decl(type_name)?;
        if let Some(found) = probe(&decl) {
            return Some(found);
        }
        if let Some(aliased) = decl.aliased.as_deref() {
            if let Some(found) = self.walk_from(aliased, depth - 1, probe) {
                return Some(found);
            }
        }
        for embedded in decl.fields.iter().filter(|field| field.embedded) {
            if let Some(found) = self.walk_from(&embedded.type_name, depth - 1, probe) {
                return Some(found);
            }
        }
        None
    }
}

/// Read the `type ...` declarations of one Go file.
///
/// Only the file's own top level is walked: a type declared inside a function
/// body is local to it and cannot be the type of a field or a package-level
/// value, so descending into bodies would cost a full tree walk for names
/// resolution can never use.
fn collect_go_types(path: &str, root: Node, source: &[u8]) -> Vec<TypeDecl> {
    let mut declarations = Vec::new();
    let mut cursor = root.walk();
    for child in root.children(&mut cursor) {
        if child.kind() != "type_declaration" {
            continue;
        }
        let mut spec_cursor = child.walk();
        for spec in child.children(&mut spec_cursor) {
            match spec.kind() {
                "type_spec" => {
                    if let Some(decl) = type_spec_decl(path, spec, source) {
                        declarations.push(decl);
                    }
                }
                "type_alias" => {
                    let Some(name) = field_text(spec, "name", source) else {
                        continue;
                    };
                    declarations.push(TypeDecl {
                        name,
                        file_path: path.to_string(),
                        kind: TypeKind::Alias,
                        fields: Vec::new(),
                        methods: Vec::new(),
                        aliased: spec
                            .child_by_field_name("type")
                            .and_then(|node| type_name_of(node, source)),
                    });
                }
                _ => {}
            }
        }
    }
    declarations
}

fn type_spec_decl(path: &str, spec: Node, source: &[u8]) -> Option<TypeDecl> {
    let name = field_text(spec, "name", source)?;
    let body = spec.child_by_field_name("type")?;
    let (kind, fields, methods, aliased) = match body.kind() {
        "struct_type" => (
            TypeKind::Struct,
            struct_fields(body, source),
            Vec::new(),
            None,
        ),
        "interface_type" => (
            TypeKind::Interface,
            Vec::new(),
            interface_methods(body, source),
            None,
        ),
        // `type Uuid string` and `type Handler func(Ctx) error`: a distinct
        // type whose methods may be looked up, defined in terms of another.
        _ => (
            TypeKind::Alias,
            Vec::new(),
            Vec::new(),
            type_name_of(body, source),
        ),
    };
    Some(TypeDecl {
        name,
        file_path: path.to_string(),
        kind,
        fields,
        methods,
        aliased,
    })
}

fn struct_fields(struct_type: Node, source: &[u8]) -> Vec<Field> {
    let mut fields = Vec::new();
    let mut finder = struct_type.walk();
    let list = struct_type.child_by_field_name("body").or_else(|| {
        struct_type
            .children(&mut finder)
            .find(|child| child.kind() == "field_declaration_list")
    });
    let Some(list) = list else {
        return fields;
    };
    let mut cursor = list.walk();
    for declaration in list.children(&mut cursor) {
        if declaration.kind() != "field_declaration" {
            continue;
        }
        let Some(type_name) = declaration
            .child_by_field_name("type")
            .and_then(|node| type_name_of(node, source))
        else {
            continue;
        };
        let names = field_names(declaration, source);
        if names.is_empty() {
            // Anonymous field: Go's embedding. The field's name *is* the
            // type's name, and its methods are promoted onto this type.
            fields.push(Field {
                name: type_name.clone(),
                type_name,
                embedded: true,
            });
            continue;
        }
        for name in names {
            fields.push(Field {
                name,
                type_name: type_name.clone(),
                embedded: false,
            });
        }
    }
    fields
}

fn field_names(declaration: Node, source: &[u8]) -> Vec<String> {
    let mut names = Vec::new();
    let mut cursor = declaration.walk();
    for child in declaration.children(&mut cursor) {
        if child.kind() == "field_identifier" {
            if let Ok(text) = child.utf8_text(source) {
                names.push(text.to_string());
            }
        }
    }
    names
}

fn interface_methods(interface_type: Node, source: &[u8]) -> Vec<String> {
    let mut methods = Vec::new();
    let mut cursor = interface_type.walk();
    for child in interface_type.children(&mut cursor) {
        // `method_elem` in current tree-sitter-go, `method_spec` in older
        // grammars; an embedded interface arrives as a bare type instead.
        if matches!(child.kind(), "method_elem" | "method_spec") {
            if let Some(name) = field_text(child, "name", source) {
                methods.push(name);
            }
        }
    }
    methods
}

/// The name of a type as written, reduced to the identifier resolution
/// compares against a receiver: the package qualifier is dropped
/// (`account_use.InterestUse` -> `InterestUse`) and a pointer is followed.
///
/// Slices, maps, channels and functions deliberately yield `None`: a call
/// cannot be made on the container itself, so answering with the element type
/// would hand resolution a type the qualifier never had.
pub fn type_name_of(node: Node, source: &[u8]) -> Option<String> {
    match node.kind() {
        "type_identifier" | "identifier" => {
            let text = node.utf8_text(source).ok()?;
            (!is_primitive(text)).then(|| text.to_string())
        }
        "qualified_type" => node
            .child_by_field_name("name")
            .and_then(|name| type_name_of(name, source)),
        "pointer_type" | "parenthesized_type" => {
            let inner = node.named_child(0)?;
            type_name_of(inner, source)
        }
        _ => None,
    }
}

/// Builtin types no lookup can ever hit, and which would otherwise fill the
/// index with entries for `string` and `error`.
fn is_primitive(name: &str) -> bool {
    matches!(
        name,
        "string"
            | "bool"
            | "error"
            | "byte"
            | "rune"
            | "any"
            | "int"
            | "int8"
            | "int16"
            | "int32"
            | "int64"
            | "uint"
            | "uint8"
            | "uint16"
            | "uint32"
            | "uint64"
            | "uintptr"
            | "float32"
            | "float64"
            | "complex64"
            | "complex128"
    )
}

fn field_text(node: Node, field: &str, source: &[u8]) -> Option<String> {
    node.child_by_field_name(field)
        .and_then(|child| child.utf8_text(source).ok())
        .map(|text| text.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::LanguageParser;
    use std::path::Path;

    const SOURCE: &str = r#"
package app

type Ctx interface {
	Logger() *Logger
	Db() *gorm.DB
}

type BaseCronScheduler struct{}

type AccountInterest struct {
	goscheduler.BaseCronScheduler
	Depend AccountInterestDepend
	name   string
	rows   []*PayoutBatchLine
}

type AccountInterestDepend struct {
	InterestUseFactory account_use.InterestUseFactory
}

type Uuid = string
"#;

    fn index() -> TypeIndex {
        let parser = LanguageParser::new().unwrap();
        let tree = parser
            .parse_to_tree(Path::new("internal/app/interest.go"), SOURCE)
            .unwrap();
        let index = TypeIndex::new();
        index.ingest_file(
            "internal/app/interest.go",
            tree.root_node(),
            SOURCE.as_bytes(),
        );
        index
    }

    #[test]
    fn test_struct_fields_drop_the_package_qualifier() {
        let index = index();
        assert_eq!(
            index.field_type("AccountInterest", "Depend").as_deref(),
            Some("AccountInterestDepend")
        );
        assert_eq!(
            index
                .field_type("AccountInterestDepend", "InterestUseFactory")
                .as_deref(),
            Some("InterestUseFactory"),
            "the receiver of a method is never written with its package"
        );
    }

    #[test]
    fn test_primitive_and_container_fields_are_not_indexed() {
        let index = index();
        assert_eq!(
            index.field_type("AccountInterest", "name"),
            None,
            "no method is ever looked up on `string`"
        );
        assert_eq!(
            index.field_type("AccountInterest", "rows"),
            None,
            "a call cannot be made on the slice itself"
        );
    }

    #[test]
    fn test_embedded_field_is_marked_and_chained() {
        let index = index();
        assert_eq!(
            index.embedded_chain("AccountInterest"),
            vec!["BaseCronScheduler".to_string()],
            "the anonymous field's type is what gets promoted"
        );
        assert!(index.embedded_chain("BaseCronScheduler").is_empty());
    }

    #[test]
    fn test_interface_is_recognised_with_its_methods() {
        let index = index();
        assert!(index.is_interface("Ctx"));
        assert!(!index.is_interface("AccountInterest"));
        assert_eq!(
            index.interface_methods("Ctx"),
            vec!["Logger".to_string(), "Db".to_string()]
        );
    }

    #[test]
    fn test_reindexing_a_file_replaces_rather_than_stacks() {
        let index = index();
        let before = index.len();
        let parser = LanguageParser::new().unwrap();
        let tree = parser
            .parse_to_tree(Path::new("internal/app/interest.go"), SOURCE)
            .unwrap();
        index.ingest_file(
            "internal/app/interest.go",
            tree.root_node(),
            SOURCE.as_bytes(),
        );
        assert_eq!(index.len(), before);
        assert_eq!(
            index.field_type("AccountInterest", "Depend").as_deref(),
            Some("AccountInterestDepend"),
            "a duplicate declaration makes every lookup bail out"
        );

        index.remove_file("internal/app/interest.go");
        assert!(index.is_empty(), "removal must leave nothing behind");
    }

    #[test]
    fn test_struct_names_lists_only_unambiguous_structs() {
        let index = index();
        let mut names = index.struct_names();
        names.sort();
        assert_eq!(
            names,
            vec![
                "AccountInterest".to_string(),
                "AccountInterestDepend".to_string(),
                "BaseCronScheduler".to_string(),
            ]
        );
    }
}
