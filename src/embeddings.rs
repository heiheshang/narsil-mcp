//! Semantic embeddings for code similarity search using TF-IDF
//!
//! Provides a simple and fast embedding system for finding similar code without
//! heavy ML dependencies. Uses TF-IDF vectors with cosine similarity.
//!
//! This is a Phase 3 feature - semantic embeddings for "find similar code" queries.

use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;

use crate::search::tokenize_code;

/// A trait for generating embeddings from code
pub trait EmbeddingProvider: Send + Sync {
    /// Generate an embedding vector from text
    fn embed(&self, text: &str) -> Vec<f32>;

    /// Get the dimensionality of embeddings
    fn dimension(&self) -> usize;
}

/// TF-IDF based embedding provider
///
/// A term's position in the vector is a hash of the term itself, so the
/// coordinate system is fixed for the life of the corpus. It used to be the
/// term's rank by document frequency, recomputed on every single insert: the
/// first document and the hundred-thousandth were written in different bases,
/// and `sparse_cosine` — which multiplies them position by position — compared
/// unrelated dimensions. Two documents with *identical* text, inserted 500
/// documents apart, scored 1.00 and below-the-noise-floor respectively.
///
/// The cost of a fixed basis is hash collisions: two terms sharing a slot have
/// their weights summed. That is the standard hashing-trick trade, it is the
/// same for every document, and it degrades a score rather than scrambling an
/// ordering. It also removes a per-insert sort of the whole vocabulary.
pub struct TfIdfEmbedding {
    /// Global term frequencies across all documents
    document_freq: HashMap<String, usize>,
    /// Total number of documents
    total_docs: usize,
    /// Vector dimensionality, and the modulus of the term-slot hash
    max_vocab_size: usize,
}

impl TfIdfEmbedding {
    /// Create a new TF-IDF embedding provider
    pub fn new(max_vocab_size: usize) -> Self {
        Self {
            document_freq: HashMap::new(),
            total_docs: 0,
            max_vocab_size,
        }
    }

    /// Add a document to update the IDF statistics
    pub fn add_document(&mut self, text: &str) {
        let tokens = tokenize_code(text);
        let unique_tokens: std::collections::HashSet<_> = tokens.into_iter().collect();

        for token in unique_tokens {
            *self.document_freq.entry(token).or_insert(0) += 1;
        }

        self.total_docs += 1;
        self.prune_document_freq();
    }

    /// The vector position a term occupies.
    ///
    /// FNV-1a rather than `DefaultHasher`: this has to give the same answer in
    /// every process and every build, because documents embedded now are
    /// compared against documents embedded later. `DefaultHasher`'s output is
    /// explicitly not guaranteed stable across releases.
    fn slot(&self, term: &str) -> usize {
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        for byte in term.as_bytes() {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x100_0000_01b3);
        }
        (hash % self.max_vocab_size as u64) as usize
    }

    /// Bound `document_freq` by dropping the rarest terms.
    ///
    /// IDF is the only thing read out of this map and a pruned term simply
    /// scores zero, so the long tail costs memory and buys nothing. 2x headroom
    /// over the target vocabulary keeps near-miss terms' counts alive. Unlike
    /// the vocabulary rebuild this replaces, it runs only when the map is
    /// actually over the limit, not on every insert.
    fn prune_document_freq(&mut self) {
        let limit = self.max_vocab_size.saturating_mul(2);
        if limit == 0 || self.document_freq.len() <= limit {
            return;
        }

        let mut ranked: Vec<(&str, usize)> = self
            .document_freq
            .iter()
            .map(|(term, &df)| (term.as_str(), df))
            .collect();
        ranked.sort_unstable_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));

        let keep: std::collections::HashSet<String> = ranked
            .iter()
            .take(limit)
            .map(|(term, _)| (*term).to_string())
            .collect();
        self.document_freq.retain(|term, _| keep.contains(term));
    }

    /// Compute IDF for a term (with smoothing to avoid zero values)
    fn idf(&self, term: &str) -> f32 {
        if self.total_docs == 0 {
            return 0.0;
        }

        let df = self.document_freq.get(term).copied().unwrap_or(0) as f32;
        if df == 0.0 {
            return 0.0;
        }

        // Use smoothed IDF: log((N + 1) / (df + 1)) + 1
        // This prevents IDF from being 0 when df == N
        ((self.total_docs as f32 + 1.0) / (df + 1.0)).ln() + 1.0
    }

    /// Compute TF for a term in a document
    fn tf(term_count: usize, total_terms: usize) -> f32 {
        if total_terms == 0 {
            return 0.0;
        }
        term_count as f32 / total_terms as f32
    }

    /// Get statistics about the embedding model
    pub fn stats(&self) -> EmbeddingStats {
        EmbeddingStats {
            total_docs: self.total_docs,
            vocab_size: self.document_freq.len().min(self.max_vocab_size),
            dimension: self.dimension(),
        }
    }
}

