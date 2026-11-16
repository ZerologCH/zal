# Bootstrap

ZAL is a language of its own. Its first compiler and VM are written in Rust
("stage 0"), the same way CPython is written in C without Python being C.

## Stage 0 (now)

- Compiler and VM: Rust 1.99, no dependencies beyond `serde`/`serde_json`.
- Orchestration logic: ZAL (`zal-src/`). The Rust side contains no workflow,
  scheduling or retry policy.
- Build: `cargo build --release` → `target/release/zal`.
- Verify: `cargo test` (golden diagnostics, runtime semantics, compiler fuzzing)
  and `zal test examples/*.zal --mock ...`.

## What would change in a later stage

- Stage 1 (optional, Phase 5): rewrite the compiler front end in ZAL, compile it
  with stage 0, and accept it only when its IR output is byte-identical to stage
  0 on the whole test corpus (golden IR tests) and the reproducible build matches.
- The VM may stay native; a VM is an execution primitive, not orchestration
  logic. Self-hosting is not a precondition for anything else in the plan.

## Reproducibility

`zal build` output is deterministic for a given source and compiler version
(constant pool order and lifting order are source-ordered). Golden IR tests are
the planned guard for the stage-1 migration.
