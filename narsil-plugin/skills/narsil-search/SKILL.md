---
name: narsil-search
description: Choose the right narsil-mcp search tool and read its results honestly. Use when locating code by name, string, or description - "where is X defined", "find the function that does Y", "search for this string", "is there code like this snippet", "which file handles Z" - across an indexed repository.
allowed-tools: mcp__narsil-mcp__*
---

# Finding code with narsil

## Before the first search

Run `list_repos` once per session and **pass `repo` to every search call**. Without
it every search tool spans all indexed repositories, and `max_results` is spent on
a corpus-wide top-k instead of on the codebase you meant. An unknown `repo` name
comes back as an error, not as "no results", so a typo is visible.

Parameter names are short: `repo`, `symbol`, `path`, `function`, `query`. The
`repo` value is the **name** from `list_repos`, never a filesystem path.

## Pick by what you know, cheapest first

| What you have | Tool | Notes |
|---|---|---|
| Exact identifier | `find_symbols` | Add `symbol_type` (`function`, `class`, `struct`) to cut noise |
| Identifier you may be misspelling | `workspace_symbol_search` | Fuzzy; use when `find_symbols` returns nothing |
| Exact string, error text, config key | `search_code` | Literal/regex; the only honest tool for "this exact text" |
| A sentence describing behaviour | `hybrid_search` | BM25 + TF-IDF; the default for natural language |
| A description whose wording differs from the code's | `neural_search` | Embeddings; needs `--neural`. Best when the code says `Проверить`, you say "validate" |
| A code snippet | `find_similar_code` | Pass the snippet, not a description |
| A known function to find look-alikes of | `find_similar_to_symbol` | Refactoring candidates |
| Need function/class boundaries, not lines | `search_chunks` | AST-aware chunks |
| A file whose name you know | `find_symbols` with `file_pattern` | Cheaper than any search |

`semantic_search` is BM25 with code-aware tokenisation - a better `search_code`,
**not** an embedding search. Only `neural_search` and `find_semantic_clones` use
embeddings, and only when the server runs with `--neural`.

## Reading results

A search hit is a pointer, not evidence. Before you state what the code does,
open it: `get_excerpt` for the lines, `get_symbol_definition` for the whole
function. Quote `path:line` so the claim can be checked.

Similarity scores rank; they do not verify. A 0.77 neural hit and a 0.42 hit are
both unread until you read them.

## Anti-patterns

| Don't | Do |
|---|---|
| Fire `search_code` + `hybrid_search` + `neural_search` at the same query | Pick by the table, escalate only if the first returns nothing useful |
| Use `neural_search` for an exact identifier | `find_symbols` - embeddings are worse at exact names |
| Report "the function that does X is `foo`" from a hit list | Open it with `get_symbol_definition` first |
| Re-run the same query with `max_results` raised after 0 hits | 0 hits means the wording is wrong, not that the limit was |
| Omit `repo` because "there is only one" | Pass it anyway; index contents change between sessions |

## Stop rule

Two searches with genuinely different phrasings and no useful hit means the query
strategy is wrong, not the tool. Switch to structure: `get_project_structure`,
`find_symbols` by type, or `get_import_graph` to find the owning module - then
read. Do not loop through synonyms.

For the full tool catalogue see the `narsil` skill.
