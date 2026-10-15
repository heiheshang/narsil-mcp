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
    PreparedNormalized, PreparedSymbol, RepoMetadata, WalkOutcome,
};
use crate::callgraph::CallGraph;
use crate::ingest::NormalizedDocument;
use crate::persist::PersistedIndex;
use crate::search::{DocType, SearchDocument};
use crate::symbols::Symbol;

/// Files are prepared and indexed in chunks so the whole corpus's content and
/// tree-sitter trees are never resident at once. The pre-streaming `collect()`
/// held every file's parse tree simultaneously — the 20 GB OOM on the 1С
/// config. A chunk's trees are dropped once it is indexed, with at most three
/// chunks in flight (see [`CodeIntelEngine::run_parse_pipeline`]).
const CHUNK_SIZE: usize = 200;

/// What the walk over one repository's files shares between its stages.
struct WalkCtx<'a> {
    path: &'a Path,
    repo_tag: &'a Arc<str>,
    /// Where symbols may come from instead of the parser.
    reuse: Option<&'a PersistedIndex>,
    /// The persisted index, when this same build saved it.
    same_build: Option<&'a PersistedIndex>,
    search_warm: bool,
    graph_loaded: bool,
    call_graph: Option<&'a CallGraph>,
}

/// One file as the parse pool hands it to the indexing thread.
struct PreparedFile {
    file_path: PathBuf,
    content: String,
    parsed: Option<crate::parser::ParsedFile>,
    /// Symbols came from the persisted index, not the parser.
    reused: bool,
    /// The search documents on disk are this file's current ones.
    unchanged: bool,
    prepared: PreparedDocuments,
}

