# ZAL Language Specification — v0.1 (stage-0)

Status: implemented by the stage-0 compiler in `compiler/` and the VM in `vm/`.
Everything in this document is covered by the test suite (`cargo test`) unless
marked *reserved*. Nothing here describes features that do not exist yet.

## 1. Source files and modules

A source file has the extension `.zal`, is UTF-8, and starts with a module header:

```zal
module scheduler.dag
import core.util          // loads <root>/core/util.zal
```

`import a.b.c` loads `a/b/c.zal` from the first import root that has it (the
importing file's directory, `./zal-src`, `$ZAL_PATH`, or `--root`). The loaded
file must declare `module a.b.c`. In v0.1 all top-level names share one global
namespace; declaring a name twice is an error. Qualified access (`dag.Node`) is
reserved for v0.2.

Comments start with `//` and run to the end of the line.

## 2. Lexical structure

- Identifiers: letter or `_`, then letters, digits, `_`.
- Integers: decimal, `_` allowed as separator (`10_000`). 64-bit signed.
- Durations: an integer immediately followed by `ms`, `s`, `min` or `h` (`250ms`, `30min`).
- Strings: `"..."` with escapes `\n \t \" \\`.
- Keywords: `module import type enum fn task workflow extern requires budget let mut
  if else while for in return match parallel race timeout retry spawn await ensure
  true false assert break continue`. `test` is contextual (only before a string at
  the top level), so a tool may be called `test`.

Newlines are not tokens, but a call `f(...)`, an index `x[...]` and a binary `-`
do not continue across a line break. `a\n(b)` is two statements.

## 3. Grammar (EBNF)

```ebnf
file      = "module" dotted { "import" dotted } { item } ;
item      = record | enum | extern | func | test ;
record    = "type" IDENT "=" "{" [ field { "," field } [","] ] "}" ;
field     = IDENT ":" type ;
enum      = "enum" IDENT "{" [ variant { "," variant } [","] ] "}" ;
variant   = IDENT [ "(" type ")" ] ;
extern    = "extern" KIND IDENT params "->" type ;          (* KIND: tool net file secret worker deploy *)
func      = ("fn" | "task" | "workflow") IDENT params [ "->" type ] { clause } block ;
test      = "test" STRING { clause } block ;
clause    = "requires" effect { "," effect }
          | "budget" "{" [ IDENT ":" expr { "," IDENT ":" expr } ] "}" ;
effect    = KIND "." ( IDENT | "*" ) ;
params    = "(" [ IDENT ":" type { "," IDENT ":" type } ] ")" ;
type      = IDENT [ "<" type { "," type } ">" ] | "(" [ type { "," type } ] ")" ;

block     = "{" { stmt } [ expr ] "}" ;
stmt      = "let" ["mut"] ( IDENT | "(" IDENT { "," IDENT } ")" ) [ ":" type ] "=" expr
          | IDENT "=" expr
          | "return" [ expr ]
          | "while" expr block
          | "for" IDENT "in" expr block
          | "ensure" expr "," expr
          | "assert" expr [ "," expr ]
          | "break" | "continue"
          | expr ;
expr      = binary ;                                        (* precedence below *)
unary     = ( "-" | "!" | "await" | "spawn" ) unary | postfix ;
postfix   = primary { "?" | "." IDENT | "." INT | "[" expr "]" } ;
primary   = INT | DUR | STRING | "true" | "false" | "(" ")" | "(" expr ")"
          | "(" expr "," expr { "," expr } ")" | "[" [ expr { "," expr } ] "]"
          | block | if | match
          | "parallel" "{" expr { [","] expr } "}"
          | "race" "{" expr { [","] expr } "}"
          | "timeout" "(" expr ")" block
          | "retry" "(" expr [ "," "backoff" ":" expr ] ")" block
          | path [ "(" args ")" ] | UPPER_IDENT "{" fieldinit { "," fieldinit } "}" ;
if        = "if" expr block [ "else" ( if | block ) ] ;
match     = "match" expr "{" { pattern "=>" expr [","] } "}" ;
pattern   = "_" | IDENT | INT | "-" INT | STRING | "true" | "false"
          | UPPER_PATH [ "(" IDENT ")" ] ;
```

Binary precedence, lowest first: `||`, `&&`, `== !=`, `< <= > >=`, `+ -`, `* / %`.
All are left-associative. In the head of `if`, `while`, `for` and `match`,
`Name {` starts the body, not a record literal; wrap the literal in parentheses.
Expressions, blocks, types and patterns nest at most 512 levels deep; deeper
nesting is a compile error.

## 4. Types

| Type | Values |
|---|---|
| `Int` | 64-bit signed; overflow and division by zero trap |
| `Bool`, `Str`, `Unit` | `Unit` is written `()` |
| `Dur` | milliseconds; `+ -` and comparison |
| `List<T>`, `Map<T>` | value semantics; `Map` keys are `Str`, iteration is key order |
| `Option<T>` | `Some(x)`, `None` |
| `Result<T, E>` | `Ok(x)`, `Err(e)` |
| `(A, B, ...)` | tuples, `.0`, `.1`, destructured by `let (a, b) = ...` |
| `Task<T>` | handle of a concurrently running computation |
| records, enums | nominal; declared with `type` and `enum` |

Type inference is local (unification). Function signatures are explicit; a
missing return type means `Unit`. Empty containers and `None` take their type
from context; where nothing constrains them, annotate (`let m: Map<Int> = map_new()`).

All values are immutable. `List`, `Map` and records are shared copy-on-write;
`x = push(x, v)` (also `set`, `remove`, `map_set`) updates in place when `x`
holds the only reference, which the compiler arranges by moving `x` into the call.

## 5. Functions and effects

There are three callable kinds and tests:

- `fn` is **pure**: it may not declare `requires` and may not call anything that
  has effects. It may call `print` (debug output).
- `task` is a unit of work that may have effects.
- `workflow` is an entry point; it may additionally declare a `budget`.
- `test "name" { ... }` is run by `zal test` and cannot be called.

An `extern KIND name(...) -> T` declares a host operation with the effect
`KIND.name`. Every function declares the effects it may use with `requires`.
The checker enforces, statically and transitively:

1. calling an extern needs its effect in the caller's `requires`;
2. calling a task/workflow needs all of the callee's declared effects;
3. `KIND.*` covers every effect of that kind.

Static effects are a *request*, never a grant. See §9.

## 6. Statements and expressions

- Blocks are expressions; their value is the trailing expression or `()`.
- `if` without `else` has type `Unit`. Branches that `return`, `break` or
  `continue` have the bottom type and unify with anything.
- `match` must be exhaustive for `Bool`, `Option`, `Result` and enums; `Int`
  and `Str` matches need `_` or a binding. Arms after a catch-all are rejected.
  Variant payloads are bound as `Some(x)`, `Color.Rgb(v)`, or ignored with `(_)`.
- `e?` on `Result<T, E>` yields `T` or returns `Err(e)` from the enclosing
  function, which must return `Result<_, E>`. On `Option<T>` it returns `None`.
- `ensure cond, err` returns `Err(err)` when `cond` is false.
- `assert cond, msg` traps the run (tests, invariants).
- `for x in list` iterates a `List`. `range(n)` produces `[0, ..., n-1]`.

## 7. Concurrency

ZAL uses structured, cooperative concurrency on fibers.

| Construct | Type | Meaning |
|---|---|---|
| `parallel { a, b, c }` | `(A, B, C)` | run all branches concurrently, wait for all |
| `race { a, b }` | `T` | first finished branch wins; the others are cancelled |
| `timeout(d) { body }` | `Option<T>` | `Some(v)` if `body` finishes within `d`, else cancel and `None` |
| `retry(n [, backoff: d]) { body }` | `Result<T, E>` | run `body` up to `n` times until `Ok`, sleeping `d` between attempts |
| `spawn e` | `Task<T>` | start `e` concurrently |
| `await t` | `T` | wait for a task |
| `join(ts)` | `List<T>` | wait for all tasks in a list |
| `await_any(ts)` | `(Int, T)` | index and value of a finished task (cancelled tasks are skipped) |
| `cancel(t)` | `Unit` | cancel a task and everything it started |
| `sleep(d)` | `Unit` | suspend for `d` |

Rules:

1. **Isolation.** Each branch of `parallel`/`race`, the body of `timeout`/`retry`
   and the operand of `spawn` is compiled as its own function. Outer variables
   are captured *by value* and are read-only inside; assigning to one is a
   compile error. ZAL fibers therefore never share mutable state.
2. **Own scope.** `return` and `?` inside such a block leave the block, not the
   enclosing function. This is what makes `retry(3) { build(x)? ... }` retry on
   any failing step.
3. **Structured lifetime.** When a task finishes, every child it started and did
   not await is cancelled. Cancelling a task cancels its children and cancels
   its in-flight host call.
4. Awaiting a cancelled task traps. Fibers are preempted every 4096 operations,
   so a busy loop cannot starve siblings.

### 7.1 Automatic parallelization of read-only calls

An extern declared `readonly` promises no observable side effects:

```zal
extern tool read_file(path: Str) -> Result<Str, Str> readonly
```

The compiler then runs independent read-only calls concurrently, even when
the program is written sequentially:

- a run of consecutive `let x = ro(..)` / `let x = ro(..)?` statements whose
  arguments do not use a name bound earlier in the run;
- `for v in xs { acc = push(acc, ro(..)) }` (also with `?`) when the call
  does not use `acc`.

Results, their order and `?` semantics are unchanged. Difference to strictly
sequential execution: when an earlier call fails, later calls of the same run
may already have started (they are cancelled); they count towards
`max_tool_calls`. Calls that are not `readonly` are never reordered.

## 8. Budgets

```zal
workflow w() requires tool.build budget { timeout: 30min, max_parallel: 8, max_tool_calls: 200 } { ... }
```

Budgets are literal and apply to the run whose entry is the workflow:

- `timeout` aborts the run when the wall-clock deadline passes.
- `max_parallel` bounds concurrent in-flight host calls. Further calls wait
  (backpressure) instead of failing.
- `max_tool_calls` aborts the run on the first call over the limit.

Budgets of nested workflows called from another workflow are not enforced in
v0.1 (*reserved*). Memory, token and cost budgets are *reserved*.

## 9. Execution model and host ABI

A program is compiled to ZAL IR v1 (`zal build`, `zal ir`). The VM loads IR,
checks `ir_version`, and executes the entry function:

1. **Preflight**: the broker must grant every concrete effect the entry declares;
   otherwise nothing runs.
2. **Per call**: every extern call is authorized by the broker again, then
   handed to the host. Denial traps the run (fail closed). A wildcard request
   (`tool.*`) is never pre-granted; each concrete call is checked.
3. Tool failures are ordinary `Err` values of the extern's declared `Result`
   type. Host/infrastructure failures trap the run.

Values cross the host boundary as JSON, by declared type: records ↔ objects,
`List`/tuples ↔ arrays, `Map` ↔ objects, `Option` ↔ `null`/value,
`Result` ↔ `{"ok": v}`/`{"err": e}`, enums ↔ `"Variant"` or
`{"variant": "V", "value": v}`, `Dur` ↔ milliseconds.

The JSONL host protocol (`zal run --host '<cmd>'`):

```text
→ {"id":1,"tool":"build","effect":"tool.build","args":{"repo":"x"}}
← {"id":1,"ok":{"ok":{"id":"art-1","size":4096}}}
← {"id":2,"error":"adapter unavailable"}          (infrastructure failure)
→ {"id":3,"cancel":true}
```

## 10. Built-in functions

`len push get set remove range map_new map_with_capacity map_get map_set map_has map_keys str print
join await_any cancel sleep json_str`. `json_str(s)` returns `s` as a quoted, escaped JSON string; `json_get_str(doc, field)` reads a string field of a JSON object; `now_ms()` is wall-clock milliseconds (for timestamps only — it makes a function non-replayable). `xs[i]` traps when out of bounds; `get(xs, i)`
returns `Option`. `str` of an enum/Option/Result value prints `#tag(payload)` in
v0.1 because runtime values carry no type names.

## 11. Reserved for later versions

`checkpoint` (durable replay of completed host calls), qualified imports,
generics in user code, closures, nested budgets, memory/token/cost budgets,
LSP.
