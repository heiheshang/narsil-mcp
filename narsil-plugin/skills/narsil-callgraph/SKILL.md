---
name: narsil-callgraph
description: Answer "who calls this / what does this call / can A reach B" with narsil-mcp's call graph, and report the answer with the confidence the graph actually has. Use for impact analysis before changing a function, tracing an execution path, finding entry points, or judging whether code is reachable.
allowed-tools: mcp__narsil-mcp__*
---

# Call graph with narsil

Requires the server to run with `--call-graph`. If `get_callers` is not in the
tool list at all, the flag is missing or the preset hides it - see the
`narsil-repo-state` skill before concluding anything about the code.

## Ask for the method you mean

Nodes are keyed `file::name` for free functions and `file::Type::name` for
methods. Three query forms, in order of preference:

| Form | Example | Use when |
|---|---|---|
| Full key | `internal/api/api.go::Handler::Get` | You already saw the key in earlier output - exact, no ambiguity |
| Type-qualified | `Handler.Get`, `Handler::Get` | You know the type - resolves against the receiver |
| Bare name | `Get` | Last resort; the answer covers **one** namesake out of many |

A bare name on a large codebase is rarely a question worth asking: 1C corpora
routinely have hundreds of identically named handlers.

## Read the header before the list

```
Resolved to `internal/app/scheduler_app.go::GoSchedulerApplication::Run` (on `GoSchedulerApplication`)
> Ambiguous name: 107 functions are named `Run`; reporting on ... only.
```

- **`Resolved to X (on Y)`** - the single node the answer is about. If it is not
  the one you meant, re-ask with the full key.
- **`Ambiguous name: N`** - the other N-1 namesakes were *not* covered. Never
  paraphrase this as "N callers of Run".
- **`*Function ... is not in the call graph.*`** - the query never resolved. This
  is not "no callers"; fix the name and re-ask.

## Read the per-edge caveats

Each entry says what its target was matched on:

| Marker | Meaning | What you may claim |
|---|---|---|
| *(no marker)* | Unique name, receiver type, or module scope matched | A fact |
| `receiver inferred from a naming convention` | The call was chained onto a factory or constructor (`XFactory(c).M()`, `NewX().M()`) and exactly one namesake is defined on type `X`; in BSL, `МодульX.M()` went to common module `X` | Strong candidate - the type came from a naming convention, not from the call site |
| `single implementation of the interface` | The receiver's type is an interface, and exactly one type in the index carries every method it declares | Strong candidate - dispatch is dynamic, another implementation could be injected |
| `same-file match` | Namesakes exist; the one in the caller's file was taken | Likely, say so |
| `name match only` | Namesakes exist and nothing distinguished them | A candidate - verify before acting |
| `not in graph, outside the index` | The import list says the target is not this repo's code: `assert.Len`, or a value declared as a foreign type (`wg.Add`) | Not project code; noise by construction, not a gap |
| `not in graph, indexed package has no such function` | The qualifier named a package this index **does** carry, which has no such function: a method reached through a value, or a file the index skipped | The one "missing" kind worth chasing - something was not read |
| `not in graph` | Nothing at the call site explains it: stdlib or builtin names, macros, a Go type conversion (`account_model.AccountUuid(x)`), which is no call at all | Not project code; usually noise |

The footer counts them (`> 12 of 23 targets ... matched by name alone`). Carry
that number into your answer. "117 callers, all matched by name" is a different
statement from "117 callers", and only the first is true.

Verify a `name match only` edge that matters: `get_excerpt` at its `file:line`,
or `find_references` on the symbol.

## Tool per question

| Question | Tool |
|---|---|
| Who calls this? | `get_callers` (`transitive=true`, `max_depth` for reach) |
| What does this call? | `get_callees` |
| Can A reach B? | `find_call_path` |
| Neighbourhood of a function | `get_call_graph` with `depth` |
| Which functions are over-connected? | `get_function_hotspots` |
| How complex is it? | `get_complexity` |

