# Implementation Plan — MCP Tool UX Fixes

**Source:** Field report from real-world usage (8 tool calls: 4 good, 4 defective), 2026-08-21.
**Scope:** 5 reported defects. Root-cause analysis done; all locations verified against current `main` (70e597a).

## Root-Cause Summary

Three of the five defects share one cause: **`ToolRegistry::dispatch` performs no
argument validation against `input_schema`** (`src/tool_handlers/mod.rs:194-205` —
schemas are only served in `tools/list`). Unknown argument keys are silently
dropped, so a tool "succeeds" with an empty filter instead of erroring.

| # | Reported symptom | Actual root cause |
|---|---|---|
| 1 | `search_code` ignores `path` | Filter param is named `file_pattern`; `path` not in contract, silently dropped |
| 2 | `get_excerpt` returns "0 excerpts" for `start_line`/`end_line` | Those params don't exist — tool takes a `lines: int[]` anchor array; unknown keys dropped → empty anchors → empty success (`src/tool_handlers/repo.rs:58-87`, `src/extract.rs:70-72`) |
| 3 | vendor/ pollutes short/numeric queries | Index walk (`src/index.rs:751-762`) has **no built-in exclusions** — only `.gitignore`; the ready-made `**/vendor/**` list in `RepoConfig` (`src/repo.rs:38-50`) is dead code |
| 4 | `find_symbols` dumps 2.2 MB instead of exact match | Filter param is `pattern` (neighbours use `query`/`name`) → dropped → no filter, and **no output cap exists**. Not PHP-specific |
| 5 | `get_file` mojibake (`â”‚` instead of `│`) | Not a decoding bug: corrupted literals are **hardcoded in narsil source** (double-encoded UTF-8 in ~30 lines across 3 files, since initial commit). File content itself is returned correctly |

---

## Sprint 1 — Point Fixes (closes all 5 observed symptoms) [DONE 2026-08-21]

### 1.1 `search_code`: path filtering

- [x] Accept `path` as alias for `file_pattern` in handler (`src/tool_handlers/search.rs:21`):
      `args.get_str("file_pattern").or_else(|| args.get_str("path"))`
- [x] Normalize a wildcard-free pattern with no `/` into `**/<pattern>` (`src/index.rs:1646`) —
      today a bare `plan-review.md` is a literal repo-root-relative match and matches nothing
- [x] Return an error on invalid glob instead of swallowing via `.ok()` (`src/index.rs:1646`;
      same defect in `find_symbols` at `src/index.rs:1472`)
- [x] Schema: add `path` property (documented as alias) and glob examples
      (`'**/plan-review.md'`, `'src/**/*.rs'`) to `file_pattern` description (`src/tool_metadata.rs:494`)
- [x] Tests: alias works; bare-filename normalization; invalid glob errors

### 1.2 `get_excerpt`: line-range mode

- [x] Accept `start_line`/`end_line` (and single `line`) as aliases building the anchor list;
      accept `file` as alias for `path` (`src/tool_handlers/repo.rs:67-86`)
- [x] When an explicit range is given: default `expand_to_scope=false` and
      `max_lines >= end - start + 1` (otherwise clamping at `src/extract.rs:255-277` silently trims)
- [x] Error instead of empty success when no anchor can be derived
      (currently `unwrap_or_default()` at `src/tool_handlers/repo.rs:67`)
- [x] Fix out-of-range anchor panic: `expand_to_scope` indexes `lines[i]` out of bounds when
      `line > file length` (`src/extract.rs:157-159`); also guard `start > end` slice (`src/extract.rs:98`)
- [x] Tests: range on a .md file returns exact lines; out-of-range anchor errors, doesn't panic

### 1.3 `find_symbols`: exact match, cap, ranking

- [x] Accept `query`/`name` as aliases for `pattern` (`src/tool_handlers/symbols.rs:21`)
- [x] Add `limit` param (default ~200) with hard cap and visible truncation marker
      "showing 200 of N" (`src/index.rs:1505-1530`; WASM twin already does `.take(100)` at `src/wasm.rs:200`)
- [x] Rank exact name match first, then case-insensitive exact, then by name length,
      before applying the cap
- [x] Reject empty `pattern` and unknown `symbol_type` with an error instead of silently
      widening to everything (`src/index.rs:1460-1470, 1489`)
- [x] Fix schema description: says "Glob or regex pattern" but code does case-insensitive
      substring (`src/tool_metadata.rs:342`) — `*Foo*` currently returns 0 hits
- [x] Bump/namespace the query-cache key so previously cached full dumps don't replay
      (`src/index.rs:1533-1537`)
- [x] Tests: exact PHP class name returns that class first; output capped; empty pattern errors

### 1.4 Mojibake literals (get_file gutter et al.)

Literal replacement only — no logic change:

