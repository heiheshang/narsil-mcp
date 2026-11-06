use anyhow::Result;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use super::{
    normalized_document_primary_source, render_normalized_search_content, CodeIntelEngine,
};
use crate::index::text::chunk_cache::ChunkSig;

type FileChunkSigs = Arc<[ChunkSig]>;
/// `(file index, chunk index, score)` from `score_cached_chunks`.
type ChunkHit = (usize, usize, f64);

impl CodeIntelEngine {
    pub async fn hybrid_search(
        &self,
        query: &str,
        repo: Option<&str>,
        max_results: usize,
        mode: &str,
        exclude_tests: Option<bool>,
    ) -> Result<String> {
        use crate::hybrid_search::create_hybrid_engine;
        use crate::security_rules::is_test_file;

        let exclude_tests = exclude_tests.unwrap_or(false); // Default false for search
        let target_repo = match repo {
            Some(r) if !r.is_empty() => Some(self.resolve_repo_name(r)?),
            _ => None,
        };

        // Never build an index per query here: on a large repository that costs
        // tens of seconds per call.
        let hybrid_engine =
            create_hybrid_engine(self.search_index.clone(), self.embedding_engine.clone());
        let fetch = if exclude_tests {
            max_results * 2
        } else {
            max_results
        };
        let scope = target_repo.as_deref();
        let results: Vec<_> = match mode {
            "bm25" => hybrid_engine.search_bm25_in(query, fetch, scope),
            "tfidf" => hybrid_engine.search_tfidf_in(query, fetch, scope),
            _ => {
                // Symbol cards have no bodies; without this list code that only
                // uses the query terms is not found.
                let bodies = self.rank_chunk_bodies(query, target_repo.as_deref(), fetch * 3);
                hybrid_engine.search_in_with(query, fetch, scope, &bodies)
            }
        }
        .into_iter()
        .filter(|r| !exclude_tests || !is_test_file(&r.file_path))
        .take(max_results)
        .collect();

        // Format results
        let mut output = String::new();
        output.push_str(&format!("# Hybrid Search Results for: `{}`\n\n", query));
        output.push_str(&format!("**Mode**: {}\n", mode));
        output.push_str(&format!("**Results**: {}\n\n", results.len()));

        for (i, result) in results.iter().enumerate() {
            output.push_str(&format!("## {}. {}\n", i + 1, result.file_path));
            if target_repo.is_none() {
                output.push_str(&format!("- **Repo**: {}\n", result.repo));
            }
            output.push_str(&format!("- **Score**: {:.4}\n", result.score));
            output.push_str(&format!(
                "- **Lines**: {}-{}\n",
                result.start_line, result.end_line
            ));

            if let Some(bm25) = result.bm25_rank {
                output.push_str(&format!("- **BM25 rank**: {}\n", bm25 + 1));
            }
            if let Some(tfidf) = result.tfidf_rank {
                output.push_str(&format!("- **TF-IDF rank**: {}\n", tfidf + 1));
            }

            if !result.matched_terms.is_empty() {
                output.push_str(&format!(
                    "- **Matched terms**: {}\n",
                    result.matched_terms.join(", ")
                ));
            }

            // The persistent indexes keep no text; regenerate the snippet.
            let content = match self.normalized_doc_content_by_id(&result.repo, &result.id) {
                Some(rendered) => crate::search::SearchIndex::generate_snippet(
                    &rendered,
                    &result.matched_terms,
                    1,
                ),
                None => self.snippet_for_document(
                    &result.repo,
                    &result.file_path,
                    result.start_line,
                    result.end_line,
                    &result.matched_terms,
                ),
            };
            output.push_str("\n```\n");
            let snippet_lines: Vec<&str> = content.lines().take(10).collect();
            output.push_str(&snippet_lines.join("\n"));
            if content.lines().count() > 10 {
                output.push_str("\n... (truncated)");
            }
            output.push_str("\n```\n\n");
        }

        if results.is_empty() {
            output.push_str("No results found.\n");
        }

        Ok(output)
    }

    /// Ids must match the symbol cards' `file::symbol` for RRF to fuse them.
    fn rank_chunk_bodies(
        &self,
        query: &str,
        target_repo: Option<&str>,
        limit: usize,
    ) -> Vec<crate::hybrid_search::HybridResult> {
        let query_tokens: std::collections::HashSet<_> =
            crate::search::tokenize_code(query).into_iter().collect();
        if query_tokens.is_empty() {
            return Vec::new();
        }

        // (repo name, repo root) per file, aligned with `files`.
        let mut owners: Vec<(String, PathBuf)> = Vec::new();
        let mut files: Vec<PathBuf> = Vec::new();
        for repo_entry in self.repos.iter() {
            if target_repo.is_some_and(|t| t != repo_entry.key()) {
                continue;
            }
            let repo_path = &repo_entry.value().path;
            for file in self
                .file_cache
                .iter()
                .filter(|e| self.owns_file(repo_path, e.key()))
            {
                owners.push((repo_entry.key().clone(), repo_path.clone()));
                files.push(file.key().clone());
            }
        }

        let (sigs, mut hits) = self.score_cached_chunks(&query_tokens, &files, None);
        hits.sort_by(|a, b| {
            b.2.partial_cmp(&a.2)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| files[a.0].cmp(&files[b.0]))
                .then_with(|| a.1.cmp(&b.1))
        });