impl EmbeddingProvider for TfIdfEmbedding {
    fn embed(&self, text: &str) -> Vec<f32> {
        let tokens = tokenize_code(text);
        let total_terms = tokens.len();

        // Count term frequencies
        let mut term_freq: HashMap<String, usize> = HashMap::new();
        for token in &tokens {
            *term_freq.entry(token.clone()).or_insert(0) += 1;
        }

        // Build TF-IDF vector
        let mut vector = vec![0.0; self.dimension()];

        for (term, &count) in &term_freq {
            let idf = self.idf(term);
            // A term the corpus has never seen, or one pruned from the long
            // tail, scores zero and contributes nothing.
            if idf == 0.0 {
                continue;
            }
            // `+=`, not `=`: colliding terms share a slot and their weights add.
            vector[self.slot(term)] += Self::tf(count, total_terms) * idf;
        }

        // L2 normalize the vector
        normalize_vector(&mut vector);

        vector
    }

    fn dimension(&self) -> usize {
        self.max_vocab_size
    }
}

/// Normalize a vector to unit length (L2 norm)
fn normalize_vector(vec: &mut [f32]) {
    let magnitude: f32 = vec.iter().map(|x| x * x).sum::<f32>().sqrt();

    if magnitude > 0.0 {
        for x in vec.iter_mut() {
            *x /= magnitude;
        }
    }
}

/// Compute cosine similarity between two vectors
pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() {
        return 0.0;
    }

    // Since vectors are normalized, dot product = cosine similarity
    a.iter().zip(b.iter()).map(|(x, y)| x * y).sum()
}

/// Pack a dense vector into sparse `(index, value)` pairs, dropping zeros.
pub fn to_sparse(v: &[f32]) -> Vec<(u32, f32)> {
    v.iter()
        .enumerate()
        .filter(|(_, &x)| x != 0.0)
        .map(|(i, &x)| (i as u32, x))
        .collect()
}

/// Unpack a sparse vector back into a dense one of the given dimension.
pub fn to_dense(sparse: &[(u32, f32)], dim: usize) -> Vec<f32> {
    let mut v = vec![0.0; dim];
    for &(i, x) in sparse {
        v[i as usize] = x;
    }
    v
}

/// Cosine similarity between a dense (normalized) query and a sparse
/// (normalized) document — dot product over the document's non-zero entries.
pub fn sparse_cosine(query: &[f32], sparse: &[(u32, f32)]) -> f32 {
    sparse
        .iter()
        .map(|&(i, x)| query.get(i as usize).copied().unwrap_or(0.0) * x)
        .sum()
}

/// Statistics about the embedding model
#[derive(Debug, Clone)]
pub struct EmbeddingStats {
    pub total_docs: usize,
    pub vocab_size: usize,
    pub dimension: usize,
}

/// Repository tag for documents indexed without repository context.
fn unknown_repo() -> Arc<str> {
    Arc::from("")
}

