---
name: narsil-search
description: Choose the right narsil-mcp search tool and read its results honestly. Use when locating code by name, string, or description - "where is X defined", "find the function that does Y", "search for this string", "is there code like this snippet", "which file handles Z" - across an indexed repository.
allowed-tools: mcp__narsil-mcp__*
---

# Finding code with narsil

## Before the first search

Run `list_repos` **on every narsil server you can reach**, not once per session,
and **pass `repo` to every search call**. Repository names are per server: one
server may index a repo root as `go-rest-api` while another was pointed at its
subdirectories and calls them `internal` and `cmd`. A name that works against
`narsil-mcp` can come back `Repository not found` from `narsil-neural` while the
same code is indexed on both.

Without `repo`, every search tool spans all indexed repositories, and
`max_results` is spent on a corpus-wide top-k instead of on the codebase you
meant. An unknown `repo` name comes back as an error, not as "no results", so a
typo is visible.

Parameter names are short: `repo`, `symbol`, `path`, `function`, `query`. The
`repo` value is the **name** from `list_repos`, never a filesystem path.

Two indexed paths whose last component is the same — a git worktree at
`worktree/<branch>/go-rest-api` alongside the primary `go-rest-api` — arrive as
two repositories competing for one name. `list_repos` is the only place that
shows how the server resolved that, so check it before trusting which checkout
your `repo` value points at.

## Pick by what you know, cheapest first

| What you have | Tool | Notes |
|---|---|---|
| Exact identifier | `find_symbols` | Add `symbol_type` to cut noise: `function`, `method`, `struct`, `class`, `interface`, `enum`, `trait`, `type` (aliases only), `field`, `test`. Go and Rust declare data as `struct`/`interface`, not `class` |
| Identifier you may be misspelling | `workspace_symbol_search` | Fuzzy; use when `find_symbols` returns nothing |
| Exact string, error text, config key | `search_code` | Case-insensitive **substring**, no regex. Ranked and truncated - see below |
| Where an identifier is *used*, not defined | `find_symbol_usages`, `find_references` | The right tool for call sites; `search_code` ranks definitions above them |
| A sentence describing behaviour | `hybrid_search` | BM25 + TF-IDF; the default for natural language |
| A description whose wording differs from the code's | `neural_search` | Embeddings; needs `--neural`. Best when the code says `Проверить`, you say "validate" |
| A code snippet | `find_similar_code` | Pass the snippet, not a description |
| A known function to find look-alikes of | `find_similar_to_symbol` | Refactoring candidates |
| Need function/class boundaries, not lines | `search_chunks` | AST-aware chunks |
| A file whose name you know | `find_symbols` with `file_pattern` | Cheaper than any search |
| A named test case | `find_symbols` with `symbol_type: "test"` | Go `t.Run("...")`, JS/TS `describe`/`it`/`test`; the line range covers the whole case block |

## One name per lookup, not a phrase

`find_symbols` matches its `query` against symbol names as **one** name or
pattern. Two identifiers in one string match nothing: `"BeneficiaryService
CreateDestination"` returns zero while `NewBeneficiaryService` and
`CreateDestination` are both indexed. `search_code` has the same shape for a
different reason — its substring includes the spaces, so a multi-word query only
hits a line carrying that exact phrase. `"BackfillLegacy stage execution"`
returns nothing in a corpus where `backfill_legacy` returns twenty results.

Neither is an AND over terms. When you have several words, either search the one
distinctive token, or hand the sentence to `hybrid_search` / `neural_search`,
which are built for wording rather than for names.

## `search_code` is a ranked grep, not a grep

It matches `line.to_lowercase().contains(query)` - a plain substring, so a regex
you pass is searched for literally. Every match is then scored and the list is
**truncated to `max_results` (default 10)** before you see it. The scoring gives

- `+3.0` for a whole-word match,
- `+1.5` for a line containing `fn ` / `def ` / `func ` / `class ` / `struct `,
- `+1.0` for a short line.

That middle bonus is the trap: **declarations systematically outrank call
sites**. Ask `search_code` for a popular identifier and you get its definitions,
its docs and its mocks, while the one call you cared about sits below the cut.

The header `Found N results` is printed **after** truncation. N is what was
shown, never what matched.

So, for "where is `Foo` called":

1. `find_symbol_usages` / `find_references` - they answer the question directly;
2. failing that, `search_code` with `file_pattern` narrowed to the owning module,
   which removes the competing files instead of out-ranking them;
3. and only then `search_code` with `max_results` raised.