`exclude_tests` is accepted but **ignored** by `get_call_graph`, `get_callers`,
`get_callees` and `get_function_hotspots` - filtering it would need the graph
rebuilt. Filter test callers yourself by path.

## What the graph cannot do

Resolution is by name plus whatever the call site or its function can be read
to say about the receiver's type. There is no flow analysis and no real type
checker, so what is covered is what is written nearby:

| Call site | How it resolves |
|---|---|
| `self.Logger()` / `this.save()` | On the enclosing method's own receiver type - exact, `Receiver` |
| `self.HandleError(err)` where the method sits on an embedded type | Promoted from the embedded type, `Receiver` - a rule of Go, not a guess |
| `svc.Create(dto)` where `svc` is a parameter, `var svc T`, `svc := NewT()`, `svc := &T{}`, or `svc, err := build()` | On the declared type, `Receiver`; a package-level `var` counts too |
| `accountRepo(ctx).FindByUuid(id)` | On the declared **result type** of the inner call, `Receiver`; when the inner call is one of several namesakes, only if they all declare the same result type |
| `self.C.Logger().SendError(err)` | Field type from the struct declaration, then the method's result type - from the interface declaration when the field is an interface |
| `XFactory(c).M()`, `NewX().M()` | On type `X` from the factory/constructor name - `receiver inferred from a naming convention`; fires for `Factory`/`Provider`/`Builder` suffixes and `New`/`Make` prefixes, snake_case included |
| `МодульX.Процедура()` (BSL, `МодульX = ОбщегоНазначения.ОбщийМодуль("X")`) | Common module `X`, when it has the procedure - `receiver inferred from a naming convention` |
| `Структура.Вставить(...)`, `Таблица.ВыгрузитьКолонку(...)` (BSL, receiver names no module) | A platform method: **no target** (`not in graph`); other names are `name match only` - a configuration procedure is reached only through its module's name or its object |
| `self.Provider.Deliver(to)` where the field is an interface | The one indexed type carrying every method the interface declares - `single implementation of the interface`; a mock in the test tree loses to production code |
| `config.Get()` where `config` is an imported package of this repo | Narrowed to that package's own directory (not its subpackages), package-level functions only - `Scope` |
| `assert.Len(t, x, 3)`, `err.Error()`, `wg.Add(1)` - a third-party package, or a name declared as a builtin, container or foreign type | **No target at all** - `not in graph, outside the index` (or plain `not in graph` for a builtin), instead of some namesake of this repo |

`get_index_status` reports the mix (`## Call Edge Resolution`), which is how you
tell a corpus that resolves well from one that mostly guesses. The three
`not in graph` rows are counted apart on purpose: a Go corpus routinely sends
around half its edges out of the graph into the standard library and into
dependencies nobody indexed, so that figure alone says nothing about quality.
Read `indexed package, no such function` instead - that is the row that grows
when something was genuinely not read.

Still unresolved, and still needing a manual check:

- an interface with several implementations in the index (the graph will not
  pick one), or one implemented outside it;
- a value out of a map, a slice element, a channel, or a multi-value assignment
  where more than one name carries a meaningful value;
- a variable reassigned to a different type between declaration and call - the
  reading is shallow: no flow analysis, last declaration wins;
- reflection, callbacks stored in variables, and platform-invoked handlers. A
  BSL handler such as `ПриДобавленииОбработчиковОбновления` legitimately shows
  zero callers - the platform calls it, not the code.

All of the above is read from Go declarations. Other languages keep the
`self`/`this`, factory-name and same-file resolution, but not the field, result
type or interface steps.

Therefore: **"0 callers" is never proof that code is dead.** Cross-check with
`find_references`, `find_symbol_usages`, and a plain string search of the name
before deleting anything.

## Anti-patterns

| Don't | Do |
|---|---|
| Report the caller count without the name-matched count | Quote both |
| Treat `Resolved to` as "the only function with that name" | Read the ambiguity line |
| Conclude "unused" from an empty caller list | Cross-check references, then check for dynamic entry points |
| Query bare names on a big corpus and act on the first answer | Qualify with the type or the full key |