- [x] `src/index.rs:1825` (get_file gutter), `:1602`, `:1598` — `â”‚`→`│`, `â†’`→`→`
- [x] `src/index.rs:3588-3648` — call-path arrows and status emoji (`â†’`, `â†“`, `âš ï¸`, `âš¡`, `âœ…`)
- [x] `src/extract.rs:299, 305` — excerpt/search snippet gutter
- [x] `src/symbols.rs:63-81` — 19 lines of symbol-kind emoji
- [x] CI guard: `git grep -nP '[\x{00e0}-\x{00ff}][\x{0080}-\x{00bf}\x{2000}-\x{20ff}]' -- 'src/*.rs'`
      must return empty (implemented as the `no_mojibake_literals_in_source` test in tests/tool_ux_tests.rs)
- [x] Test: `get_file` output for a file with box-drawing chars contains `│` and no `â` bytes

---

## Sprint 2 — Schema-Level Argument Validation [DONE 2026-08-21]

Prevents the whole silent-drop class across all ~90 tools.

- [x] Validate `arguments` keys against the tool's `input_schema` in `ToolRegistry::dispatch`
      (`src/tool_handlers/mod.rs:194-205`); unknown key → error listing accepted parameters
- [x] Enforce `required` fields from the schema (e.g. `get_excerpt` requires `lines` today
      but the handler never checks)
- [x] Escape hatch: `NARSIL_ARG_VALIDATION=strict|warn|off` env var (default strict); underscore-prefixed keys always ignored
- [x] Tests: unknown key rejected; missing required key rejected; valid calls unaffected
- [x] Sweep param naming consistency: automated cross-check of every handler's arg reads vs its schema found 3 real gaps (get_code_graph missing 8 params, get_excerpt 'file', find_symbols 'name') — all declared; added `limit`↔`max_results` aliases on search_code/find_symbols

---

## Sprint 3 — Default vendor/ Exclusion [OPEN]

- [ ] **Pre-check on the affected repo:** confirm vendor/ is actually committed/not-gitignored.
      If it IS gitignored yet still indexed, the bug is in walker config or the unconditionally
      scanned `normalized_docs` branch (1C dumps under `vendor/erp_dump`, `src/index.rs:1713-1725`) —
      fix that first, don't paper over it
- [ ] Shared `is_vendored_path()` helper next to `is_test_file` (`src/security_rules.rs:34`).
      Defaults: `vendor/`, `node_modules/`, `target/`, `dist/`, `build/`, `__pycache__/`,
      `venv/`/`.venv/`, `*.min.js`/`*.min.css`, lockfiles. Consolidate the two existing copies
      (`src/incremental.rs:1247-1268` — watch-only; `src/repo.rs:38-50` — dead code) into it
- [ ] Apply in the main index walk after the walker filter (`src/index.rs:758-762`) so
      search_code, BM25, embeddings, call graph and index size all benefit
- [ ] Add `max_file_size` guard in `read_indexable_text_file` (`src/index.rs:9031-9038`) —
      currently unbounded; minified/generated files are the other half of the noise
- [ ] Config escape hatch: `exclude: Vec<String>` globs (+ `!`-negation for opt-in back, e.g.
      when auditing vendor/) on `RepoProfile` (`src/config/schema.rs:63-107`), threaded through
      `EngineOptions` (`src/index.rs:96`)
- [ ] Optional follow-up: query-time `exclude_paths` param on `search_code` as a per-call override
- [ ] Tests: vendored file not indexed by default; config negation re-includes it; oversized file skipped

---

## Sprint 4 — Documentation Sync [OPEN]

- [ ] `docs/architecture.md`: correct the `search_code` data-flow section — it claims Tantivy BM25
      (`docs/architecture.md:372-405`); the real implementation is a linear case-insensitive
      `contains` scan over `file_cache` (`src/index.rs:1610-1770`). Tantivy is a declared dep
      unused in `src/`
- [ ] `docs/configuration.md`: document new `exclude` globs, `max_file_size`, default exclusion list
- [ ] `src/tool_metadata.rs` descriptions: `get_excerpt` (range aliases), `search_code` (path alias,
      glob semantics), `find_symbols` (substring semantics, `limit`)
- [ ] `narsil-plugin/skills/` (narsil-search, narsil): update parameter guidance to match
- [ ] CHANGELOG entry for the behavior changes (validation errors, default exclusions)

---

## Recommended Order

Sprint 1 (≈1 day, closes every observed symptom) → Sprint 2 (regression-proofing) → Sprint 3 → Sprint 4.

**Last updated:** 2026-08-21 — Sprints 1-2 complete: aliases + explicit ranges + find_symbols cap/ranking + mojibake repair; 9 new tests in tests/tool_ux_tests.rs, full suite green (1201 passed).
