//! Semantic code search with BM25 ranking and code-aware tokenization
//!
//! Provides intelligent code search that understands programming patterns.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;

use crate::tantivy_index::TantivyIndex;

/// Repository tag for documents indexed outside a repository walk
/// (WASM, transient per-query indexes) and for older persisted documents.
fn unknown_repo() -> Arc<str> {
    Arc::from("")
}

/// Key that identifies a document uniquely across repositories.
///
/// Document ids are repository-relative (`src/lib.rs::main`), so two indexed
/// repositories collide on them — in a map-backed store the second insert wins
/// and the first document is simply lost. Stores that key documents by id use
/// this instead; the document keeps its unprefixed `id` for display and for
/// content lookups.
pub fn scoped_doc_id(repo: &str, id: &str) -> String {
    if repo.is_empty() {
        id.to_string()
    } else {
        format!("{}\u{1f}{}", repo, id)
    }
}

/// Validate regex pattern to prevent ReDoS attacks
fn validate_regex_pattern(pattern: &str) -> Result<regex::Regex, String> {
    // Reject overly long patterns
    if pattern.len() > 1000 {
        return Err("Pattern too long (max 1000 chars)".to_string());
    }

    // Count potentially dangerous constructs
    let nested_quantifiers =
        pattern.matches('+').count() + pattern.matches('*').count() + pattern.matches('?').count();

    if nested_quantifiers > 20 {
        return Err("Pattern has too many quantifiers".to_string());
    }

    regex::Regex::new(pattern).map_err(|e| e.to_string())
}

/// A searchable document (file or symbol)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchDocument {
    pub id: String,
    /// Name of the repository this document belongs to. Interned as `Arc<str>`
    /// so a corpus of millions of documents pays 16 bytes each, not a String
    /// allocation each. Empty when the document was indexed outside a
    /// repository walk (WASM, transient per-query indexes).
    #[serde(default = "unknown_repo")]
    pub repo: Arc<str>,
    pub file_path: String,
    /// Raw text. The PERSISTENT index stores this empty (it duplicated
    /// `file_cache` and cost ~2.4 GB on the 1C corpus) — snippets are
    /// regenerated from disk/`file_cache` at query time. The transient
    /// per-query index built by `hybrid_search` still populates it.
    pub content: String,
    pub doc_type: DocType,
    pub start_line: usize,
    pub end_line: usize,
    /// Token frequencies (unique term -> count). The document length (total
    /// term count) is `term_freq.values().sum()`, so the per-occurrence token
    /// list is not stored — it was the dominant memory hog on large corpora.
    pub term_freq: HashMap<String, usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum DocType {
    File,
    Function,
    Class,
    Struct,
    Method,
    Other,
}

/// A search result with relevance scoring
#[derive(Debug, Clone)]
pub struct SearchResult {
    pub document: SearchDocument,
    pub score: f64,
    pub matched_terms: Vec<String>,
    pub snippet: String,
}

/// BM25 parameters
#[derive(Debug, Clone)]
pub struct BM25Params {
    /// Term frequency saturation (default: 1.2)
    pub k1: f64,
    /// Document length normalization (default: 0.75)
    pub b: f64,
}

impl Default for BM25Params {
    fn default() -> Self {
        Self { k1: 1.2, b: 0.75 }
    }
}

/// Code-aware search index.
///
/// A thin owner of a [`TantivyIndex`]. The BM25 that used to live here kept its
/// whole inverted index in a `HashMap` and was never persisted; see
/// [`crate::tantivy_index`] for what replaced it and why.
pub struct SearchIndex {
    inner: TantivyIndex,
}

impl Default for SearchIndex {
    fn default() -> Self {
        Self::new()
    }
}

impl SearchIndex {
    /// In-RAM index.
    pub fn new() -> Self {
        Self {
            // An in-RAM tantivy index has no filesystem and no existing schema
            // to reconcile, so there is no failure a caller could act on.
            inner: TantivyIndex::in_memory()
                .expect("in-memory tantivy index must be constructible"),
        }
    }

    /// Open (or create) a persisted index rooted at `path`.
    #[cfg(feature = "native")]
    pub fn open_or_create(path: &std::path::Path) -> anyhow::Result<Self> {
        Ok(Self {
            inner: TantivyIndex::open_or_create(path)?,
        })
    }

    /// The underlying index, for operations with no wrapper equivalent:
    /// `commit`, `delete_repo`, `disk_bytes`.
    pub fn tantivy(&self) -> &TantivyIndex {
        &self.inner
    }