`semantic_search` is BM25 with code-aware tokenisation - a better `search_code`,
**not** an embedding search. Only `neural_search` and `find_semantic_clones` use
embeddings, and only when the server runs with `--neural`.

## Reading results

A search hit is a pointer, not evidence. Before you state what the code does,
open it: `get_excerpt` for the lines, `get_symbol_definition` for the whole
function. Quote `path:line` so the claim can be checked.

Similarity scores rank; they do not verify. A 0.77 neural hit and a 0.42 hit are
both unread until you read them.

`find_symbols` prints `file:start-end` (`file:line` for a one-line symbol). That
end line is the symbol's last line —
feed the pair straight to `get_file(start_line, end_line)` or `get_excerpt` when
you want a slice of it. Never shell out to `sed`/`head` for a range narsil
already handed you.

Symbols owned by a type print qualified: `PaymentStore::Save`, `Operation::ID`
(Go receiver, Rust `impl` type, class name). Two entries with the same short name
and different owners are different symbols. The qualified form works as a
`pattern` and as a `symbol` argument.

## Reading a large symbol

`get_symbol_definition` returns the whole body by default, which is the wrong
default for a 400-line function. Narrow it in one call instead of paging blindly:

| You want | Argument | Result |
|---|---|---|
| The shape of the function | `outline: true` | Signature plus `if`/`switch`/`case`/`for`/`return` lines, gaps marked `⋯ N line(s)` |
| The part that mentions X | `contains: "rail"` | Only body lines matching that case-insensitive regex, plus the signature |
| A slice of the body | `offset`, `max_lines` | Page through it; a footer says how many lines were hidden |

The normal route into a long function is `outline: true` to find the region, then
`get_file` with the line range it showed. `contains` is a grep scoped to one
symbol — use it instead of `search_code` when you already know which function
you are in.

## Namesakes

`get_symbol_definition` reports every other definition of the name it answered
with (`**Also defined**: ...`). Do not ignore that line: it means the body you
are reading may not be the one you meant.

Pick one with `receiver: "AuditStore"` (the owning type) or
`file: "payment/store.go"` (a path suffix is enough). A filter that matches
nothing is an error listing the candidates, not a silent fallback to another
symbol.

A name that is not indexed at all comes back with the closest matches — a
different case, a wrong qualifier, a substring — so a failed lookup usually
answers itself without a second call.

## Anti-patterns

| Don't | Do |
|---|---|
| Fire `search_code` + `hybrid_search` + `neural_search` at the same query | Pick by the table, escalate only if the first returns nothing useful |
| Use `neural_search` for an exact identifier | `find_symbols` - embeddings are worse at exact names |
| Report "the function that does X is `foo`" from a hit list | Open it with `get_symbol_definition` first |
| Read a 400-line function whole to find one branch | `outline: true`, then `get_file` on the range it points at |
| Grep a file for a test case's boundaries | `find_symbols symbol_type: "test"` — the case block is a symbol with a line range |
| Trust `get_symbol_definition` for an ambiguous name | Read the `**Also defined**` line; re-ask with `receiver` or `file` |
| Re-run the same query with `max_results` raised after **0** hits | 0 hits means the wording is wrong, not that the limit was. A *wrong-looking* top-10 is the opposite case: there raising the limit, or narrowing `file_pattern`, is correct |
| Hunt call sites with `search_code` | `find_symbol_usages` / `find_references`; `search_code` ranks `func Foo(` above `foo.Foo(x)` |
| Read `Found N results` as "there are N matches" | It is the post-truncation count; N == `max_results` means the list was cut |
| Omit `repo` because "there is only one" | Pass it anyway; index contents change between sessions |
| Put two identifiers, or a sentence, into `find_symbols` / `search_code` | One name or one distinctive token per call; a phrase goes to `hybrid_search` / `neural_search` |
| Conclude "the tests do not cover this" from an empty search | Test files and test helpers are indexed like any source; only `vendor/`-class paths are excluded by default. Check the wording, then the checkout (`narsil-repo-state`) |
| Say "this code does not exist" after searching the wrong checkout | A worktree or second clone is invisible unless its path is in `--repos`; `list_repos` shows what is actually there |

## Stop rule

Two searches with genuinely different phrasings and no useful hit means the query
strategy is wrong, not the tool. Switch to structure: `get_project_structure`,
`find_symbols` by type, or `get_import_graph` to find the owning module - then
read. Do not loop through synonyms.

For the full tool catalogue see the `narsil` skill.
