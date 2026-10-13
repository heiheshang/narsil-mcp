//! Tantivy-backed full-text index.
//!
//! Replaces the hand-rolled BM25 in [`crate::search`], which kept its entire
//! inverted index in a `HashMap<String, Vec<usize>>` in RAM and was never
//! persisted: every process start re-tokenized the whole corpus from scratch.
//!
//! The corpus index is persisted under the cache directory, so a restart reads
//! segments off disk instead of re-reading and re-tokenizing every source file.
//!
//! Raw text retention is a per-document choice, not a per-index one. Files and
//! symbols are indexed without retaining their text — snippets are regenerated
//! from `file_cache` at query time, which is what kept ~2.4 GB off the heap on
//! the 1C corpus. Chunks written by `hybrid_search` *are* retained, because its
//! callers read `SearchResult::document.content` straight back out; unlike the
//! map this replaces, that text now lives compressed on disk and is touched
//! only for documents that reach the result set.
//!
//! # Freshness
//!
//! The previous index was a plain map: a document was searchable the moment it
//! was added. Tantivy needs a `commit` plus a reader reload. Rather than push
//! that onto every caller, the index tracks a dirty flag and commits lazily on
//! the next read. A commit is therefore paid once per indexing burst, not once
//! per document.

use anyhow::{Context, Result};
use parking_lot::{Mutex, RwLock};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tantivy::collector::{Count, TopDocs};
use tantivy::query::{BooleanQuery, BoostQuery, Occur, Query, TermQuery};
use tantivy::schema::{
    Field, IndexRecordOption, Schema, TextFieldIndexing, TextOptions, Value, FAST, INDEXED, STORED,
    STRING,
};
use tantivy::tokenizer::{TextAnalyzer, Token, TokenStream, Tokenizer};
use tantivy::{
    DocId, DocSet, Index, IndexReader, IndexWriter, ReloadPolicy, SegmentReader, TantivyDocument,
    Term,
};

use crate::search::{tokenize_code, DocType, SearchDocument, TOKENIZER_VERSION};

#[cfg(feature = "native")]
const TOKENIZER_MARKER: &str = "narsil-tokenizer-version";

/// Name under which [`CodeTokenizer`] is registered with tantivy.
pub const CODE_TOKENIZER: &str = "code";

/// Heap budget handed to the tantivy writer, in bytes.
///
/// Tantivy flushes a segment whenever this fills, so it trades peak RSS against
/// segment count. 64 MB keeps segment counts sane on large corpora while
/// staying well inside the memory budget the server runs under.
const WRITER_HEAP_BYTES: usize = 64 * 1024 * 1024;

/// Boost applied to matches on the symbol-name field relative to the body.
///
/// The hand-rolled index doubled the score of any document whose id contained
/// the raw query as a substring. Matching the query's *tokens* against the name
/// is the same intent expressed in a way the inverted index can answer.
const NAME_BOOST: f32 = 2.0;

// ============================================================================
// Code-aware tokenizer
// ============================================================================

/// Tantivy tokenizer that defers to [`tokenize_code`].
///
/// Reusing the existing function rather than composing tantivy's built-in
/// filters keeps term semantics identical to the index this replaces:
/// `camelCase`/`snake_case` splitting, stop-word removal and case folding all
/// stay in one place, so a persisted index and a freshly built one agree.
#[derive(Clone, Default)]
pub struct CodeTokenizer;

pub struct CodeTokenStream {
    tokens: Vec<String>,
    idx: usize,
    token: Token,
}

impl Tokenizer for CodeTokenizer {
    type TokenStream<'a> = CodeTokenStream;

    fn token_stream<'a>(&'a mut self, text: &'a str) -> CodeTokenStream {
        CodeTokenStream {
            tokens: tokenize_code(text),
            idx: 0,
            token: Token::default(),
        }
    }
}

impl TokenStream for CodeTokenStream {
    fn advance(&mut self) -> bool {
        if self.idx >= self.tokens.len() {
            return false;
        }
        self.token.text.clear();
        self.token.text.push_str(&self.tokens[self.idx]);
        self.token.position = self.idx;
        // `tokenize_code` synthesises terms (it splits identifiers), so a term
        // has no single byte range in the source. Offsets stay zero; nothing
        // here highlights from them — snippets are line-based.
        self.token.offset_from = 0;
        self.token.offset_to = 0;
        self.idx += 1;
        true
    }

    fn token(&self) -> &Token {
        &self.token
    }

    fn token_mut(&mut self) -> &mut Token {
        &mut self.token
    }
}

// ============================================================================
// Schema
// ============================================================================