    /// Add a document to the index.
    ///
    /// Indexing failures are logged rather than propagated: the callers are
    /// indexing loops over thousands of files, and one unparseable document
    /// must not abort the walk.
    pub fn add_document(&self, doc: SearchDocument) {
        if let Err(e) = self.inner.add_document(doc) {
            tracing::warn!("Failed to index document: {}", e);
        }
    }

    /// Add a document and keep its raw text, so `SearchResult::document.content`
    /// round-trips. Only for callers that read that field back.
    pub fn add_document_retaining_text(&self, doc: SearchDocument) {
        if let Err(e) = self.inner.add_document_retaining_text(doc) {
            tracing::warn!("Failed to index document: {}", e);
        }
    }

    /// Index content from a file.
    pub fn index_file(&self, repo: Arc<str>, file_path: &str, content: &str) {
        self.add_document(SearchDocument {
            id: file_path.to_string(),
            repo,
            file_path: file_path.to_string(),
            // Handed to tantivy for tokenization and dropped; the corpus index
            // indexes this text without storing it.
            content: content.to_string(),
            doc_type: DocType::File,
            start_line: 1,
            end_line: content.lines().count(),
            term_freq: HashMap::new(),
        });
    }

    /// Index a symbol (function, class, etc.)
    #[allow(clippy::too_many_arguments)]
    pub fn index_symbol(
        &self,
        repo: Arc<str>,
        file_path: &str,
        name: &str,
        content: &str,
        doc_type: DocType,
        start_line: usize,
        end_line: usize,
    ) {
        self.add_document(SearchDocument {
            id: format!("{}::{}", file_path, name),
            repo,
            file_path: file_path.to_string(),
            content: content.to_string(),
            doc_type,
            start_line,
            end_line,
            term_freq: HashMap::new(),
        });
    }

    /// Search the index with BM25 ranking across every indexed repository.
    pub fn search(&self, query: &str, max_results: usize) -> Vec<SearchResult> {
        self.search_in(query, max_results, None)
    }

    /// Search with BM25 ranking, optionally restricted to one repository.
    ///
    /// Filtering is a clause of the query rather than a pass over results, so
    /// `max_results` is honoured within the repository instead of being spent
    /// on other repositories.
    pub fn search_in(
        &self,
        query: &str,
        max_results: usize,
        repo: Option<&str>,
    ) -> Vec<SearchResult> {
        // Validate query pattern to prevent ReDoS attacks
        if let Err(e) = validate_regex_pattern(query) {
            eprintln!("Invalid search pattern: {}", e);
            return Vec::new();
        }

        self.inner
            .search_in(query, max_results, repo)
            .into_iter()
            .map(|(document, score, matched_terms)| {
                let snippet =
                    Self::generate_snippet(&document.content, &matched_terms, document.start_line);
                SearchResult {
                    document,
                    score,
                    matched_terms,
                    snippet,
                }
            })
            .collect()
    }

    /// Generate a snippet highlighting matched terms from already-loaded text.
    ///
    /// `content` is the document's line range (caller extracts it from
    /// disk/`file_cache`); `start_line` is its 1-indexed first line, used to
    /// number the snippet lines correctly.
    pub fn generate_snippet(content: &str, matched_terms: &[String], start_line: usize) -> String {
        let lines: Vec<&str> = content.lines().collect();
        let mut best_line_idx = 0;
        let mut best_score = 0;

        // Find the line with the most matches
        for (idx, line) in lines.iter().enumerate() {
            let line_lower = line.to_lowercase();
            let score: usize = matched_terms
                .iter()
                .filter(|term| line_lower.contains(&term.to_lowercase()))
                .count();

            if score > best_score {
                best_score = score;
                best_line_idx = idx;
            }
        }

        // Extract context around the best line
        let start = best_line_idx.saturating_sub(2);
        let end = (best_line_idx + 3).min(lines.len());

        lines[start..end]
            .iter()
            .enumerate()
            .map(|(i, line)| format!("{:4} | {}", start_line + start + i, line))
            .collect::<Vec<_>>()
            .join("\n")
    }
    /// Get statistics about the index
    pub fn stats(&self) -> IndexStats {
        IndexStats {
            total_documents: self.inner.num_docs(),
            total_terms: self.inner.num_terms(),
            avg_doc_length: self.inner.avg_doc_len(),
            doc_types: self.inner.doc_type_counts(),
        }
    }

