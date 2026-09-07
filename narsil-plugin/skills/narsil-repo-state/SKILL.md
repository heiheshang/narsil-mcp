---
name: narsil-repo-state
description: Answer history questions with narsil-mcp git tools, and diagnose why a narsil tool is missing, empty, or stale - feature flags, presets, index freshness. Use when asked who changed something and when, which files churn, or when a narsil call returns nothing and you need to tell "no such code" apart from "not indexed".
allowed-tools: mcp__narsil-mcp__*
---

# Repository state, history and diagnostics

## History (needs `--git`)

| Question | Tool |
|---|---|
| Who wrote this line, when | `get_blame` |
| How did this file evolve | `get_file_history` |
| What changed lately | `get_recent_changes` |
| How did this function evolve | `get_symbol_history` |
| Which files churn most | `get_hotspots` |
| Who knows this area | `get_contributors` |
| What is in this commit | `get_commit_diff` |
| What is uncommitted right now | `get_modified_files` |
| Which branch is indexed | `get_branch_info` |

History answers *why*, not *what*: pair `get_blame` with the commit message via
`get_commit_diff` before attributing intent to an author. `get_hotspots` ranks by
churn, which is a question about process, not quality - do not call a file bad
because it changes often.

## Before believing an empty answer

An empty narsil result has five quite different causes. Check in this order:

0. **Is the server connected?** A narsil server that failed to start reports
   `CONNECTION_CLOSED` at session start and contributes **no tools at all** -
   its absence looks exactly like a feature being off. Say the server is down;
   do not report on code you could not query. When several narsil servers are
   configured, this is per server: one may be up while another is not.
1. **`get_index_status`** - shows enabled features and document counts. Git
   tools need `--git`; `get_callers`/`get_callees`/`get_call_graph` need
   `--call-graph`; `neural_search` and `find_semantic_clones` need `--neural`;
   `sparql_query` and the CCG exports need `--graph`; `get_type_info` needs
   `--lsp` (`get_hover_info` and `go_to_definition` work without it, just less
   precisely); the remote-repo tools need `--remote`.
2. **Is the tool even offered?** A missing tool is a configuration fact, not a
   code fact. The preset filters the list: the server maps the MCP client name to
   a preset, and `cursor` maps to *balanced*, which blacklists `neural_search` and
   `find_semantic_clones` outright and caps the tool count. Force it with
   `--preset full` on the server command line, `NARSIL_PRESET=full` in its
   environment, or `preset: "full"` in `~/.config/narsil-mcp/config.yaml`. A
   per-tool `enabled: true` override does **not** defeat a preset blacklist.
   The preset is read once at process start, so this needs a restart.
3. **Is the index current?** `get_incremental_status` and the repo's file count
   in `get_index_status`. Without `--watch`, files added after startup are
   invisible until a restart or `reindex`. `reindex` re-walks the corpus - on a
   large repository that is minutes and gigabytes, so ask before triggering it in
   someone's live server.

4. **Was it indexed by an older narsil?** A persisted index keeps whatever the
   parser recorded when the files were last read. Symbol kinds and qualified
   names come from that snapshot: on an index built before those were added, Go
   structs still report `TypeAlias`, `symbol_type: "struct"` or `"test"` finds
   nothing, and `get_symbol_definition`'s `receiver` filter has nothing to match.
   The fix is a `reindex` of that repository - same cost caveat as above, so ask
   first - and the symptom is a whole category coming back empty rather than one
   symbol.

5. **Which checkout was indexed, and is your file inside it?** `--repos` lists
   specific paths, and a second checkout of the same project — a git worktree, a
   release clone — is a different path. Code you just wrote in
   `worktree/<branch>/go-rest-api` is invisible to a server started on the
   primary checkout, however fresh its index is, and `--watch` will never see it
   either. Adding the path needs a restart, and a worktree whose last directory
   component repeats the primary's produces two repositories contending for one
   name (see `narsil-search`).

   The complement of this is worth stating too, because it is the more common
   wrong conclusion: **tests are indexed**. Test files and test-helper packages
   are ordinary sources to the indexer; only the built-in exclusions
   (`vendor/`, `node_modules/`, lock and minified files) are missing, and
   `--exclude '!**/vendor/**'` re-includes even those. Compare the repo's file
   count in `get_index_status` against `find . -name '*.ext' | wc -l` before
   claiming a directory is out of the index — an empty result about test code is
   almost always the query's shape, not its coverage.

Say which of the six you checked. "No results for X in repo Y, call graph
enabled, index has 97k files" is an answer; "the tool returned nothing" is not.

## Multi-repo and remote

`list_repos` first, and pass `repo` everywhere - see the `narsil-search` skill.
Run it **per server**: repository names are assigned by whatever paths that
server was started with, so the same codebase can be `go-rest-api` on one and
`internal` / `cmd` on another indexed by subdirectory. Mismatched names surface
as `Repository not found`, which is a configuration bug on the server that was
pointed below the repo root - not a reason to drop the `repo` filter.
`validate_repo` and `discover_repos` resolve path questions before indexing.
Remote GitHub repos (`add_remote_repo`, `get_remote_file`) need `--remote` and a
`GITHUB_TOKEN`.

## Anti-patterns

| Don't | Do |
|---|---|
| Report "this code does not exist" from one empty call | Check flags, preset and index freshness first |
| Trigger `reindex` to "make sure" | It is expensive; confirm staleness, then ask |
| Blame an author from `get_blame` alone | Read the commit that introduced the line |
| Assume the tool list you saw last session still holds | Presets and flags change per server; `get_index_status` is cheap |
| Treat a dead server's silence as a fact about the code | Name the server that failed to connect, and scope the answer to what you could actually query |
| Reuse a repo name across narsil servers | `list_repos` on each; names are per server |
| Search for code you wrote in a worktree the server was not started on | Check the indexed paths first; a checkout outside `--repos` is not stale, it is absent |
| Report a directory (tests, fixtures) as "not indexed" from empty hits | Confirm against `get_index_status` file counts; only the built-in exclusions are actually missing |