#[derive(Clone, Copy)]
struct Fields {
    id: Field,
    repo: Field,
    file_path: Field,
    doc_type: Field,
    start_line: Field,
    end_line: Field,
    /// Token count, kept as a fast field so `stats` can average it without
    /// touching the documents themselves.
    doc_len: Field,
    /// Full document text, indexed for ranking.
    body: Field,
    /// Symbol name / file path, indexed separately so name hits can be boosted.
    name: Field,
    /// Raw text. Stored, but written only for documents whose callers read it
    /// back; omitting it for the rest costs nothing in the schema.
    content: Field,
    /// `repo\x1fpath`, so a single file's documents can be deleted in one
    /// term. Tantivy deletes by one term, and neither `repo` nor `file_path`
    /// identifies a file on its own across repositories.
    repo_path: Field,
}

fn build_schema() -> (Schema, Fields) {
    let mut b = Schema::builder();

    let text_indexing = TextFieldIndexing::default()
        .set_tokenizer(CODE_TOKENIZER)
        // Frequencies are what BM25 needs. Positions would roughly double the
        // postings and nothing here runs phrase queries.
        .set_index_option(IndexRecordOption::WithFreqs);
    let text_opts = TextOptions::default().set_indexing_options(text_indexing);

    let fields = Fields {
        id: b.add_text_field("id", STRING | STORED),
        repo: b.add_text_field("repo", STRING | STORED),
        file_path: b.add_text_field("file_path", STRING | STORED),
        doc_type: b.add_text_field("doc_type", STRING | STORED),
        start_line: b.add_u64_field("start_line", STORED),
        end_line: b.add_u64_field("end_line", STORED),
        doc_len: b.add_u64_field("doc_len", FAST | INDEXED),
        body: b.add_text_field("body", text_opts.clone()),
        name: b.add_text_field("name", text_opts),
        content: b.add_text_field("content", STORED),
        repo_path: b.add_text_field("repo_path", STRING),
    };

    (b.build(), fields)
}

fn doc_type_label(t: &DocType) -> &'static str {
    match t {
        DocType::File => "file",
        DocType::Function => "function",
        DocType::Class => "class",
        DocType::Struct => "struct",
        DocType::Method => "method",
        DocType::Other => "other",
    }
}

fn doc_type_from_label(s: &str) -> DocType {
    match s {
        "file" => DocType::File,
        "function" => DocType::Function,
        "class" => DocType::Class,
        "struct" => DocType::Struct,
        "method" => DocType::Method,
        _ => DocType::Other,
    }
}

const ALL_DOC_TYPES: [DocType; 6] = [
    DocType::File,
    DocType::Function,
    DocType::Class,
    DocType::Struct,
    DocType::Method,
    DocType::Other,
];

// ============================================================================
// Index
// ============================================================================

pub struct TantivyIndex {
    index: Index,
    /// Directory the index lives in, when it is persisted. `list_managed_files`
    /// reports paths relative to it, so without this `disk_bytes` stats
    /// nothing and reports zero.
    dir: Option<std::path::PathBuf>,
    /// The writer is `Mutex`-wrapped rather than shared: tantivy parallelises
    /// indexing internally across its own threads, and `commit` needs `&mut`.
    /// `add_document` itself takes `&self`, so the lock is held only briefly.
    writer: Mutex<IndexWriter>,
    reader: IndexReader,
    fields: Fields,
    /// Code-specific synonyms, applied to query terms only. Expanding at query
    /// time rather than index time keeps the postings small and lets the table
    /// change without a reindex.
    synonyms: HashMap<String, Vec<String>>,
    /// Set when documents have been added since the last commit.
    dirty: AtomicBool,
    /// Guards the commit-then-reload sequence so concurrent readers do not
    /// each commit.
    refresh: Mutex<()>,
}

impl TantivyIndex {
    /// Build an in-RAM index. Used by WASM, by tests, and by the transient
    /// per-query index in `hybrid_search`.
    pub fn in_memory() -> Result<Self> {
        let (schema, fields) = build_schema();
        let index = Index::create_in_ram(schema);
        Self::from_index(index, fields)
    }

