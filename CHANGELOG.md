# Changelog

## 0.1.0 — 2026-11-07

- Stage-0 compiler: lexer, parser, type and effect checker, IR v1.
- VM with fibers, `parallel`, `race`, `timeout`, `retry`, `spawn/await`,
  `join`, `await_any`, `cancel`, `sleep`, budgets, broker/host ABI.
- JSONL host protocol and in-process mock host.
- `zal check/build/ir/run/test/bench`.
- ZAL DAG scheduler (`zal-src/scheduler/dag.zal`).
- Tests: golden diagnostics, runtime semantics, compiler fuzzing.