/// What the walk accumulates over all files of a repository.
#[derive(Default)]
struct WalkAcc {
    languages: HashMap<String, LanguageStats>,
    symbols: Vec<Symbol>,
    neural_docs: Vec<crate::neural::NeuralDocument>,
    normalized_docs: Vec<NormalizedDocument>,
    pending_calls: Vec<(String, Vec<crate::callgraph::PendingCall>)>,
    deferred_files: std::collections::HashSet<PathBuf>,
    file_count: usize,
    total_lines: usize,
    reparsed_files: usize,
    stale_files: usize,
}

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

        // Documents and the knowledge graph of an unchanged file are reused
        // only from a run of this same build; another build may produce them
        // differently.
        let same_build =
            reuse.filter(|persisted| persisted.build_id == crate::persist::current_build_id());
        // Symbols are only reusable when the walk does not also need parse
        // trees: the call graph is built from them, and they are not persisted.
        let reuse = reuse.filter(|_| !self.options.call_graph_enabled);

        let search_warm = self.prepare_search_state(&repo_name, same_build);
        let files = self.walk_repo_files(path, &repo_name)?;
        let removed_files = match (search_warm, same_build) {
            (true, Some(persisted)) => {
                self.drop_removed_files(path, &repo_tag, &repo_name, &files, persisted)
            }
            _ => 0,
        };
        let graph_loaded = self.load_unchanged_graph(
            path,
            &repo_name,
            &files,
            same_build,
            search_warm,
            removed_files,
        );
        let reuse = if graph_loaded { same_build } else { reuse };

        // Parse + extract in chunks; the call graph is fed incrementally
        // (function defs in pass 1, call sites in pass 2). The DashMap ref is
        // scoped to this block so the non-Send guard never crosses the neural
        // `.await` below.
        let acc = {
            let call_graph = if self.options.call_graph_enabled {
                self.call_graphs.get(&repo_name)
            } else {
                None
            };
            let ctx = WalkCtx {
                path,
                repo_tag: &repo_tag,
                reuse,
                same_build,
                search_warm,
                graph_loaded,
                call_graph: call_graph.as_deref(),
            };
            let mut acc = self.run_parse_pipeline(&ctx, &files)?;
            if let Some(cg) = ctx.call_graph.filter(|_| !graph_loaded) {
                self.resolve_call_sites(cg, &ctx, &files, &mut acc, &repo_name)?;
            }
            acc
        };

        let WalkAcc {
            languages,
            symbols: symbols_vec,
            neural_docs,
            normalized_docs: normalized_docs_vec,
            file_count,
            total_lines,
            reparsed_files,
            stale_files,
            ..
        } = acc;

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

        self.index_neural_docs(neural_docs).await;

        // Record indexing metrics
        let elapsed = start_time.elapsed();
        self.metrics
            .record_repo_index(repo_name.clone(), elapsed, file_count, symbols_vec.len());

        self.repos.insert(repo_name.clone(), metadata);
        self.symbols.insert(repo_name.clone(), symbols_vec);
        self.normalized_docs
            .insert(repo_name.clone(), normalized_docs_vec);

        self.log_memory_breakdown();

        #[cfg(feature = "graph")]
        self.update_knowledge_graph(&repo_name, search_warm, stale_files, removed_files);

        Ok(WalkOutcome {
            reparsed_files,
            indexed_files: file_count,
            stale_files,
        })
    }

    /// Embed the symbols collected for the neural engine, when it is enabled.
    async fn index_neural_docs(&self, neural_docs: Vec<crate::neural::NeuralDocument>) {
        let Some(ref neural) = self.neural_engine else {
            return;
        };
        if neural_docs.is_empty() {
            return;
        }
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

    /// Transform the repository's symbols into the RDF knowledge graph, unless
    /// it already holds what the last run of this build wrote.
    #[cfg(feature = "graph")]
    fn update_knowledge_graph(
        &self,
        repo_name: &str,
        search_warm: bool,
        stale_files: usize,
        removed_files: usize,
    ) {
        use crate::persistence::RepositoryTransformer;
        let Some(ref graph) = self.knowledge_graph else {
            return;
        };
        let _graph_span = debug_span!("knowledge_graph").entered();
        // Same build and no file changed: the symbols, and so the graph,
        // are what the last run wrote.
        let graph_current = search_warm
            && stale_files == 0
            && removed_files == 0
            && RepositoryTransformer::is_transformed(graph, repo_name).unwrap_or(false);
        if graph_current {
            debug!("Knowledge graph for {repo_name} is up to date");
            return;
        }
        debug!(
            "Transforming {} symbols to RDF",
            self.symbols.get(repo_name).map_or(0, |s| s.len())
        );

        if let Some(symbols) = self.symbols.get(repo_name) {
            match Self::load_symbols_into_graph(graph, repo_name, &symbols) {
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

    /// Prepare files on the rayon pool while this thread indexes them, chunk
    /// by chunk, feeding each chunk's functions to the call graph (pass 1).
    fn run_parse_pipeline(&self, ctx: &WalkCtx<'_>, files: &[PathBuf]) -> Result<WalkAcc> {
        let mut acc = WalkAcc::default();

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
            ctx.reuse.is_some()
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
                        .filter_map(|file_path| self.prepare_file(ctx, file_path))
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

                for file in parsed_chunk {
                    self.index_prepared_file(ctx, &mut acc, file, &mut chunk_trees);
                }

                drop(documents_guard);

                // Call-graph pass 1: function definitions for this chunk.
                if let Some(cg) = ctx.call_graph {
                    let _functions_guard = functions_span.enter();
                    cg.collect_functions(&chunk_trees)?;
                    // Freeing a tree walks every node; on this thread it was
                    // a second of a BSL corpus's critical path.
                    chunk_trees.into_par_iter().for_each(drop);
                }
            }
            Ok(())
        })?;
        Ok(acc)
    }

    /// Call-graph pass 2: call sites, resolved against every function
    /// gathered in pass 1. Files whose sites pass 1 already read are only
    /// resolved; any other is re-parsed, resolved and its tree dropped on the
    /// same worker (call graph is opt-in, so the double parse only happens
    /// for it).
    fn resolve_call_sites(
        &self,
        cg: &CallGraph,
        ctx: &WalkCtx<'_>,
        files: &[PathBuf],
        acc: &mut WalkAcc,
        repo_name: &str,
    ) -> Result<()> {
        let _calls_span = debug_span!("callgraph_calls", files = files.len()).entered();
        debug!(
            "Resolving call sites across {} files, {} symbols",
            files.len(),
            acc.symbols.len()
        );
        std::mem::take(&mut acc.pending_calls)
            .into_par_iter()
            .for_each(|(relative_path, calls)| cg.resolve_pending_calls(&relative_path, calls));
        files.par_iter().try_for_each(|file_path| {
            if acc.deferred_files.contains(file_path) {
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
                .strip_prefix(ctx.path)
                .unwrap_or(file_path)
                .to_string_lossy();
            cg.collect_file_calls(&relative_path, &content, &tree)
        })?;
        cg.link_extensions();
        info!(
            "Built call graph for {} with {} files",
            repo_name, acc.file_count
        );
        Ok(())
    }

    /// Read one file on the parse pool and prepare everything that does not
    /// depend on indexing order: symbols, search documents, 1C metadata and
    /// deferred call sites. `None` when the file cannot be read as text.
    fn prepare_file(&self, ctx: &WalkCtx<'_>, file_path: &Path) -> Option<PreparedFile> {
        let parse_start = std::time::Instant::now();
        let content = read_indexable_text_file(file_path)?;
        let unchanged = ctx.search_warm
            && ctx
                .same_build
                .is_some_and(|persisted| unchanged_since(persisted, file_path));
        let (parsed, reused) = self.file_symbols(ctx, file_path, &content);
        self.metrics.record_file_parse(parse_start.elapsed());
        let mut prepared = PreparedDocuments::new(parsed.as_ref(), &content, !unchanged);
        if file_path.extension().is_some_and(|ext| ext == "xml") {
            let relative = file_path
                .strip_prefix(ctx.path)
                .unwrap_or(file_path)
                .to_string_lossy();
            prepared.platform_entries =
                crate::ingest::onec::platform_entry_points(&relative, &content);
            let into_search = !unchanged;
            prepared.normalized = collect_onec_normalized_documents(ctx.path, file_path)
                .into_iter()
                .map(|document| PreparedNormalized::new(document, into_search))
                .collect();
        }
        if let (Some(cg), Some(tree)) = (
            ctx.call_graph,
            parsed.as_ref().and_then(|parsed| parsed.tree.as_ref()),
        ) {
            let relative = file_path
                .strip_prefix(ctx.path)
                .unwrap_or(file_path)
                .to_string_lossy();
            if CallGraph::defers_calls(&relative) {
                prepared.pending_calls = Some(cg.pending_file_calls(&relative, &content, tree));
            }
        }
        Some(PreparedFile {
            file_path: file_path.to_path_buf(),
            content,
            parsed,
            reused,
            unchanged,
            prepared,
        })
    }

    /// The symbols of a file, and whether they came from the persisted index
    /// instead of the parser.
    fn file_symbols(
        &self,
        ctx: &WalkCtx<'_>,
        file_path: &Path,
        content: &str,
    ) -> (Option<crate::parser::ParsedFile>, bool) {
        if ctx.graph_loaded {
            // Every file is unchanged and recorded, those without symbols
            // included.
            let symbols = ctx
                .reuse
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
            return (parsed, true);
        }
        match reusable_symbols(ctx.reuse, file_path) {
            Some(symbols) => (
                Some(crate::parser::ParsedFile {
                    path: file_path.to_string_lossy().to_string(),
                    // Ask the parser for the label instead of deriving one
                    // from the extension: a warm start must report the same
                    // language names as a cold one, not `Rust` beside `rust`.
                    language: self
                        .parser
                        .language_name(file_path)
                        .unwrap_or_else(|| "unknown".to_string()),
                    symbols,
                    tree: None,
                }),
                true,
            ),
            None => (self.parser.parse_file(file_path, content).ok(), false),
        }
    }

    /// Index one prepared file on the indexing thread: search documents,
    /// symbols, TF-IDF and neural documents, and the parse tree for the first
    /// call-graph pass.
    fn index_prepared_file(
        &self,
        ctx: &WalkCtx<'_>,
        acc: &mut WalkAcc,
        file: PreparedFile,
        chunk_trees: &mut Vec<(String, String, tree_sitter::Tree)>,
    ) {
        let PreparedFile {
            file_path,
            content,
            parsed,
            reused,
            unchanged,
            prepared,
        } = file;
        acc.file_count += 1;
        if parsed.as_ref().is_some_and(|p| !p.symbols.is_empty()) && ctx.reuse.is_some() && !reused
        {
            // Missing from the cache or changed on disk.
            acc.reparsed_files += 1;
        }
        let lines = content.lines().count();
        acc.total_lines += lines;

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
        let lang_stats = acc.languages.entry(language_key).or_default();
        lang_stats.file_count += 1;
        lang_stats.line_count += lines;
        lang_stats.byte_count += content.len();

        // Collect symbols with file path and index for embeddings
        let relative_path = file_path
            .strip_prefix(ctx.path)
            .unwrap_or(&file_path)
            .to_string_lossy()
            .to_string();

        // Documents for an unchanged file are already in the
        // persisted index; re-adding them would duplicate every
        // hit. A changed file has its old documents dropped first,
        // because symbols it used to define may be gone.
        let index_into_search = !unchanged;
        if index_into_search {
            acc.stale_files += 1;
            if ctx.search_warm {
                self.search_index
                    .tantivy()
                    .delete_file(ctx.repo_tag, &relative_path);
                if let Some(tri) = self.substring_index.as_ref() {
                    tri.delete_file(ctx.repo_tag, &relative_path);
                }
            }
            // Substring search indexes whole file text, including
            // the punctuation a token index throws away — that is
            // the point of it.
            if let Some(tri) = self.substring_index.as_ref() {
                tri.add_file(ctx.repo_tag, &relative_path, &content);
            }
        }

        let has_parser_support = parsed.is_some();
        if let Some(parsed) = parsed {
            let crate::parser::ParsedFile { symbols, tree, .. } = parsed;

            for (symbol, prepared) in symbols.into_iter().zip(prepared.symbols) {
                self.index_file_symbol(ctx, acc, &relative_path, symbol, prepared);
            }

            // Collect tree for call graph if enabled and tree exists
            if self.options.call_graph_enabled {
                if let Some(tree) = tree {
                    chunk_trees.push((relative_path.clone(), content.clone(), tree));
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
            acc.pending_calls.push((relative_path.clone(), calls));
            acc.deferred_files.insert(file_path.clone());
        }
        if let Some(cg) = ctx.call_graph {
            for entry in prepared.platform_entries {
                cg.add_platform_entry(&entry.module_path, &entry.procedure, entry.reason);
            }
        }

        if !has_parser_support && index_into_search {
            self.search_index.index_file_counted(
                Arc::clone(ctx.repo_tag),
                &relative_path,
                &content,
                prepared.file_len,
            );
        }

        for document in prepared.normalized {
            acc.normalized_docs
                .push(self.index_normalized_document(Arc::clone(ctx.repo_tag), document));
        }
    }

    /// Index one symbol of a file: its search card, its signature for TF-IDF
    /// similarity and the neural engine.
    fn index_file_symbol(
        &self,
        ctx: &WalkCtx<'_>,
        acc: &mut WalkAcc,
        relative_path: &str,
        mut symbol: Symbol,
        prepared: PreparedSymbol,
    ) {
        symbol.file_path = relative_path.to_string();

        if let Some((card, card_len)) = prepared.card {
            self.search_index.index_symbol_counted(
                Arc::clone(ctx.repo_tag),
                relative_path,
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
                Arc::clone(ctx.repo_tag),
                relative_path.to_string(),
                &tokens,
                symbol.start_line,
                symbol.end_line,
            );

            // Collect for neural batch indexing if enabled
            if self.neural_engine.is_some() {
                acc.neural_docs.push(crate::neural::NeuralDocument {
                    id: symbol_id,
                    repo: Arc::clone(ctx.repo_tag),
                    file_path: relative_path.to_string(),
                    content: sig.clone(),
                    start_line: symbol.start_line,
                    end_line: symbol.end_line,
                    symbol_name: Some(symbol.name.clone()),
                });
            }
        }

        acc.symbols.push(symbol);
    }

    /// Whether this repository's search documents and trigrams on disk can be
    /// kept. When they cannot, both are cleared for the repository.
    fn prepare_search_state(&self, repo_name: &str, same_build: Option<&PersistedIndex>) -> bool {
        // The search index persists separately from the symbol store, so a warm
        // symbol cache does not by itself prove the documents are there: a user
        // who deleted one file but not the other would otherwise get a
        // repository that reports as cached and answers no searches. Requiring
        // documents on disk makes the two agree before anything is skipped.
        // Both indexes must hold the repository, not just one: they are
        // separate directories and a partial cache must not be mistaken for a
        // complete one.
        let search_warm = same_build.is_some()
            && self.search_index.tantivy().repo_doc_count(repo_name) > 0
            && self
                .substring_index
                .as_ref()
                .is_none_or(|tri| tri.repo_file_count(repo_name) > 0);
        if !search_warm {
            // Cold, or out of step with the symbol cache: clear this
            // repository rather than layering a second copy of every document
            // on top of the last run's.
            if let Err(e) = self.search_index.tantivy().delete_repo(repo_name) {
                warn!("Failed to clear search index for {}: {}", repo_name, e);
            }
            if let Some(tri) = self.substring_index.as_ref() {
                tri.delete_repo(repo_name);
            }
        }
        search_warm
    }

    /// The files of the repository to index: `.gitignore` respected, vendored
    /// and oversized files left out.
    fn walk_repo_files(&self, path: &Path, repo_name: &str) -> Result<Vec<PathBuf>> {
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

        Ok(files)
    }

    /// Drop the documents of files the last run indexed and this walk no
    /// longer finds. Returns how many there were.
    fn drop_removed_files(
        &self,
        path: &Path,
        repo_tag: &Arc<str>,
        repo_name: &str,
        files: &[PathBuf],
        persisted: &PersistedIndex,
    ) -> usize {
        // Unchanged files keep their documents, so those of files gone since
        // the last run must go explicitly.
        let mut removed_files = 0usize;
        let walked: std::collections::HashSet<&Path> = files.iter().map(PathBuf::as_path).collect();
        for gone in persisted
            .files
            .keys()
            .filter(|f| !walked.contains(f.as_path()))
        {
            let relative_path = gone.strip_prefix(path).unwrap_or(gone).to_string_lossy();
            self.search_index
                .tantivy()
                .delete_file(repo_tag, &relative_path);
            if let Some(tri) = self.substring_index.as_ref() {
                tri.delete_file(repo_tag, &relative_path);
            }
            removed_files += 1;
        }
        if removed_files > 0 {
            debug!("{repo_name}: dropped documents of {removed_files} removed file(s)");
        }
        removed_files
    }

    /// Load the persisted call graph when nothing changed since the last save
    /// of this same build. Returns whether it was loaded.
    fn load_unchanged_graph(
        &self,
        path: &Path,
        repo_name: &str,
        files: &[PathBuf],
        same_build: Option<&PersistedIndex>,
        search_warm: bool,
        removed_files: usize,
    ) -> bool {
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
                }) && self.load_call_graph_snapshot(path, repo_name, persisted)
            });
        if graph_loaded {
            info!("{repo_name}: no file changed; call graph loaded from its snapshot");
        }
        graph_loaded
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
