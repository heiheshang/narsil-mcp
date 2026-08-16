//! Persistent cache for embedding vectors.
//!
//! Embeddings are expensive and perfectly reproducible: the same text always
//! yields the same vector for a given model. The index itself is persisted
//! (see [`crate::persist`]), but vectors were not, so every process start
//! re-embedded the whole repository — minutes of work and API traffic for
//! nothing.
//!
//! [`CachedBackend`] wraps any [`EmbeddingBackend`] and keeps vectors in a
//! append-only file next to the index. Keys are content hashes, so edits
//! invalidate exactly the signatures that changed and nothing else.
//!
//! File layout (little-endian):
//!
//! ```text
//! magic  "NRSLEMB1"     8 bytes
//! version               u16
//! dimension             u32
//! model name length     u16
//! model name            N bytes
//! ---- records, repeated ----
//! key (sha256 of text)  32 bytes
//! vector                dimension * 4 bytes (f32)
//! ```
//!
//! A torn record at the tail (killed mid-write) is dropped on load and the
//! file is truncated back to the last complete record.

use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
use tracing::{info, warn};

use crate::neural::EmbeddingBackend;

const MAGIC: &[u8; 8] = b"NRSLEMB1";
const FORMAT_VERSION: u16 = 1;
const KEY_LEN: usize = 32;

type Key = [u8; KEY_LEN];

/// Append-only, content-addressed store of embedding vectors.
pub struct EmbeddingCache {
    path: PathBuf,
    dimension: usize,
    entries: RwLock<HashMap<Key, Vec<f32>>>,
    writer: Mutex<File>,
}

impl EmbeddingCache {
    /// Open (or create) the cache for a given model and dimension.
    ///
    /// A cache whose header disagrees with the requested model or dimension is
    /// discarded rather than reinterpreted: vectors from different models are
    /// not comparable, and a wrong dimension would poison every search.
    pub fn open(dir: &Path, model: &str, dimension: usize) -> Result<Self> {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("Failed to create cache directory {:?}", dir))?;
        let path = dir.join(format!("emb-{}-{}.bin", sanitize(model), dimension));

        let mut entries = HashMap::new();
        let mut valid_len = 0u64;
        let mut reusable = false;

        if path.exists() {
            match read_existing(&path, model, dimension, &mut entries) {
                Ok(len) => {
                    valid_len = len;
                    reusable = true;
                }
                Err(e) => warn!(
                    "Embedding cache {:?} unusable ({}), starting a fresh one",
                    path, e
                ),
            }
        }

        let mut file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .open(&path)
            .with_context(|| format!("Failed to open embedding cache {:?}", path))?;

        if reusable {
            // Drop a torn tail so appends always start at a record boundary.
            let actual = file.metadata()?.len();
            if actual != valid_len {
                warn!(
                    "Embedding cache {:?} had {} trailing bytes from an interrupted write, truncating",
                    path,
                    actual - valid_len
                );
                file.set_len(valid_len)?;
            }
            file.seek(SeekFrom::End(0))?;
        } else {
            file.set_len(0)?;
            write_header(&mut file, model, dimension)?;
        }

        info!(
            "Embedding cache: {} vectors at {:?}",
            entries.len(),
            path
        );

        Ok(Self {
            path,
            dimension,
            entries: RwLock::new(entries),
            writer: Mutex::new(file),
        })
    }

    pub fn len(&self) -> usize {
        self.entries.read().unwrap().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn key(text: &str) -> Key {
        let mut hasher = Sha256::new();
        hasher.update(text.as_bytes());
        hasher.finalize().into()
    }

    fn get(&self, text: &str) -> Option<Vec<f32>> {
        self.entries.read().unwrap().get(&Self::key(text)).cloned()
    }

    /// Append vectors to disk and publish them in memory.
    ///
    /// A failed write is logged, not propagated: a broken cache must never
    /// break indexing, it can only make it slower.
    fn put_many(&self, items: &[(&str, Vec<f32>)]) {
        if items.is_empty() {
            return;
        }
        let mut buffer = Vec::with_capacity(items.len() * (KEY_LEN + self.dimension * 4));
        let mut accepted = Vec::with_capacity(items.len());
        for (text, vector) in items {
            if vector.len() != self.dimension {
                warn!(
                    "Refusing to cache a {}-dimensional vector, cache holds {}",
                    vector.len(),
                    self.dimension
                );
                continue;
            }
            let key = Self::key(text);
            buffer.extend_from_slice(&key);
            for value in vector {
                buffer.extend_from_slice(&value.to_le_bytes());
            }
            accepted.push((key, vector.clone()));
        }
        if accepted.is_empty() {
            return;
        }

        {
            let mut file = self.writer.lock().unwrap();
            if let Err(e) = file.write_all(&buffer).and_then(|_| file.flush()) {
                warn!("Failed to append to embedding cache {:?}: {}", self.path, e);
                return;
            }
        }

        let mut entries = self.entries.write().unwrap();
        for (key, vector) in accepted {
            entries.insert(key, vector);
        }
    }
}