        let mut seen = std::collections::HashSet::new();
        let mut out = Vec::new();
        for (file_idx, chunk_idx, score) in hits {
            let Some(sig) = sigs[file_idx].as_ref().and_then(|c| c.get(chunk_idx)) else {
                continue;
            };
            let (repo, root) = &owners[file_idx];
            let rel = files[file_idx]
                .strip_prefix(root)
                .unwrap_or(&files[file_idx])
                .to_string_lossy()
                .into_owned();
            let id = match sig.symbol.as_deref() {
                Some(name) => format!("{rel}::{name}"),
                None => format!("{rel}:{chunk_idx}"),
            };
            // Split functions yield several chunks with the same key; the
            // best-scoring one stands for the function.
            if !seen.insert((repo.clone(), id.clone())) {
                continue;
            }
            out.push(crate::hybrid_search::HybridResult {
                id,
                repo: repo.clone(),
                file_path: rel,
                content: String::new(),
                start_line: sig.start_line,
                end_line: sig.end_line,
                score,
                bm25_rank: None,
                tfidf_rank: None,
                matched_terms: Vec::new(),
                symbol_name: sig.symbol.as_deref().map(str::to_string),
                result_type: "chunk".to_string(),
            });
            if out.len() >= limit {
                break;
            }
        }
        out
    }

    /// Signatures are aligned with `files`; hits are unsorted.
    fn score_cached_chunks(
        &self,
        query_tokens: &std::collections::HashSet<String>,
        files: &[PathBuf],
        target_type: Option<crate::chunking::ChunkType>,
    ) -> (Vec<Option<FileChunkSigs>>, Vec<ChunkHit>) {
        use crate::chunking::AstChunker;
        use rayon::prelude::*;

        let sigs: Vec<_> = files
            .par_iter()
            .map_init(AstChunker::new, |chunker, path| {
                self.chunk_cache.chunks_for(path, chunker)
            })
            .collect();
        // Interning happens while chunking, so resolve query ids afterwards.
        let query_ids = self.chunk_cache.known_ids(query_tokens);
        let query_len = query_tokens.len() as f64;
        let mut hits = Vec::new();
        for (file_idx, chunks) in sigs.iter().enumerate() {
            let Some(chunks) = chunks else { continue };
            for sig in chunks.iter() {
                if target_type.is_some_and(|t| sig.chunk_type != t) {
                    continue;
                }
                let common = sig.overlap(&query_ids);
                if common > 0 {
                    hits.push((file_idx, sig.index, common as f64 / query_len));
                }
            }
        }
        (sigs, hits)
    }

    /// Search over AST-aware code chunks
    pub async fn search_chunks(
        &self,
        query: &str,
        repo: Option<&str>,
        chunk_type: Option<&str>,
        max_results: usize,
        exclude_tests: Option<bool>,
    ) -> Result<String> {
        use crate::chunking::{AstChunker, ChunkType};
        use crate::search::tokenize_code;
        use crate::security_rules::is_test_file;

        let exclude_tests = exclude_tests.unwrap_or(false); // Default false for search
        let target_repo = match repo {
            Some(r) if !r.is_empty() => Some(self.resolve_repo_name(r)?),
            _ => None,
        };
        let chunker = AstChunker::new();
        let query_tokens: std::collections::HashSet<_> = tokenize_code(query).into_iter().collect();
        if query_tokens.is_empty() {
            return Ok(format!(
                "# Chunk Search Results for: `{}`\n\n**Results**: 0\n\n",
                query
            ));
        }
        let query_len = query_tokens.len() as f64;

        let target_type = chunk_type.and_then(|t| match t {
            "function" => Some(ChunkType::Function),
            "method" => Some(ChunkType::Method),
            "class" => Some(ChunkType::Class),
            "trait" => Some(ChunkType::Trait),
            "module" => Some(ChunkType::Module),
            _ => None,
        });

        // Normalized documents are not files on disk: scored directly below.
        let mut files: Vec<PathBuf> = Vec::new();
        let mut normalized_hits: Vec<(crate::chunking::CodeChunk, f64)> = Vec::new();
        for repo_entry in self.repos.iter() {
            let repo_name = repo_entry.key();
            let repo_meta = repo_entry.value();

            // Filter by repo if specified
            if let Some(ref target_repo) = target_repo {
                if repo_name != target_repo {
                    continue;
                }
            }

            let repo_path = &repo_meta.path;
            files.extend(
                self.file_cache
                    .iter()
                    .map(|e| e.key().clone())
                    .filter(|p| self.owns_file(repo_path, p))
                    .filter(|p| !exclude_tests || !is_test_file(&p.to_string_lossy())),
            );

            if let Some(documents) = self.normalized_docs.get(repo_name) {
                for document in documents.iter() {
                    if let Some(ref target) = target_type {
                        if *target != ChunkType::Module {
                            continue;
                        }
                    }

                    let primary_source = normalized_document_primary_source(document);
                    if exclude_tests && is_test_file(&primary_source) {
                        continue;
                    }

                    let content = render_normalized_search_content(document);
                    let chunk_tokens: std::collections::HashSet<_> =
                        tokenize_code(&content).into_iter().collect();
                    let common = query_tokens.intersection(&chunk_tokens).count();
                    if common > 0 {
                        normalized_hits.push((
                            crate::chunking::CodeChunk {
                                id: document.id.clone(),
                                content: content.clone(),
                                file_path: primary_source,
                                start_line: 1,
                                end_line: content.lines().count(),
                                language: "text".to_string(),
                                symbol_context: None,
                                chunk_type: ChunkType::Module,
                                doc_comment: None,
                                imports: Vec::new(),
                            },
                            common as f64 / query_len,
                        ));
                    }
                }
            }
        }

        let (_, file_hits) = self.score_cached_chunks(&query_tokens, &files, target_type);

        // Ties break on path and position: hash-map order is not stable.
        enum Hit {
            File(usize, usize),
            Normalized(usize),
        }
        let mut ranked: Vec<(Hit, f64)> = file_hits
            .iter()
            .map(|&(f, i, score)| (Hit::File(f, i), score))
            .chain(
                normalized_hits
                    .iter()
                    .enumerate()
                    .map(|(i, (_, score))| (Hit::Normalized(i), *score)),
            )
            .collect();
        let tie_key = |hit: &Hit| match hit {
            Hit::File(f, i) => (files[*f].to_string_lossy().into_owned(), *i),
            Hit::Normalized(i) => (normalized_hits[*i].0.file_path.clone(), 0),
        };
        ranked.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| tie_key(&a.0).cmp(&tie_key(&b.0)))
        });
        ranked.truncate(max_results);

        let mut rechunked: HashMap<usize, Vec<crate::chunking::CodeChunk>> = HashMap::new();
        let mut all_chunks: Vec<(crate::chunking::CodeChunk, f64)> = Vec::new();
        for (hit, score) in ranked {
            match hit {
                Hit::Normalized(i) => all_chunks.push((normalized_hits[i].0.clone(), score)),
                Hit::File(f, i) => {
                    let chunks = rechunked.entry(f).or_insert_with(|| {
                        let path = &files[f];
                        let path_str = path.to_string_lossy();
                        self.content_for(path)
                            .and_then(|content| {
                                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                    chunker.chunk_file(&content, &path_str)
                                }))
                                .ok()
                            })
                            .unwrap_or_default()
                    });
                    // Changed since scoring: the score is not about this chunk.
                    if let Some(chunk) = chunks.get(i) {
                        all_chunks.push((chunk.clone(), score));
                    }
                }
            }
        }

        // Format results
        let mut output = String::new();
        output.push_str(&format!("# Chunk Search Results for: `{}`\n\n", query));
        if let Some(ct) = chunk_type {
            output.push_str(&format!("**Filter**: {} chunks only\n", ct));
        }
        output.push_str(&format!("**Results**: {}\n\n", all_chunks.len()));

        for (i, (chunk, score)) in all_chunks.iter().enumerate() {
            output.push_str(&format!("## {}. {}\n", i + 1, chunk.id));
            output.push_str(&format!("- **File**: {}\n", chunk.file_path));
            output.push_str(&format!(
                "- **Lines**: {}-{}\n",
                chunk.start_line, chunk.end_line
            ));
            output.push_str(&format!("- **Type**: {}\n", chunk.chunk_type));
            output.push_str(&format!("- **Score**: {:.2}\n", score));

            if let Some(ref ctx) = chunk.symbol_context {
                output.push_str(&format!("- **Symbol**: `{}` ({:?})\n", ctx.name, ctx.kind));
                if let Some(ref sig) = ctx.signature {
                    output.push_str(&format!(
                        "- **Signature**: `{}`\n",
                        sig.chars().take(100).collect::<String>()
                    ));
                }
            }

            if let Some(ref doc) = chunk.doc_comment {
                let doc_preview: String = doc.lines().take(2).collect::<Vec<_>>().join(" ");
                output.push_str(&format!(
                    "- **Doc**: {}\n",
                    doc_preview.chars().take(80).collect::<String>()
                ));
            }

            output.push_str("\n```\n");
            let snippet_lines: Vec<&str> = chunk.content.lines().take(15).collect();
            output.push_str(&snippet_lines.join("\n"));
            if chunk.content.lines().count() > 15 {
                output.push_str("\n... (truncated)");
            }
            output.push_str("\n```\n\n");
        }

        if all_chunks.is_empty() {
            output.push_str("No matching chunks found.\n");
        }

        Ok(output)
    }
}