    /// Open (or create) a persisted index rooted at `path`.
    ///
    /// This is the whole point of the migration: the corpus index survives a
    /// restart instead of being rebuilt from source on every boot.
    #[cfg(feature = "native")]
    pub fn open_or_create(path: &std::path::Path) -> Result<Self> {
        use tantivy::directory::MmapDirectory;

        // The index is a cache: drop it, indexing refills it.
        let marker = path.join(TOKENIZER_MARKER);
        let indexed_with = std::fs::read_to_string(&marker)
            .ok()
            .and_then(|s| s.trim().parse::<u32>().ok());
        if path.join("meta.json").exists() && indexed_with != Some(TOKENIZER_VERSION) {
            tracing::info!(
                "Tantivy index at {:?} built with tokenizer {:?}, current is {}; rebuilding",
                path,
                indexed_with,
                TOKENIZER_VERSION
            );
            std::fs::remove_dir_all(path)
                .with_context(|| format!("remove stale tantivy index {:?}", path))?;
        }

        std::fs::create_dir_all(path)
            .with_context(|| format!("create tantivy index dir {:?}", path))?;
        std::fs::write(&marker, TOKENIZER_VERSION.to_string())
            .with_context(|| format!("write tokenizer marker {:?}", marker))?;
        let dir = MmapDirectory::open(path)
            .with_context(|| format!("open tantivy index dir {:?}", path))?;

        let (schema, fields) = build_schema();
        // A schema change between versions makes the on-disk index unreadable.
        // That is a cache, not a source of truth: wipe and rebuild rather than
        // refuse to start.
        let index = match Index::open_or_create(dir, schema.clone()) {
            Ok(index) => index,
            Err(e) => {
                tracing::warn!(
                    "Tantivy index at {:?} unusable ({}); rebuilding from scratch",
                    path,
                    e
                );
                std::fs::remove_dir_all(path).ok();
                std::fs::create_dir_all(path)
                    .with_context(|| format!("recreate tantivy index dir {:?}", path))?;
                std::fs::write(&marker, TOKENIZER_VERSION.to_string())
                    .with_context(|| format!("write tokenizer marker {:?}", marker))?;
                let dir = MmapDirectory::open(path)
                    .with_context(|| format!("reopen tantivy index dir {:?}", path))?;
                Index::open_or_create(dir, schema).context("create tantivy index")?
            }
        };

        Self::from_index_at(index, fields, Some(path.to_path_buf()))
    }

    fn from_index(index: Index, fields: Fields) -> Result<Self> {
        Self::from_index_at(index, fields, None)
    }

    fn from_index_at(
        index: Index,
        fields: Fields,
        dir: Option<std::path::PathBuf>,
    ) -> Result<Self> {
        index
            .tokenizers()
            .register(CODE_TOKENIZER, TextAnalyzer::from(CodeTokenizer));

        let writer: IndexWriter = index
            .writer(WRITER_HEAP_BYTES)
            .context("create tantivy writer")?;

        let reader = index
            .reader_builder()
            // Reloads are driven explicitly after commit, so the index is never
            // read in a state the caller did not ask for.
            .reload_policy(ReloadPolicy::Manual)
            .try_into()
            .context("create tantivy reader")?;

        Ok(Self {
            index,
            dir,
            writer: Mutex::new(writer),
            reader,
            fields,
            synonyms: crate::search::build_code_synonyms(),
            dirty: AtomicBool::new(false),
            refresh: Mutex::new(()),
        })
    }

    /// Add a document, indexing its text without retaining it.
    ///
    /// It becomes visible to searches after the next refresh, which happens
    /// automatically on the next read.
    pub fn add_document(&self, doc: SearchDocument) -> Result<()> {
        self.add(doc, false, None)
    }

    /// [`add_document`](Self::add_document) with the document length
    /// (`tokenize_code(content).len()`) already counted by the caller, which
    /// can do it in parallel; this runs on the indexing thread.
    pub fn add_document_counted(&self, doc: SearchDocument, doc_len: u64) -> Result<()> {
        self.add(doc, false, Some(doc_len))
    }

    /// Add a document and keep its raw text, so it round-trips through
    /// [`SearchDocument::content`]. Only for callers that read it back.
    pub fn add_document_retaining_text(&self, doc: SearchDocument) -> Result<()> {
        self.add(doc, true, None)
    }

    fn add(&self, doc: SearchDocument, retain_text: bool, doc_len: Option<u64>) -> Result<()> {
        let f = &self.fields;
        let mut td = TantivyDocument::new();

        td.add_text(f.id, &doc.id);
        td.add_text(f.repo, &*doc.repo);
        td.add_text(f.file_path, &doc.file_path);
        td.add_text(f.doc_type, doc_type_label(&doc.doc_type));
        td.add_u64(f.start_line, doc.start_line as u64);
        td.add_u64(f.end_line, doc.end_line as u64);

        // `name` carries the trailing `::symbol` when there is one, else the
        // path, so a query naming a function outranks one merely mentioning it.
        let name = doc.id.rsplit("::").next().unwrap_or(&doc.id);
        td.add_text(f.name, name);

        let body = if doc.content.is_empty() {
            // Callers that pre-tokenized (the normalized-document path) hand us
            // term frequencies instead of text. Reconstruct a bag of words: term
            // order is irrelevant to BM25, only counts are.
            expand_term_freq(&doc.term_freq)
        } else {
            doc.content.clone()
        };

        let doc_len = match doc_len {
            Some(counted) => {
                debug_assert_eq!(counted, tokenize_code(&body).len() as u64);
                counted
            }
            None => tokenize_code(&body).len() as u64,
        };
        td.add_u64(f.doc_len, doc_len);
        td.add_text(f.body, &body);

        if retain_text {
            td.add_text(f.content, &doc.content);
        }

        td.add_text(
            f.repo_path,
            crate::search::scoped_doc_id(&doc.repo, &doc.file_path),
        );

        self.writer
            .lock()
            .add_document(td)
            .context("add document to tantivy")?;
        self.dirty.store(true, Ordering::Release);
        Ok(())
    }

