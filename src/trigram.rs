//! Trigram index for substring search.
//!
//! `search_code` is a substring matcher — it answers `foo_bar(baz`, which no
//! token index can. It did so by scanning every cached file of every repository
//! and calling `to_lowercase()` on every line, allocating a `String` per line
//! per query. On this repository that is ~125 000 allocations for one call; on
//! a monorepo it is millions.
//!
//! This is the standard fix, the one Zoekt and Google Code Search use: index
//! every 3-character window of every file, intersect the postings for the
//! needle's own trigrams, and scan only the handful of files that could
//! possibly match. The scan still decides — trigrams over-approximate, they
//! never reject a real hit — so results are identical, just reached without
//! reading the corpus.
//!
//! The postings live in their own tantivy index rather than a `HashMap`, which
//! keeps them mmap'd on disk instead of on the heap. A trigram index is large
//! by nature; on a machine already fighting the OOM killer it has no business
//! being resident.
//!
//! Needles shorter than three characters have no trigrams. [`candidates`]
//! returns `None` for those, which means "no opinion" — the caller falls back
//! to scanning everything, exactly as before.

use anyhow::{Context, Result};
use parking_lot::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use tantivy::collector::DocSetCollector;
use tantivy::query::{BooleanQuery, Occur, Query, TermQuery};
use tantivy::schema::{
    Field, IndexRecordOption, Schema, TextFieldIndexing, TextOptions, Value, STORED, STRING,
};
use tantivy::tokenizer::{LowerCaser, NgramTokenizer, TextAnalyzer, TokenStream};
use tantivy::{Index, IndexReader, IndexWriter, ReloadPolicy, TantivyDocument, Term};

use crate::search::scoped_doc_id;

const GRAM_TOKENIZER: &str = "trigram";
const GRAM_SIZE: usize = 3;
const WRITER_HEAP_BYTES: usize = 64 * 1024 * 1024;

/// Files above this size are not trigram-indexed.
///
/// A trigram index costs roughly one posting per byte, so a handful of huge
/// generated files can dominate it. They stay searchable: they are recorded as
/// oversized and [`TrigramIndex::candidates`] returns them for *every* query,
/// which hands them back to the scan that would have read them anyway.
///
/// Getting this wrong is a silent wrong answer rather than a slow one — a file
/// missing from the index and missing from the candidate set is a file the
/// caller never looks at.
const MAX_INDEXED_FILE_BYTES: usize = 2 * 1024 * 1024;

/// Beyond this many candidates the filter has stopped earning its keep, and the
/// caller is better off scanning the repository directly than materialising a
/// huge path set.
const MAX_CANDIDATES: usize = 50_000;

#[derive(Clone, Copy)]
struct Fields {
    repo: Field,
    repo_path: Field,
    path: Field,
    grams: Field,
    /// `"1"` on files too large to gram. Never absent-but-unindexed: a file is
    /// either searchable through its trigrams or always a candidate.
    oversized: Field,
}

fn build_schema() -> (Schema, Fields) {
    let mut b = Schema::builder();

    let indexing = TextFieldIndexing::default()
        .set_tokenizer(GRAM_TOKENIZER)
        // Presence is the whole question. Frequencies and positions would
        // multiply the index for information the verifying scan supplies.
        .set_index_option(IndexRecordOption::Basic);

    let fields = Fields {
        repo: b.add_text_field("repo", STRING),
        repo_path: b.add_text_field("repo_path", STRING),
        path: b.add_text_field("path", STRING | STORED),
        grams: b.add_text_field(
            "grams",
            TextOptions::default().set_indexing_options(indexing),
        ),
        oversized: b.add_text_field("oversized", STRING),
    };

    (b.build(), fields)
}

fn analyzer() -> TextAnalyzer {
    TextAnalyzer::builder(
        NgramTokenizer::all_ngrams(GRAM_SIZE, GRAM_SIZE)
            .expect("trigram tokenizer must be constructible"),
    )
    .filter(LowerCaser)
    .build()
}

pub struct TrigramIndex {
    writer: Mutex<IndexWriter>,
    reader: IndexReader,
    analyzer: Mutex<TextAnalyzer>,
    fields: Fields,
    dirty: AtomicBool,
    refresh: Mutex<()>,
}

