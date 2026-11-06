[← Architecture](architecture.md) · [Back to README](../README.md) · [Profiling →](profiling.md)

# Performance

## Benchmarks

Benchmarked on Apple M1 (criterion.rs):

### Parsing Throughput

| Language | Input Size | Time | Throughput |
|----------|------------|------|------------|
| Rust (large file) | 278 KB | 131 µs | **1.98 GiB/s** |
| Rust (medium file) | 27 KB | 13.5 µs | 1.89 GiB/s |
| Python | ~4 KB | 16.7 µs | - |
| TypeScript | ~5 KB | 13.9 µs | - |
| Mixed (5 files) | ~15 KB | 57 µs | - |

### Search Latency

| Operation | Corpus Size | Time |
|-----------|-------------|------|
| Symbol exact match | 1,000 symbols | **483 ns** |
| Symbol prefix match | 1,000 symbols | 2.7 µs |
| Symbol fuzzy match | 1,000 symbols | 16.5 µs |
| BM25 full-text | 1,000 docs | 80 µs |
| TF-IDF similarity | 1,000 docs | 130 µs |
| Hybrid (BM25+TF-IDF) | 1,000 docs | 151 µs |

### End-to-End Indexing

| Repository | Files | Symbols | Time | Memory |
|------------|-------|---------|------|--------|
| narsil-mcp (this repo) | 53 | 1,733 | 220 ms | ~50 MB |
| rust-analyzer | 2,847 | ~50K | 2.1s | 89 MB |
| linux kernel | 78,000+ | ~500K | 45s | 2.1 GB |

**Key metrics:**
- Tree-sitter parsing: **~2 GiB/s** sustained throughput
- Symbol lookup: **<1µs** for exact match
- Full-text search: **<1ms** for most queries
- Hybrid search runs BM25 + TF-IDF in parallel via rayon

## See Also

- [Architecture](architecture.md) — why it is fast
- [Troubleshooting](troubleshooting.md) — memory issues on large repos