    /// Commit pending writes and make them visible to readers.
    ///
    /// Callers may invoke this at the end of an indexing pass to pay the cost
    /// up front; otherwise the next search does it.
    pub fn commit(&self) -> Result<()> {
        let _guard = self.refresh.lock();
        self.commit_locked()
    }

    fn commit_locked(&self) -> Result<()> {
        if !self.dirty.swap(false, Ordering::AcqRel) {
            return Ok(());
        }
        self.writer.lock().commit().context("tantivy commit")?;
        self.reader.reload().context("tantivy reader reload")?;
        Ok(())
    }

    /// Commit if there is anything pending, so a read sees every write.
    ///
    /// The lock is taken unconditionally. `dirty` is cleared when a commit
    /// *starts*, so a lock-free pre-check can observe "clean" while another
    /// thread sits between its `commit` and its `reader.reload` — and then
    /// read a snapshot that predates both. Taking the lock makes a reader wait
    /// out an in-flight commit instead of racing it. It is an uncontended
    /// mutex on the common path, and it is not held across the search itself.
    fn ensure_fresh(&self) {
        let _guard = self.refresh.lock();
        if let Err(e) = self.commit_locked() {
            tracing::warn!("Tantivy refresh failed: {}", e);
        }
    }

    /// BM25 search, optionally restricted to a single repository.
    ///
    /// The repository filter is a clause of the query rather than a filter over
    /// results, so `max_results` is spent inside the requested repository — the
    /// property the previous implementation took care to provide.
    pub fn search_in(
        &self,
        query: &str,
        max_results: usize,
        repo: Option<&str>,
    ) -> Vec<(SearchDocument, f64, Vec<String>)> {
        if max_results == 0 {
            return Vec::new();
        }
        self.ensure_fresh();

        let tokens = tokenize_code(query);
        if tokens.is_empty() {
            return Vec::new();
        }

        let searcher = self.reader.searcher();
        let f = &self.fields;

        let mut body_clauses: Vec<(Occur, Box<dyn Query>)> = Vec::new();
        let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
        let mut body_terms: Vec<&str> = Vec::new();

        for t in &tokens {
            // The name field carries the literal query terms only: a synonym
            // has no business collecting the exact-name boost.
            let name_term = Term::from_field_text(f.name, t);
            body_clauses.push((
                Occur::Should,
                Box::new(BoostQuery::new(
                    Box::new(TermQuery::new(name_term, IndexRecordOption::WithFreqs)),
                    NAME_BOOST,
                )),
            ));
        }

        for t in tokens.iter().map(String::as_str).chain(
            tokens
                .iter()
                .filter_map(|t| self.synonyms.get(t))
                .flatten()
                .map(String::as_str),
        ) {
            if !seen.insert(t) {
                continue;
            }
            body_terms.push(t);
            let term = Term::from_field_text(f.body, t);
            body_clauses.push((
                Occur::Should,
                Box::new(TermQuery::new(term, IndexRecordOption::WithFreqs)),
            ));
        }

        let text_query = BooleanQuery::new(body_clauses);

        let query: Box<dyn Query> = match repo {
            Some(r) if !r.is_empty() => {
                let repo_term = Term::from_field_text(f.repo, r);
                Box::new(BooleanQuery::new(vec![
                    (
                        Occur::Must,
                        Box::new(TermQuery::new(repo_term, IndexRecordOption::Basic)) as Box<_>,
                    ),
                    (Occur::Must, Box::new(text_query) as Box<_>),
                ]))
            }
            _ => Box::new(text_query),
        };

        let hits = match searcher.search(&query, &TopDocs::with_limit(max_results)) {
            Ok(hits) => hits,
            Err(e) => {
                tracing::warn!("Tantivy search failed: {}", e);
                return Vec::new();
            }
        };

        hits.into_iter()
            .filter_map(|(score, addr)| {
                let td: TantivyDocument = searcher.doc(addr).ok()?;
                let doc = self.document_from(&td)?;
                // Tantivy does not report which clauses matched.
                let segment = searcher.segment_reader(addr.segment_ord);
                let matched = terms_in_doc(segment, f.body, &body_terms, addr.doc_id);
                Some((doc, score as f64, matched))
            })
            .collect()
    }