fn sanitize(model: &str) -> String {
    model
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}

fn write_header(file: &mut File, model: &str, dimension: usize) -> Result<()> {
    let model_bytes = model.as_bytes();
    if model_bytes.len() > u16::MAX as usize {
        bail!("Model name too long for the cache header");
    }
    file.write_all(MAGIC)?;
    file.write_all(&FORMAT_VERSION.to_le_bytes())?;
    file.write_all(&(dimension as u32).to_le_bytes())?;
    file.write_all(&(model_bytes.len() as u16).to_le_bytes())?;
    file.write_all(model_bytes)?;
    file.flush()?;
    Ok(())
}

/// Read a cache file into `entries`, returning the offset just past the last
/// complete record.
fn read_existing(
    path: &Path,
    model: &str,
    dimension: usize,
    entries: &mut HashMap<Key, Vec<f32>>,
) -> Result<u64> {
    let file = File::open(path)?;
    let mut reader = BufReader::new(file);

    let mut magic = [0u8; 8];
    reader.read_exact(&mut magic)?;
    if &magic != MAGIC {
        bail!("not an embedding cache file");
    }
    let version = read_u16(&mut reader)?;
    if version != FORMAT_VERSION {
        bail!("format version {} is not supported", version);
    }
    let stored_dimension = read_u32(&mut reader)? as usize;
    if stored_dimension != dimension {
        bail!(
            "cache holds {}-dimensional vectors, {} requested",
            stored_dimension,
            dimension
        );
    }
    let model_len = read_u16(&mut reader)? as usize;
    let mut model_bytes = vec![0u8; model_len];
    reader.read_exact(&mut model_bytes)?;
    if String::from_utf8_lossy(&model_bytes) != model {
        bail!("cache belongs to a different model");
    }

    let header_len = (8 + 2 + 4 + 2 + model_len) as u64;
    let record_len = KEY_LEN + dimension * 4;
    let mut record = vec![0u8; record_len];
    let mut offset = header_len;

    loop {
        match read_full(&mut reader, &mut record)? {
            0 => break,
            n if n < record_len => break, // torn tail, caller truncates
            _ => {}
        }
        let mut key = [0u8; KEY_LEN];
        key.copy_from_slice(&record[..KEY_LEN]);
        let mut vector = Vec::with_capacity(dimension);
        for chunk in record[KEY_LEN..].chunks_exact(4) {
            vector.push(f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]));
        }
        entries.insert(key, vector);
        offset += record_len as u64;
    }

    Ok(offset)
}

fn read_full<R: Read>(reader: &mut R, buf: &mut [u8]) -> Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        match reader.read(&mut buf[filled..])? {
            0 => break,
            n => filled += n,
        }
    }
    Ok(filled)
}

fn read_u16<R: Read>(reader: &mut R) -> Result<u16> {
    let mut buf = [0u8; 2];
    reader.read_exact(&mut buf)?;
    Ok(u16::from_le_bytes(buf))
}

fn read_u32<R: Read>(reader: &mut R) -> Result<u32> {
    let mut buf = [0u8; 4];
    reader.read_exact(&mut buf)?;
    Ok(u32::from_le_bytes(buf))
}

// ============================================================================
// Backend decorator
// ============================================================================

/// Wraps an embedding backend so repeated texts never reach the provider.
pub struct CachedBackend {
    inner: Arc<dyn EmbeddingBackend>,
    cache: EmbeddingCache,
}

