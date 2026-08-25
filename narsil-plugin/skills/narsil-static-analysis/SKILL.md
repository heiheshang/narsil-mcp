---
name: narsil-static-analysis
description: Use narsil-mcp flow and type analysis - control flow, data flow, reaching definitions, dead code, dead stores, uninitialised variables, type inference, import graphs and cycles. Use when reasoning about how a value moves through a function, whether a branch or variable is reachable, or how modules depend on each other.
allowed-tools: mcp__narsil-mcp__*
---

# Static analysis with narsil

## Two scopes, two ladders

**Inside one function** - you must know the exact function first (`find_symbols`,
or the call-graph key from `get_callers`):

| Question | Tool |
|---|---|
| Branches, loops, basic blocks | `get_control_flow` |
| Where does this value come from / go? | `get_data_flow` |
| Which assignment reaches this use? | `get_reaching_definitions` |
| Values written and never read | `find_dead_stores` |
| Used before assignment | `find_uninitialized` |
| Types of locals (Python/JS/TS) | `infer_types`, `check_type_errors` |

**Across the repo** - sweeps that produce candidate lists:

| Question | Tool |
|---|---|
| Unreachable code | `find_dead_code` |
| Exports nobody imports | `find_unused_exports` |
| Module structure | `get_import_graph` |
| Cycles | `find_circular_imports` |

## Sweeps produce candidates, not verdicts

`find_dead_code` and `find_unused_exports` see static references only. Entry
points invoked by a framework, a platform, reflection, DI containers, tests, or
config-named handlers look identical to dead code. Before saying "unused":

1. `find_references` / `find_symbol_usages` on the symbol.
2. A literal `search_code` for the name as a **string** - config, templates,
   route tables, and 1C metadata reference handlers by name.
3. `get_callers` (see the `narsil-callgraph` skill for its caveats).

Only then, and say what you checked.

## Reading flow output

Flow tools answer about the function you named, in the file you named. They do
not follow calls: a value that leaves through a callee is where `get_data_flow`
stops and `get_callees` starts. When you need the chain, alternate the two and
say where the automated part ended.

Type inference on dynamic languages is best-effort. Quote inferred types as
inference, not as declarations.

## Anti-patterns

| Don't | Do |
|---|---|
| Run `find_dead_code` and open a deletion PR | Cross-check each candidate, then propose |
| Ask for a CFG of a whole file | These tools are per function - name it |
| Present inferred types as the language's own | Mark them as inferred |
| Use `get_data_flow` to answer a cross-function question | Combine with `get_callees` and say where the boundary was |