    fn document_from(&self, td: &TantivyDocument) -> Option<SearchDocument> {
        let f = &self.fields;
        let text = |field: Field| -> Option<String> {
            td.get_first(field)
                .and_then(|v| v.as_str())
                .map(str::to_string)
        };
        let num = |field: Field| -> usize {
            td.get_first(field).and_then(|v| v.as_u64()).unwrap_or(0) as usize
        };

        Some(SearchDocument {
            id: text(f.id)?,
            repo: Arc::from(text(f.repo).unwrap_or_default().as_str()),
            file_path: text(f.file_path).unwrap_or_default(),
            content: text(f.content).unwrap_or_default(),
            doc_type: doc_type_from_label(&text(f.doc_type).unwrap_or_default()),
            start_line: num(f.start_line),
            end_line: num(f.end_line),
            // Term frequencies were an artefact of the hand-rolled scorer.
            // Nothing outside the old index ever read them back.
            term_freq: HashMap::new(),
        })
    }

    /// Number of live documents.
    pub fn num_docs(&self) -> usize {
        self.ensure_fresh();
        self.reader.searcher().num_docs() as usize
    }

    /// Distinct indexed terms in the body field, summed across segments.
    pub fn num_terms(&self) -> usize {
        self.ensure_fresh();
        let searcher = self.reader.searcher();
        searcher
            .segment_readers()
            .iter()
            .filter_map(|sr| sr.inverted_index(self.fields.body).ok())
            .map(|inv| inv.terms().num_terms())
            .sum()
    }

    /// Mean document length in tokens, read off the `doc_len` fast field.
    pub fn avg_doc_len(&self) -> f64 {
        self.ensure_fresh();
        let searcher = self.reader.searcher();
        let mut total = 0u64;
        let mut count = 0u64;

        for sr in searcher.segment_readers() {
            let Ok(col) = sr.fast_fields().u64("doc_len") else {
                continue;
            };
            for doc in 0..sr.max_doc() {
                if sr.alive_bitset().is_some_and(|b| !b.is_alive(doc)) {
                    continue;
                }
                if let Some(v) = col.first(doc) {
                    total += v;
                    count += 1;
                }
            }
        }

        if count == 0 {
            0.0
        } else {
            total as f64 / count as f64
        }
    }

    /// Document counts per [`DocType`], one cheap term query each.
    pub fn doc_type_counts(&self) -> HashMap<DocType, usize> {
        self.ensure_fresh();
        let searcher = self.reader.searcher();
        let mut out = HashMap::new();

        for dt in ALL_DOC_TYPES {
            let term = Term::from_field_text(self.fields.doc_type, doc_type_label(&dt));
            let q = TermQuery::new(term, IndexRecordOption::Basic);
            if let Ok(n) = searcher.search(&q, &Count) {
                if n > 0 {
                    out.insert(dt, n);
                }
            }
        }
        out
    }

    /// Drop every document.
    pub fn clear(&self) -> Result<()> {
        let _guard = self.refresh.lock();
        {
            let mut w = self.writer.lock();
            // `delete_all_documents` removes committed segments and rewinds the
            // stamper; it does not touch documents still buffered in the
            // writer, which the commit below would flush straight back into a
            // fresh segment. Landing them in a segment first makes the delete
            // total.
            if self.dirty.swap(false, Ordering::AcqRel) {
                w.commit().context("tantivy commit before clear")?;
            }
            w.delete_all_documents().context("tantivy delete_all")?;
            w.commit().context("tantivy commit after clear")?;
        }
        self.reader.reload().context("tantivy reader reload")?;
        Ok(())
    }

    /// Remove every document belonging to `repo`, so a reindex replaces rather
    /// than duplicates it.
    pub fn delete_repo(&self, repo: &str) -> Result<()> {
        let term = Term::from_field_text(self.fields.repo, repo);
        self.writer.lock().delete_term(term);
        self.dirty.store(true, Ordering::Release);
        Ok(())
    }

    /// Remove every document produced from one file — its file document and
    /// each symbol within it — so a changed file can be re-indexed without
    /// leaving its previous symbols behind.
    pub fn delete_file(&self, repo: &str, file_path: &str) {
        let term = Term::from_field_text(
            self.fields.repo_path,
            &crate::search::scoped_doc_id(repo, file_path),
        );
        self.writer.lock().delete_term(term);
        self.dirty.store(true, Ordering::Release);
    }

    /// How many documents a repository currently has indexed.
    ///
    /// Guards the incremental path: the search index and the symbol store are
    /// separate files, and a user who deletes one but not the other must not
    /// end up with a repository that is "cached" yet unsearchable.
    pub fn repo_doc_count(&self, repo: &str) -> usize {
        self.ensure_fresh();
        let term = Term::from_field_text(self.fields.repo, repo);
        let q = TermQuery::new(term, IndexRecordOption::Basic);
        self.reader.searcher().search(&q, &Count).unwrap_or(0)
    }

