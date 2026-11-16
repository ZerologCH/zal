# Architecture

```text
 .zal source ──► lexer ─► parser ─► AST ─► check+lower ─► ZAL IR v1 (JSON, versioned)
                                         (types, effects,                │
                                          capabilities,                   ▼
                                          lambda lifting)        ZAL VM (vm/)
                                                                  fibers, timers,
                                                                  budgets, traps
                                                                       │ every extern call
                                                     ┌─────────────────┴───────────────┐
                                                     ▼                                 ▼
                                             Broker::authorize                    Host::call
                                       (external policy authority,         (tool proxy: JSONL process,
                                        fail closed, per call)              in-process mock for tests)
```

## Crates

| Path | Crate | Role |
|---|---|---|
| `ir/` | `zal-ir` | IR v1 data model, JSON (de)serialization, version check, disassembler |
| `compiler/` | `zal-compiler` | lexer, parser, AST, type/effect checker fused with bytecode emission, module loader |
| `vm/` | `zal-vm` | stack VM with cooperative fibers, host ABI (`Host`, `Broker`), JSON marshalling |
| `tools/cli/` | `zal-cli` | `zal check / build / ir / run / test / bench` |
| `zal-src/` | — | ZAL code: the orchestration logic itself (`scheduler/dag.zal`) |

## Where logic lives

The rule from the development plan: orchestration decisions are ZAL code; the
VM only provides mechanisms.

| Concern | Lives in |
|---|---|
| DAG validation (duplicates, unknown deps, cycles) | `zal-src/scheduler/dag.zal` |
| Ready-queue policy, fan-out limit, input routing | `zal-src/scheduler/dag.zal` |
| Per-node retry policy, fail-fast cancellation | `zal-src/scheduler/dag.zal` |
| Fibers, run queue, timers, await/race/timeout primitives | VM |
| Budget enforcement (deadline, in-flight limit, call count) | VM (declared in ZAL) |
| Capability decision | external broker (`Broker` trait) |
| Side effects (tools, network, files, secrets) | host adapters behind `Host` |

## Compiler

One pass per function: every expression is typed by unification and lowered to
stack bytecode in the same walk. Concurrent blocks are lambda-lifted into
separate functions whose parameters are the captured variables, which is how the
"no shared mutable state between fibers" rule is enforced. A peephole turns
`x = push(x, ...)` into a move so copy-on-write containers are updated in place.

## VM

- Values: `Rc`-shared immutable data; one thread; no GC pauses (reference counting).
- Fibers: one operand stack + frames each. Blocking ops (`await`, `race`,
  `timeout`, `sleep`, `join`, `await_any`) re-execute on wake; host waits are
  keyed by call id, so spurious wakes cannot resume a fiber without its result.
- Scheduler: FIFO ready queue, timer heap, host poll with deadline. Synchronous
  host replies continue without a context switch.
- Structured concurrency: finishing or cancelling a fiber cancels its children
  and their in-flight host calls.
- Traps carry a ZAL stack trace (function, line).

## IR

`ir/src/lib.rs` defines the IR. It is versioned (`ir_version`, currently 1);
the VM refuses other versions. IR v1 is stack bytecode plus static facts per
function (effects, budget, param/return types) and extern signatures. A
graph-shaped IR for whole-workflow optimization is a Phase 3 research item, not
part of v1.
