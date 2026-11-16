# ZAL cheat sheet for agents

Write ZAL that passes `zal check` on the first try. Target: stage-0 compiler (`target/release/zal`).
`SPEC.md` is the authority. Complete programs below are files in `docs/agents-examples/`;
each one passes `zal check`. Blocks marked `fragment` are not standalone.

## 1. File skeleton

```text
fragment: first line of every file
module agents.research_step        // required, first line
import scheduler.dag               // optional: loads zal-src/scheduler/dag.zal
```

- `import a.b` loads `a/b.zal` from the file's directory, `./zal-src`, `$ZAL_PATH` or `--root`. The file must declare `module a.b`.
- One global namespace: declaring a name twice is an error. Comments are `//`.

Complete: `01_skeleton.zal`.

## 2. Types

| Type | Literals and notes |
|---|---|
| `Int` | `42`, `10_000`; 64-bit; `%`, `/` trap on zero |
| `Bool` | `true`, `false`; `&&` `\|\|` `!` |
| `Str` | `"a\n"`; concatenate with `+`; `len(s)`; `str(x)` converts |
| `Unit` | `()`; a missing return type means `Unit` |
| `Dur` | `250ms`, `5s`, `2min`, `1h`; `+ -`, comparison |
| `List<T>` | `[1, 2]`, `[]`; value semantics |
| `Map<T>` | keys are `Str`; `map_new()` |
| `Option<T>` | `Some(x)`, `None` |
| `Result<T, E>` | `Ok(x)`, `Err(e)` |
| `(A, B)` | tuple; `t.0`; `let (a, b) = t` |
| `Task<T>` | from `spawn`; do not name a record `Task` |
| record | `type Node = { id: Str, n: Int }`; `Node { id: "a", n: 1 }`; `node.id` |
| enum | `enum Mode { Fast, Safe(Int) }`; `Mode.Fast`, `Mode.Safe(3)` |

- Signatures are explicit. Empty `[]`, `map_new()`, `None` need context (`let mut m: Map<Int> = map_new()`).
- Bindings are immutable unless `let mut`. Reassign with `x = expr`, no `let`.

## 3. Declaring tools

```text
fragment
extern tool read_file(path: Str) -> Result<Str, Str> readonly
extern tool write_file(path: Str, body: Str) -> Result<Unit, Str>
```

- The kind is one of `tool net file secret worker deploy`. Calls need the effect `tool.read_file`.
- A tool's own failures are `Err` values of the declared `Result`. Infrastructure failure traps the run.
- `readonly` promises no side effects. Only then can the compiler run calls concurrently (section 8).
- Never call an extern from `fn`. Call it from a `task` or `workflow`.

Complete: `02_tools.zal`.

## 4. fn, task, workflow

| Kind | `requires` | Calls tools | `budget` |
|---|---|---|---|
| `fn` | forbidden | no | no |
| `task` | yes | yes | no |
| `workflow` | yes | yes | yes (entry point) |
| `test "name"` | optional | not callable | no |

```text
fragment
task copy_one(src: Str, dst: Str) -> Result<Int, Str>
    requires tool.read_file, tool.write_file
{
    let body = read_file(src)?
    write_file(dst, body)?
    Ok(len(body))
}
```

```text
fragment
workflow copy_all(a: Str, b: Str) -> Result<Int, Str>
    requires tool.read_file, tool.write_file
    budget { timeout: 2min, max_parallel: 2, max_tool_calls: 10 }
{
    let n1 = copy_one(a, "out/" + a)?
    Ok(n1)
}
```

- Effects are checked transitively: a caller must list every effect of every task it calls.
- `requires tool.*` covers every `tool` effect. Prefer explicit lists.
- `budget` keys: `timeout`, `max_parallel`, `max_tool_calls`; literal values only. `test` blocks run under `zal test` only.

Complete: `02_tools.zal`.

## 5. Error handling