    /// Bytes the index occupies on disk, or 0 for an in-RAM index.
    pub fn disk_bytes(&self) -> u64 {
        let Some(dir) = self.dir.as_ref() else {
            return 0;
        };
        self.index
            .directory()
            .list_managed_files()
            .iter()
            .filter_map(|p| std::fs::metadata(dir.join(p)).ok())
            .map(|m| m.len())
            .sum()
    }
}

/// Rebuild a bag of words from term frequencies.
///
/// Used only by the normalized-document path, which tokenizes before it reaches
/// the index. BM25 depends on counts, not order, so repetition is faithful.
/// The `doc_len` [`add`](TantivyIndex::add) stores for a document given as
/// term frequencies.
pub fn term_freq_doc_len(tf: &HashMap<String, usize>) -> u64 {
    tokenize_code(&expand_term_freq(tf)).len() as u64
}

fn expand_term_freq(tf: &HashMap<String, usize>) -> String {
    let mut out = String::new();
    for (term, &count) in tf {
        for _ in 0..count {
            out.push_str(term);
            out.push(' ');
        }
    }
    out
}

/// Thread-safe handle, kept `RwLock`-free: tantivy handles its own
/// concurrency, so this is a thin `Arc`-able wrapper.
pub struct ConcurrentTantivyIndex {
    inner: RwLock<Arc<TantivyIndex>>,
}

impl ConcurrentTantivyIndex {
    pub fn new(inner: TantivyIndex) -> Self {
        Self {
            inner: RwLock::new(Arc::new(inner)),
        }
    }

    pub fn get(&self) -> Arc<TantivyIndex> {
        Arc::clone(&self.inner.read())
    }

    /// Swap in a fresh index, dropping the old one. Used when a persisted
    /// index must be discarded wholesale.
    pub fn replace(&self, inner: TantivyIndex) {
        *self.inner.write() = Arc::new(inner);
    }
}