/// A document with its embedding
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmbeddedDocument {
    pub id: String,
    /// Repository this document belongs to (interned; empty when unknown).
    #[serde(default = "unknown_repo")]
    pub repo: Arc<str>,
    pub file_path: String,
    pub content: String,
    pub start_line: usize,
    pub end_line: usize,
    /// Sparse embedding: `(dimension_index, value)` for non-zero entries. TF-IDF
    /// vectors are ~99% zeros (a snippet hits few vocabulary terms), so storing
    /// them sparse cuts the per-symbol cost from ~4 KB (dense 1000-dim) to a
    /// few dozen bytes.
    pub embedding: Vec<(u32, f32)>,
}

/// A similarity search result
#[derive(Debug, Clone)]
pub struct SimilarityResult {
    pub document: EmbeddedDocument,
    pub similarity: f32,
}

/// Vector store for caching embeddings
pub struct VectorStore {
    /// Embedded documents
    documents: Vec<EmbeddedDocument>,
    /// Index for fast lookup by ID
    id_to_idx: HashMap<String, usize>,
}

impl VectorStore {
    pub fn new() -> Self {
        Self {
            documents: Vec::new(),
            id_to_idx: HashMap::new(),
        }
    }

    /// Add a document with its embedding
    pub fn add(&mut self, doc: EmbeddedDocument) {
        let idx = self.documents.len();
        // Keyed per repository: `src/lib.rs::main` exists in every repository,
        // and a bare id would make the last one indexed shadow the others.
        self.id_to_idx
            .insert(crate::search::scoped_doc_id(&doc.repo, &doc.id), idx);
        self.documents.push(doc);
    }

    /// Find similar documents to a query embedding across every repository
    pub fn find_similar(
        &self,
        query_embedding: &[f32],
        max_results: usize,
    ) -> Vec<SimilarityResult> {
        self.find_similar_in(query_embedding, max_results, None)
    }

