# RFC 0002 — Direct machine-code backends (Cranelift "A", own x86-64 "B")

Status: accepted 2026-11-08. Binding contract for the three work streams.

ZAL compiles straight to machine code; no Rust source is generated. Rust
remains the stage-0 host language of the compiler and of the runtime library
only. Two backends produce machine code from the verified IR v1:

- **A** `backends/cranelift` — Cranelift IR → machine code (JIT in memory,
  or an object file for AOT).
- **B** `backends/x64` — own x86-64 encoder, own JIT memory manager, own ELF
  relocatable object writer. No third-party code generator.

Both use the same runtime ABI (below), so the VM's scheduler, fibers, host
calls, capability broker, budgets and builtins are shared and behave
identically. The existing Rust-source backend (`zal native`) stays as a
reference until A and B pass the differential suite, then it is retired.

## 1. Execution model

The unit of compilation is one IR function. Its machine code is a
*re-entrant function body*:

```c
uint32_t zal_fn_<index>(ZalCtx *ctx, uint32_t entry_pc);
```

- The runtime's dispatcher (in `zal-vm`, replacing the interpreter loop for
  a function that has native code) calls it with the pc to start at (0 for a
  fresh frame, a resume pc after a call returned or a fiber was woken).
- The body executes ops until it must leave, then returns an exit code:
  - `ZAL_EXIT_CALL = 1` — the op at `ctx->pc` pushed a new frame (call); the
    dispatcher continues with the callee and later re-enters this function at
    the saved resume pc.
  - `ZAL_EXIT_RET = 2` — the function returned to a caller frame; the result
    is on the stack as the interpreter leaves it.
  - `ZAL_EXIT_SUSPEND = 3` — the fiber blocks (host call, await, sleep,
    preemption tick, slot wait); the frame pc is saved in the runtime.
  - `ZAL_EXIT_TRAP = 4` — a runtime error is recorded in the context.
  - `ZAL_EXIT_DONE = 5` — the fiber's outermost function returned (no caller
    frame left); the runtime holds the result. `zal_rt_op` on `Ret` returns
    this instead of `RET` in that case; bodies just pass it on like any exit.
- All live state lives in memory owned by the runtime (the fiber's operand
  stack and locals), never only in registers across an exit. Machine code may
  cache values in registers between exits.

Every op has a **generic path**: a call into the runtime that executes exactly
that op with the interpreter's semantics:

```c
uint32_t zal_rt_op(ZalCtx *ctx, uint32_t pc);   // executes func.code[pc]
```

Return values of `zal_rt_op`: `0` continue at `pc+1`; `6` (`ZAL_JUMP`) continue
at `ctx->pc` (a taken branch; the backend knows the static target and may
branch directly); `1..5` as the exit codes above (return them from the body).
Branch ops (`Jump`, `JumpIf*`, `ForNext`, `UnwrapOrJump`, `MapGetJump`,
`JumpIfNotLt`, `JumpIfNotTag`) go through `zal_rt_op` too unless inlined; the
runtime does the tick/preemption accounting for backward branches.

Backends must be complete with the generic path alone (every op correct).
Inline fast paths are an optimisation on top and must be semantically
identical (differential suite: VM vs A vs B).

## 2. Context (`#[repr(C)]`, defined in `zal-vm` as `ZalCtx`)

| Field | Type | Meaning |
|---|---|---|
| `vm` | `*mut c_void` | opaque runtime state (do not touch) |
| `stack` | `*mut ZalValue` | base pointer of the fiber's operand stack buffer |
| `sp` | `u64` | number of live values on the stack (top = `stack[sp-1]`) |
| `base` | `u64` | index of local 0 of the current frame |
| `pc` | `u32` | jump target / resume pc written by the runtime |
| `func` | `u32` | index of the current function |
| `slice` | `u32` | preemption counter (runtime-owned; inline back-edges call `zal_rt_tick`) |