fn terms_in_doc(segment: &SegmentReader, field: Field, terms: &[&str], doc: DocId) -> Vec<String> {
    let Ok(inverted) = segment.inverted_index(field) else {
        return Vec::new();
    };
    terms
        .iter()
        .filter(|t| {
            let term = Term::from_field_text(field, t);
            match inverted.read_postings(&term, IndexRecordOption::Basic) {
                // `seek` must not move backwards (tantivy asserts on it).
                Ok(Some(mut postings)) => postings.doc() <= doc && postings.seek(doc) == doc,
                _ => false,
            }
        })
        .map(|t| (*t).to_string())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(id: &str, repo: &str, content: &str) -> SearchDocument {
        SearchDocument {
            id: id.to_string(),
            repo: Arc::from(repo),
            file_path: id.split("::").next().unwrap_or(id).to_string(),
            content: content.to_string(),
            doc_type: DocType::Function,
            start_line: 1,
            end_line: 10,
            term_freq: HashMap::new(),
        }
    }

    #[test]
    fn indexes_and_finds_a_document() {
        let idx = TantivyIndex::in_memory().unwrap();
        idx.add_document(doc(
            "src/a.rs::parse_config",
            "r1",
            "fn parse_config() { load_yaml() }",
        ))
        .unwrap();

        let hits = idx.search_in("parse_config", 10, None);
        assert_eq!(hits.len(), 1, "expected the indexed document back");
        assert_eq!(hits[0].0.id, "src/a.rs::parse_config");
    }

    #[test]
    fn search_is_visible_without_an_explicit_commit() {
        // The index this replaces was a plain map: documents were searchable
        // immediately. Callers must not have to learn about commits.
        let idx = TantivyIndex::in_memory().unwrap();
        idx.add_document(doc("a.rs::alpha", "r", "alpha beta"))
            .unwrap();
        assert_eq!(idx.search_in("alpha", 5, None).len(), 1);
    }

    #[test]
    fn repo_filter_applies_before_top_k() {
        // Asking for one hit in `wanted` must return the document from
        // `wanted`, not whatever wins globally.
        let idx = TantivyIndex::in_memory().unwrap();
        for i in 0..20 {
            idx.add_document(doc(
                &format!("noise{}.rs::f", i),
                "noise",
                "target target target",
            ))
            .unwrap();
        }
        idx.add_document(doc("w.rs::f", "wanted", "target"))
            .unwrap();

        let hits = idx.search_in("target", 1, Some("wanted"));
        assert_eq!(hits.len(), 1);
        assert_eq!(&*hits[0].0.repo, "wanted");
    }

    #[test]
    fn hits_report_only_the_query_terms_their_document_contains() {
        let idx = TantivyIndex::in_memory().unwrap();
        idx.add_document(doc("a.rs::f", "r", "shutdown waits for handlers"))
            .unwrap();
        idx.add_document(doc("b.rs::g", "r", "handlers registry"))
            .unwrap();
        // Indexed last, so its posting list starts after both documents above.
        idx.add_document(doc("c.rs::h", "r", "jira client"))
            .unwrap();

        let hits = idx.search_in("shutdown handlers jira", 5, None);
        let matched = |id: &str| {
            let mut terms = hits.iter().find(|h| h.0.id == id).unwrap().2.clone();
            terms.sort();
            terms
        };
        assert_eq!(matched("a.rs::f"), ["handlers", "shutdown"]);
        assert_eq!(matched("b.rs::g"), ["handlers"]);
        assert_eq!(matched("c.rs::h"), ["jira"]);
    }

    #[test]
    fn name_matches_outrank_body_mentions() {
        let idx = TantivyIndex::in_memory().unwrap();
        idx.add_document(doc("a.rs::unrelated", "r", "we call validate_input here"))
            .unwrap();
        idx.add_document(doc(
            "b.rs::validate_input",
            "r",
            "fn body does other things",
        ))
        .unwrap();

        let hits = idx.search_in("validate_input", 5, None);
        assert_eq!(hits[0].0.id, "b.rs::validate_input");
    }

    #[test]
    fn content_round_trips_only_when_the_caller_asked() {
        let idx = TantivyIndex::in_memory().unwrap();
        idx.add_document_retaining_text(doc("kept.rs::f", "r", "keep me"))
            .unwrap();
        idx.add_document(doc("lean.rs::g", "r", "keep me")).unwrap();

        let hits = idx.search_in("keep", 10, None);
        let kept = hits.iter().find(|h| h.0.id.starts_with("kept")).unwrap();
        let lean = hits.iter().find(|h| h.0.id.starts_with("lean")).unwrap();

        assert_eq!(kept.0.content, "keep me");
        assert!(
            lean.0.content.is_empty(),
            "a document indexed without retention must not carry text back"
        );
    }

    #[test]
    fn stats_reflect_indexed_documents() {
        let idx = TantivyIndex::in_memory().unwrap();
        idx.add_document(doc("a.rs::f", "r", "alpha beta gamma"))
            .unwrap();
        idx.add_document(doc("b.rs::g", "r", "delta")).unwrap();

        assert_eq!(idx.num_docs(), 2);
        assert!(idx.num_terms() >= 4);
        assert!(idx.avg_doc_len() > 0.0);

        let counts = idx.doc_type_counts();
        assert_eq!(counts.get(&DocType::Function), Some(&2));
    }

    #[test]
    fn clear_empties_the_index() {
        let idx = TantivyIndex::in_memory().unwrap();
        idx.add_document(doc("a.rs::f", "r", "alpha")).unwrap();
        idx.clear().unwrap();
        assert_eq!(idx.num_docs(), 0);
        assert!(idx.search_in("alpha", 5, None).is_empty());
    }

    #[test]
    fn delete_repo_removes_only_that_repo() {
        let idx = TantivyIndex::in_memory().unwrap();
        idx.add_document(doc("a.rs::f", "keep", "shared")).unwrap();
        idx.add_document(doc("b.rs::g", "drop", "shared")).unwrap();
        idx.delete_repo("drop").unwrap();

        let hits = idx.search_in("shared", 10, None);
        assert_eq!(hits.len(), 1);
        assert_eq!(&*hits[0].0.repo, "keep");
    }

    #[test]
    fn term_freq_documents_are_searchable() {
        // The normalized-document path supplies counts, not text.
        let mut tf = HashMap::new();
        tf.insert("inventory".to_string(), 3);
        tf.insert("warehouse".to_string(), 1);

        let idx = TantivyIndex::in_memory().unwrap();
        idx.add_document(SearchDocument {
            id: "doc1".into(),
            repo: Arc::from("r"),
            file_path: "doc1".into(),
            content: String::new(),
            doc_type: DocType::Other,
            start_line: 1,
            end_line: 1,
            term_freq: tf,
        })
        .unwrap();

        assert_eq!(idx.search_in("inventory", 5, None).len(), 1);
    }

    #[cfg(feature = "native")]
    #[test]
    fn persisted_index_survives_reopening() {
        // The whole point: a restart must not re-tokenize the corpus.
        let dir = tempfile::tempdir().unwrap();
        {
            let idx = TantivyIndex::open_or_create(dir.path()).unwrap();
            idx.add_document(doc("a.rs::persisted", "r", "durable content"))
                .unwrap();
            idx.commit().unwrap();
        }

        let reopened = TantivyIndex::open_or_create(dir.path()).unwrap();
        assert_eq!(reopened.num_docs(), 1);
        assert_eq!(
            reopened.search_in("durable", 5, None)[0].0.id,
            "a.rs::persisted"
        );
    }
}