    /// Find similar documents, optionally restricted to one repository.
    /// The filter runs before the top-k cut, so `max_results` is filled from
    /// the requested repository.
    pub fn find_similar_in(
        &self,
        query_embedding: &[f32],
        max_results: usize,
        repo: Option<&str>,
    ) -> Vec<SimilarityResult> {
        let mut results: Vec<_> = self
            .documents
            .iter()
            .filter(|doc| repo.is_none_or(|repo| &*doc.repo == repo))
            .map(|doc| {
                let similarity = sparse_cosine(query_embedding, &doc.embedding);
                SimilarityResult {
                    document: doc.clone(),
                    similarity,
                }
            })
            .collect();

        // Sort by similarity (descending)
        results.sort_by(|a, b| {
            b.similarity
                .partial_cmp(&a.similarity)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        // Take top results
        results.truncate(max_results);

        results
    }

    /// Get document by ID
    pub fn get(&self, repo: &str, id: &str) -> Option<&EmbeddedDocument> {
        self.id_to_idx
            .get(&crate::search::scoped_doc_id(repo, id))
            .and_then(|&idx| self.documents.get(idx))
    }

    /// Get number of documents
    pub fn len(&self) -> usize {
        self.documents.len()
    }

    /// Approximate memory footprint (diagnostics): (docs, content_bytes, embedding_bytes, index_bytes).
    pub fn memory_breakdown(&self) -> (usize, usize, usize, usize) {
        let content_bytes: usize = self
            .documents
            .iter()
            .map(|d| d.id.len() + d.file_path.len() + d.content.len())
            .sum();
        let embedding_bytes: usize = self
            .documents
            .iter()
            .map(|d| d.embedding.len() * std::mem::size_of::<(u32, f32)>())
            .sum();
        let index_bytes: usize = self
            .id_to_idx
            .keys()
            .map(|k| k.len() + std::mem::size_of::<usize>())
            .sum();
        (
            self.documents.len(),
            content_bytes,
            embedding_bytes,
            index_bytes,
        )
    }

    /// Check if store is empty
    pub fn is_empty(&self) -> bool {
        self.documents.is_empty()
    }

    /// Clear the store
    pub fn clear(&mut self) {
        self.documents.clear();
        self.id_to_idx.clear();
    }
}

impl Default for VectorStore {
    fn default() -> Self {
        Self::new()
    }
}

/// Thread-safe vector store wrapper
pub struct ConcurrentVectorStore {
    inner: RwLock<VectorStore>,
}

impl ConcurrentVectorStore {
    pub fn new() -> Self {
        Self {
            inner: RwLock::new(VectorStore::new()),
        }
    }

    pub fn add(&self, doc: EmbeddedDocument) {
        self.inner.write().add(doc);
    }

    pub fn find_similar(
        &self,
        query_embedding: &[f32],
        max_results: usize,
    ) -> Vec<SimilarityResult> {
        self.inner.read().find_similar(query_embedding, max_results)
    }

    pub fn find_similar_in(
        &self,
        query_embedding: &[f32],
        max_results: usize,
        repo: Option<&str>,
    ) -> Vec<SimilarityResult> {
        self.inner
            .read()
            .find_similar_in(query_embedding, max_results, repo)
    }

    pub fn get(&self, repo: &str, id: &str) -> Option<EmbeddedDocument> {
        self.inner.read().get(repo, id).cloned()
    }

    pub fn len(&self) -> usize {
        self.inner.read().len()
    }

    /// Approximate memory footprint (diagnostics): (docs, content_bytes, embedding_bytes, index_bytes).
    pub fn memory_breakdown(&self) -> (usize, usize, usize, usize) {
        self.inner.read().memory_breakdown()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.read().is_empty()
    }

    pub fn clear(&self) {
        self.inner.write().clear();
    }
}

impl Default for ConcurrentVectorStore {
    fn default() -> Self {
        Self::new()
    }
}

/// Embedding engine that combines provider and store
pub struct EmbeddingEngine {
    provider: Arc<RwLock<TfIdfEmbedding>>,
    store: Arc<ConcurrentVectorStore>,
}

impl EmbeddingEngine {
    pub fn new(max_vocab_size: usize) -> Self {
        Self {
            provider: Arc::new(RwLock::new(TfIdfEmbedding::new(max_vocab_size))),
            store: Arc::new(ConcurrentVectorStore::new()),
        }
    }

    /// Index a code snippet
    pub fn index_snippet(
        &self,
        id: String,
        repo: Arc<str>,
        file_path: String,
        content: String,
        start_line: usize,
        end_line: usize,
    ) {
        self.index_snippet_inner(id, repo, file_path, content, start_line, end_line, true);
    }

    /// Index a snippet but do NOT retain its raw text (persistent-index path).
    /// The embedding is still computed and stored; the display text is dropped
    /// and regenerated from disk at query time (saves ~0.65 GB on the 1C
    /// corpus). The transient per-query engines keep using [`index_snippet`].
    pub fn index_snippet_embed_only(
        &self,
        id: String,
        repo: Arc<str>,
        file_path: String,
        content: String,
        start_line: usize,
        end_line: usize,
    ) {
        self.index_snippet_inner(id, repo, file_path, content, start_line, end_line, false);
    }

    #[allow(clippy::too_many_arguments)]
    fn index_snippet_inner(
        &self,
        id: String,
        repo: Arc<str>,
        file_path: String,
        content: String,
        start_line: usize,
        end_line: usize,
        keep_text: bool,
    ) {
        // Update IDF statistics
        self.provider.write().add_document(&content);

        // Generate embedding (dense), then pack it sparse before storing.
        let embedding = to_sparse(&self.provider.read().embed(&content));

        let stored_text = if keep_text { content } else { String::new() };

        // Store the embedded document
        self.store.add(EmbeddedDocument {
            id,
            repo,
            file_path,
            content: stored_text,
            start_line,
            end_line,
            embedding,
        });
    }

    /// Find similar code to a query string across every repository
    pub fn find_similar_code(&self, query: &str, max_results: usize) -> Vec<SimilarityResult> {
        self.find_similar_code_in(query, max_results, None)
    }

    /// Find similar code to a query string, optionally scoped to one repository
    pub fn find_similar_code_in(
        &self,
        query: &str,
        max_results: usize,
        repo: Option<&str>,
    ) -> Vec<SimilarityResult> {
        let query_embedding = self.provider.read().embed(query);
        self.store
            .find_similar_in(&query_embedding, max_results, repo)
    }

    /// Find code similar to a specific document of a repository. Results are
    /// restricted to that repository — a "what else looks like this symbol"
    /// answer from an unrelated codebase is noise, not a match.
    pub fn find_similar_to_doc(
        &self,
        repo: &str,
        doc_id: &str,
        max_results: usize,
    ) -> Vec<SimilarityResult> {
        if let Some(doc) = self.store.get(repo, doc_id) {
            let query = to_dense(&doc.embedding, self.provider.read().dimension());
            self.store.find_similar_in(&query, max_results, Some(repo))
        } else {
            Vec::new()
        }
    }

    /// Get statistics
    pub fn stats(&self) -> (EmbeddingStats, usize) {
        let embedding_stats = self.provider.read().stats();
        let doc_count = self.store.len();
        (embedding_stats, doc_count)
    }

    /// Approximate memory footprint (diagnostics).
    /// Returns (docs, content_bytes, embedding_bytes, index_bytes, doc_freq_bytes).
    pub fn memory_breakdown(&self) -> (usize, usize, usize, usize, usize) {
        let (docs, content_bytes, embedding_bytes, index_bytes) = self.store.memory_breakdown();
        let df_bytes: usize = self
            .provider
            .read()
            .document_freq
            .keys()
            .map(|k| k.len() + std::mem::size_of::<usize>())
            .sum();
        (docs, content_bytes, embedding_bytes, index_bytes, df_bytes)
    }

    /// Clear all data
    pub fn clear(&self) {
        self.store.clear();
        let mut provider = self.provider.write();
        provider.document_freq.clear();
        provider.total_docs = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tfidf_embedding() {
        let mut tfidf = TfIdfEmbedding::new(100);

        // Add some documents
        tfidf.add_document("fn hello_world() { println!(\"Hello\"); }");
        tfidf.add_document("fn goodbye_world() { println!(\"Goodbye\"); }");
        tfidf.add_document("fn main() { hello_world(); }");

        assert_eq!(tfidf.total_docs, 3);
        assert!(!tfidf.document_freq.is_empty());

        // Generate embedding
        let embedding = tfidf.embed("fn hello_world()");
        assert_eq!(embedding.len(), 100);

        // Check normalization (L2 norm should be ~1.0)
        let magnitude: f32 = embedding.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((magnitude - 1.0).abs() < 0.001 || magnitude == 0.0);
    }

    #[test]
    fn test_document_freq_is_bounded() {
        let mut tfidf = TfIdfEmbedding::new(5);

        // Many documents, each with a distinct token, so the corpus has far
        // more unique terms than the vocabulary.
        for i in 0..100 {
            tfidf.add_document(&format!("unique_token_{i} fn_{i}"));
        }

        let limit = tfidf.max_vocab_size * 2;
        assert!(
            tfidf.document_freq.len() <= limit,
            "document_freq grew to {}, expected <= {}",
            tfidf.document_freq.len(),
            limit
        );
        // Embedding dimension stays the configured size.
        let embedding = tfidf.embed("fn_42");
        assert_eq!(embedding.len(), tfidf.max_vocab_size);
    }

    #[test]
    fn test_cosine_similarity() {
        let vec1 = vec![1.0, 0.0, 0.0];
        let vec2 = vec![1.0, 0.0, 0.0];
        let vec3 = vec![0.0, 1.0, 0.0];

        // Identical vectors
        assert!((cosine_similarity(&vec1, &vec2) - 1.0).abs() < 0.001);

        // Orthogonal vectors
        assert!((cosine_similarity(&vec1, &vec3) - 0.0).abs() < 0.001);
    }

    #[test]
    fn test_normalize_vector() {
        let mut vec = vec![3.0, 4.0];
        normalize_vector(&mut vec);

        let magnitude: f32 = vec.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((magnitude - 1.0).abs() < 0.001);
    }

    #[test]
    fn test_vector_store() {
        let mut store = VectorStore::new();

        let doc1 = EmbeddedDocument {
            id: "doc1".to_string(),
            repo: Arc::from("demo"),
            file_path: "test.rs".to_string(),
            content: "fn hello()".to_string(),
            start_line: 1,
            end_line: 5,
            embedding: vec![(0, 1.0)],
        };

        let doc2 = EmbeddedDocument {
            id: "doc2".to_string(),
            repo: Arc::from("demo"),
            file_path: "test2.rs".to_string(),
            content: "fn goodbye()".to_string(),
            start_line: 10,
            end_line: 15,
            embedding: vec![(0, 0.9), (1, 0.1)],
        };

        store.add(doc1);
        store.add(doc2);

        assert_eq!(store.len(), 2);

        // Find similar to a query
        let query = vec![1.0, 0.0, 0.0];
        let results = store.find_similar(&query, 2);

        assert_eq!(results.len(), 2);
        assert_eq!(results[0].document.id, "doc1"); // Should be most similar
        assert!(results[0].similarity > results[1].similarity);
    }

    #[test]
    fn test_embedding_engine() {
        let engine = EmbeddingEngine::new(100);

        engine.index_snippet(
            "test1".to_string(),
            Arc::from("demo"),
            "test.rs".to_string(),
            "fn calculate_sum(a: i32, b: i32) -> i32 { a + b }".to_string(),
            1,
            1,
        );

        engine.index_snippet(
            "test2".to_string(),
            Arc::from("demo"),
            "test.rs".to_string(),
            "fn calculate_product(x: i32, y: i32) -> i32 { x * y }".to_string(),
            3,
            3,
        );

        engine.index_snippet(
            "test3".to_string(),
            Arc::from("demo"),
            "test.rs".to_string(),
            "fn print_hello() { println!(\"Hello\"); }".to_string(),
            5,
            5,
        );

        // Find similar to a math function
        let results = engine.find_similar_code("fn add_numbers(a: i32, b: i32)", 3);
        assert!(!results.is_empty());

        // The math functions (test1, test2) should rank higher than print_hello (test3)
        // We check that test3 (non-math) is either not in results or ranks last
        let result_ids: Vec<&str> = results.iter().map(|r| r.document.id.as_str()).collect();
        assert!(
            !result_ids.contains(&"test3") || result_ids.last() == Some(&"test3"),
            "print_hello should rank lower than math functions, got: {:?}",
            result_ids
        );

        // Stats
        let (stats, doc_count) = engine.stats();
        assert_eq!(doc_count, 3);
        assert_eq!(stats.total_docs, 3);
        assert!(stats.vocab_size > 0);
    }

    #[test]
    fn test_find_similar_to_doc() {
        let engine = EmbeddingEngine::new(100);

        engine.index_snippet(
            "doc1".to_string(),
            Arc::from("demo"),
            "test.rs".to_string(),
            "fn fibonacci(n: u32) -> u32 { if n <= 1 { n } else { fibonacci(n-1) + fibonacci(n-2) } }".to_string(),
            1,
            5,
        );

        engine.index_snippet(
            "doc2".to_string(),
            Arc::from("demo"),
            "test.rs".to_string(),
            "fn factorial(n: u32) -> u32 { if n <= 1 { 1 } else { n * factorial(n-1) } }"
                .to_string(),
            7,
            11,
        );

        engine.index_snippet(
            "doc3".to_string(),
            Arc::from("demo"),
            "test.rs".to_string(),
            "fn print_message(msg: &str) { println!(\"{}\", msg); }".to_string(),
            13,
            15,
        );

        // Find similar to fibonacci
        let results = engine.find_similar_to_doc("demo", "doc1", 3);
        assert!(results.len() >= 2);

        // First result should be itself (doc1)
        assert_eq!(results[0].document.id, "doc1");

        // Other results should include doc2 and doc3 (order may vary by platform)
        let other_ids: Vec<&str> = results
            .iter()
            .skip(1)
            .map(|r| r.document.id.as_str())
            .collect();
        assert!(other_ids.contains(&"doc2") || other_ids.contains(&"doc3"));
    }

    #[test]
    fn test_empty_text_embedding() {
        let tfidf = TfIdfEmbedding::new(100);
        let embedding = tfidf.embed("");
        assert_eq!(embedding.len(), 100);
        assert!(embedding.iter().all(|&x| x == 0.0));
    }

    #[test]
    fn test_code_tokenization_integration() {
        let mut tfidf = TfIdfEmbedding::new(100);

        // Test with actual code patterns
        tfidf.add_document("getUserById");
        tfidf.add_document("get_user_by_id");
        tfidf.add_document("GetUserById");

        let emb1 = tfidf.embed("getUserById");
        let emb2 = tfidf.embed("get_user_by_id");

        // Should have high similarity due to tokenization
        let sim = cosine_similarity(&emb1, &emb2);
        assert!(
            sim > 0.5,
            "Similarity should be high for similar identifiers"
        );
    }

    /// Two repositories share the id `src/lib.rs::main`. Keyed by bare id, the
    /// second insert shadowed the first and `find_similar_to_doc` answered from
    /// whichever repository happened to be indexed last.
    #[test]
    fn test_colliding_ids_across_repos_stay_separate() {
        let engine = EmbeddingEngine::new(100);

        engine.index_snippet(
            "src/lib.rs::main".to_string(),
            Arc::from("alpha"),
            "src/lib.rs".to_string(),
            "fn main() { alpha_specific_marker(); }".to_string(),
            1,
            3,
        );
        engine.index_snippet(
            "src/lib.rs::main".to_string(),
            Arc::from("beta"),
            "src/lib.rs".to_string(),
            "fn main() { beta_specific_marker(); }".to_string(),
            1,
            3,
        );

        for repo in ["alpha", "beta"] {
            let results = engine.find_similar_to_doc(repo, "src/lib.rs::main", 5);
            assert!(!results.is_empty(), "{repo}: document was shadowed");
            assert!(
                results.iter().all(|r| &*r.document.repo == repo),
                "{repo}: results leaked from another repository"
            );
        }
    }

    #[test]
    fn test_find_similar_code_in_scopes_to_repo() {
        let engine = EmbeddingEngine::new(100);

        engine.index_snippet(
            "alpha.rs::sum".to_string(),
            Arc::from("alpha"),
            "alpha.rs".to_string(),
            "fn calculate_sum(a: i32, b: i32) -> i32 { a + b }".to_string(),
            1,
            1,
        );
        engine.index_snippet(
            "beta.rs::sum".to_string(),
            Arc::from("beta"),
            "beta.rs".to_string(),
            "fn calculate_sum(a: i32, b: i32) -> i32 { a + b }".to_string(),
            1,
            1,
        );

        let all = engine.find_similar_code("calculate sum", 10);
        assert_eq!(all.len(), 2);

        let scoped = engine.find_similar_code_in("calculate sum", 10, Some("beta"));
        assert_eq!(scoped.len(), 1);
        assert_eq!(&*scoped[0].document.repo, "beta");

        assert!(engine
            .find_similar_code_in("calculate sum", 10, Some("gamma"))
            .is_empty());
    }

    #[test]
    fn test_embedding_sort_handles_nan() {
        let mut store = VectorStore::new();
        store.add(EmbeddedDocument {
            id: "a".to_string(),
            repo: Arc::from("demo"),
            file_path: "a.rs".to_string(),
            content: "fn a()".to_string(),
            start_line: 1,
            end_line: 1,
            embedding: vec![(0, 1.0)],
        });
        store.add(EmbeddedDocument {
            id: "c".to_string(),
            repo: Arc::from("demo"),
            file_path: "c.rs".to_string(),
            content: "fn c()".to_string(),
            start_line: 1,
            end_line: 1,
            embedding: vec![(0, 1.0)],
        });

        // Test that find_similar doesn't panic (uses the fixed sort internally)
        let query_with_nan = vec![f32::NAN];
        let results_from_store = store.find_similar(&query_with_nan, 10);
        // Should not panic
        assert_eq!(results_from_store.len(), 2);

        // Also directly test sort with NaN values
        let make_result = |id: &str, sim: f32| SimilarityResult {
            document: EmbeddedDocument {
                id: id.to_string(),
                repo: Arc::from("demo"),
                file_path: format!("{}.rs", id),
                content: format!("fn {}()", id),
                start_line: 1,
                end_line: 1,
                embedding: vec![(0, 1.0)],
            },
            similarity: sim,
        };
        let mut results = [
            make_result("a", 0.9),
            make_result("b", f32::NAN),
            make_result("c", 0.5),
        ];

        // This should not panic with our fix
        results.sort_by(|a, b| {
            b.similarity
                .partial_cmp(&a.similarity)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        // The non-NaN values should still be properly ordered
        let non_nan: Vec<f32> = results
            .iter()
            .filter(|r| !r.similarity.is_nan())
            .map(|r| r.similarity)
            .collect();
        if non_nan.len() >= 2 {
            assert!(
                non_nan[0] >= non_nan[1],
                "Non-NaN values should be sorted descending"
            );
        }
    }

    /// The reproduction from the v1.9.0 review. Two documents with identical
    /// text, inserted 500 documents apart, must rank alike — the vocabulary
    /// used to be re-indexed by document-frequency rank on every insert, so
    /// they ended up in different coordinate bases and the early one fell out
    /// of the results entirely (`late` 1.00, noise 0.58, `early` not in top 5).
    #[test]
    fn test_identical_documents_rank_alike_regardless_of_insertion_order() {
        let engine = EmbeddingEngine::new(4096);
        let repo: Arc<str> = Arc::from("test-repo");
        let text = "fn compute_checksum(buffer: &[u8]) -> u32 { buffer.iter().fold(0, |a, b| a ^ *b as u32) }";

        let index = |id: &str, content: &str| {
            engine.index_snippet(
                id.to_string(),
                Arc::clone(&repo),
                format!("{id}.rs"),
                content.to_string(),
                1,
                1,
            );
        };

        index("early", text);
        for i in 0..500 {
            index(
                &format!("noise{i}"),
                &format!("fn unrelated_helper_{i}(value: usize) -> usize {{ value + {i} }}"),
            );
        }
        index("late", text);

        let results = engine.find_similar_code(text, 5);
        let ranked: Vec<&str> = results.iter().map(|r| r.document.id.as_str()).collect();
        assert!(
            ranked.contains(&"early") && ranked.contains(&"late"),
            "both copies must surface, got {ranked:?}"
        );

        let score = |id: &str| {
            results
                .iter()
                .find(|r| r.document.id == id)
                .map(|r| r.similarity)
                .unwrap()
        };
        // Their scores need not be bit-identical — IDF legitimately moves as
        // the corpus grows — but they must be close, and both must beat noise.
        assert!(
            (score("early") - score("late")).abs() < 0.05,
            "identical text scored {} vs {}",
            score("early"),
            score("late")
        );
        let best_noise = results
            .iter()
            .filter(|r| r.document.id.starts_with("noise"))
            .map(|r| r.similarity)
            .fold(0.0f32, f32::max);
        assert!(
            score("early") > best_noise,
            "an exact copy scored {} against noise at {best_noise}",
            score("early")
        );
    }

    /// A term's slot must depend only on the term, never on what else the
    /// corpus has seen — that independence is the whole fix.
    #[test]
    fn test_term_slots_are_independent_of_corpus_state() {
        let mut fresh = TfIdfEmbedding::new(256);
        fresh.add_document("alpha beta");

        let mut crowded = TfIdfEmbedding::new(256);
        for i in 0..300 {
            crowded.add_document(&format!("filler_{i} beta beta beta"));
        }
        crowded.add_document("alpha beta");

        for term in ["alpha", "beta"] {
            assert_eq!(
                fresh.slot(term),
                crowded.slot(term),
                "slot for {term} moved with corpus state"
            );
        }
    }
}
