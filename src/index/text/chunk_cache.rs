//! Re-chunking the whole repository on every `search_chunks` call is too slow,
//! so chunk boundaries and interned token ids are kept per file. No text: the
//! chunks that reach a result are re-chunked from disk.

use crate::chunking::{AstChunker, ChunkType};
use crate::search::tokenize_code;
use crate::FastMap;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::SystemTime;

#[derive(Debug)]
pub struct ChunkSig {
    /// Position in `AstChunker::chunk_file` output.
    pub index: usize,
    pub start_line: usize,
    pub end_line: usize,
    pub chunk_type: ChunkType,
    /// Without the splitter's " (continued)" suffix, to match the symbol's search card.
    pub symbol: Option<Box<str>>,
    /// Sorted and deduplicated: `overlap` binary-searches it.
    tokens: Box<[u32]>,
}

impl ChunkSig {
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

    /// `None` when the file is unreadable or the chunker panicked on it.
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

    /// Drops tokens no chunk contains; callers must still divide by the full
    /// distinct-token count.
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

    /// Drops every file under `root`; returns how many.
    pub fn remove_under(&self, root: &Path) -> usize {
        let before = self.files.len();
        self.files.retain(|path, _| !path.starts_with(root));
        before - self.files.len()
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

        let again = cache.chunks_for(&path, &chunker).unwrap();
        assert!(Arc::ptr_eq(&first, &again));

        write(&path, "def store_items(items):\n    return len(items)\n");
        let changed = cache.chunks_for(&path, &chunker).unwrap();
        assert!(!Arc::ptr_eq(&first, &changed));
        let ids = cache.known_ids(&query);
        assert!(changed.iter().all(|c| c.overlap(&ids) == 0));
    }

    #[test]
    fn remove_under_drops_only_that_tree() {
        let dir = tempfile::tempdir().unwrap();
        let wt = dir.path().join("wt");
        std::fs::create_dir(&wt).unwrap();
        let kept = dir.path().join("a.py");
        let dropped = wt.join("b.py");
        write(&kept, "def a():\n    return 1\n");
        write(&dropped, "def b():\n    return 2\n");

        let cache = ChunkTokenCache::new();
        let chunker = AstChunker::new();
        cache.chunks_for(&kept, &chunker).unwrap();
        cache.chunks_for(&dropped, &chunker).unwrap();

        assert_eq!(cache.remove_under(&wt), 1);
        assert!(cache.files.contains_key(&kept));
        assert!(!cache.files.contains_key(&dropped));
    }

    #[test]
    fn unseen_query_tokens_are_dropped() {
        let cache = ChunkTokenCache::new();
        let query: HashSet<String> = ["never".to_string()].into_iter().collect();
        assert!(cache.known_ids(&query).is_empty());
    }
}