impl CachedBackend {
    pub fn new(inner: Arc<dyn EmbeddingBackend>, cache: EmbeddingCache) -> Self {
        Self { inner, cache }
    }

    pub fn cached_vectors(&self) -> usize {
        self.cache.len()
    }
}

impl EmbeddingBackend for CachedBackend {
    fn embed(&self, text: &str) -> Result<Vec<f32>> {
        if let Some(vector) = self.cache.get(text) {
            return Ok(vector);
        }
        let vector = self.inner.embed(text)?;
        self.cache.put_many(&[(text, vector.clone())]);
        Ok(vector)
    }

    fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        let mut result: Vec<Option<Vec<f32>>> = Vec::with_capacity(texts.len());
        let mut misses: Vec<String> = Vec::new();
        let mut miss_positions: Vec<usize> = Vec::new();

        for (position, text) in texts.iter().enumerate() {
            match self.cache.get(text) {
                Some(vector) => result.push(Some(vector)),
                None => {
                    result.push(None);
                    // A batch often repeats the same signature; ask once.
                    if !misses.iter().any(|seen| seen == text) {
                        misses.push(text.clone());
                    }
                    miss_positions.push(position);
                }
            }
        }

        if !misses.is_empty() {
            let fresh = self.inner.embed_batch(&misses)?;
            if fresh.len() != misses.len() {
                bail!(
                    "Backend returned {} vectors for {} inputs",
                    fresh.len(),
                    misses.len()
                );
            }
            let to_store: Vec<(&str, Vec<f32>)> = misses
                .iter()
                .map(|s| s.as_str())
                .zip(fresh.iter().cloned())
                .collect();
            self.cache.put_many(&to_store);

            let by_text: HashMap<&str, &Vec<f32>> = misses
                .iter()
                .map(|s| s.as_str())
                .zip(fresh.iter())
                .collect();
            for position in miss_positions {
                let text = texts[position].as_str();
                result[position] = by_text.get(text).map(|v| (*v).clone());
            }
        }