impl TrigramIndex {
    pub fn in_memory() -> Result<Self> {
        let (schema, fields) = build_schema();
        Self::from_index(Index::create_in_ram(schema), fields)
    }

    /// Open (or create) a persisted trigram index rooted at `path`.
    #[cfg(feature = "native")]
    pub fn open_or_create(path: &std::path::Path) -> Result<Self> {
        use tantivy::directory::MmapDirectory;

        std::fs::create_dir_all(path)
            .with_context(|| format!("create trigram index dir {:?}", path))?;
        let (schema, fields) = build_schema();

        let open = |p: &std::path::Path| -> Result<Index> {
            let dir = MmapDirectory::open(p)
                .with_context(|| format!("open trigram index dir {:?}", p))?;
            Index::open_or_create(dir, schema.clone()).context("create trigram index")
        };

        // The index is a cache. A schema that no longer matches is a reason to
        // rebuild it, not a reason to refuse to start.
        let index = match open(path) {
            Ok(index) => index,
            Err(e) => {
                tracing::warn!(
                    "Trigram index at {:?} unusable ({}); rebuilding from scratch",
                    path,
                    e
                );
                std::fs::remove_dir_all(path).ok();
                std::fs::create_dir_all(path)
                    .with_context(|| format!("recreate trigram index dir {:?}", path))?;
                open(path)?
            }
        };

        Self::from_index(index, fields)
    }

    fn from_index(index: Index, fields: Fields) -> Result<Self> {
        index.tokenizers().register(GRAM_TOKENIZER, analyzer());

        let writer: IndexWriter = index
            .writer(WRITER_HEAP_BYTES)
            .context("create trigram writer")?;
        let reader = index
            .reader_builder()
            .reload_policy(ReloadPolicy::Manual)
            .try_into()
            .context("create trigram reader")?;

        Ok(Self {
            writer: Mutex::new(writer),
            reader,
            analyzer: Mutex::new(analyzer()),
            fields,
            dirty: AtomicBool::new(false),
            refresh: Mutex::new(()),
        })
    }

    /// Index one file's text. `rel_path` is repository-relative.
    pub fn add_file(&self, repo: &str, rel_path: &str, content: &str) {
        let f = &self.fields;
        let oversized = content.len() > MAX_INDEXED_FILE_BYTES;

        let mut td = TantivyDocument::new();
        td.add_text(f.repo, repo);
        td.add_text(f.repo_path, scoped_doc_id(repo, rel_path));
        td.add_text(f.path, rel_path);
        if oversized {
            // Recorded, not grammed: it stays a candidate for every query.
            td.add_text(f.oversized, "1");
        } else {
            td.add_text(f.grams, content);
        }

        if let Err(e) = self.writer.lock().add_document(td) {
            tracing::warn!("Failed to trigram-index {}: {}", rel_path, e);
            return;
        }
        self.dirty.store(true, Ordering::Release);
    }

    pub fn delete_file(&self, repo: &str, rel_path: &str) {
        let term = Term::from_field_text(self.fields.repo_path, &scoped_doc_id(repo, rel_path));
        self.writer.lock().delete_term(term);
        self.dirty.store(true, Ordering::Release);
    }

    pub fn delete_repo(&self, repo: &str) {
        let term = Term::from_field_text(self.fields.repo, repo);
        self.writer.lock().delete_term(term);
        self.dirty.store(true, Ordering::Release);
    }

    pub fn commit(&self) -> Result<()> {
        let _guard = self.refresh.lock();
        self.commit_locked()
    }