#[cfg(test)]
mod delete_ordering_tests {
    use super::*;

    fn doc(id: &str, repo: &str, content: &str) -> SearchDocument {
        SearchDocument {
            id: id.to_string(),
            repo: Arc::from(repo),
            file_path: id.split("::").next().unwrap_or(id).to_string(),
            content: content.to_string(),
            doc_type: DocType::Function,
            start_line: 1,
            end_line: 10,
            term_freq: HashMap::new(),
        }
    }

    #[test]
    fn an_index_from_another_tokenizer_version_is_rebuilt_empty() {
        let dir = tempfile::tempdir().unwrap();
        {
            let idx = TantivyIndex::open_or_create(dir.path()).unwrap();
            idx.add_document(doc("a.rs::f", "r", "durable")).unwrap();
            idx.commit().unwrap();
        }
        std::fs::write(dir.path().join(TOKENIZER_MARKER), "1").unwrap();

        let reopened = TantivyIndex::open_or_create(dir.path()).unwrap();
        assert_eq!(reopened.num_docs(), 0);
        assert_eq!(
            std::fs::read_to_string(dir.path().join(TOKENIZER_MARKER)).unwrap(),
            TOKENIZER_VERSION.to_string()
        );
    }

    #[test]
    fn a_delete_does_not_swallow_documents_added_after_it() {
        // The indexing walk clears a repository before re-adding it. If the
        // delete also removed documents added later in the same uncommitted
        // batch, every freshly indexed repository would come back empty.
        let idx = TantivyIndex::in_memory().unwrap();
        idx.delete_repo("beta").unwrap();
        idx.add_document(doc("b.rs::compute_beta", "beta", "compute beta total"))
            .unwrap();

        let hits = idx.search_in("compute total", 10, None);
        assert_eq!(
            hits.len(),
            1,
            "a document added after the delete must survive it"
        );
    }

    #[test]
    fn clearing_one_repo_does_not_disturb_another_already_indexed() {
        // Repositories are walked one after another, and each clears itself
        // first. Alpha is already in the index when beta's clear runs.
        let idx = TantivyIndex::in_memory().unwrap();
        idx.delete_repo("alpha").unwrap();
        idx.add_document(doc("a.rs::compute_alpha", "alpha", "compute alpha total"))
            .unwrap();

        idx.delete_repo("beta").unwrap();
        idx.add_document(doc("b.rs::compute_beta", "beta", "compute beta total"))
            .unwrap();

        let hits = idx.search_in("compute total", 10, None);
        assert_eq!(
            hits.len(),
            2,
            "both repositories must be searchable, got {hits:?}"
        );
    }

    #[test]
    fn a_reader_never_overtakes_an_in_flight_commit() {
        // Regression. `dirty` is cleared when a commit *starts*, so a reader
        // that checked the flag without taking the refresh lock could sail
        // past a commit whose `reader.reload` had not happened yet, and answer
        // from a snapshot older than both. It showed up as an indexed
        // repository missing from an unscoped search, only under load.
        use std::sync::Arc;

        let idx = Arc::new(TantivyIndex::in_memory().unwrap());
        let mut seen_before = 0usize;

        for round in 0..40 {
            idx.add_document(doc(&format!("r{round}.rs::t"), "repo", "unique_marker"))
                .unwrap();

            let committer = {
                let idx = Arc::clone(&idx);
                std::thread::spawn(move || idx.commit().unwrap())
            };
            // Races the commit on purpose.
            let during = idx.search_in("unique_marker", 100, None).len();
            committer.join().unwrap();

            assert!(
                during >= seen_before,
                "visibility went backwards: {during} < {seen_before}"
            );
            seen_before = during;

            let after = idx.search_in("unique_marker", 100, None).len();
            assert_eq!(
                after,
                round + 1,
                "every committed document must be visible once the commit is done"
            );
        }
    }

    #[test]
    fn a_commit_between_the_two_repos_changes_nothing() {
        // A search arriving mid-walk commits the index. That must not change
        // what the walk ends up producing.
        let idx = TantivyIndex::in_memory().unwrap();
        idx.delete_repo("alpha").unwrap();
        idx.add_document(doc("a.rs::compute_alpha", "alpha", "compute alpha total"))
            .unwrap();

        // Stand-in for a query landing between the two repositories.
        let _ = idx.search_in("compute total", 10, None);

        idx.delete_repo("beta").unwrap();
        idx.add_document(doc("b.rs::compute_beta", "beta", "compute beta total"))
            .unwrap();

        let hits = idx.search_in("compute total", 10, None);
        assert_eq!(
            hits.len(),
            2,
            "both repositories must be searchable, got {hits:?}"
        );
    }
}
