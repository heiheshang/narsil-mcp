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

An empty narsil result has three quite different causes. Check in this order:

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

Say which of the three you checked. "No results for X in repo Y, call graph
enabled, index has 97k files" is an answer; "the tool returned nothing" is not.

## Multi-repo and remote

`list_repos` first, and pass `repo` everywhere - see the `narsil-search` skill.
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