    fn commit_locked(&self) -> Result<()> {
        if !self.dirty.swap(false, Ordering::AcqRel) {
            return Ok(());
        }
        self.writer.lock().commit().context("trigram commit")?;
        self.reader.reload().context("trigram reader reload")?;
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
            tracing::warn!("Trigram refresh failed: {}", e);
        }
    }

    /// Number of files indexed for `repo`.
    pub fn repo_file_count(&self, repo: &str) -> usize {
        self.ensure_fresh();
        let term = Term::from_field_text(self.fields.repo, repo);
        let q = TermQuery::new(term, IndexRecordOption::Basic);
        self.reader
            .searcher()
            .search(&q, &tantivy::collector::Count)
            .unwrap_or(0)
    }

    /// Distinct trigrams of `needle`, in the same form the index holds.
    ///
    /// Generated through the very analyzer used at index time, so the two can
    /// never disagree about case folding or character boundaries.
    fn needle_grams(&self, needle: &str) -> Vec<String> {
        let mut analyzer = self.analyzer.lock();
        let mut stream = analyzer.token_stream(needle);
        let mut out = Vec::new();
        while stream.advance() {
            let t = &stream.token().text;
            // A space in the needle stands for any whitespace run in the file
            // (`search_code` collapses both sides), so a gram spanning one may
            // be spelled differently in a file that matches. Only whitespace-
            // free grams are safe to require.
            if t.chars().any(char::is_whitespace) {
                continue;
            }
            if !out.iter().any(|g| g == t) {
                out.push(t.clone());
            }
        }
        out
    }

    /// Repository-relative paths that could contain `needle`.
    ///
    /// `None` means the index cannot narrow this query and the caller should
    /// scan everything: the needle is too short to have trigrams, or the
    /// candidate set grew large enough that filtering no longer pays.
    ///
    /// The result is an over-approximation by construction — trigram presence
    /// does not imply the characters appear in order — so the caller must still
    /// verify each file.
    pub fn candidates(&self, repo: &str, needle: &str) -> Option<Vec<String>> {
        if needle.chars().count() < GRAM_SIZE {
            return None;
        }
        let grams = self.needle_grams(needle);
        if grams.is_empty() {
            return None;
        }

        self.ensure_fresh();
        let f = &self.fields;

        let mut clauses: Vec<(Occur, Box<dyn Query>)> = Vec::with_capacity(grams.len() + 1);
        clauses.push((
            Occur::Must,
            Box::new(TermQuery::new(
                Term::from_field_text(f.repo, repo),
                IndexRecordOption::Basic,
            )),
        ));
        // Every trigram of the needle must be present. This is what makes the
        // filter selective: a file missing even one cannot contain the needle.
        for g in &grams {
            clauses.push((
                Occur::Must,
                Box::new(TermQuery::new(
                    Term::from_field_text(f.grams, g),
                    IndexRecordOption::Basic,
                )),
            ));
        }

        let searcher = self.reader.searcher();
        let docs = match searcher.search(&BooleanQuery::new(clauses), &DocSetCollector) {
            Ok(docs) => docs,
            Err(e) => {
                tracing::warn!("Trigram search failed: {}", e);
                return None;
            }
        };

        // Files too large to gram cannot be ruled out by trigrams, so they
        // join every candidate set. Without this the filter would silently
        // hide them from `search_code` entirely.
        let oversized = BooleanQuery::new(vec![
            (
                Occur::Must,
                Box::new(TermQuery::new(
                    Term::from_field_text(f.repo, repo),
                    IndexRecordOption::Basic,
                )) as Box<dyn Query>,
            ),
            (
                Occur::Must,
                Box::new(TermQuery::new(
                    Term::from_field_text(f.oversized, "1"),
                    IndexRecordOption::Basic,
                )) as Box<dyn Query>,
            ),
        ]);
        let skipped = searcher
            .search(&oversized, &DocSetCollector)
            .unwrap_or_default();

        if docs.len() + skipped.len() > MAX_CANDIDATES {
            return None;
        }

        let mut paths = Vec::with_capacity(docs.len() + skipped.len());
        for addr in docs.into_iter().chain(skipped) {
            let Ok(td) = searcher.doc::<TantivyDocument>(addr) else {
                // A document we cannot read is a document we cannot rule out.
                return None;
            };
            if let Some(p) = td.get_first(f.path).and_then(|v| v.as_str()) {
                paths.push(p.to_string());
            }
        }
        paths.sort();
        paths.dedup();
        Some(paths)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn populated() -> TrigramIndex {
        let idx = TrigramIndex::in_memory().unwrap();
        idx.add_file("r", "a.rs", "fn parse_config(path: &str) {}");
        idx.add_file("r", "b.rs", "fn unrelated() { println!(\"hi\") }");
        idx.add_file("other", "c.rs", "fn parse_config() {}");
        idx
    }

    #[test]
    fn narrows_to_files_that_can_match() {
        let idx = populated();
        let c = idx.candidates("r", "parse_config").unwrap();
        assert_eq!(c, vec!["a.rs"], "only the file containing the needle");
    }

    #[test]
    fn candidates_are_scoped_to_the_repository() {
        let idx = populated();
        let c = idx.candidates("other", "parse_config").unwrap();
        assert_eq!(c, vec!["c.rs"]);
    }

    #[test]
    fn matching_is_case_insensitive() {
        // `search_code` lowercases both sides; the index must agree.
        let idx = populated();
        assert_eq!(idx.candidates("r", "PARSE_CONFIG").unwrap(), vec!["a.rs"]);
    }

    #[test]
    fn punctuation_is_searchable() {
        // The whole reason a token index cannot answer this.
        let idx = populated();
        assert_eq!(idx.candidates("r", "path: &str").unwrap(), vec!["a.rs"]);
    }

    #[test]
    fn grams_spanning_whitespace_are_not_required() {
        // `search_code` lets one space in the needle match any whitespace run,
        // so a file aligned with two spaces or a tab is still a hit.
        let idx = TrigramIndex::in_memory().unwrap();
        idx.add_file("r", "q.sql", "WHERE  status\t= 'new'");
        assert_eq!(
            idx.candidates("r", "where status = 'new'").unwrap(),
            vec!["q.sql"]
        );
        // Nothing but whitespace-spanning grams: no opinion, scan everything.
        assert!(idx.candidates("r", "a b").is_none());
    }

    #[test]
    fn short_needles_yield_no_opinion() {
        // Under three characters there are no trigrams; the caller must scan.
        let idx = populated();
        assert!(idx.candidates("r", "fn").is_none());
        assert!(idx.candidates("r", "").is_none());
    }

    #[test]
    fn a_needle_absent_everywhere_yields_no_candidates() {
        let idx = populated();
        let c = idx.candidates("r", "quux_nonexistent").unwrap();
        assert!(c.is_empty(), "empty is an answer, not an absence of one");
    }

    #[test]
    fn over_approximation_never_drops_a_real_hit() {
        // Trigram presence does not imply order, so candidates may include
        // files that do not match — but never exclude one that does.
        let idx = TrigramIndex::in_memory().unwrap();
        idx.add_file("r", "scrambled.rs", "cba_ba_abc");
        idx.add_file("r", "real.rs", "abc_bca");

        let c = idx.candidates("r", "abc").unwrap();
        assert!(c.contains(&"real.rs".to_string()));
    }

    #[test]
    fn deleted_files_stop_being_candidates() {
        let idx = populated();
        idx.delete_file("r", "a.rs");
        assert!(idx.candidates("r", "parse_config").unwrap().is_empty());
    }

    #[test]
    fn deleting_a_repo_leaves_others_alone() {
        let idx = populated();
        idx.delete_repo("r");
        assert!(idx.candidates("r", "parse_config").unwrap().is_empty());
        assert_eq!(
            idx.candidates("other", "parse_config").unwrap(),
            vec!["c.rs"]
        );
    }

    #[test]
    fn oversized_files_stay_candidates_for_every_query() {
        // A file too large to gram must not be filtered away: it is not in the
        // trigram postings, so excluding it would hide its contents from
        // `search_code` completely — a wrong answer, not a slow one.
        let idx = TrigramIndex::in_memory().unwrap();
        let huge = format!("needle_in_here{}", "x".repeat(MAX_INDEXED_FILE_BYTES));
        idx.add_file("r", "huge.rs", &huge);
        idx.add_file("r", "small.rs", "nothing of interest");

        let c = idx.candidates("r", "needle_in_here").unwrap();
        assert!(
            c.contains(&"huge.rs".to_string()),
            "oversized file must survive the filter, got {c:?}"
        );

        // Even for a needle it cannot contain — the scan decides, not us.
        let other = idx.candidates("r", "completely_absent").unwrap();
        assert_eq!(other, vec!["huge.rs"]);
    }

    #[cfg(feature = "native")]
    #[test]
    fn persisted_index_survives_reopening() {
        let dir = tempfile::tempdir().unwrap();
        {
            let idx = TrigramIndex::open_or_create(dir.path()).unwrap();
            idx.add_file("r", "a.rs", "fn parse_config() {}");
            idx.commit().unwrap();
        }
        let reopened = TrigramIndex::open_or_create(dir.path()).unwrap();
        assert_eq!(
            reopened.candidates("r", "parse_config").unwrap(),
            vec!["a.rs"]
        );
    }
}