- `e?` on `Result<T, E>`: yields `T`, or returns `Err(e)`. The enclosing fn must return `Result<_, E>` with the same `E`.
- `e?` on `Option<T>`: yields `T`, or returns `None`.
- `ensure cond, err`: returns `Err(err)` when `cond` is false.
- `assert cond, msg`: traps. Use in tests.
- `match` must be exhaustive for `Bool`, `Option`, `Result` and enums. Use `_ =>` otherwise.
- Patterns are one level deep: `Some(x)`, `Err(_)`, `Mode.Safe(n)`. Nested `Some(Ok(x))` is rejected; match twice.

```text
fragment
fn fetch_size(url: Str) -> Result<Int, Str> {
    let body = fetch(url)?
    ensure len(body) > 0, "empty body"
    Ok(len(body))
}

let size = match fetch_size(u) {
    Ok(n) => n
    Err(e) => 0
}
```

Complete: `01_skeleton.zal`.

## 6. Lists and maps

```text
fragment
let mut xs: List<Int> = []
xs = push(xs, 1)                  // push returns a new list: assign it
let first = xs[0]                 // traps when out of range
let maybe = get(xs, 0)            // Option<Int>
for x in xs { total = total + x }

let mut m: Map<Int> = map_new()
m = map_set(m, "k", 1)
let n = match map_get(m, "k") {   // map_get returns Option: always match it
    Some(v) => v
    None => 0
}
```

- Also: `len`, `set(xs, i, v)`, `remove(xs, i)`, `range(n)` (`0..n-1`), `map_has`, `map_keys`.
- Write `xs = set(xs, i, v)`; `xs[i] = v` is not valid syntax.

Complete: `03_collections.zal`.

## 7. Concurrency

| Construct | Type | Use |
|---|---|---|
| `parallel { a, b }` | `(A, B)` | run all, wait for all |
| `race { a, b }` | `T` | first finished wins, others cancelled |
| `timeout(5s) { body }` | `Option<T>` | `None` if the body is too slow |
| `retry(3, backoff: 250ms) { body }` | `Result<T, E>` | body must be a `Result`; re-run on `Err` |
| `spawn e` / `await t` | `Task<T>` / `T` | start now, collect later |
| `await_any(ts)` | `(Int, T)` | index and value of the first finished task |
| `cancel(t)`, `sleep(d)`, `join(ts)` | | cancel; wait; wait for a list of tasks |

- Blocks in `parallel`, `race`, `timeout`, `retry` and `spawn` capture outer variables read-only. Assigning to them is an error.
- `return` and `?` inside those blocks leave the block, not the function. That is how `retry` re-runs a failing step.
- Child tasks not awaited are cancelled when their parent finishes.

```text
fragment
let (x, y) = parallel {
    fetch(a)
    fetch(b)
}
let t1 = spawn fetch(a)
let r1 = await t1
let (k, first) = await_any(tasks)  // tasks: List<Task<Result<Str, Str>>>
```

Complete: `04_concurrency.zal`.

## 8. Automatic parallelization of readonly calls

Write sequentially. ZAL runs independent `readonly` calls concurrently.

- A run of consecutive `let x = ro(..)` or `let x = ro(..)?` whose arguments do not use a name bound earlier in the run.
- `for v in xs { acc = push(acc, ro(..)) }` (also with `?`) when the call does not use `acc`.
- Results, order and `?` semantics do not change. Non-`readonly` calls are never reordered.
- Caveat: if an earlier call fails, later calls of the same run may already have started. They count towards `max_tool_calls`.

Complete: `05_readonly_fanout.zal`.

## 9. Common mistakes

Messages are copied from `zal check` output, the wording of `compiler/src/check.rs` and `tests/golden/errors/*.zal`. The `file:line:` prefix is omitted.

