[← Performance](performance.md) · [Back to README](../README.md) · [Development →](development.md)

# Profiling

CPU profiles come from [samply](https://github.com/mstange/samply); the time split
between indexing phases and tool calls comes from `tracing` spans.

| Question | Use |
|----------|-----|
| Which functions burn CPU | `make profile-index`, `make profile-bench`, `make profile-attach` |
| Which indexing phase is slow | `NARSIL_TRACE_SPANS=1 RUST_LOG=narsil_mcp=debug` |
| Which tool call is slow | the `Tool execution completed` log line (`duration_ms`) and `get_metrics` |
| Did a change make search faster | a paired before/after run (`eval/ragdiag`), not two profiles |

## Setup

```bash
cargo install --locked samply
echo 1 | sudo tee /proc/sys/kernel/perf_event_paranoid   # until reboot
make profile-check
```

`make profile-check` fails with the exact command to run when samply is missing or
`perf_event_paranoid` is above 1.

The release profile strips symbols (`strip = true`), so a release binary profiles as
bare addresses. `make profile-build` builds the `profiling` Cargo profile instead:
release settings (LTO, `codegen-units = 1`, `panic = "abort"`) plus
`debug = "line-tables-only"` and no stripping. The binary lands in
`target/profiling/narsil-mcp`. The `bench` profile carries symbols the same way.

## Make targets

| Target | What it does |
|--------|--------------|
| `profile-build` | builds `target/profiling/narsil-mcp` with `RUST_FEATURES` |
| `profile-check` | checks samply and `perf_event_paranoid` |
| `profile-index` | profiles a full `--reindex --git --call-graph` of `REPOS` |
| `profile-bench` | profiles criterion bench `BENCH`, optionally filtered by `BENCH_FILTER` |
| `profile-attach` | profiles a running process `PID` for `PROFILE_SECONDS`, or until Ctrl+C |
| `profile-open` | opens `PROFILE` (default: the newest) in the Firefox Profiler |

Variables: `PROFILE_OUT` (default `target/profiles`), `SAMPLY_RATE` (default 1000 Hz),
`REPOS`, `BENCH` (default `indexing`), `BENCH_FILTER`, `PID`, `PROFILE_SECONDS`, `PROFILE`.

`profile-attach` needs `perf_event_paranoid = -1`: attaching maps a perf buffer per
CPU and exceeds `perf_event_mlock_kb` otherwise (samply reports `mmap failed`).

```bash
make profile-index REPOS=~/src/big-repo
make profile-bench BENCH=indexing BENCH_FILTER=call_graph_build
make profile-attach PID=$(pgrep -n narsil-mcp)
make profile-open
```

Indexing runs in a background task and the server exits on stdin EOF, so
`profile-index` cannot simply redirect stdin from `/dev/null`.
`scripts/profile-index.sh` keeps stdin open until the server logs
`Re-indexing complete` (or an indexing error), then closes it and lets the server exit.

`profile-bench` runs the bench with `--bench --profile-time 20`: criterion iterates
the selected benchmarks for 20 seconds without collecting statistics.

Not every bench exercises the library: `parsing` and `search`, and most of
`indexing`, are self-contained placeholders. The ones that call narsil are
`call_graph_build_50_go_files` (in `indexing`), `filtering` and `token_usage`.

## Tracing spans

```bash
NARSIL_TRACE_SPANS=1 RUST_LOG=narsil_mcp=debug \
  target/profiling/narsil-mcp --repos ./test-fixtures --reindex --call-graph </dev/null
```

- `RUST_LOG` sets the log filter. Without it the filter is `info`, or `debug` with
  `--verbose`. An unparsable `RUST_LOG` is reported with a warning and ignored.
- `NARSIL_TRACE_SPANS=1` prints a `close` event for every span to stderr, with
  `time.busy` and `time.idle`. It is off by default.

Spans:

| Span | Level | Covers |
|------|-------|--------|
| `tool{name}` | info | one tool call, stdio and HTTP |
| `index_repo{repo, warm}` | info | indexing one repository |
| `walk` | debug | directory walk and exclusion filters |
| `parse` | debug | tree-sitter parsing, summed over all chunks |
| `index_documents` | debug | BM25 symbol cards, TF-IDF, trigram and 1C documents |
| `callgraph_functions` | debug | call graph pass 1: function definitions |
| `callgraph_calls{files}` | debug | call graph pass 2: re-parsing and call resolution, in parallel |
| `neural_embeddings` | debug | neural embedding batch |
| `knowledge_graph` | debug | RDF transformation (`--graph`) |
| `commit_search_index` | debug | tantivy and trigram commit at startup |
| `save_index` | debug | SQLite persistence (`--persist`) |

Phases inside the per-chunk loop use one span that is entered once per chunk, so
their `time.busy` is already the total across chunks. `parse` of the next chunk
runs while `index_documents` handles the current one, so phase times add up to
more than `index_repo`.

## Reading a profile

- Turn on **Invert call stack** to see where time is spent rather than `main` and the
  tokio runtime.
- Check the thread tracks: rayon threads do parsing and the call graph, tokio workers
  serve requests, `spawn_blocking` threads compute embeddings.
- Idle threads with wall-clock time passing point at waiting (SQLite mutex, network,
  locks), which a CPU profile barely shows.
- Async stacks are cut at `.await`; for async paths, start from the spans.
- Profile on a corpus of the size you care about: `test-fixtures` indexes in
  milliseconds.
- A profile opens with symbols only next to the binary it was recorded from.

samply does not measure memory. Use `heaptrack` or `dhat`, keeping in mind that the
global allocator is mimalloc.