The buffer has room for the verified maximum depth; inline code may write
`stack[sp]` and increment `sp` without a capacity check (same proof as the
interpreter's unchecked push). The runtime re-reads `sp` after every call
into machine code and machine code re-reads `stack`/`sp`/`base` after every
call into the runtime (a call may reallocate or switch frames).

Field offsets: `vm` 0, `stack` 8, `sp` 16, `base` 24, `pc` 32, `func` 36,
`slice` 40; size 48 (`zal_vm::native::CTX_OFF_*`). Implementation notes
(stream R): `ZalCtx::vm` points to a runtime session whose header holds the
monomorphised entry points, so the `zal_rt_*` symbols are plain C functions;
`ctx->slice` carries the interpreter's preemption counter across native and
interpreted frames of one scheduler turn. JIT backends resolve the five
symbols with `zal_vm::native::runtime_symbols()`.

## 3. Value layout (`#[repr(C, u8)]`, 24 bytes, fixed from this RFC on)

Offset 0: `u8` tag. Offset 8: 16-byte payload.

| Tag | Variant | Payload |
|---|---|---|
| 0 | Unit | — |
| 1 | Int | `i64` at +8 |
| 2 | Bool | `u8` at +8 (0/1) |
| 3 | Str | `Rc<str>` (fat pointer) at +8 |
| 4 | Dur | `i64` at +8 (ms) |
| 5 | List | `Rc<Vec<Value>>` at +8 |
| 6 | Map | `Rc<ZMap>` at +8 |
| 7.. | remaining variants in declaration order | runtime-private |

Owned payloads are `Rc` pointers to the allocation (strong count, weak count,
then the data), not to the data; machine code treats them as opaque.

Tags 0, 1, 2, 4 are **plain** (no ownership): machine code may copy, overwrite
and drop them as raw 24 bytes. Any other tag owns a reference count: copying
needs `zal_rt_clone(dst, src)`, overwriting/dropping needs `zal_rt_drop(v)`
(or the generic op path).

## 4. Runtime entry points for inline code (C ABI, `extern "C"`)

| Function | Purpose |
|---|---|
| `uint32_t zal_rt_op(ZalCtx*, uint32_t pc)` | generic path for any op |
| `uint32_t zal_rt_tick(ZalCtx*, uint32_t pc)` | backward-branch accounting; `pc` is the branch **target** (the resume pc: the interpreter saves `pc = t` before ticking); returns 0 or `ZAL_EXIT_SUSPEND`/`TRAP` (then `ctx->pc` = `pc`) |
| `void zal_rt_clone(ZalValue *dst, const ZalValue *src)` | write a clone of `*src` into uninitialised `*dst` |
| `void zal_rt_drop(ZalValue *v)` | drop the value in place |
| `uint32_t zal_rt_trap(ZalCtx*, uint32_t pc, uint32_t code)` | record a trap (code: 1 = integer overflow in +, 2 = in -, 3 = in *, 4 = division by zero / overflow, 5 = remainder) and return `ZAL_EXIT_TRAP` |

Required inline fast paths (both backends, with tag guards falling back to
`zal_rt_op` when an operand is not plain): `Const` (plain constants),
`Unit`, `LoadLocal`, `StoreLocal`, `IncLocal`, `Pop`, Int `Add`/`Sub`/`Mul`
(overflow → trap), `Lt`/`Le`/`Gt`/`Ge`/`Eq`/`Ne` on Int, `JumpIfFalse`/
`JumpIfTrue` on Bool, `JumpIfNotLt` on Int, `Jump` (backward → `zal_rt_tick`).

## 5. Native table and AOT

- Backend crates (`zal-backend-cranelift`, `zal-backend-x64`) implement
  `zal_vm::native::NativeBackend { name, jit(&Program) -> Result<JitModule, String>,
  object(&Program) -> Result<Vec<u8>, String> }` and export
  `pub fn backend() -> Box<dyn NativeBackend>`; `JitModule::new(table, keep)`
  owns the executable memory.
- JIT: the backend returns a table `fn_ptrs[function_index]` (null = run in
  the interpreter); `Vm::set_native(table)` installs it.
- AOT object file exports:
  - `zal_fn_<i>` for every compiled function,
  - `zal_fn_table` — array of `n` function pointers,
  - `zal_fn_count` (`u32`),
  - `zal_ir` / `zal_ir_len` — the program's IR (JSON bytes of
    `Program::to_json()`) embedded as data; `zal_ir_len` is a `uint64_t`,
    `zal_fn_count` a `uint32_t`, a null `zal_fn_table` entry is interpreted.