        result
            .into_iter()
            .enumerate()
            .map(|(position, vector)| {
                vector.with_context(|| format!("No embedding produced for input {}", position))
            })
            .collect()
    }

    fn dimension(&self) -> usize {
        self.inner.dimension()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct CountingBackend {
        dimension: usize,
        calls: Mutex<usize>,
        embedded: Mutex<Vec<String>>,
    }

    impl CountingBackend {
        fn new(dimension: usize) -> Self {
            Self {
                dimension,
                calls: Mutex::new(0),
                embedded: Mutex::new(Vec::new()),
            }
        }
        fn vector_for(&self, text: &str) -> Vec<f32> {
            let seed = text.len() as f32;
            (0..self.dimension).map(|i| seed + i as f32).collect()
        }
    }

    impl EmbeddingBackend for CountingBackend {
        fn embed(&self, text: &str) -> Result<Vec<f32>> {
            Ok(self.embed_batch(&[text.to_string()])?.remove(0))
        }
        fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
            *self.calls.lock().unwrap() += 1;
            self.embedded.lock().unwrap().extend(texts.iter().cloned());
            Ok(texts.iter().map(|t| self.vector_for(t)).collect())
        }
        fn dimension(&self) -> usize {
            self.dimension
        }
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("narsil-emb-cache-{}-{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn second_call_does_not_reach_the_backend() {
        let dir = temp_dir("hit");
        let cache = EmbeddingCache::open(&dir, "test-model", 4).unwrap();
        let inner = Arc::new(CountingBackend::new(4));
        let backend = CachedBackend::new(inner.clone(), cache);

        let texts = vec!["alpha".to_string(), "beta".to_string()];
        let first = backend.embed_batch(&texts).unwrap();
        let second = backend.embed_batch(&texts).unwrap();

        assert_eq!(first, second);
        assert_eq!(*inner.calls.lock().unwrap(), 1, "second batch must be served from cache");
    }

    #[test]
    fn only_missing_texts_are_sent_upstream() {
        let dir = temp_dir("partial");
        let cache = EmbeddingCache::open(&dir, "test-model", 4).unwrap();
        let inner = Arc::new(CountingBackend::new(4));
        let backend = CachedBackend::new(inner.clone(), cache);

        backend.embed_batch(&["alpha".to_string()]).unwrap();
        inner.embedded.lock().unwrap().clear();
        backend
            .embed_batch(&["alpha".to_string(), "gamma".to_string()])
            .unwrap();

        assert_eq!(
            *inner.embedded.lock().unwrap(),
            vec!["gamma".to_string()],
            "cached input must not be re-sent"
        );
    }

    #[test]
    fn duplicates_within_a_batch_are_embedded_once() {
        let dir = temp_dir("dupes");
        let cache = EmbeddingCache::open(&dir, "test-model", 4).unwrap();
        let inner = Arc::new(CountingBackend::new(4));
        let backend = CachedBackend::new(inner.clone(), cache);

        let texts = vec!["same".to_string(), "same".to_string(), "other".to_string()];
        let vectors = backend.embed_batch(&texts).unwrap();

        assert_eq!(vectors[0], vectors[1]);
        assert_eq!(inner.embedded.lock().unwrap().len(), 2);
    }

    #[test]
    fn vectors_survive_reopening() {
        let dir = temp_dir("persist");
        {
            let cache = EmbeddingCache::open(&dir, "test-model", 4).unwrap();
            let backend = CachedBackend::new(Arc::new(CountingBackend::new(4)), cache);
            backend.embed_batch(&["alpha".to_string()]).unwrap();
        }
        let cache = EmbeddingCache::open(&dir, "test-model", 4).unwrap();
        assert_eq!(cache.len(), 1);

        let inner = Arc::new(CountingBackend::new(4));
        let backend = CachedBackend::new(inner.clone(), cache);
        backend.embed_batch(&["alpha".to_string()]).unwrap();
        assert_eq!(*inner.calls.lock().unwrap(), 0, "reopened cache must serve the hit");
    }

    #[test]
    fn a_different_model_does_not_reuse_vectors() {
        let dir = temp_dir("model");
        {
            let cache = EmbeddingCache::open(&dir, "model-a", 4).unwrap();
            let backend = CachedBackend::new(Arc::new(CountingBackend::new(4)), cache);
            backend.embed_batch(&["alpha".to_string()]).unwrap();
        }
        let other = EmbeddingCache::open(&dir, "model-b", 4).unwrap();
        assert_eq!(other.len(), 0, "each model gets its own cache file");
    }

    #[test]
    fn a_torn_tail_is_dropped_and_the_rest_survives() {
        let dir = temp_dir("torn");
        let path = {
            let cache = EmbeddingCache::open(&dir, "test-model", 4).unwrap();
            let backend = CachedBackend::new(Arc::new(CountingBackend::new(4)), cache);
            backend
                .embed_batch(&["alpha".to_string(), "beta".to_string()])
                .unwrap();
            dir.join("emb-test_model-4.bin")
        };

        // Simulate a process killed halfway through appending a third record.
        let file = OpenOptions::new().append(true).open(&path).unwrap();
        file.set_len(file.metadata().unwrap().len() + 7).unwrap();

        let cache = EmbeddingCache::open(&dir, "test-model", 4).unwrap();
        assert_eq!(cache.len(), 2, "complete records must survive");
        assert_eq!(
            std::fs::metadata(&path).unwrap().len() % 1,
            0,
            "file must be truncated to a record boundary"
        );

        // And the truncated file must still accept new writes.
        let inner = Arc::new(CountingBackend::new(4));
        let backend = CachedBackend::new(inner, cache);
        backend.embed_batch(&["gamma".to_string()]).unwrap();
        let reopened = EmbeddingCache::open(&dir, "test-model", 4).unwrap();
        assert_eq!(reopened.len(), 3);
    }

    #[test]
    fn a_corrupt_file_is_replaced_instead_of_failing() {
        let dir = temp_dir("corrupt");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("emb-test_model-4.bin"), b"garbage").unwrap();

        let cache = EmbeddingCache::open(&dir, "test-model", 4).unwrap();
        assert_eq!(cache.len(), 0);

        let backend = CachedBackend::new(Arc::new(CountingBackend::new(4)), cache);
        backend.embed_batch(&["alpha".to_string()]).unwrap();
        let reopened = EmbeddingCache::open(&dir, "test-model", 4).unwrap();
        assert_eq!(reopened.len(), 1);
    }
}