    /// Clear the index
    pub fn clear(&self) {
        if let Err(e) = self.inner.clear() {
            tracing::warn!("Failed to clear search index: {}", e);
        }
    }
}

/// Code-specific synonym groups, applied to query terms at search time.
///
/// Lifted out of the old `SearchIndex` unchanged: the table is the accumulated
/// domain knowledge here, and it outlived the scorer it was written for.
pub fn build_code_synonyms() -> HashMap<String, Vec<String>> {
    let mut synonyms = HashMap::new();

    // Common programming synonyms
    let synonym_groups = vec![
        vec![
            "func",
            "function",
            "fn",
            "def",
            "method",
            "proc",
            "procedure",
        ],
        vec!["struct", "structure", "class", "type", "record"],
        vec!["err", "error", "exception", "failure", "fault"],
        vec!["ok", "success", "result"],
        vec!["str", "string", "text"],
        vec!["int", "integer", "number", "num"],
        vec!["bool", "boolean", "flag"],
        vec!["arr", "array", "list", "vec", "vector", "slice"],
        vec!["dict", "dictionary", "map", "hashmap", "hash"],
        vec!["cfg", "config", "configuration", "settings", "options"],
        vec!["msg", "message", "payload"],
        vec!["req", "request"],
        vec!["res", "resp", "response"],
        vec!["db", "database", "store", "storage"],
        vec!["auth", "authentication", "authorize"],
        vec!["impl", "implement", "implementation"],
        vec!["init", "initialize", "setup", "create", "new"],
        vec!["del", "delete", "remove", "drop"],
        vec!["get", "fetch", "retrieve", "read", "load"],
        vec!["set", "update", "write", "save", "store"],
        vec!["async", "asynchronous", "concurrent"],
        vec!["sync", "synchronous", "blocking"],
    ];

    for group in synonym_groups {
        for term in &group {
            let others: Vec<String> = group
                .iter()
                .filter(|&t| t != term)
                .map(|s| s.to_string())
                .collect();
            synonyms.insert(term.to_string(), others);
        }
    }

    synonyms
}
/// Index statistics
#[derive(Debug, Clone)]
pub struct IndexStats {
    pub total_documents: usize,
    pub total_terms: usize,
    pub avg_doc_length: f64,
    pub doc_types: HashMap<DocType, usize>,
}

/// Code-aware tokenization
pub fn tokenize_code(text: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();

    for ch in text.chars() {
        if ch.is_alphanumeric() || ch == '_' {
            current.push(ch);
        } else if !current.is_empty() {
            // Split camelCase and snake_case
            tokens.extend(split_identifier(&current));
            current.clear();
        }
    }

    if !current.is_empty() {
        tokens.extend(split_identifier(&current));
    }

    // Lowercase and filter short tokens. Most identifiers in an indexed
    // corpus are ASCII (keywords, common names); `to_ascii_lowercase` skips
    // the Unicode case-fold tables `to_lowercase` must consult for the
    // general case, and is measurably cheaper at the scale of "every token
    // in every file" — profiling a 49k-file/237k-symbol index showed
    // `to_lowercase`'s Unicode path as one of the largest single self-time
    // contributors. Falls back to full `to_lowercase` for non-ASCII tokens
    // (e.g. Cyrillic identifiers in BSL/1C source) so case-folding stays
    // correct for them.
    tokens
        .into_iter()
        .map(|t| {
            if t.is_ascii() {
                t.to_ascii_lowercase()
            } else {
                t.to_lowercase()
            }
        })
        .filter(|t| t.len() >= 2)
        .filter(|t| !is_stop_word(t))
        .collect()
}