- The runtime ships as a static library `libzal_aot.a` (crate
  `runtime/aot`, `crate-type = ["staticlib"]`) with `main`: it reads the
  embedded IR, installs `zal_fn_table`, parses the same command-line options
  as `zal run` (`--entry --args --args-file --mock --policy`) and prints the
  result like `zal run`.
- Linking: `cc out.o libzal_aot.a -lpthread -ldl -lm -o out`.
  `zal_cli::aot_link(object, out)` does this (library from `$ZAL_AOT_LIB`, else
  next to `zal`, else `target/release/`; build it with
  `cargo build --release -p zal-aot`).

## 6. CLI

- `zal run … --backend vm|cranelift|x64` (JIT; default `vm`).
- `zal build … --backend cranelift|x64 -o <binary>` (AOT executable).
- Registration (stream R plumbing in `tools/cli`): cargo features
  `cranelift` / `x64` on `zal-cli`; each stream adds its crate as an optional
  dependency, points the feature at it (`cranelift = ["dep:zal-backend-cranelift"]`)
  and the `#[cfg(feature)]` line in `zal_cli::backends()` already calls
  `zal_backend_<name>::backend()`. Without the feature `--backend <name>`
  prints "backend not built". A hidden backend `fake` (generic path only, no
  codegen) exercises the whole pipeline.
- Differential suite gains the modes `cranelift`, `x64` (JIT) and
  `cranelift-aot`, `x64-aot`; every case must equal the VM.

## 7. Work streams

- **R (runtime):** `ZalCtx`, Value layout, `zal_rt_*`, the dispatcher for
  native functions inside `Vm`, `set_native`, `runtime/aot` static library,
  CLI plumbing for `--backend` (calling into the backend crates through a
  small trait `NativeBackend { fn jit(&Program) -> Table; fn object(&Program) -> Vec<u8> }`).
- **A (Cranelift):** `backends/cranelift`: codegen for all ops (generic path
  + required fast paths), JIT via `cranelift-jit`, objects via
  `cranelift-object`.
- **B (x64):** `backends/x64`: encoder, JIT executable memory (mmap
  W^X: write, then mprotect read+exec), ELF64 relocatable writer (`.text`,
  `.data`, `.symtab`, `.strtab`, `.rela.text` with `R_X86_64_PLT32`/`PC32`/
  `R_X86_64_64`), codegen with the same shape as A.

## Addendum v2 (2026-11-08) — performance contract

Measured after v1: both backends are correct (623/623 differential, JIT and
AOT) but slower than the interpreter, because most ops take the generic path
(`zal_rt_op` ≈ 23 ns vs ≈ 13 ns per interpreted op) and every call/return and
every backward branch leaves machine code. v2 removes these costs. Target:
both backends clearly faster than the VM on W2 medium/large and W9
static_chain10, and much faster on scalar loops.

