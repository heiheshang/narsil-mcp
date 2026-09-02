// Library exports for integration tests
#![recursion_limit = "256"]

/// A `DashMap` hashed with `ahash` instead of the default SipHash-based
/// `RandomState` — for index-time maps hot enough (one entry per symbol/file,
/// looked up per call site across a large corpus) that SipHash's DoS
/// resistance costs real throughput. See the longer note on
/// `callgraph::CallGraph::nodes` for why that tradeoff is accepted even
/// though narsil-mcp's own supply-chain-audit use case means these keys can
/// come from adversarial (vendored, untrusted) source.
///
/// One alias instead of five hand-written `DashMap<K, V, ahash::RandomState>`
/// declarations spread across `callgraph.rs`/`index.rs`: changing the hasher
/// choice is then one edit, not five kept in lockstep by hand.
pub(crate) type FastMap<K, V> = dashmap::DashMap<K, V, ahash::RandomState>;

// Core modules (always available)
pub mod cache;
pub mod callgraph;
pub mod cfg;
pub mod chunking;
pub mod config;
pub mod dead_code;
pub mod dfg;
pub mod embeddings;
pub mod extract;
pub mod hybrid_search;
pub mod incremental;
pub mod ingest;
pub mod metrics;
pub mod parser;
pub mod repo;
pub mod search;
pub mod security_config;
pub mod security_rules;
pub mod store;
pub mod supply_chain;
pub mod symbols;
pub mod taint;
pub mod text;
pub mod tool_metadata;
pub mod type_inference;
pub mod validation;

// Knowledge graph persistence (requires oxigraph)
#[cfg(feature = "graph")]
pub mod persistence;

// Code Context Graph generation (requires graph feature)
#[cfg(feature = "graph")]
pub mod ccg;

// Native-only modules (require tokio, octocrab, lsp, etc.)
#[cfg(feature = "native")]
pub mod git;
#[cfg(feature = "native")]
pub mod http_server;
#[cfg(feature = "native")]
pub mod index;
#[cfg(feature = "native")]
pub mod lsp;
#[cfg(feature = "native")]
pub mod mcp;
#[cfg(feature = "native")]
pub mod neural;
#[cfg(feature = "native")]
pub mod neural_cache;
#[cfg(feature = "native")]
pub mod persist;
#[cfg(feature = "native")]
pub mod remote;
#[cfg(feature = "native")]
pub mod streaming;
#[cfg(feature = "native")]
pub mod tool_handlers;

// WASM module (only compiled when targeting wasm32)
#[cfg(feature = "wasm")]
pub mod wasm;