| Compiler says | Cause | Fix |
|---|---|---|
| `calling `build` needs capability `tool.build`; add `requires tool.build` to `w`` | caller lacks the effect | add it to `requires` |
| `calling `inner` needs capability `tool.deploy`; add `requires tool.deploy` to `outer`` | callee's effect is not transitively declared | add the callee's effect to the caller |
| `calling `build` needs capability `tool.build`; `fn f` is pure; only a `task` or `workflow` may use effects` | tool call inside `fn` | make it a `task` |
| `` `fn f` is pure and cannot declare `requires`; make it a `task` `` | `requires` on `fn` | make it a `task` |
| `cannot assign to `x`; declare it with `let mut`` | reassigning an immutable binding | `let mut x = ...` |
| `cannot assign to `n` (variables captured by a concurrent block are read-only copies)` | assigning a captured variable in `parallel`/`spawn`/etc. | return the value from the block |
| `` `break`/`continue` outside a loop (loops do not cross concurrent blocks) `` | `break` in a block | move the loop inside the block |
| `only a `workflow` can declare a budget` | `budget` on `task` | move it to a `workflow` |
| `unknown effect kind in `requires proxmox.root`` | kind not in the list in section 3 | use a valid kind |
| `match on `Color` is not exhaustive; add the missing cases or `_ =>`` | missing arm | add the arm or `_ =>` |
| `return value of `f`: expected `Int`, found `Str`` | type mismatch | fix the value or signature |
| `` `?` needs the enclosing function to return Result with the same error type: expected `Int`, found `Str` `` | `?` into a different `E` | change `E` or map the error |
| `` a `retry` body must produce a Result: expected `Result<?1, ?2>`, found `Int` `` | `retry` body is not a `Result` | return a `Result` from the body |
| `operands: expected `Option<Int>`, found `Int`` | `map_get` used as a value | `match` the `Option` |
| `` `List<Int>` has no fields `` | `xs.len` | use `len(xs)` |
| `record `A` has no field `z`` | typo or wrong record | fix the field name |
| `let binding: expected `Int`, found `Str`` | annotation mismatch | fix the annotation or value |
| `return value of `f`: expected `Int`, found `Unit`` | `if` without `else` used as the value (type `Unit`) | add `else` |
| `expected `)`, found `(`` | nested pattern `Some(Ok(x))` | match twice |
| `expected an expression, found `=`` | `xs[0] = 3` | `xs = set(xs, 0, 3)` |
| `expected an expression, found keyword `mut`` | `mut x = 1` | `let mut x = 1` |
| *(no error)* | `push(xs, 1)` without assignment: the list is discarded | `xs = push(xs, 1)` |

## 10. Pre-submit checklist

1. `module` is the first line.
2. Every tool and task called is in `requires` (transitively).
3. No `requires`, tool call or budget in `fn`.
4. Reassigned variables are `let mut`. No `xs[i] = v`.
5. `?` only inside functions returning `Result` with the same `E`.
6. Every `match` is exhaustive. Every `map_get`/`get` result is matched.
7. Run `zal check file.zal`.

## 11. Worked example

A research step: three readonly reads, a readonly grep, a summary through a tool. Each tool is declared, the workflow lists them in `requires`, and the budget bounds the run. The three reads are written one after another and run concurrently (section 8).

```zal
module agents.research_step

// A small research step: read three sources, grep them, summarise via a tool.
type Summary = { text: Str, sources: Int }

extern tool read_file(path: Str) -> Result<Str, Str> readonly
extern tool grep(pattern: Str, text: Str) -> Result<Str, Str> readonly
extern tool summarise(text: Str) -> Result<Str, Str>

workflow research_step(a: Str, b: Str, c: Str, pattern: Str) -> Result<Summary, Str>
    requires tool.read_file, tool.grep, tool.summarise
    budget { timeout: 5min, max_parallel: 4, max_tool_calls: 20 }
{
    // The three reads are independent and readonly: ZAL runs them concurrently.
    let fa = read_file(a)?
    let fb = read_file(b)?
    let fc = read_file(c)?
    let hits = grep(pattern, fa + "\n" + fb + "\n" + fc)?
    ensure len(hits) > 0, "no matches for " + pattern
    let text = summarise(hits)?
    Ok(Summary { text: text, sources: 3 })
}
```

File: `docs/agents-examples/06_research_step.zal` (`zal check` passes).

> Since 2026-11-08: `return`, `break` and `continue` may stand directly as a
> `match` arm (`Err(e) => return Err(e),`); wrapping them in `{ }` is no longer needed.
