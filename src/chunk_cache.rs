//! Per-file cache of chunk boundaries and token sets backing `search_chunks`.
//!
//! `search_chunks` scores every AST chunk of a repository by how many query
//! tokens it contains. Re-chunking and re-tokenizing the whole repository per
//! query cost ~40 s on a 2.3k-module 1C dump, so the boundaries and the token
//! sets are kept here, keyed by file and revalidated against size + mtime.
//!
//! No chunk text is stored — `file_cache` deliberately keeps paths only, and
//! this cache follows suit: tokens are interned to `u32`, and the few chunks
//! that make it into a result are re-chunked from disk for display. The cache
//! is filled lazily by the first `search_chunks` call, so servers that never
//! use the tool pay nothing.

use crate::chunking::{AstChunker, ChunkType};
use crate::search::tokenize_code;
use crate::FastMap;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::SystemTime;

/// What `search_chunks` needs to score a chunk without its text.
#[derive(Debug)]
pub struct ChunkSig {
    /// Position in `AstChunker::chunk_file` output, to re-materialize the chunk.
    pub index: usize,
    pub start_line: usize,
    pub end_line: usize,
    pub chunk_type: ChunkType,
    /// Enclosing symbol, without the splitter's " (continued)" suffix, so a
    /// chunk can be matched to the symbol's search card.
    pub symbol: Option<Box<str>>,
    /// Sorted, deduplicated interned token ids.
    tokens: Box<[u32]>,
}

impl ChunkSig {
    /// How many of the (sorted, deduplicated) query token ids the chunk contains.
    pub fn overlap(&self, query_ids: &[u32]) -> usize {
        query_ids
            .iter()
            .filter(|id| self.tokens.binary_search(id).is_ok())
            .count()
    }
}

struct Entry {
    len: u64,
    modified: SystemTime,
    chunks: Arc<[ChunkSig]>,
}

#[derive(Default)]
pub struct ChunkTokenCache {
    files: FastMap<PathBuf, Entry>,
    terms: FastMap<String, u32>,
    next_term: AtomicU32,
}

impl ChunkTokenCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Chunk signatures for `path`, re-chunking only when the file's size or
    /// mtime changed since it was cached. `None` when the file is unreadable
    /// or the chunker panicked on it.
    pub fn chunks_for(&self, path: &Path, chunker: &AstChunker) -> Option<Arc<[ChunkSig]>> {
        let meta = std::fs::metadata(path).ok()?;
        // Without an mtime there is nothing to validate against: always re-chunk.
        let modified = meta.modified().ok();
        if let (Some(modified), Some(entry)) = (modified, self.files.get(path)) {
            if entry.len == meta.len() && entry.modified == modified {
                return Some(Arc::clone(&entry.chunks));
            }
        }

        let content = std::fs::read_to_string(path).ok()?;
        let path_str = path.to_string_lossy();
        let chunks = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            chunker.chunk_file(&content, &path_str)
        })) {
            Ok(chunks) => chunks,
            Err(_) => {
                tracing::warn!("Skipping file due to chunking error: {}", path_str);
                return None;
            }
        };

        let sigs: Arc<[ChunkSig]> = chunks
            .iter()
            .enumerate()
            .map(|(index, chunk)| ChunkSig {
                index,
                start_line: chunk.start_line,
                end_line: chunk.end_line,
                chunk_type: chunk.chunk_type,
                symbol: chunk.symbol_context.as_ref().map(|ctx| {
                    ctx.name
                        .strip_suffix(" (continued)")
                        .unwrap_or(&ctx.name)
                        .into()
                }),
                tokens: self.intern_set(&chunk.content),
            })
            .collect();

        if let Some(modified) = modified {
            self.files.insert(
                path.to_path_buf(),
                Entry {
                    len: meta.len(),
                    modified,
                    chunks: Arc::clone(&sigs),
                },
            );
        }
        Some(sigs)
    }

    /// Ids of the query tokens any cached chunk has ever contained, sorted and
    /// deduplicated. A token no chunk has seen cannot match, so it is dropped
    /// here — callers still divide by the full distinct-token count.
    pub fn known_ids(&self, query_tokens: &HashSet<String>) -> Vec<u32> {
        let mut ids: Vec<u32> = query_tokens
            .iter()
            .filter_map(|t| self.terms.get(t).map(|id| *id))
            .collect();
        ids.sort_unstable();
        ids.dedup();
        ids
    }

    pub fn remove(&self, path: &Path) {
        self.files.remove(path);
    }

    pub fn clear(&self) {
        self.files.clear();
        self.terms.clear();
        self.next_term.store(0, Ordering::Relaxed);
    }

    fn intern_set(&self, text: &str) -> Box<[u32]> {
        let mut ids: Vec<u32> = tokenize_code(text)
            .into_iter()
            .map(|t| {
                *self
                    .terms
                    .entry(t)
                    .or_insert_with(|| self.next_term.fetch_add(1, Ordering::Relaxed))
            })
            .collect();
        ids.sort_unstable();
        ids.dedup();
        ids.into_boxed_slice()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write(path: &Path, body: &str) {
        let mut f = std::fs::File::create(path).unwrap();
        f.write_all(body.as_bytes()).unwrap();
    }

    #[test]
    fn scores_from_cache_and_revalidates_on_change() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.py");
        write(
            &path,
            "def fetch_data(url):\n    return urlopen(url).read()\n",
        );

        let cache = ChunkTokenCache::new();
        let chunker = AstChunker::new();
        let first = cache.chunks_for(&path, &chunker).unwrap();
        let query: HashSet<String> = tokenize_code("fetch urlopen").into_iter().collect();
        let ids = cache.known_ids(&query);
        assert!(first.iter().any(|c| c.overlap(&ids) == 2));

        // Unchanged file: the very same allocation comes back.
        let again = cache.chunks_for(&path, &chunker).unwrap();
        assert!(Arc::ptr_eq(&first, &again));

        // Different size: re-chunked, old tokens no longer match.
        write(&path, "def store_items(items):\n    return len(items)\n");
        let changed = cache.chunks_for(&path, &chunker).unwrap();
        assert!(!Arc::ptr_eq(&first, &changed));
        let ids = cache.known_ids(&query);
        assert!(changed.iter().all(|c| c.overlap(&ids) == 0));
    }

    #[test]
    fn unseen_query_tokens_are_dropped() {
        let cache = ChunkTokenCache::new();
        let query: HashSet<String> = ["never".to_string()].into_iter().collect();
        assert!(cache.known_ids(&query).is_empty());
    }
}