1. **Inline slice counting.** `ZAL_SLICE` (u32, = the interpreter's SLICE) is
   exported. On a taken backward branch machine code does
   `ctx->slice += 1; if ctx->slice >= ZAL_SLICE { r = zal_rt_tick(ctx, target) }`.
   The runtime guarantees `zal_rt_tick` has no effect besides the slice check
   (deadline, preemption) — calling it only at the limit is equivalent.
2. **Lean generic path.** `zal_rt_op` executes one op without setting up the
   interpreter loop (direct per-op code, no frame-pc write unless the op can
   exit). Same semantics, target ≤ 6 ns overhead.
3. **Typed helpers** (C ABI, no frame/pc bookkeeping; all take `ZalValue*`
   pointing into the operand stack/locals; return 0 on success or a nonzero
   code meaning "not applicable — take the generic path"):
   - `int64_t zal_rt_len(const ZalValue *v)` → length of List/Map/Str (chars), or -1.
   - `uint32_t zal_rt_index(ZalValue *dst, const ZalValue *list, int64_t i)` → clone element into dst; 1 = out of bounds / not a list (generic path traps).
   - `uint32_t zal_rt_for_next(ZalValue *slot3)` → `ForNext` on locals [list, index, var]: 0 = advanced, 1 = exhausted, 2 = generic.
   - `uint32_t zal_rt_upd(ZalValue *local, ZalValue *args, uint32_t builtin)` → in-place `UpdLocal` (push/set/remove/map_set) consuming args; 0 ok, 2 generic.
   - `uint32_t zal_rt_tag(const ZalValue *v)` → variant tag (or Bool 0/1), 0xFFFF if not a variant.
   - `uint32_t zal_rt_payload(ZalValue *dst, const ZalValue *v)` → clone variant payload into dst; 2 = none/generic.
   - `uint32_t zal_rt_field(ZalValue *dst, const ZalValue *rec, uint32_t i)` → clone record field.
   - `uint32_t zal_rt_map_get(ZalValue *dst, const ZalValue *map, const ZalValue *key)` → 0 found, 1 missing, 2 generic.
   - `void zal_rt_const(ZalValue *dst, uint32_t const_index, const ZalCtx*)` → clone a (non-plain) constant.
   - `uint32_t zal_rt_concat(ZalCtx*, uint32_t n)` → `ConcatN` on the top n stack values.
4. **Cheap calls.** `Call` of a function with native code: machine code calls
   `zal_rt_call(ctx, pc, callee, argc)` which pushes the frame and returns
   `ZAL_EXIT_CALL`; the dispatcher's native→native transition reuses the
   current session (no new context). `Ret` returns `ZAL_EXIT_RET` and the
   dispatcher re-enters the caller at its resume pc directly. Target: a
   native→native call+return ≤ 15 ns total.

Work split: R implements 1–4 in the runtime first (with unit tests per helper
against the interpreter's semantics); A and B use them in codegen as soon as
they are on main, then add type tracking within basic blocks to drop guards.

### v2 runtime notes (stream R, as implemented)

- `ZAL_SLICE = 512` (`zal_vm::native::ZAL_SLICE`). `zal_rt_tick(ctx, target)`
  serves both styles: a v2 caller has already incremented `ctx->slice` to the
  limit (the runtime does not count again); a v1 caller that calls on every
  back edge without counting is counted by the runtime. Either way the call
  has no effect below the limit.
- `zal_rt_op` runs a lean per-op path directly on `ctx->stack` for every op
  that touches neither the scheduler nor the frame stack (all stack/local,
  arithmetic, comparison, branch, aggregate, variant, map/list ops and the
  inline builtins), and `Call`/`Ret` through the same code as `zal_rt_call`.
  It decides before mutating; anything else (traps, unexpected operand types,
  the slice limit on a backward branch, scheduler ops, `--profile`) runs the
  interpreter's code for that op. After `ZAL_NEXT` only `ctx->stack/sp/base`
  are guaranteed; `ctx->pc`/`ctx->func` are current after `ZAL_JUMP` and exits.
- Helper conventions: `dst` is uninitialised, or it is the very pointer of
  the value being read (`zal_rt_index(dst=list, list, i)` replaces the list
  by the element; the source is consumed). Nonzero = nothing changed.
  `zal_rt_upd` builtin codes: `ZAL_BUILTIN_PUSH = 1`, `SET = 3`,
  `REMOVE = 4`, `MAP_SET = 8` (`zal_ir::Builtin` order); `args` points at the
  deepest argument (Set: [i, x], MapSet: [key, value]); on 0 the arguments are
  moved out (lower `sp` by the count without dropping).
  `zal_rt_map_get`: `dst` must not alias `key`.
- `zal_rt_call(ctx, pc, callee, argc)` = the interpreter's `Call` at `pc`
  (resume pc `pc + 1`, locals padded with Unit, call tick, depth limit);
  returns `ZAL_EXIT_CALL` with `ctx` describing the callee at pc 0, or
  `SUSPEND`/`TRAP`. The dispatcher keeps one session/context per scheduler
  turn and, on `CALL`/`RET` to a function with native code, calls its body at
  `ctx->pc` directly (no frame re-read, no new context).
- `zal_rt_ret(ctx)` (extra entry): `Ret` without op decoding, same result
  as `zal_rt_op` on a `Ret` (`ZAL_EXIT_RET`, or `ZAL_EXIT_DONE` for the
  fiber's outermost function).
- `runtime_symbols()` returns all 17 entry points (a `[_; 17]`); the AOT
  library `libzal_aot.a` exports every one of them (checked by
  `tools/cli/tests/aot.rs` against `runtime_symbols()`).
- Contract points confirmed for the backends: (1) `zal_rt_upd`'s `builtin`
  is the `zal_ir::Builtin` index (Push 1, Set 3, Remove 4, MapSet 8);
  (2) `ZAL_SLICE` is exported by `zal-vm`; (3) every helper that returns
  "not applicable" (nonzero, also `zal_rt_index` = 1 and `zal_rt_map_get` = 2;
  `zal_rt_map_get` = 1 "missing" leaves `dst` untouched too) changes no memory,
  so the op can be re-run through `zal_rt_op`; (4) when `zal_rt_tick` handles
  the limit it resets `ctx->slice` to 0 itself, before the deadline check and
  preemption, as the interpreter does.
- During native execution the fiber's `Vec` length is not maintained by
  `zal_rt_call`/`zal_rt_ret`/the lean path; the runtime sets it from
  `ctx->sp` whenever it leaves to the scheduler or the interpreter.

### v3 runtime helpers (stream R, 2026-11-08)

Same conventions as v2 (C ABI, no frame/pc bookkeeping, "not applicable" =
nonzero and nothing changed, so the op can be re-run through `zal_rt_op`).
All are in `runtime_symbols()` (now 20 entries) and exported by `libzal_aot.a`.

- `uint32_t zal_rt_make(ZalCtx*, uint32_t kind, uint32_t n)`:
  `kind` `ZAL_MAKE_LIST = 0`, `ZAL_MAKE_TUPLE = 1`, `ZAL_MAKE_RECORD = 2` build
  the aggregate from the top `n` values (replacing them);
  `ZAL_MAKE_VARIANT = 3` with `n = tag | (has << 16)` (`has`: the payload is
  the top value). Updates `ctx->sp`; 0, or 2 for an unknown kind.
- `uint32_t zal_rt_builtin(ZalCtx*, uint32_t builtin, uint32_t argc)`:
  `builtin` = `zal_ir::Builtin` index. Executes Len, Push, Get, Set, Remove,
  Range, MapNew, MapWithCapacity, MapGet, MapSet, MapHas, MapKeys, ToStr,
  JsonStr, JsonGetStr, NowMs on the top `argc` values (updates `ctx->sp` and
  may move `ctx->stack`); 2 for Print, Join, AwaitAny, Cancel, Sleep, a wrong
  `argc`, or operands the interpreter would trap on / treat specially.
- `uint32_t zal_rt_map_get_jump(ZalCtx*, uint32_t local)`: `MapGetJump`
  with map and key (Str) on top of the stack; consumes both; 0 = found (value
  stored into local `local`), 1 = missing (take the branch, with the usual
  inline slice counting for a backward target), 2 = generic.
- Scheduler ops on the native path: `zal_rt_op` on `Spawn`, `Await`,
  `AwaitAll`, `Extern` no longer sets up the interpreter loop (same steps as
  the interpreter's arms; error paths still use it). VM ordering is unchanged
  (no eager first poll). Children/awaiter lists of finished fibers are
  pooled, and the run-local maps (pending host calls, mock tool tables) use
  the Fx hasher instead of SipHash; both also speed up the interpreter.
- Measured on W9 `static_chain10` (callgrind, instructions inside `Vm::run`
  per run): before v3 VM 71.6k / x64 72.1k; after VM 63.9k / x64 60.0k /
  cranelift 67.7k. Most of the rest is shared runtime work: host call
  (~8.7k, mock), fiber start/spawn (~6k), malloc/free (~6k), scheduler turns
  (~6k, about 30 per run); the native-only part is the generic-path ops the
  backends still call (`zal_rt_clone` 3.4k, `zal_rt_ret` 3.2k,
  `zal_rt_op`+lean path 5.3k, `zal_rt_concat` 1.8k, `zal_rt_call` 1.7k).
  Rebuilding the session per scheduler turn costs about 20 instructions
  (~0.6k per run), so it was left as is.

## Addendum v4 (2026-11-08) — inline data access for JIT code

Measured after v3 on W2-Z (one-level fan-out DAG, k ≤ 12): ≈ 11.6k instructions per
scheduled node on Cranelift, ≈ 65 per ZAL op, most of it in the typed
helpers. v4 lets machine code read and update owned values directly.

1. **Data layout, measured in-process.** `zal_vm::native::data_layout()`
   returns `Option<DataLayout>` (`#[repr(C)]`, byte offsets): it builds one
   value of each kind and locates the words, and returns `None` if the build
   does not have the shape below (then backends use the helpers only).
   - Every owned payload word at value+8 points to an `Rc` allocation:
     strong count at +0, weak count at +8, data from +16 (v1 §3).
   - List (`Rc<Vec<Value>>`, tag 5): buffer pointer, capacity and length are
     the words at `list_ptr` / `list_cap` / `list_len` from the allocation
     start; elements are 24-byte values.
   - Str (3), Tuple (7), Record (8) hold a fat pointer: allocation pointer at
     value+`fat_ptr`, count at value+`fat_len`; data at allocation+16.
   - Variant (9): `u16` variant tag at value+8, payload pointer at value+16
     (0 = none; the payload value is at allocation+16).
   - Task (10): `u32` at value+8, no ownership (copied/dropped raw).
   The layout is **only valid for code running in the process that measured
   it** (JIT). AOT objects link a separately built runtime and keep using the
   helpers; `ZAL_NO_INLINE_DATA=1` disables v4 in the JIT as well.
2. **Inline ops (Cranelift JIT).** With a layout: `ForNext`, `IndexLL`,
   `LenLocal` (List; other kinds via `zal_rt_len`), `Builtin(Len)` on a List,
   `LoadField`, `FieldGet`, `JumpIfNotTag` on variants, `MovePayload`,
   `LoadPayload`, and `UpdLocal` push / set / remove on a **unique** list
   (strong = weak = 1; push only with spare capacity). Rc clone/drop inline
   for Str, List, Map, Tuple, Record and Variant payloads (the last
   reference still drops through `zal_rt_drop`). Shared lists, growth,
   out-of-range indices and unexpected tags take the helper or the generic
   op, so semantics (traps, copy-on-write) are unchanged.
3. **JIT constants.** `zal_vm::native::jit_constants(prog)` = the program's
   constants plus one empty List, owned by the JIT module. Str constants and
   `MakeList(0)` are cloned from it inline (refcount increment). Values are
   immutable, so sharing is unobservable; the first push into the shared
   empty list copies it into a fresh buffer as for any shared list.
4. **Runtime.** A native `AwaitAny` that is directly followed by
   `SplitTuple(2)` pushes index and result without building the tuple and
   resumes after the split (`ZAL_JUMP`).
5. **Compiler.** `spawn f(a, b)` whose arguments are plain variables spawns
   `f` directly with the captured values instead of a lifted wrapper (same
   values, same order; reading a variable has no effect).
