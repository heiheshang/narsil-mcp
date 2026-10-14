use anyhow::Result;
use rayon::prelude::*;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;
use tracing::{debug, debug_span, info, warn, Instrument};

use super::{
    collect_onec_normalized_documents, ext_to_language, read_indexable_text_file, reusable_symbols,
    symbol_kind_to_doc_type, unchanged_since, CodeIntelEngine, LanguageStats, PreparedDocuments,
    PreparedNormalized, RepoMetadata, WalkOutcome,
};
use crate::callgraph::CallGraph;
use crate::ingest::NormalizedDocument;
use crate::persist::PersistedIndex;
use crate::search::{DocType, SearchDocument};
use crate::symbols::Symbol;

impl CodeIntelEngine {
    #[tracing::instrument(
        name = "index_repo",
        skip_all,
        fields(repo = tracing::field::Empty, warm = reuse.is_some())
    )]
    pub(super) async fn index_repo_inner(
        &self,
        path: &Path,
        reuse: Option<&PersistedIndex>,
    ) -> Result<WalkOutcome> {
        let start_time = std::time::Instant::now();
        let repo_name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("unknown")
            .to_string();
        tracing::Span::current().record("repo", repo_name.as_str());
        // Interned once per repository: every search/neural document carries a
        // cheap clone of this tag so searches can be scoped to one repo.
        let repo_tag: Arc<str> = Arc::from(repo_name.as_str());

        let mut languages: HashMap<String, LanguageStats> = HashMap::new();
        let mut symbols_vec: Vec<Symbol> = Vec::new();
        let mut neural_docs: Vec<crate::neural::NeuralDocument> = Vec::new();
        let mut normalized_docs_vec: Vec<NormalizedDocument> = Vec::new();
        let mut pending_calls: Vec<(String, Vec<crate::callgraph::PendingCall>)> = Vec::new();
        let mut deferred_files: std::collections::HashSet<PathBuf> =
            std::collections::HashSet::new();
        let mut file_count = 0;
        let mut total_lines = 0;
        let mut reparsed_files = 0usize;
        let mut stale_files = 0usize;

        // Documents and the knowledge graph of an unchanged file are reused
        // only from a run of this same build; another build may produce them
        // differently.
        let same_build =
            reuse.filter(|persisted| persisted.build_id == crate::persist::current_build_id());
        // Symbols are only reusable when the walk does not also need parse
        // trees: the call graph is built from them, and they are not persisted.
        let reuse = reuse.filter(|_| !self.options.call_graph_enabled);

        // The search index persists separately from the symbol store, so a warm
        // symbol cache does not by itself prove the documents are there: a user
        // who deleted one file but not the other would otherwise get a
        // repository that reports as cached and answers no searches. Requiring
        // documents on disk makes the two agree before anything is skipped.
        // Both indexes must hold the repository, not just one: they are
        // separate directories and a partial cache must not be mistaken for a
        // complete one.
        let search_warm = same_build.is_some()
            && self.search_index.tantivy().repo_doc_count(&repo_name) > 0
            && self
                .substring_index
                .as_ref()
                .is_none_or(|tri| tri.repo_file_count(&repo_name) > 0);
        if !search_warm {
            // Cold, or out of step with the symbol cache: clear this
            // repository rather than layering a second copy of every document
            // on top of the last run's.
            if let Err(e) = self.search_index.tantivy().delete_repo(&repo_name) {
                warn!("Failed to clear search index for {}: {}", repo_name, e);
            }
            if let Some(tri) = self.substring_index.as_ref() {
                tri.delete_repo(&repo_name);
            }
        }

        let walk_span = debug_span!("walk").entered();
        debug!("Walking {:?}", path);
        // Use ignore crate to respect .gitignore
        let walker = ignore::WalkBuilder::new(path)
            .hidden(true)
            .git_ignore(true)
            .git_global(true)
            .git_exclude(true)
            .require_git(false)
            .build();

        // Vendored/generated content is excluded by default so committed
        // vendor/, node_modules/ etc. don't drown search results; users can
        // opt back in with a `!`-prefixed pattern in `index_exclude`.
        let index_filter = crate::security_rules::IndexFilter::new(&self.options.index_exclude)?;
        let max_file_size = self.options.index_max_file_size;
        let mut skipped_vendored = 0usize;
        let mut skipped_large = 0usize;

        let files: Vec<PathBuf> = walker
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().map(|ft| ft.is_file()).unwrap_or(false))
            .filter(|e| {
                let rel = e.path().strip_prefix(path).unwrap_or(e.path());
                if index_filter.is_excluded(rel) {
                    skipped_vendored += 1;
                    return false;
                }
                if let Some(max) = max_file_size {
                    if e.metadata().map(|m| m.len() > max).unwrap_or(false) {
                        skipped_large += 1;
                        return false;
                    }
                }
                true
            })
            .map(|e| e.path().to_path_buf())
            .collect();
        drop(walk_span);

        if skipped_vendored > 0 || skipped_large > 0 {
            info!(
                "{}: skipped {} vendored/generated file(s) and {} oversized file(s) during indexing",
                repo_name, skipped_vendored, skipped_large
            );
        }

        // Unchanged files keep their documents, so those of files gone since
        // the last run must go explicitly.
        let mut removed_files = 0usize;
        if let (true, Some(persisted)) = (search_warm, same_build) {
            let walked: std::collections::HashSet<&Path> =
                files.iter().map(PathBuf::as_path).collect();
            for gone in persisted
                .files
                .keys()
                .filter(|f| !walked.contains(f.as_path()))
            {
                let relative_path = gone.strip_prefix(path).unwrap_or(gone).to_string_lossy();
                self.search_index
                    .tantivy()
                    .delete_file(&repo_tag, &relative_path);
                if let Some(tri) = self.substring_index.as_ref() {
                    tri.delete_file(&repo_tag, &relative_path);
                }
                removed_files += 1;
            }
            if removed_files > 0 {
                debug!("{repo_name}: dropped documents of {removed_files} removed file(s)");
            }
        }

        // Same build and not a file added, changed or removed since the last
        // save: the call graph this walk would build is the persisted one, and
        // the symbols in SQLite are those the parser would extract. Both are
        // loaded and nothing is parsed. Files the last save did not record
        // were unreadable or binary then, and must still be.
        let graph_loaded = self.options.call_graph_enabled
            && search_warm
            && removed_files == 0
            && same_build.is_some_and(|persisted| {
                let _span = debug_span!("check_unchanged").entered();
                files.par_iter().all(|file_path| {
                    unchanged_since(persisted, file_path)
                        || (!persisted.files.contains_key(file_path)
                            && read_indexable_text_file(file_path).is_none())
                }) && self.load_call_graph_snapshot(path, &repo_name, persisted)
            });
        if graph_loaded {
            info!("{repo_name}: no file changed; call graph loaded from its snapshot");
        }
        let reuse = if graph_loaded { same_build } else { reuse };

        // Process in chunks so the whole corpus's content + tree-sitter trees
        // are never resident at once. The pre-streaming `collect()` held every
        // file's parse tree simultaneously — the 20 GB OOM on the 1С config.
        // A chunk's trees are dropped once it is indexed, with at most three
        // chunks in flight (see the parse pipeline below).
        const CHUNK_SIZE: usize = 200;
        let metrics = Arc::clone(&self.metrics);

        // Parse + extract in chunks; the call graph is fed incrementally
        // (function defs in pass 1, call sites in pass 2). The DashMap ref is
        // scoped to this block so the non-Send guard never crosses the neural
        // `.await` below.
        {
            let call_graph = if self.options.call_graph_enabled {
                self.call_graphs.get(&repo_name)
            } else {
                None
            };

            // One span per phase, entered once per chunk: the close event
            // reports the phase's total across chunks, not 200 files' worth.
            // `parse` runs alongside `index_documents`, so the two overlap.
            let parse_span = debug_span!("parse", files = files.len());
            let documents_span = debug_span!("index_documents");
            let functions_span = debug_span!("callgraph_functions");
            // Which side of the parse pipeline is the bottleneck: parsing
            // waits on a full channel, indexing waits on an empty one.
            let parse_wait_span = debug_span!("parse_wait");
            let documents_wait_span = debug_span!("index_documents_wait");
            debug!(
                "Parsing {} files in chunks of {} (reusing cached symbols: {})",
                files.len(),
                CHUNK_SIZE,
                reuse.is_some()
            );

            // Chunk N+1 is parsed on the rayon pool while this thread indexes
            // chunk N; in lockstep each side idled while the other worked.
            // The channel holds one chunk, so at most three are resident.
            let (parsed_tx, parsed_rx) = std::sync::mpsc::sync_channel(1);
            std::thread::scope(|scope| -> Result<()> {
                scope.spawn(|| {
                    // Moved in, so the receiver sees the end once the last chunk is sent.
                    let parsed_tx = parsed_tx;
                    for chunk in files.chunks(CHUNK_SIZE) {
                        let parse_guard = parse_span.enter();
                        let parsed_chunk: Vec<_> = chunk
                            .par_iter()
                            .filter_map(|file_path| {
                                let parse_start = std::time::Instant::now();
                                let content = read_indexable_text_file(file_path)?;
                                let unchanged = search_warm
                                    && same_build.is_some_and(|persisted| {
                                        unchanged_since(persisted, file_path)
                                    });
                                let (parsed, reused) = if graph_loaded {
                                    // Every file is unchanged and recorded, those
                                    // without symbols included.
                                    let symbols = reuse
                                        .and_then(|persisted| persisted.files.get(file_path))
                                        .map(|metadata| metadata.symbols.clone());
                                    let parsed = symbols.and_then(|symbols| {
                                        Some(crate::parser::ParsedFile {
                                            path: file_path.to_string_lossy().to_string(),
                                            language: self.parser.language_name(file_path)?,
                                            symbols,
                                            tree: None,
                                        })
                                    });
                                    (parsed, true)
                                } else {
                                    match reusable_symbols(reuse, file_path) {
                                        Some(symbols) => (
                                            Some(crate::parser::ParsedFile {
                                                path: file_path.to_string_lossy().to_string(),
                                                // Ask the parser for the label instead of
                                                // deriving one from the extension: a warm
                                                // start must report the same language names
                                                // as a cold one, not `Rust` beside `rust`.
                                                language: self
                                                    .parser
                                                    .language_name(file_path)
                                                    .unwrap_or_else(|| "unknown".to_string()),
                                                symbols,
                                                tree: None,
                                            }),
                                            true,
                                        ),
                                        None => (
                                            self.parser.parse_file(file_path, &content).ok(),
                                            false,
                                        ),
                                    }
                                };
                                metrics.record_file_parse(parse_start.elapsed());
                                let mut prepared =
                                    PreparedDocuments::new(parsed.as_ref(), &content, !unchanged);
                                if file_path.extension().is_some_and(|ext| ext == "xml") {
                                    let relative = file_path
                                        .strip_prefix(path)
                                        .unwrap_or(file_path)
                                        .to_string_lossy();
                                    prepared.platform_entries =
                                        crate::ingest::onec::platform_entry_points(
                                            &relative, &content,
                                        );
                                    let into_search = !unchanged;
                                    prepared.normalized =
                                        collect_onec_normalized_documents(path, file_path)
                                            .into_iter()
                                            .map(|document| {
                                                PreparedNormalized::new(document, into_search)
                                            })
                                            .collect();
                                }
                                if let (Some(cg), Some(tree)) = (
                                    call_graph.as_ref(),
                                    parsed.as_ref().and_then(|parsed| parsed.tree.as_ref()),
                                ) {
                                    let relative = file_path
                                        .strip_prefix(path)
                                        .unwrap_or(file_path)
                                        .to_string_lossy();
                                    if CallGraph::defers_calls(&relative) {
                                        prepared.pending_calls =
                                            Some(cg.pending_file_calls(&relative, &content, tree));
                                    }
                                }
                                Some((
                                    file_path.clone(),
                                    content,
                                    parsed,
                                    reused,
                                    unchanged,
                                    prepared,
                                ))
                            })
                            .collect();
                        drop(parse_guard);
                        // The receiver is gone only when indexing stopped on an error.
                        let sent = parse_wait_span.in_scope(|| parsed_tx.send(parsed_chunk));
                        if sent.is_err() {
                            break;
                        }
                    }
                });

                while let Ok(parsed_chunk) = documents_wait_span.in_scope(|| parsed_rx.recv()) {
                    let documents_guard = documents_span.enter();
                    let mut chunk_trees: Vec<(String, String, tree_sitter::Tree)> = Vec::new();

                    for (file_path, content, parsed, reused, unchanged, prepared) in parsed_chunk {
                        file_count += 1;
                        if parsed.as_ref().is_some_and(|p| !p.symbols.is_empty())
                            && reuse.is_some()
                            && !reused
                        {
                            // Missing from the cache or changed on disk.
                            reparsed_files += 1;
                        }
                        let lines = content.lines().count();
                        total_lines += lines;

                        // Track parseable languages separately from text-only files.
                        let language_key = parsed
                            .as_ref()
                            .map(|parsed| parsed.language.clone())
                            .unwrap_or_else(|| {
                                file_path
                                    .extension()
                                    .and_then(|ext| ext.to_str())
                                    .map(ext_to_language)
                                    .filter(|lang| !lang.is_empty())
                                    .unwrap_or_else(|| "text".to_string())
                            });
                        let lang_stats = languages.entry(language_key).or_default();
                        lang_stats.file_count += 1;
                        lang_stats.line_count += lines;
                        lang_stats.byte_count += content.len();

                        // Collect symbols with file path and index for embeddings
                        let relative_path = file_path
                            .strip_prefix(path)
                            .unwrap_or(&file_path)
                            .to_string_lossy()
                            .to_string();

                        // Documents for an unchanged file are already in the
                        // persisted index; re-adding them would duplicate every
                        // hit. A changed file has its old documents dropped first,
                        // because symbols it used to define may be gone.
                        let index_into_search = !unchanged;
                        if index_into_search {
                            stale_files += 1;
                            if search_warm {
                                self.search_index
                                    .tantivy()
                                    .delete_file(&repo_tag, &relative_path);
                                if let Some(tri) = self.substring_index.as_ref() {
                                    tri.delete_file(&repo_tag, &relative_path);
                                }
                            }
                            // Substring search indexes whole file text, including
                            // the punctuation a token index throws away — that is
                            // the point of it.
                            if let Some(tri) = self.substring_index.as_ref() {
                                tri.add_file(&repo_tag, &relative_path, &content);
                            }
                        }

                        let has_parser_support = parsed.is_some();
                        if let Some(parsed) = parsed {
                            let crate::parser::ParsedFile { symbols, tree, .. } = parsed;

                            for (mut symbol, prepared) in symbols.into_iter().zip(prepared.symbols)
                            {
                                symbol.file_path = relative_path.clone();

                                if let Some((card, card_len)) = prepared.card {
                                    self.search_index.index_symbol_counted(
                                        Arc::clone(&repo_tag),
                                        &relative_path,
                                        &symbol.name,
                                        &card,
                                        symbol_kind_to_doc_type(&symbol.kind),
                                        symbol.start_line,
                                        symbol.end_line,
                                        Some(card_len),
                                    );
                                }
                                let tokens = prepared.signature_tokens;

                                // Index symbol into embedding engine for similarity search
                                if let (Some(ref sig), Some(tokens)) = (&symbol.signature, tokens) {
                                    let symbol_id = format!("{}::{}", relative_path, symbol.name);
                                    self.embedding_engine.index_tokens_embed_only(
                                        symbol_id.clone(),
                                        Arc::clone(&repo_tag),
                                        relative_path.clone(),
                                        &tokens,
                                        symbol.start_line,
                                        symbol.end_line,
                                    );

                                    // Collect for neural batch indexing if enabled
                                    if self.neural_engine.is_some() {
                                        neural_docs.push(crate::neural::NeuralDocument {
                                            id: symbol_id,
                                            repo: Arc::clone(&repo_tag),
                                            file_path: relative_path.clone(),
                                            content: sig.clone(),
                                            start_line: symbol.start_line,
                                            end_line: symbol.end_line,
                                            symbol_name: Some(symbol.name.clone()),
                                        });
                                    }
                                }

                                symbols_vec.push(symbol);
                            }

                            // Collect tree for call graph if enabled and tree exists
                            if self.options.call_graph_enabled {
                                if let Some(tree) = tree {
                                    chunk_trees.push((
                                        relative_path.clone(),
                                        content.clone(),
                                        tree,
                                    ));
                                }
                            }
                        }

                        // Record the file as indexed (content is read from disk on
                        // demand via `content_for`, not cached here).
                        self.file_cache.insert(file_path.clone(), ());

                        // Index the whole file into search only when the parser
                        // extracted no symbols (text-only / unsupported languages).
                        // Parsed files index per-symbol cards above instead.
                        if let Some(calls) = prepared.pending_calls {
                            pending_calls.push((relative_path.clone(), calls));
                            deferred_files.insert(file_path.clone());
                        }
                        if let Some(cg) = call_graph.as_ref() {
                            for entry in prepared.platform_entries {
                                cg.add_platform_entry(
                                    &entry.module_path,
                                    &entry.procedure,
                                    entry.reason,
                                );
                            }
                        }

                        if !has_parser_support && index_into_search {
                            self.search_index.index_file_counted(
                                Arc::clone(&repo_tag),
                                &relative_path,
                                &content,
                                prepared.file_len,
                            );
                        }

                        for document in prepared.normalized {
                            normalized_docs_vec.push(
                                self.index_normalized_document(Arc::clone(&repo_tag), document),
                            );
                        }
                    }

                    drop(documents_guard);

                    // Call-graph pass 1: function definitions for this chunk.
                    if let Some(cg) = call_graph.as_ref() {
                        let _functions_guard = functions_span.enter();
                        cg.collect_functions(&chunk_trees)?;
                        // Freeing a tree walks every node; on this thread it was
                        // a second of a BSL corpus's critical path.
                        chunk_trees.into_par_iter().for_each(drop);
                    }
                }
                Ok(())
            })?;

            // Call-graph pass 2: call sites, resolved against every function
            // gathered in pass 1. Files whose sites pass 1 already read are
            // only resolved; any other is re-parsed, resolved and its tree
            // dropped on the same worker (call graph is opt-in, so the double
            // parse only happens for it).
            if let Some(cg) = call_graph.as_ref().filter(|_| !graph_loaded) {
                let _calls_span = debug_span!("callgraph_calls", files = files.len()).entered();
                debug!(
                    "Resolving call sites across {} files, {} symbols",
                    files.len(),
                    symbols_vec.len()
                );
                pending_calls
                    .into_par_iter()
                    .for_each(|(relative_path, calls)| {
                        cg.resolve_pending_calls(&relative_path, calls)
                    });
                files.par_iter().try_for_each(|file_path| {
                    if deferred_files.contains(file_path) {
                        return Ok(());
                    }
                    let Some(content) = read_indexable_text_file(file_path) else {
                        return Ok(());
                    };
                    // Only the tree: symbols were extracted in pass 1.
                    let Ok(tree) = self.parser.parse_to_tree(file_path, &content) else {
                        return Ok(());
                    };
                    let relative_path = file_path
                        .strip_prefix(path)
                        .unwrap_or(file_path)
                        .to_string_lossy();
                    cg.collect_file_calls(&relative_path, &content, &tree)
                })?;
                cg.link_extensions();
                info!(
                    "Built call graph for {} with {} files",
                    repo_name, file_count
                );
            }
        }

        let metadata = RepoMetadata {
            name: repo_name.clone(),
            path: path.to_path_buf(),
            file_count,
            total_lines,
            languages,
            last_indexed: SystemTime::now(),
        };

        info!(
            "Indexed {} files, {} symbols in {}",
            file_count,
            symbols_vec.len(),
            repo_name
        );

        // Batch index neural embeddings if enabled
        if let Some(ref neural) = self.neural_engine {
            if !neural_docs.is_empty() {
                info!(
                    "Generating neural embeddings for {} symbols...",
                    neural_docs.len()
                );
                let items: Vec<(crate::neural::NeuralDocument,)> =
                    neural_docs.into_iter().map(|d| (d,)).collect();
                // `index_batch` drives a blocking reqwest client. Calling it directly
                // from this async fn runs it on a tokio worker thread, which stalls
                // the runtime after ~128 requests: the server answers, the client
                // never picks the response up, and every later request times out.
                let neural = Arc::clone(neural);
                let neural_span = debug_span!("neural_embeddings", symbols = items.len());
                match tokio::task::spawn_blocking(move || neural.index_batch(&items))
                    .instrument(neural_span)
                    .await
                {
                    Ok(Ok(())) => info!("Neural embeddings indexed successfully"),
                    Ok(Err(e)) => warn!(
                        "Failed to batch index neural embeddings — semantic search falls back \
                         to TF-IDF for these symbols: {e}"
                    ),
                    Err(e) => warn!("Neural embedding task panicked: {e}"),
                }
            }
        }

        // Record indexing metrics
        let elapsed = start_time.elapsed();
        self.metrics
            .record_repo_index(repo_name.clone(), elapsed, file_count, symbols_vec.len());

        self.repos.insert(repo_name.clone(), metadata);
        self.symbols.insert(repo_name.clone(), symbols_vec);
        self.normalized_docs
            .insert(repo_name.clone(), normalized_docs_vec);

        self.log_memory_breakdown();

        // Transform symbols to RDF knowledge graph if enabled
        #[cfg(feature = "graph")]
        if let Some(ref graph) = self.knowledge_graph {
            use crate::persistence::RepositoryTransformer;
            let _graph_span = debug_span!("knowledge_graph").entered();
            // Same build and no file changed: the symbols, and so the graph,
            // are what the last run wrote.
            let graph_current = search_warm
                && stale_files == 0
                && removed_files == 0
                && RepositoryTransformer::is_transformed(graph, &repo_name).unwrap_or(false);
            if graph_current {
                debug!("Knowledge graph for {repo_name} is up to date");
            } else {
                debug!(
                    "Transforming {} symbols to RDF",
                    self.symbols.get(&repo_name).map_or(0, |s| s.len())
                );

                if let Some(symbols) = self.symbols.get(&repo_name) {
                    match Self::load_symbols_into_graph(graph, &repo_name, &symbols) {
                        Ok(()) => info!(
                            "Transformed {} symbols to RDF knowledge graph for {}",
                            symbols.len(),
                            repo_name
                        ),
                        Err(e) => warn!(
                            "Failed to transform symbols to RDF for {}: {}",
                            repo_name, e
                        ),
                    }
                }
                if let Err(e) = graph.flush() {
                    warn!("Failed to flush knowledge graph: {e}");
                }
            }
        }

        Ok(WalkOutcome {
            reparsed_files,
            indexed_files: file_count,
            stale_files,
        })
    }

    /// Writes the triples of `symbols`, then those of the repository, in one
    /// bulk load. The repository type triple that
    /// [`RepositoryTransformer::is_transformed`] looks for lands in the same
    /// commit, so it never marks a half-written graph.
    #[cfg(feature = "graph")]
    fn load_symbols_into_graph(
        graph: &crate::persistence::KnowledgeGraph,
        repo_name: &str,
        symbols: &[Symbol],
    ) -> Result<()> {
        use crate::persistence::{RepositoryTransformer, SymbolTransformer};
        // Chunks keep the triples built ahead of the loader bounded.
        const CHUNK: usize = 16 * 1024;

        let symbol_quads = symbols.chunks(CHUNK).flat_map(|chunk| {
            let built: Vec<Result<Vec<_>>> = chunk
                .par_iter()
                .map(|symbol| {
                    let mut quads = Vec::with_capacity(10);
                    SymbolTransformer::quads(repo_name, symbol, &mut quads).map(|_| quads)
                })
                .collect();
            built.into_iter().flat_map(|quads| match quads {
                Ok(quads) => quads.into_iter().map(Ok).collect::<Vec<_>>(),
                Err(e) => vec![Err(e)],
            })
        });

        let file_paths: Vec<&str> = symbols
            .iter()
            .map(|s| s.file_path.as_str())
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .collect();
        let mut quads = Vec::new();
        let repo_quads: Vec<Result<_>> =
            match RepositoryTransformer::quads(repo_name, file_paths.into_iter(), &mut quads) {
                Ok(_) => quads.into_iter().map(Ok).collect(),
                Err(e) => vec![Err(e)],
            };

        graph.bulk_load(symbol_quads.chain(repo_quads))
    }

    /// Load the call-graph snapshot saved with `persisted` into this
    /// repository's graph. False when there is none, it is from another save,
    /// or it cannot be read; the graph is then left as it was.
    fn load_call_graph_snapshot(
        &self,
        path: &Path,
        repo_name: &str,
        persisted: &PersistedIndex,
    ) -> bool {
        let _span = debug_span!("load_call_graph").entered();
        if persisted.call_graph_stamp.is_empty() {
            return false;
        }
        let (Some(store), Some(cg)) = (self.index_store.as_ref(), self.call_graphs.get(repo_name))
        else {
            return false;
        };
        let snapshot = match store.load_call_graph(path) {
            Ok(Some(snapshot)) => snapshot,
            Ok(None) => return false,
            Err(e) => {
                warn!("Failed to read call-graph snapshot for {repo_name}: {e}");
                return false;
            }
        };
        cg.from_snapshot(&snapshot, Some(&persisted.call_graph_stamp))
            .unwrap_or_else(|e| {
                warn!("Failed to load call-graph snapshot for {repo_name}: {e}");
                false
            })
    }

    /// Without `prepared.search` the search index already holds this document
    /// from a previous run. The TF-IDF embeddings below are not persisted, so
    /// they are rebuilt either way.
    pub(super) fn index_normalized_document(
        &self,
        repo: Arc<str>,
        prepared: PreparedNormalized,
    ) -> NormalizedDocument {
        let PreparedNormalized {
            document,
            file_path,
            end_line,
            tokens,
            search,
        } = prepared;
        let start_line = 1;

        // Raw text is NOT retained: display re-renders it from `normalized_docs`
        // on demand (see `normalized_doc_content_by_id`). Storing it here and in
        // the embedding engine doubled ~1 GB of redundant content on the 1C dump.
        if let Some((term_freq, doc_len)) = search {
            self.search_index.add_document_counted(
                SearchDocument {
                    id: document.id.clone(),
                    repo: Arc::clone(&repo),
                    file_path: file_path.clone(),
                    content: String::new(),
                    doc_type: DocType::Other,
                    start_line,
                    end_line,
                    term_freq,
                },
                doc_len,
            );
        }

        self.embedding_engine.index_tokens_embed_only(
            document.id.clone(),
            repo,
            file_path,
            &tokens,
            start_line,
            end_line,
        );
        document
    }
}