/// Split identifiers by camelCase, PascalCase, and snake_case
fn split_identifier(ident: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();

    let chars: Vec<char> = ident.chars().collect();

    for i in 0..chars.len() {
        let ch = chars[i];
        let prev = i.checked_sub(1).map(|idx| chars[idx]);
        let prev_lower = prev.is_some_and(|c| c.is_lowercase());
        let next_lower = i + 1 < chars.len() && chars[i + 1].is_lowercase();
        let split_before = prev.is_some_and(|prev_ch| {
            (prev_ch.is_ascii_digit() && !ch.is_ascii_digit())
                || (!prev_ch.is_ascii_digit() && ch.is_ascii_digit())
                || is_script_boundary(prev_ch, ch)
                || (ch.is_uppercase()
                    && (prev_lower || (i + 1 < chars.len() && next_lower))
                    && !current.is_empty())
        });

        // Split on underscore
        if ch == '_' {
            if !current.is_empty() {
                parts.push(current.clone());
                current.clear();
            }
            continue;
        }

        // Split on digit/letter boundaries, Latin/Cyrillic transitions, and camelCase edges.
        if split_before {
            parts.push(current.clone());
            current.clear();
        }

        current.push(ch);
    }

    if !current.is_empty() {
        parts.push(current);
    }

    // Also include the full identifier
    parts.push(ident.to_string());

    parts
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ScriptGroup {
    Latin,
    Cyrillic,
}

fn script_group(ch: char) -> Option<ScriptGroup> {
    if ch.is_ascii_alphabetic() {
        Some(ScriptGroup::Latin)
    } else if ('\u{0400}'..='\u{04FF}').contains(&ch)
        || ('\u{0500}'..='\u{052F}').contains(&ch)
        || ('\u{2DE0}'..='\u{2DFF}').contains(&ch)
        || ('\u{A640}'..='\u{A69F}').contains(&ch)
        || ch == 'ё'
        || ch == 'Ё'
    {
        Some(ScriptGroup::Cyrillic)
    } else {
        None
    }
}

fn is_script_boundary(prev: char, current: char) -> bool {
    matches!(
        (script_group(prev), script_group(current)),
        (Some(left), Some(right)) if left != right
    )
}

/// Count term frequencies
pub fn count_terms(tokens: &[String]) -> HashMap<String, usize> {
    let mut counts = HashMap::new();
    for token in tokens {
        *counts.entry(token.clone()).or_default() += 1;
    }
    counts
}

/// Check if a token is a common stop word in code
fn is_stop_word(token: &str) -> bool {
    const STOP_WORDS: &[&str] = &[
        "the", "a", "an", "is", "are", "was", "were", "be", "been", "to", "of", "in", "for", "on",
        "with", "at", "by", "from", "if", "then", "else", "do", "while", "this", "that", "it",
        "let", "var", "const", "mut", "pub", "fn", "def", "class", "return", "true", "false",
        "null", "none", "nil", "self",
    ];
    STOP_WORDS.contains(&token)
}

/// Thread-safe search index wrapper.
///
/// Tantivy serialises its own writes and serves concurrent readers from an
/// immutable snapshot, so this no longer needs a lock of its own — the
/// `RwLock` it used to hold made every indexing thread queue behind one
/// writer.
pub struct ConcurrentSearchIndex {
    inner: SearchIndex,
}

impl Default for ConcurrentSearchIndex {
    fn default() -> Self {
        Self::new()
    }
}

impl ConcurrentSearchIndex {
    pub fn new() -> Self {
        Self {
            inner: SearchIndex::new(),
        }
    }

    /// Persisted index rooted at `path`.
    #[cfg(feature = "native")]
    pub fn open_or_create(path: &std::path::Path) -> anyhow::Result<Self> {
        Ok(Self {
            inner: SearchIndex::open_or_create(path)?,
        })
    }

    /// The underlying index, for `commit`, `delete_repo` and `disk_bytes`.
    pub fn tantivy(&self) -> &TantivyIndex {
        self.inner.tantivy()
    }

    /// Add a single document to the search index.
    /// Used by WASM interface for incremental indexing.
    pub fn add_document(&self, doc: SearchDocument) {
        self.inner.add_document(doc);
    }

    /// Add a document whose raw text must round-trip; see
    /// [`SearchIndex::add_document_retaining_text`].
    pub fn add_document_retaining_text(&self, doc: SearchDocument) {
        self.inner.add_document_retaining_text(doc);
    }

    pub fn index_file(&self, repo: Arc<str>, file_path: &str, content: &str) {
        self.inner.index_file(repo, file_path, content);
    }

    #[allow(clippy::too_many_arguments)]
    pub fn index_symbol(
        &self,
        repo: Arc<str>,
        file_path: &str,
        name: &str,
        content: &str,
        doc_type: DocType,
        start_line: usize,
        end_line: usize,
    ) {
        self.inner.index_symbol(
            repo, file_path, name, content, doc_type, start_line, end_line,
        );
    }

    pub fn search(&self, query: &str, max_results: usize) -> Vec<SearchResult> {
        self.inner.search(query, max_results)
    }

    /// BM25 search restricted to a single repository when `repo` is set.
    pub fn search_in(
        &self,
        query: &str,
        max_results: usize,
        repo: Option<&str>,
    ) -> Vec<SearchResult> {
        self.inner.search_in(query, max_results, repo)
    }

    pub fn stats(&self) -> IndexStats {
        self.inner.stats()
    }

    pub fn clear(&self) {
        self.inner.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tokenize_camel_case() {
        let tokens = tokenize_code("getUserById");
        assert!(tokens.contains(&"get".to_string()));
        assert!(tokens.contains(&"user".to_string()));
        // "by" is filtered out as a stop word
        assert!(tokens.contains(&"id".to_string()));
        assert!(tokens.contains(&"getuserbyid".to_string())); // Full identifier is included
    }

    #[test]
    fn test_tokenize_snake_case() {
        let tokens = tokenize_code("get_user_by_id");
        assert!(tokens.contains(&"get".to_string()));
        assert!(tokens.contains(&"user".to_string()));
    }

    #[test]
    fn test_search_index() {
        let index = SearchIndex::new();

        index.index_file(
            Arc::from("demo"),
            "user.rs",
            r#"
            pub fn get_user_by_id(id: u32) -> User {
                // Fetch user from database
            }
        "#,
        );

        index.index_file(
            Arc::from("demo"),
            "order.rs",
            r#"
            pub fn create_order(user: &User) -> Order {
                // Create new order
            }
        "#,
        );

        let results = index.search("user", 10);
        assert!(!results.is_empty());
        assert!(results[0].score > 0.0);
    }

    #[test]
    fn test_scoped_doc_id_separates_repos() {
        assert_ne!(
            scoped_doc_id("alpha", "src/lib.rs::main"),
            scoped_doc_id("beta", "src/lib.rs::main")
        );
        // Untagged documents keep their bare id.
        assert_eq!(scoped_doc_id("", "src/lib.rs::main"), "src/lib.rs::main");
    }

    #[test]
    fn test_search_in_restricts_to_repo() {
        let index = SearchIndex::new();

        index.index_file(Arc::from("alpha"), "user.rs", "pub fn get_user() {}");
        index.index_file(Arc::from("beta"), "user.rs", "pub fn get_user() {}");

        let all = index.search("user", 10);
        assert_eq!(all.len(), 2);

        let alpha = index.search_in("user", 10, Some("alpha"));
        assert_eq!(alpha.len(), 1);
        assert_eq!(&*alpha[0].document.repo, "alpha");

        let beta = index.search_in("user", 10, Some("beta"));
        assert_eq!(beta.len(), 1);
        assert_eq!(&*beta[0].document.repo, "beta");

        assert!(index.search_in("user", 10, Some("gamma")).is_empty());
    }

    /// `max_results` must be spent inside the requested repository, not on
    /// hits from other repositories that are then filtered away.
    #[test]
    fn test_search_in_budget_is_not_eaten_by_other_repos() {
        let index = SearchIndex::new();

        for i in 0..20 {
            index.index_file(
                Arc::from("noise"),
                &format!("noise_{i}.rs"),
                "pub fn handle_user_login() {}",
            );
        }
        index.index_file(Arc::from("target"), "auth.rs", "pub fn user_login() {}");

        let results = index.search_in("user login", 3, Some("target"));
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].document.file_path, "auth.rs");
    }

    #[test]
    fn test_persistent_index_stores_empty_content() {
        let index = SearchIndex::new();
        index.index_symbol(
            Arc::from("demo"),
            "f.rs",
            "foo",
            "fn foo() { bar(); }",
            DocType::Function,
            1,
            1,
        );
        index.index_file(Arc::from("demo"), "g.rs", "fn bar() {}");

        // The corpus index must NOT retain raw text (cost ~2.4 GB on the 1C
        // corpus) — it is regenerated from disk/file_cache at query time.
        let hits = index.search("foo", 10);
        assert!(!hits.is_empty(), "indexed symbol must be findable");
        for hit in &hits {
            assert!(
                hit.document.content.is_empty(),
                "corpus index must not carry raw text back, got {:?}",
                hit.document.content
            );
        }
    }

    #[test]
    fn test_generate_snippet_line_numbering() {
        let content = "line1\nline2\nline3\nline4\nline5";
        let terms = vec!["line3".to_string()];
        let snippet = SearchIndex::generate_snippet(content, &terms, 10);
        // Best line is "line3" (offset 2); context ±2 → lines 10..15 numbered
        // from start_line=10, not from 1.
        assert!(snippet.contains("12 | line3"), "got: {}", snippet);
        assert!(snippet.contains("10 | line1"), "got: {}", snippet);
    }

    #[test]
    fn test_split_identifier() {
        let parts = split_identifier("getUserByID");
        assert!(parts.contains(&"get".to_string()));
        assert!(parts.contains(&"User".to_string()));
        assert!(parts.contains(&"By".to_string()));
        assert!(parts.contains(&"ID".to_string()));
    }

    #[test]
    fn test_tokenize_cyrillic_camel_case() {
        let tokens = tokenize_code("ОбработатьXMLДанные");
        assert!(tokens.contains(&"обработать".to_string()));
        assert!(tokens.contains(&"xml".to_string()));
        assert!(tokens.contains(&"данные".to_string()));
        assert!(tokens.contains(&"обработатьxmlданные".to_string()));
    }

    #[test]
    fn test_tokenize_mixed_script_identifier() {
        let tokens = tokenize_code("xmlклиент");
        assert!(tokens.contains(&"xml".to_string()));
        assert!(tokens.contains(&"клиент".to_string()));
        assert!(tokens.contains(&"xmlклиент".to_string()));
    }

    #[test]
    fn test_tokenize_mixed_digits_and_cyrillic() {
        let tokens = tokenize_code("УТ11ОбменXML");
        assert!(tokens.contains(&"ут".to_string()));
        assert!(tokens.contains(&"11".to_string()));
        assert!(tokens.contains(&"обмен".to_string()));
        assert!(tokens.contains(&"xml".to_string()));
        assert!(tokens.contains(&"ут11обменxml".to_string()));
    }

    #[test]
    fn test_synonym_expansion() {
        // Synonyms are applied to query terms: searching `func` must reach a
        // document that only ever says `function`.
        let index = SearchIndex::new();
        index.index_file(
            Arc::from("demo"),
            "a.rs",
            "the function keyword introduces a definition",
        );

        assert!(
            !index.search("func", 10).is_empty(),
            "a query term's synonyms must reach documents using the other spelling"
        );
    }

    // Security tests for regex DoS prevention
    #[test]
    fn test_validate_regex_pattern_valid() {
        let result = validate_regex_pattern("simple_pattern");
        assert!(result.is_ok());

        let result = validate_regex_pattern(r"fn\s+\w+");
        assert!(result.is_ok());
    }

    #[test]
    fn test_regex_dos_prevention_pattern_too_long() {
        // Prevent ReDoS by rejecting patterns >1000 chars
        let long_pattern = "a".repeat(1001);
        let result = validate_regex_pattern(&long_pattern);
        assert!(result.is_err());
        assert_eq!(result.unwrap_err(), "Pattern too long (max 1000 chars)");
    }

    #[test]
    fn test_regex_dos_prevention_too_many_quantifiers() {
        // Prevent ReDoS by rejecting patterns with >20 quantifiers
        let dangerous_pattern = "a+b*c+d*e+f*g+h*i+j*k+l*m+n*o+p*q+r*s+t*u+v*w+x*y+z*";
        let result = validate_regex_pattern(dangerous_pattern);
        assert!(result.is_err());
        assert_eq!(result.unwrap_err(), "Pattern has too many quantifiers");
    }

    #[test]
    fn test_validate_regex_pattern_invalid_syntax() {
        let invalid_pattern = "[invalid(";
        let result = validate_regex_pattern(invalid_pattern);
        assert!(result.is_err());
    }

    #[test]
    fn test_sort_handles_nan_scores() {
        let index = SearchIndex::new();

        index.index_file(Arc::from("demo"), "a.rs", "fn hello() { }");
        index.index_file(Arc::from("demo"), "b.rs", "fn world() { }");

        // Search should not panic even if scores could theoretically be NaN
        let results = index.search("hello", 10);
        assert!(!results.is_empty());

        // Directly test the sort with NaN values
        let mut scores: Vec<(usize, (f64, Vec<String>))> = vec![
            (0, (1.0, vec!["a".to_string()])),
            (1, (f64::NAN, vec!["b".to_string()])),
            (2, (0.5, vec!["c".to_string()])),
        ];
        // This should not panic with our fix
        scores.sort_by(|a, b| {
            b.1 .0
                .partial_cmp(&a.1 .0)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
    }
}
